//! BPS 403 自动摘除模型。
//!
//! 触发：BPS 端点对某账号回 403（账号级 usage policy 拦截）时，把该账号在宿主
//! 里的模型名单（`credentials.model_whitelist` / `credentials.model_mapping`）中的
//! 指定模型摘掉，宿主调度器随即不再把这些模型派给这个号，同分组其它账号照常接；
//! 摘除时长（默认跟随「BPS 冷却：403」）到点后自动把模型加回去。
//!
//! 只落盘「账号 -> 摘了哪些模型、什么时候恢复」，不含任何凭据。
//! 恢复走宿主 admin API（`PUT /api/v1/admin/accounts/{id}`），失败按指数退避重试。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::PluginConfig;
use crate::service::SharedState;

/// 恢复失败后的退避上限（ms）。
const RESTORE_RETRY_MAX_MS: u64 = 30 * 60 * 1000;
/// 后台巡检间隔（秒）。
const SWEEP_INTERVAL_SECONDS: u64 = 15;

/// 一次「摘除模型」的记录。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DropRecord {
    #[serde(default)]
    pub account_id: i64,
    #[serde(default)]
    pub account_name: String,
    /// 被摘掉的模型（恢复时按这份清单加回）。
    #[serde(default)]
    pub models: Vec<String>,
    /// 原名单形态：`model_whitelist` / `model_mapping`。
    #[serde(default)]
    pub form: String,
    #[serde(default)]
    pub dropped_at_ms: u64,
    /// 计划恢复时间（ms）。
    #[serde(default)]
    pub until_ms: u64,
    #[serde(default)]
    pub restore_failures: u32,
    #[serde(default)]
    pub next_retry_ms: u64,
    #[serde(default)]
    pub last_error: String,
}

#[derive(Default)]
pub struct ModelDropStore {
    inner: Mutex<DropInner>,
}

#[derive(Default)]
struct DropInner {
    records: BTreeMap<i64, DropRecord>,
    loaded: bool,
}

impl ModelDropStore {
    /// 首次使用时从落盘文件加载（路径为空 = 不落盘）。
    pub fn ensure_loaded(&self, path: &str) {
        {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.loaded {
                return;
            }
        }
        let mut records = BTreeMap::new();
        let path = path.trim();
        if !path.is_empty() {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(raw) = serde_json::from_slice::<BTreeMap<String, DropRecord>>(&bytes) {
                    for (key, mut record) in raw {
                        let id = key.parse::<i64>().ok().unwrap_or(record.account_id);
                        if id == 0 {
                            continue;
                        }
                        record.account_id = id;
                        records.insert(id, record);
                    }
                }
            }
        }
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.loaded = true;
        if guard.records.is_empty() {
            guard.records = records;
        }
    }

    pub fn snapshot(&self) -> Vec<DropRecord> {
        self.lock().records.values().cloned().collect()
    }

    pub fn get(&self, account_id: i64) -> Option<DropRecord> {
        self.lock().records.get(&account_id).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DropInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn upsert(&self, record: DropRecord, path: &str) {
        self.lock().records.insert(record.account_id, record);
        self.persist(path);
    }

    fn remove(&self, account_id: i64, path: &str) -> Option<DropRecord> {
        let removed = self.lock().records.remove(&account_id);
        if removed.is_some() {
            self.persist(path);
        }
        removed
    }

    fn note_restore_failure(&self, account_id: i64, err: &str, path: &str) {
        let now = crate::donor::now_ms();
        {
            let mut guard = self.lock();
            if let Some(record) = guard.records.get_mut(&account_id) {
                record.restore_failures = record.restore_failures.saturating_add(1);
                let delay = (30_000u64)
                    .saturating_mul(1u64 << record.restore_failures.min(6))
                    .min(RESTORE_RETRY_MAX_MS);
                record.next_retry_ms = now.saturating_add(delay);
                record.last_error = err.to_string();
            } else {
                return;
            }
        }
        self.persist(path);
    }

    fn persist(&self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let payload: BTreeMap<String, DropRecord> = self
            .lock()
            .records
            .iter()
            .map(|(id, record)| (id.to_string(), record.clone()))
            .collect();
        let Ok(bytes) = serde_json::to_vec_pretty(&payload) else {
            return;
        };
        if let Some(dir) = std::path::Path::new(path).parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let _ = std::fs::write(path, bytes);
    }
}

/// BPS 403：把配置里的模型从这个账号的宿主名单上摘掉（异步执行，不阻塞请求）。
pub fn note_bps_403(state: &Arc<SharedState>, config: &PluginConfig, account_id: i64) {
    if !config.bps_403_drop_models_enabled {
        return;
    }
    let seconds = config.bps_403_drop_seconds();
    if seconds == 0 {
        return;
    }
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    if base.is_empty() || key.is_empty() {
        return;
    }
    let models = config.bps_403_drop_model_list();
    let path = config.model_drop_state_file();
    let config = config.clone();
    let state = Arc::clone(state);
    tokio::spawn(async move {
        apply_drop(
            &state, &config, &base, &key, &path, account_id, &models, seconds,
        )
        .await;
    });
}

async fn apply_drop(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    base: &str,
    key: &str,
    path: &str,
    account_id: i64,
    models: &[String],
    seconds: u32,
) {
    state.model_drop.ensure_loaded(path);
    let existing = state.model_drop.get(account_id);
    let now = crate::donor::now_ms();
    let until = now.saturating_add(seconds as u64 * 1000);
    match crate::admin::drop_account_models(base, key, account_id, models).await {
        Ok(Some(outcome)) => {
            let mut merged: Vec<String> = existing
                .as_ref()
                .map(|record| record.models.clone())
                .unwrap_or_default();
            for name in &outcome.removed {
                if !merged.contains(name) {
                    merged.push(name.clone());
                }
            }
            let record = DropRecord {
                account_id,
                account_name: if outcome.account_name.is_empty() {
                    existing
                        .as_ref()
                        .map(|record| record.account_name.clone())
                        .unwrap_or_default()
                } else {
                    outcome.account_name
                },
                models: merged,
                form: outcome.form.as_str().to_string(),
                dropped_at_ms: existing
                    .as_ref()
                    .map(|record| record.dropped_at_ms)
                    .filter(|value| *value > 0)
                    .unwrap_or(now),
                until_ms: existing
                    .as_ref()
                    .map(|record| record.until_ms)
                    .unwrap_or(0)
                    .max(until),
                restore_failures: 0,
                next_retry_ms: 0,
                last_error: String::new(),
            };
            let text = format!(
                "403 自动摘除模型 acc={account_id} 模型={} 形式={} {}s 后恢复",
                record.models.join(","),
                record.form,
                record.until_ms.saturating_sub(now) / 1000
            );
            crate::bps::note(config, account_id, &text);
            eprintln!("[codex-native-transport] {text}");
            state.model_drop.upsert(record, path);
        }
        Ok(None) => {
            // 账号上本来就没配这些模型：已经摘过的号只需把恢复时间往后推。
            if let Some(mut record) = existing {
                record.until_ms = record.until_ms.max(until);
                record.restore_failures = 0;
                record.next_retry_ms = 0;
                let text = format!(
                    "403 自动摘除模型 acc={account_id}（已摘过，恢复时间顺延 {}s）",
                    record.until_ms.saturating_sub(now) / 1000
                );
                crate::bps::note(config, account_id, &text);
                state.model_drop.upsert(record, path);
            }
        }
        Err(err) => {
            let text = format!("403 自动摘除模型失败 acc={account_id}: {err}");
            crate::bps::note(config, account_id, &text);
            eprintln!("[codex-native-transport] {text}");
        }
    }
}

/// 后台巡检：到点的记录把模型加回账号；关掉功能 = 立刻全部恢复。
pub async fn run(state: Arc<SharedState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECONDS)).await;
        let config = state.current_config();
        let path = config.model_drop_state_file();
        if path.trim().is_empty() {
            continue;
        }
        state.model_drop.ensure_loaded(&path);
        let base = config.admin_api_base.trim().to_string();
        let key = config.admin_api_key.trim().to_string();
        let disabled = !config.bps_403_drop_models_enabled;
        let now = crate::donor::now_ms();
        let due: Vec<DropRecord> = state
            .model_drop
            .snapshot()
            .into_iter()
            .filter(|record| (disabled || now >= record.until_ms) && now >= record.next_retry_ms)
            .collect();
        if due.is_empty() {
            continue;
        }
        if base.is_empty() || key.is_empty() {
            continue;
        }
        for record in due {
            match restore_one(&state, &config, &base, &key, &path, &record).await {
                Ok(()) => {}
                Err(err) => {
                    let text = format!(
                        "403 摘除模型恢复失败 acc={} ({}) : {err}",
                        record.account_id, record.account_name
                    );
                    crate::bps::note(&config, record.account_id, &text);
                    eprintln!("[codex-native-transport] {text}");
                    state
                        .model_drop
                        .note_restore_failure(record.account_id, &err, &path);
                }
            }
        }
    }
}

/// 面板手动恢复 / 巡检到点恢复共用的单账号恢复。
pub async fn restore_one(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    base: &str,
    key: &str,
    path: &str,
    record: &DropRecord,
) -> Result<(), String> {
    if let Err(err) = crate::admin::restore_account_models(
        base,
        key,
        record.account_id,
        &record.models,
        &record.form,
    )
    .await
    {
        if account_gone(&err) {
            // 账号已被删除：宿主上已经没有这份名单可恢复，摘除记录直接丢弃。
            // 留着的话巡检每轮都重试一次、永远失败——现网见过一个删掉的号让
            // 日志刷了 600 多条「恢复失败 ... account detail status 404」。
            state.model_drop.remove(record.account_id, path);
            let text = format!(
                "403 摘除记录丢弃 acc={}（账号已不存在，无需恢复）",
                record.account_id
            );
            crate::bps::note(config, record.account_id, &text);
            eprintln!("[codex-native-transport] {text}");
            return Ok(());
        }
        return Err(err);
    }
    state.model_drop.remove(record.account_id, path);
    let text = format!(
        "403 摘除模型已恢复 acc={} 模型={}",
        record.account_id,
        record.models.join(",")
    );
    crate::bps::note(config, record.account_id, &text);
    eprintln!("[codex-native-transport] {text}");
    Ok(())
}

/// 宿主明确回「账号不存在」：这类失败重试多少次都不会成功，按陈旧记录丢弃。
fn account_gone(err: &str) -> bool {
    err.contains("account detail status 404") || err.contains("account detail status 410")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("cnt-model-drop-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("model-drop.json");
        let path_str = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);

        let store = ModelDropStore::default();
        store.ensure_loaded(&path_str);
        store.upsert(
            DropRecord {
                account_id: 42,
                account_name: "oauth---a@b.c".to_string(),
                models: vec!["gpt-6-astra".to_string(), "gpt-5.6-sol".to_string()],
                form: "model_mapping".to_string(),
                dropped_at_ms: 1_000,
                until_ms: 2_000,
                ..DropRecord::default()
            },
            &path_str,
        );
        assert_eq!(store.snapshot().len(), 1);

        let reloaded = ModelDropStore::default();
        reloaded.ensure_loaded(&path_str);
        let record = reloaded.get(42).unwrap();
        assert_eq!(record.models, vec!["gpt-6-astra", "gpt-5.6-sol"]);
        assert_eq!(record.form, "model_mapping");
        assert_eq!(record.until_ms, 2_000);

        reloaded.remove(42, &path_str);
        assert!(reloaded.snapshot().is_empty());
        let again = ModelDropStore::default();
        again.ensure_loaded(&path_str);
        assert!(again.snapshot().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_failure_backs_off() {
        let dir = std::env::temp_dir().join(format!("cnt-model-drop-b-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("model-drop.json");
        let path_str = path.to_string_lossy().to_string();
        let store = ModelDropStore::default();
        store.ensure_loaded(&path_str);
        store.upsert(
            DropRecord {
                account_id: 7,
                until_ms: 1,
                ..DropRecord::default()
            },
            &path_str,
        );
        store.note_restore_failure(7, "boom", &path_str);
        let record = store.get(7).unwrap();
        assert_eq!(record.restore_failures, 1);
        assert!(record.next_retry_ms > crate::donor::now_ms());
        assert_eq!(record.last_error, "boom");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 宿主回「账号不存在」时不再无限重试：识别为陈旧记录后丢弃。
    #[test]
    fn stale_record_is_recognized_when_account_is_gone() {
        assert!(account_gone("account detail status 404"));
        assert!(account_gone("account detail status 410"));
        assert!(!account_gone("account detail status 500"));
        assert!(!account_gone(
            "account update status 404: {\"error\":\"x\"}"
        ));
        assert!(!account_gone("account detail request: connection refused"));
    }
}
