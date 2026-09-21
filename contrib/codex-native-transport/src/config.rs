//! 插件配置：严格解析（拒绝未知字段），空配置规范化为完整默认值。

use serde::{Deserialize, Serialize};

/// 与前端账号套餐展示口径一致：忽略大小写及空格/下划线/连字符，合并已知别名。
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
    /// 兼容保留（已被 per_account_fingerprint 接管，normalize 时写穿）。
    pub per_account_cookie_jar: bool,
    /// 请求体缓冲上限（MB）。真实 Codex 以 Content-Length 发送完整 JSON 体，
    /// 插件同样先缓冲以保证不退化为 chunked 传输。
    pub max_request_body_mb: u32,
    /// reqwest client 缓存上限（按 账号 × 代理 × 协议 组合缓存，LRU 淘汰）。
    pub max_cached_clients: u32,
    /// turn-state 诊断（默认关）：开启后，对 ChatGPT Codex 内部接口请求，
    /// 把每次上游响应里的 x-codex-turn-state / openai-model 观测落成 JSONL，
    /// 用于实证"上游何时铸 turn-state、跨 session 重放认不认"。只观测、不改写。
    pub turn_state_diag: bool,
    /// turn-state 诊断落盘路径（空 = 用系统临时目录下的默认文件）。
    /// 生产建议指向挂载卷，如 /app/data/codex-native-transport/turnstate-diag.jsonl。
    pub turn_state_diag_path: String,
    /// turn-state 处理模式：
    /// - passthrough（默认）：不干预 turn-state（完全信任宿主链路）；
    /// - strip：剥离 turn-state（不做会话粘连）；
    /// - pin：按 (account × model) 养一个 turn-state 池并在出站注入，命中同后端以抗降智。
    pub turn_state_mode: String,
    /// pin 身份策略：
    /// - pinned（默认）：钉住 turn-state，同时保持 session/thread/prompt_cache_key；
    /// - rotate（实验）：钉住 turn-state + 每请求换新 session，会破坏缓存亲和。
    pub pin_identity_strategy: String,
    /// pin 池里单个 turn-state 的**兜底最大存活（秒）**：过期即丢弃重铸。
    /// 注意：主判据已改为“换发信号”（上游在响应里回带新 turn-state = 票费了，见
    /// TurnStatePool::note_injected_reissue），这里只是防止信号迟迟不来时死握脏票的天花板，
    /// 故可设得比旧的 45min 宽（默认 3h）。
    pub pin_max_age_seconds: u32,
    /// 有效锁票到期前提前探铸的窗口（秒）。0 = 关闭；默认 300（5 分钟）。
    /// 预刷新成功前保留旧票，只有拿到新的 292/332 可锁定票才替换。
    pub pin_refresh_before_expiry_seconds: u32,
    /// pin 判失：连续 overload 次数达到该阈值即把该 (account×model) 格标记失效。
    pub pin_fail_threshold: u32,
    /// pin 判失：近窗口内 overload 占比（百分比）达到该阈值也判失效。
    pub pin_fail_ratio_pct: u32,
    /// pin 池持久化路径（空 = 不持久化，仅内存）。只落 turn-state + 统计，绝不落 bearer。
    /// 生产建议指向挂载卷，如 /app/data/codex-native-transport/turnstate-pool.json。
    pub pin_persist_path: String,
    /// 内嵌管理面板监听地址（空 = 关闭）。生产务必绑内网/回环，如 127.0.0.1:8848。
    pub panel_addr: String,
    /// 面板访问 token（空 = 面板关闭，绝不无鉴权暴露）。Bearer 或 ?token= 均可。
    pub panel_token: String,
    /// canary 主动探测降智/重铸 turn-state（默认关）。开启后按 interval 对已知 (account×model) 探测。
    pub canary_enabled: bool,
    /// canary 探测间隔（秒）。
    pub canary_interval_seconds: u32,
    /// 被动养池（默认开）：靠真实用户流量养池——上游响应里出现 292 真6票就锁进池子。
    /// 关掉 = 只有主动养池/手动来源能锁票（生产一般保持开启）。仅在 pin 模式下有意义。
    pub passive_warming_enabled: bool,
    /// 主动养池（默认关）：后台用最小 hi 请求(复用真实模板的 bearer，仅内存)对
    /// "还没锁到 292/332"的 (account×model) 不断探铸，走出口池轮换换 IP，锁到有效票即停，
    /// TTL 过期再养。需要该 (account×model) 至少有过一次真实流量以取得可用模板。
    pub active_warming_enabled: bool,
    /// 主动养池两次探铸之间的间隔（秒，≥1）。越小越快锁票但越费 token/额度，也越像机器人流量。
    pub active_warming_interval_seconds: u32,
    /// 全池主动养池（默认关）：插件内置 admin key，直接调宿主 admin API 枚举**全部**
    /// 可调度 openai oauth 号 + 取 access_token，对每个 (号 × warming_models) 没锁 292 的
    /// 格用最小 hi 造票（借用任一真实模板做请求形状，出口走 egress_pool）。覆盖空闲号——
    /// 不再要求该号先有过真实流量。需要 turn_state_mode=pin 且 egress_pool 非空。
    pub admin_warming_enabled: bool,
    /// 宿主 admin API 基址（插件与后端同容器，通常 http://127.0.0.1:8080）。
    pub admin_api_base: String,
    /// 宿主 admin API key（请求头 x-api-key）。**仅内存**，随插件配置加密存储于 DB。
    pub admin_api_key: String,
    /// 全池主动养池一轮结束后的间隔（秒）。
    pub admin_warming_interval_seconds: u32,
    /// pin 逻辑关注的上游模型列表（多行文本，一行一个）。**只有这些模型**参与 pin：
    /// 铸票出口池、身份轮换、票注入、被动捕获、格级/账号级休息、主动/全池养池、面板展示。
    /// 其它模型的请求在 pin 模式下一律按 passthrough 处理（走账号自己的出口、保留客户端身份、
    /// 不建养池格、不消耗额度）。只填会铸 292 真6票的模型，默认 gpt-6-astra + gpt-5.6-sol。
    pub warming_models: String,
    /// pin 逻辑关注的 OpenAI OAuth 套餐类型。空数组 = 全部套餐；非空时只处理匹配
    /// credentials.plan_type 的账号。匹配前会做小写、分隔符归一化，并把 chatgpt_pro
    /// 视为 pro。"unknown" 可显式选择缺失或程序暂不认识的套餐。
    pub warming_plan_types: Vec<String>,
    /// 每账号休息编排——两次休息之间的最短活跃时间（秒）。休息**不再按时间触发**：
    /// 只有当该号的养池格没有有效 292 票、且出口池已经整整轮转一圈仍铸不出 292 时才进入
    /// 休息。本值只是防抖：一个号恢复后至少活跃这么久才允许再次休息，避免出口池很小时
    /// 反复快速进出休息。0 = 不设最短活跃时间。默认 1800（30 分钟）。
    pub warming_duty_seconds: u32,
    /// 休息时长（秒），两层共用。格级：某 (account×model) 出口池转满一圈仍无 292 → 这一格
    /// 休息这么久（不探铸、真实流量直通不注入），不碰宿主；同一账号别的模型有 292 时只歇这一格。
    /// 账号级（叠加）：该号所有养池模型都没 292 时，再把调度优先级临时调成
    /// `warming_drain_priority`（宿主只在**分配新会话**时看优先级；已粘住的老会话继续命中该号，
    /// 闲置满宿主粘性 TTL 后自然脱落）。到点恢复原优先级并重新开始轮转出口池铸票。
    /// 0 = 不休息。默认 600（10 分钟）。宿主不再被 schedulable=false 摘号，老会话不会被打散、
    /// 提示缓存不丢。需配 admin API + 开启任一养池 + egress_pool 非空。
    pub warming_rest_seconds: u32,
    /// 休息期间临时写入宿主账号的调度优先级（数值越大越靠后；宿主新会话按优先级升序选号）。
    /// 只要还有任何一个其它可调度号，新会话就不会落到休息中的号上。默认 9999。
    pub warming_drain_priority: i64,
    /// 放弃铸票：某 (account×model) 格连续经历这么多轮「出口池转满一圈仍无 292 → 休息」后，
    /// 判定该格当前铸不出真6，进入放弃态：真实流量退回直通（走账号自己的出口、保留客户端
    /// 会话身份、不注入、不换身份），养池不再对它探铸。0 = 永不放弃。默认 3。
    pub pin_giveup_rounds: u32,
    /// 放弃态持续时长（秒），到点后允许再试**一圈**出口池；仍无 292 立即重新放弃。默认 21600（6h）。
    pub pin_giveup_retry_seconds: u32,
    /// 出口池换下一个代理的门槛：同一格连续多少次非 292（假票/短票/报错）就把游标 +1。
    /// 越小换 IP 越快、一圈转完越快（一圈 = pool.len() × 本值 次失败），休息/放弃也来得越早。
    /// 默认 3；1 = 一次 312 就换。
    pub egress_advance_threshold: u32,
    /// 出口代理池（多行文本，一行一个标准代理 URL）。非空时接管铸票出口：
    /// pin 模式下对"还没锁到 292 真票"的 (account×model) 出站请求，从池里取一个代理发；
    /// 每格独立游标，连续 egress_advance_threshold 次非-292/报错即换池里下一个;出 292 即锁票。
    /// 支持 socks5h:// / socks5:// / http:// / https://，可带账密。空行与 # 注释行忽略。
    pub egress_pool: String,
    /// 铸票出口模式（四选一）：
    /// - account_proxy：账号原代理，不启用专用出口；
    /// - static_pool：静态 egress_pool；
    /// - proxy_api：第三方动态代理 API；
    /// - quya_random：云桥随机 IPv6 网关。
    /// None 只用于兼容旧配置，normalize 后会迁移成明确值。
    pub egress_mode: Option<String>,
    /// 动态代理 API。开启后，未锁票的每次铸票请求都先调用 API 获取一个新的 SOCKS5
    /// 代理，静态 egress_pool 完全不参与。API 返回格式固定为 host:port:user:password。
    pub egress_proxy_api_enabled: bool,
    /// 动态代理 API 完整 URL。URL 中可能含供应商凭据，随插件配置加密存储。
    pub egress_proxy_api_url: String,
    /// SOCKS/TLS 建连失败或 API 获取失败时，是否重新调用 API 获取新代理后重试。
    pub egress_proxy_api_retry_enabled: bool,
    /// 首次尝试之外的最大重试次数。只重试确认尚未把请求发到上游的连接阶段错误。
    pub egress_proxy_api_max_retries: u32,
    /// API 获取失败、格式错误或连接重试耗尽后，是否回退账号原代理。
    pub egress_proxy_api_fallback_to_account_proxy: bool,
    /// 云桥随机 IPv6 网关列表，一行一个 host:port（也接受 http://host:port）。
    /// 每次需要铸票时轮转选择网关并生成全新随机用户名。
    pub egress_quya_random_servers: String,
    /// 云桥随机 IPv6 网关的共享分配密码。属于敏感配置，由宿主加密保存。
    pub egress_quya_random_password: String,
    /// 锁到有效 turn-state 后是否回到账号原本配置的代理。
    /// true（默认）= 锁票后业务流量走账号原代理；
    /// false = 继续走铸出该锁票的代理池出口。
    pub use_account_proxy_after_lock: bool,
    /// Telegram 通知 bot token（BotFather 下发；空 = 不通知）。仅内存，随插件配置加密存 DB。
    /// 填了 token + chat_id 即开启：每当账号(×模型)的智商发生切换（掉智/回真6）就直连
    /// api.telegram.org 推一条通知。best-effort，绝不影响转发与养池。
    pub tg_bot_token: String,
    /// Telegram 通知目标：数字 chat id 或 `@频道名`。空 = 不通知。
    pub tg_chat_id: String,
    /// 通知哪些事件：degrade_only（仅掉智）/ degrade_recover（掉智+回真6，默认）/
    /// all（含过载失效/恢复）。
    pub tg_notify_events: String,
    /// 通知投递模式：immediate（即时，每格带冷却，默认）/ digest（批量摘要，
    /// 每 tg_notify_cooldown_seconds 汇总一条）。
    pub tg_notify_mode: String,
    /// immediate 模式：同一(账号×模型)两次通知的最小间隔（秒，防抖）。
    /// digest 模式：两次摘要之间的汇总窗口（秒）。0 = 不限流。默认 300。
    pub tg_notify_cooldown_seconds: u32,
    /// 出站身份 Profile。
    pub identity: IdentityConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IdentityConfig {
    /// passthrough | machine。machine 对已有会话身份做按账号稳定的 1:1 假名化。
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
    /// machine 模式下根据最终 User-Agent 校正 turn metadata sandbox。
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
            version_auto_sync: true,
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
            per_account_fingerprint: Some(false),
            per_account_cookie_jar: true,
            max_request_body_mb: 128,
            max_cached_clients: 256,
            turn_state_diag: false,
            turn_state_diag_path: String::new(),
            turn_state_mode: "passthrough".to_string(),
            pin_identity_strategy: "pinned".to_string(),
            pin_max_age_seconds: 10800,
            pin_refresh_before_expiry_seconds: 300,
            pin_fail_threshold: 3,
            pin_fail_ratio_pct: 50,
            pin_persist_path: String::new(),
            panel_addr: String::new(),
            panel_token: String::new(),
            canary_enabled: false,
            canary_interval_seconds: 600,
            passive_warming_enabled: true,
            active_warming_enabled: false,
            active_warming_interval_seconds: 15,
            admin_warming_enabled: false,
            admin_api_base: "http://127.0.0.1:8080".to_string(),
            admin_api_key: String::new(),
            admin_warming_interval_seconds: 60,
            warming_models: "gpt-6-astra\ngpt-5.6-sol".to_string(),
            warming_plan_types: Vec::new(),
            egress_advance_threshold: 3,
            warming_duty_seconds: 1800,
            warming_rest_seconds: 600,
            warming_drain_priority: 9999,
            pin_giveup_rounds: 3,
            pin_giveup_retry_seconds: 21600,
            egress_pool: String::new(),
            egress_mode: None,
            egress_proxy_api_enabled: false,
            egress_proxy_api_url: String::new(),
            egress_proxy_api_retry_enabled: true,
            egress_proxy_api_max_retries: 3,
            egress_proxy_api_fallback_to_account_proxy: true,
            egress_quya_random_servers: "142.54.187.42:49000\n107.150.62.202:49000".to_string(),
            egress_quya_random_password: String::new(),
            use_account_proxy_after_lock: true,
            tg_bot_token: String::new(),
            tg_chat_id: String::new(),
            tg_notify_events: "degrade_recover".to_string(),
            tg_notify_mode: "immediate".to_string(),
            tg_notify_cooldown_seconds: 300,
            identity: IdentityConfig::default(),
        }
    }
}

/// 历史已移除的配置键：解析时静默丢弃，保证从旧版本升级时存量配置仍能加载。
const LEGACY_KEYS: &[&str] = &[
    "one_id_per_request",
    // 0.7.0 的 IPv6 轮换代理接口（已于 0.7.1 移除：单 /64 段轮换对上游等于没换 IP）。
    "egress_api_base",
    "egress_api_key",
    "egress_api_mode",
    "egress_api_ttl_seconds",
    "egress_api_lap_size",
    // 0.7.1 的两个静态池参数（已于 0.7.2 移除：改为每行独立连接，填几行就是几个出口）。
    "egress_pool_fresh_client",
    "egress_pool_lap_size",
    "warp_enabled",
    "warp_proxy_url",
    "warp_rotate_url",
    "warp_auto_rotate",
];

const VALID_TURN_STATE_MODES: &[&str] = &["passthrough", "strip", "pin"];
const VALID_PIN_STRATEGIES: &[&str] = &["rotate", "pinned"];
const VALID_EGRESS_MODES: &[&str] = &["account_proxy", "static_pool", "proxy_api", "quya_random"];
const VALID_FINGERPRINT_MODES: &[&str] = &["passthrough", "machine"];
const VALID_TG_EVENTS: &[&str] = &["degrade_only", "degrade_recover", "all"];
const VALID_TG_MODES: &[&str] = &["immediate", "digest"];

const VALID_PROFILES: &[&str] = &[
    "passthrough",
    "codex_cli",
    "codex_desktop",
    "opencode",
    "pi",
    "custom",
];

impl PluginConfig {
    pub fn parse(raw: &[u8]) -> Result<Self, String> {
        let trimmed_empty = raw.iter().all(|b| b.is_ascii_whitespace());
        let mut config: Self = if raw.is_empty() || trimmed_empty {
            Self::default()
        } else {
            // 先剥掉历史已废弃的键（见 LEGACY_KEYS），再严格解析。结构体启用
            // deny_unknown_fields，若不预先剥掉，从旧版本升级时存量配置会解析失败。
            let mut value: serde_json::Value =
                serde_json::from_slice(raw).map_err(|err| format!("invalid config JSON: {err}"))?;
            if let Some(obj) = value.as_object_mut() {
                for k in LEGACY_KEYS {
                    obj.remove(*k);
                }
            }
            serde_json::from_value(value).map_err(|err| format!("invalid config JSON: {err}"))?
        };
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// 补齐系统管理字段。
    fn normalize(&mut self) {
        // 0.7.8 及更早版本只有动态 API 布尔开关；首次读取旧配置时迁移为互斥模式。
        let egress_mode = self
            .egress_mode
            .as_deref()
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| {
                if self.egress_proxy_api_enabled {
                    "proxy_api".to_string()
                } else if self.egress_pool_list().is_empty() {
                    "account_proxy".to_string()
                } else {
                    "static_pool".to_string()
                }
            });
        self.egress_proxy_api_enabled = egress_mode == "proxy_api";
        self.egress_mode = Some(egress_mode);

        let mut plans = Vec::new();
        for plan in &self.warming_plan_types {
            let plan = normalize_plan_type(plan);
            if !plan.is_empty() && !plans.contains(&plan) {
                plans.push(plan);
            }
        }
        self.warming_plan_types = plans;

        // 指纹总开关：默认关闭（统一指纹）。在设置里打开后变为"一账号一指纹"。
        let master = self.per_account_fingerprint.unwrap_or(false);
        self.per_account_fingerprint = Some(master);
        // 写穿到兼容字段，内部逻辑只读总开关。
        self.per_account_cookie_jar = master;
        self.identity.per_account_installation_id = master;
        // 两种模式都需要稳定种子（统一模式派生全局唯一设备 id）。
        if self.identity.installation_id_seed.trim().is_empty() {
            self.identity.installation_id_seed = generate_seed();
        }
    }

    /// 指纹总开关（normalize 后恒为 Some，默认 false = 统一指纹）。
    pub fn per_account(&self) -> bool {
        self.per_account_fingerprint.unwrap_or(false)
    }

    /// 解析出口代理池：按行拆分，去空白、跳过空行与 # 注释行。保持配置里的先后顺序。
    pub fn egress_pool_list(&self) -> Vec<String> {
        self.egress_pool
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.to_string())
            .collect()
    }

    /// 当前明确选择的出口模式。
    pub fn egress_mode_value(&self) -> &str {
        self.egress_mode.as_deref().unwrap_or("account_proxy")
    }

    pub fn uses_proxy_api(&self) -> bool {
        self.egress_mode_value() == "proxy_api"
    }

    pub fn uses_quya_random(&self) -> bool {
        self.egress_mode_value() == "quya_random"
    }

    /// 实际参与轮转的静态池。只有 static_pool 模式会返回条目，其它模式完整忽略。
    pub fn effective_egress_pool_list(&self) -> Vec<String> {
        if self.egress_mode_value() == "static_pool" {
            self.egress_pool_list()
        } else {
            Vec::new()
        }
    }

    /// 是否配置了铸票出口池。决定 pin 模式下未锁票的格走不走专用出口，
    /// 以及休息编排「转满一圈」信号是否存在。
    pub fn egress_enabled(&self) -> bool {
        match self.egress_mode_value() {
            "static_pool" => !self.egress_pool_list().is_empty(),
            "proxy_api" | "quya_random" => true,
            _ => false,
        }
    }

    /// 出口轮转器的「一圈长度」= 池条目数（每行一个独立出口 / 独立连接）。
    pub fn egress_lap_len(&self) -> usize {
        self.effective_egress_pool_list().len()
    }

    /// 动态代理 API 一次业务请求最多尝试几组代理。
    pub fn egress_proxy_api_max_attempts(&self) -> usize {
        if self.egress_proxy_api_retry_enabled {
            self.egress_proxy_api_max_retries.saturating_add(1) as usize
        } else {
            1
        }
    }

    /// 换出口门槛 = egress_advance_threshold。
    pub fn egress_threshold_effective(&self) -> u32 {
        self.egress_advance_threshold
    }

    /// 某上游模型是否在 pin 关注列表里（不在 = 该请求按 passthrough 处理）。
    pub fn is_warming_model(&self, model: &str) -> bool {
        let model = model.trim();
        !model.is_empty()
            && self
                .warming_models
                .split(['\n', ','])
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.starts_with('#'))
                .any(|s| s == model)
    }

    /// 解析 pin 关注模型列表：按行/逗号拆分，去空白、跳过空行与 # 注释行、去重保序。
    pub fn warming_models_list(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for token in self
            .warming_models
            .split(['\n', ','])
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
        {
            let s = token.to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    /// 套餐范围匹配。空范围表示全部；配置了范围但账号套餐尚未同步时返回 false，
    /// 使请求 fail-open 为 passthrough，而不是误把未知账号纳入 pin。
    pub fn is_warming_plan_type(&self, plan_type: Option<&str>) -> bool {
        if self.warming_plan_types.is_empty() {
            return true;
        }
        let normalized = plan_type
            .map(normalize_plan_type)
            .filter(|plan| !plan.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        self.warming_plan_types.contains(&normalized)
    }

    pub fn quya_random_servers_list(&self) -> Vec<String> {
        self.egress_quya_random_servers
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect()
    }

    /// Telegram 通知是否启用：填了 token + chat_id 即开启（“填了就通知”）。
    pub fn tg_notify_enabled(&self) -> bool {
        !self.tg_bot_token.trim().is_empty() && !self.tg_chat_id.trim().is_empty()
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
        if !VALID_TURN_STATE_MODES.contains(&self.turn_state_mode.as_str()) {
            return Err(format!(
                "turn_state_mode must be one of {}",
                VALID_TURN_STATE_MODES.join("/")
            ));
        }
        if !VALID_PIN_STRATEGIES.contains(&self.pin_identity_strategy.as_str()) {
            return Err(format!(
                "pin_identity_strategy must be one of {}",
                VALID_PIN_STRATEGIES.join("/")
            ));
        }
        if !VALID_EGRESS_MODES.contains(&self.egress_mode_value()) {
            return Err(format!(
                "egress_mode must be one of {}",
                VALID_EGRESS_MODES.join("/")
            ));
        }
        // 上限放宽到 24h：主判据是换发信号，这里只是兜底天花板，允许设得更宽。
        if !(30..=86400).contains(&self.pin_max_age_seconds) {
            return Err("pin_max_age_seconds must be within 30..=86400".to_string());
        }
        if self.pin_refresh_before_expiry_seconds > 86400 {
            return Err("pin_refresh_before_expiry_seconds must be within 0..=86400".to_string());
        }
        if !(1..=20).contains(&self.pin_fail_threshold) {
            return Err("pin_fail_threshold must be within 1..=20".to_string());
        }
        if !(1..=100).contains(&self.pin_fail_ratio_pct) {
            return Err("pin_fail_ratio_pct must be within 1..=100".to_string());
        }
        // 养池休息占空比：0 = 不休息；非 0 需在合理区间。
        if self.warming_duty_seconds != 0 && !(60..=86400).contains(&self.warming_duty_seconds) {
            return Err("warming_duty_seconds must be 0 or within 60..=86400".to_string());
        }
        if self.warming_rest_seconds != 0 && !(30..=86400).contains(&self.warming_rest_seconds) {
            return Err("warming_rest_seconds must be 0 or within 30..=86400".to_string());
        }
        if !(0..=1_000_000).contains(&self.warming_drain_priority) {
            return Err("warming_drain_priority must be within 0..=1000000".to_string());
        }
        if self.pin_giveup_rounds > 100 {
            return Err("pin_giveup_rounds must be within 0..=100".to_string());
        }
        if !(60..=604_800).contains(&self.pin_giveup_retry_seconds) {
            return Err("pin_giveup_retry_seconds must be within 60..=604800".to_string());
        }
        if !(1..=10).contains(&self.egress_advance_threshold) {
            return Err("egress_advance_threshold must be within 1..=10".to_string());
        }
        if self.egress_proxy_api_max_retries > 10 {
            return Err("egress_proxy_api_max_retries must be within 0..=10".to_string());
        }
        if matches!(
            self.egress_mode_value(),
            "static_pool" | "proxy_api" | "quya_random"
        ) && self.turn_state_mode != "pin"
        {
            return Err("专用铸票出口需要 turn_state_mode=pin".to_string());
        }
        if self.uses_proxy_api() {
            if self.turn_state_mode != "pin" {
                return Err(
                    "egress_proxy_api_enabled 需要 turn_state_mode=pin（动态代理只接管铸票出口）"
                        .to_string(),
                );
            }
            let raw = self.egress_proxy_api_url.trim();
            if raw.is_empty() {
                return Err("egress_proxy_api_enabled 需要填写代理 API URL".to_string());
            }
            let url = reqwest::Url::parse(raw)
                .map_err(|_| "egress_proxy_api_url 必须是有效 URL".to_string())?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err("egress_proxy_api_url 必须是有效的 http:// 或 https:// URL".to_string());
            }
            if !self.use_account_proxy_after_lock {
                return Err(
                    "动态代理 API 模式需要开启“锁定后使用账号原代理”（临时代理会话不持久化）"
                        .to_string(),
                );
            }
        }
        if self.uses_quya_random() {
            let servers = self.quya_random_servers_list();
            if servers.is_empty() {
                return Err("云桥随机 IPv6 模式需要至少填写一台代理服务器".to_string());
            }
            for server in &servers {
                crate::proxy_api::parse_quya_gateway(server)
                    .map_err(|_| format!("云桥代理服务器格式无效：{server}"))?;
            }
            if self.egress_quya_random_password.trim().is_empty() {
                return Err("云桥随机 IPv6 模式需要填写共享分配密码".to_string());
            }
            if !self.use_account_proxy_after_lock {
                return Err(
                    "云桥随机 IPv6 模式需要开启“锁定后使用账号原代理”（随机租约只用于铸票）"
                        .to_string(),
                );
            }
        }
        // Telegram 通知：枚举合法 + 冷却上限。token/chat 自由文本，留空即关闭。
        if !VALID_TG_EVENTS.contains(&self.tg_notify_events.as_str()) {
            return Err(format!(
                "tg_notify_events must be one of {}",
                VALID_TG_EVENTS.join("/")
            ));
        }
        if !VALID_TG_MODES.contains(&self.tg_notify_mode.as_str()) {
            return Err(format!(
                "tg_notify_mode must be one of {}",
                VALID_TG_MODES.join("/")
            ));
        }
        if self.tg_notify_cooldown_seconds > 86400 {
            return Err("tg_notify_cooldown_seconds must be within 0..=86400".to_string());
        }
        if !self.panel_addr.trim().is_empty() && self.panel_token.trim().is_empty() {
            return Err("panel_token 必须非空：面板绝不允许无鉴权暴露".to_string());
        }
        if self.canary_enabled && !(30..=86400).contains(&self.canary_interval_seconds) {
            return Err("canary_interval_seconds must be within 30..=86400".to_string());
        }
        if !self.warming_plan_types.is_empty()
            && (self.admin_api_base.trim().is_empty() || self.admin_api_key.trim().is_empty())
        {
            return Err(
                "限定账号套餐范围需要配置 admin_api_base 和 admin_api_key（用于读取 credentials.plan_type）"
                    .to_string(),
            );
        }
        if self.active_warming_enabled {
            if self.turn_state_mode != "pin" {
                return Err(
                    "active_warming_enabled 需要 turn_state_mode=pin（主动养池就是养 pin 池）"
                        .to_string(),
                );
            }
            if !(1..=3600).contains(&self.active_warming_interval_seconds) {
                return Err("active_warming_interval_seconds must be within 1..=3600".to_string());
            }
        }
        if self.admin_warming_enabled {
            if self.turn_state_mode != "pin" {
                return Err(
                    "admin_warming_enabled 需要 turn_state_mode=pin（全池养池就是养 pin 池）"
                        .to_string(),
                );
            }
            if self.admin_api_base.trim().is_empty() {
                return Err(
                    "admin_warming_enabled 需要 admin_api_base 非空（宿主 admin API 基址）"
                        .to_string(),
                );
            }
            let base = self.admin_api_base.trim();
            if !(base.starts_with("http://") || base.starts_with("https://")) {
                return Err("admin_api_base 必须以 http:// 或 https:// 开头".to_string());
            }
            if self.admin_api_key.trim().is_empty() {
                return Err(
                    "admin_warming_enabled 需要 admin_api_key 非空（宿主 admin API key）"
                        .to_string(),
                );
            }
            if !(5..=3600).contains(&self.admin_warming_interval_seconds) {
                return Err("admin_warming_interval_seconds must be within 5..=3600".to_string());
            }
            if self.warming_models_list().is_empty() {
                return Err(
                    "admin_warming_enabled 需要 warming_models 至少配置一个模型".to_string()
                );
            }
            if !self.egress_enabled() {
                return Err(
                    "admin_warming_enabled 需要选择静态池、动态代理 API 或云桥随机 IPv6 出口"
                        .to_string(),
                );
            }
        }
        if self.turn_state_mode == "pin" && self.warming_models_list().is_empty() {
            return Err(
                "turn_state_mode=pin 需要 warming_models 至少一个模型（只有列表里的模型参与 pin，其余直通）"
                    .to_string(),
            );
        }
        if self.egress_mode_value() == "static_pool" {
            let pool = self.egress_pool_list();
            if pool.is_empty() {
                return Err("egress_mode=static_pool 需要至少一个静态代理地址".to_string());
            }
            for entry in &pool {
                if !(entry.starts_with("socks5://")
                    || entry.starts_with("socks5h://")
                    || entry.starts_with("http://")
                    || entry.starts_with("https://"))
                {
                    return Err(format!(
                        "egress_pool 条目必须以 socks5:// / socks5h:// / http:// / https:// 开头：{entry}"
                    ));
                }
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
        if self.identity.version_sync_interval_hours > 168 {
            return Err("identity.version_sync_interval_hours must be within 0..=168".to_string());
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
    // 用时间戳 + 进程 id + 计数器拼一个足够随机的本地种子（非安全用途）。
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "{:x}-{:x}-{:x}",
        now,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_normalizes_to_defaults() {
        for raw in [b"".as_slice(), b"{}".as_slice()] {
            let parsed = PluginConfig::parse(raw).unwrap();
            // 默认：统一指纹（总开关关闭），写穿到兼容字段，种子自动生成。
            assert_eq!(parsed.per_account_fingerprint, Some(false));
            assert!(!parsed.per_account_cookie_jar);
            assert!(!parsed.identity.per_account_installation_id);
            assert!(!parsed.identity.installation_id_seed.is_empty());
            assert!(!parsed.force_http11);
            assert_eq!(parsed.max_request_body_mb, 128);
            assert_eq!(parsed.pin_refresh_before_expiry_seconds, 300);
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
    fn legacy_config_without_master_switch_defaults_to_unified() {
        // 旧版本保存的配置（无总开关字段）：按默认统一指纹处理，并写穿覆盖旧字段。
        let legacy = PluginConfig::parse(
            br#"{"per_account_cookie_jar":true,"identity":{"per_account_installation_id":true}}"#,
        )
        .unwrap();
        assert!(!legacy.per_account());
        assert!(!legacy.per_account_cookie_jar);
        assert!(!legacy.identity.per_account_installation_id);
    }

    #[test]
    fn turn_state_mode_defaults_and_validates() {
        assert_eq!(
            PluginConfig::parse(b"{}").unwrap().turn_state_mode,
            "passthrough"
        );
        assert_eq!(
            PluginConfig::parse(b"{}").unwrap().pin_identity_strategy,
            "pinned"
        );
        assert!(PluginConfig::parse(br#"{"turn_state_mode":"pin"}"#).is_ok());
        assert!(PluginConfig::parse(br#"{"turn_state_mode":"nope"}"#).is_err());
        assert!(PluginConfig::parse(br#"{"pin_identity_strategy":"nope"}"#).is_err());
    }

    #[test]
    fn legacy_keys_are_stripped_on_parse() {
        // 从旧版本升级：存量配置里残留的 warp_* / one_id_per_request 应被静默丢弃，
        // 而不是因 deny_unknown_fields 解析失败。
        let cfg = PluginConfig::parse(
            br#"{"turn_state_mode":"pin","one_id_per_request":true,"warp_enabled":true,"warp_proxy_url":"socks5h://x:1080","warp_rotate_url":"http://y/rotate","warp_auto_rotate":true}"#,
        )
        .unwrap();
        assert_eq!(cfg.turn_state_mode, "pin");
        // 真正的未知键仍应被拒绝。
        assert!(PluginConfig::parse(br#"{"totally_unknown_key":1}"#).is_err());
    }

    #[test]
    fn warming_flags_default_and_validate() {
        // 默认：被动开、主动关、间隔 15s。
        let cfg = PluginConfig::parse(b"{}").unwrap();
        assert!(cfg.passive_warming_enabled);
        assert!(!cfg.active_warming_enabled);
        assert_eq!(cfg.active_warming_interval_seconds, 15);
        // 主动养池需要 pin 模式。
        assert!(PluginConfig::parse(br#"{"active_warming_enabled":true}"#).is_err());
        // pin 模式下合法。
        let ok = PluginConfig::parse(
            br#"{"turn_state_mode":"pin","active_warming_enabled":true,"active_warming_interval_seconds":10}"#,
        )
        .unwrap();
        assert!(ok.active_warming_enabled);
        assert_eq!(ok.active_warming_interval_seconds, 10);
        // 间隔越界拒绝。
        assert!(PluginConfig::parse(
            br#"{"turn_state_mode":"pin","active_warming_enabled":true,"active_warming_interval_seconds":0}"#
        )
        .is_err());
        // 主动关着时不校验间隔。
        assert!(PluginConfig::parse(br#"{"active_warming_interval_seconds":2}"#).is_ok());
        assert!(PluginConfig::parse(br#"{"turn_state_mode":"pin","active_warming_enabled":true,"active_warming_interval_seconds":1}"#).is_ok());
        let d = PluginConfig::parse(b"{}").unwrap();
        assert_eq!(d.egress_advance_threshold, 3);
        assert!(PluginConfig::parse(br#"{"egress_advance_threshold":0}"#).is_err());
        assert!(PluginConfig::parse(br#"{"egress_advance_threshold":1}"#).is_ok());
        // 被动可单独关。
        let p = PluginConfig::parse(br#"{"passive_warming_enabled":false}"#).unwrap();
        assert!(!p.passive_warming_enabled);
    }

    #[test]
    fn warming_duty_rest_defaults_and_validate() {
        // 默认：活跃 30min、休息 10min；兜底票龄默认 3h。
        let d = PluginConfig::parse(b"{}").unwrap();
        assert_eq!(d.warming_duty_seconds, 1800);
        assert_eq!(d.warming_rest_seconds, 600);
        assert_eq!(d.warming_drain_priority, 9999);
        assert_eq!(d.pin_giveup_rounds, 3);
        assert_eq!(d.pin_giveup_retry_seconds, 21600);
        assert!(PluginConfig::parse(br#"{"warming_drain_priority":-1}"#).is_err());
        assert!(PluginConfig::parse(br#"{"pin_giveup_rounds":0}"#).is_ok());
        assert!(PluginConfig::parse(br#"{"pin_giveup_rounds":101}"#).is_err());
        assert!(PluginConfig::parse(br#"{"pin_giveup_retry_seconds":10}"#).is_err());
        assert_eq!(d.pin_max_age_seconds, 10800);
        assert_eq!(d.pin_refresh_before_expiry_seconds, 300);
        // 0 = 不休息，合法。
        assert!(
            PluginConfig::parse(br#"{"warming_duty_seconds":0,"warming_rest_seconds":0}"#).is_ok()
        );
        // 越界拒绝。
        assert!(PluginConfig::parse(br#"{"warming_duty_seconds":10}"#).is_err());
        assert!(PluginConfig::parse(br#"{"warming_rest_seconds":10}"#).is_err());
        // pin_max_age 放宽到 24h：旧上限 3600 以上现在合法。
        assert!(PluginConfig::parse(br#"{"pin_max_age_seconds":10800}"#).is_ok());
        assert!(PluginConfig::parse(br#"{"pin_max_age_seconds":90000}"#).is_err());
        assert!(PluginConfig::parse(br#"{"pin_refresh_before_expiry_seconds":86401}"#).is_err());
    }

    #[test]
    fn tg_notify_defaults_and_validate() {
        let d = PluginConfig::parse(b"{}").unwrap();
        assert!(d.tg_bot_token.is_empty());
        assert!(d.tg_chat_id.is_empty());
        assert_eq!(d.tg_notify_events, "degrade_recover");
        assert_eq!(d.tg_notify_mode, "immediate");
        assert_eq!(d.tg_notify_cooldown_seconds, 300);
        assert!(!d.tg_notify_enabled());
        // 填了 token+chat 即启用。
        let on = PluginConfig::parse(br#"{"tg_bot_token":"123:abc","tg_chat_id":"@ch"}"#).unwrap();
        assert!(on.tg_notify_enabled());
        // 只填一个不算启用。
        assert!(!PluginConfig::parse(br#"{"tg_bot_token":"123:abc"}"#)
            .unwrap()
            .tg_notify_enabled());
        // 枚举非法拒绝。
        assert!(PluginConfig::parse(br#"{"tg_notify_events":"bogus"}"#).is_err());
        assert!(PluginConfig::parse(br#"{"tg_notify_mode":"bogus"}"#).is_err());
        // 冷却上限。
        assert!(PluginConfig::parse(br#"{"tg_notify_cooldown_seconds":90000}"#).is_err());
        assert!(PluginConfig::parse(br#"{"tg_notify_cooldown_seconds":0}"#).is_ok());
    }

    #[test]
    fn egress_pool_lap_is_line_count_and_legacy_keys_strip() {
        let cfg = PluginConfig::parse(
            br#"{"turn_state_mode":"pin","egress_pool":"socks5h://u:p@gate:31\nsocks5h://u:p@gate:31\nsocks5h://u:p@gate:31","egress_advance_threshold":2}"#,
        )
        .unwrap();
        // 同一 URL 填三行 = 三个出口（每行独立连接）。
        assert_eq!(cfg.egress_pool_list().len(), 3);
        assert_eq!(cfg.egress_lap_len(), 3);
        assert_eq!(cfg.egress_threshold_effective(), 2);
        // 0.7.0 / 0.7.1 的键静默剥离，升级不炸。
        let cfg = PluginConfig::parse(
            br#"{"turn_state_mode":"pin","egress_api_base":"http://x:8080","egress_api_key":"k","egress_api_mode":"auth","egress_api_ttl_seconds":1800,"egress_api_lap_size":8,"egress_pool_fresh_client":true,"egress_pool_lap_size":16}"#,
        )
        .unwrap();
        assert!(!cfg.egress_enabled());
    }

    #[test]
    fn egress_pool_parses_and_validates() {
        // 默认空。
        let defaults = PluginConfig::parse(b"{}").unwrap();
        assert!(defaults.egress_pool_list().is_empty());
        assert!(!defaults.egress_proxy_api_enabled);
        assert!(defaults.egress_proxy_api_retry_enabled);
        assert_eq!(defaults.egress_proxy_api_max_retries, 3);
        assert!(defaults.egress_proxy_api_fallback_to_account_proxy);
        assert!(defaults.use_account_proxy_after_lock);
        assert!(
            !PluginConfig::parse(br#"{"use_account_proxy_after_lock":false}"#)
                .unwrap()
                .use_account_proxy_after_lock
        );
        // 多行解析：去空白、跳过空行与 # 注释，保序。
        let cfg = PluginConfig::parse(
            br#"{"turn_state_mode":"pin","egress_pool":"socks5h://a:1080\n  # note\n\nhttp://u:p@b:8080\n"}"#,
        )
        .unwrap();
        assert_eq!(
            cfg.egress_pool_list(),
            vec![
                "socks5h://a:1080".to_string(),
                "http://u:p@b:8080".to_string()
            ]
        );
        // 非空需 pin 模式。
        assert!(PluginConfig::parse(br#"{"egress_pool":"socks5h://a:1080"}"#).is_err());
        // 非法协议头报错。
        assert!(
            PluginConfig::parse(br#"{"turn_state_mode":"pin","egress_pool":"1.2.3.4:1080"}"#)
                .is_err()
        );
        // 合法混填 v4/v6 + 多协议。
        assert!(PluginConfig::parse(
            br#"{"turn_state_mode":"pin","egress_pool":"socks5h://1.2.3.4:1080\nhttp://[2a01::1]:8080"}"#
        )
        .is_ok());
    }

    #[test]
    fn dynamic_proxy_api_validates_and_overrides_static_pool() {
        let cfg = PluginConfig::parse(
            br#"{"turn_state_mode":"pin","egress_proxy_api_enabled":true,"egress_proxy_api_url":"https://proxy.example/api?token=secret","egress_proxy_api_retry_enabled":true,"egress_proxy_api_max_retries":3,"egress_proxy_api_fallback_to_account_proxy":true,"egress_pool":"socks5h://static:1080","use_account_proxy_after_lock":true}"#,
        )
        .unwrap();
        assert!(cfg.egress_enabled());
        assert!(cfg.effective_egress_pool_list().is_empty());
        assert_eq!(cfg.egress_proxy_api_max_attempts(), 4);

        let no_retry = PluginConfig::parse(
            br#"{"turn_state_mode":"pin","egress_proxy_api_enabled":true,"egress_proxy_api_url":"https://proxy.example/api","egress_proxy_api_retry_enabled":false,"egress_proxy_api_max_retries":9,"use_account_proxy_after_lock":true}"#,
        )
        .unwrap();
        assert_eq!(no_retry.egress_proxy_api_max_attempts(), 1);

        for raw in [
            br#"{"turn_state_mode":"pin","egress_proxy_api_enabled":true,"use_account_proxy_after_lock":true}"#.as_slice(),
            br#"{"turn_state_mode":"pin","egress_proxy_api_enabled":true,"egress_proxy_api_url":"ftp://proxy.example/api","use_account_proxy_after_lock":true}"#.as_slice(),
            br#"{"turn_state_mode":"pin","egress_proxy_api_enabled":true,"egress_proxy_api_url":"https://proxy.example/api","use_account_proxy_after_lock":false}"#.as_slice(),
            br#"{"turn_state_mode":"passthrough","egress_proxy_api_enabled":true,"egress_proxy_api_url":"https://proxy.example/api","use_account_proxy_after_lock":true}"#.as_slice(),
            br#"{"turn_state_mode":"pin","egress_proxy_api_enabled":true,"egress_proxy_api_url":"https://proxy.example/api","egress_proxy_api_max_retries":11,"use_account_proxy_after_lock":true}"#.as_slice(),
        ] {
            assert!(PluginConfig::parse(raw).is_err());
        }
    }

    #[test]
    fn plan_scope_normalizes_aliases_and_unknown_is_explicit() {
        let cfg = PluginConfig::parse(
            br#"{"admin_api_key":"k","warming_plan_types":[" Pro ","chatgpt_pro","self-serve-business-prolite","unknown"]}"#,
        )
        .unwrap();
        assert_eq!(
            cfg.warming_plan_types,
            vec![
                "pro".to_string(),
                "self_serve_business_prolite".to_string(),
                "unknown".to_string()
            ]
        );
        assert!(cfg.is_warming_plan_type(Some("CHATGPT_PRO")));
        assert!(cfg.is_warming_plan_type(Some("self_serve_business_prolite")));
        assert!(cfg.is_warming_plan_type(None));
        assert!(!cfg.is_warming_plan_type(Some("plus")));
        assert!(PluginConfig::parse(br#"{"warming_plan_types":["pro"]}"#).is_err());
    }

    #[test]
    fn quya_random_is_mutually_exclusive_and_requires_account_proxy_after_lock() {
        let cfg = PluginConfig::parse(
            br#"{"turn_state_mode":"pin","egress_mode":"quya_random","egress_quya_random_servers":"142.54.187.42:49000\n107.150.62.202:49000","egress_quya_random_password":"secret","egress_pool":"not-a-proxy","egress_proxy_api_url":"not-a-url","use_account_proxy_after_lock":true}"#,
        )
        .unwrap();
        assert!(cfg.uses_quya_random());
        assert!(!cfg.uses_proxy_api());
        assert!(cfg.effective_egress_pool_list().is_empty());
        assert_eq!(cfg.quya_random_servers_list().len(), 2);

        for raw in [
            br#"{"turn_state_mode":"pin","egress_mode":"quya_random","egress_quya_random_servers":"142.54.187.42:49000","use_account_proxy_after_lock":true}"#.as_slice(),
            br#"{"turn_state_mode":"pin","egress_mode":"quya_random","egress_quya_random_servers":"142.54.187.42:49000","egress_quya_random_password":"secret","use_account_proxy_after_lock":false}"#.as_slice(),
            br#"{"turn_state_mode":"pin","egress_mode":"quya_random","egress_quya_random_servers":"bad","egress_quya_random_password":"secret","use_account_proxy_after_lock":true}"#.as_slice(),
        ] {
            assert!(PluginConfig::parse(raw).is_err());
        }
    }

    #[test]
    fn admin_warming_defaults_and_validate() {
        // 默认：关，基址给了本地后端，key 空，间隔 60，模型 gpt-6-astra。
        let d = PluginConfig::parse(b"{}").unwrap();
        assert!(!d.admin_warming_enabled);
        assert_eq!(d.admin_api_base, "http://127.0.0.1:8080");
        assert!(d.admin_api_key.is_empty());
        assert_eq!(d.admin_warming_interval_seconds, 60);
        assert_eq!(
            d.warming_models_list(),
            vec!["gpt-6-astra".to_string(), "gpt-5.6-sol".to_string()]
        );
        assert!(d.is_warming_model("gpt-6-astra"));
        assert!(d.is_warming_model(" gpt-5.6-sol "));
        assert!(!d.is_warming_model("gpt-5.2"));
        assert!(!d.is_warming_model(""));
        // pin 模式要求关注列表非空；passthrough 不要求。
        assert!(PluginConfig::parse(br#"{"turn_state_mode":"pin","warming_models":" "}"#).is_err());
        assert!(
            PluginConfig::parse(br#"{"turn_state_mode":"passthrough","warming_models":" "}"#)
                .is_ok()
        );

        // warming_models 支持换行/逗号混填 + 去重保序 + 跳注释。
        let m = PluginConfig::parse(
            br#"{"warming_models":"gpt-6-astra, gpt-5.6-luna\n# c\ngpt-6-astra\ngpt-5.6-sol"}"#,
        )
        .unwrap();
        assert_eq!(
            m.warming_models_list(),
            vec![
                "gpt-6-astra".to_string(),
                "gpt-5.6-luna".to_string(),
                "gpt-5.6-sol".to_string()
            ]
        );

        // 开全池养池：需 pin + base + key + 模型 + egress_pool 都齐。
        let base = r#""turn_state_mode":"pin","admin_warming_enabled":true,"admin_api_key":"k","egress_pool":"socks5h://a:1080""#;
        let ok = PluginConfig::parse(format!("{{{base}}}").as_bytes()).unwrap();
        assert!(ok.admin_warming_enabled);

        // 非 pin 模式拒绝。
        assert!(PluginConfig::parse(
            br#"{"admin_warming_enabled":true,"admin_api_key":"k","egress_pool":"socks5h://a:1080"}"#
        )
        .is_err());
        // 缺 key 拒绝。
        assert!(PluginConfig::parse(
            br#"{"turn_state_mode":"pin","admin_warming_enabled":true,"egress_pool":"socks5h://a:1080"}"#
        )
        .is_err());
        // 缺 egress_pool 拒绝（全池造票靠出口池）。
        assert!(PluginConfig::parse(
            br#"{"turn_state_mode":"pin","admin_warming_enabled":true,"admin_api_key":"k"}"#
        )
        .is_err());
        // base 非法 scheme 拒绝。
        assert!(PluginConfig::parse(
            br#"{"turn_state_mode":"pin","admin_warming_enabled":true,"admin_api_key":"k","admin_api_base":"127.0.0.1:8080","egress_pool":"socks5h://a:1080"}"#
        )
        .is_err());
        // 间隔越界拒绝。
        assert!(PluginConfig::parse(
            br#"{"turn_state_mode":"pin","admin_warming_enabled":true,"admin_api_key":"k","egress_pool":"socks5h://a:1080","admin_warming_interval_seconds":2}"#
        )
        .is_err());
        // 空 warming_models 拒绝。
        assert!(PluginConfig::parse(
            br#"{"turn_state_mode":"pin","admin_warming_enabled":true,"admin_api_key":"k","egress_pool":"socks5h://a:1080","warming_models":"   "}"#
        )
        .is_err());
        // 关着时不校验任何依赖。
        assert!(PluginConfig::parse(br#"{"warming_models":"","admin_api_base":"bad"}"#).is_ok());
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
        assert!(PluginConfig::parse(br#"{"identity":{"fingerprint_mode":"random"}}"#).is_err());
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
            "per_account_cookie_jar",
            "max_request_body_mb",
            "max_cached_clients",
            "warming_plan_types",
            "egress_mode",
            "egress_proxy_api_enabled",
            "egress_proxy_api_url",
            "egress_proxy_api_retry_enabled",
            "egress_proxy_api_max_retries",
            "egress_proxy_api_fallback_to_account_proxy",
            "egress_quya_random_servers",
            "egress_quya_random_password",
            "use_account_proxy_after_lock",
            "identity",
        ] {
            assert!(object.contains_key(key), "missing {key}");
        }
        let identity = object["identity"].as_object().unwrap();
        for key in [
            "fingerprint_mode",
            "profile",
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
