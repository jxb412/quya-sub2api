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

/// 账号列表行（不含凭据）：套餐默认规则只需要 id + 套餐类型。
#[derive(Debug, Clone)]
pub struct AccountPlanRow {
    pub account_id: i64,
    pub name: String,
    pub plan_type: String,
}

/// 枚举账号列表（分页取全；name -> id / status / plan_type / schedulable）。
/// **不带 status 过滤**：暂停 / 限流 / 报错的号也要能列出来。
async fn fetch_account_rows(
    client: &reqwest::Client,
    base: &str,
    key: &str,
) -> Result<HashMap<String, (i64, String, String, bool)>, String> {
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
    Ok(rows)
}

/// 只枚举账号列表（不拉凭据导出）：给「套餐默认开关」这类轻量周期任务用。
pub async fn fetch_plan_rows(base: &str, key: &str) -> Result<Vec<AccountPlanRow>, String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() || key.trim().is_empty() {
        return Err("未配置 Sub2API 管理 API（admin_api_base / admin_api_key）".to_string());
    }
    let rows = fetch_account_rows(&client, base, key).await?;
    let mut out: Vec<AccountPlanRow> = rows
        .into_iter()
        .map(
            |(name, (account_id, _status, plan_type, _schedulable))| AccountPlanRow {
                account_id,
                name,
                plan_type,
            },
        )
        .collect();
    out.sort_by_key(|row| row.account_id);
    Ok(out)
}

/// 枚举全部（含暂停/报错）OpenAI OAuth 账号，并取回可出站身份。
pub async fn fetch_all_targets(base: &str, key: &str) -> Result<Vec<AdminTarget>, String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() || key.trim().is_empty() {
        return Err("未配置 Sub2API 管理 API（admin_api_base / admin_api_key）".to_string());
    }

    let rows = fetch_account_rows(&client, base, key).await?;

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

/// 账号模型名单的存储形态。
///
/// - `Whitelist`：新式 `credentials.model_whitelist`（字符串数组）；
/// - `Mapping`：旧式 `credentials.model_mapping`（模型 -> 上游模型 映射）。
///
/// 宿主对两者的判定等价（精确自映射条目也算白名单），摘除与恢复都必须按
/// **账号原本那一份**写回，绝不凭空换形态：写空名单等于放开全部模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelForm {
    Whitelist,
    Mapping,
}

impl ModelForm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Whitelist => "model_whitelist",
            Self::Mapping => "model_mapping",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "model_whitelist" => Some(Self::Whitelist),
            "model_mapping" => Some(Self::Mapping),
            _ => None,
        }
    }
}

/// 账号当前配置的模型名单（形态 + 模型名）。
#[derive(Debug, Clone)]
pub struct AccountModels {
    pub form: ModelForm,
    pub models: Vec<String>,
}

/// 摘除结果：真正被移出名单的模型（账号上本来就有的那些）。
#[derive(Debug, Clone)]
pub struct ModelDropOutcome {
    pub account_name: String,
    pub form: ModelForm,
    pub removed: Vec<String>,
}

/// 从凭据里读出模型名单。两份都不存在时返回 None（= 账号不限模型）。
pub fn account_models(
    credentials: &serde_json::Map<String, serde_json::Value>,
) -> Option<AccountModels> {
    if let Some(items) = credentials
        .get("model_whitelist")
        .and_then(|value| value.as_array())
    {
        let models: Vec<String> = items
            .iter()
            .filter_map(|value| value.as_str())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        return Some(AccountModels {
            form: ModelForm::Whitelist,
            models,
        });
    }
    if let Some(map) = credentials
        .get("model_mapping")
        .and_then(|value| value.as_object())
    {
        let models: Vec<String> = map
            .iter()
            .filter(|(_, value)| value.is_string())
            .map(|(key, _)| key.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        return Some(AccountModels {
            form: ModelForm::Mapping,
            models,
        });
    }
    None
}

fn normalize_drop_list(drop: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in drop {
        let trimmed = value.trim();
        if trimmed.is_empty() || out.iter().any(|seen| seen == trimmed) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out
}

/// 把 `drop` 里的模型从名单里剔除；返回（改后的 credentials，实际剔除的模型）。
pub fn without_models(
    credentials: &serde_json::Map<String, serde_json::Value>,
    form: ModelForm,
    drop: &[String],
) -> (serde_json::Map<String, serde_json::Value>, Vec<String>) {
    let wanted = normalize_drop_list(drop);
    let mut out = credentials.clone();
    let mut removed: Vec<String> = Vec::new();
    match credentials.get(form.as_str()) {
        Some(serde_json::Value::Array(items)) => {
            let mut kept = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str().map(str::trim) {
                    Some(name) if wanted.iter().any(|want| want == name) => {
                        removed.push(name.to_string());
                    }
                    _ => kept.push(item.clone()),
                }
            }
            out.insert(form.as_str().to_string(), serde_json::Value::Array(kept));
        }
        Some(serde_json::Value::Object(map)) => {
            let mut kept = serde_json::Map::new();
            for (key, value) in map {
                if wanted.iter().any(|want| want == key.trim()) {
                    removed.push(key.trim().to_string());
                    continue;
                }
                kept.insert(key.clone(), value.clone());
            }
            out.insert(form.as_str().to_string(), serde_json::Value::Object(kept));
        }
        _ => {}
    }
    (out, removed)
}

/// 把模型按原形态加回名单（已存在的条目不会重复添加）。
/// 名单键本身不存在（账号此刻不限模型）时返回 None —— 凭空建名单会把
/// 账号从「不限模型」变成「只允许这几个」，那是收紧而不是恢复。
pub fn with_models(
    credentials: &serde_json::Map<String, serde_json::Value>,
    form: ModelForm,
    add: &[String],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut out = credentials.clone();
    match credentials.get(form.as_str()) {
        Some(serde_json::Value::Array(items)) => {
            let mut next = items.clone();
            for name in normalize_drop_list(add) {
                if !next
                    .iter()
                    .any(|item| item.as_str().map(str::trim) == Some(name.as_str()))
                {
                    next.push(serde_json::Value::String(name));
                }
            }
            out.insert(form.as_str().to_string(), serde_json::Value::Array(next));
        }
        Some(serde_json::Value::Object(map)) => {
            let mut next = map.clone();
            for name in normalize_drop_list(add) {
                next.entry(name.clone())
                    .or_insert_with(|| serde_json::Value::String(name));
            }
            out.insert(form.as_str().to_string(), serde_json::Value::Object(next));
        }
        _ => return None,
    }
    Some(out)
}

/// `GET /api/v1/admin/accounts/{id}`：取账号名与 credentials（敏感键已被宿主脱敏，
/// 更新时宿主会自动保留原值，所以这里直接把整份 credentials 回传即可）。
async fn fetch_account_credentials(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    account_id: i64,
) -> Result<(String, serde_json::Map<String, serde_json::Value>), String> {
    let url = format!("{base}/api/v1/admin/accounts/{account_id}");
    let resp = client
        .get(&url)
        .header("x-api-key", key)
        .send()
        .await
        .map_err(|e| format!("account detail request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("account detail status {}", resp.status().as_u16()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("account detail decode: {e}"))?;
    let data = body
        .get("data")
        .ok_or_else(|| "account detail: missing data".to_string())?;
    let name = data
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let credentials = data
        .get("credentials")
        .and_then(|value| value.as_object())
        .cloned()
        .ok_or_else(|| "account detail: missing credentials".to_string())?;
    Ok((name, credentials))
}

/// `PUT /api/v1/admin/accounts/{id}`：只回传 credentials。
///
/// 实测宿主对凭据的非敏感键是整体替换、敏感键（access_token / refresh_token 等）
/// 缺省即保留，所以调用方必须把 GET 回来的整份 credentials 改完后原样传回，
/// 否则会把 email / chatgpt_account_id / plan_type 这些字段一起抹掉。
async fn put_account_credentials(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    account_id: i64,
    credentials: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let url = format!("{base}/api/v1/admin/accounts/{account_id}");
    let resp = client
        .put(&url)
        .header("x-api-key", key)
        .json(&serde_json::json!({ "credentials": credentials }))
        .send()
        .await
        .map_err(|e| format!("account update request: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let snippet = resp.text().await.unwrap_or_default();
        let snippet: String = snippet.chars().take(200).collect();
        return Err(format!("account update status {status}: {snippet}"));
    }
    Ok(())
}

/// 把 `models` 从账号的模型名单里摘掉。账号上没有这些模型时返回 Ok(None)。
pub async fn drop_account_models(
    base: &str,
    key: &str,
    account_id: i64,
    models: &[String],
) -> Result<Option<ModelDropOutcome>, String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() || key.trim().is_empty() {
        return Err("未配置 Sub2API 管理 API（admin_api_base / admin_api_key）".to_string());
    }
    let (name, credentials) = fetch_account_credentials(&client, base, key, account_id).await?;
    let Some(current) = account_models(&credentials) else {
        return Ok(None);
    };
    let (next, removed) = without_models(&credentials, current.form, models);
    if removed.is_empty() {
        return Ok(None);
    }
    put_account_credentials(&client, base, key, account_id, &next).await?;
    Ok(Some(ModelDropOutcome {
        account_name: name,
        form: current.form,
        removed,
    }))
}

/// 把之前摘掉的模型加回账号名单（按原形态）。
pub async fn restore_account_models(
    base: &str,
    key: &str,
    account_id: i64,
    models: &[String],
    form: &str,
) -> Result<(), String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() || key.trim().is_empty() {
        return Err("未配置 Sub2API 管理 API（admin_api_base / admin_api_key）".to_string());
    }
    if models.is_empty() {
        return Ok(());
    }
    let (_, credentials) = fetch_account_credentials(&client, base, key, account_id).await?;
    let mut target = ModelForm::parse(form);
    // 账号的名单形态可能在这期间被后台改过：以当前实际存在的那个键为准。
    match account_models(&credentials) {
        Some(current) => {
            if target.is_none() || credentials.get(target.unwrap().as_str()).is_none() {
                target = Some(current.form);
            }
        }
        None => {
            // 现在没有任何名单键：加回去等于把「不限模型」收紧成白名单，跳过。
            return Ok(());
        }
    }
    let target = target.ok_or_else(|| "未知的模型名单形态".to_string())?;
    let Some(next) = with_models(&credentials, target, models) else {
        return Ok(());
    };
    put_account_credentials(&client, base, key, account_id, &next).await
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

    fn creds(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn account_models_reads_both_forms() {
        let mapping = creds(serde_json::json!({
            "email": "a@b.c",
            "model_mapping": {"gpt-6-astra": "gpt-6-astra", "gpt-5.5": "gpt-5.5"}
        }));
        let found = account_models(&mapping).unwrap();
        assert_eq!(found.form, ModelForm::Mapping);
        assert_eq!(found.models.len(), 2);

        let whitelist = creds(serde_json::json!({
            "model_whitelist": ["gpt-6-astra", "gpt-5.5"]
        }));
        let found = account_models(&whitelist).unwrap();
        assert_eq!(found.form, ModelForm::Whitelist);
        assert_eq!(
            found.models,
            vec!["gpt-6-astra".to_string(), "gpt-5.5".to_string()]
        );

        let empty = creds(serde_json::json!({ "email": "a@b.c" }));
        assert!(account_models(&empty).is_none());
    }

    #[test]
    fn without_models_keeps_the_rest_of_credentials() {
        let original = creds(serde_json::json!({
            "email": "a@b.c",
            "plan_type": "pro",
            "model_mapping": {
                "gpt-6-astra": "gpt-6-astra",
                "gpt-5.6-sol": "gpt-5.6-sol",
                "gpt-5.5": "gpt-5.5"
            }
        }));
        let drop = vec!["gpt-6-astra".to_string(), "gpt-5.6-sol".to_string()];
        let (next, removed) = without_models(&original, ModelForm::Mapping, &drop);
        assert_eq!(removed, drop);
        let mapping = next.get("model_mapping").unwrap().as_object().unwrap();
        assert_eq!(mapping.len(), 1);
        assert!(mapping.contains_key("gpt-5.5"));
        // 非模型键一字不动：宿主对凭据是非敏感键整体替换，少一个就丢一个。
        assert_eq!(next.get("email").and_then(|v| v.as_str()), Some("a@b.c"));
        assert_eq!(next.get("plan_type").and_then(|v| v.as_str()), Some("pro"));

        // 账号上本来就没有这些模型：不动账号。
        let (same, removed) = without_models(&original, ModelForm::Mapping, &["gpt-9".to_string()]);
        assert!(removed.is_empty());
        assert_eq!(same, original);
    }

    #[test]
    fn without_models_handles_whitelist_arrays() {
        let original = creds(serde_json::json!({
            "model_whitelist": ["gpt-6-astra", "gpt-5.5", "gpt-5.6-sol"]
        }));
        let (next, removed) = without_models(
            &original,
            ModelForm::Whitelist,
            &["gpt-5.6-sol".to_string(), "gpt-5.6-sol".to_string()],
        );
        assert_eq!(removed, vec!["gpt-5.6-sol".to_string()]);
        let kept: Vec<&str> = next
            .get("model_whitelist")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(kept, vec!["gpt-6-astra", "gpt-5.5"]);
    }

    #[test]
    fn with_models_restores_without_duplicating() {
        let after_drop = creds(serde_json::json!({
            "email": "a@b.c",
            "model_mapping": {"gpt-5.5": "gpt-5.5"}
        }));
        let restored = with_models(
            &after_drop,
            ModelForm::Mapping,
            &["gpt-5.5".to_string(), "gpt-6-astra".to_string()],
        )
        .unwrap();
        let mapping = restored.get("model_mapping").unwrap().as_object().unwrap();
        assert_eq!(mapping.len(), 2);
        assert_eq!(
            mapping.get("gpt-6-astra").and_then(|v| v.as_str()),
            Some("gpt-6-astra")
        );

        // 名单键不存在（账号不限模型）：不凭空建白名单。
        let unrestricted = creds(serde_json::json!({ "email": "a@b.c" }));
        assert!(with_models(
            &unrestricted,
            ModelForm::Mapping,
            &["gpt-6-astra".to_string()]
        )
        .is_none());
    }
}
