//! Codex 客户端版本自动同步。
//!
//! 轮询 npm registry 的 `@openai/codex` 包元数据（官方 CLI 的发布渠道），
//! 把 dist-tags.latest 作为当前生效版本，供身份 Profile 的 UA/version 头改写。
//! 拉取失败时保持上一次成功值，从未成功过则回退到配置的 pinned 版本。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::service::SharedState;

const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@openai/codex/latest";
/// 轮询检查粒度（每次醒来检查是否到达配置的同步间隔）。
const WAKE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// 官方版本形态校验（与宿主 NormalizeCodexClientVersion 一致的宽松子集）。
fn is_valid_version(version: &str) -> bool {
    if version.is_empty() || version.len() > 64 {
        return false;
    }
    let mut parts = 0;
    for segment in version.split(&['.', '-'][..]) {
        if segment.is_empty() || !segment.chars().all(|c| c.is_ascii_alphanumeric()) {
            return false;
        }
        parts += 1;
    }
    parts >= 2 && version.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// 最近一次成功同步到的版本（None = 从未成功，使用 pinned）。
#[derive(Default)]
pub struct VersionCache {
    synced: RwLock<Option<String>>,
}

impl VersionCache {
    pub fn synced(&self) -> Option<String> {
        self.synced
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, version: String) {
        let mut guard = self
            .synced
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(version);
    }
}

/// 拉取一次最新版本。独立小 client（不影响业务 client 缓存与其指纹）。
pub async fn fetch_latest_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .get(NPM_LATEST_URL)
        .timeout(Duration::from_secs(15))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("npm registry status {}", response.status()));
    }
    let value: serde_json::Value = response.json().await.map_err(|err| err.to_string())?;
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if !is_valid_version(&version) {
        return Err(format!("invalid version from registry: {version:?}"));
    }
    Ok(version)
}

/// 后台同步循环：启动即尝试一次，此后按配置间隔刷新。
/// 配置热更新（ApplyConfig）后无需重启任务——每次醒来读当前配置。
pub async fn run(state: Arc<SharedState>) {
    let mut last_attempt: Option<tokio::time::Instant> = None;
    loop {
        let config = state.current_config();
        let enabled = config.identity.version_auto_sync && config.identity.profile != "passthrough";
        if enabled {
            let interval = Duration::from_secs(
                u64::from(config.identity.version_sync_interval_hours.max(1)) * 3600,
            );
            let due = last_attempt
                .map(|at| at.elapsed() >= interval)
                .unwrap_or(true);
            if due {
                last_attempt = Some(tokio::time::Instant::now());
                if let Ok(version) = fetch_latest_version().await {
                    state.version.set(version);
                }
            }
        }
        tokio::time::sleep(WAKE_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_version_shapes() {
        assert!(is_valid_version("0.153.4"));
        assert!(is_valid_version("0.154.0-alpha.1"));
        assert!(!is_valid_version(""));
        assert!(!is_valid_version("latest"));
        assert!(!is_valid_version("0.153.4; rm -rf /"));
    }

    #[test]
    fn cache_roundtrip() {
        let cache = VersionCache::default();
        assert!(cache.synced().is_none());
        cache.set("0.154.0".to_string());
        assert_eq!(cache.synced().as_deref(), Some("0.154.0"));
    }
}
