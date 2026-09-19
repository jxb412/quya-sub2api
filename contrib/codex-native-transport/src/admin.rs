//! 内置 admin key 的宿主 admin API 客户端：给"全池主动养池"提供
//! 「枚举可调度 openai oauth 号 + 取 access_token」的能力，给休息编排提供
//! 「读/写账号调度优先级」的能力（优先级排空，不再翻 schedulable）。
//!
//! 数据来源两拼一（按账号 name join）：
//! - `GET /api/v1/admin/accounts?platform=openai&type=oauth&status=active&lite=1`
//!   → 拿 id / name / schedulable（列表接口脱敏，无 token）；
//! - `GET /api/v1/admin/accounts/data?platform=openai&type=oauth&include_proxies=false`
//!   → 拿 name / credentials.access_token / credentials.chatgpt_account_id（导出接口原文凭据）。
//!
//! access_token 仅内存流转、绝不落盘（与插件既有 bearer 原则一致）。
//! 直连宿主（127.0.0.1，不过任何代理）。

use std::collections::HashMap;
use std::time::Duration;

/// 一个待养的目标号：宿主数字 id（= 插件养池格键）+ bearer + chatgpt 账号头。
#[derive(Debug, Clone)]
pub struct WarmTarget {
    pub account_id: i64,
    pub access_token: String,
    pub chatgpt_account_id: Option<String>,
}

/// 直连宿主的 http 客户端（不过代理，短超时）。
fn admin_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()
}

/// 只枚举全部可调度 openai oauth 号的宿主数字 id（不摸凭据）。
/// 供休息编排循环用——它只需要 id 来暂停/恢复调度，无需 access_token，
/// 避免每轮把全池 bearer 都导出一遍。
pub async fn fetch_schedulable_ids(base: &str, key: &str) -> Result<Vec<i64>, String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim_end_matches('/');
    let mut ids = Vec::new();
    let mut page = 1u32;
    loop {
        let url = format!(
            "{base}/api/v1/admin/accounts?platform=openai&type=oauth&status=active&lite=1&page={page}&page_size=1000"
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
        let items = body
            .get("data")
            .and_then(|d| d.get("items"))
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let got = items.len();
        for it in &items {
            // lite 列表本身只含可调度号（暂停的号会从列表消失），这里仍显式校验一层。
            let schedulable = it
                .get("schedulable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !schedulable {
                continue;
            }
            if let Some(id) = it.get("id").and_then(serde_json::Value::as_i64) {
                ids.push(id);
            }
        }
        let pages = body
            .get("data")
            .and_then(|d| d.get("pages"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(1);
        if got == 0 || (page as i64) >= pages {
            break;
        }
        page += 1;
    }
    Ok(ids)
}

/// 枚举全部可调度的 openai oauth 号并取回其 access_token。
pub async fn fetch_targets(base: &str, key: &str) -> Result<Vec<WarmTarget>, String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim_end_matches('/');

    // 1) 列表：id / name / schedulable（分页取全）。
    let mut id_by_name: HashMap<String, i64> = HashMap::new();
    let mut page = 1u32;
    loop {
        let url = format!(
            "{base}/api/v1/admin/accounts?platform=openai&type=oauth&status=active&lite=1&page={page}&page_size=1000"
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
        let items = body
            .get("data")
            .and_then(|d| d.get("items"))
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let got = items.len();
        for it in &items {
            let schedulable = it
                .get("schedulable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !schedulable {
                continue;
            }
            let (Some(id), Some(name)) = (
                it.get("id").and_then(serde_json::Value::as_i64),
                it.get("name").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            id_by_name.insert(name.to_string(), id);
        }
        let pages = body
            .get("data")
            .and_then(|d| d.get("pages"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(1);
        if got == 0 || (page as i64) >= pages {
            break;
        }
        page += 1;
    }

    if id_by_name.is_empty() {
        return Ok(Vec::new());
    }

    // 2) 导出：name → access_token / chatgpt_account_id（原文凭据）。
    let url = format!(
        "{base}/api/v1/admin/accounts/data?platform=openai&type=oauth&include_proxies=false"
    );
    let resp = client
        .get(&url)
        .header("x-api-key", key)
        .send()
        .await
        .map_err(|e| format!("export request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("export status {}", resp.status().as_u16()));
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

    let mut out = Vec::new();
    for acc in &accounts {
        let Some(name) = acc.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(&account_id) = id_by_name.get(name) else {
            continue; // 不在"可调度"列表里，跳过
        };
        let creds = acc.get("credentials");
        let access_token = creds
            .and_then(|c| c.get("access_token"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        let Some(access_token) = access_token else {
            continue; // 无可用 bearer，跳过
        };
        let chatgpt_account_id = creds
            .and_then(|c| c.get("chatgpt_account_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        out.push(WarmTarget {
            account_id,
            access_token,
            chatgpt_account_id,
        });
    }
    Ok(out)
}

/// 读取某账号当前的调度优先级：`GET /api/v1/admin/accounts/{id}` → data.priority。
/// 休息编排在改低优先级前先记下原值，到点写回。直连宿主，不过代理。
pub async fn fetch_account_priority(base: &str, key: &str, account_id: i64) -> Result<i64, String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim_end_matches('/');
    let url = format!("{base}/api/v1/admin/accounts/{account_id}");
    let resp = client
        .get(&url)
        .header("x-api-key", key)
        .send()
        .await
        .map_err(|e| format!("get account request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("get account status {}", resp.status().as_u16()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("get account decode: {e}"))?;
    body.get("data")
        .and_then(|d| d.get("priority"))
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| "get account: missing data.priority".to_string())
}

/// 写某账号的调度优先级：`PUT /api/v1/admin/accounts/{id}` body `{"priority": N}`。
/// 宿主该接口按指针字段做局部更新，只传 priority 不会动名称/凭据/分组等其它字段。
/// 休息编排用它做「优先级排空」：休息时改成 warming_drain_priority（新会话不再分配到该号，
/// 已粘住的老会话不受影响），到点写回原值。直连宿主，不过代理。
pub async fn set_account_priority(
    base: &str,
    key: &str,
    account_id: i64,
    priority: i64,
) -> Result<(), String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim_end_matches('/');
    let url = format!("{base}/api/v1/admin/accounts/{account_id}");
    let resp = client
        .put(&url)
        .header("x-api-key", key)
        .json(&serde_json::json!({ "priority": priority }))
        .send()
        .await
        .map_err(|e| format!("set priority request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("set priority status {}", resp.status().as_u16()));
    }
    Ok(())
}
