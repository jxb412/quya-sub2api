//! BPS 官方 Excel 插件授权（账号密码 + TOTP → Excel OAuth）的插件侧凭据仓。
//!
//! BPS 端点只认「官方 Excel 加载项」这一路 OAuth 凭据（client_id
//! `app_fnr0pYvVwwFDocDumLG3H2Bp`）：同账号对照实测，宿主给的 Codex token 打过去是
//! 403，Excel token 打过去是 200；改 UA / 改 scope / 改指纹都换不来 200。
//!
//! 授权过程没法在插件进程里跑（容器里没有 node/python，也过不了 auth.openai.com 的
//! Cloudflare 托管挑战），所以登录交给宿主机上的 `bps-auth` 服务（见 contrib/bps-auth），
//! 插件这边只做四件事：
//!
//! 1. 保存账号的密码 / TOTP（只落本机数据目录 0600，绝不出现在面板 JSON、绝不写日志）；
//! 2. 触发一次授权，把返回的 Excel 凭据（access + refresh）存下来；
//! 3. 后台按「提前刷新窗口」自动轮换 refresh_token；
//! 4. BPS 出站时用 Excel access_token 替换宿主的 bearer。
//!
//! 任何一步失败都**回退宿主凭据**：拿不到 Excel token 时 BPS 通道照旧按宿主 token 出站，
//! 不会把请求卡住，也不会因为插件自身的授权问题影响正常通道。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::config::PluginConfig;
use crate::service::SharedState;

/// 官方 Excel 加载项的 OAuth client_id（与 contrib/bps-auth 里的常量同源）。
pub const EXCEL_CLIENT_ID: &str = "app_fnr0pYvVwwFDocDumLG3H2Bp";
/// 授权请求的额外超时余量：给网络与排队留 30s。
const LOGIN_TIMEOUT_SLACK_SECONDS: u64 = 64;
/// 刷新凭据的 HTTP 超时。
const REFRESH_TIMEOUT_SECONDS: u64 = 90;
/// 后台刷新巡检步长：每 15s 判断一次是否到了刷新周期。
const REFRESH_TICK_SECONDS: u64 = 15;
/// 授权 / 刷新失败留痕的去重窗口。
const NOTE_DEDUPE_MS: u64 = 300_000;
/// 拿不到 expires_in 时的兜底有效期（Excel access_token 实测 10 天）。
const DEFAULT_TOKEN_TTL_MS: u64 = 6 * 86_400_000;

fn now_ms() -> u64 {
    crate::donor::now_ms()
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 一个账号的登录材料。**只落本机数据目录，绝不出面板、绝不写日志。**
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AccountSecret {
    /// 登录邮箱（留空 = 从宿主账号名 `oauth---xxx@yyy` 推导）。
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub totp_secret: String,
    /// 本次授权使用的出口代理（留空 = 用全局 `bps_auth_proxy_url`）。
    #[serde(default)]
    pub proxy_url: String,
    #[serde(default)]
    pub updated_at_ms: u64,
}

/// 一次成功的 Excel OAuth 结果。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ExcelCredential {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub chatgpt_account_id: String,
    #[serde(default)]
    pub chatgpt_user_id: String,
    #[serde(default)]
    pub expires_at_ms: u64,
    #[serde(default)]
    pub obtained_at_ms: u64,
    #[serde(default)]
    pub refreshed_at_ms: u64,
    /// 最近一次失败原因（成功会清空）。
    #[serde(default)]
    pub last_error: String,
    #[serde(default)]
    pub last_error_at_ms: u64,
}

/// 面板轮询用的授权批次状态。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AuthRun {
    pub running: bool,
    pub account_id: i64,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub stage: String,
    pub error: String,
}

/// 面板展示的授权服务健康度（由后台循环刷新，面板只读缓存）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ServiceHealth {
    pub checked_at_ms: u64,
    pub ok: bool,
    pub version: String,
    pub detail: String,
}

/// 面板用的单账号视图：**不含任何凭据**。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AccountAuthView {
    pub has_secret: bool,
    pub email: String,
    pub has_credential: bool,
    pub remaining_seconds: i64,
    pub expires_at_ms: u64,
    pub obtained_at_ms: u64,
    pub refreshed_at_ms: u64,
    pub last_error: String,
    pub last_error_at_ms: u64,
    /// chatgpt_account_id 前 8 位，用来核对授权的是不是同一个上游账号。
    pub workspace_hint: String,
}

#[derive(Default)]
struct Inner {
    secrets: BTreeMap<i64, AccountSecret>,
    credentials: BTreeMap<i64, ExcelCredential>,
    run: AuthRun,
    health: ServiceHealth,
    loaded: bool,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Snapshot {
    #[serde(default)]
    secrets: BTreeMap<i64, AccountSecret>,
    #[serde(default)]
    credentials: BTreeMap<i64, ExcelCredential>,
}

/// Excel 凭据仓（进程内唯一，挂在 SharedState 上）。
#[derive(Default)]
pub struct BpsAuthStore {
    inner: Mutex<Inner>,
}

impl BpsAuthStore {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock_or_recover(&self.inner)
    }

    /// 首次访问时按需从磁盘恢复。凭据与密码都只落这一个文件（0600）。
    pub fn ensure_loaded(&self, path: &str) {
        {
            let guard = self.lock();
            if guard.loaded {
                return;
            }
        }
        let mut snapshot = Snapshot::default();
        let path = path.trim();
        if !path.is_empty() {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(parsed) = serde_json::from_slice::<Snapshot>(&bytes) {
                    snapshot = parsed;
                }
            }
        }
        let mut guard = self.lock();
        guard.loaded = true;
        if guard.secrets.is_empty() {
            guard.secrets = snapshot.secrets;
        }
        if guard.credentials.is_empty() {
            guard.credentials = snapshot.credentials;
        }
    }

    fn persist(&self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let (secrets, credentials) = {
            let guard = self.lock();
            (guard.secrets.clone(), guard.credentials.clone())
        };
        let snapshot = Snapshot {
            secrets,
            credentials,
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) else {
            return;
        };
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(path, bytes).is_ok() {
            restrict_permissions(path);
        }
    }

    pub fn run_state(&self) -> AuthRun {
        self.lock().run.clone()
    }

    pub fn is_running(&self) -> bool {
        self.lock().run.running
    }

    pub fn health(&self) -> ServiceHealth {
        self.lock().health.clone()
    }

    fn set_health(&self, health: ServiceHealth) {
        self.lock().health = health;
    }

    fn begin_run(&self, account_id: i64) -> bool {
        let mut guard = self.lock();
        if guard.run.running {
            return false;
        }
        guard.run = AuthRun {
            running: true,
            account_id,
            started_at_ms: now_ms(),
            finished_at_ms: 0,
            stage: "start".to_string(),
            error: String::new(),
        };
        true
    }

    fn set_stage(&self, stage: &str) {
        self.lock().run.stage = stage.to_string();
    }

    fn finish_run(&self, error: String) {
        let mut guard = self.lock();
        guard.run.running = false;
        guard.run.finished_at_ms = now_ms();
        guard.run.error = error;
    }

    pub fn secret(&self, account_id: i64) -> Option<AccountSecret> {
        self.lock().secrets.get(&account_id).cloned()
    }

    pub fn set_secret(&self, account_id: i64, mut secret: AccountSecret, path: &str) {
        secret.updated_at_ms = now_ms();
        self.lock().secrets.insert(account_id, secret);
        self.persist(path);
    }

    pub fn credential(&self, account_id: i64) -> Option<ExcelCredential> {
        self.lock().credentials.get(&account_id).cloned()
    }

    fn set_credential(&self, account_id: i64, mut credential: ExcelCredential, path: &str) {
        credential.last_error.clear();
        credential.last_error_at_ms = 0;
        self.lock().credentials.insert(account_id, credential);
        self.persist(path);
    }

    fn mark_error(&self, account_id: i64, message: &str, path: &str) {
        let mut guard = self.lock();
        if let Some(credential) = guard.credentials.get_mut(&account_id) {
            credential.last_error = message.to_string();
            credential.last_error_at_ms = now_ms();
        }
        drop(guard);
        self.persist(path);
    }

    /// 清掉一个账号的密码 + 凭据（换号 / 停用时用）。
    pub fn clear_account(&self, account_id: i64, path: &str) {
        {
            let mut guard = self.lock();
            guard.secrets.remove(&account_id);
            guard.credentials.remove(&account_id);
        }
        self.persist(path);
    }

    /// 只有到期时间仍在 `margin_ms` 之外才返回 access_token。
    pub fn token_if_valid(&self, account_id: i64, now: u64, margin_ms: u64) -> Option<String> {
        let guard = self.lock();
        let credential = guard.credentials.get(&account_id)?;
        if credential.access_token.trim().is_empty() {
            return None;
        }
        if credential.expires_at_ms <= now.saturating_add(margin_ms) {
            return None;
        }
        Some(credential.access_token.clone())
    }

    /// 需要后台刷新 / 面板展示的账号 id（有凭据的那批）。
    pub fn credential_accounts(&self) -> Vec<i64> {
        self.lock().credentials.keys().copied().collect()
    }

    pub fn view(&self, account_id: i64, now: u64) -> AccountAuthView {
        let guard = self.lock();
        let secret = guard.secrets.get(&account_id);
        let credential = guard.credentials.get(&account_id);
        AccountAuthView {
            has_secret: secret.map(|s| !s.password.is_empty()).unwrap_or(false),
            email: secret
                .map(|s| s.email.clone())
                .filter(|value| !value.is_empty())
                .or_else(|| credential.map(|c| c.email.clone()))
                .unwrap_or_default(),
            has_credential: credential
                .map(|c| !c.access_token.trim().is_empty())
                .unwrap_or(false),
            remaining_seconds: credential
                .map(|c| (c.expires_at_ms as i64 - now as i64) / 1000)
                .unwrap_or(0),
            expires_at_ms: credential.map(|c| c.expires_at_ms).unwrap_or(0),
            obtained_at_ms: credential.map(|c| c.obtained_at_ms).unwrap_or(0),
            refreshed_at_ms: credential.map(|c| c.refreshed_at_ms).unwrap_or(0),
            last_error: credential.map(|c| c.last_error.clone()).unwrap_or_default(),
            last_error_at_ms: credential.map(|c| c.last_error_at_ms).unwrap_or(0),
            workspace_hint: credential
                .map(|c| c.chatgpt_account_id.chars().take(8).collect())
                .unwrap_or_default(),
        }
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &str) {}

/// `oauth---xxx@yyy` → `xxx@yyy`。取不到合法邮箱时返回 None。
pub fn email_from_name(name: &str) -> Option<String> {
    let value = name.trim();
    if value.is_empty() {
        return None;
    }
    let tail = match value.rsplit_once("---") {
        Some((_, tail)) => tail,
        None => value,
    };
    let tail = tail.trim();
    if tail.contains('@') && !tail.chars().any(char::is_whitespace) {
        Some(tail.to_string())
    } else {
        None
    }
}

fn token_str(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// `2026-10-19T02:39:18.000Z` → Unix 毫秒。只认这一种形状。
fn iso_ms(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.len() < 20 || !value.ends_with('Z') {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: u64 = value.get(5..7)?.parse().ok()?;
    let day: u64 = value.get(8..10)?.parse().ok()?;
    let hour: u64 = value.get(11..13)?.parse().ok()?;
    let minute: u64 = value.get(14..16)?.parse().ok()?;
    let second: u64 = value.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days as i64 * 86_400 + (hour * 3600 + minute * 60 + second) as i64;
    u64::try_from(seconds).ok().map(|secs| secs * 1000)
}

/// Howard Hinnant 的 days_from_civil（与 bps.rs 同源的小工具）。
fn days_from_civil(year: i64, month: u64, day: u64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// JWT 的 `exp`（秒 → 毫秒）。解析失败返回 None。
fn jwt_exp_ms(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64_url_decode(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let exp = value.get("exp").and_then(serde_json::Value::as_i64)?;
    u64::try_from(exp).ok().map(|secs| secs * 1000)
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for ch in input.chars() {
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '-' | '+' => 62,
            '_' | '/' => 63,
            '=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}
fn str_of(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// 与宿主机 bps-auth 服务的交互
// ---------------------------------------------------------------------------

/// 凭据 / 密码的落盘路径（面板上显示的那一份；空 = 由巡检结论路径推导）。
pub fn state_file(config: &PluginConfig) -> String {
    config.bps_auth_state_file()
}

fn service_base(config: &PluginConfig) -> Result<String, String> {
    let base = config
        .bps_auth_service_url
        .trim()
        .trim_end_matches('/')
        .to_string();
    if base.is_empty() {
        return Err("未配置 bps-auth 服务地址（bps_auth_service_url）".to_string());
    }
    Ok(base)
}

/// 调一次宿主机上的 bps-auth 服务。回环直连，不过任何代理。
async fn call_service(
    config: &PluginConfig,
    method: &str,
    path: &str,
    payload: Option<serde_json::Value>,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let base = service_base(config)?;
    let token = config.bps_auth_service_token.trim().to_string();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout)
        .build()
        .map_err(|err| format!("构造授权服务客户端失败: {err}"))?;
    let url = format!("{base}{path}");
    let mut request = match method {
        "GET" => client.get(&url),
        _ => client.post(&url),
    };
    if !token.is_empty() {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(body) = payload {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|err| {
        format!(
            "授权服务请求失败: {}",
            crate::transport::safe_reqwest_error(&err)
        )
    })?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("授权服务 HTTP {}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|_| "授权服务返回的不是 JSON".to_string())
}

/// 探测 bps-auth 服务的运行时状态，并缓存给面板。
pub async fn probe_health(state: &Arc<SharedState>, config: &PluginConfig) -> ServiceHealth {
    let mut health = ServiceHealth {
        checked_at_ms: now_ms(),
        ..ServiceHealth::default()
    };
    match call_service(config, "GET", "/health", None, Duration::from_secs(20)).await {
        Ok(value) => {
            health.ok = value
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            health.version = str_of(&value, "version");
            let runtime = value
                .get("runtime")
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            health.detail = runtime.chars().take(400).collect();
        }
        Err(err) => {
            health.ok = false;
            health.detail = err;
        }
    }
    state.bps_auth.set_health(health.clone());
    health
}

/// 从宿主账号列表里取这个号的登录邮箱（`oauth---xxx@yyy` → `xxx@yyy`）。
async fn resolve_email(config: &PluginConfig, account_id: i64) -> Result<String, String> {
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    if base.is_empty() || key.is_empty() {
        return Err(
            "未配置 Sub2API 管理 API（admin_api_base / admin_api_key），无法取账号邮箱".to_string(),
        );
    }
    let targets = crate::admin::fetch_all_targets(&base, &key).await?;
    let target = targets
        .iter()
        .find(|row| row.account_id == account_id)
        .ok_or_else(|| "账号不存在或未被导出".to_string())?;
    email_from_name(&target.name)
        .ok_or_else(|| format!("无法从账号名 {:?} 解析出登录邮箱，请手动填写", target.name))
}

fn parse_login_credentials(
    out: &serde_json::Value,
    email: &str,
    expected_workspace: &str,
) -> Result<ExcelCredential, String> {
    let credentials = out
        .get("credentials")
        .ok_or_else(|| "授权服务没有返回凭据".to_string())?;
    let access_token = token_str(credentials, "access_token");
    if access_token.is_empty() {
        return Err("授权返回的 access_token 为空".to_string());
    }
    let refresh_token = token_str(credentials, "refresh_token");
    if refresh_token.is_empty() {
        return Err("授权返回的 refresh_token 为空".to_string());
    }
    let client_id = {
        let value = token_str(credentials, "client_id");
        if value.is_empty() {
            EXCEL_CLIENT_ID.to_string()
        } else {
            value
        }
    };
    // 只认官方 Excel 加载项的凭据：别的 client 打 BPS 一定是 403，存下来只会误导排查。
    if client_id != EXCEL_CLIENT_ID {
        return Err(format!("授权客户端 {client_id} 不是官方 Excel 加载项"));
    }
    let chatgpt_account_id = token_str(credentials, "chatgpt_account_id");
    if chatgpt_account_id.is_empty() {
        return Err("授权返回里没有 chatgpt_account_id".to_string());
    }
    let workspace = expected_workspace.trim();
    if !workspace.is_empty() && workspace != chatgpt_account_id {
        return Err("授权拿到的 workspace 与目标账号不一致，已丢弃".to_string());
    }
    let expires_at_ms = iso_ms(&token_str(credentials, "expires_at"))
        .or_else(|| jwt_exp_ms(&access_token))
        .unwrap_or_else(|| now_ms().saturating_add(DEFAULT_TOKEN_TTL_MS));
    Ok(ExcelCredential {
        email: {
            let value = token_str(credentials, "email");
            if value.is_empty() {
                email.to_string()
            } else {
                value
            }
        },
        client_id,
        access_token,
        refresh_token,
        chatgpt_account_id,
        chatgpt_user_id: token_str(credentials, "chatgpt_user_id"),
        expires_at_ms,
        obtained_at_ms: now_ms(),
        refreshed_at_ms: 0,
        last_error: String::new(),
        last_error_at_ms: 0,
    })
}

fn apply_refresh(
    old: &ExcelCredential,
    tokens: &serde_json::Value,
) -> Result<ExcelCredential, String> {
    let access_token = token_str(tokens, "access_token");
    if access_token.is_empty() {
        return Err("刷新返回里没有 access_token".to_string());
    }
    let rotated = token_str(tokens, "refresh_token");
    let expires_in = tokens
        .get("expires_in")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value > 0);
    let expires_at_ms = expires_in
        .map(|secs| now_ms().saturating_add(secs as u64 * 1000))
        .or_else(|| jwt_exp_ms(&access_token))
        .unwrap_or_else(|| now_ms().saturating_add(DEFAULT_TOKEN_TTL_MS));
    let mut next = old.clone();
    next.access_token = access_token;
    // 上游没轮换 refresh_token 时保留旧的，避免把唯一一份凭据覆盖成空。
    if !rotated.is_empty() {
        next.refresh_token = rotated;
    }
    next.expires_at_ms = expires_at_ms;
    next.refreshed_at_ms = now_ms();
    next.last_error.clear();
    next.last_error_at_ms = 0;
    Ok(next)
}

/// 跑一次完整授权。`override_secret` 里非空的字段会先合并进已保存的材料。
pub async fn login_account(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    account_id: i64,
    override_secret: Option<AccountSecret>,
) -> Result<(), String> {
    let path = state_file(config);
    state.bps_auth.ensure_loaded(&path);
    if let Some(secret) = override_secret {
        let mut merged = state.bps_auth.secret(account_id).unwrap_or_default();
        if !secret.email.trim().is_empty() {
            merged.email = secret.email.trim().to_string();
        }
        if !secret.password.is_empty() {
            merged.password = secret.password;
        }
        if !secret.totp_secret.trim().is_empty() {
            merged.totp_secret = secret.totp_secret.trim().to_string();
        }
        if !secret.proxy_url.trim().is_empty() {
            merged.proxy_url = secret.proxy_url.trim().to_string();
        }
        state.bps_auth.set_secret(account_id, merged, &path);
    }
    let mut secret = state.bps_auth.secret(account_id).unwrap_or_default();
    if secret.password.trim().is_empty() {
        return Err("该账号还没填登录密码".to_string());
    }
    if secret.email.trim().is_empty() {
        let resolved = resolve_email(config, account_id).await?;
        secret.email = resolved;
        state.bps_auth.set_secret(account_id, secret.clone(), &path);
    }
    let proxy = if secret.proxy_url.trim().is_empty() {
        config.bps_auth_proxy_url.trim().to_string()
    } else {
        secret.proxy_url.trim().to_string()
    };
    if !state.bps_auth.begin_run(account_id) {
        return Err("已有一次授权在进行中，请等它结束".to_string());
    }
    let outcome = login_inner(
        state,
        config,
        account_id,
        &path,
        &secret.email,
        &secret.password,
        &secret.totp_secret,
        &proxy,
    )
    .await;
    let error = match &outcome {
        Ok(()) => String::new(),
        Err(err) => err.clone(),
    };
    state.bps_auth.finish_run(error);
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn login_inner(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    account_id: i64,
    path: &str,
    email: &str,
    password: &str,
    totp_secret: &str,
    proxy_url: &str,
) -> Result<(), String> {
    // 目标 workspace：宿主账号上记着的那个 chatgpt_account_id。授权拿到的 workspace
    // 与它不一致时直接丢弃，避免把别的号（例如个人号 / 另一个组织）的凭据挂上来。
    let expected_workspace = expected_workspace_of(config, account_id).await;
    let payload = serde_json::json!({
        "email": email,
        "password": password,
        "totp_secret": totp_secret,
        "proxy_url": proxy_url,
        "expected_workspace": expected_workspace,
        "request_id": format!("acc-{account_id}-{}", now_ms()),
    });
    state.bps_auth.set_stage("login");
    let timeout = Duration::from_secs(
        config.bps_auth_login_timeout_seconds.max(60) as u64 + LOGIN_TIMEOUT_SLACK_SECONDS,
    );
    let out = match call_service(config, "POST", "/login", Some(payload), timeout).await {
        Ok(value) => value,
        Err(err) => {
            state.bps_auth.mark_error(account_id, &err, path);
            return Err(err);
        }
    };
    if out.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let code = {
            let value = str_of(&out, "error");
            if value.is_empty() {
                "login_failed".to_string()
            } else {
                value
            }
        };
        let stage = str_of(&out, "stage");
        let message = if stage.is_empty() || stage == "unknown" {
            code
        } else {
            format!("{code} (stage={stage})")
        };
        state.bps_auth.mark_error(account_id, &message, path);
        return Err(message);
    }
    let credential = parse_login_credentials(&out, email, &expected_workspace)?;
    state.bps_auth.set_credential(account_id, credential, path);
    Ok(())
}

/// 目标账号在宿主上记录的 chatgpt_account_id（取不到就返回空，不阻断授权）。
async fn expected_workspace_of(config: &PluginConfig, account_id: i64) -> String {
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    if base.is_empty() || key.is_empty() {
        return String::new();
    }
    let Ok(targets) = crate::admin::fetch_all_targets(&base, &key).await else {
        return String::new();
    };
    targets
        .iter()
        .find(|row| row.account_id == account_id)
        .and_then(|row| row.chatgpt_account_id.clone())
        .unwrap_or_default()
}

/// 用 refresh_token 换一份新的 access_token。
pub async fn refresh_account(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    account_id: i64,
) -> Result<(), String> {
    let path = state_file(config);
    state.bps_auth.ensure_loaded(&path);
    let credential = state
        .bps_auth
        .credential(account_id)
        .ok_or_else(|| "该账号还没有 Excel 凭据".to_string())?;
    if credential.refresh_token.trim().is_empty() {
        let message = "Excel 凭据没有 refresh_token，需要重新授权".to_string();
        state.bps_auth.mark_error(account_id, &message, &path);
        return Err(message);
    }
    let payload = serde_json::json!({
        "refresh_token": credential.refresh_token,
        "proxy_url": config.bps_auth_proxy_url.trim(),
    });
    let out = match call_service(
        config,
        "POST",
        "/refresh",
        Some(payload),
        Duration::from_secs(REFRESH_TIMEOUT_SECONDS),
    )
    .await
    {
        Ok(value) => value,
        Err(err) => {
            state.bps_auth.mark_error(account_id, &err, &path);
            return Err(err);
        }
    };
    if out.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let message = {
            let value = str_of(&out, "error");
            if value.is_empty() {
                "refresh_failed".to_string()
            } else {
                value
            }
        };
        state.bps_auth.mark_error(account_id, &message, &path);
        return Err(message);
    }
    let tokens = out
        .get("credentials")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let next = apply_refresh(&credential, &tokens)?;
    state.bps_auth.set_credential(account_id, next, &path);
    Ok(())
}

/// BPS 出站用的 Excel access_token：够用就直接给，临近过期就刷一次，拿不到返回 None
/// （调用方保持宿主凭据不变）。
pub async fn access_token(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    account_id: i64,
) -> Option<String> {
    if !config.bps_auth_enabled || !config.bps_auth_use_for_bps {
        return None;
    }
    let path = state_file(config);
    state.bps_auth.ensure_loaded(&path);
    let margin = config.bps_auth_refresh_margin_seconds as u64 * 1000;
    if let Some(token) = state.bps_auth.token_if_valid(account_id, now_ms(), margin) {
        return Some(token);
    }
    if !config.bps_auth_auto_refresh {
        return None;
    }
    if let Err(err) = refresh_account(state, config, account_id).await {
        crate::bps::note_throttled(
            config,
            account_id,
            "bps-auth-refresh",
            NOTE_DEDUPE_MS,
            &format!("升级 BPS Excel 凭据失败，本条回退宿主凭据: {err}"),
        );
        return None;
    }
    let token = state.bps_auth.token_if_valid(account_id, now_ms(), 0);
    if token.is_none() {
        crate::bps::note_throttled(
            config,
            account_id,
            "bps-auth-refresh",
            NOTE_DEDUPE_MS,
            "刷新后 Excel 凭据仍不可用，本条回退宿主凭据",
        );
    }
    token
}

/// 后台循环：探活 + 到点把临近过期的 Excel 凭据刷一遍。
pub async fn refresh_loop(state: Arc<SharedState>) {
    let mut last_health_ms = 0u64;
    let mut last_sweep_ms = 0u64;
    loop {
        tokio::time::sleep(Duration::from_secs(REFRESH_TICK_SECONDS)).await;
        let config = state.current_config();
        if !config.bps_auth_enabled {
            continue;
        }
        let now = now_ms();
        if now.saturating_sub(last_health_ms) >= 60_000 {
            last_health_ms = now;
            probe_health(&state, &config).await;
        }
        if !config.bps_auth_auto_refresh {
            continue;
        }
        let interval_ms = config.bps_auth_refresh_interval_seconds.max(60) as u64 * 1000;
        if now.saturating_sub(last_sweep_ms) < interval_ms {
            continue;
        }
        last_sweep_ms = now;
        let path = state_file(&config);
        state.bps_auth.ensure_loaded(&path);
        let margin = config.bps_auth_refresh_margin_seconds as u64 * 1000;
        for account_id in state.bps_auth.credential_accounts() {
            if state
                .bps_auth
                .token_if_valid(account_id, now, margin)
                .is_some()
            {
                continue;
            }
            if let Err(err) = refresh_account(&state, &config, account_id).await {
                crate::bps::note_throttled(
                    &config,
                    account_id,
                    "bps-auth-sweep",
                    NOTE_DEDUPE_MS,
                    &format!("后台刷新 BPS Excel 凭据失败: {err}"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_is_parsed_from_account_name() {
        assert_eq!(
            email_from_name("oauth---lynette_bell1990@aol.com").as_deref(),
            Some("lynette_bell1990@aol.com")
        );
        assert_eq!(
            email_from_name("nybd5rc1qi@outlook.com").as_deref(),
            Some("nybd5rc1qi@outlook.com")
        );
        assert_eq!(email_from_name("oauth---no-at-sign"), None);
        assert_eq!(email_from_name(""), None);
    }

    #[test]
    fn expiring_tokens_are_not_reused() {
        let store = BpsAuthStore::default();
        store.ensure_loaded("");
        let mut credential = ExcelCredential {
            access_token: "tok".to_string(),
            expires_at_ms: 10_000,
            ..ExcelCredential::default()
        };
        store.set_credential(7, credential.clone(), "");
        assert!(store.token_if_valid(7, 1_000, 1_000).is_some());
        assert!(store.token_if_valid(7, 9_500, 1_000).is_none());
        credential.access_token = String::new();
        store.set_credential(7, credential, "");
        assert!(store.token_if_valid(7, 1_000, 0).is_none());
    }

    #[test]
    fn refresh_keeps_old_refresh_token_when_absent() {
        let old = ExcelCredential {
            access_token: "old".to_string(),
            refresh_token: "keep-me".to_string(),
            expires_at_ms: 1,
            ..ExcelCredential::default()
        };
        let tokens = serde_json::json!({"access_token": "new", "expires_in": 600});
        let next = apply_refresh(&old, &tokens).expect("refresh");
        assert_eq!(next.access_token, "new");
        assert_eq!(next.refresh_token, "keep-me");
        assert!(next.expires_at_ms > crate::donor::now_ms());
    }

    #[test]
    fn iso_timestamps_are_parsed() {
        assert_eq!(iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_ms("2026-10-19T02:39:18.000Z"), Some(1_792_377_558_000));
        assert_eq!(iso_ms("not-a-date"), None);
    }
}
