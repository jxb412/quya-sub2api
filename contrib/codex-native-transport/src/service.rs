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
pub const PLUGIN_ID: &str = "io.quya.codex-native-transport";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: u32 = 1;
const TRANSPORT_API_VERSION: u32 = 1;
const CAPABILITY: &str = "openai.oauth.outbound_transport.v1";

const TEST_URL: &str = "https://chatgpt.com/robots.txt";

pub struct SharedState {
    pub config: RwLock<PluginConfig>,
    pub clients: ClientCache,
    pub version: VersionCache,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(PluginConfig::default()),
            clients: ClientCache::default(),
            version: VersionCache::default(),
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

    let began = Instant::now();
    let request = client
        .request(method, &start.url)
        .headers(headers)
        .body(body);

    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
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
    };

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
                content_length,
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
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                bytes_received += bytes.len() as i64;
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

    let _ = tx
        .send(Ok(ForwardResponse {
            frame: Some(forward_response::Frame::End(ForwardResponseEnd {
                bytes_received,
                duration_ms: began.elapsed().as_millis() as i64,
            })),
        }))
        .await;
}
