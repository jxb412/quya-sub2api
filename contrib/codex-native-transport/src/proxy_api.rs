//! Dynamic SOCKS5 proxy acquisition and connection-safe retry support.

use std::time::Duration;

use futures_util::StreamExt;

use crate::config::PluginConfig;
use crate::transport::ClientCache;

const MAX_API_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyLease {
    pub proxy_url: String,
    pub endpoint: String,
    /// Unique non-zero client-cache slot. Each API acquisition gets a fresh connection even when
    /// the provider returns the same gateway URL, which is required by connection-rotating pools.
    pub slot: usize,
}

pub struct ProxyApiClient {
    client: reqwest::Client,
    next_slot: std::sync::atomic::AtomicUsize,
    next_quya_server: std::sync::atomic::AtomicUsize,
}

impl Default for ProxyApiClient {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
            .expect("build dynamic proxy API client");
        Self {
            client,
            next_slot: std::sync::atomic::AtomicUsize::new(1),
            next_quya_server: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl ProxyApiClient {
    /// Fetch one lease. Error text intentionally excludes the configured URL because it commonly
    /// contains the provider username and password in its query string.
    pub async fn acquire(&self, api_url: &str) -> Result<ProxyLease, String> {
        let response = self
            .client
            .get(api_url)
            .send()
            .await
            .map_err(|_| "proxy API request failed".to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "proxy API returned HTTP {}",
                response.status().as_u16()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_API_RESPONSE_BYTES as u64)
        {
            return Err("proxy API response is too large".to_string());
        }

        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "proxy API response read failed".to_string())?;
            if body.len().saturating_add(chunk.len()) > MAX_API_RESPONSE_BYTES {
                return Err("proxy API response is too large".to_string());
            }
            body.extend_from_slice(&chunk);
        }
        let text = std::str::from_utf8(&body)
            .map_err(|_| "proxy API response is not UTF-8".to_string())?;
        let mut lease = parse_proxy_api_response(text)?;
        lease.slot = self
            .next_slot
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .max(1);
        Ok(lease)
    }

    /// Build one short-lived Yunqiao IPv6 lease. Every acquisition receives a fresh username;
    /// gateways are selected round-robin so retries naturally move to the other server.
    pub fn acquire_quya(&self, servers: &[String], password: &str) -> Result<ProxyLease, String> {
        if servers.is_empty() {
            return Err("Yunqiao random IPv6 gateway list is empty".to_string());
        }
        let index = self
            .next_quya_server
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % servers.len();
        let (host, port) = parse_quya_gateway(&servers[index])?;
        let username = format!("cnt{}", uuid::Uuid::new_v4().simple());
        let mut lease = build_proxy_url("http", &host, port, &username, password)?;
        lease.slot = self
            .next_slot
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .max(1);
        Ok(lease)
    }
}

/// Parse `host:port` or `http://host:port` without accepting embedded credentials or paths.
pub fn parse_quya_gateway(raw: &str) -> Result<(String, u16), String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty Yunqiao gateway".to_string());
    }
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let url = reqwest::Url::parse(&candidate)
        .map_err(|_| "invalid Yunqiao gateway endpoint".to_string())?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(
            "Yunqiao gateway must be http host:port without credentials or path".to_string(),
        );
    }
    let host = url
        .host_str()
        .filter(|host| !host.trim().is_empty())
        .ok_or_else(|| "Yunqiao gateway host is missing".to_string())?
        .to_string();
    let port = url
        .port()
        .filter(|port| *port > 0)
        .ok_or_else(|| "Yunqiao gateway port is missing".to_string())?;
    Ok((host, port))
}

/// Provider response: `host:port:username:password`. The first non-empty line is used.
pub fn parse_proxy_api_response(raw: &str) -> Result<ProxyLease, String> {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| "proxy API returned an empty response".to_string())?;
    let fields: Vec<&str> = line.split(':').collect();
    if fields.len() != 4 {
        return Err("proxy API response must be host:port:username:password".to_string());
    }
    let host = fields[0].trim();
    let port = fields[1]
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| "proxy API response contains an invalid port".to_string())?;
    let username = fields[2].trim();
    let password = fields[3].trim();
    if host.is_empty() || username.is_empty() || password.is_empty() {
        return Err("proxy API response contains an empty field".to_string());
    }
    if [host, username, password]
        .iter()
        .any(|value| value.chars().any(char::is_control))
    {
        return Err("proxy API response contains control characters".to_string());
    }
    build_proxy_url("socks5h", host, port, username, password)
}

/// Build a validated proxy URL while percent-encoding credentials.
pub fn build_proxy_url(
    scheme: &str,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<ProxyLease, String> {
    if !matches!(scheme, "socks5" | "socks5h" | "http" | "https") {
        return Err("unsupported proxy protocol".to_string());
    }
    let authority = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut url = reqwest::Url::parse(&format!("{scheme}://{authority}"))
        .map_err(|_| "invalid proxy endpoint".to_string())?;
    if url.host_str().is_none() {
        return Err("invalid proxy endpoint".to_string());
    }
    if !username.is_empty() || !password.is_empty() {
        url.set_username(username)
            .map_err(|_| "invalid proxy username".to_string())?;
        url.set_password(Some(password))
            .map_err(|_| "invalid proxy password".to_string())?;
    }
    Ok(ProxyLease {
        proxy_url: url.to_string(),
        endpoint: authority,
        slot: 0,
    })
}

pub struct ProxySendResult {
    pub response: reqwest::Response,
    pub proxy_url: String,
    pub used_api: bool,
}

pub enum ProxySendError {
    Api(String),
    ClientBuild(String),
    Upstream(reqwest::Error),
}

async fn send_once(
    clients: &ClientCache,
    config: &PluginConfig,
    account_id: i64,
    proxy_url: &str,
    slot: usize,
    method: &reqwest::Method,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> Result<reqwest::Response, ProxySendError> {
    let client = clients
        .client_for_slot(config, account_id, proxy_url, slot)
        .map_err(ProxySendError::ClientBuild)?;
    client
        .request(method.clone(), url)
        .headers(headers.clone())
        .body(body.to_vec())
        .send()
        .await
        .map_err(ProxySendError::Upstream)
}

#[allow(clippy::too_many_arguments)]
pub async fn send_with_optional_proxy_api(
    api: &ProxyApiClient,
    clients: &ClientCache,
    config: &PluginConfig,
    account_id: i64,
    use_dynamic: bool,
    fallback_proxy_url: &str,
    default_proxy_url: &str,
    default_slot: usize,
    method: &reqwest::Method,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> Result<ProxySendResult, ProxySendError> {
    let dynamic_mode = config.egress_mode_value();
    if !use_dynamic || !matches!(dynamic_mode, "proxy_api" | "quya_random") {
        let response = send_once(
            clients,
            config,
            account_id,
            default_proxy_url,
            default_slot,
            method,
            url,
            headers,
            body,
        )
        .await?;
        return Ok(ProxySendResult {
            response,
            proxy_url: default_proxy_url.to_string(),
            used_api: false,
        });
    }

    let attempts = config.egress_proxy_api_max_attempts().max(1);
    let quya_servers = config.quya_random_servers_list();
    let mut last_error = ProxySendError::Api("dynamic proxy acquisition did not run".to_string());
    for _ in 0..attempts {
        let acquired = if dynamic_mode == "proxy_api" {
            api.acquire(config.egress_proxy_api_url.trim()).await
        } else {
            api.acquire_quya(&quya_servers, config.egress_quya_random_password.trim())
        };
        let lease = match acquired {
            Ok(lease) => lease,
            Err(message) => {
                last_error = ProxySendError::Api(message);
                continue;
            }
        };
        match send_once(
            clients,
            config,
            account_id,
            &lease.proxy_url,
            lease.slot,
            method,
            url,
            headers,
            body,
        )
        .await
        {
            Ok(response) => {
                return Ok(ProxySendResult {
                    response,
                    proxy_url: lease.proxy_url,
                    used_api: true,
                });
            }
            Err(ProxySendError::Upstream(err)) if err.is_connect() => {
                last_error = ProxySendError::Upstream(err);
            }
            Err(ProxySendError::ClientBuild(message)) => {
                last_error = ProxySendError::ClientBuild(message);
            }
            Err(other) => return Err(other),
        }
    }

    if config.egress_proxy_api_fallback_to_account_proxy {
        let response = send_once(
            clients,
            config,
            account_id,
            fallback_proxy_url,
            0,
            method,
            url,
            headers,
            body,
        )
        .await?;
        return Ok(ProxySendResult {
            response,
            proxy_url: fallback_proxy_url.to_string(),
            used_api: false,
        });
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_response_and_escapes_credentials() {
        let lease =
            parse_proxy_api_response("199.190.45.172:9595:user-name:p@ss word\r\n").unwrap();
        assert_eq!(lease.endpoint, "199.190.45.172:9595");
        assert_eq!(
            lease.proxy_url,
            "socks5h://user-name:p%40ss%20word@199.190.45.172:9595"
        );
    }

    #[test]
    fn rejects_empty_malformed_and_invalid_port_responses() {
        for raw in [
            "",
            "host:1080:user",
            "host:nope:user:pass",
            "host:0:user:pass",
            "host:1080::pass",
        ] {
            assert!(parse_proxy_api_response(raw).is_err(), "accepted {raw:?}");
        }
    }

    #[test]
    fn supports_ipv6_when_building_known_proxy_records() {
        let lease = build_proxy_url("socks5", "2604:4300:a:f4::2", 1080, "u", "p").unwrap();
        assert_eq!(lease.endpoint, "[2604:4300:a:f4::2]:1080");
        assert_eq!(lease.proxy_url, "socks5://u:p@[2604:4300:a:f4::2]:1080");
    }

    #[test]
    fn parses_quya_gateway_and_rejects_credentials_or_paths() {
        assert_eq!(
            parse_quya_gateway("142.54.187.42:49000").unwrap(),
            ("142.54.187.42".to_string(), 49000)
        );
        assert_eq!(
            parse_quya_gateway("http://107.150.62.202:49000").unwrap(),
            ("107.150.62.202".to_string(), 49000)
        );
        for raw in [
            "https://142.54.187.42:49000",
            "http://u:p@142.54.187.42:49000",
            "http://142.54.187.42:49000/path",
            "142.54.187.42",
        ] {
            assert!(parse_quya_gateway(raw).is_err(), "accepted {raw:?}");
        }
    }
}
