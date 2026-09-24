//! 账号智力巡检（降智检测 + 自动暂停/恢复 + 每账号降智处理开关）。
//!
//! 判据很土但直接：拿账号自己的 bearer 打一次真实 Codex /responses，
//! 问一句知识库日期类问题，回答里出现 `2024`（可配）即判定该号当前被降智。
//!
//! 请求形状（url / 请求头 / body）借用池里任一条真实流量模板，只把 input 换成
//! 巡检问题、model 换成巡检模型、bearer 与 chatgpt-account-id 换成目标号，
//! 因此出站特征与真实业务流量一致。bearer 只在内存流转，绝不落盘；落盘的只有
//! 巡检结论（合格/不合格 + 回答片段）与每账号「自动降智处理」开关。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::admin::{self, AdminTarget};
use crate::config::{normalize_plan_type, PluginConfig};
use crate::donor::{now_ms, Template};
use crate::service::SharedState;

/// 巡检默认模型。
pub const DEFAULT_INTEL_MODEL: &str = "gpt-6-astra";
/// 巡检默认问题。
pub const DEFAULT_INTEL_PROMPT: &str = "你在知识库日期 不允许联网快速回答";
/// 命中即判不合格的标记。
pub const DEFAULT_INTEL_FAIL_MARKER: &str = "2024";

/// 单个账号的巡检结论。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct IntelResult {
    /// true = 合格（回答里没有降智标记）。
    pub ok: bool,
    /// 回答片段（截断，便于人工复核）。
    pub answer: String,
    /// 失败原因（网络 / 上游 4xx / 无回答正文等）。
    pub error: String,
    pub latency_ms: i64,
    pub checked_at_ms: u64,
    pub model: String,
    /// 本次是否因为超时（而不是上游明确报错）被判不合格。
    #[serde(default)]
    pub timed_out: bool,
}

/// 巡检批次状态（给面板轮询用）。
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct IntelRun {
    pub running: bool,
    pub total: usize,
    pub done: usize,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    /// 收尾说明 / 错误。
    pub note: String,
}

#[derive(Default)]
struct Inner {
    results: BTreeMap<i64, IntelResult>,
    run: IntelRun,
    loaded: bool,
}

/// 巡检结果内存仓（可落盘）。进程内唯一，挂在 SharedState 上。
#[derive(Default)]
pub struct IntelStore {
    inner: Mutex<Inner>,
}

impl IntelStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn snapshot(&self) -> (BTreeMap<i64, IntelResult>, IntelRun) {
        let guard = self.lock();
        (guard.results.clone(), guard.run.clone())
    }

    pub fn get(&self, account_id: i64) -> Option<IntelResult> {
        self.lock().results.get(&account_id).cloned()
    }

    /// 是否已有巡检在跑（面板据此决定是否再放一批进来）。
    pub fn is_running(&self) -> bool {
        self.lock().run.running
    }

    fn set_result(&self, account_id: i64, result: IntelResult) {
        self.lock().results.insert(account_id, result);
    }

    /// 开一批巡检；已有巡检在跑时返回 false。
    fn begin(&self, total: usize) -> bool {
        let mut guard = self.lock();
        if guard.run.running {
            return false;
        }
        guard.run = IntelRun {
            running: true,
            total,
            done: 0,
            started_at_ms: now_ms(),
            finished_at_ms: 0,
            note: String::new(),
        };
        true
    }

    fn set_total(&self, total: usize) {
        self.lock().run.total = total;
    }

    fn progress(&self, done: usize) {
        self.lock().run.done = done;
    }

    fn finish(&self, note: String) {
        let mut guard = self.lock();
        guard.run.running = false;
        guard.run.finished_at_ms = now_ms();
        guard.run.note = note;
    }

    /// 首次读到面板时按需从磁盘恢复上一次的结论。
    pub fn ensure_loaded(&self, path: &str) {
        {
            let guard = self.lock();
            if guard.loaded {
                return;
            }
        }
        let path = path.trim();
        let mut loaded = BTreeMap::new();
        if !path.is_empty() {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(map) = serde_json::from_slice::<BTreeMap<i64, IntelResult>>(&bytes) {
                    loaded = map;
                }
            }
        }
        let mut guard = self.lock();
        guard.loaded = true;
        if guard.results.is_empty() {
            guard.results = loaded;
        }
    }

    fn persist(&self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let results = self.lock().results.clone();
        let Ok(bytes) = serde_json::to_vec(&results) else {
            return;
        };
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, bytes);
    }
}

/// 每账号「自动降智处理 + 通道状态」。
///
/// 勾上的账号：智力不合格时**不暂停调度**，而是把 bps_models 里的模型请求
/// 改走 BPS 通道；等它连续合格 N 次、并且合格后的保持期也走完，才切回正常
/// 通道，避免刚恢复就被下一轮巡检打回造成的来回抖动。只落 id → 状态，
/// 不含任何凭据。
#[derive(Default)]
pub struct DegradeStore {
    inner: Mutex<DegradeInner>,
}

#[derive(Default)]
struct DegradeInner {
    states: BTreeMap<i64, ChannelState>,
    loaded: bool,
}

/// 单个账号的通道状态。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ChannelState {
    /// 是否参与「自动降智处理」（面板勾选）。
    #[serde(default)]
    pub enabled: bool,
    /// true = 最近一次结论为不合格（强制走 BPS）。
    #[serde(default)]
    pub failed: bool,
    /// 连续合格的次数（达到确认次数才允许切回）。
    #[serde(default)]
    pub healthy_streak: u32,
    /// 首次判合格的时间（ms；0 = 还没合格过）。
    #[serde(default)]
    pub healthy_since_ms: u64,
    /// 最近一次进入 BPS 通道的时间（ms；0 = 不在 BPS 上）。
    #[serde(default)]
    pub bps_since_ms: u64,
    /// 最近一次通道切换时间（ms）与原因。
    #[serde(default)]
    pub last_switch_ms: u64,
    #[serde(default)]
    pub last_reason: String,
}

/// 当前通道决策（面板 / 日志用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelDecision {
    pub enabled: bool,
    pub use_bps: bool,
    pub reason: String,
    /// 还需要在 BPS 上保持多久（ms；0 = 已可切回）。
    pub hold_remaining_ms: u64,
    pub healthy_streak: u32,
    pub confirmations: u32,
    pub bps_since_ms: u64,
}

impl DegradeStore {
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
        let path = path.trim();
        let mut loaded = BTreeMap::new();
        if !path.is_empty() {
            if let Ok(bytes) = std::fs::read(path) {
                // 兼容旧版 `{"54": true}` 的布尔格式。
                if let Ok(raw) = serde_json::from_slice::<BTreeMap<i64, serde_json::Value>>(&bytes)
                {
                    for (account_id, value) in raw {
                        let state = match value {
                            serde_json::Value::Bool(enabled) => ChannelState {
                                enabled,
                                failed: enabled,
                                ..ChannelState::default()
                            },
                            other => {
                                serde_json::from_value::<ChannelState>(other).unwrap_or_default()
                            }
                        };
                        loaded.insert(account_id, state);
                    }
                }
            }
        }
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.loaded = true;
        if guard.states.is_empty() {
            guard.states = loaded;
        }
    }

    fn state(&self, account_id: i64) -> ChannelState {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .states
            .get(&account_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 是否参与「自动降智处理」。
    pub fn is_enabled(&self, account_id: i64) -> bool {
        self.state(account_id).enabled
    }

    /// 面板勾选 / 取消勾选。
    pub fn set(&self, account_id: i64, enabled: bool, path: &str) {
        self.ensure_loaded(path);
        let now = now_ms();
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = guard.states.entry(account_id).or_default();
            if entry.enabled != enabled {
                entry.last_switch_ms = now;
                entry.last_reason = if enabled {
                    "面板开启降智处理，先按 BPS 兜住".to_string()
                } else {
                    "面板关闭降智处理，回到正常通道".to_string()
                };
            }
            entry.enabled = enabled;
            if enabled {
                // 重新参与时按「尚未确认」起步：先走 BPS，巡检确认合格后再切回。
                entry.failed = false;
                entry.healthy_streak = 0;
                entry.healthy_since_ms = 0;
                if entry.bps_since_ms == 0 {
                    entry.bps_since_ms = now;
                }
            } else {
                entry.bps_since_ms = 0;
            }
        }
        self.persist(path);
    }

    /// 记录一次巡检结论。`None` = 结论不确定（网络失败等），不改通道判定。
    pub fn apply_verdict(
        &self,
        account_id: i64,
        ok: Option<bool>,
        config: &PluginConfig,
        path: &str,
    ) {
        let Some(ok) = ok else {
            return;
        };
        self.ensure_loaded(path);
        let now = now_ms();
        let confirmations = config.intel_confirmations.clamp(1, 10);
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = guard.states.entry(account_id).or_default();
            if ok {
                entry.failed = false;
                if entry.healthy_streak == 0 {
                    entry.healthy_since_ms = now;
                }
                entry.healthy_streak = entry.healthy_streak.saturating_add(1);
                if entry.healthy_streak >= confirmations && entry.bps_since_ms != 0 {
                    entry.bps_since_ms = 0;
                    entry.last_switch_ms = now;
                    entry.last_reason = format!(
                        "已连续合格 {confirmations} 次，保持 {}s 后切回正常通道",
                        config.bps_hold_after_healthy_seconds
                    );
                }
            } else {
                if !entry.failed {
                    entry.last_switch_ms = now;
                    entry.last_reason = "巡检不合格，切入 BPS 通道".to_string();
                }
                entry.failed = true;
                entry.healthy_streak = 0;
                entry.healthy_since_ms = 0;
                if entry.bps_since_ms == 0 {
                    entry.bps_since_ms = now;
                }
            }
        }
        self.persist(path);
    }

    /// 面板 / 日志用的通道决策。
    pub fn decision(&self, account_id: i64, config: &PluginConfig, now: u64) -> ChannelDecision {
        channel_decision(&self.state(account_id), config, now)
    }

    /// 记录「这个号实际用了 BPS」（面板显示进入时间）。
    pub fn mark_bps_used(&self, account_id: i64, now: u64) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.states.entry(account_id).or_default();
        if entry.bps_since_ms == 0 {
            entry.bps_since_ms = now;
        }
    }

    pub fn snapshot(&self) -> BTreeMap<i64, ChannelState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .states
            .clone()
    }

    fn persist(&self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let states = self.snapshot();
        let Ok(bytes) = serde_json::to_vec(&states) else {
            return;
        };
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, bytes);
    }
}

/// 由账号通道状态 + 配置算出当前该走哪条通道（service / 面板共用同一套判定）。
pub fn channel_decision(state: &ChannelState, config: &PluginConfig, now: u64) -> ChannelDecision {
    let confirmations = config.intel_confirmations.clamp(1, 10);
    let base = ChannelDecision {
        enabled: state.enabled,
        use_bps: false,
        reason: String::new(),
        hold_remaining_ms: 0,
        healthy_streak: state.healthy_streak,
        confirmations,
        bps_since_ms: state.bps_since_ms,
    };
    if !state.enabled {
        return ChannelDecision {
            reason: "未开启降智处理".to_string(),
            ..base
        };
    }
    if state.failed {
        return ChannelDecision {
            use_bps: true,
            reason: "巡检不合格，走 BPS 通道".to_string(),
            ..base
        };
    }
    if state.healthy_streak < confirmations {
        return ChannelDecision {
            use_bps: true,
            reason: format!("合格确认中 {}/{}", state.healthy_streak, confirmations),
            ..base
        };
    }
    let hold_ms = config.bps_hold_after_healthy_seconds as u64 * 1000;
    let since = if state.healthy_since_ms == 0 {
        now
    } else {
        state.healthy_since_ms
    };
    let remaining = hold_ms.saturating_sub(now.saturating_sub(since));
    if remaining > 0 {
        return ChannelDecision {
            use_bps: true,
            reason: format!("合格保持期，还有 {}s 切回正常通道", remaining / 1000),
            hold_remaining_ms: remaining,
            ..base
        };
    }
    ChannelDecision {
        reason: "已恢复，走正常通道".to_string(),
        ..base
    }
}

/// 一批巡检的汇总。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SweepOutcome {
    pub total: usize,
    pub qualified: usize,
    pub unqualified: usize,
    pub failed: usize,
    pub paused: usize,
    pub resumed: usize,
    /// 因勾了「自动降智处理」而跳过暂停的号数量。
    pub degrade_handled: usize,
    pub note: String,
}

/// 用模板 body 构造巡检 body：只换 model / input，强制 stream + store:false，
/// 其余字段（instructions / reasoning / include / tools …）原样保留。
pub fn build_intel_body(template_body: &[u8], model: &str, question: &str) -> Vec<u8> {
    let fallback = || {
        format!(
            r#"{{"model":{},"instructions":"You are a careful reasoning assistant.","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":{}}}]}}],"stream":true,"store":false}}"#,
            serde_json::to_string(model).unwrap_or_else(|_| "\"gpt-6-astra\"".to_string()),
            serde_json::to_string(question).unwrap_or_else(|_| "\"?\"".to_string())
        )
        .into_bytes()
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(template_body) else {
        return fallback();
    };
    let Some(obj) = value.as_object_mut() else {
        return fallback();
    };
    let Some(_) = obj.get("input") else {
        return fallback();
    };
    obj.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    obj.insert(
        "input".to_string(),
        serde_json::json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": question}]
        }]),
    );
    obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    obj.insert("store".to_string(), serde_json::Value::Bool(false));
    // 巡检是独立探针，绝不接续任何会员的响应链。
    obj.remove("previous_response_id");
    serde_json::to_vec(&value).unwrap_or_else(|_| fallback())
}

/// 从 Responses SSE 文本里抽出助手回答正文。
pub fn extract_output_text(sse: &str) -> String {
    let mut out = String::new();
    for line in sse.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        let kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) {
                    out.push_str(delta);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    collect_item_text(item, &mut out);
                }
            }
            "response.completed" => {
                if out.trim().is_empty() {
                    if let Some(items) = value
                        .get("response")
                        .and_then(|r| r.get("output"))
                        .and_then(serde_json::Value::as_array)
                    {
                        for item in items {
                            collect_item_text(item, &mut out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn collect_item_text(item: &serde_json::Value, out: &mut String) {
    if item.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return;
    }
    let Some(content) = item.get("content").and_then(serde_json::Value::as_array) else {
        return;
    };
    for part in content {
        let kind = part
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if kind == "output_text" {
            if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                out.push_str(text);
            }
        }
    }
}

/// 截断到 max 个字符（按字符而不是字节，避免把中文切坏）。
pub fn clip(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    trimmed.chars().take(max).collect::<String>() + "…"
}

/// 巡检模型（空则用默认）。
pub fn intel_model(config: &PluginConfig) -> String {
    if config.intel_model.trim().is_empty() {
        DEFAULT_INTEL_MODEL.to_string()
    } else {
        config.intel_model.trim().to_string()
    }
}

/// 巡检问题（空则用默认）。
pub fn intel_question(config: &PluginConfig) -> String {
    if config.intel_prompt.trim().is_empty() {
        DEFAULT_INTEL_PROMPT.to_string()
    } else {
        config.intel_prompt.trim().to_string()
    }
}

/// 降智标记（空则用默认）。
pub fn intel_marker(config: &PluginConfig) -> String {
    if config.intel_fail_marker.trim().is_empty() {
        DEFAULT_INTEL_FAIL_MARKER.to_string()
    } else {
        config.intel_fail_marker.trim().to_string()
    }
}

/// 不合格关键词列表：支持逗号 / 竖线分隔（任一命中即判不合格）。
///
/// 单一关键词（例如 `2024`）容易被模型的措辞绕过：同一个降智号有时会答
/// 「我不知道自己的知识截止日期」，不含 `2024`，于是被误判成「合格」。
/// 因此这里允许一次填多个关键词，例如 `2024,无法确认,没有提供`。
pub fn intel_markers(config: &PluginConfig) -> Vec<String> {
    let raw = config.intel_fail_marker.trim();
    if raw.is_empty() {
        return vec![DEFAULT_INTEL_FAIL_MARKER.to_string()];
    }
    let markers: Vec<String> = raw
        .split([',', '|', ';', '，', '｜', '；'])
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();
    if markers.is_empty() {
        vec![DEFAULT_INTEL_FAIL_MARKER.to_string()]
    } else {
        markers
    }
}

/// 回答里是否命中任一不合格关键词。
pub fn answer_is_unqualified(answer: &str, markers: &[String]) -> bool {
    markers
        .iter()
        .any(|marker| answer.contains(marker.as_str()))
}

/// 回答里是否出现年份证据（4 位数字且以 19/20 开头）。
///
/// 巡检问的就是「你的知识库日期」，正常回答一定带年份；答不出年份的（例如
/// 「好的，我将仅基于已有知识回答」这类敷衍）按不合格处理，避免降智号被误判成合格。
pub fn answer_has_year(answer: &str) -> bool {
    let bytes = answer.as_bytes();
    for start in 0..bytes.len().saturating_sub(3) {
        let window = &bytes[start..start + 4];
        if !window.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let head = &window[..2];
        if head == b"19" || head == b"20" {
            return true;
        }
    }
    false
}

/// 探测失败（网络 / 上游 4xx / 无正文）不是「不合格」，绝不能据此暂停账号。
pub fn qualification_unknown(result: &IntelResult) -> bool {
    !result.error.trim().is_empty()
}

/// 把一次巡检结果折算成通道判定：`Some(ok)` = 明确结论，`None` = 结论不确定（不改通道）。
/// 超时是否算不合格由 `intel_timeout_is_failed` 决定；其余网络类错误一律视为不确定。
pub fn channel_verdict(result: &IntelResult, config: &PluginConfig) -> Option<bool> {
    if !qualification_unknown(result) {
        return Some(result.ok);
    }
    if result.timed_out && config.intel_timeout_is_failed {
        return Some(false);
    }
    None
}

/// 借模板形状 + 目标号凭据组一次巡检请求头。
fn probe_headers(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    donor: &Template,
    target: &AdminTarget,
    ids: &crate::identity::RequestIds,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, values) in &donor.headers {
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        for value in values {
            if let Ok(header_value) = HeaderValue::from_str(value) {
                headers.append(header_name.clone(), header_value);
            }
        }
    }
    if let Some(resolved) =
        crate::identity::resolve_identity(&config.identity, &state.effective_version(config))
    {
        crate::identity::apply_identity_headers(
            &mut headers,
            &resolved,
            &config.identity.residency,
        );
    }
    // 每次巡检独立会话：剥离 borrow 来的 turn-state，重签一套会话/设备 id。
    headers.remove("x-codex-turn-state");
    crate::identity::apply_one_id_headers(&mut headers, ids);
    // 借来的模板身份换成目标号：bearer + chatgpt-account-id。
    let bearer = format!("Bearer {}", target.access_token);
    if let Ok(value) = HeaderValue::from_str(&bearer) {
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    if let Some(account_id) = target.chatgpt_account_id.as_deref() {
        for name in ["chatgpt-account-id", "x-openai-account-id"] {
            if headers.contains_key(name) {
                if let Ok(value) = HeaderValue::from_str(account_id) {
                    headers.insert(name, value);
                }
            }
        }
    }
    headers
}

/// 对单个账号跑一次智力巡检。
pub async fn probe_account(
    state: &Arc<SharedState>,
    donor: &Template,
    target: &AdminTarget,
    config: &PluginConfig,
) -> IntelResult {
    let model = intel_model(config);
    let question = intel_question(config);
    let markers = intel_markers(config);
    let began = Instant::now();
    let mut result = IntelResult {
        model: model.clone(),
        checked_at_ms: now_ms(),
        ..Default::default()
    };

    if target.access_token.trim().is_empty() {
        result.error = "账号无可用 access_token（导出接口未返回）".to_string();
        result.latency_ms = began.elapsed().as_millis() as i64;
        return result;
    }

    let ids = crate::identity::RequestIds::mint();
    let headers = probe_headers(state, config, donor, target, &ids);
    let mut body = build_intel_body(&donor.body, &model, &question);
    if let Some(rewritten) = crate::identity::apply_one_id_body(&body, &ids) {
        body = rewritten;
    }

    let client =
        match state
            .clients
            .client_for(config, target.account_id, target.proxy_url.as_str())
        {
            Ok(client) => client,
            Err(err) => {
                result.error = format!("构建出站客户端失败: {err}");
                result.latency_ms = began.elapsed().as_millis() as i64;
                return result;
            }
        };

    // 单次巡检的超时与重试：超时按不合格处理（可关），因为被限流/降智的号
    // 最常见的表现就是长时间不给回答。
    let timeout = Duration::from_secs(config.intel_prompt_timeout_seconds.clamp(5, 600) as u64);
    let attempts = config.intel_prompt_retries.clamp(0, 5) + 1;
    let mut timed_out = false;
    let mut last_error = String::new();
    let mut received = None;
    for attempt in 1..=attempts {
        if attempt > 1 {
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        let send = client
            .request(reqwest::Method::POST, &donor.url)
            .headers(headers.clone())
            .body(body.clone())
            .send();
        match tokio::time::timeout(timeout, send).await {
            Ok(Ok(response)) => {
                received = Some(response);
                break;
            }
            Ok(Err(err)) => {
                let classified = crate::transport::classify_reqwest_error(&err);
                timed_out = false;
                last_error = format!("请求失败 [{}]: {}", classified.code, classified.message);
            }
            Err(_) => {
                timed_out = true;
                last_error = format!("请求超时（{}s）", timeout.as_secs());
            }
        }
    }
    let Some(response) = received else {
        result.error = if attempts > 1 {
            format!("{last_error}（已尝试 {attempts} 次）")
        } else {
            last_error
        };
        result.timed_out = timed_out;
        if timed_out && config.intel_timeout_is_failed {
            result.ok = false;
        }
        result.latency_ms = began.elapsed().as_millis() as i64;
        return result;
    };

    let status = response.status();
    let mut sse = String::new();
    let read = async {
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
    };
    if tokio::time::timeout(timeout, read).await.is_err() {
        result.error = format!("读取回答超时（{}s）", timeout.as_secs());
        result.timed_out = true;
        if config.intel_timeout_is_failed {
            result.ok = false;
        }
        result.latency_ms = began.elapsed().as_millis() as i64;
        return result;
    }
    result.latency_ms = began.elapsed().as_millis() as i64;

    if !status.is_success() {
        result.error = format!("上游返回 {}: {}", status.as_u16(), clip(&sse, 200));
        return result;
    }

    let answer = extract_output_text(&sse);
    if answer.trim().is_empty() {
        result.error = "上游未返回回答正文".to_string();
        result.answer = clip(&sse, 400);
        return result;
    }
    result.answer = clip(&answer, 400);
    result.ok = !answer_is_unqualified(&answer, &markers);
    if result.ok && config.intel_require_year && !answer_has_year(&answer) {
        // 没有年份证据 = 没回答到「知识库日期」这个点，按降智处理。
        result.ok = false;
    }
    result
}

/// 并发跑一批账号，写入结论，并按需自动暂停/恢复调度。
async fn probe_batch(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    donor: &Template,
    targets: Vec<AdminTarget>,
) -> SweepOutcome {
    let concurrency = config.intel_concurrency.clamp(1, 16) as usize;
    let auto_pause = config.intel_auto_pause;
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    let degrade_path = config.degrade_state_file();

    let futures = targets.into_iter().map(|target| {
        let state = Arc::clone(state);
        async move {
            let result = probe_account(&state, donor, &target, config).await;
            (target, result)
        }
    });
    let mut stream = futures_util::stream::iter(futures).buffer_unordered(concurrency);

    let mut outcome = SweepOutcome::default();
    while let Some((target, result)) = stream.next().await {
        state.intel.set_result(target.account_id, result.clone());
        outcome.total += 1;
        if qualification_unknown(&result) {
            outcome.failed += 1;
        } else if result.ok {
            outcome.qualified += 1;
        } else {
            outcome.unqualified += 1;
        }
        state.intel.progress(outcome.total);

        // 通道判定：不确定结论不改变当前通道；超时按不合格处理（可配）。
        let verdict = channel_verdict(&result, config);
        let has_verdict = verdict.is_some();
        let effective_ok = verdict.unwrap_or(false);
        state
            .degrade
            .apply_verdict(target.account_id, verdict, config, &degrade_path);

        if auto_pause && has_verdict && !base.is_empty() && !key.is_empty() {
            // 勾了「自动降智处理」的号：不暂停，改由降智通道兜住这两个模型。
            if !effective_ok && state.degrade.is_enabled(target.account_id) {
                outcome.degrade_handled += 1;
                continue;
            }
            let want = effective_ok;
            if want != target.schedulable {
                match admin::set_account_schedulable(&base, &key, target.account_id, want).await {
                    Ok(()) => {
                        if want {
                            outcome.resumed += 1;
                        } else {
                            outcome.paused += 1;
                        }
                    }
                    Err(err) => {
                        outcome.note = format!("#{} 切换调度失败: {err}", target.account_id);
                    }
                }
            }
        }
    }
    state.degrade.persist(&degrade_path);
    outcome
}

/// 跑一批巡检：枚举账号 → 按套餐过滤 → 逐个探测 → 可选自动暂停/恢复。
pub async fn run_sweep(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    only_ids: Option<Vec<i64>>,
) -> Result<SweepOutcome, String> {
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    if base.is_empty() || key.is_empty() {
        return Err("未配置 Sub2API 管理 API（admin_api_base / admin_api_key）".to_string());
    }
    if !state.intel.begin(0) {
        return Err("已有巡检在进行中".to_string());
    }
    let result = run_sweep_inner(state, config, only_ids).await;
    match result {
        Ok(outcome) => {
            state.intel.finish(format!(
                "完成：合格 {} / 不合格 {} / 失败 {} / 暂停 {} / 恢复 {} / 走降智通道 {}",
                outcome.qualified,
                outcome.unqualified,
                outcome.failed,
                outcome.paused,
                outcome.resumed,
                outcome.degrade_handled
            ));
            Ok(outcome)
        }
        Err(err) => {
            state.intel.finish(format!("失败：{err}"));
            Err(err)
        }
    }
}

async fn run_sweep_inner(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    only_ids: Option<Vec<i64>>,
) -> Result<SweepOutcome, String> {
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    state.degrade.ensure_loaded(&config.degrade_state_file());
    let all = admin::fetch_all_targets(&base, &key).await?;
    let wanted: Option<std::collections::HashSet<i64>> = match only_ids {
        Some(ids) if !ids.is_empty() => {
            Some(ids.into_iter().collect::<std::collections::HashSet<i64>>())
        }
        _ => None,
    };
    let plan_filter = config.intel_plan_filter();

    let targets: Vec<AdminTarget> = all
        .into_iter()
        .filter(|target| match &wanted {
            Some(set) => set.contains(&target.account_id),
            None => true,
        })
        .filter(|target| {
            plan_filter.is_empty()
                || plan_filter
                    .iter()
                    .any(|want| *want == normalize_plan_type(&target.plan_type))
        })
        .filter(|target| !target.access_token.trim().is_empty())
        .collect();

    if targets.is_empty() {
        return Err("没有匹配的账号（检查套餐筛选 / 管理 API）".to_string());
    }
    let Some(donor) = state.template.any_recent() else {
        return Err(
            "尚无真实 Codex 流量模板可用：请先让一条真实业务请求经过插件，再跑巡检".to_string(),
        );
    };

    state.intel.set_total(targets.len());
    let outcome = probe_batch(state, config, &donor, targets).await;
    state.intel.persist(&config.intel_state_path);
    Ok(outcome)
}

/// 自动巡检循环：按 intel_loop_interval_seconds 周期跑一批全量巡检。
pub async fn intel_loop(state: Arc<SharedState>) {
    let mut last_started_ms: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let config = state.current_config();
        if !config.intel_enabled || !config.intel_loop_enabled {
            continue;
        }
        if config.admin_api_base.trim().is_empty() || config.admin_api_key.trim().is_empty() {
            continue;
        }
        let interval_ms = config.intel_loop_interval_seconds.clamp(60, 86_400) as u64 * 1000;
        let now = now_ms();
        if last_started_ms != 0 && now.saturating_sub(last_started_ms) < interval_ms {
            continue;
        }
        last_started_ms = now;
        match run_sweep(&state, &config, None).await {
            Ok(outcome) => eprintln!(
                "[codex-native-transport] intel sweep done: total={} ok={} bad={} failed={} paused={} resumed={} degrade={}",
                outcome.total,
                outcome.qualified,
                outcome.unqualified,
                outcome.failed,
                outcome.paused,
                outcome.resumed,
                outcome.degrade_handled
            ),
            Err(err) => eprintln!("[codex-native-transport] intel sweep skipped: {err}"),
        }
    }
}

/// 面板用：账号 + 上一次巡检结论 + 降智处理开关 的合并视图。
pub fn merge_rows(
    targets: &[AdminTarget],
    results: &BTreeMap<i64, IntelResult>,
    channels: &BTreeMap<i64, ChannelState>,
    config: &PluginConfig,
    now: u64,
) -> Vec<serde_json::Value> {
    let mut rows = Vec::with_capacity(targets.len());
    for target in targets {
        let result = results.get(&target.account_id);
        let state = channels
            .get(&target.account_id)
            .cloned()
            .unwrap_or_default();
        let decision = channel_decision(&state, config, now);
        let state_label = match result {
            Some(r) if !r.error.trim().is_empty() => "error",
            Some(r) if r.ok => "good",
            Some(_) => "bad",
            None => "unknown",
        };
        rows.push(serde_json::json!({
            "id": target.account_id,
            "name": target.name,
            "plan_type": target.plan_type,
            "status": target.status,
            "schedulable": target.schedulable,
            "intel": state_label,
            "answer": result.map(|r| r.answer.clone()).unwrap_or_default(),
            "error": result.map(|r| r.error.clone()).unwrap_or_default(),
            "latency_ms": result.map(|r| r.latency_ms).unwrap_or(0),
            "checked_at_ms": result.map(|r| r.checked_at_ms).unwrap_or(0),
            "model": result.map(|r| r.model.clone()).unwrap_or_default(),
            "timed_out": result.map(|r| r.timed_out).unwrap_or(false),
            "degrade": state.enabled,
            "channel": if !state.enabled {
                "off"
            } else if decision.use_bps {
                "bps"
            } else {
                "normal"
            },
            "channel_reason": decision.reason,
            "hold_remaining_ms": decision.hold_remaining_ms,
            "healthy_streak": state.healthy_streak,
            "confirmations": decision.confirmations,
            "bps_since_ms": state.bps_since_ms,
            "last_reason": state.last_reason,
        }));
    }
    rows
}

/// 面板用：把账号按套餐类型归类计数（套餐筛选下拉的数据源）。
pub fn plan_type_counts(targets: &[AdminTarget]) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for target in targets {
        let key = normalize_plan_type(&target.plan_type);
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intel_body_rewrites_model_and_question_only() {
        let tpl = br#"{"model":"gpt-5.4","instructions":"You are Codex.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"long"}]}],"stream":false,"store":true,"reasoning":{"effort":"high"},"previous_response_id":"resp_1"}"#;
        let body = build_intel_body(tpl, "gpt-6-astra", "你在知识库日期 不允许联网快速回答");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["model"], "gpt-6-astra");
        assert_eq!(
            v["input"][0]["content"][0]["text"],
            "你在知识库日期 不允许联网快速回答"
        );
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        assert!(v.get("previous_response_id").is_none());
        // 其余形状字段原样保留。
        assert_eq!(v["instructions"], "You are Codex.");
        assert_eq!(v["reasoning"]["effort"], "high");
    }

    #[test]
    fn intel_body_falls_back_on_garbage_template() {
        for tpl in [b"not json".as_slice(), br#"{"model":"x"}"#.as_slice()] {
            let body = build_intel_body(tpl, "gpt-6-astra", "q");
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["model"], "gpt-6-astra");
            assert_eq!(v["input"][0]["content"][0]["text"], "q");
        }
    }

    #[test]
    fn extract_output_text_reads_deltas_and_final_items() {
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"知识库\"}\n\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"截止 2024 年\"}\n\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert_eq!(extract_output_text(sse), "知识库截止 2024 年");

        let only_done = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"2025 年\"}]}}\n\n";
        assert_eq!(extract_output_text(only_done), "2025 年");
    }

    #[test]
    fn probe_failures_are_not_treated_as_unqualified() {
        let transport_failure = IntelResult {
            ok: false,
            error: "上游返回 429".to_string(),
            ..Default::default()
        };
        assert!(qualification_unknown(&transport_failure));
        let really_degraded = IntelResult {
            ok: false,
            answer: "2024".to_string(),
            ..Default::default()
        };
        assert!(!qualification_unknown(&really_degraded));
    }

    #[test]
    fn clip_keeps_whole_characters() {
        assert_eq!(clip("  你好世界  ", 2), "你好…");
        assert_eq!(clip("你好", 5), "你好");
    }

    #[test]
    fn degrade_store_round_trips() {
        let store = DegradeStore::default();
        store.ensure_loaded("");
        assert!(!store.is_enabled(7));
        store.set(7, true, "");
        assert!(store.is_enabled(7));
        store.set(7, false, "");
        assert!(!store.is_enabled(7));
    }

    fn channel_test_config() -> PluginConfig {
        let mut config = PluginConfig::default();
        config.intel_confirmations = 2;
        config.bps_hold_after_healthy_seconds = 60;
        config.bps_session_sticky_seconds = 1800;
        config.bps_fallback_cooldown_seconds = 120;
        config.intel_timeout_is_failed = true;
        config
    }

    #[test]
    fn degrade_requires_confirmations_then_holds_before_normal() {
        let config = channel_test_config();
        let store = DegradeStore::default();
        store.ensure_loaded("");
        store.set(9, true, "");
        let now = now_ms();

        // 刚开启：还没巡检结论，先按 BPS 兜住。
        let d = store.decision(9, &config, now);
        assert!(d.use_bps, "{}", d.reason);
        assert!(d.reason.contains("合格确认中 0/2"), "{}", d.reason);

        // 第 1 次合格：还没到确认次数，继续走 BPS。
        store.apply_verdict(9, Some(true), &config, "");
        let d = store.decision(9, &config, now);
        assert!(d.use_bps, "{}", d.reason);
        assert_eq!(d.healthy_streak, 1);
        assert!(d.reason.contains("合格确认中 1/2"), "{}", d.reason);

        // 第 2 次合格：确认够了，但仍要等「恢复保持期」。
        store.apply_verdict(9, Some(true), &config, "");
        let d = store.decision(9, &config, now);
        assert!(d.use_bps, "{}", d.reason);
        assert!(d.reason.contains("保持期"), "{}", d.reason);
        assert!(d.hold_remaining_ms > 0);

        // 保持期结束：切回正常通道，并且是稳定结论。
        let d = store.decision(9, &config, now + 61_000);
        assert!(!d.use_bps, "{}", d.reason);
        assert!(d.reason.contains("正常通道"), "{}", d.reason);

        // 再来一次不确定结论：通道不变。
        store.apply_verdict(9, None, &config, "");
        let d = store.decision(9, &config, now + 61_000);
        assert!(!d.use_bps, "{}", d.reason);

        // 中途一次不合格：立刻切回 BPS，并把连续合格清零。
        store.apply_verdict(9, Some(false), &config, "");
        let d = store.decision(9, &config, now + 61_000);
        assert!(d.use_bps, "{}", d.reason);
        assert_eq!(d.healthy_streak, 0);
        assert!(d.reason.contains("不合格"), "{}", d.reason);
    }

    #[test]
    fn degrade_hold_zero_switches_back_immediately() {
        let mut config = channel_test_config();
        config.intel_confirmations = 1;
        config.bps_hold_after_healthy_seconds = 0;
        let store = DegradeStore::default();
        store.ensure_loaded("");
        store.set(11, true, "");
        store.apply_verdict(11, Some(true), &config, "");
        let d = store.decision(11, &config, now_ms());
        assert!(!d.use_bps, "{}", d.reason);
        assert_eq!(d.hold_remaining_ms, 0);
    }

    #[test]
    fn degrade_store_accepts_legacy_bool_state() {
        let mut path = std::env::temp_dir();
        path.push(format!("cnt-degrade-legacy-{}.json", std::process::id()));
        let path_str = path.to_string_lossy().to_string();
        std::fs::write(&path, br#"{"54":true,"55":false}"#).unwrap();

        let store = DegradeStore::default();
        store.ensure_loaded(&path_str);
        assert!(store.is_enabled(54));
        assert!(!store.is_enabled(55));
        // 旧格式的 `true` 视为「已被判不合格」，先走 BPS 兜住。
        let states = store.snapshot();
        assert!(states.get(&54).unwrap().failed);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn year_evidence_requires_a_year_in_the_answer() {
        assert!(answer_has_year("我的知识库更新至 2024 年 6 月"));
        assert!(answer_has_year("截止到 2025-06"));
        assert!(!answer_has_year(
            "好的。我将仅基于已有知识回答，不联网，并尽量简洁。"
        ));
        assert!(!answer_has_year("我无法确认"));
    }

    #[test]
    fn fail_marker_accepts_multiple_keywords() {
        let mut config = PluginConfig::default();
        config.intel_fail_marker = "2024, 无法确认｜没有提供".to_string();
        let markers = intel_markers(&config);
        assert_eq!(markers, vec!["2024", "无法确认", "没有提供"]);
        assert!(answer_is_unqualified(
            "我的知识库更新至 2024 年 6 月",
            &markers
        ));
        assert!(answer_is_unqualified(
            "当前会话没有提供我的确切知识截止日期",
            &markers
        ));
        assert!(!answer_is_unqualified("2025 年 6 月", &markers));

        config.intel_fail_marker = "   ".to_string();
        assert_eq!(intel_markers(&config), vec!["2024".to_string()]);
    }

    #[test]
    fn channel_verdict_handles_timeout_and_unknown_errors() {
        let mut config = channel_test_config();

        let timed_out = IntelResult {
            error: "请求超时（45s）".to_string(),
            timed_out: true,
            ..Default::default()
        };
        assert_eq!(channel_verdict(&timed_out, &config), Some(false));

        config.intel_timeout_is_failed = false;
        assert_eq!(channel_verdict(&timed_out, &config), None);
        config.intel_timeout_is_failed = true;

        let network_error = IntelResult {
            error: "请求失败 [PLUGIN_UPSTREAM_CONNECT]: tunnel failed".to_string(),
            ..Default::default()
        };
        assert_eq!(channel_verdict(&network_error, &config), None);

        let healthy = IntelResult {
            ok: true,
            answer: "2025 年".to_string(),
            ..Default::default()
        };
        assert_eq!(channel_verdict(&healthy, &config), Some(true));

        let degraded = IntelResult {
            ok: false,
            answer: "2024 年".to_string(),
            ..Default::default()
        };
        assert_eq!(channel_verdict(&degraded, &config), Some(false));
    }
}
