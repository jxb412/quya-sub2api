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

use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// 一个待养的目标号：宿主数字 id（= 插件养池格键）+ bearer + chatgpt 账号头。
#[derive(Debug, Clone)]
pub struct WarmTarget {
    pub account_id: i64,
    pub access_token: String,
    pub chatgpt_account_id: Option<String>,
    pub proxy_url: String,
}

/// 直连宿主的 http 客户端（不过代理，短超时）。
fn admin_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()
}

#[derive(Debug)]
struct ExistingAccountPage {
    ids: Vec<i64>,
    page: u32,
    pages: u32,
    total: usize,
}

/// 严格解析账号快照页。同步任务会据此删除本地历史状态，因此任何结构异常都必须报错，
/// 由调用方 fail-open 保留现状，绝不能把异常响应当成“当前没有账号”。
fn parse_existing_account_page(body: &serde_json::Value) -> Result<ExistingAccountPage, String> {
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
        .ok_or_else(|| "list response: missing data.items array".to_string())?;
    let page = data
        .get("page")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| "list response: invalid data.page".to_string())?;
    let pages = data
        .get("pages")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| "list response: invalid data.pages".to_string())?;
    let total = data
        .get("total")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| "list response: invalid data.total".to_string())?;

    let mut ids = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .filter(|id| *id > 0)
            .ok_or_else(|| "list response: item missing valid id".to_string())?;
        ids.push(id);
    }
    Ok(ExistingAccountPage {
        ids,
        page,
        pages,
        total,
    })
}

/// 枚举宿主中所有尚未删除的 OpenAI OAuth 账号。
///
/// 特意不传 status，也不检查 schedulable：暂停、错误、限流中的账号仍然存在，必须保留其
/// turn-state 与连接状态。只有仓储层已经排除的软删除账号才会从完整快照中消失。
pub async fn fetch_existing_oauth_ids(base: &str, key: &str) -> Result<HashSet<i64>, String> {
    let client = admin_client().map_err(|e| format!("build client: {e}"))?;
    let base = base.trim_end_matches('/');
    let mut ids = HashSet::new();
    let mut expected_total = None;
    let mut page = 1u32;
    loop {
        let url = format!(
            "{base}/api/v1/admin/accounts?platform=openai&type=oauth&lite=1&page={page}&page_size=1000"
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
        let parsed = parse_existing_account_page(&body)?;
        if parsed.page != page {
            return Err(format!(
                "list response: requested page {page}, got {}",
                parsed.page
            ));
        }
        match expected_total {
            Some(total) if total != parsed.total => {
                return Err("list response: total changed during snapshot".to_string());
            }
            None => expected_total = Some(parsed.total),
            _ => {}
        }
        ids.extend(parsed.ids);
        if page >= parsed.pages {
            break;
        }
        page += 1;
    }
    if ids.len() != expected_total.unwrap_or_default() {
        return Err(format!(
            "list response: snapshot count {} does not match total {}",
            ids.len(),
            expected_total.unwrap_or_default()
        ));
    }
    Ok(ids)
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
    let url = format!("{base}/api/v1/admin/accounts/data?platform=openai&type=oauth");
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
        if let Ok(lease) =
            crate::proxy_api::build_proxy_url(protocol, host, port, username, password)
        {
            proxy_by_key.insert(proxy_key.to_string(), lease.proxy_url);
        }
    }

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
        let proxy_url = acc
            .get("proxy_key")
            .and_then(serde_json::Value::as_str)
            .and_then(|key| proxy_by_key.get(key))
            .cloned()
            .unwrap_or_default();
        out.push(WarmTarget {
            account_id,
            access_token,
            chatgpt_account_id,
            proxy_url,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_snapshot_includes_unschedulable_and_error_accounts() {
        let body = serde_json::json!({
            "code": 0,
            "message": "success",
            "data": {
                "items": [
                    {"id": 10, "status": "active", "schedulable": true},
                    {"id": 11, "status": "error", "schedulable": false},
                    {"id": 12, "status": "disabled", "schedulable": false}
                ],
                "total": 3,
                "page": 1,
                "page_size": 1000,
                "pages": 1
            }
        });
        let page = parse_existing_account_page(&body).unwrap();
        assert_eq!(page.ids, vec![10, 11, 12]);
        assert_eq!(page.total, 3);
    }

    #[test]
    fn malformed_snapshot_is_rejected_instead_of_treated_as_empty() {
        for body in [
            serde_json::json!({"code": 0, "data": {"pages": 1, "page": 1, "total": 0}}),
            serde_json::json!({"code": 0, "data": {"items": [], "page": 1, "total": 0}}),
            serde_json::json!({"code": 500, "data": {"items": [], "pages": 1, "page": 1, "total": 0}}),
            serde_json::json!({"code": 0, "data": {"items": [{"name": "missing-id"}], "pages": 1, "page": 1, "total": 1}}),
        ] {
            assert!(parse_existing_account_page(&body).is_err());
        }
    }
}
