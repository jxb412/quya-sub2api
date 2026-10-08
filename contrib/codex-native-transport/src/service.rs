//! Sub2API TransportPlugin gRPC 服务实现。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::config::PluginConfig;
use crate::donor::TemplateCache;
use crate::identity;
use crate::intel::{DegradeStore, IntelStore};
use crate::proto::sub2api::plugin::v1::{
    forward_request, forward_response, transport_plugin_server::TransportPlugin,
    ApplyConfigRequest, ApplyConfigResponse, ForwardRequest, ForwardRequestStart, ForwardResponse,
    ForwardResponseEnd, ForwardResponseError, ForwardResponseStart, GetInfoRequest,
    GetInfoResponse, HeaderValues, HealthRequest, HealthResponse, TestConfigRequest,
    TestConfigResponse, ValidateConfigRequest, ValidateConfigResponse,
};
use crate::transport::{self, ClientCache};
use crate::version_sync::VersionCache;

/// 必须与 manifest.json 的 id 一致（宿主 GetInfo 校验）。
pub const PLUGIN_ID: &str = "io.quya.codex-native-transport";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: u32 = 1;
const TRANSPORT_API_VERSION: u32 = 1;
const CAPABILITY: &str = "openai.oauth.outbound_transport.v1";

const TEST_URL: &str = "https://chatgpt.com/robots.txt";

/// 会话级通道粘滞 + 账号级 BPS 失败冷却。
///
/// 目的是让「换通道」对客户端尽量透明：
/// * 同一个会话一旦走过 BPS，就继续走 BPS 直到它空闲超过
///   `bps_session_sticky_seconds`，避免会话中途换端点把上游 prompt cache 亲和打断；
/// * BPS 出站失败回退过的会话，本会话内不再试 BPS，同时给该号一个冷却期，
///   避免每个请求都先撞一次失败再回退（那会让客户端明显变慢）。
/// * 带 `previous_response_id` 的请求（客户端靠服务端状态续写）不进 BPS，
///   并把整条会话钉在正常通道 `bps_previous_response_pin_seconds` 秒 —— BPS
///   上游 422 拒这个字段，剥掉就等于丢历史，而只跳单条会让同一会话在两条
///   通道之间撕裂。
#[derive(Default)]
pub struct ChannelSticky {
    inner: std::sync::Mutex<StickyInner>,
}

#[derive(Default)]
struct StickyInner {
    /// 会话键 -> (是否走 BPS, 最后使用时间 ms)
    sessions: HashMap<String, (bool, u64)>,
    /// 账号 -> BPS 失败冷却截止时间 ms
    cooldown: HashMap<i64, u64>,
    /// 账号 -> (UTC 日期键, 当日已经打到 BPS 端点的请求数)
    daily: HashMap<i64, (String, u32)>,
    /// 账号 -> (记录时间 ms, 最近一次 BPS 回退 / 跳过原因)，面板展示用
    last_reason: HashMap<i64, (u64, String)>,
    /// 会话键 -> 被 `previous_response_id` 门禁钉在正常通道的截止时间 ms。
    ///
    /// 只跳单条会造成「同一会话前几轮走 BPS、后面几条走正常通道」的撕裂，
    /// 上游会话状态分成两套，客户端照样接不上上下文；所以命中门禁时把整个
    /// 会话钉住，窗口内一律不走 BPS。
    normal_pins: HashMap<String, u64>,
}

const STICKY_MAX_SESSIONS: usize = 8192;

impl ChannelSticky {
    fn lock(&self) -> std::sync::MutexGuard<'_, StickyInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 这个会话上一次是不是走的 BPS（超出粘滞窗口则视为过期）。
    pub fn session_wants_bps(&self, key: &str, now: u64, ttl_ms: u64) -> bool {
        if ttl_ms == 0 {
            return false;
        }
        let mut guard = self.lock();
        guard
            .sessions
            .retain(|_, (_, last)| now.saturating_sub(*last) <= ttl_ms);
        guard
            .sessions
            .get(key)
            .map(|(bps, _)| *bps)
            .unwrap_or(false)
    }

    pub fn mark_session(&self, key: &str, bps: bool, now: u64) {
        let mut guard = self.lock();
        if guard.sessions.len() >= STICKY_MAX_SESSIONS {
            guard.sessions.clear();
        }
        guard.sessions.insert(key.to_string(), (bps, now));
    }

    /// 这个会话是否已经被 `previous_response_id` 门禁钉在正常通道。
    ///
    /// `ttl_ms = 0`（配置关掉钉会话）或已过截止时间都返回 false，并把过期项清掉。
    pub fn session_pinned_normal(&self, key: &str, now: u64, ttl_ms: u64) -> bool {
        if ttl_ms == 0 {
            return false;
        }
        let mut guard = self.lock();
        guard.normal_pins.retain(|_, until| *until > now);
        guard.normal_pins.contains_key(key)
    }

    /// 把整个会话钉在正常通道（截止时间 = now + ttl），并清掉它的 BPS 粘滞标记，
    /// 保证后续请求不会又被 `sticky_bps` 拉回 BPS。`ttl_ms = 0` 时只清粘滞。
    pub fn pin_session_normal(&self, key: &str, now: u64, ttl_ms: u64) {
        let mut guard = self.lock();
        guard.sessions.insert(key.to_string(), (false, now));
        if ttl_ms == 0 {
            return;
        }
        if guard.normal_pins.len() >= STICKY_MAX_SESSIONS {
            guard.normal_pins.clear();
        }
        guard
            .normal_pins
            .insert(key.to_string(), now.saturating_add(ttl_ms));
    }

    pub fn cooldown_active(&self, account_id: i64, now: u64) -> bool {
        self.lock()
            .cooldown
            .get(&account_id)
            .map(|until| now < *until)
            .unwrap_or(false)
    }

    pub fn set_cooldown(&self, account_id: i64, until_ms: u64) {
        self.lock().cooldown.insert(account_id, until_ms);
    }

    /// 冷却剩余毫秒（0 = 当前不在冷却里）。面板「BPS 冷却」列用。
    pub fn cooldown_remaining_ms(&self, account_id: i64, now: u64) -> u64 {
        self.lock()
            .cooldown
            .get(&account_id)
            .map(|until| until.saturating_sub(now))
            .unwrap_or(0)
    }

    /// 该号「今天」（UTC）已经打到 BPS 端点的次数；跨日自动归零。
    pub fn daily_count(&self, account_id: i64, now: u64) -> u32 {
        let day = crate::donor::utc_day_key(now);
        self.lock()
            .daily
            .get(&account_id)
            .filter(|(key, _)| *key == day)
            .map(|(_, count)| *count)
            .unwrap_or(0)
    }

    /// 记一次「今天真的把请求发到了 BPS 端点」，返回当日累计次数。
    ///
    /// 只在出站 URL 真被换成 BPS 端点之后调用：本地改写失败、冷却期、已达
    /// 每日上限而回退正常通道的请求都不计数。
    pub fn bump_daily(&self, account_id: i64, now: u64) -> u32 {
        let day = crate::donor::utc_day_key(now);
        let mut guard = self.lock();
        let entry = guard
            .daily
            .entry(account_id)
            .or_insert_with(|| (day.clone(), 0));
        if entry.0 != day {
            *entry = (day, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1
    }

    /// 记下最近一次 BPS 回退 / 跳过原因（面板展示，不落盘）。
    pub fn set_last_reason(&self, account_id: i64, text: String, now: u64) {
        self.lock().last_reason.insert(account_id, (now, text));
    }

    /// 最近一次 BPS 回退 / 跳过原因：`(发生时间 ms, 文本)`。
    pub fn last_reason(&self, account_id: i64) -> Option<(u64, String)> {
        self.lock().last_reason.get(&account_id).cloned()
    }
}

/// 不同 BPS 错误的冷却时长（秒）与面板文案。
///
/// 分类口径：传输层没连上（含超时）= `status None`；401 / 429 / 400 / 403 各自
/// 独立；其余状态码（含 5xx）统一用 `bps_fallback_cooldown_seconds` 兜底。
/// 返回的秒数为 0 表示该类错误不设冷却（照旧回退原通道，只是不记冷却）。
fn bps_cooldown_for(config: &PluginConfig, status: Option<u16>) -> (u32, &'static str) {
    match status {
        None => (config.bps_cooldown_timeout_seconds, "BPS 超时 / 连接失败"),
        Some(400) => (config.bps_cooldown_400_seconds, "BPS 400 请求被上游拒绝"),
        Some(401) => (config.bps_cooldown_401_seconds, "BPS 401 凭证失效"),
        Some(403) => (config.bps_cooldown_403_seconds, "BPS 403 账号被上游拦截"),
        Some(429) => (config.bps_cooldown_429_seconds, "BPS 429 触发限流"),
        Some(code) => (
            config.bps_fallback_cooldown_seconds,
            if code >= 500 {
                "BPS 5xx 上游异常"
            } else {
                "BPS 其它错误"
            },
        ),
    }
}

/// BPS 出站失败：本会话退回正常通道，并给这个号一个冷却期，
/// 避免每个请求都先撞一次失败再回退（那会让客户端明显变慢）。
///
/// 冷却时长按错误类别区分（配置里 401 / 429 / 400 / 403 / 超时 各自可调，
/// 其它状态码用 `bps_fallback_cooldown_seconds` 兜底，0 = 不设冷却）。
/// `status = None` 表示传输层就没连上（连接失败 / 超时）。
fn note_bps_failure(
    state: &Arc<SharedState>,
    account_id: i64,
    sticky_key: Option<&str>,
    config: &PluginConfig,
    status: Option<u16>,
) {
    let now = crate::donor::now_ms();
    if let Some(key) = sticky_key {
        state.channels.mark_session(key, false, now);
    }
    let (seconds, label) = bps_cooldown_for(config, status);
    let reason = format!("{label}，冷却 {seconds}s");
    state
        .channels
        .set_last_reason(account_id, reason.clone(), now);
    if seconds > 0 {
        state
            .channels
            .set_cooldown(account_id, now + seconds as u64 * 1000);
    }
    // BPS 403 = 上游账号级拦截：按配置把这个号的模型从宿主名单里摘掉（摘除时长
    // 与冷却同步），到点由 model_drop 的后台循环把模型加回去。
    if status == Some(403) {
        crate::model_drop::note_bps_403(state, config, account_id);
    }
}

/// 线路统计：记下这条请求最终落在哪条线路（normal / bps）、上游状态码是多少。
/// 面板「账号智力巡检」页按账号展示成功 / 失败次数与当前线路，因此这里只做
/// 计数，不参与任何路由决策（status = 0 表示传输层就没连上）。
fn record_route(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    account_id: i64,
    route: &str,
    status: u16,
) {
    state
        .degrade
        .record_route(account_id, route, status, &config.degrade_state_file());
}

pub struct SharedState {
    pub config: RwLock<PluginConfig>,
    pub clients: ClientCache,
    pub version: VersionCache,
    /// 最近一条真实 Codex 流量形状（巡检 / BPS 自检借形状用）。
    pub template: TemplateCache,
    /// 账号智力巡检结论仓。
    pub intel: IntelStore,
    /// 每账号「自动降智处理」开关。
    pub degrade: DegradeStore,
    /// 会话级通道粘滞 + BPS 失败冷却。
    pub channels: ChannelSticky,
    /// BPS 403 自动摘除模型（403 → 摘模型 → 到点恢复）。
    pub model_drop: crate::model_drop::ModelDropStore,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(PluginConfig::default()),
            clients: ClientCache::default(),
            version: VersionCache::default(),
            template: TemplateCache::default(),
            intel: IntelStore::default(),
            degrade: DegradeStore::default(),
            channels: ChannelSticky::default(),
            model_drop: crate::model_drop::ModelDropStore::default(),
        })
    }

    pub fn current_config(&self) -> PluginConfig {
        self.config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 当前生效版本：自动同步成功值优先，否则配置的 pinned 版本。
    pub fn effective_version(&self, config: &PluginConfig) -> String {
        if config.identity.version_auto_sync {
            if let Some(synced) = self.version.synced() {
                return synced;
            }
        }
        config.identity.pinned_version.trim().to_string()
    }
}

pub struct TransportService {
    state: Arc<SharedState>,
}

impl TransportService {
    pub fn new(state: Arc<SharedState>) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl TransportPlugin for TransportService {
    async fn get_info(
        &self,
        _request: Request<GetInfoRequest>,
    ) -> Result<Response<GetInfoResponse>, Status> {
        Ok(Response::new(GetInfoResponse {
            plugin_id: PLUGIN_ID.to_string(),
            plugin_version: PLUGIN_VERSION.to_string(),
            protocol_version: PROTOCOL_VERSION,
            transport_api_version: TRANSPORT_API_VERSION,
            capabilities: vec![CAPABILITY.to_string()],
        }))
    }

    async fn health(
        &self,
        _request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        Ok(Response::new(HealthResponse {
            healthy: true,
            message: "ok".to_string(),
        }))
    }

    async fn validate_config(
        &self,
        request: Request<ValidateConfigRequest>,
    ) -> Result<Response<ValidateConfigResponse>, Status> {
        match PluginConfig::parse(&request.into_inner().config_json) {
            Ok(config) => Ok(Response::new(ValidateConfigResponse {
                valid: true,
                message: String::new(),
                normalized_config_json: config.normalized_json(),
            })),
            Err(message) => Ok(Response::new(ValidateConfigResponse {
                valid: false,
                message,
                normalized_config_json: Vec::new(),
            })),
        }
    }

    async fn apply_config(
        &self,
        request: Request<ApplyConfigRequest>,
    ) -> Result<Response<ApplyConfigResponse>, Status> {
        match PluginConfig::parse(&request.into_inner().config_json) {
            Ok(config) => {
                {
                    let mut guard = self
                        .state
                        .config
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if *guard != config {
                        // 配置变化时丢弃旧 client（关闭旧空闲连接）。
                        self.state.clients.clear();
                    }
                    *guard = config;
                }
                Ok(Response::new(ApplyConfigResponse {
                    applied: true,
                    message: String::new(),
                }))
            }
            Err(message) => Ok(Response::new(ApplyConfigResponse {
                applied: false,
                message,
            })),
        }
    }

    async fn test_config(
        &self,
        request: Request<TestConfigRequest>,
    ) -> Result<Response<TestConfigResponse>, Status> {
        let raw = request.into_inner().config_json;
        let config = if raw.is_empty() {
            self.state.current_config()
        } else {
            match PluginConfig::parse(&raw) {
                Ok(config) => config,
                Err(message) => {
                    return Ok(Response::new(TestConfigResponse {
                        success: false,
                        message,
                        latency_ms: 0,
                    }))
                }
            }
        };

        let client = match self.state.clients.client_for(&config, 0, "") {
            Ok(client) => client,
            Err(message) => {
                return Ok(Response::new(TestConfigResponse {
                    success: false,
                    message,
                    latency_ms: 0,
                }))
            }
        };
        let began = Instant::now();
        let result = client
            .get(TEST_URL)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        let latency_ms = began.elapsed().as_millis() as i64;
        match result {
            Ok(response) => Ok(Response::new(TestConfigResponse {
                success: true,
                message: format!(
                    "GET {TEST_URL} -> {} over {:?}",
                    response.status(),
                    response.version()
                ),
                latency_ms,
            })),
            Err(err) => Ok(Response::new(TestConfigResponse {
                success: false,
                message: transport::safe_reqwest_error(&err),
                latency_ms,
            })),
        }
    }

    type ForwardStream =
        Pin<Box<dyn Stream<Item = Result<ForwardResponse, Status>> + Send + 'static>>;

    async fn forward(
        &self,
        request: Request<Streaming<ForwardRequest>>,
    ) -> Result<Response<Self::ForwardStream>, Status> {
        let mut inbound = request.into_inner();
        let start = match inbound.message().await {
            Ok(Some(ForwardRequest {
                frame: Some(forward_request::Frame::Start(start)),
            })) => start,
            Ok(_) => return Err(Status::invalid_argument("first frame must be start")),
            Err(status) => return Err(status),
        };
        let state = Arc::clone(&self.state);
        let (tx, rx) = mpsc::channel::<Result<ForwardResponse, Status>>(32);
        tokio::spawn(async move {
            run_forward(state, start, inbound, tx).await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

fn error_frame(code: &str, message: String, request_sent: bool) -> ForwardResponse {
    ForwardResponse {
        frame: Some(forward_response::Frame::Error(ForwardResponseError {
            code: code.to_string(),
            message,
            request_sent,
        })),
    }
}

fn version_parts(version: reqwest::Version) -> (&'static str, i32, i32) {
    match version {
        reqwest::Version::HTTP_09 => ("HTTP/0.9", 0, 9),
        reqwest::Version::HTTP_10 => ("HTTP/1.0", 1, 0),
        reqwest::Version::HTTP_11 => ("HTTP/1.1", 1, 1),
        reqwest::Version::HTTP_2 => ("HTTP/2.0", 2, 0),
        reqwest::Version::HTTP_3 => ("HTTP/3.0", 3, 0),
        _ => ("HTTP/1.1", 1, 1),
    }
}

async fn run_forward(
    state: Arc<SharedState>,
    start: ForwardRequestStart,
    mut inbound: Streaming<ForwardRequest>,
    tx: mpsc::Sender<Result<ForwardResponse, Status>>,
) {
    let config = state.current_config();
    let body_cap = config.max_request_body_mb as usize * 1024 * 1024;

    if let Err(message) = transport::validate_forward_target(&start) {
        let _ = tx
            .send(Ok(error_frame("PLUGIN_BAD_REQUEST", message, false)))
            .await;
        return;
    }

    // 1. 收齐请求体（真实 Codex 以 Content-Length 完整发送 JSON 体，
    //    缓冲后交给 reqwest 同样产生定长请求，不退化为 chunked）。
    let mut body: Vec<u8> = Vec::new();
    loop {
        match inbound.message().await {
            Ok(Some(frame)) => match frame.frame {
                Some(forward_request::Frame::BodyChunk(chunk)) => {
                    if body.len() + chunk.len() > body_cap {
                        let _ = tx
                            .send(Ok(error_frame(
                                "PLUGIN_BODY_TOO_LARGE",
                                format!("request body exceeds {} MB", config.max_request_body_mb),
                                false,
                            )))
                            .await;
                        return;
                    }
                    body.extend_from_slice(&chunk);
                }
                Some(forward_request::Frame::BodyEnd(_)) => break,
                Some(forward_request::Frame::Start(_)) => {
                    let _ = tx
                        .send(Ok(error_frame(
                            "PLUGIN_PROTOCOL_ERROR",
                            "duplicate start frame".to_string(),
                            false,
                        )))
                        .await;
                    return;
                }
                None => {}
            },
            Ok(None) => {
                let _ = tx
                    .send(Ok(error_frame(
                        "PLUGIN_PROTOCOL_ERROR",
                        "request stream closed before body_end".to_string(),
                        false,
                    )))
                    .await;
                return;
            }
            Err(status) => {
                // 宿主取消（下游断开 / failover 超时）：无需再回帧。
                let _ = tx
                    .send(Ok(error_frame(
                        "PLUGIN_REQUEST_ABORTED",
                        format!("request stream error: {status}"),
                        false,
                    )))
                    .await;
                return;
            }
        }
    }

    // 2. 取 client（按 账号 × 代理 × 协议 缓存；cookie jar 按账号隔离）。
    let client = match state
        .clients
        .client_for(&config, start.account_id, &start.proxy_url)
    {
        Ok(client) => client,
        Err(message) => {
            let _ = tx
                .send(Ok(error_frame("PLUGIN_CLIENT_BUILD", message, false)))
                .await;
            return;
        }
    };

    // 2.1 留一条真实流量形状给巡检 / BPS 自检复用（只记形状，不落盘）。
    if identity::is_codex_backend_request(&start.url) && start.url.contains("/responses") {
        let template_headers: Vec<(String, Vec<String>)> = start
            .headers
            .iter()
            .map(|(name, values)| (name.clone(), values.values.clone()))
            .collect();
        state.template.record(
            &start.url,
            template_headers,
            &body,
            crate::donor::now_ms(),
            2 * 1024 * 1024,
        );
    }

    // 3. 构造请求：方法 + URL + canonical 顺序的请求头 + 定长请求体。
    let method = match reqwest::Method::from_bytes(start.method.as_bytes()) {
        Ok(method) => method,
        Err(err) => {
            let _ = tx
                .send(Ok(error_frame(
                    "PLUGIN_BAD_REQUEST",
                    format!("invalid method {:?}: {err}", start.method),
                    false,
                )))
                .await;
            return;
        }
    };
    let codex_backend = identity::is_codex_backend_request(&start.url);
    let strict_native_headers = config.identity.profile != "passthrough";
    // BPS 会话键必须用「身份改写之前」的原始请求来算。one_id_per_request 会把
    // prompt_cache_key 换成每请求一个新 ID，machine 模式也会换成伪名；等改写完
    // 再取键，BPS 会话粘滞和 previous_response_id 门禁就都绑定不到同一会话上。
    let client_session_key = bps_client_conversation_key(&start.headers, &body);
    let mut headers =
        transport::ordered_headers(&start.headers, codex_backend, strict_native_headers);

    // 3.1 身份 Profile 边缘改写（仅 ChatGPT Codex 内部接口请求）。
    if codex_backend {
        if let Some(resolved) =
            identity::resolve_identity(&config.identity, &state.effective_version(&config))
        {
            identity::apply_identity_headers(&mut headers, &resolved, &config.identity.residency);
        }
        if config.one_id_per_request {
            // 一并发一套 ID：每条请求独立 session/thread/request-id + 独立设备 id，
            // 并剥离 turn-state。应对同账号多路并发共用脏会话触发 server_is_overloaded。
            let ids = identity::RequestIds::mint();
            identity::apply_one_id_headers(&mut headers, &ids);
            if let Some(rewritten) = identity::apply_one_id_body(&body, &ids) {
                body = rewritten;
            }
        } else if config.identity.fingerprint_mode == "machine" {
            let machine =
                identity::MachineContext::new(&config.identity, start.account_id, &headers);
            identity::apply_machine_headers(&mut headers, &machine);
            if let Some(rewritten) = identity::apply_machine_body(&body, &machine) {
                body = rewritten;
            }
        } else {
            // 指纹总开关：开 = 按账号派生独立设备 id；关 = 全部账号共用同一个稳定设备 id。
            let fingerprint_account = if config.per_account() {
                start.account_id
            } else {
                0
            };
            let installation_id = identity::per_account_installation_id(
                &config.identity.installation_id_seed,
                fingerprint_account,
            );
            identity::apply_installation_id_headers(&mut headers, &installation_id);
            if let Some(rewritten) = identity::apply_installation_id_body(&body, &installation_id) {
                body = rewritten;
            }
        }
    }

    // 3.2 降智账号 BPS 通道（默认关）：仅对勾了「自动降智处理」的账号、且仅对
    //     bps_models 列出的模型生效。改写后出站 URL 换成 BPS 端点，
    //     请求体剥掉客户端 tools，响应流再把载体工具翻译回真实 function_call。
    let mut bps_stream: Option<crate::bps::BpsStream> = None;
    // 追踪用：这条请求的账号 / 模型（面板 `/api/bps/stream` 显示）。
    let bps_trace_account = start.account_id;
    let mut bps_trace_model = String::new();
    // BPS 出站失败（4xx/5xx 或传输错误）时用来立刻回退到正常 Codex 通道的原始请求，
    // 保证这条实验通道永远不会把客户端卡死。
    let mut bps_fallback: Option<(String, reqwest::header::HeaderMap, Vec<u8>)> = None;
    let mut request_url = start.url.clone();
    // 本会话（账号 + 会话键）的通道粘滞键：避免同一会话中途换端点撕裂上游缓存。
    let mut sticky_key: Option<String> = None;
    // 回退路径已经记过线路统计就别在终点重复记账（同一条请求只记一次）。
    let mut route_counted = false;
    if codex_backend && config.bps_enabled {
        state.degrade.ensure_loaded(&config.degrade_state_file());
        let now = crate::donor::now_ms();
        let decision = state.degrade.decision(start.account_id, &config, now);
        let sticky_ms = config.bps_session_sticky_seconds as u64 * 1000;
        let rewritten_session_key = bps_conversation_key(&start.headers, &body);
        let conversation_key = client_session_key.clone();
        sticky_key = conversation_key
            .as_ref()
            .map(|key| bps_sticky_key(start.account_id, key));
        let normal_pin_key = conversation_key.as_deref().map(normal_pin_key);
        let sticky_bps = sticky_key
            .as_deref()
            .map(|key| state.channels.session_wants_bps(key, now, sticky_ms))
            .unwrap_or(false);
        let cooling = state.channels.cooldown_active(start.account_id, now);
        // previous_response_id 门禁（默认开）。BPS 上游严格白名单会 422 掉这个
        // 字段，插件只能剥掉再发：对「靠服务端状态续写」的客户端（每轮只发增量
        // input + 上一轮 id）剥掉就等于丢历史，表现为上下文接不上。所以命中就把
        // 这条请求改走账号正常通道，并把整条会话钉住（只跳单条会撕裂会话）。
        let pin_ms = config.bps_previous_response_pin_seconds as u64 * 1000;
        let pinned_normal = normal_pin_key
            .as_deref()
            .map(|key| state.channels.session_pinned_normal(key, now, pin_ms))
            .unwrap_or(false);
        // 附件闸门：上游对 base64 图片 / 文件一律 422，只有绝对 https 图片地址
        // 能透传（网关自己下载）。勾了「带附件请求跳过 BPS」时，带这类附件的请求
        // 直接走正常通道，宁可这条不吃降智兜底，也不把客户的图丢掉。
        let mut media_blocks_bps = false;
        let mut dynamic_tools_blocks_bps = false;
        let mut previous_response_blocks_bps = false;
        let mut client_gate_blocks_bps = false;
        let has_prev_id = crate::bps::has_previous_response_id(&body);
        // 这个账号这条请求本来就想走 BPS（降智处理开 + 未冷却 + 判定或会话粘滞要求）。
        // 客户端门禁只在「本来要走 BPS」时才有意义，免得给根本没进 BPS 的账号
        // 也挂一条「跳过 BPS」的原因。
        let wants_bps = decision.enabled && !cooling && (decision.use_bps || sticky_bps);
        // 客户端门禁：BPS 上游会给每条请求注入自己那套 Excel/Work 插件上下文
        // 与账号级人设，第三方聊天客户端（WorkBuddy / OpenClaw 等）会被那套
        // 上下文盖掉，表现为答非所问或串到上游自带人设。官方 Codex 客户端
        // 自带完整会话与系统提示，注入只是白耗缓存，所以只放官方客户端过门。
        // 判定口径与 sub2api `IsCodexOfficialClientByHeaders` 一致（UA 前缀集 +
        // `Codex ` 家族 + UA 尾部 name 兜底 + originator 精确集合）。
        //
        // 身份来源优先级：宿主透传头（`x-sub2api-client-user-agent` /
        // `x-sub2api-client-originator`，宿主在身份收口前抓到客户端原始身份）→
        // 请求头 user-agent / originator。
        //
        // 为什么必须先看宿主透传头：宿主的出站身份收口（指纹收敛 / 统一出口）会把
        // user-agent / originator 强制改写成网关规范 Codex 身份，插件直接读请求头
        // 只会看到官方形态，门禁会永远放行。旧宿主不写这两个头时退回旧行为。
        let (client_user_agent, client_originator, client_source) =
            crate::identity::pick_client_identity(
                raw_header(&start.headers, crate::identity::HOST_CLIENT_UA_HEADER),
                raw_header(
                    &start.headers,
                    crate::identity::HOST_CLIENT_ORIGINATOR_HEADER,
                ),
                raw_header(&start.headers, "user-agent"),
                raw_header(&start.headers, "originator"),
            );
        let client_label = format!(
            "{} src={client_source}",
            crate::identity::client_identity_label(client_user_agent, client_originator)
        );
        let official_codex_client =
            crate::identity::is_official_codex_client(client_user_agent, client_originator);
        if config.bps_official_client_only && !official_codex_client && wants_bps {
            client_gate_blocks_bps = true;
        }
        if wants_bps {
            let media = crate::bps::media_gate(&body, config.bps_keep_https_images);
            media_blocks_bps =
                config.bps_skip_on_media && media == crate::bps::MediaGate::Unsupported;
            // 动态工具协议（tool_search）在 BPS 上必然失效：顶层 tools 会被摘掉、
            // 回程还会抑制 tool_search_* 项。命中就让这条请求走账号正常通道。
            dynamic_tools_blocks_bps =
                config.bps_skip_on_dynamic_tools && crate::bps::has_dynamic_tools(&body);
            // previous_response_id：命中就跳过 BPS，并把这个会话（账号 + 会话键）
            // 钉住 bps_previous_response_pin_seconds 秒，避免同一会话在两条通道
            // 之间来回跳。
            if config.bps_skip_on_previous_response_id && has_prev_id {
                previous_response_blocks_bps = true;
                if let Some(key) = normal_pin_key.as_deref() {
                    state.channels.pin_session_normal(key, now, pin_ms);
                }
            }
        }
        // 不合格 → 立刻走 BPS；已恢复 → 进行中的会话继续留在 BPS（缓存亲和），
        // 只有新会话才用正常通道；BPS 刚失败过 → 冷却期内直接用正常通道。
        let mut use_bps = wants_bps
            && !media_blocks_bps
            && !dynamic_tools_blocks_bps
            && !previous_response_blocks_bps
            && !client_gate_blocks_bps
            && !pinned_normal;
        if !use_bps && client_gate_blocks_bps {
            // 留痕：这条请求为什么没吃到降智兜底（面板原因 + bps-notes 现场行）。
            let reason = "非官方 Codex 客户端，跳过 BPS 走正常通道".to_string();
            state
                .channels
                .set_last_reason(start.account_id, reason, now);
            crate::bps::note_throttled(
                &config,
                start.account_id,
                &client_label,
                3_600_000,
                &format!(
                    "客户端门禁跳过 BPS model={:?} {}",
                    body_model(&body),
                    client_label
                ),
            );
        } else if !use_bps && previous_response_blocks_bps {
            state.channels.set_last_reason(
                start.account_id,
                if pin_ms > 0 {
                    format!(
                        "带 previous_response_id，跳过 BPS，整条会话钉在正常通道 {}s",
                        config.bps_previous_response_pin_seconds
                    )
                } else {
                    "带 previous_response_id，本条跳过 BPS".to_string()
                },
                now,
            );
        } else if !use_bps && pinned_normal && decision.enabled && !cooling {
            state.channels.set_last_reason(
                start.account_id,
                "本会话已因 previous_response_id 钉在正常通道".to_string(),
                now,
            );
        }
        // 每账号每日 BPS 次数上限（0 = 不限）：超了就直接走该账号的正常通道，
        // 只在面板留一条原因，客户端这一条请求不受影响。
        if use_bps && config.bps_daily_limit_per_account > 0 {
            let used = state.channels.daily_count(start.account_id, now);
            if used >= config.bps_daily_limit_per_account {
                use_bps = false;
                state.channels.set_last_reason(
                    start.account_id,
                    format!(
                        "今日 BPS 次数已达上限 {}（已用 {}），本条走正常通道",
                        config.bps_daily_limit_per_account, used
                    ),
                    now,
                );
            }
        }
        // previous_response_id 门禁的现场记录：只在少见的续写会话上写一行，
        // 方便排查「这条会话为什么没走 BPS / 为什么钉不住」。
        if has_prev_id || previous_response_blocks_bps || pinned_normal {
            crate::bps::note(
                &config,
                start.account_id,
                &format!(
                    "会话门禁 url={} model={:?} 原始键={:?} 改写后键={:?} previous_response_id={} sticky={} pinned={} media={} dyn={} decision={} cooling={} 结果={}",
                    start.url,
                    body_model(&body),
                    client_session_key,
                    rewritten_session_key,
                    has_prev_id,
                    sticky_bps,
                    pinned_normal,
                    media_blocks_bps,
                    dynamic_tools_blocks_bps,
                    decision.use_bps,
                    cooling,
                    if use_bps { "bps" } else { "正常通道" }
                ),
            );
        }
        if use_bps {
            if let Some(key) = sticky_key.as_deref() {
                state.channels.mark_session(key, true, now);
            }
            let model = body_model(&body);
            if let Some(model) = model {
                bps_trace_model = model.clone();
                if crate::bps::is_bps_model(&config.bps_model_list(), &model) {
                    let bridge = crate::bps::BridgeOptions::from_config(&config);
                    // 会话作用域：账号级；会话锚点优先使用宿主已经按会员/API Key
                    // 隔离过的 prompt_cache_key / session / thread / window 标识。
                    // 完全没有稳定标识时用本次宿主 request_id：宁可这一类客户端不跨
                    // 请求复用 BPS 缓存，也不能让多个会员因相同首问共用 task/turn。
                    let scope = format!("acct:{}", start.account_id);
                    let session_anchor = bps_session_anchor(
                        client_session_key.as_deref(),
                        start.request_id.as_str(),
                    );
                    match crate::bps::prepare_request_with_session_key_scoped(
                        &body,
                        Some(&scope),
                        Some(&session_anchor),
                        &bridge,
                    ) {
                        Ok(prepared) => {
                            state.degrade.mark_bps_used(start.account_id, now);
                            // 诊断留痕：这条请求原样带了哪些客户端工具、桥接是否把目录写进提示词。
                            crate::bps::remember_last_rewrite(crate::bps::rewrite_note(
                                start.account_id,
                                &model,
                                bridge.mode,
                                &body,
                                config.bps_endpoint.trim(),
                            ));
                            let targets = crate::bps::call_targets(&body);
                            // 回程改写选项必须在 body 换成改写后版本之前，从客户端原始请求体里取：
                            // 清洗要把上游回显的 instructions / tools 换回客户端自己发的那份。
                            let scrub = config
                                .bps_scrub_echo
                                .then(|| crate::bps::EchoScrub::from_request(&body))
                                .flatten();
                            bps_fallback =
                                Some((request_url.clone(), headers.clone(), body.clone()));
                            body = prepared.body;
                            request_url = config.bps_endpoint.trim().to_string();
                            // 记一次「今天真的打了 BPS」：每日上限判定与面板展示都用它。
                            state.channels.bump_daily(start.account_id, now);
                            // BPS 客户端特征头（auth-mode / accept-encoding / Origin /
                            // 可选 UA 实验档）统一从配置生成，出站与面板自检同一份口径。
                            for (name, value) in crate::bps::bps_client_headers(&config) {
                                let (Ok(name), Ok(value)) = (
                                    reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                                    reqwest::header::HeaderValue::from_str(&value),
                                ) else {
                                    continue;
                                };
                                headers.insert(name, value);
                            }
                            bps_stream = Some(
                                crate::bps::BpsStream::with_targets_and_scope(
                                    &bridge,
                                    targets,
                                    prepared.call_cache_scope,
                                )
                                .with_response_rewrite(scrub, config.bps_normalize_usage),
                            );
                        }
                        // body 不是可用的 Responses JSON：保持旧行为，原样放行。
                        Err(crate::bps::PrepareError::NotApplicable) => {}
                        Err(err) => {
                            // 本地拒绝：例如客户端给了上游不认识的推理挡位。不改写、
                            // 不上 BPS，这条走该账号的正常通道，并落盘留痕 —— 宿主会
                            // 吞掉插件的 stderr，只有日志文件查得到原因。
                            crate::bps::note(
                                &config,
                                start.account_id,
                                &format!("skip bps (local reject) model={model}: {err}"),
                            );
                            if let Some(key) = sticky_key.as_deref() {
                                state.channels.mark_session(key, false, now);
                            }
                        }
                    }
                }
            }
        } else if !media_blocks_bps && !dynamic_tools_blocks_bps {
            if let Some(key) = sticky_key.as_deref() {
                // 本会话改走正常通道：记下来，避免它下一轮又被粘回 BPS。
                state.channels.mark_session(key, false, now);
            }
        }
    }
    let bps_active = bps_stream.is_some();

    // 3.3 推理挡位自愈（默认开）。现网最大的一类 400 跟内容无关：客户端按自己的
    //     能力表挑了 `minimal`，而那个模型只认 `none/low/medium/high/xhigh`
    //     （142 上 24h 命中 9000+ 次，全是 gpt-5.5 的 /v1/chat/completions）。
    //     记过的组合在这里直接换成上游支持的挡位，省掉一次注定 400 的往返；
    //     第一次撞上时由下面的响应分支按上游给的支持列表重发一次。
    //     BPS 通道的挡位在 prepare_request 里已经归一化，不走这条。
    if config.effort_retry_enabled && !bps_active {
        if let (Some(model), Some(requested)) =
            (body_model(&body), crate::bps::requested_effort_of(&body))
        {
            if let Some(replacement) = crate::bps::remembered_effort_fix(&model, &requested) {
                if let Some(rewritten) = crate::bps::replace_effort(&body, &replacement) {
                    crate::bps::note(
                        &config,
                        start.account_id,
                        &format!("effort pre-fix: {model} {requested} -> {replacement}"),
                    );
                    body = rewritten;
                }
            }
        }
    }

    let began = Instant::now();
    // 挡位自愈要用原始 body 重发：转成 Bytes 后克隆只是引用计数，不复制内容
    // （`.clone()` 花的是计数，不是 128MB 的上下文）。
    let retry_method = method.clone();
    let mut retry_headers = headers.clone();
    // 自愈会改写 body（挡位字面量长度会变），content-length 必须由 reqwest 按新
    // body 重算，不能沿用宿主给的那一份。
    retry_headers.remove(reqwest::header::CONTENT_LENGTH);
    let request_body = bytes::Bytes::from(body);
    let retry_body: Option<bytes::Bytes> =
        config.effort_retry_enabled.then(|| request_body.clone());
    let request = client
        .request(method.clone(), &request_url)
        .headers(headers)
        .body(request_body);

    let mut response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            // BPS 连接层失败：直接回退原通道，别把错误丢给客户端。
            if let Some((url, fallback_headers, original_body)) = bps_fallback.take() {
                note_bps_failure(
                    &state,
                    start.account_id,
                    sticky_key.as_deref(),
                    &config,
                    None,
                );
                // BPS 连接层就失败了：算 BPS 线一次失败（状态码 0）。
                record_route(&state, &config, start.account_id, "bps", 0);
                route_counted = true;
                crate::bps::note(
                    &config,
                    start.account_id,
                    &format!(
                        "bps transport error: {}",
                        transport::classify_reqwest_error(&err).message
                    ),
                );
                eprintln!(
                    "[codex-native-transport] bps transport error ({}), falling back to {url}",
                    transport::classify_reqwest_error(&err).message
                );
                match client
                    .request(method.clone(), &url)
                    .headers(fallback_headers)
                    .body(original_body)
                    .send()
                    .await
                {
                    Ok(retry) => {
                        record_route(
                            &state,
                            &config,
                            start.account_id,
                            "normal",
                            retry.status().as_u16(),
                        );
                        bps_stream = None;
                        retry
                    }
                    Err(err) => {
                        record_route(&state, &config, start.account_id, "normal", 0);
                        let classified = transport::classify_reqwest_error(&err);
                        let _ = tx
                            .send(Ok(error_frame(
                                classified.code,
                                classified.message,
                                classified.request_sent,
                            )))
                            .await;
                        return;
                    }
                }
            } else {
                record_route(&state, &config, start.account_id, "normal", 0);
                let classified = transport::classify_reqwest_error(&err);
                let _ = tx
                    .send(Ok(error_frame(
                        classified.code,
                        classified.message,
                        classified.request_sent,
                    )))
                    .await;
                return;
            }
        }
    };
    if let Some((url, headers, original_body)) = bps_fallback {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            note_bps_failure(
                &state,
                start.account_id,
                sticky_key.as_deref(),
                &config,
                Some(status),
            );
            // BPS 端点回了非 2xx：算 BPS 线一次失败。
            record_route(&state, &config, start.account_id, "bps", status);
            route_counted = true;
            let snippet = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>();
            crate::bps::note(
                &config,
                start.account_id,
                &format!("bps rejected {status}: {snippet}"),
            );
            eprintln!(
                "[codex-native-transport] bps rejected with {status} ({snippet}), falling back to {url}"
            );
            match client
                .request(method, &url)
                .headers(headers)
                .body(original_body)
                .send()
                .await
            {
                Ok(retry) => {
                    record_route(
                        &state,
                        &config,
                        start.account_id,
                        "normal",
                        retry.status().as_u16(),
                    );
                    response = retry;
                    bps_stream = None;
                }
                Err(err) => {
                    record_route(&state, &config, start.account_id, "normal", 0);
                    let classified = transport::classify_reqwest_error(&err);
                    let _ = tx
                        .send(Ok(error_frame(
                            classified.code,
                            format!("bps {status} 且回退原通道失败: {}", classified.message),
                            classified.request_sent,
                        )))
                        .await;
                    return;
                }
            }
        }
    }

    // 3.4 挡位自愈的响应分支：上游用 400 明确说「这个挡位我不认」时，按它给出的
    //     支持列表换成最接近的一档重发一次（成功后记住这个组合，后续同类请求在
    //     3.3 里直接换）。只在正常通道、且这条 400 确实点名了客户端给的那个挡位
    //     时触发；不满足条件就把已经读进内存的 400 原样回放给宿主。
    if !bps_active {
        if let (Some(original), true) = (retry_body.as_ref(), response.status().as_u16() == 400) {
            let status = response.status();
            let version = response.version();
            let upstream_headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            let model = body_model(original).unwrap_or_default();
            let requested = crate::bps::requested_effort_of(original).unwrap_or_default();
            let replacement = (!model.is_empty() && !requested.is_empty())
                .then(|| crate::bps::effort_correction(&text))
                .flatten()
                .filter(|(rejected, _)| rejected == &requested)
                .map(|(_, replacement)| replacement)
                .filter(|replacement| crate::bps::replace_effort(original, replacement).is_some());
            let mut healed = None;
            if let Some(replacement) = replacement {
                // 上面 filter 里试过一次，这里必定能改写成功。
                let rewritten = crate::bps::replace_effort(original, &replacement)
                    .expect("replace_effort pre-checked");
                crate::bps::remember_effort_fix(&model, &requested, &replacement);
                crate::bps::note(
                    &config,
                    start.account_id,
                    &format!("effort self-heal: {model} {requested} -> {replacement}"),
                );
                match client
                    .request(retry_method.clone(), &request_url)
                    .headers(retry_headers.clone())
                    .body(rewritten)
                    .send()
                    .await
                {
                    Ok(retry) => healed = Some(retry),
                    Err(err) => {
                        let classified = transport::classify_reqwest_error(&err);
                        record_route(&state, &config, start.account_id, "normal", 0);
                        let _ = tx
                            .send(Ok(error_frame(
                                classified.code,
                                format!("挡位自愈重发失败: {}", classified.message),
                                classified.request_sent,
                            )))
                            .await;
                        return;
                    }
                }
            }
            match healed {
                Some(retry) => response = retry,
                None => {
                    record_route(&state, &config, start.account_id, "normal", status.as_u16());
                    let (protocol, protocol_major, protocol_minor) = version_parts(version);
                    let status_line = match status.canonical_reason() {
                        Some(reason) => format!("{} {}", status.as_u16(), reason),
                        None => status.as_u16().to_string(),
                    };
                    let mut header_map = std::collections::HashMap::new();
                    for name in upstream_headers.keys() {
                        let values: Vec<String> = upstream_headers
                            .get_all(name)
                            .iter()
                            .filter_map(|value| value.to_str().ok().map(str::to_string))
                            .collect();
                        header_map.insert(name.as_str().to_string(), HeaderValues { values });
                    }
                    let bytes = text.into_bytes();
                    let length = bytes.len() as i64;
                    if tx
                        .send(Ok(ForwardResponse {
                            frame: Some(forward_response::Frame::Start(ForwardResponseStart {
                                status_code: status.as_u16() as i32,
                                status: status_line,
                                protocol: protocol.to_string(),
                                protocol_major,
                                protocol_minor,
                                headers: header_map,
                                content_length: length,
                            })),
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if !bytes.is_empty()
                        && tx
                            .send(Ok(ForwardResponse {
                                frame: Some(forward_response::Frame::BodyChunk(bytes)),
                            }))
                            .await
                            .is_err()
                    {
                        return;
                    }
                    let _ = tx
                        .send(Ok(ForwardResponse {
                            frame: Some(forward_response::Frame::End(ForwardResponseEnd {
                                bytes_received: length,
                                duration_ms: began.elapsed().as_millis() as i64,
                            })),
                        }))
                        .await;
                    return;
                }
            }
        }
    }

    // 线路统计：没走过回退的请求，在终点按「最终哪条线路 + 上游状态码」记一次。
    if !route_counted {
        let route = if bps_stream.is_some() {
            "bps"
        } else {
            "normal"
        };
        record_route(
            &state,
            &config,
            start.account_id,
            route,
            response.status().as_u16(),
        );
    }

    // 4. 响应头帧。
    let status_code = response.status().as_u16() as i32;
    let (protocol, protocol_major, protocol_minor) = version_parts(response.version());
    let status_line = match response.status().canonical_reason() {
        Some(reason) => format!("{} {}", response.status().as_u16(), reason),
        None => response.status().as_u16().to_string(),
    };
    let mut header_map = std::collections::HashMap::new();
    for name in response.headers().keys() {
        let values: Vec<String> = response
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_string))
            .collect();
        header_map.insert(name.as_str().to_string(), HeaderValues { values });
    }
    let content_length = response
        .content_length()
        .map(|length| length as i64)
        .unwrap_or(-1);

    if tx
        .send(Ok(ForwardResponse {
            frame: Some(forward_response::Frame::Start(ForwardResponseStart {
                status_code,
                status: status_line,
                protocol: protocol.to_string(),
                protocol_major,
                protocol_minor,
                headers: header_map,
                // BPS 通道会重写 SSE 分帧，长度不再等于上游值，交给宿主按流读取。
                content_length: if bps_active { -1 } else { content_length },
            })),
        }))
        .await
        .is_err()
    {
        return;
    }

    // 5. 流式转发响应体（SSE 逐块低延迟回传）。
    let mut stream = response.bytes_stream();
    let mut bytes_received: i64 = 0;
    let mut bps = bps_stream;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let payload = match bps.as_mut() {
                    Some(rewriter) => rewriter.push(&bytes),
                    None => bytes.to_vec(),
                };
                if payload.is_empty() {
                    continue;
                }
                bytes_received += payload.len() as i64;
                if tx
                    .send(Ok(ForwardResponse {
                        frame: Some(forward_response::Frame::BodyChunk(payload)),
                    }))
                    .await
                    .is_err()
                {
                    // 宿主已放弃读取（下游断开），停止拉取上游。
                    return;
                }
            }
            Err(err) => {
                let _ = tx
                    .send(Ok(error_frame(
                        "PLUGIN_UPSTREAM_READ",
                        transport::safe_reqwest_error(&err),
                        true,
                    )))
                    .await;
                return;
            }
        }
    }

    // BPS 通道：吐出末尾未成行/未完整分帧的残留字节。
    if let Some(rewriter) = bps.as_mut() {
        let rest = rewriter.finish();
        if !rest.is_empty() {
            bytes_received += rest.len() as i64;
            if tx
                .send(Ok(ForwardResponse {
                    frame: Some(forward_response::Frame::BodyChunk(rest)),
                }))
                .await
                .is_err()
            {
                return;
            }
        }
    }
    // 诊断留痕：这条响应流的原始帧与改写结果（面板 `/api/bps/stream`）。
    if let Some(rewriter) = bps.as_ref() {
        crate::bps::remember_stream_trace(rewriter.trace(bps_trace_account, &bps_trace_model));
    }

    let _ = tx
        .send(Ok(ForwardResponse {
            frame: Some(forward_response::Frame::End(ForwardResponseEnd {
                bytes_received,
                duration_ms: began.elapsed().as_millis() as i64,
            })),
        }))
        .await;
}
/// 从请求体里取模型名（BPS 通道的路由判据）。
fn body_model(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

/// 从宿主转发的原始请求头里取第一个值（头名大小写不敏感）。
///
/// `start.headers` 是宿主原样透传的 map（键名保留宿主的大小写形态，例如
/// `User-Agent`），所以不能直接按下标取。
fn raw_header<'a>(headers: &'a HashMap<String, HeaderValues>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, values)| values.values.first())
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// BPS 通道的会话键：优先请求体里的 `prompt_cache_key`，其次会话相关请求头，
/// 最后读取 `client_metadata`。宿主会先把这些值按 API Key + OAuth 账号作用域
/// 隔离，再交给插件；这里读取的是隔离后的值，不是会员原始标识。
fn bps_conversation_key(headers: &HashMap<String, HeaderValues>, body: &[u8]) -> Option<String> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        for path in [
            &["prompt_cache_key"][..],
            &["client_metadata", "session_id"][..],
            &["client_metadata", "session-id"][..],
            &["client_metadata", "thread_id"][..],
            &["client_metadata", "thread-id"][..],
            &["client_metadata", "x-codex-window-id"][..],
        ] {
            let mut current = &value;
            let mut found = true;
            for segment in path {
                current = match current.get(*segment) {
                    Some(next) => next,
                    None => {
                        found = false;
                        break;
                    }
                };
            }
            if found {
                if let Some(key) = current
                    .as_str()
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                {
                    return Some(key.to_string());
                }
            }
        }
        // Codex 还会把同一组标识嵌在 client_metadata 的 JSON 字符串头里；宿主
        // 的账号/API Key 收敛层会同步改写这层内容，优先取其中的会话字段而不是
        // 把整段 JSON 当作一个键。
        if let Some(raw) = value
            .get("client_metadata")
            .and_then(|metadata| metadata.get("x-codex-turn-metadata"))
            .and_then(serde_json::Value::as_str)
        {
            if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(raw) {
                for name in [
                    "session_id",
                    "session-id",
                    "thread_id",
                    "thread-id",
                    "window_id",
                ] {
                    if let Some(key) = metadata
                        .get(name)
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                    {
                        return Some(key.to_string());
                    }
                }
            }
        }
    }
    // Go 的 http.Header 会把头名规范化成 `X-Codex-Window-Id`；必须大小写
    // 不敏感读取。旧实现直接 HashMap::get(小写名)，实际线上经常永远取不到。
    for name in [
        "session-id",
        "session_id",
        "thread-id",
        "thread_id",
        "x-codex-window-id",
    ] {
        if let Some(value) = raw_header(headers, name) {
            return Some(value.to_string());
        }
    }
    None
}

/// 优先使用新宿主在账号指纹改写前捕获的会员会话键。
///
/// * explicit：只信任私有 key，忽略宿主后来注入的账号级 session；
/// * none：确认客户端没有显式稳定键，必须退到本次 request_id；
/// * 私有头缺失：兼容旧宿主，沿用旧的请求体/头提取逻辑。
fn bps_client_conversation_key(
    headers: &HashMap<String, HeaderValues>,
    body: &[u8],
) -> Option<String> {
    match raw_header(
        headers,
        crate::identity::HOST_CLIENT_CONVERSATION_SOURCE_HEADER,
    ) {
        Some(source) if source.eq_ignore_ascii_case("explicit") => raw_header(
            headers,
            crate::identity::HOST_CLIENT_CONVERSATION_KEY_HEADER,
        )
        .map(str::to_string),
        Some(source) if source.eq_ignore_ascii_case("none") => None,
        Some(_) => None,
        None => bps_conversation_key(headers, body),
    }
}

fn bps_sticky_key(account_id: i64, conversation_key: &str) -> String {
    format!("acct:{account_id}:conversation:{conversation_key}")
}

/// 正常通道 pin 故意不包含上游账号：调度换账号后仍属于同一会员会话。
fn normal_pin_key(conversation_key: &str) -> String {
    format!("client-conversation:{conversation_key}")
}

/// 给 BPS 派生标识使用的本地会话锚点。稳定客户端会话键优先；没有任何会话键时
/// 退到宿主为每次转发生成的唯一 request_id，确保不同会员不会因相同输入碰撞。
fn bps_session_anchor(client_session_key: Option<&str>, request_id: &str) -> String {
    if let Some(key) = client_session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return format!("session:{key}");
    }
    let request_id = request_id.trim();
    if request_id.is_empty() {
        return format!("request:{}", uuid::Uuid::now_v7());
    }
    format!("request:{request_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_cooldowns() -> PluginConfig {
        let mut config = PluginConfig::default();
        config.bps_cooldown_timeout_seconds = 11;
        config.bps_cooldown_400_seconds = 22;
        config.bps_cooldown_401_seconds = 33;
        config.bps_cooldown_403_seconds = 44;
        config.bps_cooldown_429_seconds = 55;
        config.bps_fallback_cooldown_seconds = 66;
        config
    }

    #[test]
    fn bps_cooldown_is_per_status_class() {
        let config = config_with_cooldowns();
        assert_eq!(bps_cooldown_for(&config, None).0, 11);
        assert_eq!(bps_cooldown_for(&config, Some(400)).0, 22);
        assert_eq!(bps_cooldown_for(&config, Some(401)).0, 33);
        assert_eq!(bps_cooldown_for(&config, Some(403)).0, 44);
        assert_eq!(bps_cooldown_for(&config, Some(429)).0, 55);
        // 其它状态码（含 5xx）走兜底值。
        assert_eq!(bps_cooldown_for(&config, Some(422)).0, 66);
        assert_eq!(bps_cooldown_for(&config, Some(503)).0, 66);
        assert_eq!(bps_cooldown_for(&config, Some(503)).1, "BPS 5xx 上游异常");
        assert_eq!(bps_cooldown_for(&config, Some(422)).1, "BPS 其它错误");
        // 0 = 该类错误不设冷却。
        let mut off = config_with_cooldowns();
        off.bps_cooldown_403_seconds = 0;
        assert_eq!(bps_cooldown_for(&off, Some(403)).0, 0);
    }

    #[test]
    fn bps_conversation_key_reads_canonicalized_header_names() {
        let mut headers = HashMap::new();
        headers.insert(
            "X-Codex-Window-Id".to_string(),
            HeaderValues {
                values: vec!["window-scoped-by-host".to_string()],
            },
        );
        assert_eq!(
            bps_conversation_key(&headers, br#"{"model":"gpt-6-astra"}"#),
            Some("window-scoped-by-host".to_string())
        );
    }

    #[test]
    fn bps_conversation_key_prefers_body_prompt_key_and_reads_client_metadata() {
        let headers = HashMap::new();
        let body = br#"{"model":"gpt-6-astra","client_metadata":{"thread_id":"thread-scoped"}}"#;
        assert_eq!(
            bps_conversation_key(&headers, body),
            Some("thread-scoped".to_string())
        );
        let nested = br#"{"client_metadata":{"x-codex-turn-metadata":"{\"session_id\":\"nested-session\"}"}}"#;
        assert_eq!(
            bps_conversation_key(&headers, nested),
            Some("nested-session".to_string())
        );
        let body_with_prompt =
            br#"{"prompt_cache_key":"body-key","client_metadata":{"thread_id":"thread-scoped"}}"#;
        assert_eq!(
            bps_conversation_key(&headers, body_with_prompt),
            Some("body-key".to_string())
        );
    }

    #[test]
    fn private_explicit_conversation_key_beats_host_fingerprint_fields() {
        let mut headers = HashMap::new();
        headers.insert(
            "X-Sub2api-Client-Conversation-Source".to_string(),
            HeaderValues {
                values: vec!["explicit".to_string()],
            },
        );
        headers.insert(
            "X-Sub2api-Client-Conversation-Key".to_string(),
            HeaderValues {
                values: vec!["member-key".to_string()],
            },
        );
        let body = br#"{"prompt_cache_key":"account-fixed-key","client_metadata":{"session_id":"account-fixed-session"}}"#;
        assert_eq!(
            bps_client_conversation_key(&headers, body),
            Some("member-key".to_string())
        );
    }

    #[test]
    fn private_none_ignores_host_injected_fixed_session() {
        let mut headers = HashMap::new();
        headers.insert(
            "x-sub2api-client-conversation-source".to_string(),
            HeaderValues {
                values: vec!["none".to_string()],
            },
        );
        let body = br#"{"prompt_cache_key":"account-fixed-key","client_metadata":{"session_id":"account-fixed-session"}}"#;
        assert_eq!(bps_client_conversation_key(&headers, body), None);
        assert_ne!(
            bps_session_anchor(None, "req-a"),
            bps_session_anchor(None, "req-b")
        );
    }

    #[test]
    fn old_host_without_private_headers_keeps_legacy_behavior() {
        let headers = HashMap::new();
        let body = br#"{"prompt_cache_key":"legacy-key"}"#;
        assert_eq!(
            bps_client_conversation_key(&headers, body),
            Some("legacy-key".to_string())
        );
    }

    #[test]
    fn normal_pin_key_survives_account_switch_but_bps_sticky_does_not() {
        assert_eq!(normal_pin_key("member-key"), normal_pin_key("member-key"));
        assert_ne!(
            bps_sticky_key(72, "member-key"),
            bps_sticky_key(73, "member-key")
        );
        let sticky = ChannelSticky::default();
        let key = normal_pin_key("member-key");
        sticky.pin_session_normal(&key, 1_000, 30_000);
        assert!(sticky.session_pinned_normal(&normal_pin_key("member-key"), 2_000, 30_000));
    }

    #[test]
    fn bps_session_anchor_falls_back_to_unique_request_id() {
        assert_eq!(
            bps_session_anchor(Some("member-session"), "req-1"),
            "session:member-session"
        );
        assert_eq!(bps_session_anchor(None, "req-1"), "request:req-1");
    }

    #[test]
    fn daily_bps_counter_is_per_account_and_resets_on_utc_rollover() {
        let sticky = ChannelSticky::default();
        // 2026-09-26T05:20:00Z，同一天的 +60s 仍算当天。
        let day1 = 1_790_400_000_000u64;
        assert_eq!(sticky.daily_count(7, day1), 0);
        assert_eq!(sticky.bump_daily(7, day1), 1);
        assert_eq!(sticky.bump_daily(7, day1), 2);
        assert_eq!(sticky.daily_count(7, day1 + 60_000), 2);
        // 账号之间互不影响。
        assert_eq!(sticky.daily_count(8, day1), 0);
        // 跨 UTC 日自动归零。
        assert_eq!(sticky.daily_count(7, day1 + 86_400_000), 0);
        assert_eq!(sticky.bump_daily(7, day1 + 86_400_000), 1);
        assert_eq!(sticky.daily_count(7, day1 + 86_400_000), 1);
    }

    #[test]
    fn cooldown_expires_and_reports_remaining_ms() {
        let sticky = ChannelSticky::default();
        assert!(!sticky.cooldown_active(5, 1_000));
        assert_eq!(sticky.cooldown_remaining_ms(5, 1_000), 0);
        sticky.set_cooldown(5, 10_000);
        assert!(sticky.cooldown_active(5, 9_999));
        assert_eq!(sticky.cooldown_remaining_ms(5, 4_000), 6_000);
        assert_eq!(sticky.cooldown_remaining_ms(5, 10_000), 0);
        assert!(!sticky.cooldown_active(5, 10_000));
    }

    #[test]
    fn bps_last_reason_is_recorded_per_account() {
        let sticky = ChannelSticky::default();
        sticky.set_last_reason(9, "BPS 403 账号被上游拦截，冷却 3600s".to_string(), 1_234);
        let (at, text) = sticky.last_reason(9).expect("reason");
        assert_eq!(at, 1_234);
        assert!(text.contains("403"));
        assert!(sticky.last_reason(10).is_none());
    }

    #[test]
    fn previous_response_pin_blocks_bps_then_expires() {
        let sticky = ChannelSticky::default();
        let key = "7:sess-a";
        // 会话本来在走 BPS。
        sticky.mark_session(key, true, 1_000);
        assert!(sticky.session_wants_bps(key, 1_000, 60_000));
        // 命中 previous_response_id 门禁：整条会话钉到正常通道。
        sticky.pin_session_normal(key, 1_000, 30_000);
        assert!(!sticky.session_wants_bps(key, 1_000, 60_000));
        assert!(sticky.session_pinned_normal(key, 1_000, 30_000));
        assert!(sticky.session_pinned_normal(key, 30_000, 30_000));
        // 过期后不再钉住，会话可以重新进 BPS。
        assert!(!sticky.session_pinned_normal(key, 31_001, 30_000));
        sticky.mark_session(key, true, 31_001);
        assert!(sticky.session_wants_bps(key, 31_001, 60_000));
    }

    #[test]
    fn previous_response_pin_off_only_clears_sticky() {
        let sticky = ChannelSticky::default();
        let key = "7:sess-b";
        sticky.mark_session(key, true, 1_000);
        // ttl = 0：只把当前会话从 BPS 粘滞里摘掉，不钉窗口。
        sticky.pin_session_normal(key, 1_000, 0);
        assert!(!sticky.session_wants_bps(key, 1_000, 60_000));
        assert!(!sticky.session_pinned_normal(key, 1_000, 0));
        assert!(!sticky.session_pinned_normal(key, 1_000, 60_000));
    }

    #[test]
    fn previous_response_id_detection_ignores_empty_and_non_string() {
        assert!(crate::bps::has_previous_response_id(
            br#"{"model":"gpt-6-astra","previous_response_id":"resp_1"}"#
        ));
        assert!(!crate::bps::has_previous_response_id(
            br#"{"model":"gpt-6-astra","previous_response_id":""}"#
        ));
        assert!(!crate::bps::has_previous_response_id(
            br#"{"model":"gpt-6-astra","previous_response_id":"   "}"#
        ));
        assert!(!crate::bps::has_previous_response_id(
            br#"{"model":"gpt-6-astra","previous_response_id":null}"#
        ));
        assert!(!crate::bps::has_previous_response_id(
            br#"{"model":"gpt-6-astra","input":[]}"#
        ));
        assert!(!crate::bps::has_previous_response_id(b"not json"));
    }
}
