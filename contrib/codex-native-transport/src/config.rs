//! 插件配置：严格解析（拒绝未知字段），空配置规范化为完整默认值。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PluginConfig {
    /// 强制 HTTP/1.1（默认 false：与真实 Codex 一致走 ALPN h2）。
    pub force_http11: bool,
    /// 连接建立超时（秒）。0 表示不设置——与 Codex 默认 client 一致。
    pub connect_timeout_seconds: u32,
    /// 指纹隔离总开关：
    /// - true  = 一账号一指纹：每个账号有恒定且互不相同的 installation-id，
    ///           Cloudflare cookie jar 也按账号隔离（一账号一设备）。
    /// - false = 统一指纹：所有账号共用同一个稳定 installation-id 和共享 cookie jar
    ///           （整个网关对外表现为同一台设备）。
    /// None 表示旧配置未设置，normalize 时由旧字段推导。
    pub per_account_fingerprint: Option<bool>,
    /// 一并发一套 ID（应对同一 OAuth 账号多路并发触发 server_is_overloaded）：
    /// 开启后，每条出站 Codex 请求签发一套独立标识——
    /// session-id / thread-id / x-client-request-id 共用一个新 UUIDv7（与真实
    /// codex 三头同值的行为一致），installation-id 换成独立随机值，并剥离
    /// x-codex-turn-state 避免脏会话粘连；头与 body client_metadata 同步改写。
    /// 代价：会话延续（turn-state / 远端 compaction 粘连）失效。默认关闭。
    pub one_id_per_request: bool,
    /// 兼容保留（已被 per_account_fingerprint 接管，normalize 时写穿）。
    pub per_account_cookie_jar: bool,
    /// 请求体缓冲上限（MB）。真实 Codex 以 Content-Length 发送完整 JSON 体，
    /// 插件同样先缓冲以保证不退化为 chunked 传输。
    pub max_request_body_mb: u32,
    /// reqwest client 缓存上限（按 账号 × 代理 × 协议 组合缓存，LRU 淘汰）。
    pub max_cached_clients: u32,
    /// 出站身份 Profile。
    pub identity: IdentityConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IdentityConfig {
    /// passthrough | machine
    /// passthrough：不改变宿主已经生成的会话/缓存身份。
    /// machine：按账号种子对下游会话身份做稳定 1:1 假名化，同时保留会话边界。
    pub fingerprint_mode: String,
    /// passthrough | codex_cli | codex_desktop | opencode | pi | custom
    /// passthrough（默认）：完全信任宿主的身份收口，不做边缘改写。
    pub profile: String,
    /// profile=custom 时的 originator。
    pub custom_originator: String,
    /// profile=custom 时的 UA 模板，支持 {version} {os} {terminal} 占位符。
    pub custom_user_agent_template: String,
    /// UA 模板的 {os} 段。
    pub os_segment: String,
    /// UA 模板的 {terminal} 段。
    pub terminal_segment: String,
    /// x-openai-internal-codex-residency 头（空 = 不发送；官方目前仅 "us"）。
    pub residency: String,
    /// machine 模式下移除当前 Codex 已不发送的遗留身份头。
    pub strip_non_native_identity_headers: bool,
    /// machine 模式下根据最终 User-Agent 的系统类型校正 turn metadata sandbox。
    pub align_sandbox_with_user_agent: bool,
    /// 兼容保留（已被顶层 per_account_fingerprint 接管，normalize 时写穿）。
    pub per_account_installation_id: bool,
    /// 派生 installation id 的本地种子（安装后自动生成，一般无需修改）。
    pub installation_id_seed: String,
    /// 版本自动同步（npm registry @openai/codex）。
    pub version_auto_sync: bool,
    /// 同步间隔（小时）。
    pub version_sync_interval_hours: u32,
    /// auto_sync 关闭或尚未同步成功时使用的版本号。
    pub pinned_version: String,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            fingerprint_mode: "passthrough".to_string(),
            profile: "passthrough".to_string(),
            custom_originator: String::new(),
            custom_user_agent_template: String::new(),
            os_segment: crate::identity::default_os_segment().to_string(),
            terminal_segment: crate::identity::default_terminal_segment().to_string(),
            residency: String::new(),
            strip_non_native_identity_headers: true,
            align_sandbox_with_user_agent: true,
            per_account_installation_id: false,
            installation_id_seed: String::new(),
            // 自动同步只会更新 UA/version 文本，不会更新编译进二进制的 TLS/HTTP2
            // 依赖版本；默认关闭，避免产生“版本号已升级、网络栈未升级”的混合特征。
            version_auto_sync: false,
            version_sync_interval_hours: 6,
            pinned_version: "0.153.4".to_string(),
        }
    }
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            force_http11: false,
            connect_timeout_seconds: 0,
            // 多账号网关默认隔离连接池、installation id 与 Cloudflare cookie。
            per_account_fingerprint: Some(true),
            one_id_per_request: false,
            per_account_cookie_jar: true,
            max_request_body_mb: 128,
            max_cached_clients: 256,
            identity: IdentityConfig::default(),
        }
    }
}

const VALID_PROFILES: &[&str] = &[
    "passthrough",
    "codex_cli",
    "codex_desktop",
    "opencode",
    "pi",
    "custom",
];

const VALID_FINGERPRINT_MODES: &[&str] = &["passthrough", "machine"];

impl PluginConfig {
    pub fn parse(raw: &[u8]) -> Result<Self, String> {
        let trimmed_empty = raw.iter().all(|b| b.is_ascii_whitespace());
        let mut config: Self = if raw.is_empty() || trimmed_empty {
            Self::default()
        } else {
            serde_json::from_slice(raw).map_err(|err| format!("invalid config JSON: {err}"))?
        };
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// 补齐系统管理字段。
    fn normalize(&mut self) {
        // 指纹总开关：新安装默认一账号一指纹。显式保存过 false 的旧配置仍保持 false。
        let master = self.per_account_fingerprint.unwrap_or(true);
        self.per_account_fingerprint = Some(master);
        // 写穿到兼容字段，内部逻辑只读总开关。
        self.per_account_cookie_jar = master;
        self.identity.per_account_installation_id = master;
        // 两种模式都需要稳定种子（统一模式派生全局唯一设备 id）。
        if self.identity.installation_id_seed.trim().is_empty() {
            self.identity.installation_id_seed = generate_seed();
        }
    }

    /// 指纹总开关（normalize 后恒为 Some；新安装默认 true = 每账号隔离）。
    pub fn per_account(&self) -> bool {
        self.per_account_fingerprint.unwrap_or(false)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.connect_timeout_seconds > 300 {
            return Err("connect_timeout_seconds must be within 0..=300".to_string());
        }
        if !(1..=1024).contains(&self.max_request_body_mb) {
            return Err("max_request_body_mb must be within 1..=1024".to_string());
        }
        if !(8..=4096).contains(&self.max_cached_clients) {
            return Err("max_cached_clients must be within 8..=4096".to_string());
        }
        if !VALID_PROFILES.contains(&self.identity.profile.as_str()) {
            return Err(format!(
                "identity.profile must be one of {}",
                VALID_PROFILES.join("/")
            ));
        }
        if !VALID_FINGERPRINT_MODES.contains(&self.identity.fingerprint_mode.as_str()) {
            return Err(format!(
                "identity.fingerprint_mode must be one of {}",
                VALID_FINGERPRINT_MODES.join("/")
            ));
        }
        if self.identity.profile == "custom" {
            if self.identity.custom_originator.trim().is_empty() {
                return Err("identity.custom_originator is required for custom profile".to_string());
            }
            if self.identity.custom_user_agent_template.trim().is_empty() {
                return Err(
                    "identity.custom_user_agent_template is required for custom profile"
                        .to_string(),
                );
            }
        }
        match self.identity.residency.trim() {
            "" | "us" => {}
            other => {
                return Err(format!(
                    "identity.residency must be empty or \"us\", got {other:?}"
                ))
            }
        }
        if !(1..=168).contains(&self.identity.version_sync_interval_hours) {
            return Err("identity.version_sync_interval_hours must be within 1..=168".to_string());
        }
        let pinned = self.identity.pinned_version.trim();
        if pinned.is_empty() || pinned.len() > 64 {
            return Err("identity.pinned_version is required".to_string());
        }
        Ok(())
    }

    pub fn normalized_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("serialize plugin config")
    }
}

fn generate_seed() -> String {
    // machine HMAC 与 installation id 派生共用该种子，因此必须来自系统随机源。
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_normalizes_to_defaults() {
        for raw in [b"".as_slice(), b"{}".as_slice()] {
            let parsed = PluginConfig::parse(raw).unwrap();
            // 默认：一账号一指纹，身份会话保持透传，种子自动生成。
            assert_eq!(parsed.per_account_fingerprint, Some(true));
            assert!(parsed.per_account_cookie_jar);
            assert!(parsed.identity.per_account_installation_id);
            assert_eq!(parsed.identity.fingerprint_mode, "passthrough");
            assert!(!parsed.identity.version_auto_sync);
            assert!(!parsed.identity.installation_id_seed.is_empty());
            assert!(!parsed.force_http11);
            assert_eq!(parsed.max_request_body_mb, 128);
        }
    }

    #[test]
    fn master_switch_overrides_legacy_fields() {
        // 打开总开关 → 一账号一指纹，写穿覆盖旧字段。
        let parsed = PluginConfig::parse(
            br#"{"per_account_fingerprint":true,"per_account_cookie_jar":false,"identity":{"per_account_installation_id":false}}"#,
        )
        .unwrap();
        assert!(parsed.per_account());
        assert!(parsed.per_account_cookie_jar);
        assert!(parsed.identity.per_account_installation_id);
    }

    #[test]
    fn config_without_master_switch_uses_safe_per_account_default() {
        // 未规范化配置没有总开关时，按新安装的安全默认值处理。
        let legacy = PluginConfig::parse(
            br#"{"per_account_cookie_jar":true,"identity":{"per_account_installation_id":true}}"#,
        )
        .unwrap();
        assert!(legacy.per_account());
        assert!(legacy.per_account_cookie_jar);
        assert!(legacy.identity.per_account_installation_id);
    }

    #[test]
    fn one_id_per_request_parses_and_defaults_off() {
        assert!(!PluginConfig::parse(b"{}").unwrap().one_id_per_request);
        let parsed = PluginConfig::parse(br#"{"one_id_per_request":true}"#).unwrap();
        assert!(parsed.one_id_per_request);
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(PluginConfig::parse(br#"{"nope":1}"#).is_err());
        assert!(PluginConfig::parse(br#"{"identity":{"nope":1}}"#).is_err());
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(PluginConfig::parse(br#"{"max_request_body_mb":0}"#).is_err());
        assert!(PluginConfig::parse(br#"{"connect_timeout_seconds":301}"#).is_err());
        assert!(PluginConfig::parse(br#"{"max_cached_clients":4}"#).is_err());
        assert!(PluginConfig::parse(br#"{"identity":{"profile":"chrome"}}"#).is_err());
        assert!(PluginConfig::parse(br#"{"identity":{"fingerprint_mode":"full"}}"#).is_err());
        assert!(PluginConfig::parse(br#"{"identity":{"residency":"eu"}}"#).is_err());
    }

    #[test]
    fn custom_profile_requires_originator_and_template() {
        assert!(PluginConfig::parse(br#"{"identity":{"profile":"custom"}}"#).is_err());
        let parsed = PluginConfig::parse(
            br#"{"identity":{"profile":"custom","custom_originator":"my_client","custom_user_agent_template":"my_client/{version}"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.identity.profile, "custom");
    }

    #[test]
    fn seed_is_generated_once_and_stable() {
        let parsed = PluginConfig::parse(b"{}").unwrap();
        assert!(!parsed.identity.installation_id_seed.is_empty());
        // 已有种子不被覆盖
        let json = parsed.normalized_json();
        let reparsed = PluginConfig::parse(&json).unwrap();
        assert_eq!(
            parsed.identity.installation_id_seed,
            reparsed.identity.installation_id_seed
        );
    }

    #[test]
    fn normalized_json_is_complete() {
        let normalized = PluginConfig::default().normalized_json();
        let value: serde_json::Value = serde_json::from_slice(&normalized).unwrap();
        let object = value.as_object().unwrap();
        for key in [
            "force_http11",
            "connect_timeout_seconds",
            "per_account_fingerprint",
            "one_id_per_request",
            "per_account_cookie_jar",
            "max_request_body_mb",
            "max_cached_clients",
            "identity",
        ] {
            assert!(object.contains_key(key), "missing {key}");
        }
        let identity = object["identity"].as_object().unwrap();
        for key in [
            "profile",
            "fingerprint_mode",
            "residency",
            "strip_non_native_identity_headers",
            "align_sandbox_with_user_agent",
            "per_account_installation_id",
            "version_auto_sync",
            "pinned_version",
        ] {
            assert!(identity.contains_key(key), "missing identity.{key}");
        }
    }
}
