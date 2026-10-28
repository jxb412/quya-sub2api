//! 奥特曼变阻器（Altman Resistor）：对外 usage 计量覆盖。
//!
//! 移植自 C2A V41 的 `altman_resistor`（`backend-go/internal/billing/altman_resistor.go`），
//! 语义保持一致：**只改回给宿主的 usage 计量，不改上游请求、不改模型输出、不碰真实缓存**。
//!
//! 规则（给定百分比 P，0–99）：
//! - 缓存读取 = `floor(总输入 × P / 100)`；
//! - 剩余输入 = `总输入 - 缓存读取`，其中先保留原有的缓存写入桶（若上游给了这个键），
//!   其余算普通输入；
//! - **所有输入桶相加仍等于原总输入**，`total_tokens` 不变；
//! - 原始 usage 无效（缺总输入）、数量不安全（负数 / 溢出）或明细互相矛盾
//!   （缓存读取 + 缓存写入 > 总输入）时**不改**，绝不伪造新的计量。
//!
//! 为什么放在回程：插件夹在上游和宿主之间，宿主按我们回给它的 usage 计量与计费，
//! 所以这里是唯一能统一覆盖 Responses / Chat、流式 / 非流式的收口点。

use serde_json::{Map, Value};

/// 百分比上限。100 及以上等于「全额缓存」，没有调整意义，也容易掩盖真实用量。
pub const MAX_PERCENT: u8 = 99;

/// 总输入字段（Responses 用 `input_tokens`，Chat Completions 用 `prompt_tokens`）。
const TOTAL_KEYS: [&str; 2] = ["input_tokens", "prompt_tokens"];

/// 明细对象键（与总输入字段一一对应）。
const DETAIL_KEYS: [&str; 2] = ["input_tokens_details", "prompt_tokens_details"];

/// 缓存读取键。
const CACHED_KEY: &str = "cached_tokens";

/// 网关/Anthropic 式「缓存写入」键，出现了才保留。
const CACHE_WRITE_KEYS: [&str; 2] = ["cache_write_tokens", "cache_creation_tokens"];

/// 百分比换算：`floor(total × percent / 100)`，用 u128 中间量避免乘法溢出。
fn percent_of(total: u64, percent: u8) -> u64 {
    let percent = percent.min(MAX_PERCENT) as u128;
    ((total as u128 * percent) / 100) as u64
}

fn read_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            // 上游偶尔回浮点（例如 17634.0），整数值照收，其余当无效。
            number
                .as_f64()
                .filter(|raw| {
                    raw.is_finite() && *raw >= 0.0 && raw.fract() == 0.0 && *raw <= u64::MAX as f64
                })
                .map(|raw| raw as u64)
        }),
        _ => None,
    }
}

/// 对单个 usage 对象应用变阻器。返回是否真的改动过。
///
/// `usage` 必须是 usage 本体（`{"input_tokens":…,"input_tokens_details":{…}}`）。
pub fn apply(usage: &mut Map<String, Value>, percent: u8) -> bool {
    // 1. 总输入：两种命名都认，缺了就不动（无法安全重算）。
    let Some(total) = TOTAL_KEYS
        .iter()
        .find_map(|key| usage.get(*key).and_then(read_u64))
    else {
        return false;
    };
    if total == 0 {
        return false;
    }

    // 2. 找到与总输入同名的明细对象；没有就按需新建（上游没开缓存时不会有）。
    let detail_key = DETAIL_KEYS
        .iter()
        .copied()
        .find(|key| usage.contains_key(*key))
        .unwrap_or(if usage.contains_key("prompt_tokens") {
            "prompt_tokens_details"
        } else {
            "input_tokens_details"
        });
    if !usage.get(detail_key).map(Value::is_object).unwrap_or(false) {
        usage.insert(detail_key.to_string(), Value::Object(Map::new()));
    }
    let Some(details) = usage.get_mut(detail_key).and_then(Value::as_object_mut) else {
        return false;
    };

    let cached_before = details.get(CACHED_KEY).and_then(read_u64).unwrap_or(0);
    let write_key = CACHE_WRITE_KEYS
        .iter()
        .copied()
        .find(|key| details.contains_key(*key));
    let write_before = write_key
        .and_then(|key| details.get(key))
        .and_then(read_u64)
        .unwrap_or(0);

    // 3. 明细自相矛盾（读取 + 写入超过总输入）或单个桶超过总量：不伪造，原样放行。
    if cached_before > total
        || write_before > total
        || cached_before.saturating_add(write_before) > total
    {
        return false;
    }

    // 4. 重算：缓存读取按百分比，剩余输入先补缓存写入，再算普通输入。
    let cached_after = percent_of(total, percent);
    let remaining = total.saturating_sub(cached_after);
    let write_after = write_before.min(remaining);

    let mut modified = false;
    if cached_before != cached_after {
        details.insert(CACHED_KEY.to_string(), Value::from(cached_after));
        modified = true;
    }
    if let Some(key) = write_key {
        if write_before != write_after {
            details.insert(key.to_string(), Value::from(write_after));
            modified = true;
        }
    }

    // `input_tokens` / `prompt_tokens`（总输入）与 `total_tokens` 一律保持原值：
    // 变阻器只重分配输入桶，不改变这次请求真实消耗的总量。
    modified
}

/// 对 Responses 形状的对象应用变阻器：改它内部的 `usage`。
pub fn apply_in_response(response: &mut Map<String, Value>, percent: u8) -> bool {
    match response.get_mut("usage").and_then(Value::as_object_mut) {
        Some(usage) => apply(usage, percent),
        None => false,
    }
}

/// 百分比是否在可用区间（0..=99）。
pub fn is_valid_percent(percent: u8) -> bool {
    percent <= MAX_PERCENT
}

/// 正常 Codex 通道的响应流改写器。
///
/// BPS 通道本来就有 `BpsStream` 做逐帧改写，变阻器可以搭车；正常通道是原样透传，
/// 所以这里单独给一层**只在开启时挂载**的窄过滤器：按 `\n\n` 分帧，只改带 `usage`
/// 的 JSON 帧，其余字节原样转发。关闭时这条路径完全不被创建，行为与改动前一致。
#[derive(Default)]
pub struct FrameStream {
    percent: Option<u8>,
    buf: Vec<u8>,
}

impl FrameStream {
    pub fn new(percent: u8) -> Self {
        Self {
            percent: Some(percent.min(MAX_PERCENT)),
            buf: Vec::new(),
        }
    }

    /// 送入一段上游字节，返回应转发给宿主的字节（可能为空）。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = find_frame_end(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..end).collect();
            self.emit(&frame, &mut out);
        }
        out
    }

    /// 流结束：把残留的不完整帧（非流式整段 JSON 就走这里）也处理掉。
    pub fn finish(&mut self) -> Vec<u8> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let frame = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        self.emit(&frame, &mut out);
        out
    }

    fn emit(&self, frame: &[u8], out: &mut Vec<u8>) {
        let Some(percent) = self.percent else {
            out.extend_from_slice(frame);
            return;
        };
        let text = String::from_utf8_lossy(frame);
        match rewrite_frame_text(&text, percent) {
            Some(rewritten) => out.extend_from_slice(rewritten.as_bytes()),
            None => out.extend_from_slice(frame),
        }
    }
}

/// 找 `\n\n` 分帧边界（含分隔符本身）。
fn find_frame_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2)
        .position(|pair| pair == b"\n\n")
        .map(|pos| pos + 2)
}

/// 改写一帧里的 usage；没有 usage 或没改动时返回 None（调用方原样转发）。
fn rewrite_frame_text(text: &str, percent: u8) -> Option<String> {
    let mut changed_any = false;
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.trim();
            if payload.starts_with('{') {
                if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(payload) {
                    let mut changed = false;
                    if let Some(response) = value
                        .get_mut("response")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        changed |= apply_in_response(response, percent);
                    }
                    if let Some(usage) = value
                        .get_mut("usage")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        changed |= apply(usage, percent);
                    }
                    if changed {
                        if let Ok(rewritten) = serde_json::to_string(&value) {
                            lines.push(format!("data: {rewritten}"));
                            changed_any = true;
                            continue;
                        }
                    }
                }
            }
        }
        lines.push(line.to_string());
    }
    // 非 SSE 的整段 JSON 体（没有 data: 行）也要覆盖。
    if !changed_any && !text.contains("data:") {
        let trimmed = text.trim();
        if trimmed.starts_with('{') {
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(trimmed) {
                let mut changed = false;
                if let Some(response) = value
                    .get_mut("response")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    changed |= apply_in_response(response, percent);
                }
                if let Some(usage) = value
                    .get_mut("usage")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    changed |= apply(usage, percent);
                }
                if changed {
                    return serde_json::to_string(&value).ok();
                }
            }
        }
    }
    if changed_any {
        Some(lines.join("\n"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn ninety_percent_of_thousand() {
        let mut u = usage(json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 100},
            "total_tokens": 1200,
        }));
        assert!(apply(&mut u, 90));
        assert_eq!(u["input_tokens"], 1000, "总输入不能变");
        assert_eq!(u["total_tokens"], 1200, "总 token 不能变");
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 900);
    }

    #[test]
    fn zero_percent_clears_cache_read() {
        let mut u = usage(json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 900},
        }));
        assert!(apply(&mut u, 0));
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 0);
    }

    #[test]
    fn ninety_nine_percent_rounds_down_on_small_inputs() {
        let mut u = usage(json!({
            "input_tokens": 19,
            "input_tokens_details": {"cached_tokens": 0},
        }));
        assert!(apply(&mut u, 99));
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 18);
    }

    #[test]
    fn cache_write_bucket_is_preserved_within_remaining_input() {
        let mut u = usage(json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 10, "cache_write_tokens": 500},
        }));
        assert!(apply(&mut u, 90));
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 900);
        // 剩余 100 装不下 500，写入桶被压到 100，三个桶相加 = 1000。
        assert_eq!(u["input_tokens_details"]["cache_write_tokens"], 100);
        let details = &u["input_tokens_details"];
        assert_eq!(
            details["cached_tokens"].as_u64().unwrap()
                + details["cache_write_tokens"].as_u64().unwrap(),
            1000
        );
    }

    #[test]
    fn chat_completions_shape_uses_prompt_tokens() {
        let mut u = usage(json!({
            "prompt_tokens": 2000,
            "completion_tokens": 30,
            "total_tokens": 2030,
            "prompt_tokens_details": {"cached_tokens": 0},
        }));
        assert!(apply(&mut u, 50));
        assert_eq!(u["prompt_tokens"], 2000);
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 1000);
        assert_eq!(u["total_tokens"], 2030);
    }

    #[test]
    fn contradictory_details_are_left_untouched() {
        let mut u = usage(json!({
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 200},
        }));
        assert!(!apply(&mut u, 90));
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 200);
    }

    #[test]
    fn missing_total_is_left_untouched() {
        let mut u = usage(json!({"input_tokens_details": {"cached_tokens": 5}}));
        assert!(!apply(&mut u, 90));
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 5);
    }

    #[test]
    fn details_object_is_created_when_upstream_omits_it() {
        let mut u = usage(json!({"input_tokens": 800}));
        assert!(apply(&mut u, 25));
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 200);
    }

    #[test]
    fn idempotent_on_second_pass() {
        let mut u = usage(json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 300},
        }));
        assert!(apply(&mut u, 90));
        assert!(!apply(&mut u, 90), "已经是目标值就不该再算改动");
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 900);
    }

    #[test]
    fn response_wrapper_only_touches_usage() {
        let mut response = usage(json!({
            "id": "resp_1",
            "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 0}},
        }));
        assert!(apply_in_response(&mut response, 60));
        assert_eq!(response["id"], "resp_1");
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            600
        );
        let mut bare = usage(json!({"id": "resp_2"}));
        assert!(!apply_in_response(&mut bare, 60));
    }

    #[test]
    fn percent_bounds() {
        assert!(is_valid_percent(0));
        assert!(is_valid_percent(99));
        assert!(!is_valid_percent(100));
    }

    #[test]
    fn frame_stream_rewrites_usage_frames_and_passes_others_through() {
        let mut stream = FrameStream::new(90);
        let first = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":1000,\"input_tokens_details\":{\"cached_tokens\":0}}}}\n\n";
        let second = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
        let mut payload = Vec::new();
        payload.extend(stream.push(first.as_bytes()));
        payload.extend(stream.push(second.as_bytes()));
        payload.extend(stream.finish());
        let text = String::from_utf8(payload).unwrap();
        assert!(
            text.contains("\"cached_tokens\":900"),
            "缓存读取应被改写: {text}"
        );
        assert!(
            text.contains("event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}"),
            "无 usage 的帧必须原样透传"
        );
    }

    #[test]
    fn frame_stream_handles_split_and_non_stream_bodies() {
        let mut stream = FrameStream::new(50);
        let body = "{\"id\":\"resp_2\",\"usage\":{\"input_tokens\":800,\"input_tokens_details\":{\"cached_tokens\":10}}}";
        // 逐字节喂入，模拟上游分块
        let mut payload = Vec::new();
        for chunk in body.as_bytes().chunks(7) {
            payload.extend(stream.push(chunk));
        }
        payload.extend(stream.finish());
        let text = String::from_utf8(payload).unwrap();
        let value: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["usage"]["input_tokens_details"]["cached_tokens"], 400);
        assert_eq!(value["usage"]["input_tokens"], 800);
    }

    #[test]
    fn frame_stream_leaves_unparseable_bytes_untouched() {
        let mut stream = FrameStream::new(90);
        let raw = b": keepalive\n\n";
        let mut payload = stream.push(raw);
        payload.extend(stream.finish());
        assert_eq!(payload, raw.to_vec());
    }
}
