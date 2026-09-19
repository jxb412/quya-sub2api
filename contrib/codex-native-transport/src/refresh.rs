//! 主动刷新 / canary：把每账号×模型最近一次真实出站请求当模板（含 bearer，仅内存，绝不落盘），
//! 定时用它对上游发一道受控推理题，铸取新 turn-state 并判静默降智，驱动养池状态机。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use futures_util::StreamExt;

use crate::proto::sub2api::plugin::v1::HeaderValues;
use crate::service::SharedState;
use crate::turn_state::{is_degraded_from_sse, now_ms, Outcome, PinParams};

/// 判定 canary 结果的推理题（要求出思维链；正常号会产生 reasoning，降智号不会）。
const CANARY_QUESTION: &str = "Three friends split a bill. Alice pays 40% of the total. Bob pays $12 less than Alice. Carol pays the remaining $18. Work out the total bill step by step, then give the final number.";

/// 养池并发上限：单轮同时进行的探铸数。控制对上游/出口池的并发压力，
/// 同时把「串行一圈要一分多钟」压缩到秒级，显著提高到期后重铸的及时性。
const WARM_CONCURRENCY: usize = 8;

/// 一次真实出站的忠实快照（复用它构造 canary，保证头/URL/代理与真实链路一致）。
/// 含 authorization bearer —— **仅内存，绝不持久化**。
#[derive(Clone)]
pub struct Template {
    pub url: String,
    pub proxy_url: String,
    pub raw_headers: HashMap<String, HeaderValues>,
    pub body: Vec<u8>,
    pub updated_ms: u64,
}

/// 每账号×模型的请求模板缓存（仅内存）。
#[derive(Default)]
pub struct CredCache {
    templates: Mutex<HashMap<(i64, String), Template>>,
}

#[derive(Clone)]
struct ProbeConversation {
    conversation_id: String,
    installation_id: String,
    context_window_id: String,
}

/// 后台探测使用独立、稳定的缓存域：同一账号、模型、用途和出口复用
/// conversation/installation/window，只刷新 turn_id，避免每轮探测制造新的缓存前缀。
#[derive(Default)]
pub struct ProbeIdentityCache {
    identities: Mutex<HashMap<(i64, String, String, usize), ProbeConversation>>,
}

impl ProbeIdentityCache {
    pub fn request_ids(
        &self,
        account_id: i64,
        model: &str,
        purpose: &str,
        pool_slot: usize,
    ) -> crate::identity::RequestIds {
        let key = (
            account_id,
            model.to_string(),
            purpose.to_string(),
            pool_slot,
        );
        let mut identities = self
            .identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let conversation = identities.entry(key).or_insert_with(|| {
            let minted = crate::identity::RequestIds::mint();
            ProbeConversation {
                conversation_id: minted.conversation_id,
                installation_id: minted.installation_id,
                context_window_id: minted.context_window_id,
            }
        });
        let fresh_turn = crate::identity::RequestIds::mint();
        crate::identity::RequestIds {
            conversation_id: conversation.conversation_id.clone(),
            installation_id: conversation.installation_id.clone(),
            turn_id: fresh_turn.turn_id,
            context_window_id: conversation.context_window_id.clone(),
        }
    }
}

impl CredCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(i64, String), Template>> {
        self.templates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 记录一次真实出站（仅 codex 后端、带模型时）。覆盖旧模板以保持 bearer 新鲜。
    pub fn record(
        &self,
        account_id: i64,
        model: &str,
        url: &str,
        proxy_url: &str,
        raw_headers: &HashMap<String, HeaderValues>,
        body: &[u8],
    ) {
        let mut guard = self.lock();
        guard.insert(
            (account_id, model.to_string()),
            Template {
                url: url.to_string(),
                proxy_url: proxy_url.to_string(),
                raw_headers: raw_headers.clone(),
                body: body.to_vec(),
                updated_ms: now_ms() as u64,
            },
        );
    }

    pub fn get(&self, account_id: i64, model: &str) -> Option<Template> {
        self.lock().get(&(account_id, model.to_string())).cloned()
    }

    /// 当前已知的所有 (account, model) 键（供 canary 循环遍历）。
    pub fn keys(&self) -> Vec<(i64, String)> {
        self.lock().keys().cloned().collect()
    }

    /// 任取一条最近更新的真实模板，作为 admin 全池养池的"形状 donor"。
    /// bearer 会被目标号覆写，这里只借 url/头/body 的形状。
    pub fn any_recent(&self) -> Option<Template> {
        self.lock().values().max_by_key(|t| t.updated_ms).cloned()
    }
}

/// canary 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryReport {
    /// 无模板（该账号还没真实流量过）。
    NoTemplate,
    /// 正常：铸到新 turn-state，未降智。
    Healthy,
    /// 上游 overload。
    Overload,
    /// 静默降智（reasoning 缺失）。
    Degraded,
    /// 其它错误（含 4xx/5xx/网络错误/bearer 过期）。
    Error,
}

/// 用模板 body 构造 canary body：替换 input 为推理题、effort=high、stream、不落存储。
fn build_canary_body(template_body: &[u8]) -> Vec<u8> {
    let fallback = || {
        format!(
            r#"{{"model":"gpt-5.4","instructions":"You are a careful reasoning assistant.","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":{}}}]}}],"stream":true,"store":false,"reasoning":{{"effort":"high"}}}}"#,
            serde_json::to_string(CANARY_QUESTION).unwrap()
        )
        .into_bytes()
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(template_body) else {
        return fallback();
    };
    let Some(obj) = value.as_object_mut() else {
        return fallback();
    };
    obj.insert(
        "input".to_string(),
        serde_json::json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": CANARY_QUESTION}]
        }]),
    );
    obj.insert(
        "reasoning".to_string(),
        serde_json::json!({"effort": "high"}),
    );
    obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    obj.insert("store".to_string(), serde_json::Value::Bool(false));
    // Canary is an independent probe; never continue a member's response chain.
    obj.remove("previous_response_id");
    serde_json::to_vec(&value).unwrap_or_else(|_| fallback())
}

/// 对 (account, model) 跑一次 canary，并据结果更新养池。
pub async fn run_canary(state: &Arc<SharedState>, account_id: i64, model: &str) -> CanaryReport {
    let Some(template) = state.creds.get(account_id, model) else {
        return CanaryReport::NoTemplate;
    };
    let config = state.current_config();
    let params = PinParams::from_config(
        config.pin_max_age_seconds,
        config.pin_fail_threshold,
        config.pin_fail_ratio_pct,
    );

    let client = match state
        .clients
        .client_for(&config, account_id, &template.proxy_url)
    {
        Ok(client) => client,
        Err(_) => return CanaryReport::Error,
    };

    // 忠实复用真实头 + 身份改写 + 每次换新 session（铸全新 turn-state）。
    let mut headers = crate::transport::ordered_headers(&template.raw_headers, true);
    if let Some(resolved) =
        crate::identity::resolve_identity(&config.identity, &state.effective_version(&config))
    {
        crate::identity::apply_identity_headers(
            &mut headers,
            &resolved,
            &config.identity.residency,
        );
    }
    // canary 要铸新的，主动剥掉任何既有 turn-state。
    headers.remove("x-codex-turn-state");
    let ids = state.probe_ids.request_ids(account_id, model, "canary", 0);
    crate::identity::apply_one_id_headers(&mut headers, &ids);
    let mut body = build_canary_body(&template.body);
    if let Some(rewritten) = crate::identity::apply_one_id_body(&body, &ids) {
        body = rewritten;
    }

    let method = reqwest::Method::POST;
    let response = match client
        .request(method, &template.url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(_) => {
            state
                .pool
                .record_outcome(account_id, model, Outcome::OtherError, 0.0, &params);
            return CanaryReport::Error;
        }
    };

    let status = response.status();
    let resp_turn_state = response
        .headers()
        .get("x-codex-turn-state")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if !status.is_success() {
        // 4xx（bearer 过期等）/5xx：不据此判降智，只作错误计数。
        let outcome = if status.as_u16() == 429 || status.as_u16() == 529 {
            Outcome::Overload
        } else {
            Outcome::OtherError
        };
        state
            .pool
            .record_outcome(account_id, model, outcome, 0.0, &params);
        return if outcome == Outcome::Overload {
            CanaryReport::Overload
        } else {
            CanaryReport::Error
        };
    }

    // 读取 SSE 体（有界，最多 512KB 足够覆盖 completed 事件）。
    let mut sse = String::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                sse.push_str(&String::from_utf8_lossy(&bytes));
                if sse.len() > 512 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let overloaded = sse.contains("server_is_overloaded")
        || sse.contains("\"overloaded\"")
        || sse.contains("slow_down")
        || sse.contains("too_many_requests");
    if overloaded {
        state
            .pool
            .record_outcome(account_id, model, Outcome::Overload, 0.0, &params);
        return CanaryReport::Overload;
    }
    if is_degraded_from_sse(&sse) {
        state
            .pool
            .record_outcome(account_id, model, Outcome::Degraded, 0.0, &params);
        return CanaryReport::Degraded;
    }
    // 正常：铸到新 turn-state 则钉入池（若该格为空/过期）。
    state
        .pool
        .capture_if_empty(account_id, model, resp_turn_state.as_deref(), &params);
    state
        .pool
        .record_outcome(account_id, model, Outcome::Success, 0.0, &params);
    state.pool.persist_throttled(&config.pin_persist_path);
    CanaryReport::Healthy
}

// ---------------------------------------------------------------------------
// 主动养池（active warming）：后台用最小 hi 请求对"还没锁到 292/332"的 (account×model)
// 不断探铸。与 canary 的区别：
// - body 是最小 hi（省 token），不是推理题——锁票判据只看 292/332 票长，无需推理信号；
// - 出口走 egress_pool 轮换（干净 IP 才铸得出有效票），canary 走模板原代理；
// - 目标是"锁到 292/332 就停"，TTL 过期/被清后自动再养。
// bearer 仍来自真实流量模板，仅内存，绝不落盘。
// ---------------------------------------------------------------------------

/// 主动养池单次探铸结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmReport {
    /// 无模板（该 (account×model) 还没真实流量过，无 bearer 可用）。
    NoTemplate,
    /// 该格已持有效 292/332 锁票，无需养。
    AlreadyLocked,
    /// 本次铸到 292/332 并锁定。
    Locked,
    /// 本次拿到非 292/332（假票/无票），继续轮。
    Miss,
    /// 上游 overload（429/529/overloaded）。
    Overload,
    /// 其它错误（网络/4xx/5xx/bearer 失效）。
    Error,
}

/// 用模板 body 构造最小养池 body：input 换成 "hi"，stream、不落存储；
/// 其余字段（instructions/reasoning/client_metadata/prompt_cache_key/include/tools）
/// 原样保留，维持真实 codex 请求形状（身份轮换由 apply_one_id_body 接手改写）。
/// `model_override`：admin 养池借用别号的模板做形状时，把模型换成目标模型。
fn build_warm_body(template_body: &[u8], model_override: Option<&str>) -> Vec<u8> {
    let fallback = || {
        let model = model_override.unwrap_or("gpt-6-astra");
        format!(
            r#"{{"model":{},"input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]}}],"stream":true,"store":false}}"#,
            serde_json::to_string(model).unwrap_or_else(|_| "\"gpt-6-astra\"".to_string())
        )
        .into_bytes()
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(template_body) else {
        return fallback();
    };
    let Some(obj) = value.as_object_mut() else {
        return fallback();
    };
    if let Some(model) = model_override {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    }
    obj.insert(
        "input".to_string(),
        serde_json::json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]
        }]),
    );
    obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    obj.insert("store".to_string(), serde_json::Value::Bool(false));
    // Warm probes use their own stable cache domain.
    obj.remove("previous_response_id");
    serde_json::to_vec(&value).unwrap_or_else(|_| fallback())
}

/// 一次养池探铸的请求来源（形状 + 可选身份覆写）。
/// - 模板路径：url/headers/body 取自该号自己的真实模板，bearer/账号头沿用（override=None）。
/// - admin 路径：url/headers/body 借用任一"donor"真实模板做形状，bearer/账号头覆写成目标号，
///   model 换成目标模型。
pub struct WarmSource<'a> {
    pub account_id: i64,
    pub model: &'a str,
    pub url: &'a str,
    pub raw_headers: &'a HashMap<String, HeaderValues>,
    pub body: &'a [u8],
    /// admin 路径提供目标号的 access_token；模板路径为 None（沿用模板里的 authorization）。
    pub bearer_override: Option<&'a str>,
    /// admin 路径提供目标号的 chatgpt-account-id；模板路径为 None。
    pub chatgpt_account_id: Option<&'a str>,
    /// 是否把 model 换成 self.model（admin 借形状时为 true）。
    pub override_model: bool,
}

/// 养池探铸核心：出口池轮换 + 身份对齐 + 最小 hi，铸到 292 即锁进池。
/// 模板路径与 admin 路径共用此函数。
pub async fn warm_send(state: &Arc<SharedState>, src: WarmSource<'_>) -> WarmReport {
    let account_id = src.account_id;
    let model = src.model;
    let config = state.current_config();
    let params = PinParams::from_config(
        config.pin_max_age_seconds,
        config.pin_fail_threshold,
        config.pin_fail_ratio_pct,
    );
    if !state.pool.needs_mint(account_id, model, &params) {
        return WarmReport::AlreadyLocked;
    }

    // 铸票出口：egress_pool（轮换游标）。admin 路径没有本号代理可回落，
    // 池空且非模板路径时直接放弃（养池就是靠干净出口池铸票）。
    let egress_pool = config.egress_pool_list();
    let egress_lap_len = config.egress_lap_len();
    let egress_threshold = config.egress_threshold_effective();
    let use_pool = !egress_pool.is_empty();
    let mut pool_slot: usize = 0;
    let proxy: String = if use_pool {
        let idx = state
            .egress
            .current_index(account_id, model, egress_pool.len());
        pool_slot = idx + 1;
        egress_pool[idx].clone()
    } else if src.bearer_override.is_none() {
        // 模板路径：可回落到本号真实代理（url 同源）。这里用空串让 client 走默认，
        // 与 forward 的“宿主原代理”不同，故仅在出口池为空时退化；admin 路径已在上面被排除。
        String::new()
    } else {
        return WarmReport::Error;
    };

    // 按出口池行号隔离 client：每行独立连接。
    let client = match state
        .clients
        .client_for_slot(&config, account_id, &proxy, pool_slot)
    {
        Ok(client) => client,
        Err(_) => return WarmReport::Error,
    };

    // 忠实复用真实头形状 + 身份改写 + 每次换新会话（铸全新 turn-state）。
    let mut headers = crate::transport::ordered_headers(src.raw_headers, true);
    if let Some(resolved) =
        crate::identity::resolve_identity(&config.identity, &state.effective_version(&config))
    {
        crate::identity::apply_identity_headers(
            &mut headers,
            &resolved,
            &config.identity.residency,
        );
    }
    headers.remove("x-codex-turn-state");
    let purpose = if src.bearer_override.is_some() {
        "admin_warm"
    } else {
        "warm"
    };
    let ids = state
        .probe_ids
        .request_ids(account_id, model, purpose, pool_slot);
    crate::identity::apply_one_id_headers(&mut headers, &ids);
    // admin 路径：把借来的模板身份换成目标号（bearer + chatgpt-account-id）。
    if let Some(bearer) = src.bearer_override {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {bearer}")) {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
    }
    if let Some(acct) = src.chatgpt_account_id {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(acct) {
            headers.insert("chatgpt-account-id", value);
        }
    }
    let model_override = if src.override_model {
        Some(model)
    } else {
        None
    };
    let mut body = build_warm_body(src.body, model_override);
    if let Some(rewritten) = crate::identity::apply_one_id_body(&body, &ids) {
        body = rewritten;
    }

    let response = match client
        .request(reqwest::Method::POST, src.url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(_) => {
            state
                .egress
                .on_result(account_id, model, false, egress_lap_len, egress_threshold);
            return WarmReport::Error;
        }
    };

    let status = response.status();
    let resp_turn_state = response
        .headers()
        .get("x-codex-turn-state")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let is_real6 = resp_turn_state
        .as_deref()
        .is_some_and(|ts| crate::turn_state::is_lockable_ticket_len(ts.len()));

    state.egress.on_result(
        account_id,
        model,
        is_real6,
        egress_lap_len,
        egress_threshold,
    );

    // 养池探铸也写诊断 JSONL（path=warm/admin_warm + 出口），便于按出口统计 292 率。
    if config.turn_state_diag {
        let resp_model = response
            .headers()
            .get("openai-model")
            .or_else(|| response.headers().get("x-openai-model"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        crate::turn_state::append_observation(
            &config.turn_state_diag_path,
            &crate::turn_state::Observation {
                ts_ms: now_ms(),
                account_id,
                outbound_model: Some(model.to_string()),
                status_code: status.as_u16() as i32,
                req_session_id: Some(ids.conversation_id.clone()),
                req_in_turn_state_len: 0,
                req_turn_state_prefix: None,
                req_turn_state_len: 0,
                pinned_injected: false,
                resp_turn_state: resp_turn_state.clone(),
                resp_turn_state_len: resp_turn_state.as_deref().map(str::len).unwrap_or(0),
                resp_openai_model: resp_model,
                resp_turn_state_changed: resp_turn_state.is_some(),
                path: if src.bearer_override.is_some() {
                    "admin_warm".to_string()
                } else {
                    "warm".to_string()
                },
                egress: use_pool.then(|| crate::transport::proxy_host(&proxy)),
            },
        );
    }

    if !status.is_success() {
        let outcome = if status.as_u16() == 429 || status.as_u16() == 529 {
            Outcome::Overload
        } else {
            Outcome::OtherError
        };
        state
            .pool
            .record_outcome(account_id, model, outcome, 0.0, &params);
        return if outcome == Outcome::Overload {
            WarmReport::Overload
        } else {
            WarmReport::Error
        };
    }

    // 判据只看响应头票长（x-codex-turn-state 是响应头，headers 到手即已确定 292/312，
    // body 内容对判定无影响）。仅象征性排干极少量 body 维持连接卫生，拿到即走——
    // 相比旧的 256KB 排干，单次探铸耗时大幅下降，配合并发显著提高铸票吞吐。
    let mut drained = 0usize;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                drained += bytes.len();
                if drained > 4 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    if is_real6 {
        state
            .pool
            .capture_if_empty(account_id, model, resp_turn_state.as_deref(), &params);
        state
            .pool
            .record_outcome(account_id, model, Outcome::Success, 0.0, &params);
        state.pool.persist_throttled(&config.pin_persist_path);
        WarmReport::Locked
    } else {
        state
            .pool
            .record_outcome(account_id, model, Outcome::Degraded, 0.0, &params);
        WarmReport::Miss
    }
}

/// 对 (account, model) 跑一次主动养池探铸（模板路径：用该号自己的真实模板）。
pub async fn run_warm(state: &Arc<SharedState>, account_id: i64, model: &str) -> WarmReport {
    let Some(template) = state.creds.get(account_id, model) else {
        return WarmReport::NoTemplate;
    };
    warm_send(
        state,
        WarmSource {
            account_id,
            model,
            url: &template.url,
            raw_headers: &template.raw_headers,
            body: &template.body,
            bearer_override: None,
            chatgpt_account_id: None,
            override_model: false,
        },
    )
    .await
}

/// admin 全池养池：借用任一真实模板做形状，用目标号的 bearer/账号头造票。
pub async fn run_warm_admin(
    state: &Arc<SharedState>,
    donor: &Template,
    account_id: i64,
    model: &str,
    bearer: &str,
    chatgpt_account_id: Option<&str>,
) -> WarmReport {
    warm_send(
        state,
        WarmSource {
            account_id,
            model,
            url: &donor.url,
            raw_headers: &donor.raw_headers,
            body: &donor.body,
            bearer_override: Some(bearer),
            chatgpt_account_id,
            override_model: true,
        },
    )
    .await
}

/// admin 全池养池后台循环：内置 admin key，调 admin API export 枚举全部可调度
/// openai oauth 号 + 取 bearer，对每个 (号 × warming_models) 没锁 292 的格各探铸一次
/// （借用真实模板做形状；出口走 egress_pool）。锁到就停，TTL 过期下轮再养。
pub async fn admin_warm_loop(state: Arc<SharedState>) {
    loop {
        let config = state.current_config();
        if !config.admin_warming_enabled
            || config.turn_state_mode != "pin"
            || config.admin_api_base.trim().is_empty()
            || config.admin_api_key.trim().is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }
        let interval = config.admin_warming_interval_seconds.clamp(15, 3600) as u64;
        let params = PinParams::from_config(
            config.pin_max_age_seconds,
            config.pin_fail_threshold,
            config.pin_fail_ratio_pct,
        );

        // 形状 donor：任取一条最近的真实 codex 模板。没有就等真实流量先喂一条。
        let Some(donor) = state.creds.any_recent() else {
            eprintln!(
                "[codex-native-transport] admin_warm: no donor template yet, waiting for real codex traffic"
            );
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            continue;
        };
        let donor = Arc::new(donor);

        let targets =
            crate::admin::fetch_targets(config.admin_api_base.trim(), config.admin_api_key.trim())
                .await;
        let targets = match targets {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[codex-native-transport] admin_warm: fetch targets failed: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                continue;
            }
        };

        let models = config.warming_models_list();
        // 组装本轮 jobs：跳过已锁到有效 292 的格；其余并发探铸。
        let mut jobs: Vec<(i64, String, String, Option<String>)> = Vec::new();
        let mut skipped = 0u32;
        for t in &targets {
            // 休息中的号（已被摘出调度）不铸票。正常它已不在 schedulable 列表里，
            // 这里再兜一层，防暂停与下一次枚举之间的竞态。
            if state.acct_rest.is_resting(t.account_id) {
                skipped += 1;
                continue;
            }
            for model in &models {
                // 已锁到有效 292、或已放弃铸票（直通）的格都不探。
                if !state.pool.needs_mint(t.account_id, model, &params) {
                    skipped += 1;
                    continue;
                }
                jobs.push((
                    t.account_id,
                    model.clone(),
                    t.access_token.clone(),
                    t.chatgpt_account_id.clone(),
                ));
            }
        }
        // 有界并发探铸（buffer_unordered 在单任务内并发 I/O，控制对上游/出口池压力）。
        let results: Vec<WarmReport> = futures_util::stream::iter(jobs)
            .map(|(account_id, model, bearer, cgid)| {
                let state = Arc::clone(&state);
                let donor = Arc::clone(&donor);
                async move {
                    run_warm_admin(
                        &state,
                        donor.as_ref(),
                        account_id,
                        &model,
                        &bearer,
                        cgid.as_deref(),
                    )
                    .await
                }
            })
            .buffer_unordered(WARM_CONCURRENCY)
            .collect()
            .await;
        let (mut minted, mut miss, mut err) = (0u32, 0u32, 0u32);
        for r in &results {
            match r {
                WarmReport::Locked => minted += 1,
                WarmReport::Miss => miss += 1,
                WarmReport::AlreadyLocked => skipped += 1,
                _ => err += 1,
            }
        }
        eprintln!(
            "[codex-native-transport] admin_warm cycle done: targets={} models={} minted292={} miss={} err={} skipped={}",
            targets.len(),
            models.len(),
            minted,
            miss,
            err,
            skipped
        );
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

/// 养池休息持久化路径：暂停集落盘在 pin_persist_path 同目录的 `.rest` 兄弟文件。
/// pin_persist_path 为空则返回空串（休息集仅内存，重启后由启动兜底恢复不可用——
/// 因此生产建议配置 pin_persist_path）。
fn rest_persist_path(pin_persist_path: &str) -> String {
    let p = pin_persist_path.trim();
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}.rest")
    }
}

/// 休息编排后台循环（事件触发，两层）。
///
/// 生效条件（每 15s 巡检）：pin 模式 + 开了任一养池(主动/全池) + 配了 admin API +
/// rest>0 + egress_pool 非空。
///
/// **格级休息**（(号×模型) 维度，不碰宿主）：某格把出口池整整转过一圈仍铸不出 292
/// （EgressRotor::lap_exhausted）→ 记一轮 stuck；未达 pin_giveup_rounds 就让这一格休息
/// warming_rest_seconds（不探铸、真实流量直通不注入），到点自动重开一圈；达阈值进入放弃态
/// pin_giveup_retry_seconds。持有 292 的格、票过期后一圈内能重新铸出的格永不休息。
///
/// **账号级排空**（叠加在格级之上）：本轮有格进入休息、且该号在 warming_models 上**没有任何
/// 有效 292**、且距上次恢复满 warming_duty_seconds → 把宿主调度 priority 改成
/// warming_drain_priority（新会话不再分配到它，已粘住的老会话继续命中、不被打散），
/// 休息到点写回原优先级。任一模型手里有 292 的号只歇卡住的格，优先级不动。
///
/// 休息集（含原优先级）落盘；插件重启后到点恢复、关停前主动写回，账号不会卡在低优先级。
pub async fn rest_loop(state: Arc<SharedState>) {
    // 启动兜底：载入上次落盘的休息集，随后循环里到点即恢复（插件曾崩溃/重启也不卡低优先级）。
    {
        let cfg = state.current_config();
        state
            .acct_rest
            .load(&rest_persist_path(&cfg.pin_persist_path));
    }
    loop {
        let config = state.current_config();
        let duty = config.warming_duty_seconds as u64;
        let rest = config.warming_rest_seconds as u64;
        let admin_ready =
            !config.admin_api_base.trim().is_empty() && !config.admin_api_key.trim().is_empty();
        let warming_on = config.active_warming_enabled || config.admin_warming_enabled;
        let enabled = config.turn_state_mode == "pin"
            && warming_on
            && admin_ready
            && rest > 0
            && config.egress_enabled();
        let base = config.admin_api_base.trim();
        let key = config.admin_api_key.trim();
        let path = rest_persist_path(&config.pin_persist_path);
        let now = now_ms() as u64;

        if !enabled {
            // 关掉/未配齐时：把仍被我们压低优先级的号立即写回，避免卡低优先级，然后待命。
            if admin_ready {
                for (id, _, orig) in state.acct_rest.paused_list() {
                    if crate::admin::set_account_priority(base, key, id, orig)
                        .await
                        .is_ok()
                    {
                        state.acct_rest.mark_resumed(id, now, &path);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }

        let params = PinParams::from_config(
            config.pin_max_age_seconds,
            config.pin_fail_threshold,
            config.pin_fail_ratio_pct,
        );
        let models = config.warming_models_list();
        let duty_ms = duty * 1000;
        let rest_ms = rest * 1000;
        let retry_ms = config.pin_giveup_retry_seconds as u64 * 1000;

        // 1) 到点恢复：休息期满的号写回原优先级，并给它的养池格重开一圈出口池轮转。
        let mut resumed = 0u32;
        for (id, rest_until, orig) in state.acct_rest.paused_list() {
            if now < rest_until {
                continue;
            }
            match crate::admin::set_account_priority(base, key, id, orig).await {
                Ok(()) => {
                    state.acct_rest.mark_resumed(id, now, &path);
                    for model in &models {
                        state.egress.reset_lap(id, model);
                    }
                    resumed += 1;
                }
                Err(e) => {
                    eprintln!(
                        "[codex-native-transport] rest: restore priority acct {id} failed: {e}"
                    );
                }
            }
        }

        // 2) 枚举当前可调度号。休息分两层：
        //    - 格级：某 (号×模型) 出口池转满一圈仍无 292 → 记一轮 stuck；未达放弃阈值就让这一格
        //      单独休息 rest（停探、真实流量直通），不碰账号优先级。这样「astra 有 292、terra 只出
        //      312」的号只有 terra 歇，astra 照常注入。
        //    - 账号级：该号所有养池模型都没有有效 292、且本轮至少一个格进入格级休息时，再叠加
        //      优先级排空（新会话不再分配到它）；持有任何 292 的号永不排空。
        let mut paused = 0u32;
        let mut cell_rested = 0u32;
        let mut abandoned = 0u32;
        match crate::admin::fetch_schedulable_ids(base, key).await {
            Ok(ids) => {
                for id in ids {
                    let mut rested_here = 0u32;
                    for m in &models {
                        if !state.egress.lap_exhausted(id, m)
                            || state.pool.is_parked(id, m, now)
                            || state.pool.injectable(id, m, &params).is_some()
                        {
                            continue;
                        }
                        // 这一圈已经用掉：无论休不休息都重开计数，恢复后从头再转一圈。
                        state.egress.reset_lap(id, m);
                        if state
                            .pool
                            .note_stuck_round(id, m, config.pin_giveup_rounds, retry_ms)
                        {
                            abandoned += 1;
                            eprintln!(
                                "[codex-native-transport] rest: acct {id} model {m} abandoned minting for {}s (no 292 after {} exhausted laps)",
                                config.pin_giveup_retry_seconds, config.pin_giveup_rounds
                            );
                        } else {
                            state.pool.park(id, m, rest_ms);
                            rested_here += 1;
                            cell_rested += 1;
                        }
                    }
                    if rested_here > 0 {
                        state.pool.persist_throttled(&config.pin_persist_path);
                    }
                    // 账号级排空：本轮有格进入休息、该号没有任何有效 292、且过了防抖。
                    if rested_here == 0 || !state.acct_rest.can_rest(id, now, duty_ms) {
                        continue;
                    }
                    let holds_ticket = models
                        .iter()
                        .any(|m| state.pool.injectable(id, m, &params).is_some());
                    if holds_ticket {
                        continue; // 别的模型手里有真6票：只歇卡住的格，不动账号优先级。
                    }
                    // 进入休息：先读原优先级，再改低。读不到不动（下轮再试）。
                    let orig = match crate::admin::fetch_account_priority(base, key, id).await {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("[codex-native-transport] rest: read priority acct {id} failed: {e}");
                            continue;
                        }
                    };
                    match crate::admin::set_account_priority(
                        base,
                        key,
                        id,
                        config.warming_drain_priority,
                    )
                    .await
                    {
                        Ok(()) => {
                            state.acct_rest.mark_paused(id, now, rest_ms, orig, &path);
                            paused += 1;
                        }
                        Err(e) => {
                            eprintln!("[codex-native-transport] rest: drain acct {id} failed: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[codex-native-transport] rest: fetch schedulable ids failed: {e}");
            }
        }
        if resumed > 0 || paused > 0 || cell_rested > 0 || abandoned > 0 {
            eprintln!(
                "[codex-native-transport] rest cycle: cells_rested={cell_rested} accounts_drained={paused} resumed={resumed} abandoned={abandoned} (rest={rest}s min_active={duty}s drain_priority={})",
                config.warming_drain_priority
            );
        }
        // 巡检节拍：固定 15s，比 rest 细，保证进入/恢复及时。
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
}

/// 关停前恢复：宿主 Kill 插件（停用/升级/重启）时先把休息中账号的优先级写回原值再退出。
/// go-plugin 只给约 2s 的优雅窗口，这里硬性限时 1.5s；超时也不阻塞退出——落盘的休息集
/// 让下一次启动能继续兜底恢复。
pub async fn restore_on_shutdown(state: &Arc<SharedState>) {
    let paused = state.acct_rest.paused_list();
    if paused.is_empty() {
        return;
    }
    let config = state.current_config();
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    if base.is_empty() || key.is_empty() {
        return;
    }
    let path = rest_persist_path(&config.pin_persist_path);
    let now = now_ms() as u64;
    let work = async {
        let results: Vec<(i64, Result<(), String>)> = futures_util::stream::iter(paused)
            .map(|(id, _, orig)| {
                let base = base.clone();
                let key = key.clone();
                async move {
                    (
                        id,
                        crate::admin::set_account_priority(&base, &key, id, orig).await,
                    )
                }
            })
            .buffer_unordered(WARM_CONCURRENCY)
            .collect()
            .await;
        let (mut ok, mut failed) = (0u32, 0u32);
        for (id, r) in results {
            match r {
                Ok(()) => {
                    state.acct_rest.mark_resumed(id, now, &path);
                    ok += 1;
                }
                Err(e) => {
                    eprintln!(
                        "[codex-native-transport] shutdown: restore priority acct {id} failed: {e}"
                    );
                    failed += 1;
                }
            }
        }
        (ok, failed)
    };
    match tokio::time::timeout(std::time::Duration::from_millis(1500), work).await {
        Ok((ok, failed)) => eprintln!(
            "[codex-native-transport] shutdown: restored priority for {ok} resting account(s), {failed} failed"
        ),
        Err(_) => eprintln!(
            "[codex-native-transport] shutdown: restore timed out; resting set persisted for next start"
        ),
    }
}

/// 主动养池后台循环：每轮遍历已知 (account×model)，对"没锁到 292"且模板新鲜的格
/// 各探铸一次（锁到 292 的格自动跳过 = 锁到就停），一轮结束睡 interval 再来。
/// TTL 过期/票被清后 injectable 变空，下一轮自动恢复养护。
pub async fn warm_loop(state: Arc<SharedState>) {
    loop {
        let config = state.current_config();
        if !config.active_warming_enabled || config.turn_state_mode != "pin" {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }
        let interval = config.active_warming_interval_seconds.clamp(5, 3600) as u64;
        let params = PinParams::from_config(
            config.pin_max_age_seconds,
            config.pin_fail_threshold,
            config.pin_fail_ratio_pct,
        );
        // 只养 warming_models 里配置的模型：主动养池与全池养池口径一致，别的模型
        // （真实流量顺带建的格，如 codex-auto-review/gpt-5.2 等）不铸票、不浪费额度。
        let warming: std::collections::HashSet<String> =
            config.warming_models_list().into_iter().collect();
        // 收集本轮待养的格：属于 warming_models、模板新鲜(<3h，跳过 bearer 极可能已失效
        // 的陈旧模板)、且当前没锁到有效 292。
        let now = now_ms() as u64;
        let jobs: Vec<(i64, String)> = state
            .creds
            .keys()
            .into_iter()
            .filter(|(account_id, model)| {
                if !warming.contains(model) {
                    return false;
                }
                // 休息中的号不铸票（已被摘出调度，真正休息）。
                if state.acct_rest.is_resting(*account_id) {
                    return false;
                }
                let fresh = state
                    .creds
                    .get(*account_id, model)
                    .map(|t| now.saturating_sub(t.updated_ms) < 3 * 3600 * 1000)
                    .unwrap_or(false);
                fresh && state.pool.needs_mint(*account_id, model, &params)
            })
            .collect();
        // 有界并发探铸。
        futures_util::stream::iter(jobs)
            .for_each_concurrent(WARM_CONCURRENCY, |(account_id, model)| {
                let state = Arc::clone(&state);
                async move {
                    if state.current_config().active_warming_enabled {
                        let _ = run_warm(&state, account_id, &model).await;
                    }
                }
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

/// canary 后台循环：按配置间隔遍历已知 (account×model)，对空闲/失效/降智格重铸探测。
pub async fn canary_loop(state: Arc<SharedState>) {
    loop {
        let config = state.current_config();
        if !config.canary_enabled {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }
        let interval = config.canary_interval_seconds.max(30) as u64;
        let params = PinParams::from_config(
            config.pin_max_age_seconds,
            config.pin_fail_threshold,
            config.pin_fail_ratio_pct,
        );
        // 只照看 warming_models 里配置的模型，别的模型不探测、不浪费额度。
        let warming: std::collections::HashSet<String> =
            config.warming_models_list().into_iter().collect();
        // 只探测“需要照看”的格：空/过期/失效/降智，或从未探测过。
        for (account_id, model) in state.creds.keys() {
            if !state.current_config().canary_enabled {
                break;
            }
            if !warming.contains(&model) {
                continue;
            }
            // 跳过 bearer 极可能已失效的陈旧模板（>3h 无真实流量）。
            let fresh = state
                .creds
                .get(account_id, &model)
                .map(|t| (now_ms() as u64).saturating_sub(t.updated_ms) < 3 * 3600 * 1000)
                .unwrap_or(false);
            if fresh && should_probe(&state, account_id, &model, &params) {
                let _ = run_canary(&state, account_id, &model).await;
                // 轻微错峰，避免同账号并发。
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

/// 该格是否值得探测：无可注入值（空/过期/失效）即需要重铸。
fn should_probe(
    state: &Arc<SharedState>,
    account_id: i64,
    model: &str,
    params: &PinParams,
) -> bool {
    state.pool.needs_mint(account_id, model, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canary_body_overrides_input_and_reasoning() {
        let tpl = br#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"old"}]}],"stream":false,"reasoning":{"effort":"low"}}"#;
        let out = build_canary_body(tpl);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-5.4");
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        assert_eq!(v["reasoning"]["effort"], "high");
        assert!(v["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("split a bill"));
    }

    #[test]
    fn canary_body_falls_back_on_garbage() {
        let out = build_canary_body(b"not json");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");
    }

    #[test]
    fn warm_body_is_minimal_hi_and_preserves_shape() {
        // input 换成最小 "hi"，stream/store 强制；其余字段（model/instructions/
        // reasoning/client_metadata/prompt_cache_key/include）原样保留。
        let tpl = br#"{"model":"gpt-6-astra","instructions":"You are Codex...","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"a very long previous user conversation"}]}],"stream":false,"store":true,"reasoning":{"effort":"medium","summary":"auto"},"prompt_cache_key":"conv-1","client_metadata":{"session_id":"conv-1"},"include":["reasoning.encrypted_content"]}"#;
        let out = build_warm_body(tpl, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-6-astra");
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        assert_eq!(v["input"][0]["content"][0]["text"], "hi");
        assert_eq!(v["input"].as_array().unwrap().len(), 1);
        // 形状保留：身份轮换后续由 apply_one_id_body 改写。
        assert_eq!(v["reasoning"]["effort"], "medium");
        assert_eq!(v["prompt_cache_key"], "conv-1");
        assert_eq!(v["client_metadata"]["session_id"], "conv-1");
        assert_eq!(v["instructions"], "You are Codex...");
        assert_eq!(v["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn warm_body_falls_back_on_garbage() {
        let out = build_warm_body(b"not json", None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["input"][0]["content"][0]["text"], "hi");
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        // fallback 尊重 model_override。
        let out2 = build_warm_body(b"not json", Some("gpt-5.6-luna"));
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2["model"], "gpt-5.6-luna");
    }

    #[test]
    fn warm_body_overrides_model_for_admin_path() {
        // admin 借别号的模板做形状：model 换成目标模型，input 仍为 hi，其余形状保留。
        let tpl = br#"{"model":"gpt-6-astra","instructions":"x","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"long"}]}],"stream":false,"store":true}"#;
        let out = build_warm_body(tpl, Some("gpt-5.6-sol"));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-5.6-sol");
        assert_eq!(v["input"][0]["content"][0]["text"], "hi");
        assert_eq!(v["instructions"], "x");
    }

    #[test]
    fn cred_cache_roundtrip() {
        let cache = CredCache::new();
        assert!(cache.get(1, "m").is_none());
        cache.record(1, "m", "https://x/y", "", &HashMap::new(), b"{}");
        assert!(cache.get(1, "m").is_some());
        assert_eq!(cache.keys(), vec![(1, "m".to_string())]);
    }

    #[test]
    fn cred_cache_any_recent_picks_newest() {
        let cache = CredCache::new();
        assert!(cache.any_recent().is_none());
        cache.record(1, "m", "https://a/1", "", &HashMap::new(), b"{\"n\":1}");
        std::thread::sleep(std::time::Duration::from_millis(2));
        cache.record(2, "m", "https://b/2", "", &HashMap::new(), b"{\"n\":2}");
        let donor = cache.any_recent().unwrap();
        assert_eq!(donor.url, "https://b/2");
    }

    #[test]
    fn probe_identity_reuses_cache_domain_but_refreshes_turn_id() {
        let cache = ProbeIdentityCache::default();
        let first = cache.request_ids(7, "gpt-6-astra", "warm", 2);
        let second = cache.request_ids(7, "gpt-6-astra", "warm", 2);
        assert_eq!(first.conversation_id, second.conversation_id);
        assert_eq!(first.installation_id, second.installation_id);
        assert_eq!(first.context_window_id, second.context_window_id);
        assert_ne!(first.turn_id, second.turn_id);
        let other = cache.request_ids(7, "gpt-6-astra", "canary", 2);
        assert_ne!(first.conversation_id, other.conversation_id);
    }
}
