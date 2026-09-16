//! 上游 HTTP 传输：reqwest client 构造 / 缓存、Cloudflare-only cookie jar、
//! 请求头排序。所有构造参数与 codex-rs（rust-v0.153.4）的默认 client 对齐：
//! - TLS 后端：reqwest default-tls（native-tls），不调用 use_rustls_tls()
//! - 不设置连接池 / TCP keepalive / HTTP2 调优（保持 reqwest 默认，与 codex 一致）
//! - 不启用压缩 feature：出站不带自动 accept-encoding（与 codex 一致）
//! - Cloudflare 基建 cookie jar，白名单与 codex chatgpt_cloudflare_cookies.rs 一致

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::config::PluginConfig;
use crate::proto::sub2api::plugin::v1::ForwardRequestStart;

// ---------------------------------------------------------------------------
// Cloudflare-only cookie jar（对齐 codex-rs http-client/chatgpt_cloudflare_cookies.rs）
// ---------------------------------------------------------------------------

/// 与 codex 一致的 ChatGPT 第一方主机判定。
fn is_allowed_chatgpt_host(host: &str) -> bool {
    const EXACT_HOSTS: &[&str] = &["chatgpt.com", "chat.openai.com", "chatgpt-staging.com"];
    const SUBDOMAIN_SUFFIXES: &[&str] = &[".chatgpt.com", ".chatgpt-staging.com"];
    EXACT_HOSTS.contains(&host)
        || SUBDOMAIN_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

fn is_chatgpt_cookie_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && url.host_str().is_some_and(is_allowed_chatgpt_host)
}

/// 与 codex 一致的 Cloudflare 服务 cookie 白名单。
fn is_allowed_cloudflare_cookie_name(name: &str) -> bool {
    matches!(
        name,
        "__cf_bm"
            | "__cflb"
            | "__cfruid"
            | "__cfseq"
            | "__cfwaitingroom"
            | "_cfuvid"
            | "cf_clearance"
            | "cf_ob_info"
            | "cf_use_ob"
    ) || name.starts_with("cf_chl_")
}

fn set_cookie_name(header: &str) -> Option<&str> {
    let (name, _) = header.split_once('=')?;
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

fn only_cloudflare_cookies(header: HeaderValue) -> Option<HeaderValue> {
    let header = header.to_str().ok()?;
    let cookies = header
        .split(';')
        .filter_map(|cookie| {
            let cookie = cookie.trim();
            let name = cookie.split_once('=')?.0.trim();
            is_allowed_cloudflare_cookie_name(name).then_some(cookie)
        })
        .collect::<Vec<_>>()
        .join("; ");
    if cookies.is_empty() {
        None
    } else {
        HeaderValue::from_str(&cookies).ok()
    }
}

/// 只保存 Cloudflare 基建 cookie 的 jar。绝不能存 ChatGPT 账号 / 会话 cookie。
#[derive(Default)]
pub struct CloudflareOnlyJar {
    jar: Jar,
}

impl CookieStore for CloudflareOnlyJar {
    fn set_cookies(
        &self,
        cookie_headers: &mut dyn Iterator<Item = &HeaderValue>,
        url: &reqwest::Url,
    ) {
        if !is_chatgpt_cookie_url(url) {
            return;
        }
        let mut allowed = cookie_headers.filter(|header| {
            header
                .to_str()
                .ok()
                .and_then(set_cookie_name)
                .is_some_and(is_allowed_cloudflare_cookie_name)
        });
        self.jar.set_cookies(&mut allowed, url);
    }

    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue> {
        if !is_chatgpt_cookie_url(url) {
            return None;
        }
        self.jar.cookies(url).and_then(only_cloudflare_cookies)
    }
}

// ---------------------------------------------------------------------------
// Client 缓存
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    /// per_account_cookie_jar=true 时为账号 ID（cookie jar 按账号隔离），否则为 0。
    account: i64,
    proxy: Option<String>,
    force_http11: bool,
    connect_timeout_seconds: u32,
}

struct CachedClient {
    client: reqwest::Client,
    last_used: Instant,
}

#[derive(Default)]
pub struct ClientCache {
    clients: Mutex<HashMap<ClientKey, CachedClient>>,
}

impl ClientCache {
    pub fn clear(&self) {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub fn client_for(
        &self,
        config: &PluginConfig,
        account_id: i64,
        proxy_url: &str,
    ) -> Result<reqwest::Client, String> {
        let proxy = {
            let trimmed = proxy_url.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let key = ClientKey {
            account: if config.per_account() { account_id } else { 0 },
            proxy,
            force_http11: config.force_http11,
            connect_timeout_seconds: config.connect_timeout_seconds,
        };

        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = clients.get_mut(&key) {
            cached.last_used = Instant::now();
            return Ok(cached.client.clone());
        }

        let client = build_client(config, key.proxy.as_deref())?;
        if clients.len() >= config.max_cached_clients as usize {
            if let Some(oldest) = clients
                .iter()
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(key, _)| key.clone())
            {
                clients.remove(&oldest);
            }
        }
        clients.insert(
            key,
            CachedClient {
                client: client.clone(),
                last_used: Instant::now(),
            },
        );
        Ok(client)
    }
}

/// 构造与 codex 默认 client 对齐的 reqwest client。
///
/// 注意：这里刻意 **不** 设置连接池大小、keepalive、HTTP2 窗口等参数——
/// codex 的 HttpClientBuilder 同样不设置，保持 reqwest/h2 默认值才能让
/// HTTP/2 SETTINGS 帧与真实客户端一致。
fn build_client(config: &PluginConfig, proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder =
        reqwest::Client::builder().cookie_provider(Arc::new(CloudflareOnlyJar::default()));
    if config.connect_timeout_seconds > 0 {
        builder =
            builder.connect_timeout(Duration::from_secs(config.connect_timeout_seconds as u64));
    }
    if config.force_http11 {
        builder = builder.http1_only();
    }
    if let Some(proxy_url) = proxy {
        let proxy_url = reqwest_proxy_url(proxy_url)?;
        let proxy = reqwest::Proxy::all(proxy_url)
            .map_err(|_| "invalid proxy URL or unsupported proxy scheme".to_string())?;
        builder = builder.proxy(proxy);
    } else {
        // Sub2API 已显式决定是否使用账号代理；插件不应再次读取进程代理环境。
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|err| format!("build HTTP client: {err}"))
}

// ---------------------------------------------------------------------------
// 请求头排序
// ---------------------------------------------------------------------------

/// gRPC 的 map<string, HeaderValues> 不保序，宿主侧 Go map 遍历本身也随机。
/// 按固定顺序重建 HeaderMap，保证同一账号的出站请求头顺序恒定
/// （hyper h2 按 HeaderMap 插入序编码 HEADERS 帧）。
/// 与真实 codex 0.153.4 的 HeaderMap 插入顺序（即 hyper 发送顺序）逐项对齐。
/// 依据：本机 codex exec 明文抓包 + core/src/client.rs 的 extra_headers 组装顺序。
/// 真实序：extra(x-codex-*) → stream_request(x-client-request-id/session-id/thread-id)
///        → accept → content-type → authorization(+chatgpt-account-id)
///        → Client 默认头(originator/user-agent/residency) → host/content-length(hyper 追加)。
const EARLY_HEADER_ORDER: &[&str] = &[
    "x-codex-beta-features",
    "x-codex-turn-state",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-codex-parent-thread-id",
    "x-openai-subagent",
    "x-openai-memgen-request",
    "x-oai-attestation",
    "x-codex-routing-hint",
    "x-codex-installation-id",
    "x-client-request-id",
    "session-id",
    "thread-id",
    "accept",
    "openai-beta",
    "content-type",
    "authorization",
    "chatgpt-account-id",
];
/// reqwest 默认头在真实客户端里最后合并（capture 中位于 host 之前的末尾段）。
const LATE_HEADER_ORDER: &[&str] = &[
    "originator",
    "user-agent",
    "x-openai-internal-codex-residency",
];

/// 真实 codex 0.153.4 对 ChatGPT Codex 后端绝不发送的头（宿主/老客户端遗留），
/// 转发时剥除以保证与真实客户端一致:
/// - version: 宿主强制注入,官方 0.153.4 已不发送该头;
/// - accept-encoding: 官方 reqwest 未启用压缩特性,不发送;
/// - cookie: 上游 Cloudflare cookie 由插件的 jar 统一捕获回放（设备一致性），
///   透传中转链路上的 cookie 会产生重复/串号。
const CODEX_ALWAYS_STRIP_HEADERS: &[&str] = &["cookie"];
const CODEX_STRICT_STRIP_HEADERS: &[&str] = &["version", "accept-encoding"];

/// 由 hyper/reqwest 管理、禁止透传的头。
fn is_managed_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

pub fn ordered_headers(
    raw: &HashMap<String, crate::proto::sub2api::plugin::v1::HeaderValues>,
    codex_backend: bool,
    strict_native_headers: bool,
) -> HeaderMap {
    let mut lowered: HashMap<String, &crate::proto::sub2api::plugin::v1::HeaderValues> =
        HashMap::with_capacity(raw.len());
    for (name, values) in raw {
        let name = name.to_ascii_lowercase();
        if codex_backend
            && (CODEX_ALWAYS_STRIP_HEADERS.contains(&name.as_str())
                || strict_native_headers && CODEX_STRICT_STRIP_HEADERS.contains(&name.as_str()))
        {
            continue;
        }
        lowered.insert(name, values);
    }

    let mut headers = HeaderMap::with_capacity(raw.len());
    let mut append = |name: &str, values: &crate::proto::sub2api::plugin::v1::HeaderValues| {
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            return;
        };
        for value in &values.values {
            if let Ok(header_value) = HeaderValue::from_str(value) {
                headers.append(header_name.clone(), header_value);
            }
        }
    };

    let in_order =
        |name: &str| EARLY_HEADER_ORDER.contains(&name) || LATE_HEADER_ORDER.contains(&name);
    // 1. 真实 extra/请求级头（x-codex-* → ids → accept/content-type/auth）。
    for name in EARLY_HEADER_ORDER {
        if let Some(values) = lowered.get(*name) {
            append(name, values);
        }
    }
    // 2. 未知头：真实客户端的额外请求级头会出现在默认头之前。
    let mut remaining: Vec<&String> = lowered
        .keys()
        .filter(|name| !in_order(name.as_str()) && !is_managed_header(name))
        .collect();
    remaining.sort();
    for name in remaining {
        append(name, lowered[name]);
    }
    // 3. Client 默认头（originator/user-agent/residency），与真实客户端相同位于最后；
    //    host 与 content-length 由 hyper 在其后自动追加。
    for name in LATE_HEADER_ORDER {
        if let Some(values) = lowered.get(*name) {
            append(name, values);
        }
    }
    headers
}

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

pub struct ClassifiedError {
    pub code: &'static str,
    pub message: String,
    /// 请求是否可能已到达上游。true 时宿主禁止自动换号重放。
    pub request_sent: bool,
}

pub fn classify_reqwest_error(err: &reqwest::Error) -> ClassifiedError {
    let message = safe_reqwest_error(err);
    if err.is_connect() {
        // TCP / TLS / 代理 CONNECT 建立失败：请求未写出。
        return ClassifiedError {
            code: "PLUGIN_UPSTREAM_CONNECT",
            message,
            request_sent: false,
        };
    }
    if err.is_timeout() {
        return ClassifiedError {
            code: "PLUGIN_UPSTREAM_TIMEOUT",
            message,
            request_sent: true,
        };
    }
    if err.is_request() {
        return ClassifiedError {
            code: "PLUGIN_UPSTREAM_REQUEST",
            message,
            request_sent: true,
        };
    }
    ClassifiedError {
        code: "PLUGIN_UPSTREAM_IO",
        message,
        request_sent: true,
    }
}

pub fn safe_reqwest_error(err: &reqwest::Error) -> String {
    if err.is_connect() {
        "upstream connection or proxy tunnel failed".to_string()
    } else if err.is_timeout() {
        "upstream request timed out".to_string()
    } else if err.is_builder() {
        "upstream request could not be constructed".to_string()
    } else if err.is_decode() {
        "upstream response could not be decoded".to_string()
    } else {
        "upstream request failed".to_string()
    }
}

pub fn validate_forward_target(start: &ForwardRequestStart) -> Result<(), String> {
    if start.platform != "openai" || start.account_type != "oauth" {
        return Err("plugin only accepts OpenAI OAuth accounts".to_string());
    }
    if start.account_id <= 0 {
        return Err("account_id must be positive".to_string());
    }
    let url = reqwest::Url::parse(&start.url).map_err(|_| "invalid upstream URL".to_string())?;
    let host = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "upstream URL is missing a host".to_string())?;
    let loopback_test = matches!(host.as_str(), "127.0.0.1" | "::1" | "localhost");
    if url.scheme() != "https" && !(loopback_test && url.scheme() == "http") {
        return Err("upstream URL must use HTTPS".to_string());
    }
    let allowed = is_allowed_chatgpt_host(&host) || host == "api.openai.com" || loopback_test;
    if !allowed {
        return Err("upstream host is not an approved OpenAI/Codex host".to_string());
    }
    Ok(())
}

fn reqwest_proxy_url(raw: &str) -> Result<reqwest::Url, String> {
    let mut parsed = reqwest::Url::parse(raw)
        .map_err(|_| "invalid proxy URL or unsupported proxy scheme".to_string())?;
    if !matches!(
        parsed.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) || parsed.host_str().is_none()
    {
        return Err("invalid proxy URL or unsupported proxy scheme".to_string());
    }
    // Sub2API's existing Go SOCKS5 path sends the destination hostname to the
    // proxy. reqwest distinguishes that behavior as socks5h; normalize the
    // stored socks5 scheme so rollout does not change DNS or IPv4/IPv6 routing.
    if parsed.scheme() == "socks5" {
        parsed
            .set_scheme("socks5h")
            .map_err(|_| "invalid proxy URL or unsupported proxy scheme".to_string())?;
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::sub2api::plugin::v1::HeaderValues;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn values(items: &[&str]) -> HeaderValues {
        HeaderValues {
            values: items.iter().map(|value| value.to_string()).collect(),
        }
    }

    #[test]
    fn orders_headers_like_real_codex_emission() {
        // 输入乱序（gRPC map 无序），输出必须复现真实 codex 0.153.4 抓包的发送顺序：
        // x-codex-* → 请求 id 头 → accept/content-type/authorization → 未知头 → 默认头。
        let mut raw = HashMap::new();
        raw.insert("User-Agent".to_string(), values(&["codex_cli_rs/0.153.4"]));
        raw.insert("originator".to_string(), values(&["codex_cli_rs"]));
        raw.insert("Authorization".to_string(), values(&["Bearer x"]));
        raw.insert("Accept".to_string(), values(&["text/event-stream"]));
        raw.insert("Content-Type".to_string(), values(&["application/json"]));
        raw.insert("chatgpt-account-id".to_string(), values(&["acc"]));
        raw.insert("session-id".to_string(), values(&["s1"]));
        raw.insert("thread-id".to_string(), values(&["t1"]));
        raw.insert("x-client-request-id".to_string(), values(&["t1"]));
        raw.insert("x-codex-beta-features".to_string(), values(&["f"]));
        raw.insert("x-codex-window-id".to_string(), values(&["w:0"]));
        raw.insert("x-codex-turn-metadata".to_string(), values(&["{}"]));
        raw.insert("X-Custom".to_string(), values(&["a", "b"]));
        raw.insert("Host".to_string(), values(&["chatgpt.com"]));

        let headers = ordered_headers(&raw, false, false);
        let names: Vec<&str> = headers.keys().map(|name| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "x-codex-beta-features",
                "x-codex-window-id",
                "x-codex-turn-metadata",
                "x-client-request-id",
                "session-id",
                "thread-id",
                "accept",
                "content-type",
                "authorization",
                "chatgpt-account-id",
                "x-custom",
                "originator",
                "user-agent",
            ]
        );
        assert_eq!(headers.get_all("x-custom").iter().count(), 2);
        assert!(headers.get("host").is_none());
    }

    #[test]
    fn strips_non_codex_headers_for_codex_backend() {
        // 真实 codex 0.153.4 不发送 version / accept-encoding / cookie（cookie 由插件 jar 管理）。
        let mut raw = HashMap::new();
        raw.insert("version".to_string(), values(&["0.50.0"]));
        raw.insert("Accept-Encoding".to_string(), values(&["gzip"]));
        raw.insert("Cookie".to_string(), values(&["__cf_bm=stale"]));
        raw.insert("originator".to_string(), values(&["codex_cli_rs"]));

        let codex = ordered_headers(&raw, true, true);
        assert!(codex.get("version").is_none());
        assert!(codex.get("accept-encoding").is_none());
        assert!(codex.get("cookie").is_none());
        assert!(codex.get("originator").is_some());

        // 非 codex 后端保持透传。
        let other = ordered_headers(&raw, false, false);
        assert!(other.get("version").is_some());
    }

    #[test]
    fn passthrough_keeps_host_identity_headers_but_never_host_cookie() {
        let mut raw = HashMap::new();
        raw.insert("version".to_string(), values(&["0.153.4"]));
        raw.insert("Accept-Encoding".to_string(), values(&["gzip"]));
        raw.insert("Cookie".to_string(), values(&["session=secret"]));
        let headers = ordered_headers(&raw, true, false);
        assert_eq!(headers.get("version").unwrap(), "0.153.4");
        assert_eq!(headers.get("accept-encoding").unwrap(), "gzip");
        assert!(headers.get("cookie").is_none());
    }

    #[test]
    fn forward_target_rejects_non_openai_and_untrusted_hosts() {
        let mut start = ForwardRequestStart {
            request_id: "r".to_string(),
            method: "POST".to_string(),
            url: "https://chatgpt.com/backend-api/codex/responses".to_string(),
            host: "chatgpt.com".to_string(),
            headers: HashMap::new(),
            proxy_url: String::new(),
            account_id: 1,
            account_concurrency: 1,
            platform: "openai".to_string(),
            account_type: "oauth".to_string(),
            content_length: 0,
            has_body: false,
        };
        assert!(validate_forward_target(&start).is_ok());
        start.url = "https://example.com/steal".to_string();
        assert!(validate_forward_target(&start).is_err());
        start.url = "http://chatgpt.com/backend-api/codex/responses".to_string();
        assert!(validate_forward_target(&start).is_err());
        start.url = "http://127.0.0.1/test".to_string();
        assert!(validate_forward_target(&start).is_ok());
    }

    #[test]
    fn cloudflare_jar_only_keeps_infra_cookies_for_chatgpt() {
        let jar = CloudflareOnlyJar::default();
        let url: reqwest::Url = "https://chatgpt.com/backend-api/codex/responses"
            .parse()
            .unwrap();
        let allowed = HeaderValue::from_static("__cf_bm=token123; Path=/; Secure");
        let denied = HeaderValue::from_static("session=secret; Path=/; Secure");
        jar.set_cookies(&mut [&allowed, &denied].into_iter(), &url);
        let sent = jar.cookies(&url).unwrap();
        assert_eq!(sent.to_str().unwrap(), "__cf_bm=token123");

        let other: reqwest::Url = "https://api.openai.com/v1".parse().unwrap();
        assert!(jar.cookies(&other).is_none());
    }

    #[tokio::test]
    async fn authenticated_socks5_proxy_uses_remote_dns_and_forwards_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let proxy = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let mut greeting = [0_u8; 2];
            socket.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting[0], 5);
            let mut methods = vec![0_u8; greeting[1] as usize];
            socket.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&2));
            socket.write_all(&[5, 2]).await.unwrap();

            let mut auth = [0_u8; 2];
            socket.read_exact(&mut auth).await.unwrap();
            assert_eq!(auth[0], 1);
            let mut username = vec![0_u8; auth[1] as usize];
            socket.read_exact(&mut username).await.unwrap();
            let password_len = socket.read_u8().await.unwrap();
            let mut password = vec![0_u8; password_len as usize];
            socket.read_exact(&mut password).await.unwrap();
            assert_eq!(username, b"proxy-user");
            assert_eq!(password, b"proxy-pass");
            socket.write_all(&[1, 0]).await.unwrap();

            let mut request = [0_u8; 4];
            socket.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..3], &[5, 1, 0]);
            assert_eq!(request[3], 3, "socks5 must be normalized to proxy DNS");
            let hostname_len = socket.read_u8().await.unwrap();
            let mut hostname = vec![0_u8; hostname_len as usize];
            socket.read_exact(&mut hostname).await.unwrap();
            let port = socket.read_u16().await.unwrap();
            assert_eq!(hostname, b"unit.test");
            assert_eq!(port, 18080);
            socket
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();

            let mut received = Vec::new();
            let mut buffer = [0_u8; 512];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0, "request closed before HTTP headers");
                received.extend_from_slice(&buffer[..count]);
            }
            assert!(received.starts_with(b"GET /probe HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .await
                .unwrap();
        });

        let config = PluginConfig::default();
        let proxy_url = format!("socks5://proxy-user:proxy-pass@{proxy_addr}");
        let client = build_client(&config, Some(&proxy_url)).unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            client.get("http://unit.test:18080/probe").send(),
        )
        .await
        .expect("SOCKS request timed out")
        .expect("SOCKS request failed");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "OK");
        proxy.await.unwrap();
    }
}
