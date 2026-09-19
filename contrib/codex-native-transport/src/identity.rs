//! 出站身份 Profile：在插件边缘（真正写向网络之前）统一改写客户端身份。
//!
//! Sub2API 宿主已经做了一层身份收口（enforceCodexIdentityHeaders）。本模块
//! 是最后一道边缘改写，用于：
//! - 选择身份形态（codex_cli / codex_desktop / opencode / pi / custom）；
//! - 版本号自动同步（npm registry @openai/codex），UA 与 version 头同源改写；
//! - 可选 residency 头（x-openai-internal-codex-residency）；
//! - 可选按账号隔离 x-codex-installation-id（头 + body client_metadata +
//!   内嵌 x-codex-turn-metadata 三处一致改写，避免身份分裂）。
//!
//! profile = "passthrough"（默认）时完全不动宿主给出的请求。

use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::config::IdentityConfig;

pub const RESIDENCY_HEADER: &str = "x-openai-internal-codex-residency";
const MACHINE_PSEUDONYM_PREFIX: &str = "quya:codex-machine:v1:";
const MACHINE_ACCOUNT_KEY_PREFIX: &str = "quya:codex-machine-account:v1:";
const INSTALLATION_ID_PREFIX: &str = "quya:codex-installation:v2:";

type HmacSha256 = Hmac<Sha256>;

/// 各 Profile 的默认 originator 与 UA 模板。
/// 模板占位符：{version} {os} {terminal}
/// codex_cli 的形态取自 codex-rs get_codex_user_agent()：
///   {originator}/{version} ({os_type} {os_version}; {arch}) {terminal}
/// 其余 Profile 为可编辑预设——上线前建议用真实客户端抓包校准 UA 形态。
pub fn profile_preset(profile: &str) -> Option<(&'static str, &'static str)> {
    match profile {
        "codex_cli" => Some(("codex_cli_rs", "codex_cli_rs/{version} ({os}) {terminal}")),
        "codex_desktop" => Some((
            "codex_chatgpt_desktop",
            "codex_chatgpt_desktop/{version} ({os})",
        )),
        "opencode" => Some(("opencode", "opencode/{version} ({os})")),
        "pi" => Some(("pi", "pi/{version} ({os})")),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedIdentity {
    pub originator: String,
    pub user_agent: String,
    pub version: String,
}

/// 由配置 + 当前生效版本号解析出完整出站身份。
pub fn resolve_identity(config: &IdentityConfig, version: &str) -> Option<ResolvedIdentity> {
    let (originator, template): (String, String) = match config.profile.as_str() {
        "passthrough" => return None,
        "custom" => {
            let originator = config.custom_originator.trim();
            let template = config.custom_user_agent_template.trim();
            if originator.is_empty() || template.is_empty() {
                return None;
            }
            (originator.to_string(), template.to_string())
        }
        other => {
            let (originator, template) = profile_preset(other)?;
            (originator.to_string(), template.to_string())
        }
    };

    let user_agent = template
        .replace("{version}", version)
        .replace("{os}", config.os_segment.trim())
        .replace("{terminal}", config.terminal_segment.trim());
    // 折叠模板变量为空时残留的双空格。
    let user_agent = user_agent.split_whitespace().collect::<Vec<_>>().join(" ");

    Some(ResolvedIdentity {
        originator,
        user_agent,
        version: version.to_string(),
    })
}

/// 对出站请求头应用身份改写。只对携带 originator 的请求生效
/// （与宿主 enforceCodexIdentityHeaders 的判定一致：compat 桥接等
/// 非 ChatGPT 内部接口路径已显式删除 originator，不应被补回）。
pub fn apply_identity_headers(
    headers: &mut HeaderMap,
    identity: &ResolvedIdentity,
    residency: &str,
) {
    if !headers.contains_key("originator") {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(&identity.originator) {
        headers.insert("originator", value);
    }
    if let Ok(value) = HeaderValue::from_str(&identity.user_agent) {
        headers.insert("user-agent", value);
    }
    if headers.contains_key("version") {
        if let Ok(value) = HeaderValue::from_str(&identity.version) {
            headers.insert("version", value);
        }
    }
    let residency = residency.trim();
    if !residency.is_empty() && !headers.contains_key(RESIDENCY_HEADER) {
        if let Ok(value) = HeaderValue::from_str(residency) {
            headers.insert(RESIDENCY_HEADER, value);
        }
    }
}

fn hmac_digest(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

fn uuid_from_digest(digest: &[u8; 32], version7: bool, timestamp: Option<&[u8; 6]>) -> String {
    let mut bytes = [0u8; 16];
    if version7 {
        if let Some(timestamp) = timestamp {
            bytes[..6].copy_from_slice(timestamp);
        }
        bytes[6..].copy_from_slice(&digest[..10]);
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
    } else {
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
    }
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

/// 按账号派生稳定的 installation id（UUIDv4 形态，同账号恒定）。
pub fn per_account_installation_id(seed: &str, account_id: i64) -> String {
    let message = format!("{INSTALLATION_ID_PREFIX}{account_id}");
    uuid_from_digest(
        &hmac_digest(seed.as_bytes(), message.as_bytes()),
        false,
        None,
    )
}

/// 改写请求头中的 installation id（含 x-codex-turn-metadata JSON 内嵌字段）。
///
/// 重要：只改写、绝不插入。真实 codex 0.153.4 的主流式请求（POST /responses）
/// 不携带 x-codex-installation-id 头（该 id 只出现在 x-codex-turn-metadata 头 JSON
/// 与 body 的 client_metadata 中；仅 compact 等少数端点才作为独立头发送）。
/// 无条件插入会给请求添加真实客户端没有的特征。
pub fn apply_installation_id_headers(headers: &mut HeaderMap, installation_id: &str) {
    if headers.contains_key("x-codex-installation-id") {
        if let Ok(value) = HeaderValue::from_str(installation_id) {
            headers.insert("x-codex-installation-id", value);
        }
    }
    if let Some(existing) = headers.get("x-codex-turn-metadata").cloned() {
        if let Ok(raw) = existing.to_str() {
            if let Some(rewritten) = rewrite_json_field(raw, "installation_id", installation_id) {
                if let Ok(value) = HeaderValue::from_str(&rewritten) {
                    headers.insert("x-codex-turn-metadata", value);
                }
            }
        }
    }
}

/// 改写 JSON 请求体中的 client_metadata 身份字段，与头保持一致。
/// body 不是 JSON 对象或没有 client_metadata 时原样返回。
pub fn apply_installation_id_body(body: &[u8], installation_id: &str) -> Option<Vec<u8>> {
    let mut root: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = root.as_object_mut()?;
    let metadata = object.get_mut("client_metadata")?.as_object_mut()?;

    let mut modified = false;
    if metadata.contains_key("x-codex-installation-id") {
        metadata.insert(
            "x-codex-installation-id".to_string(),
            serde_json::Value::String(installation_id.to_string()),
        );
        modified = true;
    }
    if let Some(serde_json::Value::String(embedded)) = metadata.get("x-codex-turn-metadata") {
        if let Some(rewritten) = rewrite_json_field(embedded, "installation_id", installation_id) {
            metadata.insert(
                "x-codex-turn-metadata".to_string(),
                serde_json::Value::String(rewritten),
            );
            modified = true;
        }
    }
    if !modified {
        return None;
    }
    serde_json::to_vec(&root).ok()
}

fn rewrite_json_field(raw: &str, field: &str, value: &str) -> Option<String> {
    let mut parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = parsed.as_object_mut()?;
    if !object.contains_key(field) {
        return None;
    }
    object.insert(
        field.to_string(),
        serde_json::Value::String(value.to_string()),
    );
    serde_json::to_string(&parsed).ok().map(ascii_escape_json)
}

/// 与官方 to_ascii_json_string（utils/string/src/json.rs 的 AsciiJsonFormatter）等价：
/// 非 ASCII 字符转义为小写 \uXXXX（UTF-16 码元，含代理对）。turn-metadata 头 JSON
/// 在真实客户端里就是这种形态，改写后必须保持一致。
/// 说明：serde_json 输出中非 ASCII 字符只可能出现在字符串字面量内，逐字符转义是安全的。
fn ascii_escape_json(serialized: String) -> String {
    if serialized.is_ascii() {
        return serialized;
    }
    let mut out = String::with_capacity(serialized.len() + 16);
    let mut utf16 = [0u16; 2];
    for ch in serialized.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            for code_unit in ch.encode_utf16(&mut utf16) {
                out.push_str(&format!("\\u{code_unit:04x}"));
            }
        }
    }
    out
}

/// 判定是否为 ChatGPT Codex 内部接口请求（身份改写只作用于该面）。
pub fn is_codex_backend_request(url: &str) -> bool {
    url.contains("/backend-api/codex")
}

// ---------------------------------------------------------------------------
// machine：一账号一设备、下游会话 1:1 假名化
// ---------------------------------------------------------------------------

/// machine 模式只改变已经存在的身份字段：账号拥有恒定 installation id，
/// session/thread/window/prompt_cache_key 使用同一 HMAC 映射，因此既隔离不同
/// 下游用户，又保留同一用户的会话边界与缓存亲和。
pub struct MachineContext {
    account_key: [u8; 32],
    pub installation_id: String,
    sandbox_tag: String,
    strip_non_native_identity_headers: bool,
    align_sandbox_with_user_agent: bool,
}

impl MachineContext {
    pub fn new(config: &IdentityConfig, account_id: i64, headers: &HeaderMap) -> Self {
        let account_key = hmac_digest(
            config.installation_id_seed.as_bytes(),
            format!("{MACHINE_ACCOUNT_KEY_PREFIX}{account_id}").as_bytes(),
        );
        let installation_id = per_account_installation_id(&config.installation_id_seed, account_id);
        let sandbox_tag = headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .map(sandbox_tag_from_user_agent)
            .unwrap_or_default();
        Self {
            account_key,
            installation_id,
            sandbox_tag,
            strip_non_native_identity_headers: config.strip_non_native_identity_headers,
            align_sandbox_with_user_agent: config.align_sandbox_with_user_agent,
        }
    }

    fn pseudonym(&self, value: &str) -> String {
        let raw = value.trim();
        if raw.is_empty() {
            return value.to_string();
        }
        let digest = hmac_digest(
            &self.account_key,
            format!("{MACHINE_PSEUDONYM_PREFIX}{raw}").as_bytes(),
        );
        if let Ok(parsed) = Uuid::parse_str(raw) {
            if parsed.get_version_num() == 7 {
                let mut timestamp = [0u8; 6];
                timestamp.copy_from_slice(&parsed.as_bytes()[..6]);
                return uuid_from_digest(&digest, true, Some(&timestamp));
            }
        }
        uuid_from_digest(&digest, false, None)
    }

    fn window_pseudonym(&self, value: &str) -> String {
        let raw = value.trim();
        if let Some((head, suffix)) = raw.rsplit_once(':') {
            if Uuid::parse_str(head).is_ok() {
                return format!("{}:{suffix}", self.pseudonym(head));
            }
            return value.to_string();
        }
        if Uuid::parse_str(raw).is_ok() {
            return self.pseudonym(raw);
        }
        value.to_string()
    }
}

pub fn apply_machine_headers(headers: &mut HeaderMap, machine: &MachineContext) {
    let has_native_session = ["session-id", "thread-id"]
        .iter()
        .any(|name| headers.get(*name).is_some_and(|value| !value.is_empty()));

    replace_header_if_present(headers, "x-codex-installation-id", &machine.installation_id);
    for name in [
        "session-id",
        "thread-id",
        "x-client-request-id",
        "x-codex-parent-thread-id",
    ] {
        if let Some(raw) = header_text(headers, name) {
            replace_header_if_present(headers, name, &machine.pseudonym(&raw));
        }
    }
    if let Some(raw) = header_text(headers, "x-codex-window-id") {
        replace_header_if_present(
            headers,
            "x-codex-window-id",
            &machine.window_pseudonym(&raw),
        );
    }
    if let Some(raw) = header_text(headers, "x-codex-turn-metadata") {
        if let Some(rewritten) = rewrite_machine_turn_metadata(&raw, machine) {
            replace_header_if_present(headers, "x-codex-turn-metadata", &rewritten);
        }
    }

    if machine.strip_non_native_identity_headers {
        headers.remove("conversation_id");
        if has_native_session {
            headers.remove("session_id");
        }
    }
}

pub fn apply_machine_body(body: &[u8], machine: &MachineContext) -> Option<Vec<u8>> {
    let mut root: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = root.as_object_mut()?;
    let mut modified = false;

    if let Some(raw) = object
        .get("prompt_cache_key")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
    {
        object.insert(
            "prompt_cache_key".to_string(),
            serde_json::Value::String(machine.pseudonym(&raw)),
        );
        modified = true;
    }

    if let Some(metadata) = object
        .get_mut("client_metadata")
        .and_then(serde_json::Value::as_object_mut)
    {
        if metadata.contains_key("x-codex-installation-id") {
            metadata.insert(
                "x-codex-installation-id".to_string(),
                serde_json::Value::String(machine.installation_id.clone()),
            );
            modified = true;
        }
        for key in ["session_id", "thread_id", "x-codex-parent-thread-id"] {
            if let Some(raw) = metadata
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
            {
                metadata.insert(
                    key.to_string(),
                    serde_json::Value::String(machine.pseudonym(&raw)),
                );
                modified = true;
            }
        }
        if let Some(raw) = metadata
            .get("x-codex-window-id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        {
            metadata.insert(
                "x-codex-window-id".to_string(),
                serde_json::Value::String(machine.window_pseudonym(&raw)),
            );
            modified = true;
        }
        if let Some(raw) = metadata
            .get("x-codex-turn-metadata")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        {
            if let Some(rewritten) = rewrite_machine_turn_metadata(&raw, machine) {
                metadata.insert(
                    "x-codex-turn-metadata".to_string(),
                    serde_json::Value::String(rewritten),
                );
                modified = true;
            }
        }
    }

    modified.then(|| serde_json::to_vec(&root).ok()).flatten()
}

fn rewrite_machine_turn_metadata(raw: &str, machine: &MachineContext) -> Option<String> {
    let mut parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = parsed.as_object_mut()?;
    let mut modified = false;

    if object.contains_key("installation_id") {
        object.insert(
            "installation_id".to_string(),
            serde_json::Value::String(machine.installation_id.clone()),
        );
        modified = true;
    }
    for key in [
        "session_id",
        "thread_id",
        "parent_thread_id",
        "forked_from_thread_id",
    ] {
        if let Some(raw) = object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        {
            object.insert(
                key.to_string(),
                serde_json::Value::String(machine.pseudonym(&raw)),
            );
            modified = true;
        }
    }
    if let Some(raw) = object
        .get("window_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
    {
        object.insert(
            "window_id".to_string(),
            serde_json::Value::String(machine.window_pseudonym(&raw)),
        );
        modified = true;
    }
    if machine.align_sandbox_with_user_agent {
        if let Some(current) = object
            .get("sandbox")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        {
            let next = rewrite_platform_sandbox(&current, &machine.sandbox_tag);
            if next != current {
                object.insert("sandbox".to_string(), serde_json::Value::String(next));
                modified = true;
            }
        }
    }

    if !modified {
        return None;
    }
    serde_json::to_string(&parsed).ok().map(ascii_escape_json)
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn replace_header_if_present(headers: &mut HeaderMap, name: &str, value: &str) {
    if !headers.contains_key(name) {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(value) {
        if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.insert(header_name, value);
        }
    }
}

/// 对真实会员请求应用稳定身份：machine 模式做按账号的稳定 1:1 映射；
/// passthrough 模式完全保留宿主已有身份。两条路径都不会创建会话或缓存键。
pub fn apply_stable_request_identity(
    config: &IdentityConfig,
    per_account: bool,
    account_id: i64,
    headers: &mut HeaderMap,
    body: &mut Vec<u8>,
) {
    if config.fingerprint_mode == "machine" {
        let machine = MachineContext::new(config, account_id, headers);
        apply_machine_headers(headers, &machine);
        if let Some(rewritten) = apply_machine_body(body, &machine) {
            *body = rewritten;
        }
        return;
    }

    let _ = (config, per_account, account_id, headers, body);
}

fn sandbox_tag_from_user_agent(user_agent: &str) -> String {
    let lowered = user_agent.to_ascii_lowercase();
    if lowered.contains("windows") {
        "windows_sandbox".to_string()
    } else if lowered.contains("mac os") || lowered.contains("darwin") || lowered.contains("macos")
    {
        "seatbelt".to_string()
    } else if !lowered.trim().is_empty() {
        "seccomp".to_string()
    } else {
        String::new()
    }
}

fn rewrite_platform_sandbox(current: &str, target: &str) -> String {
    if target.is_empty() {
        return current.to_string();
    }
    match current {
        "seatbelt" | "seccomp" | "windows_sandbox" | "windows_elevated" => {}
        _ => return current.to_string(),
    }
    if target == "windows_sandbox" && current == "windows_elevated" {
        return current.to_string();
    }
    target.to_string()
}

// ---------------------------------------------------------------------------
// 会话身份轮换（每条请求签发一套全新 ID）
//
// 共享底层工具：pin/rotate 策略与各养池路径都用它给出站请求签发一套互不相关
// 的标识（顺带剥掉旧 turn-state），避免同一 OAuth 账号多路并发复用同一脏会话被
// 上游按 session / thread / request-id / installation 维度识别、触发
// server_is_overloaded 降载。
//
// 形态与真实 codex 完全一致（实抓验证）：
// - session-id == thread-id == x-client-request-id，同一个 UUIDv7；
// - turn_id / root_turn_id / context_window_id 是独立 UUIDv7；
// - installation_id 是 UUIDv4；
// - window_id = "{session}:{窗口号}"。
// 改写原则不变：只替换请求里已存在的字段/头，绝不插入新头。
// ---------------------------------------------------------------------------

/// 单条请求签发的一套独立标识。
pub struct RequestIds {
    /// 会话 id（UUIDv7）：session-id / thread-id / x-client-request-id 共用。
    pub conversation_id: String,
    /// 设备 id（UUIDv4 形态随机值）。
    pub installation_id: String,
    /// turn id（UUIDv7）：turn_id / root_turn_id 存在时使用。
    pub turn_id: String,
    /// context window id（UUIDv7）。
    pub context_window_id: String,
}

impl RequestIds {
    pub fn mint() -> Self {
        Self {
            conversation_id: rand_ids::new_uuid_v7(),
            installation_id: rand_ids::new_uuid_v4(),
            turn_id: rand_ids::new_uuid_v7(),
            context_window_id: rand_ids::new_uuid_v7(),
        }
    }
}

/// 一并发一套 ID：改写请求头。
/// - 三个会话头（存在才改写）统一换成新 UUIDv7；
/// - x-codex-installation-id 头存在才换（真实主请求不带该头，绝不插入）；
/// - 剥离 x-codex-turn-state（turn state 是上游按账号会话签发的，跨请求
///   复用属于脏会话粘连）；
/// - x-codex-turn-metadata 头 JSON 内的各 id 字段同步改写。
pub fn apply_one_id_headers(headers: &mut HeaderMap, ids: &RequestIds) {
    for name in ["session-id", "thread-id", "x-client-request-id"] {
        if headers.contains_key(name) {
            if let Ok(value) = HeaderValue::from_str(&ids.conversation_id) {
                headers.insert(name, value);
            }
        }
    }
    if headers.contains_key("x-codex-installation-id") {
        if let Ok(value) = HeaderValue::from_str(&ids.installation_id) {
            headers.insert("x-codex-installation-id", value);
        }
    }
    // window-id 头形态 "{session}:{n}"，session 部分必须与新会话 id 一致。
    if let Some(existing) = headers.get("x-codex-window-id").cloned() {
        if let Ok(raw) = existing.to_str() {
            let rewritten = rewrite_window_id(raw, &ids.conversation_id);
            if let Ok(value) = HeaderValue::from_str(&rewritten) {
                headers.insert("x-codex-window-id", value);
            }
        }
    }
    headers.remove("x-codex-turn-state");
    if let Some(existing) = headers.get("x-codex-turn-metadata").cloned() {
        if let Ok(raw) = existing.to_str() {
            if let Some(rewritten) = rewrite_turn_metadata_ids(raw, ids) {
                if let Ok(value) = HeaderValue::from_str(&rewritten) {
                    headers.insert("x-codex-turn-metadata", value);
                }
            }
        }
    }
}

/// 一并发一套 ID：改写 body client_metadata，与头保持一致。
/// key 名与官方 responses_metadata.rs 的 client_metadata() 完全一致。
pub fn apply_one_id_body(body: &[u8], ids: &RequestIds) -> Option<Vec<u8>> {
    let mut root: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = root.as_object_mut()?;

    let mut modified = false;

    // 根级 prompt_cache_key：真实 codex 恒等于当前 session_id（实抓验证）。轮换会话时
    // 必须同步改写，否则 session-id 换了、prompt_cache_key 仍指向旧会话——新会话被旧
    // 缓存键绑回旧的（可能已降智的）路由，正是真6概率被压低的元凶之一。仅改写不插入。
    if object.contains_key("prompt_cache_key") {
        object.insert(
            "prompt_cache_key".to_string(),
            serde_json::Value::String(ids.conversation_id.clone()),
        );
        modified = true;
    }

    if let Some(metadata) = object
        .get_mut("client_metadata")
        .and_then(|m| m.as_object_mut())
    {
        set_metadata_string(
            metadata,
            "x-codex-installation-id",
            &ids.installation_id,
            &mut modified,
        );
        set_metadata_string(metadata, "session_id", &ids.conversation_id, &mut modified);
        set_metadata_string(metadata, "thread_id", &ids.conversation_id, &mut modified);
        set_metadata_string(metadata, "turn_id", &ids.turn_id, &mut modified);
        // 真实 codex 的 client_metadata 顶层也带 root_turn_id（首轮 == turn_id）。
        // 只改嵌套 turn-metadata 里的 root_turn_id 而漏掉这里，会让新 turn 仍挂在旧 turn
        // 树根上，被服务端当成旧会话延续。两处必须一起换新。
        set_metadata_string(metadata, "root_turn_id", &ids.turn_id, &mut modified);

        if let Some(serde_json::Value::String(window)) = metadata.get("x-codex-window-id") {
            let rewritten = rewrite_window_id(window, &ids.conversation_id);
            metadata.insert(
                "x-codex-window-id".to_string(),
                serde_json::Value::String(rewritten),
            );
            modified = true;
        }
        if let Some(serde_json::Value::String(embedded)) = metadata.get("x-codex-turn-metadata") {
            if let Some(rewritten) = rewrite_turn_metadata_ids(embedded, ids) {
                metadata.insert(
                    "x-codex-turn-metadata".to_string(),
                    serde_json::Value::String(rewritten),
                );
                modified = true;
            }
        }
    }

    if !modified {
        return None;
    }
    serde_json::to_vec(&root).ok()
}

/// 只改写 client_metadata 中已存在的字段（不插入），命中即置位 modified。
fn set_metadata_string(
    metadata: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: &str,
    modified: &mut bool,
) {
    if metadata.contains_key(key) {
        metadata.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
        *modified = true;
    }
}

/// 改写 turn-metadata JSON 里的所有 id 字段（存在才改写，键序保持）。
fn rewrite_turn_metadata_ids(raw: &str, ids: &RequestIds) -> Option<String> {
    let mut parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = parsed.as_object_mut()?;
    let mut modified = false;

    for (field, value) in [
        ("installation_id", ids.installation_id.as_str()),
        ("session_id", ids.conversation_id.as_str()),
        ("thread_id", ids.conversation_id.as_str()),
        ("turn_id", ids.turn_id.as_str()),
        ("root_turn_id", ids.turn_id.as_str()),
        ("context_window_id", ids.context_window_id.as_str()),
    ] {
        if object.contains_key(field) {
            object.insert(
                field.to_string(),
                serde_json::Value::String(value.to_string()),
            );
            modified = true;
        }
    }
    if let Some(serde_json::Value::String(window)) = object.get("window_id") {
        let rewritten = rewrite_window_id(window, &ids.conversation_id);
        object.insert(
            "window_id".to_string(),
            serde_json::Value::String(rewritten),
        );
        modified = true;
    }

    if !modified {
        return None;
    }
    serde_json::to_string(&parsed).ok().map(ascii_escape_json)
}

/// window_id 形态为 "{session}:{窗口号}"，替换 session 部分并保留窗口号。
fn rewrite_window_id(original: &str, conversation_id: &str) -> String {
    match original.rsplit_once(':') {
        Some((_, number)) if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{conversation_id}:{number}")
        }
        _ => format!("{conversation_id}:0"),
    }
}

/// 无第三方依赖的随机 UUID 生成。
/// 随机源：进程级随机密钥的 SipHash（std RandomState）+ 原子计数器 + 系统时钟。
/// 不用于任何安全场景，只需统计上不可预测、不重复。
mod rand_ids {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static KEYS: OnceLock<(RandomState, RandomState)> = OnceLock::new();

    fn random_bits() -> (u64, u64) {
        let (state_a, state_b) = KEYS.get_or_init(|| (RandomState::new(), RandomState::new()));
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let mut hasher_a = state_a.build_hasher();
        (counter, nanos, 0xa5u8).hash(&mut hasher_a);
        let mut hasher_b = state_b.build_hasher();
        (counter, nanos, 0x5au8).hash(&mut hasher_b);
        (hasher_a.finish(), hasher_b.finish())
    }

    /// RFC 9562 UUIDv7：48bit unix 毫秒 + 版本位 + 74bit 随机。
    /// 真实 codex 的 session/turn id 正是 UUIDv7（时间前缀与请求时刻一致）。
    pub fn new_uuid_v7() -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let (rand_a, rand_b) = random_bits();
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        bytes[6..8].copy_from_slice(&(rand_a as u16).to_be_bytes());
        bytes[8..].copy_from_slice(&rand_b.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0f) | 0x70; // version 7
        bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
        format_uuid(&bytes)
    }

    /// 随机 UUIDv4（installation_id 的真实形态）。
    pub fn new_uuid_v4() -> String {
        let (rand_a, rand_b) = random_bits();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&rand_a.to_be_bytes());
        bytes[8..].copy_from_slice(&rand_b.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
        bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
        format_uuid(&bytes)
    }

    fn format_uuid(bytes: &[u8; 16]) -> String {
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        )
    }
}

/// 提取 UA 模板可能需要的默认 os 段（与宿主规范 UA 的形态一致）。
pub fn default_os_segment() -> &'static str {
    "Ubuntu 22.04; x86_64"
}

pub fn default_terminal_segment() -> &'static str {
    "xterm-256color"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IdentityConfig;

    fn base_identity_config(profile: &str) -> IdentityConfig {
        IdentityConfig {
            profile: profile.to_string(),
            ..IdentityConfig::default()
        }
    }

    #[test]
    fn passthrough_returns_none() {
        assert!(resolve_identity(&base_identity_config("passthrough"), "0.153.4").is_none());
    }

    #[test]
    fn codex_cli_profile_builds_official_ua_shape() {
        let identity = resolve_identity(&base_identity_config("codex_cli"), "0.153.4").unwrap();
        assert_eq!(identity.originator, "codex_cli_rs");
        assert_eq!(
            identity.user_agent,
            "codex_cli_rs/0.153.4 (Ubuntu 22.04; x86_64) xterm-256color"
        );
    }

    #[test]
    fn identity_headers_only_apply_with_originator() {
        let identity = resolve_identity(&base_identity_config("codex_cli"), "0.153.4").unwrap();
        let mut headers = HeaderMap::new();
        apply_identity_headers(&mut headers, &identity, "us");
        assert!(headers.is_empty(), "no originator -> untouched");

        headers.insert("originator", HeaderValue::from_static("other"));
        headers.insert("version", HeaderValue::from_static("0.146.0"));
        apply_identity_headers(&mut headers, &identity, "us");
        assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(headers.get("version").unwrap(), "0.153.4");
        assert_eq!(headers.get(RESIDENCY_HEADER).unwrap(), "us");
    }

    #[test]
    fn per_account_installation_id_is_stable_and_distinct() {
        let a1 = per_account_installation_id("seed", 1);
        let a1_again = per_account_installation_id("seed", 1);
        let a2 = per_account_installation_id("seed", 2);
        assert_eq!(a1, a1_again);
        assert_ne!(a1, a2);
        // UUIDv4 形态
        assert_eq!(a1.len(), 36);
        assert_eq!(&a1[14..15], "4");
    }

    #[test]
    fn installation_id_rewrites_header_and_body_consistently() {
        let id = "11111111-2222-4333-8444-555555555555";
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-turn-metadata",
            HeaderValue::from_static(
                r#"{"installation_id":"old","sandbox":"off","zebra":"z","apple":"a"}"#,
            ),
        );
        apply_installation_id_headers(&mut headers, id);
        // 主请求不带 x-codex-installation-id 头：绝不插入（真实 0.153.4 行为）。
        assert!(headers.get("x-codex-installation-id").is_none());
        let metadata = headers
            .get("x-codex-turn-metadata")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(metadata.contains(id));
        assert!(metadata.contains("sandbox"));
        // preserve_order：改写后键序保持原样（zebra 在 apple 前），不得重排为字母序。
        assert!(metadata.find("zebra").unwrap() < metadata.find("apple").unwrap());

        // 请求本身带该头（如 compact 端点）时才改写。
        headers.insert("x-codex-installation-id", HeaderValue::from_static("old"));
        apply_installation_id_headers(&mut headers, id);
        assert_eq!(headers.get("x-codex-installation-id").unwrap(), id);
    }

    #[test]
    fn stable_passthrough_preserves_session_and_prompt_cache_key() {
        let original = "01991f14-7580-7a41-8f63-b04ac43f52d1";
        let config = IdentityConfig {
            installation_id_seed: "0123456789abcdef0123456789abcdef".to_string(),
            ..IdentityConfig::default()
        };
        let mut headers = HeaderMap::new();
        headers.insert("session-id", HeaderValue::from_str(original).unwrap());
        headers.insert("thread-id", HeaderValue::from_str(original).unwrap());
        let mut body = format!(
            r#"{{"prompt_cache_key":"{original}","client_metadata":{{"session_id":"{original}","thread_id":"{original}","x-codex-installation-id":"old"}}}}"#
        )
        .into_bytes();

        apply_stable_request_identity(&config, true, 42, &mut headers, &mut body);

        assert_eq!(headers.get("session-id").unwrap(), original);
        assert_eq!(headers.get("thread-id").unwrap(), original);
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["prompt_cache_key"], original);
        assert_eq!(value["client_metadata"]["session_id"], original);
        assert_eq!(value["client_metadata"]["thread_id"], original);
        assert_eq!(value["client_metadata"]["x-codex-installation-id"], "old");
    }

    #[test]
    fn machine_keeps_header_body_and_cache_key_in_one_identity_domain() {
        let original = "01991f14-7580-7a41-8f63-b04ac43f52d1";
        let config = IdentityConfig {
            fingerprint_mode: "machine".to_string(),
            installation_id_seed: "0123456789abcdef0123456789abcdef".to_string(),
            ..IdentityConfig::default()
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "user-agent",
            HeaderValue::from_static("codex_cli_rs/0.153.4 (Ubuntu 22.04; x86_64) xterm-256color"),
        );
        headers.insert("session-id", HeaderValue::from_str(original).unwrap());
        headers.insert("thread-id", HeaderValue::from_str(original).unwrap());
        headers.insert("session_id", HeaderValue::from_str(original).unwrap());
        headers.insert(
            "x-codex-turn-metadata",
            HeaderValue::from_str(&format!(
                r#"{{"installation_id":"old","session_id":"{original}","thread_id":"{original}","window_id":"{original}:2","sandbox":"windows_sandbox"}}"#
            ))
            .unwrap(),
        );
        let machine = MachineContext::new(&config, 42, &headers);
        let expected = machine.pseudonym(original);
        apply_machine_headers(&mut headers, &machine);

        assert_eq!(headers.get("session-id").unwrap(), expected.as_str());
        assert_eq!(headers.get("thread-id").unwrap(), expected.as_str());
        assert!(headers.get("session_id").is_none());
        let turn: serde_json::Value = serde_json::from_str(
            headers
                .get("x-codex-turn-metadata")
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(turn["session_id"], expected);
        assert_eq!(turn["sandbox"], "seccomp");

        let body = format!(
            r#"{{"prompt_cache_key":"{original}","client_metadata":{{"session_id":"{original}","thread_id":"{original}","x-codex-installation-id":"old"}}}}"#
        );
        let rewritten = apply_machine_body(body.as_bytes(), &machine).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(value["prompt_cache_key"], expected);
        assert_eq!(value["client_metadata"]["session_id"], expected);
        assert_eq!(
            value["client_metadata"]["x-codex-installation-id"],
            machine.installation_id
        );
    }

    #[test]
    fn machine_is_stable_per_account_and_distinct_across_accounts() {
        let config = IdentityConfig {
            installation_id_seed: "0123456789abcdef0123456789abcdef".to_string(),
            ..IdentityConfig::default()
        };
        let headers = HeaderMap::new();
        let account_a = MachineContext::new(&config, 7, &headers);
        let account_a_again = MachineContext::new(&config, 7, &headers);
        let account_b = MachineContext::new(&config, 8, &headers);
        let source = "01991f14-7580-7a41-8f63-b04ac43f52d1";
        assert_eq!(
            account_a.pseudonym(source),
            account_a_again.pseudonym(source)
        );
        assert_ne!(account_a.pseudonym(source), account_b.pseudonym(source));
        assert_eq!(account_a.installation_id, account_a_again.installation_id);
        assert_ne!(account_a.installation_id, account_b.installation_id);
    }

    #[test]
    fn uuid_v7_and_v4_have_correct_shape_and_are_unique() {
        let v7_a = rand_ids::new_uuid_v7();
        let v7_b = rand_ids::new_uuid_v7();
        let v4 = rand_ids::new_uuid_v4();
        assert_ne!(v7_a, v7_b);
        // 版本位与变体位。
        assert_eq!(&v7_a[14..15], "7");
        assert_eq!(&v4[14..15], "4");
        for id in [&v7_a, &v7_b, &v4] {
            assert_eq!(id.len(), 36);
            assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        }
        // v7 时间前缀：两次连续生成的毫秒前缀单调不减。
        assert!(v7_b[..8] >= v7_a[..8]);
    }

    #[test]
    fn one_id_per_request_rewrites_headers_consistently() {
        let ids = RequestIds::mint();
        let mut headers = HeaderMap::new();
        headers.insert("session-id", HeaderValue::from_static("old-conv"));
        headers.insert("thread-id", HeaderValue::from_static("old-conv"));
        headers.insert("x-client-request-id", HeaderValue::from_static("old-conv"));
        headers.insert("x-codex-turn-state", HeaderValue::from_static("dirty"));
        headers.insert("x-codex-window-id", HeaderValue::from_static("old-conv:3"));
        headers.insert(
            "x-codex-turn-metadata",
            HeaderValue::from_static(
                r#"{"installation_id":"old-inst","session_id":"old-conv","thread_id":"old-conv","turn_id":"old-turn","root_turn_id":"old-turn","context_window_id":"old-ctx","window_id":"old-conv:3","request_kind":"turn"}"#,
            ),
        );

        apply_one_id_headers(&mut headers, &ids);

        // 三个会话头同值（与真实 codex 一致），且换成了新 id。
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            assert_eq!(
                headers.get(name).unwrap().to_str().unwrap(),
                ids.conversation_id
            );
        }
        // turn-state 剥离；installation 头原本不存在则绝不插入。
        assert!(headers.get("x-codex-turn-state").is_none());
        assert!(headers.get("x-codex-installation-id").is_none());
        // window-id 头与新会话 id 一致，窗口号保留。
        assert_eq!(
            headers.get("x-codex-window-id").unwrap().to_str().unwrap(),
            format!("{}:3", ids.conversation_id)
        );
        // turn-metadata 内各 id 同步改写，窗口号保留。
        let metadata = headers
            .get("x-codex-turn-metadata")
            .unwrap()
            .to_str()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
        assert_eq!(value["installation_id"], ids.installation_id.as_str());
        assert_eq!(value["session_id"], ids.conversation_id.as_str());
        assert_eq!(value["thread_id"], ids.conversation_id.as_str());
        assert_eq!(value["turn_id"], ids.turn_id.as_str());
        assert_eq!(value["root_turn_id"], ids.turn_id.as_str());
        assert_eq!(value["context_window_id"], ids.context_window_id.as_str());
        assert_eq!(
            value["window_id"],
            format!("{}:3", ids.conversation_id).as_str()
        );
        assert_eq!(value["request_kind"], "turn");
    }

    #[test]
    fn one_id_per_request_rewrites_body_consistently() {
        let ids = RequestIds::mint();
        let body = br#"{"model":"gpt-5","stream":true,"prompt_cache_key":"old-conv","client_metadata":{"root_turn_id":"old-turn","x-codex-installation-id":"old-inst","session_id":"old-conv","thread_id":"old-conv","x-codex-window-id":"old-conv:0","turn_id":"old-turn","x-codex-turn-metadata":"{\"installation_id\":\"old-inst\",\"session_id\":\"old-conv\"}"}}"#;
        let rewritten = apply_one_id_body(body, &ids).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        // 根级 prompt_cache_key 换成新会话 id（真实 codex 恒等于 session_id）。
        assert_eq!(value["prompt_cache_key"], ids.conversation_id.as_str());
        let metadata = &value["client_metadata"];
        assert_eq!(
            metadata["x-codex-installation-id"],
            ids.installation_id.as_str()
        );
        assert_eq!(metadata["session_id"], ids.conversation_id.as_str());
        assert_eq!(metadata["thread_id"], ids.conversation_id.as_str());
        assert_eq!(metadata["turn_id"], ids.turn_id.as_str());
        // client_metadata 顶层 root_turn_id 也换新（与 turn_id 一致）。
        assert_eq!(metadata["root_turn_id"], ids.turn_id.as_str());
        assert_eq!(
            metadata["x-codex-window-id"],
            format!("{}:0", ids.conversation_id).as_str()
        );
        let embedded: serde_json::Value =
            serde_json::from_str(metadata["x-codex-turn-metadata"].as_str().unwrap()).unwrap();
        assert_eq!(embedded["installation_id"], ids.installation_id.as_str());
        assert_eq!(embedded["session_id"], ids.conversation_id.as_str());
        // 键序保持：model 在 client_metadata 前。
        let raw = String::from_utf8(rewritten).unwrap();
        assert!(raw.find("model").unwrap() < raw.find("client_metadata").unwrap());

        // 只有根级 prompt_cache_key、没有 client_metadata 时也应改写。
        let only_pck =
            apply_one_id_body(br#"{"model":"gpt-5","prompt_cache_key":"old-conv"}"#, &ids).unwrap();
        let v2: serde_json::Value = serde_json::from_slice(&only_pck).unwrap();
        assert_eq!(v2["prompt_cache_key"], ids.conversation_id.as_str());

        // 既无 client_metadata 又无 prompt_cache_key 的 body 原样不动。
        assert!(apply_one_id_body(br#"{"model":"gpt-5"}"#, &ids).is_none());
    }

    #[test]
    fn turn_metadata_rewrite_keeps_official_ascii_escape() {
        // 官方 to_ascii_json_string 会把非 ASCII 转成小写 \uXXXX；改写后必须保持同形态。
        let raw = r#"{"installation_id":"old","agent_name":"\u4e2d\u6587\ud83d\ude00"}"#;
        let rewritten = rewrite_json_field(raw, "installation_id", "new-id").unwrap();
        assert!(rewritten.contains(r"\u4e2d\u6587"));
        assert!(rewritten.contains(r"\ud83d\ude00"));
        assert!(rewritten.is_ascii());
        assert!(rewritten.contains("new-id"));
    }

    #[test]
    fn installation_id_rewrites_body_client_metadata() {
        let id = "11111111-2222-4333-8444-555555555555";
        let body = br#"{"model":"gpt-5","client_metadata":{"x-codex-installation-id":"old","x-codex-turn-metadata":"{\"installation_id\":\"old\"}"}}"#;
        let rewritten = apply_installation_id_body(body, id).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(value["client_metadata"]["x-codex-installation-id"], id);
        assert!(value["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .unwrap()
            .contains(id));
    }
}
