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

/// BPS 身份派生的 HMAC 前缀（与 machine 身份的前缀分开，同一份种子下两套派生
/// 不会互相碰撞）。
const BPS_SCOPE_PREFIX: &str = "bps-scope:";

/// 用安装种子 + 账号作用域派生稳定的 UUID 形态标识。
///
/// BPS 通道的 `metadata.task_id` / `metadata.turn_id` 与假名化的
/// `prompt_cache_key` 都走这里：与 [`MachineContext`] 的假名化同源（同一个 HMAC
/// 种子、同样的 UUID 形态输出），所以降智通道里的会话身份与身份 Profile 层是同一套
/// 口径——上游看到的仍然只是「一个账号下的若干会话」，账号之间互不串味，并且同一
/// 份请求每次派生的结果完全一致（重试可复现，缓存亲和不受影响）。
pub fn scoped_identifier(seed: &str, scope: &str, label: &str, value: &str) -> String {
    let message = format!("{BPS_SCOPE_PREFIX}{scope}\u{0}{label}\u{0}{value}");
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
// 官方 Codex 客户端判定（与 sub2api internal/pkg/openai/request.go 同口径）
// ---------------------------------------------------------------------------

/// 宿主透传的「客户端自报身份」头。宿主在把请求交给插件之前，会把出站身份收口为
/// 网关规范 Codex 身份（指纹收敛 / 统一出口，`enforceCodexIdentityHeaders`），因此
/// 插件看到的 `user-agent` / `originator` **永远是官方形态**，直接按它们判定客户端
/// 来源只会永远命中。宿主另用这两个私有头把客户端原始身份交给插件，判定口径因此与
/// 宿主侧 `openai.IsCodexOfficialClientByHeaders(userAgent, originator)` 完全一致。
///
/// 兼容性：旧宿主不写这两个头，插件回退到请求头 `user-agent` / `originator`。
pub const HOST_CLIENT_UA_HEADER: &str = "x-sub2api-client-user-agent";
pub const HOST_CLIENT_ORIGINATOR_HEADER: &str = "x-sub2api-client-originator";

/// 宿主私有透传头前缀。插件只读这些头做判定，**绝不能出站到上游**：它们既不是真实
/// 客户端会发的头，也会把中转链路特征暴露给上游。
pub const HOST_PRIVATE_HEADER_PREFIX: &str = "x-sub2api-";

/// 官方客户端家族 UA 前缀。逐项都是确定字面量，绝不含会被 TrimSpace 退化成
/// 裸 `codex` 的空格前缀（`Codex ` 家族由 family prefix 单独处理）。
const CODEX_OFFICIAL_CLIENT_UA_PREFIXES: [&str; 9] = [
    "codex_cli_rs/",
    "codex-tui/",
    "codex_vscode/",
    "codex_vscode_copilot/",
    "codex_app/",
    "codex_chatgpt_desktop/",
    "codex_atlas/",
    "codex_exec/",
    "codex_sdk_ts/",
];

/// `Codex ` 前缀家族（Codex Desktop 等）。**保留尾随空格**：去掉会退化成裸
/// `codex`，把任何含 codex 的串都放行。
const CODEX_OFFICIAL_CLIENT_FAMILY_PREFIX: &str = "codex ";

/// 官方客户端 originator 精确集合（app-server `initialize` 写入 clientInfo.name）。
/// originator 侧只用精确集合 + `Codex ` 家族前缀，不做「含 codex」宽松兜底。
const CODEX_OFFICIAL_CLIENT_ORIGINATORS: [&str; 9] = [
    "codex_cli_rs",
    "codex-tui",
    "codex_vscode",
    "codex_vscode_copilot",
    "codex_app",
    "codex_chatgpt_desktop",
    "codex_atlas",
    "codex_exec",
    "codex_sdk_ts",
];

/// 头值归一化：去首尾空白 + 小写（与 sub2api `normalizeCodexClientHeader` 一致）。
fn normalize_client_header(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// 前缀集匹配：优先 `starts_with`，UA 被网关拼接成复合串时退化为 `contains`
/// （与 sub2api 非 strict 版 `matchCodexClientHeaderPrefixes` 一致）。
fn match_client_prefixes(value: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| {
        let prefix = normalize_client_header(prefix);
        !prefix.is_empty() && (value.starts_with(&prefix) || value.contains(&prefix))
    })
}

/// 从 codex-rs 格式 UA 的最后一个括号组里取出 clientInfo.name。
///
/// `CODEX_INTERNAL_ORIGINATOR_OVERRIDE` 只改 UA 前缀（originator 段），不改尾部
/// 的 `(name; version)` 括号组，所以从尾部能恢复被 override 的真实客户端标识
/// （例如 `cccc/0.142.0 ... (codex-tui; 0.142.0)` → `codex-tui`）。
fn codex_ua_trailer_name(ua: &str) -> String {
    let Some(open) = ua.rfind('(') else {
        return String::new();
    };
    let rest = &ua[open + 1..];
    let Some(close) = rest.find(')') else {
        return String::new();
    };
    let inner = rest[..close].trim();
    match inner.find(';') {
        Some(semi) => inner[..semi].trim().to_string(),
        None => inner.to_string(),
    }
}

/// originator 是否为官方 Codex 客户端（精确集合 + `Codex ` 家族前缀）。
pub fn is_official_codex_client_originator(originator: &str) -> bool {
    let value = normalize_client_header(originator);
    if value.is_empty() {
        return false;
    }
    CODEX_OFFICIAL_CLIENT_ORIGINATORS.contains(&value.as_str())
        || value.starts_with(CODEX_OFFICIAL_CLIENT_FAMILY_PREFIX)
}

/// User-Agent 是否为官方 Codex 客户端：前缀集 → `Codex ` 家族 → 尾部 name 兜底。
pub fn is_official_codex_client_user_agent(user_agent: &str) -> bool {
    let ua = normalize_client_header(user_agent);
    if ua.is_empty() {
        return false;
    }
    if match_client_prefixes(&ua, &CODEX_OFFICIAL_CLIENT_UA_PREFIXES) {
        return true;
    }
    if ua.starts_with(CODEX_OFFICIAL_CLIENT_FAMILY_PREFIX) {
        return true;
    }
    let name = codex_ua_trailer_name(&ua);
    !name.is_empty() && is_official_codex_client_originator(&name)
}

/// 这条请求是否来自官方 Codex 客户端家族（UA 或 originator 命中）。
///
/// 与 sub2api `IsCodexOfficialClientByHeaders` 同口径：WorkBuddy / OpenClaw 这类
/// 第三方客户端两者都不命中，因此会被 BPS 门禁挡在降智兜底通道之外。
///
/// 入参必须是**身份 Profile 改写之前**的原始 UA / originator：profile 会把出站
/// UA 换成官方形态，改写后再判就只能永远命中。
pub fn is_official_codex_client(user_agent: &str, originator: &str) -> bool {
    if is_official_codex_client_user_agent(user_agent) {
        return true;
    }
    is_official_codex_client_originator(originator)
}

/// 供面板/日志留痕：客户端自报身份（UA 首段 + originator），截断到安全长度。
pub fn client_identity_label(user_agent: &str, originator: &str) -> String {
    let short_ua: String = user_agent.trim().chars().take(64).collect();
    format!("ua={short_ua:?} originator={originator:?}")
}

/// 客户端自报身份来源：`host` = 宿主透传头，`request` = 请求头回退。
pub fn pick_client_identity<'a>(
    host_user_agent: Option<&'a str>,
    host_originator: Option<&'a str>,
    request_user_agent: Option<&'a str>,
    request_originator: Option<&'a str>,
) -> (&'a str, &'a str, &'static str) {
    match host_user_agent {
        Some(ua) => (ua, host_originator.unwrap_or_default(), "host"),
        None => (
            request_user_agent.unwrap_or_default(),
            request_originator.unwrap_or_default(),
            "request",
        ),
    }
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
// 一并发一套 ID（one_id_per_request）
//
// 应对场景：同一 OAuth 账号被多路并发复用时，上游按 session / thread /
// request-id / installation 维度识别"共用脏会话"，触发 server_is_overloaded
// 降载。开启后每条出站请求签发一套互不相关的标识。
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
            conversation_id: Uuid::now_v7().to_string(),
            installation_id: Uuid::new_v4().to_string(),
            turn_id: Uuid::now_v7().to_string(),
            context_window_id: Uuid::now_v7().to_string(),
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
    let has_native_session =
        headers.contains_key("session-id") || headers.contains_key("thread-id");
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
    headers.remove("conversation_id");
    if has_native_session {
        headers.remove("session_id");
    }
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
    if object.contains_key("prompt_cache_key") {
        object.insert(
            "prompt_cache_key".to_string(),
            serde_json::Value::String(ids.conversation_id.clone()),
        );
        modified = true;
    }

    if let Some(metadata) = object
        .get_mut("client_metadata")
        .and_then(serde_json::Value::as_object_mut)
    {
        let set_string = |metadata: &mut serde_json::Map<String, serde_json::Value>,
                          key: &str,
                          value: &str,
                          modified: &mut bool| {
            if metadata.contains_key(key) {
                metadata.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
                *modified = true;
            }
        };

        set_string(
            metadata,
            "x-codex-installation-id",
            &ids.installation_id,
            &mut modified,
        );
        set_string(metadata, "session_id", &ids.conversation_id, &mut modified);
        set_string(metadata, "thread_id", &ids.conversation_id, &mut modified);
        set_string(metadata, "turn_id", &ids.turn_id, &mut modified);

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
    fn uuid_v7_and_v4_have_correct_shape_and_are_unique() {
        let v7_a = Uuid::now_v7().to_string();
        let v7_b = Uuid::now_v7().to_string();
        let v4 = Uuid::new_v4().to_string();
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
    fn machine_keeps_header_body_and_cache_key_in_one_identity_domain() {
        let original = "01991f14-7580-7a41-8f63-b04ac43f52d1";
        let mut config = IdentityConfig::default();
        config.fingerprint_mode = "machine".to_string();
        config.installation_id_seed = "0123456789abcdef0123456789abcdef".to_string();
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
        let mut config = IdentityConfig::default();
        config.installation_id_seed = "0123456789abcdef0123456789abcdef".to_string();
        let headers = HeaderMap::new();
        let a = MachineContext::new(&config, 7, &headers);
        let a_again = MachineContext::new(&config, 7, &headers);
        let b = MachineContext::new(&config, 8, &headers);
        let source = "01991f14-7580-7a41-8f63-b04ac43f52d1";
        assert_eq!(a.pseudonym(source), a_again.pseudonym(source));
        assert_ne!(a.pseudonym(source), b.pseudonym(source));
        assert_eq!(a.installation_id, a_again.installation_id);
        assert_ne!(a.installation_id, b.installation_id);
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
        let body = br#"{"model":"gpt-5","stream":true,"client_metadata":{"x-codex-installation-id":"old-inst","session_id":"old-conv","thread_id":"old-conv","x-codex-window-id":"old-conv:0","turn_id":"old-turn","x-codex-turn-metadata":"{\"installation_id\":\"old-inst\",\"session_id\":\"old-conv\"}"}}"#;
        let rewritten = apply_one_id_body(body, &ids).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        let metadata = &value["client_metadata"];
        assert_eq!(
            metadata["x-codex-installation-id"],
            ids.installation_id.as_str()
        );
        assert_eq!(metadata["session_id"], ids.conversation_id.as_str());
        assert_eq!(metadata["thread_id"], ids.conversation_id.as_str());
        assert_eq!(metadata["turn_id"], ids.turn_id.as_str());
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

        // 没有 client_metadata 的 body 原样不动。
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

    // ----- 官方 Codex 客户端判定（BPS 客户端门禁口径，与 sub2api 对齐）-----

    #[test]
    fn official_client_matches_sub2api_rules() {
        // 官方 UA 前缀家族（逐字面量）。
        for ua in [
            "codex_cli_rs/0.147.0 (Ubuntu 22.4.0; x86_64) xterm-256color",
            "codex-tui/0.144.1 (Mac OS 15.0; arm64)",
            "codex_vscode/1.0.0",
            "codex_vscode_copilot/1.2.3",
            "codex_app/0.9.0",
            "codex_chatgpt_desktop/1.0.0",
            "codex_atlas/1.0.0",
            "codex_exec/0.5.0",
            "codex_sdk_ts/0.2.0",
            "Codex Desktop/0.147.0-alpha.1.2 (Windows 10.0.26200; x86_64) unknown",
        ] {
            assert!(
                is_official_codex_client_user_agent(ua),
                "should match: {ua}"
            );
        }
        // UA 被网关拼成复合串时退化为包含匹配。
        assert!(is_official_codex_client_user_agent(
            "SomeProxy/1.0 codex_cli_rs/0.147.0"
        ));
        // UA 尾部括号组兜底：originator 被 override（cccc）但尾部保留真实客户端名。
        assert!(is_official_codex_client_user_agent(
            "cccc/0.142.0 (Ubuntu 22.4.0; x86_64) xterm-256color (codex-tui; 0.142.0)"
        ));
    }

    #[test]
    fn non_official_clients_are_rejected() {
        for ua in [
            // 本单客户实际 UA：第三方 WorkBuddy 客户端。
            "WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.147.0",
            "OpenClaw/1.0 (Linux; x86_64)",
            "OpenAI/JS 6.39.1",
            "curl/8.21.0",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/140.0.0.0",
            "",
        ] {
            assert!(
                !is_official_codex_client_user_agent(ua),
                "should NOT match: {ua}"
            );
        }
        assert!(!is_official_codex_client_originator("workbuddy"));
        assert!(!is_official_codex_client_originator("codex-cli"));
        // 宽松版（与 sub2api 非 strict 的 IsCodexOfficialClientRequest 同口径）允许
        // 包含匹配，所以前缀里塞官方 token 的伪造串仍会命中——这是刻意对齐上游
        // 行为，不在这里收窄。
        assert!(is_official_codex_client_user_agent(
            "evil-codex_cli_rs/1.0.0"
        ));
    }

    #[test]
    fn originator_family_matches_exactly_or_codex_space() {
        for value in [
            "codex_cli_rs",
            "codex-tui",
            "codex_vscode",
            "codex_vscode_copilot",
            "codex_app",
            "codex_chatgpt_desktop",
            "codex_atlas",
            "codex_exec",
            "codex_sdk_ts",
            "Codex Desktop",
        ] {
            assert!(
                is_official_codex_client_originator(value),
                "should match originator: {value}"
            );
        }
        for value in ["", "  ", "codex", "codexcli", "codex_tui", "my-codex_atlas"] {
            assert!(
                !is_official_codex_client_originator(value),
                "should NOT match originator: {value}"
            );
        }
    }

    #[test]
    fn headers_gate_uses_user_agent_or_originator() {
        assert!(is_official_codex_client("codex_cli_rs/0.147.0", ""));
        assert!(is_official_codex_client(
            "WorkBuddy/5.6.2 CLI/2.147.0",
            "codex_cli_rs"
        ));
        assert!(!is_official_codex_client(
            "WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.147.0",
            "workbuddy"
        ));
        assert!(!is_official_codex_client("", ""));
        assert_eq!(
            client_identity_label("WorkBuddy/5.6.2 CLI/2.147.0", "workbuddy"),
            "ua=\"WorkBuddy/5.6.2 CLI/2.147.0\" originator=\"workbuddy\""
        );
    }

    // ----- 客户端身份来源：宿主透传头优先，旧宿主回退请求头 -----

    #[test]
    fn client_identity_prefers_host_headers() {
        // 宿主透传：出站 UA 已被收口成规范身份，真实来源看宿主头。
        let (ua, originator, source) = pick_client_identity(
            Some("WorkBuddy/5.6.2 CLI/2.147.0"),
            Some("workbuddy"),
            Some("codex_cli_rs/0.153.4 (Ubuntu 22.04; x86_64) xterm-256color"),
            Some("codex_cli_rs"),
        );
        assert_eq!(source, "host");
        assert_eq!(ua, "WorkBuddy/5.6.2 CLI/2.147.0");
        assert_eq!(originator, "workbuddy");
        assert!(!is_official_codex_client(ua, originator));

        // 宿主透传官方身份 → 门禁放行。
        let (ua, originator, source) = pick_client_identity(
            Some("codex_cli_rs/0.153.4 (Ubuntu 22.04; x86_64) xterm-256color"),
            Some("codex_cli_rs"),
            Some("codex_cli_rs/0.153.4"),
            Some("codex_cli_rs"),
        );
        assert_eq!(source, "host");
        assert!(is_official_codex_client(ua, originator));

        // 宿主只给了 UA、originator 头缺失 → originator 视为空，不误判。
        let (ua, originator, source) = pick_client_identity(
            Some("WorkBuddy/5.6.2 CLI/2.147.0"),
            None,
            Some("codex_cli_rs/0.153.4"),
            Some("codex_cli_rs"),
        );
        assert_eq!(source, "host");
        assert_eq!(originator, "");
        assert!(!is_official_codex_client(ua, originator));

        // 旧宿主（没有透传头）→ 回退请求头，行为与 0.4.33 一致。
        let (ua, originator, source) = pick_client_identity(
            None,
            Some("codex_cli_rs"),
            Some("WorkBuddy/5.6.2 CLI/2.147.0"),
            Some("workbuddy"),
        );
        assert_eq!(source, "request");
        assert_eq!(ua, "WorkBuddy/5.6.2 CLI/2.147.0");
        assert_eq!(originator, "workbuddy");

        // 两头都缺 → 空串。
        let (ua, originator, source) = pick_client_identity(None, None, None, None);
        assert_eq!(source, "request");
        assert!(ua.is_empty() && originator.is_empty());
    }
}
