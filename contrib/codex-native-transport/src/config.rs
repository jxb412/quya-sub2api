//! 插件配置：严格解析（拒绝未知字段），空配置规范化为完整默认值。

use serde::{Deserialize, Serialize};

/// 与宿主/前端账号套餐展示口径一致：忽略大小写及空格/下划线/连字符，合并已知别名。
pub fn normalize_plan_type(value: &str) -> String {
    let compact: String = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
        .collect();
    match compact.as_str() {
        "chatgptpro" => "pro".to_string(),
        "selfservebusinessprolite" => "self_serve_business_prolite".to_string(),
        "" => String::new(),
        other => other.to_string(),
    }
}

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
    /// 内嵌管理面板监听地址（空 = 关闭面板）。
    /// 例：`127.0.0.1:8848`（仅本机）、`0.0.0.0:8848`（对外，务必配强 token）。
    pub panel_addr: String,
    /// 面板访问 token（空 = 面板不启动，绝不无鉴权暴露）。支持 `?token=` 或 Bearer。
    pub panel_token: String,
    /// 宿主 Sub2API admin API 基址（插件通常与宿主同容器：http://127.0.0.1:8080）。
    pub admin_api_base: String,
    /// 宿主 admin API key（请求头 `x-api-key`）。仅内存流转，随配置加密存储于 DB。
    pub admin_api_key: String,
    /// 账号智力巡检（降智检测）总开关。默认关。
    pub intel_enabled: bool,
    /// 巡检关注的套餐类型（空 = 全部）。比较前按 normalize_plan_type 归一化。
    pub intel_plan_types: Vec<String>,
    /// 巡检使用的模型（空 = gpt-6-astra）。
    pub intel_model: String,
    /// 巡检提问（空 = 默认知识库日期问题）。
    pub intel_prompt: String,
    /// 判不合格的标记（空 = 2024）：回答里出现该字符串即判该号当前被降智。
    pub intel_fail_marker: String,
    /// 自动循环巡检开关。关 = 只在面板里手动触发。
    pub intel_loop_enabled: bool,
    /// 自动巡检间隔（秒，钳到 60..=86400）。
    pub intel_loop_interval_seconds: u32,
    /// 巡检并发（钳到 1..=16）。
    pub intel_concurrency: u32,
    /// 不合格 → 自动暂停该号调度；恢复合格 → 自动开启调度。
    /// 面板里对单个账号开启「自动降智处理」后，该号不参与自动暂停（改走降智通道）。
    pub intel_auto_pause: bool,
    /// 巡检结论落盘路径（空 = 不落盘）。建议指向挂载卷。
    pub intel_state_path: String,
    /// 巡检单次请求超时（秒，5..=600）。
    pub intel_prompt_timeout_seconds: u32,
    /// 巡检失败重试次数（0..=5，总尝试次数 = 该值 + 1）。
    pub intel_prompt_retries: u32,
    /// 巡检超时是否按「不合格」处理（默认 true）。
    pub intel_timeout_is_failed: bool,
    /// 连续合格多少次才允许切回正常通道（1..=10）。
    pub intel_confirmations: u32,
    /// 巡检回答里必须出现年份（19xx/20xx），否则判不合格。
    ///
    /// 降智号除了答「2024」，还会答「我不知道自己的知识截止日期」这种不含
    /// 关键词的敷衍话；只靠关键词会把这种号误判成合格，通道又切回正常。
    pub intel_require_year: bool,
    /// 判定合格后仍在 BPS 通道保持多久（秒，0 = 立刻允许切回）。
    pub bps_hold_after_healthy_seconds: u32,
    /// 会话粘滞：同一会话用过 BPS 之后，空闲多久内继续走 BPS（秒，0 = 关闭）。
    pub bps_session_sticky_seconds: u32,
    /// BPS 出站失败后的冷却（秒）：冷却期内该号不再尝试 BPS。
    pub bps_fallback_cooldown_seconds: u32,
    /// 降智账号 BPS 通道总开关（默认关）。开启后，面板里勾了
    /// 「自动降智处理」的账号，其 bps_models 请求改走 BPS 端点。
    pub bps_enabled: bool,
    /// BPS 端点地址。
    pub bps_endpoint: String,
    /// 走 BPS 通道的模型列表（空 = gpt-6-astra + gpt-5.6-sol）。
    pub bps_models: Vec<String>,
    /// BPS 工具桥接方案（默认 text）：
    /// text     = 客户端工具目录 + 一行 JSON 文本协议，历史工具项转文本（默认）；
    /// native   = 历史 function_call / function_call_output 保持原生 item 形状回放，
    ///            新调用仍走文本协议（ghcp_proxy 的 native item 路线）；
    /// officejs = 历史原生回放 + 让模型用上游 Excel 插件的 run_officejs 当运货卡车，
    ///            代理从 code 字段还原真实客户端工具调用。
    pub bps_tool_mode: String,
    /// 客户端工具目录放到提示词末尾（默认 false = 目录前置 + 末尾短提醒）。
    pub bps_catalog_at_prompt_end: bool,
    /// 是否把客户端的 prompt_cache_key 转发给 BPS（默认 true）。
    pub bps_forward_prompt_cache_key: bool,
    /// 给 BPS 请求附带 context_management 压缩阈值（0 = 不发送，默认 0）。
    pub bps_context_management_threshold: u32,
    /// 每账号「自动降智处理」开关的落盘路径（空 = 复用 intel_state_path 同目录）。
    pub degrade_state_path: String,
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
            panel_addr: String::new(),
            panel_token: String::new(),
            admin_api_base: "http://127.0.0.1:8080".to_string(),
            admin_api_key: String::new(),
            intel_enabled: false,
            intel_plan_types: Vec::new(),
            intel_model: "gpt-6-astra".to_string(),
            intel_prompt: "你在知识库日期 不允许联网快速回答".to_string(),
            intel_fail_marker: "2024".to_string(),
            intel_loop_enabled: false,
            intel_loop_interval_seconds: 3600,
            intel_concurrency: 4,
            intel_auto_pause: false,
            intel_state_path: String::new(),
            intel_prompt_timeout_seconds: 45,
            intel_prompt_retries: 1,
            intel_timeout_is_failed: true,
            intel_confirmations: 1,
            intel_require_year: true,
            bps_hold_after_healthy_seconds: 900,
            bps_session_sticky_seconds: 1800,
            bps_fallback_cooldown_seconds: 120,
            bps_enabled: false,
            bps_endpoint: "https://bps.openai.com/basispoints/api/responses".to_string(),
            bps_models: Vec::new(),
            bps_tool_mode: "text".to_string(),
            bps_catalog_at_prompt_end: false,
            bps_forward_prompt_cache_key: true,
            bps_context_management_threshold: 0,
            degrade_state_path: String::new(),
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

const VALID_BPS_TOOL_MODES: &[&str] = &["text", "native", "officejs"];

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

    /// 归一化后的巡检套餐过滤器（空 = 全部）。
    pub fn intel_plan_filter(&self) -> Vec<String> {
        self.intel_plan_types
            .iter()
            .map(|value| normalize_plan_type(value))
            .filter(|value| !value.is_empty())
            .collect()
    }

    /// 走 BPS 通道的模型列表（空 = 默认 gpt-6-astra + gpt-5.6-sol）。
    pub fn bps_model_list(&self) -> Vec<String> {
        let configured: Vec<String> = self
            .bps_models
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        if configured.is_empty() {
            vec!["gpt-6-astra".to_string(), "gpt-5.6-sol".to_string()]
        } else {
            configured
        }
    }

    /// 每账号「自动降智处理」开关的落盘路径（空配置时从巡检结论路径推导）。
    pub fn degrade_state_file(&self) -> String {
        let explicit = self.degrade_state_path.trim();
        if !explicit.is_empty() {
            return explicit.to_string();
        }
        let intel = self.intel_state_path.trim();
        if intel.is_empty() {
            return String::new();
        }
        match std::path::Path::new(intel).parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir
                .join("degrade-handling.json")
                .to_string_lossy()
                .into_owned(),
            _ => String::new(),
        }
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
        if !self.panel_addr.trim().is_empty() && self.panel_token.trim().is_empty() {
            return Err(
                "panel_addr 非空时必须同时配置 panel_token（面板不做无鉴权暴露）".to_string(),
            );
        }
        if !(60..=86_400).contains(&self.intel_loop_interval_seconds) {
            return Err("intel_loop_interval_seconds must be within 60..=86400".to_string());
        }
        if !(1..=16).contains(&self.intel_concurrency) {
            return Err("intel_concurrency must be within 1..=16".to_string());
        }
        if !(5..=600).contains(&self.intel_prompt_timeout_seconds) {
            return Err("intel_prompt_timeout_seconds must be within 5..=600".to_string());
        }
        if self.intel_prompt_retries > 5 {
            return Err("intel_prompt_retries must be within 0..=5".to_string());
        }
        if !(1..=10).contains(&self.intel_confirmations) {
            return Err("intel_confirmations must be within 1..=10".to_string());
        }
        if self.bps_hold_after_healthy_seconds > 86_400 {
            return Err("bps_hold_after_healthy_seconds must be within 0..=86400".to_string());
        }
        if self.bps_session_sticky_seconds > 86_400 {
            return Err("bps_session_sticky_seconds must be within 0..=86400".to_string());
        }
        if self.bps_fallback_cooldown_seconds > 3600 {
            return Err("bps_fallback_cooldown_seconds must be within 0..=3600".to_string());
        }
        if self.bps_enabled {
            let endpoint = self.bps_endpoint.trim();
            if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
                return Err("bps_endpoint must start with http:// or https://".to_string());
            }
            if !VALID_BPS_TOOL_MODES.contains(&self.bps_tool_mode.trim()) {
                return Err(format!(
                    "bps_tool_mode must be one of {}",
                    VALID_BPS_TOOL_MODES.join("/")
                ));
            }
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
    fn intel_and_panel_defaults() {
        let d = PluginConfig::default();
        assert!(!d.intel_enabled);
        assert!(!d.intel_loop_enabled);
        assert_eq!(d.intel_model, "gpt-6-astra");
        assert_eq!(d.admin_api_base, "http://127.0.0.1:8080");
        assert!(d.panel_addr.is_empty());
        assert!(!d.bps_enabled);
        assert_eq!(
            d.bps_model_list(),
            vec!["gpt-6-astra".to_string(), "gpt-5.6-sol".to_string()]
        );
    }

    #[test]
    fn panel_requires_token() {
        assert!(PluginConfig::parse(br#"{"panel_addr":"127.0.0.1:8848"}"#).is_err());
        assert!(
            PluginConfig::parse(br#"{"panel_addr":"127.0.0.1:8848","panel_token":"t"}"#).is_ok()
        );
    }

    #[test]
    fn plan_filter_normalizes_aliases() {
        let parsed = PluginConfig::parse(
            br#"{"intel_plan_types":[" Pro ","chatgpt_pro","self-serve-business-prolite","unknown"]}"#,
        )
        .unwrap();
        assert_eq!(
            parsed.intel_plan_filter(),
            vec![
                "pro".to_string(),
                "pro".to_string(),
                "self_serve_business_prolite".to_string(),
                "unknown".to_string()
            ]
        );
    }

    #[test]
    fn bps_endpoint_validated_only_when_enabled() {
        assert!(PluginConfig::parse(br#"{"bps_endpoint":"bad","bps_enabled":false}"#).is_ok());
        assert!(PluginConfig::parse(br#"{"bps_endpoint":"bad","bps_enabled":true}"#).is_err());
        assert!(PluginConfig::parse(
            br#"{"bps_enabled":true,"bps_endpoint":"https://bps.openai.com/basispoints/api/responses"}"#
        )
        .is_ok());
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
            "panel_addr",
            "panel_token",
            "admin_api_base",
            "admin_api_key",
            "intel_enabled",
            "intel_plan_types",
            "intel_model",
            "intel_prompt",
            "intel_fail_marker",
            "intel_loop_enabled",
            "intel_loop_interval_seconds",
            "intel_concurrency",
            "intel_auto_pause",
            "intel_state_path",
            "intel_prompt_timeout_seconds",
            "intel_prompt_retries",
            "intel_timeout_is_failed",
            "intel_confirmations",
            "intel_require_year",
            "bps_hold_after_healthy_seconds",
            "bps_session_sticky_seconds",
            "bps_fallback_cooldown_seconds",
            "bps_enabled",
            "bps_endpoint",
            "bps_models",
            "bps_tool_mode",
            "bps_catalog_at_prompt_end",
            "bps_forward_prompt_cache_key",
            "bps_context_management_threshold",
            "degrade_state_path",
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
