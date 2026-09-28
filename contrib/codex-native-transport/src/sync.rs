//! 跨机状态同步（默认关）。
//!
//! 两台服务器共用同一批账号（同一份 PG）时，同一个账号在两台上是同一个上游身份，
//! 面板上那个「自动降智处理」开关理应对两台都生效。这里用插件自己内嵌的面板端口
//! 做一条实例间的同步链路：
//!
//! * `sync_peers` 里写对端面板地址（`http://host:8848`），`sync_token` 是共享密钥；
//! * 本机改动（面板点开关）立刻推全量快照，另外每 `sync_interval_seconds` 对一次账；
//! * 收到对端快照按 `enabled_at_ms` 取新（last-write-wins），**收到的不再转发**，
//!   所以两台互推也不会震荡；
//! * 线路统计（normal_ok / bps_ok …）是每台自己的流量，不参与同步。
//!
//! 对端不可达时只在内存里记一条失败报告（面板 /api/status 里能看到），不影响
//! 转发链路：这条链路失败绝不会影响客户端请求。
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

use crate::config::PluginConfig;
use crate::donor::now_ms;
use crate::service::SharedState;

/// 对端推送用的鉴权头（值 = sync_token，空则复用 panel_token）。
pub const SYNC_TOKEN_HEADER: &str = "x-cnt-sync-token";
/// 面板读请求体的上限（快照比这个大就直接放弃推送）。
pub const MAX_SNAPSHOT_BYTES: usize = 512 * 1024;
/// 推送/对账单次超时。
const PUSH_TIMEOUT: Duration = Duration::from_secs(15);
/// 本地连续改动合并窗口（面板批量勾选会连点很多次）。
const COALESCE: Duration = Duration::from_millis(300);

fn wake() -> &'static Arc<Notify> {
    static WAKE: OnceLock<Arc<Notify>> = OnceLock::new();
    WAKE.get_or_init(|| Arc::new(Notify::new()))
}

/// 通知后台任务「本地状态变了，尽快推一次」。可在任意线程调用。
pub fn notify() {
    wake().notify_one();
}

#[derive(Clone, Debug)]
struct PeerResult {
    base: String,
    ok: bool,
    detail: String,
}

#[derive(Clone, Debug, Default)]
struct Report {
    at_ms: u64,
    enabled: bool,
    peer_count: usize,
    results: Vec<PeerResult>,
}

fn report_slot() -> &'static Mutex<Option<Report>> {
    static SLOT: Mutex<Option<Report>> = Mutex::new(None);
    &SLOT
}

fn store_report(report: &Report) {
    if let Ok(mut guard) = report_slot().lock() {
        *guard = Some(report.clone());
    }
}

/// 面板 /api/status 用的同步状态。
pub fn status_json() -> serde_json::Value {
    let report = report_slot().lock().ok().and_then(|guard| guard.clone());
    match report {
        Some(report) => {
            let results: Vec<serde_json::Value> = report
                .results
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "peer": row.base,
                        "ok": row.ok,
                        "detail": row.detail,
                    })
                })
                .collect();
            serde_json::json!({
                "last_push_ms": report.at_ms,
                "last_push_enabled": report.enabled,
                "peer_count": report.peer_count,
                "results": results,
            })
        }
        None => serde_json::json!({ "last_push_ms": 0, "peer_count": 0, "results": [] }),
    }
}

/// 本机开关快照（全量）。
pub fn snapshot_json(state: &Arc<SharedState>, config: &PluginConfig) -> serde_json::Value {
    let mut accounts = serde_json::Map::new();
    for (account_id, enabled, at_ms, reason) in
        state.degrade.sync_entries(&config.degrade_state_file())
    {
        accounts.insert(
            account_id.to_string(),
            serde_json::json!({ "enabled": enabled, "at_ms": at_ms, "reason": reason }),
        );
    }
    serde_json::json!({
        "version": 1,
        "kind": "degrade",
        "sent_at_ms": now_ms(),
        "accounts": accounts,
    })
}

/// 应用对端推来的快照，返回面板要回给人的 JSON。
pub fn apply_remote_json(state: &Arc<SharedState>, body: &[u8]) -> String {
    let parsed: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(err) => {
            return format!(
                "{{\"ok\":false,\"error\":{}}}",
                serde_json::Value::String(format!("invalid snapshot: {err}"))
            )
        }
    };
    let Some(accounts) = parsed
        .get("accounts")
        .and_then(serde_json::Value::as_object)
    else {
        return "{\"ok\":false,\"error\":\"accounts missing\"}".to_string();
    };
    let config = state.current_config();
    let path = config.degrade_state_file();
    let mut seen = 0usize;
    let mut applied = 0usize;
    for (key, value) in accounts {
        let Ok(account_id) = key.parse::<i64>() else {
            continue;
        };
        let enabled = value
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let at_ms = value
            .get("at_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let reason = value
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        seen += 1;
        if state
            .degrade
            .set_remote(account_id, enabled, at_ms, reason, &path)
        {
            applied += 1;
        }
    }
    serde_json::json!({ "ok": true, "seen": seen, "applied": applied }).to_string()
}

/// 把本机快照推给所有对端，返回本次报告。
pub async fn push_all(state: &Arc<SharedState>) -> Report {
    let config = state.current_config();
    let peers = config.sync_peer_list();
    let mut report = Report {
        at_ms: now_ms(),
        enabled: config.sync_enabled,
        peer_count: peers.len(),
        results: Vec::new(),
    };
    if !config.sync_enabled || !config.sync_push || peers.is_empty() {
        store_report(&report);
        return report;
    }
    let token = config.sync_auth_token();
    let body = match serde_json::to_vec(&snapshot_json(state, &config)) {
        Ok(body) => body,
        Err(err) => {
            report.results.push(PeerResult {
                base: "(snapshot)".to_string(),
                ok: false,
                detail: format!("序列化快照失败: {err}"),
            });
            store_report(&report);
            return report;
        }
    };
    if body.len() > MAX_SNAPSHOT_BYTES {
        report.results.push(PeerResult {
            base: "(snapshot)".to_string(),
            ok: false,
            detail: format!("快照 {} 字节超过上限，已跳过推送", body.len()),
        });
        store_report(&report);
        return report;
    }
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(PUSH_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            report.results.push(PeerResult {
                base: "(client)".to_string(),
                ok: false,
                detail: format!("HTTP 客户端创建失败: {err}"),
            });
            store_report(&report);
            return report;
        }
    };
    for base in peers {
        let url = format!("{base}/api/state/push");
        let outcome = client
            .post(&url)
            .header(SYNC_TOKEN_HEADER, token.as_str())
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await;
        match outcome {
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                let snippet: String = text.chars().take(200).collect();
                report.results.push(PeerResult {
                    base: base.clone(),
                    ok: status.is_success(),
                    detail: format!("HTTP {} {}", status.as_u16(), snippet.trim()),
                });
            }
            Err(err) => report.results.push(PeerResult {
                base: base.clone(),
                ok: false,
                detail: crate::transport::safe_reqwest_error(&err),
            }),
        }
    }
    store_report(&report);
    report
}

/// 后台同步循环：本地改动立刻推；`sync_interval_seconds` > 0 时按周期对账。
pub async fn run(state: Arc<SharedState>) {
    loop {
        let interval = state.current_config().sync_interval_seconds.min(3600) as u64;
        let notified = wake().notified();
        if interval == 0 {
            notified.await;
        } else {
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            }
        }
        if !state.current_config().sync_enabled || !state.current_config().sync_push {
            // 同步关着，或本机是纯接收端：把通知消化掉，别空转。
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        tokio::time::sleep(COALESCE).await;
        let report = push_all(&state).await;
        if let Some(row) = report.results.iter().find(|row| !row.ok) {
            eprintln!(
                "[codex-native-transport] sync push to {} failed: {}",
                row.base, row.detail
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::SharedState;

    fn state() -> Arc<SharedState> {
        SharedState::new()
    }

    fn temp_path(name: &str) -> String {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        path.to_string_lossy().to_string()
    }

    #[test]
    fn snapshot_and_apply_round_trip_converges_on_newest() {
        // 两台各自的落盘路径（同一个路径会互相读到对方的文件）。
        let path_a = temp_path("cnt-sync-roundtrip-a.json");
        let path_b = temp_path("cnt-sync-roundtrip-b.json");
        let a = state();
        let b = state();
        // A 在 t=1000 打开 54；B 那边是空状态。
        a.degrade.set_remote(54, true, 1000, "", &path_a);
        b.degrade.ensure_loaded(&path_b);
        assert!(!b.degrade.is_enabled(54));

        // A 的快照推到 B：B 采纳。
        let mut cfg_a = PluginConfig::default();
        cfg_a.degrade_state_path = path_a.clone();
        let mut cfg_b = PluginConfig::default();
        cfg_b.degrade_state_path = path_b.clone();
        let snap = serde_json::to_vec(&snapshot_json(&a, &cfg_a)).unwrap();
        let report: serde_json::Value =
            serde_json::from_str(&apply_remote_json(&b, &snap)).unwrap();
        assert_eq!(report["applied"], 1);
        assert!(b.degrade.is_enabled(54));
        // 幂等：同样的快照再来一次不再改动。
        let report: serde_json::Value =
            serde_json::from_str(&apply_remote_json(&b, &snap)).unwrap();
        assert_eq!(report["applied"], 0);

        // B 在 t=2000 关掉 54，A 的旧时间戳不能把它盖回来。
        b.degrade.set_remote(54, false, 2000, "", &path_b);
        assert!(!b.degrade.is_enabled(54));
        let snap_b = serde_json::to_vec(&snapshot_json(&b, &cfg_b)).unwrap();
        let report: serde_json::Value =
            serde_json::from_str(&apply_remote_json(&a, &snap_b)).unwrap();
        assert_eq!(report["applied"], 1);
        assert!(!a.degrade.is_enabled(54));

        // 再把 A 的旧快照推回 B：时间戳更旧，保持关闭。
        let report: serde_json::Value =
            serde_json::from_str(&apply_remote_json(&b, &snap)).unwrap();
        assert_eq!(report["applied"], 0);
        assert!(!b.degrade.is_enabled(54));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    #[test]
    fn legacy_entries_lose_to_any_explicit_change_but_tie_prefers_enabled() {
        let path = temp_path("cnt-sync-legacy.json");
        std::fs::write(&path, br#"{"54":{"enabled":true,"failed":true}}"#).unwrap();

        let store = state();
        let entries = store.degrade.sync_entries(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 54);
        assert!(entries[0].1);
        assert_eq!(entries[0].2, 1, "旧格式按最旧时间戳处理");

        // 显式关闭（now_ms 级别）能盖过旧格式。
        store
            .degrade
            .set_remote(54, false, 1_790_000_000_000, "", &path);
        assert!(!store.degrade.is_enabled(54));
        // 时间戳相同的「开」优先（两台都是旧格式时以勾上的那台为准）。
        store
            .degrade
            .set_remote(54, true, 1_790_000_000_000, "", &path);
        assert!(store.degrade.is_enabled(54));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bad_snapshots_are_rejected_without_touching_state() {
        let store = state();
        assert!(apply_remote_json(&store, b"not json").contains("\"ok\":false"));
        assert!(apply_remote_json(&store, b"{\"version\":1}").contains("\"ok\":false"));
        assert!(apply_remote_json(&store, b"{\"accounts\":{}}").contains("\"ok\":true"));
    }
}
