//! Sub2API TransportPlugin gRPC 服务实现。

use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::config::PluginConfig;
use crate::identity;
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
pub const PLUGIN_ID: &str = "io.sub2api.codex-native-transport";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: u32 = 1;
const TRANSPORT_API_VERSION: u32 = 1;
const CAPABILITY: &str = "openai.oauth.outbound_transport.v1";

const TEST_URL: &str = "https://chatgpt.com/robots.txt";

/// 出口代理池轮转：每个 (account×model) 格独立游标 + 连续脏计数 + 本圈已换出口数。
/// 连续拿到非-292（脏/错）达阈值即游标 +1，换池里下一个代理；出 292 即清零（该格锁票、结束轮转）。
/// 一圈 = 自上次锁票/上次休息恢复以来，已经换过 pool_len 个出口仍没铸出 292——这是休息编排的
/// 唯一触发信号（「所有 IP 都试完了还不行」）。门槛来自配置 egress_advance_threshold（默认 3）。

#[derive(Default, Clone, Copy)]
struct RotorCell {
    /// 游标（模 pool_len 使用）。
    idx: usize,
    /// 连续脏计数（非 292 / 报错）。
    dirty: u32,
    /// 本圈已换过的出口数（每次游标 +1 即 +1）。
    tried: usize,
    /// 本圈是否已转满（tried ≥ pool_len）。锁到 292 或休息恢复时清零。
    exhausted: bool,
}

#[derive(Default)]
pub struct EgressRotor {
    cells: std::sync::Mutex<std::collections::HashMap<(i64, String), RotorCell>>,
}

impl EgressRotor {
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<(i64, String), RotorCell>> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 当前该格该用池里第几个（不改状态）。pool_len 必须 > 0。
    pub fn current_index(&self, account_id: i64, model: &str, pool_len: usize) -> usize {
        if pool_len == 0 {
            return 0;
        }
        self.lock()
            .get(&(account_id, model.to_string()))
            .map(|c| c.idx % pool_len)
            .unwrap_or(0)
    }

    /// 一次铸票结果：292 真票 → 清零脏计数与本圈计数（该格结束轮转）；否则累计，
    /// 达 threshold 即游标 +1（模 pool_len）换下一个代理；换满 pool_len 个
    /// 出口仍无 292 即标记本圈转满（exhausted）。游标继续取模轮转，不会停在最后一个出口上。
    /// （pub：主动养池 warm_loop 也复用同一游标推进逻辑。）
    pub fn on_result(
        &self,
        account_id: i64,
        model: &str,
        is_real6: bool,
        pool_len: usize,
        threshold: u32,
    ) {
        if pool_len == 0 {
            return;
        }
        let threshold = threshold.max(1);
        let mut cells = self.lock();
        let entry = cells
            .entry((account_id, model.to_string()))
            .or_insert_with(RotorCell::default);
        if is_real6 {
            entry.dirty = 0;
            entry.tried = 0;
            entry.exhausted = false;
            return;
        }
        entry.dirty += 1;
        if entry.dirty >= threshold {
            entry.idx = (entry.idx + 1) % pool_len;
            entry.dirty = 0;
            entry.tried = entry.tried.saturating_add(1);
            if entry.tried >= pool_len {
                entry.exhausted = true;
            }
        }
    }

    /// 该格本圈是否已把出口池转满仍无 292（休息编排的触发信号）。
    pub fn lap_exhausted(&self, account_id: i64, model: &str) -> bool {
        self.lock()
            .get(&(account_id, model.to_string()))
            .map(|c| c.exhausted)
            .unwrap_or(false)
    }

    /// 重新开一圈（休息恢复 / 放弃态到期重试时调用）：清本圈计数，游标从当前位置继续。
    pub fn reset_lap(&self, account_id: i64, model: &str) {
        if let Some(c) = self.lock().get_mut(&(account_id, model.to_string())) {
            c.tried = 0;
            c.dirty = 0;
            c.exhausted = false;
        }
    }

    pub fn retain_accounts(&self, existing: &std::collections::HashSet<i64>) -> usize {
        let mut cells = self.lock();
        let before = cells.len();
        cells.retain(|(account_id, _), _| existing.contains(account_id));
        before.saturating_sub(cells.len())
    }
}

/// 选择本次业务请求使用的代理池槽位。
/// - 未锁票：用轮转器当前槽位铸票；
/// - 已锁票且开启“账号原代理”：不使用池；
/// - 已锁票且关闭该开关：仅在票记录了铸票槽位时沿用该出口。
fn select_pool_index(
    pool_len: usize,
    mint_needed: bool,
    use_account_proxy_after_lock: bool,
    locked_egress_idx: Option<usize>,
    mint_index: usize,
) -> Option<usize> {
    if pool_len == 0 {
        return None;
    }
    if mint_needed {
        return Some(mint_index % pool_len);
    }
    if use_account_proxy_after_lock {
        return None;
    }
    locked_egress_idx.map(|idx| idx % pool_len)
}

/// 每账号养池休息编排（事件触发 + 优先级排空）。
///
/// 触发：该号在 warming_models 上的养池格**没有任何有效 292 票**，且至少一个格已把出口池
/// 转满一圈仍铸不出 292（见 EgressRotor::lap_exhausted）。持有 292、或票过期后一圈内就能
/// 重新铸出的号永远不休息。
///
/// 动作：调宿主 admin API 把该号的调度 `priority` 临时改成 `warming_drain_priority`。宿主只在
/// 给**新会话**选号时比较优先级，粘性会话命中路径完全不看它——所以老会话继续留在原号上
/// （闲置满宿主粘性 TTL 自然脱落），提示缓存不丢；只是不再有新会话进来，养池也不探铸。
/// 休息到点写回原优先级并重开一圈出口池轮转。
///
/// 持久化：休息集（{id, rest_until_ms, orig_priority}）落盘。插件重启后据此到点恢复原优先级，
/// 避免账号永远留在低优先级（优先级是宿主侧持久字段，不会自愈）。
#[derive(Default)]
pub struct AcctRest {
    inner: std::sync::Mutex<std::collections::HashMap<i64, AcctPhase>>,
}

#[derive(Clone, Copy)]
struct AcctPhase {
    /// 是否处于休息（优先级已被我们改低，欠一次恢复）。
    paused: bool,
    /// 休息截止时刻（ms）；paused 时有效。
    rest_until_ms: u64,
    /// 休息前的原优先级（恢复时写回）；paused 时有效。
    orig_priority: i64,
    /// 最近一次恢复（或首次见到）的时刻（ms），用于 warming_duty_seconds 防抖。
    last_resumed_ms: u64,
}

impl AcctRest {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<i64, AcctPhase>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 该账号当前是否处于休息（优先级已排空）——养池循环据此跳过它、转发路径据此走直通。
    pub fn is_resting(&self, account_id: i64) -> bool {
        self.lock()
            .get(&account_id)
            .map(|p| p.paused)
            .unwrap_or(false)
    }

    /// 休息中账号距恢复的剩余秒数（供面板展示）；未休息返回 None。
    pub fn rest_remaining_s(&self, account_id: i64, now_ms: u64) -> Option<u64> {
        self.lock().get(&account_id).and_then(|p| {
            if p.paused {
                Some(p.rest_until_ms.saturating_sub(now_ms) / 1000)
            } else {
                None
            }
        })
    }

    /// 休息中账号的原优先级（供面板展示）；未休息返回 None。
    pub fn orig_priority(&self, account_id: i64) -> Option<i64> {
        self.lock().get(&account_id).and_then(|p| {
            if p.paused {
                Some(p.orig_priority)
            } else {
                None
            }
        })
    }

    /// 当前所有休息中账号 (id, rest_until_ms, orig_priority)，供休息循环判到点恢复。
    pub fn paused_list(&self) -> Vec<(i64, u64, i64)> {
        self.lock()
            .iter()
            .filter(|(_, p)| p.paused)
            .map(|(id, p)| (*id, p.rest_until_ms, p.orig_priority))
            .collect()
    }

    /// 该账号现在是否允许进入休息：未在休息，且距上次恢复已满 duty_ms（防抖）。
    /// 首次见到的号记录当前时刻为起点（同样要先活跃满 duty_ms）。duty_ms=0 不设防抖。
    pub fn can_rest(&self, account_id: i64, now: u64, duty_ms: u64) -> bool {
        let mut map = self.lock();
        let phase = map.entry(account_id).or_insert(AcctPhase {
            paused: false,
            rest_until_ms: 0,
            orig_priority: 0,
            last_resumed_ms: now,
        });
        if phase.paused {
            return false;
        }
        duty_ms == 0 || now.saturating_sub(phase.last_resumed_ms) >= duty_ms
    }

    /// 标记账号已进入休息（优先级改低成功后调用），并落盘休息集。
    pub fn mark_paused(
        &self,
        account_id: i64,
        now: u64,
        rest_ms: u64,
        orig_priority: i64,
        path: &str,
    ) {
        {
            let mut map = self.lock();
            let phase = map.entry(account_id).or_insert(AcctPhase {
                paused: false,
                rest_until_ms: 0,
                orig_priority: 0,
                last_resumed_ms: now,
            });
            phase.paused = true;
            phase.rest_until_ms = now + rest_ms;
            phase.orig_priority = orig_priority;
        }
        self.save(path);
    }

    /// 标记账号已恢复（原优先级写回成功后调用），重置防抖起点并落盘。
    pub fn mark_resumed(&self, account_id: i64, now: u64, path: &str) {
        {
            let mut map = self.lock();
            let phase = map.entry(account_id).or_insert(AcctPhase {
                paused: false,
                rest_until_ms: 0,
                orig_priority: 0,
                last_resumed_ms: now,
            });
            phase.paused = false;
            phase.rest_until_ms = 0;
            phase.last_resumed_ms = now;
        }
        self.save(path);
    }

    /// 删除宿主已不存在账号的休息/防抖记录，并立即重写休息快照。
    pub fn retain_accounts(&self, existing: &std::collections::HashSet<i64>, path: &str) -> usize {
        let removed = {
            let mut map = self.lock();
            let before = map.len();
            map.retain(|account_id, _| existing.contains(account_id));
            before.saturating_sub(map.len())
        };
        if removed > 0 {
            self.save(path);
        }
        removed
    }

    /// 从盘载入休息集（插件启动时调用一次）。path 为空则跳过。
    pub fn load(&self, path: &str) {
        if path.trim().is_empty() {
            return;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(arr) = serde_json::from_slice::<Vec<PausedRecord>>(&bytes) else {
            return;
        };
        let mut map = self.lock();
        for rec in arr {
            // 旧格式（无 orig_priority，schedulable 摘号时代）的记录没有可恢复的优先级：
            // 跳过——那类号由宿主/面板手动恢复 schedulable，与优先级排空无关。
            let Some(orig) = rec.orig_priority else {
                continue;
            };
            map.insert(
                rec.id,
                AcctPhase {
                    paused: true,
                    rest_until_ms: rec.rest_until_ms,
                    orig_priority: orig,
                    last_resumed_ms: 0,
                },
            );
        }
    }

    /// 落盘当前休息集（best-effort）。path 为空则跳过。
    fn save(&self, path: &str) {
        if path.trim().is_empty() {
            return;
        }
        let arr: Vec<PausedRecord> = self
            .lock()
            .iter()
            .filter(|(_, p)| p.paused)
            .map(|(id, p)| PausedRecord {
                id: *id,
                rest_until_ms: p.rest_until_ms,
                orig_priority: Some(p.orig_priority),
            })
            .collect();
        if let Ok(bytes) = serde_json::to_vec(&arr) {
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, bytes);
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PausedRecord {
    id: i64,
    rest_until_ms: u64,
    /// 休息前原优先级；旧版本落盘的记录没有该字段。
    #[serde(default)]
    orig_priority: Option<i64>,
}

pub struct SharedState {
    pub config: RwLock<PluginConfig>,
    pub clients: ClientCache,
    pub version: VersionCache,
    pub pool: crate::turn_state::TurnStatePool,
    pub creds: crate::refresh::CredCache,
    pub probe_ids: crate::refresh::ProbeIdentityCache,
    pub proxy_api: crate::proxy_api::ProxyApiClient,
    pub egress: Arc<EgressRotor>,
    pub acct_rest: AcctRest,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(PluginConfig::default()),
            clients: ClientCache::default(),
            version: VersionCache::default(),
            pool: crate::turn_state::TurnStatePool::new(),
            creds: crate::refresh::CredCache::new(),
            probe_ids: crate::refresh::ProbeIdentityCache::default(),
            proxy_api: crate::proxy_api::ProxyApiClient::default(),
            egress: Arc::new(EgressRotor::default()),
            acct_rest: AcctRest::default(),
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
                // pin 池启动载入（只执行一次）。
                {
                    let config = self.state.current_config();
                    let params = crate::turn_state::PinParams::from_config(
                        config.pin_max_age_seconds,
                        config.pin_fail_threshold,
                        config.pin_fail_ratio_pct,
                    );
                    self.state.pool.load_once(&config.pin_persist_path, &params);
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

        let began = Instant::now();
        let result = if config.egress_proxy_api_enabled {
            // 配置测试必须验证代理 API 本身，不能因“失败回退”而把直连成功误报成 API 成功。
            let mut test_config = config.clone();
            test_config.egress_proxy_api_fallback_to_account_proxy = false;
            crate::proxy_api::send_with_optional_proxy_api(
                &self.state.proxy_api,
                &self.state.clients,
                &test_config,
                0,
                true,
                "",
                "",
                0,
                &reqwest::Method::GET,
                TEST_URL,
                &reqwest::header::HeaderMap::new(),
                &[],
            )
            .await
            .map(|sent| sent.response)
            .map_err(|err| match err {
                crate::proxy_api::ProxySendError::Api(message)
                | crate::proxy_api::ProxySendError::ClientBuild(message) => message,
                crate::proxy_api::ProxySendError::Upstream(err) => {
                    transport::full_error_chain(&err)
                }
            })
        } else {
            match self.state.clients.client_for(&config, 0, "") {
                Ok(client) => client
                    .get(TEST_URL)
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await
                    .map_err(|err| transport::full_error_chain(&err)),
                Err(message) => Err(message),
            }
        };
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
            Err(message) => Ok(Response::new(TestConfigResponse {
                success: false,
                message,
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

/// 取出站 HeaderMap 里某个头的首个值（用于 turn-state 诊断，只读）。
fn header_first(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// 在宿主给的原始头 map（大小写不定）里大小写无关地取首个值。
fn raw_header_first(
    raw: &std::collections::HashMap<String, HeaderValues>,
    name: &str,
) -> Option<String> {
    raw.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, values)| values.values.first().cloned())
}

/// 实验注入头：携带一个已捕获的 turn-state，让插件在所有身份处理之后
/// 强行把它写回 x-codex-turn-state（用于实证方案乙：pin turn-state + 换 session）。
/// 双重门控：仅当 turn_state_diag=on 且请求带该头时生效；该头本身绝不发往上游。
const EXPERIMENT_PIN_HEADER: &str = "x-cnt-pin-turn-state";

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

    // 2. codex 后端判定 / 出站模型 / pin 参数（用于铸票出口路由与后续注入）。
    let codex_backend = identity::is_codex_backend_request(&start.url);
    let pin_params = crate::turn_state::PinParams::from_config(
        config.pin_max_age_seconds,
        config.pin_fail_threshold,
        config.pin_fail_ratio_pct,
    );
    let ts_mode = config.turn_state_mode.as_str();
    let outbound_model = if codex_backend {
        crate::turn_state::parse_model(&body)
    } else {
        None
    };
    // pin 只对 warming_models 里的模型生效；其它模型（gpt-5.2、codex-auto-review…）即使在
    // pin 模式下也按 passthrough 处理：不走出口池、不换身份、不注入、不建格、不扫 SSE。
    let pin_active = ts_mode == "pin"
        && codex_backend
        && outbound_model
            .as_deref()
            .is_some_and(|model| config.is_warming_model(model));

    // 2.0 直通兜底：账号在休息（优先级排空中）、或该格处于格级休息/放弃态时，真实流量不再
    // 承担铸票——走宿主原代理、保留客户端会话身份、不换身份、不注入（等同 passthrough）。
    // 格级判定让「同一账号 astra 有 292、terra 只出 312」时只有 terra 直通，astra 照常注入。
    let parked = pin_active
        && (state.acct_rest.is_resting(start.account_id)
            || outbound_model.as_deref().is_some_and(|model| {
                state
                    .pool
                    .is_parked(start.account_id, model, crate::turn_state::now_ms() as u64)
            }));

    // 2.1 选择出站代理。未锁票时由代理池轮转铸票；已锁票后由配置决定回到账号原代理，
    // 或继续使用铸出该票的代理池出口。旧版本票没有记录槽位时安全回退到账号原代理。
    let locked_ticket = if pin_active && !parked {
        outbound_model.as_deref().and_then(|model| {
            state
                .pool
                .injectable_with_egress(start.account_id, model, &pin_params)
        })
    } else {
        None
    };
    let mint_needed = pin_active && !parked && locked_ticket.is_none();
    let use_proxy_api = mint_needed && config.egress_proxy_api_enabled;
    let egress_pool = config.effective_egress_pool_list();
    let egress_lap_len = config.egress_lap_len();
    let egress_threshold = config.egress_threshold_effective();
    let mint_index = if !egress_pool.is_empty() {
        outbound_model
            .as_deref()
            .map(|model| {
                state
                    .egress
                    .current_index(start.account_id, model, egress_pool.len())
            })
            .unwrap_or(0)
    } else {
        0
    };
    let locked_egress_idx = locked_ticket.as_ref().and_then(|(_, idx)| *idx);
    let selected_pool_idx = if pin_active && !parked && outbound_model.is_some() {
        select_pool_index(
            egress_pool.len(),
            mint_needed,
            config.use_account_proxy_after_lock,
            locked_egress_idx,
            mint_index,
        )
    } else {
        None
    };
    let use_pool = selected_pool_idx.is_some();
    let mint_via_pool = mint_needed && use_pool;
    // 出口池行号（1 起）：每行独立 client / 独立连接，同一网关填多行就是多个出口。
    let mut pool_slot: usize = 0;
    let effective_proxy: String = if let Some(idx) = selected_pool_idx {
        pool_slot = idx + 1;
        egress_pool[idx].clone()
    } else {
        start.proxy_url.clone()
    };
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
    let mut headers = transport::ordered_headers(&start.headers, codex_backend);
    // 采集 canary 模板（忠实快照：真实 URL/头/代理/体，含 bearer，仅内存不落盘）。
    if let Some(model) = outbound_model.as_deref() {
        state.creds.record(
            start.account_id,
            model,
            &start.url,
            &start.proxy_url,
            &start.headers,
            &body,
        );
    }
    let mut pin_injected = false;

    // 3.1 身份 Profile 边缘改写（仅 ChatGPT Codex 内部接口请求）。
    if codex_backend {
        if let Some(resolved) =
            identity::resolve_identity(&config.identity, &state.effective_version(&config))
        {
            identity::apply_identity_headers(&mut headers, &resolved, &config.identity.residency);
        }
        if pin_active {
            // pin：命中同后端以抗降智。休息/放弃态下不换身份（保客户端会话与提示缓存）。
            if config.pin_identity_strategy == "rotate" && !parked {
                // 方案乙（默认，已实测服务端接受）：换新 session（顺带剥掉旧 turn-state），
                // 随后强行注入池里钉住的 turn-state。
                let ids = identity::RequestIds::mint();
                identity::apply_one_id_headers(&mut headers, &ids);
                if let Some(rewritten) = identity::apply_one_id_body(&body, &ids) {
                    body = rewritten;
                }
            } else {
                // pinned 保持下游会话边界与缓存键；machine 只做稳定 1:1 假名化。
                identity::apply_stable_request_identity(
                    &config.identity,
                    config.per_account(),
                    start.account_id,
                    &mut headers,
                    &mut body,
                );
            }
            // 直通兜底（休息/放弃）下**不注入**：此时保留的是客户端真实会话身份，再贴一张别的
            // 会话铸出的票就是 0.6.20 那种跨用户串会话；池里的票留到恢复后配合换身份再用。
            if !parked {
                if let Some((ts, _)) = locked_ticket.as_ref() {
                    if let Ok(value) = reqwest::header::HeaderValue::from_str(ts) {
                        headers.insert("x-codex-turn-state", value);
                        pin_injected = true;
                    }
                }
            }
        } else {
            // passthrough（默认）/ strip / pin 模式下的非关注模型。
            if ts_mode == "strip" {
                headers.remove("x-codex-turn-state");
            }
            identity::apply_stable_request_identity(
                &config.identity,
                config.per_account(),
                start.account_id,
                &mut headers,
                &mut body,
            );
        }
    }

    // turn-state 诊断 + 方案乙实验注入。
    let diag_on = config.turn_state_diag && codex_backend;
    // 入站（宿主交给插件时）就带的 turn-state 长度——对比出站可看出是否被剥。
    let diag_in_ts_len = raw_header_first(&start.headers, "x-codex-turn-state")
        .as_deref()
        .map(str::len)
        .unwrap_or(0);
    // 实验注入（双重门控：diag on + 带 x-cnt-pin-turn-state 头）：把已捕获的
    // turn-state 强行写回 x-codex-turn-state，覆盖会话轮换时的剥离；实验头本身不发往上游。
    let mut pinned_injected = false;
    if diag_on {
        // 优先客户端实验头（若宿主放行）；否则读服务器端文件（宿主剥头时用）。
        if let Some(pin) = raw_header_first(&start.headers, EXPERIMENT_PIN_HEADER) {
            headers.remove(EXPERIMENT_PIN_HEADER);
            let pin = pin.trim();
            if !pin.is_empty() {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(pin) {
                    headers.insert("x-codex-turn-state", value);
                    pinned_injected = true;
                }
            }
        }
        if !pinned_injected {
            if let Some((account_id, pin)) =
                crate::turn_state::read_pin_inject(&config.turn_state_diag_path)
            {
                // 仅对目标账号注入，避免影响其它流量。
                if account_id == start.account_id {
                    if let Ok(value) = reqwest::header::HeaderValue::from_str(&pin) {
                        headers.insert("x-codex-turn-state", value);
                        pinned_injected = true;
                    }
                }
            }
        }
    }
    // body / headers 稍后被移动进 request，这里先取本次出站的 model / session / turn-state。
    let (diag_model, diag_req_session, diag_req_ts) = if diag_on {
        (
            crate::turn_state::parse_model(&body),
            header_first(&headers, "session-id").or_else(|| header_first(&headers, "session_id")),
            header_first(&headers, "x-codex-turn-state"),
        )
    } else {
        (None, None, None)
    };

    let began = Instant::now();
    let sent = crate::proxy_api::send_with_optional_proxy_api(
        &state.proxy_api,
        &state.clients,
        &config,
        start.account_id,
        use_proxy_api,
        &start.proxy_url,
        &effective_proxy,
        pool_slot,
        &method,
        &start.url,
        &headers,
        &body,
    )
    .await;
    let sent = match sent {
        Ok(sent) => sent,
        Err(crate::proxy_api::ProxySendError::Upstream(err)) => {
            // 走代理池铸票时连不上/发送失败：按一次"脏"计入该格，触发游标推进，绕开死代理。
            if mint_via_pool {
                if let Some(model) = outbound_model.as_deref() {
                    state.egress.on_result(
                        start.account_id,
                        model,
                        false,
                        egress_lap_len,
                        egress_threshold,
                    );
                }
            }
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
        Err(crate::proxy_api::ProxySendError::Api(message)) => {
            let _ = tx
                .send(Ok(error_frame("PLUGIN_PROXY_API", message, false)))
                .await;
            return;
        }
        Err(crate::proxy_api::ProxySendError::ClientBuild(message)) => {
            let _ = tx
                .send(Ok(error_frame("PLUGIN_CLIENT_BUILD", message, false)))
                .await;
            return;
        }
    };
    let response = sent.response;
    // API 回退到账号原代理时不把账号代理标成“专用铸票出口”。
    let diag_egress = (use_pool || sent.used_api).then(|| transport::proxy_host(&sent.proxy_url));

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

    // 上游响应里的 turn-state（diag 与 pin 捕获共用）。
    let resp_turn_state = header_map
        .get("x-codex-turn-state")
        .and_then(|h| h.values.first().cloned());
    if pin_active {
        if let Some(model) = outbound_model.as_deref() {
            // 本次注入了钉票 + 服务端回带了新 turn-state = 我们注入的那张“票费了”（被换发）。
            // 这是比硬 TTL 精确的过期信号：丢弃死票，换发若是 292 就地重锁。正常复用有效票时
            // 上游不回 turn-state，此调用是 no-op，锁票照常黏着到过期上限。
            if pin_injected {
                state.pool.note_injected_reissue_with_egress(
                    start.account_id,
                    model,
                    resp_turn_state.as_deref(),
                    selected_pool_idx,
                    &pin_params,
                );
            }
            // 被动养池：该格尚无钉住值（空/过期/刚被换发清掉）时，捕获响应里的 292 真6票锁进池。
            if config.passive_warming_enabled {
                state.pool.capture_if_empty_with_egress(
                    start.account_id,
                    model,
                    resp_turn_state.as_deref(),
                    selected_pool_idx,
                    &pin_params,
                );
            }
        }
    }
    // 代理池自动轮转：本次走代理池铸票——拿到 292 真票则清零该格脏计数（该格结束轮转）；
    // 否则（假票/短票/无票）累计，连续达阈值即该格游标 +1，换池里下一个代理。
    if mint_via_pool {
        if let Some(model) = outbound_model.as_deref() {
            let is_real6 = resp_turn_state
                .as_deref()
                .is_some_and(|ts| crate::turn_state::is_lockable_ticket_len(ts.len()));
            state.egress.on_result(
                start.account_id,
                model,
                is_real6,
                egress_lap_len,
                egress_threshold,
            );
        }
    }

    // turn-state 诊断：观测上游是否在响应头里铸/换发 turn-state，以及服务端实际模型。
    if diag_on {
        let resp_ts = resp_turn_state.clone();
        let resp_ts_len = resp_ts.as_deref().map(str::len).unwrap_or(0);
        let resp_model = header_map
            .get("openai-model")
            .or_else(|| header_map.get("x-openai-model"))
            .and_then(|h| h.values.first().cloned());
        let resp_turn_state_changed = match (&resp_ts, &diag_req_ts) {
            (Some(r), Some(q)) => r != q,
            (Some(_), None) => true,
            _ => false,
        };
        crate::turn_state::append_observation(
            &config.turn_state_diag_path,
            &crate::turn_state::Observation {
                ts_ms: crate::turn_state::now_ms(),
                account_id: start.account_id,
                outbound_model: diag_model.clone(),
                status_code,
                req_session_id: diag_req_session.clone(),
                req_in_turn_state_len: diag_in_ts_len,
                req_turn_state_prefix: diag_req_ts
                    .as_deref()
                    .map(|v| crate::turn_state::prefix(v, 8)),
                req_turn_state_len: diag_req_ts.as_deref().map(str::len).unwrap_or(0),
                pinned_injected: pinned_injected || pin_injected,
                resp_turn_state: resp_ts,
                resp_turn_state_len: resp_ts_len,
                resp_openai_model: resp_model,
                resp_turn_state_changed,
                path: "forward".to_string(),
                egress: diag_egress.clone(),
            },
        );
    }

    if tx
        .send(Ok(ForwardResponse {
            frame: Some(forward_response::Frame::Start(ForwardResponseStart {
                status_code,
                status: status_line,
                protocol: protocol.to_string(),
                protocol_major,
                protocol_minor,
                headers: header_map,
                content_length,
            })),
        }))
        .await
        .is_err()
    {
        return;
    }

    // 5. 流式转发响应体（SSE 逐块低延迟回传）。
    // pin 模式下顺带扫描 SSE 体判定本次结果（完成 / overload / 其它错误），驱动养池状态机。
    let pin_track = pin_active && outbound_model.is_some();
    let mut scan = SseOutcomeScan::default();
    let mut stream = response.bytes_stream();
    let mut bytes_received: i64 = 0;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                bytes_received += bytes.len() as i64;
                if pin_track {
                    scan.feed(&bytes);
                }
                if tx
                    .send(Ok(ForwardResponse {
                        frame: Some(forward_response::Frame::BodyChunk(bytes.to_vec())),
                    }))
                    .await
                    .is_err()
                {
                    // 宿主已放弃读取（下游断开），停止拉取上游。
                    return;
                }
            }
            Err(err) => {
                if pin_track {
                    if let Some(model) = outbound_model.as_deref() {
                        state.pool.record_outcome(
                            start.account_id,
                            model,
                            crate::turn_state::Outcome::OtherError,
                            0.0,
                            &pin_params,
                        );
                    }
                }
                let _ = tx
                    .send(Ok(error_frame(
                        "PLUGIN_UPSTREAM_READ",
                        transport::full_error_chain(&err),
                        true,
                    )))
                    .await;
                return;
            }
        }
    }

    if pin_track {
        if let Some(model) = outbound_model.as_deref() {
            let tps = scan.tps();
            // 降智判定唯一标准：本次响应是否拿到 292 真6票。
            let resp_is_real6 = resp_turn_state
                .as_deref()
                .is_some_and(|ts| crate::turn_state::is_lockable_ticket_len(ts.len()));
            state.pool.record_outcome(
                start.account_id,
                model,
                scan.outcome(resp_is_real6),
                tps,
                &pin_params,
            );
            state.pool.persist_throttled(&config.pin_persist_path);
        }
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

/// 扫描 SSE 体：仅用于估算 TPS（面板统计）。降智判定不再看流内容，唯一标准是 292 票长。
#[derive(Default)]
struct SseOutcomeScan {
    completed: bool,
    /// 滚动尾缓冲（最多 64KB）：completed 事件在流末尾，含完整 usage，供提取 output_tokens 估 TPS。
    buf: Vec<u8>,
    /// 首/末字节到达时刻（ms），用于估算 TPS。
    first_ms: u64,
    last_ms: u64,
}

impl SseOutcomeScan {
    fn feed(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let now = crate::turn_state::now_ms() as u64;
        if self.first_ms == 0 {
            self.first_ms = now;
        }
        self.last_ms = now;
        self.buf.extend_from_slice(chunk);
        // 标记为 sticky：一旦命中就保留（在追加后、裁剪前搜索，避免漏掉边界标记）。用于 TPS 估算。
        if find_bytes(&self.buf, b"response.completed") {
            self.completed = true;
        }
        // 只保留尾部 64KB（completed 事件在末尾）。
        const KEEP: usize = 64 * 1024;
        if self.buf.len() > KEEP {
            let cut = self.buf.len() - KEEP;
            self.buf.drain(0..cut);
        }
    }

    /// 从缓冲末尾提取 output_tokens（取最后一次出现）。
    fn output_tokens(&self) -> u64 {
        let hay = String::from_utf8_lossy(&self.buf);
        last_json_uint(&hay, "\"output_tokens\"")
    }

    /// 估算 TPS = output_tokens / 首末字节间隔秒。无法估算时返回 0。
    fn tps(&self) -> f64 {
        if !self.completed {
            return 0.0;
        }
        let tokens = self.output_tokens();
        let secs = self.last_ms.saturating_sub(self.first_ms) as f64 / 1000.0;
        if tokens == 0 || secs <= 0.0 {
            return 0.0;
        }
        tokens as f64 / secs
    }

    /// 判定唯一标准：拿到 292 真6票 = 不降智(成功)；否则一律判降智。
    /// TPS / reasoning / completed / overload 都不再参与判定——票长 292 是唯一权威信号。
    fn outcome(&self, is_real6: bool) -> crate::turn_state::Outcome {
        use crate::turn_state::Outcome;
        if is_real6 {
            Outcome::Success
        } else {
            Outcome::Degraded
        }
    }
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|window| window == needle)
}

/// 取字符串里最后一次 `"key": N` 的无符号整数值。
fn last_json_uint(hay: &str, key: &str) -> u64 {
    let mut result = 0u64;
    let mut from = 0usize;
    while let Some(pos) = hay[from..].find(key) {
        let start = from + pos + key.len();
        let digits: String = hay[start..]
            .chars()
            .skip_while(|c| *c == ':' || c.is_whitespace())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse::<u64>() {
            result = n;
        }
        from = start;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acct_rest_pause_resume_cycle() {
        let rest = AcctRest::default();
        let duty = 1000u64;
        let now = 10_000_000u64;
        // 首见：以 now 为活跃起点，duty 内不允许休息；duty=0 立即允许。
        assert!(!rest.can_rest(100, now, duty));
        assert!(rest.can_rest(100, now + duty, duty));
        assert!(rest.can_rest(101, now, 0));
        assert!(!rest.is_resting(100));
        // 进入休息 500ms，原优先级 10。
        rest.mark_paused(100, now + duty, 500, 10, "");
        assert!(rest.is_resting(100));
        assert_eq!(rest.orig_priority(100), Some(10));
        assert_eq!(rest.paused_list(), vec![(100, now + duty + 500, 10)]);
        assert_eq!(rest.rest_remaining_s(100, now + duty + 100), Some(0));
        // 休息中不允许再次休息。
        assert!(!rest.can_rest(100, now + 10 * duty, duty));
        // 恢复：重置防抖起点。
        let t = now + duty + 400;
        rest.mark_resumed(100, t, "");
        assert!(!rest.is_resting(100));
        assert_eq!(rest.orig_priority(100), None);
        assert!(rest.paused_list().is_empty());
        assert!(!rest.can_rest(100, t + duty - 1, duty));
        assert!(rest.can_rest(100, t + duty, duty));
    }

    #[test]
    fn acct_rest_persist_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir
            .join(format!("cnt-rest-test-{}.json", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&path);
        let a = AcctRest::default();
        a.mark_paused(7, 5_000, 3_000, 10, &path); // rest_until=8000
        a.mark_paused(9, 5_000, 3_000, 20, &path);
        // 新实例从盘载入：两个号仍处休息，原优先级可恢复。
        let b = AcctRest::default();
        b.load(&path);
        assert!(b.is_resting(7));
        assert!(b.is_resting(9));
        let mut got = b.paused_list();
        got.sort();
        assert_eq!(got, vec![(7, 8_000, 10), (9, 8_000, 20)]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn acct_rest_load_skips_legacy_records_without_priority() {
        let dir = std::env::temp_dir();
        let path = dir
            .join(format!("cnt-rest-legacy-{}.json", std::process::id()))
            .to_string_lossy()
            .into_owned();
        // 旧版本（schedulable 摘号）落盘格式：没有 orig_priority。
        std::fs::write(&path, br#"[{"id":7,"rest_until_ms":8000}]"#).unwrap();
        let b = AcctRest::default();
        b.load(&path);
        assert!(!b.is_resting(7));
        assert!(b.paused_list().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn egress_rotor_marks_lap_exhausted_after_full_cycle() {
        let rotor = EgressRotor::default();
        let pool_len = 2usize;
        // 每 3 次脏结果换一个出口；换满 2 个出口即本圈转满。
        for _ in 0..3 {
            rotor.on_result(5, "m", false, pool_len, 3);
        }
        assert_eq!(rotor.current_index(5, "m", pool_len), 1);
        assert!(!rotor.lap_exhausted(5, "m"));
        for _ in 0..3 {
            rotor.on_result(5, "m", false, pool_len, 3);
        }
        assert_eq!(rotor.current_index(5, "m", pool_len), 0); // 游标取模继续轮转
        assert!(rotor.lap_exhausted(5, "m"));
        // 重开一圈：清转满标记，游标位置保留。
        rotor.reset_lap(5, "m");
        assert!(!rotor.lap_exhausted(5, "m"));
        assert_eq!(rotor.current_index(5, "m", pool_len), 0);
        // 292 真票也清转满标记。
        for _ in 0..6 {
            rotor.on_result(5, "m", false, pool_len, 3);
        }
        assert!(rotor.lap_exhausted(5, "m"));
        rotor.on_result(5, "m", true, pool_len, 3);
        assert!(!rotor.lap_exhausted(5, "m"));
        // 未知格：未转满。
        assert!(!rotor.lap_exhausted(6, "m"));
    }

    #[test]
    fn egress_rotor_advances_after_threshold_and_wraps() {
        let rotor = EgressRotor::default();
        let pool_len = 3;
        // 初始游标 0。
        assert_eq!(rotor.current_index(20, "m", pool_len), 0);
        // 连续脏未达阈值：不换。
        rotor.on_result(20, "m", false, pool_len, 3);
        rotor.on_result(20, "m", false, pool_len, 3);
        assert_eq!(rotor.current_index(20, "m", pool_len), 0);
        // 第 3 次脏达阈值：游标 +1。
        rotor.on_result(20, "m", false, pool_len, 3);
        assert_eq!(rotor.current_index(20, "m", pool_len), 1);
        // 再连续 3 次脏：到 2。
        for _ in 0..3 {
            rotor.on_result(20, "m", false, pool_len, 3);
        }
        assert_eq!(rotor.current_index(20, "m", pool_len), 2);
        // 再 3 次：回绕到 0。
        for _ in 0..3 {
            rotor.on_result(20, "m", false, pool_len, 3);
        }
        assert_eq!(rotor.current_index(20, "m", pool_len), 0);
    }

    #[test]
    fn egress_rotor_real6_resets_dirty_and_holds_index() {
        let rotor = EgressRotor::default();
        let pool_len = 4;
        // 攒 2 次脏后来一张 292：脏清零，游标不动。
        rotor.on_result(7, "m", false, pool_len, 3);
        rotor.on_result(7, "m", false, pool_len, 3);
        rotor.on_result(7, "m", true, pool_len, 3);
        assert_eq!(rotor.current_index(7, "m", pool_len), 0);
        // 清零后再来 2 次脏也不换（说明计数确实归零）。
        rotor.on_result(7, "m", false, pool_len, 3);
        rotor.on_result(7, "m", false, pool_len, 3);
        assert_eq!(rotor.current_index(7, "m", pool_len), 0);
    }

    #[test]
    fn egress_rotor_isolated_per_cell() {
        let rotor = EgressRotor::default();
        let pool_len = 2;
        // A 格换到 1，B 格不受影响。
        for _ in 0..3u32 {
            rotor.on_result(1, "a", false, pool_len, 3);
        }
        assert_eq!(rotor.current_index(1, "a", pool_len), 1);
        assert_eq!(rotor.current_index(1, "b", pool_len), 0);
        assert_eq!(rotor.current_index(2, "a", pool_len), 0);
    }

    #[test]
    fn egress_rotor_pool_len_zero_is_safe() {
        let rotor = EgressRotor::default();
        rotor.on_result(1, "a", false, 0, 3);
        assert_eq!(rotor.current_index(1, "a", 0), 0);
    }

    #[test]
    fn proxy_selection_uses_account_proxy_after_lock_when_enabled() {
        assert_eq!(select_pool_index(10, false, true, Some(7), 3), None);
    }

    #[test]
    fn proxy_selection_keeps_minting_exit_after_lock_when_disabled() {
        assert_eq!(select_pool_index(10, false, false, Some(7), 3), Some(7));
        // 代理池缩短时仍安全取模。
        assert_eq!(select_pool_index(3, false, false, Some(7), 1), Some(1));
    }

    #[test]
    fn proxy_selection_uses_rotor_while_minting_and_falls_back_for_legacy_ticket() {
        assert_eq!(select_pool_index(10, true, true, None, 6), Some(6));
        assert_eq!(select_pool_index(10, true, false, None, 6), Some(6));
        // 旧版本锁票没有铸票槽位，不能猜一个出口，回到账号原代理。
        assert_eq!(select_pool_index(10, false, false, None, 6), None);
        assert_eq!(select_pool_index(0, true, false, Some(2), 6), None);
    }
}
