//! 宿主 Sub2API admin API 客户端（供账号智力巡检使用）。
//!
//! 数据来源两拼一（按账号 name join）：
//! - `GET /api/v1/admin/accounts?platform=openai&type=oauth`（**不带 status 过滤**）
//!   → 拿 id / name / status / schedulable / credentials.plan_type；
//! - `GET /api/v1/admin/accounts/data?platform=openai&type=oauth`
//!   → 拿 name / credentials.access_token / credentials.chatgpt_account_id（原文凭据）。
//!
//! access_token 仅内存流转、绝不落盘（与插件既有 bearer 原则一致）。
//! 直连宿主（通常 127.0.0.1:8080，与插件同容器），不过任何代理。

use std::collections::HashMap;
use std::time::Duration;

/// 一个巡检/管理目标号。
///
/// 与真实流量不同，巡检**不要求账号可调度**：暂停/限流/报错的号也必须能列出来，
/// 否则面板看不到「已暂停的号」，也无法把它重新开启。
#[derive(Debug, Clone)]
pub struct AdminTarget {
    pub account_id: i64,
    pub name: String,
    pub plan_type: String,
    pub status: String,
    pub schedulable: bool,
    /// 仅内存流转，绝不落盘。
    pub access_token: String,
    pub chatgpt_account_id: Option<String>,
    pub proxy_url: String,
}

/// 由宿主导出的代理记录拼出 reqwest 可用的代理 URL。
///
/// 与宿主 `proxy_key` 同口径：带认证信息时应使用 socks5h / http 形式，
/// 让 DNS 也在代理侧解析（与真实客户端一致）。
pub fn build_proxy_url(
    protocol: &str,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let scheme = match protocol.trim().to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" => "socks5h",
        "socks4" | "socks4a" => "socks4a",
        "http" | "https" => "http",
        other => return Err(format!("unsupported proxy protocol {other:?}")),
    };
    if host.trim().is_empty() || port == 0 {
        return Err("proxy host/port is empty".to_string());
    }
    if username.is_empty() {
        return Ok(format!("{scheme}://{host}:{port}"));
    }
    if password.is_empty() {
        return Ok(format!("{scheme}://{username}@{host}:{port}"));
    }
    let user = url_encode(username);
    let pass = url_encode(password);
    Ok(format!("{scheme}://{user}:{pass}@{host}:{port}"))
}

fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 直连宿主的 http 客户端（不过代理，短超时）。
fn admin_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
}

/// 枚举全部（含暂停/报错）OpenAI OAuth 账号，并取回可出站身份。
pub async fn fetch_all_targets(base: &str, key: &str) -> Result<Vec<AdminTarget>, String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() || key.trim().is_empty() {
        return Err("未配置 Sub2API 管理 API（admin_api_base / admin_api_key）".to_string());
    }

    // 1) 列表：id / name / status / schedulable / plan_type（分页取全，不带 status 过滤）。
    let mut rows: HashMap<String, (i64, String, String, bool)> = HashMap::new();
    let mut page = 1u32;
    loop {
        let url = format!(
            "{base}/api/v1/admin/accounts?platform=openai&type=oauth&page={page}&page_size=1000"
        );
        let resp = client
            .get(&url)
            .header("x-api-key", key)
            .send()
            .await
            .map_err(|e| format!("list request: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("list status {}", resp.status().as_u16()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("list decode: {e}"))?;
        if body.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
            return Err("list response: code is not 0".to_string());
        }
        let data = body
            .get("data")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| "list response: missing data object".to_string())?;
        let items = data
            .get("items")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let got = items.len();
        for item in &items {
            let (Some(id), Some(name)) = (
                item.get("id").and_then(serde_json::Value::as_i64),
                item.get("name").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let schedulable = item
                .get("schedulable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let plan_type = item
                .get("credentials")
                .and_then(|c| c.get("plan_type"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            rows.insert(name.to_string(), (id, status, plan_type, schedulable));
        }
        let pages = data
            .get("pages")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(1);
        if got == 0 || (page as i64) >= pages {
            break;
        }
        page += 1;
    }

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // 2) 导出：name → access_token / chatgpt_account_id（原文凭据）。
    let url = format!("{base}/api/v1/admin/accounts/data?platform=openai&type=oauth");
    let resp = client
        .get(&url)
        .header("x-api-key", key)
        .send()
        .await
        .map_err(|e| format!("export request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "export status {}（账号导出接口不可用，检查 admin key 与 step-up 设置）",
            resp.status().as_u16()
        ));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("export decode: {e}"))?;
    let accounts = body
        .get("data")
        .and_then(|d| d.get("accounts"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let proxies = body
        .get("data")
        .and_then(|d| d.get("proxies"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();

    let mut proxy_by_key: HashMap<String, String> = HashMap::new();
    for proxy in &proxies {
        let (Some(proxy_key), Some(protocol), Some(host), Some(port)) = (
            proxy.get("proxy_key").and_then(serde_json::Value::as_str),
            proxy.get("protocol").and_then(serde_json::Value::as_str),
            proxy.get("host").and_then(serde_json::Value::as_str),
            proxy
                .get("port")
                .and_then(serde_json::Value::as_u64)
                .and_then(|port| u16::try_from(port).ok()),
        ) else {
            continue;
        };
        let username = proxy
            .get("username")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let password = proxy
            .get("password")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if let Ok(proxy_url) = build_proxy_url(protocol, host, port, username, password) {
            proxy_by_key.insert(proxy_key.to_string(), proxy_url);
        }
    }

    let mut out = Vec::new();
    for acc in &accounts {
        let Some(name) = acc.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some((id, status, plan_type, schedulable)) = rows.get(name) else {
            continue;
        };
        let creds = acc.get("credentials");
        let access_token = creds
            .and_then(|c| c.get("access_token"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let chatgpt_account_id = creds
            .and_then(|c| c.get("chatgpt_account_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        let proxy_url = acc
            .get("proxy_key")
            .and_then(|v| v.as_str())
            .and_then(|key| proxy_by_key.get(key))
            .cloned()
            .unwrap_or_default();
        out.push(AdminTarget {
            account_id: *id,
            name: name.to_string(),
            plan_type: plan_type.clone(),
            status: status.clone(),
            schedulable: *schedulable,
            access_token,
            chatgpt_account_id,
            proxy_url,
        });
    }
    out.sort_by_key(|target| target.account_id);
    Ok(out)
}

/// 暂停 / 开启某账号的调度：`POST /api/v1/admin/accounts/{id}/schedulable`
/// body `{"schedulable": bool}`。直连宿主，不过代理。
pub async fn set_account_schedulable(
    base: &str,
    key: &str,
    account_id: i64,
    schedulable: bool,
) -> Result<(), String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() || key.trim().is_empty() {
        return Err("未配置 Sub2API 管理 API（admin_api_base / admin_api_key）".to_string());
    }
    let url = format!("{base}/api/v1/admin/accounts/{account_id}/schedulable");
    let resp = client
        .post(&url)
        .header("x-api-key", key)
        .json(&serde_json::json!({ "schedulable": schedulable }))
        .send()
        .await
        .map_err(|e| format!("set schedulable request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("set schedulable status {}", resp.status().as_u16()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_is_built_for_all_supported_schemes() {
        assert_eq!(
            build_proxy_url("socks5", "1.2.3.4", 1080, "u", "p").unwrap(),
            "socks5h://u:p@1.2.3.4:1080"
        );
        assert_eq!(
            build_proxy_url("http", "1.2.3.4", 8080, "", "").unwrap(),
            "http://1.2.3.4:8080"
        );
        assert_eq!(
            build_proxy_url("socks5h", "h", 1, "a@b", "p:ss").unwrap(),
            "socks5h://a%40b:p%3Ass@h:1"
        );
        assert!(build_proxy_url("ftp", "h", 1, "", "").is_err());
        assert!(build_proxy_url("socks5", "", 0, "", "").is_err());
    }
}
