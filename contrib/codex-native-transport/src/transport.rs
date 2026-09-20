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
    /// 出口池行号（1 起；0 = 非出口池请求）。同一代理 URL 填多行时每行独立 client /
    /// 独立连接，配合「按连接轮换出口」的网关，每行就是一个独立出口。
    slot: usize,
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

    /// 清理已删除账号隔离的连接；account=0 是全局共享 client，始终保留。
    pub fn retain_accounts(&self, existing: &std::collections::HashSet<i64>) -> usize {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = clients.len();
        clients.retain(|key, _| key.account == 0 || existing.contains(&key.account));
        before.saturating_sub(clients.len())
    }

    pub fn client_for(
        &self,
        config: &PluginConfig,
        account_id: i64,
        proxy_url: &str,
    ) -> Result<reqwest::Client, String> {
        self.client_for_slot(config, account_id, proxy_url, 0)
    }

    /// 出口池专用：按 (代理 URL × 行号) 缓存，每行一条独立连接。
    pub fn client_for_slot(
        &self,
        config: &PluginConfig,
        account_id: i64,
        proxy_url: &str,
        slot: usize,
    ) -> Result<reqwest::Client, String> {
        let proxy = {
            let trimmed = proxy_url.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let key = ClientKey {
            account: if config.per_account() { account_id } else { 0 },
            proxy,
            slot,
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

/// 从代理 URL 抽取 host:port，隐去 scheme 与账号密码（面板 / 诊断展示用）。
pub fn proxy_host(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after_scheme
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(after_scheme);
    host.trim_end_matches('/').to_string()
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
        let proxy =
            reqwest::Proxy::all(proxy_url).map_err(|err| format!("invalid proxy URL: {err}"))?;
        builder = builder.proxy(proxy);
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
const CODEX_STRIP_HEADERS: &[&str] = &["version", "accept-encoding", "cookie"];

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
) -> HeaderMap {
    let mut lowered: HashMap<String, &crate::proto::sub2api::plugin::v1::HeaderValues> =
        HashMap::with_capacity(raw.len());
    for (name, values) in raw {
        let name = name.to_ascii_lowercase();
        if codex_backend && CODEX_STRIP_HEADERS.contains(&name.as_str()) {
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
    let message = full_error_chain(err);
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

pub fn full_error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut current = err.source();
    while let Some(cause) = current {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        current = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::sub2api::plugin::v1::HeaderValues;

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

        let headers = ordered_headers(&raw, false);
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

        let codex = ordered_headers(&raw, true);
        assert!(codex.get("version").is_none());
        assert!(codex.get("accept-encoding").is_none());
        assert!(codex.get("cookie").is_none());
        assert!(codex.get("originator").is_some());

        // 非 codex 后端保持透传。
        let other = ordered_headers(&raw, false);
        assert!(other.get("version").is_some());
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
}
