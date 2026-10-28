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

use std::collections::{BTreeMap, BTreeSet};
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
/// 出口探测超时：只用来判断「这个代理连不通吗」，别在这里耗太久。
const PROXY_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(20);
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

/// 批量授权（面板「批量登录 BPS」/ 备注自动授权）的进度快照，面板轮询用。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BatchRun {
    pub running: bool,
    /// 本轮计划授权的账号数（已经剔掉没有登录材料的）。
    pub total: usize,
    /// 没有登录材料、直接跳过的账号数。
    pub skipped: usize,
    pub done: usize,
    pub ok: usize,
    pub failed: usize,
    pub concurrency: u32,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    /// 最近在处理的账号 / 阶段。
    pub account_id: i64,
    pub stage: String,
    /// 最近一次失败摘要（`#账号 原因`，不含任何凭据）。
    pub error: String,
}

/// 一轮批量授权的最终结果（调用方拿它写报表 / 日志）。
#[derive(Debug, Clone, Default)]
pub struct BatchOutcome {
    pub total: usize,
    pub skipped: usize,
    pub ok: usize,
    pub failed: usize,
    pub error: String,
}

/// 批量授权的执行参数。
#[derive(Debug, Clone, Copy)]
pub struct BatchOptions {
    pub concurrency: usize,
}

impl BatchOptions {
    /// 按插件配置取并发（钳到 1..=`MAX_BATCH_CONCURRENCY`）。
    pub fn from_config(config: &PluginConfig) -> Self {
        Self {
            concurrency: config.bps_auth_batch_concurrency.clamp(1, MAX_BATCH_CONCURRENCY as u32)
                as usize,
        }
    }

    /// 面板可以临时指定一个并发（不写回配置）。
    pub fn with_concurrency(mut self, concurrency: Option<u32>) -> Self {
        if let Some(value) = concurrency {
            self.concurrency = value.clamp(1, MAX_BATCH_CONCURRENCY as u32) as usize;
        }
        self
    }
}

/// 授权槽位来源：手动同时只允许一个；批量可以多个账号并行（同账号仍不会重叠）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginSlot {
    Manual,
    Batch,
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
    /// 最近一次授权实际用的出口（已去掉用户名密码），含回退原因。
    pub proxy_note: String,
    pub proxy_note_at_ms: u64,
}

/// 备注同步的一次结论（面板展示用；不含任何凭据）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NotesSyncReport {
    /// 本轮同步开始时间。
    pub at_ms: u64,
    /// 同步是否跑通（管理 API 拿到账号列表）。
    pub ok: bool,
    /// 跑不通时的原因（直接给面板看）。
    pub error: String,
    /// 扫过的账号数。
    pub scanned: usize,
    /// 备注里带登录材料的账号数。
    pub with_material: usize,
    /// 材料相比上一轮发生变化的账号数。
    pub changed: usize,
    /// 本轮排队去授权的账号数。
    pub queued: usize,
    /// 备注里没有材料的账号数（这些号不会走 BPS）。
    pub skipped: usize,
    /// 本轮自动授权的结果（只统计 `bps_auth_notes_auto_login` 排到队的那些账号）。
    pub login_ok: usize,
    pub login_failed: usize,
    /// 最近一次自动授权失败的摘要（`#账号 原因`，不含任何凭据）。
    pub login_error: String,
}

/// 备注同步每轮最多排队几个账号去授权（bps-auth 侧车并发有限，别一次压爆）。
const NOTES_LOGIN_PER_PASS: usize = 3;
/// 同一账号重复入队授权的去重窗口（避免每条请求都排一次）。
const LOGIN_QUEUE_DEDUPE_MS: u64 = 900_000;
/// 批量授权的并发上限（再多也只是在 bps-auth 侧排队）。
const MAX_BATCH_CONCURRENCY: usize = 8;

#[derive(Default)]
struct Inner {
    secrets: BTreeMap<i64, AccountSecret>,
    credentials: BTreeMap<i64, ExcelCredential>,
    run: AuthRun,
    batch: BatchRun,
    /// 正在授权的账号（手动 + 批量共用，防止同一个账号被并发登两次）。
    running_accounts: BTreeSet<i64>,
    /// 还没拿到凭据就失败的账号 → (原因, 时间)。面板要能看见「这个号为什么没授权成功」，
    /// 这些账号连凭据条目都还没有，所以单独放一份（只存内存，重启后靠下一轮授权重填）。
    errors: BTreeMap<i64, (String, u64)>,
    /// 最近一次授权实际用的出口（已脱敏）+ 时间，面板展示用。
    proxy_notes: BTreeMap<i64, (String, u64)>,
    health: ServiceHealth,
    notes_sync: NotesSyncReport,
    /// 已经排过授权的账号 → 入队时间（去重，防止重复排队）。
    login_queue: BTreeMap<i64, u64>,
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

    /// 面板上的批量授权进度快照。
    pub fn batch_state(&self) -> BatchRun {
        self.lock().batch.clone()
    }

    /// 手动单个在跑、或批量在跑，都算「忙」（面板按钮据此禁用、备注同步据此让路）。
    pub fn is_running(&self) -> bool {
        let guard = self.lock();
        guard.run.running || guard.batch.running
    }

    pub fn health(&self) -> ServiceHealth {
        self.lock().health.clone()
    }

    fn set_health(&self, health: ServiceHealth) {
        self.lock().health = health;
    }

    /// 占一个授权槽位：手动独占，批量按账号去重（同一个账号不会同时登两次）。
    fn begin_run_slot(&self, account_id: i64, slot: LoginSlot) -> bool {
        let mut guard = self.lock();
        if guard.running_accounts.contains(&account_id) {
            return false;
        }
        if slot == LoginSlot::Manual {
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
        }
        guard.running_accounts.insert(account_id);
        if slot == LoginSlot::Batch {
            guard.batch.account_id = account_id;
            guard.batch.stage = "开始授权".to_string();
        }
        true
    }

    fn set_stage(&self, stage: &str) {
        let mut guard = self.lock();
        guard.run.stage = stage.to_string();
        if guard.batch.running {
            guard.batch.stage = stage.to_string();
        }
    }

    fn finish_run_slot(&self, account_id: i64, slot: LoginSlot, error: String) {
        let mut guard = self.lock();
        guard.running_accounts.remove(&account_id);
        if slot == LoginSlot::Manual {
            guard.run.running = false;
            guard.run.finished_at_ms = now_ms();
            guard.run.error = error;
        }
    }

    /// 开一轮批量：重置计数。
    fn begin_batch(&self, total: usize, skipped: usize, concurrency: u32) {
        let mut guard = self.lock();
        guard.batch = BatchRun {
            running: true,
            total,
            skipped,
            concurrency,
            started_at_ms: now_ms(),
            stage: "开始".to_string(),
            ..BatchRun::default()
        };
    }

    /// 记一个账号的批量结果（`error` 为空 = 成功）。
    fn batch_add_result(&self, account_id: i64, error: &str) {
        let mut guard = self.lock();
        guard.batch.done += 1;
        if error.trim().is_empty() {
            guard.batch.ok += 1;
        } else {
            guard.batch.failed += 1;
            guard.batch.error = format!("#{account_id} {error}").chars().take(300).collect();
        }
    }

    fn finish_batch(&self) {
        let mut guard = self.lock();
        guard.batch.running = false;
        guard.batch.stage = "结束".to_string();
        guard.batch.finished_at_ms = now_ms();
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
        {
            let mut guard = self.lock();
            guard.credentials.insert(account_id, credential);
            guard.errors.remove(&account_id);
        }
        self.persist(path);
    }

    fn mark_error(&self, account_id: i64, message: &str, path: &str) {
        {
            let mut guard = self.lock();
            if let Some(credential) = guard.credentials.get_mut(&account_id) {
                credential.last_error = message.to_string();
                credential.last_error_at_ms = now_ms();
            } else {
                guard
                    .errors
                    .insert(account_id, (message.to_string(), now_ms()));
            }
        }
        self.persist(path);
    }

    /// 记一条「这次授权走的哪个出口」的结论（面板展示，已去掉用户名密码）。
    fn set_proxy_note(&self, account_id: i64, note: String) {
        let mut guard = self.lock();
        if note.trim().is_empty() {
            guard.proxy_notes.remove(&account_id);
        } else {
            guard.proxy_notes.insert(account_id, (note, now_ms()));
        }
    }

    /// 某个账号最近一次授权的出口结论（文本, 时间）——与 `errors` 同一种排布。
    pub fn proxy_note(&self, account_id: i64) -> Option<(String, u64)> {
        self.lock().proxy_notes.get(&account_id).cloned()
    }

    /// 清掉一个账号的密码 + 凭据（换号 / 停用时用）。
    pub fn clear_account(&self, account_id: i64, path: &str) {
        {
            let mut guard = self.lock();
            guard.secrets.remove(&account_id);
            guard.credentials.remove(&account_id);
            guard.errors.remove(&account_id);
            guard.proxy_notes.remove(&account_id);
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

    /// 该账号是否已经有登录材料（来自备注同步或面板手填）。
    pub fn has_secret(&self, account_id: i64) -> bool {
        self.lock()
            .secrets
            .get(&account_id)
            .map(|secret| !secret.password.trim().is_empty())
            .unwrap_or(false)
    }

    /// 用备注（或面板）里的材料覆盖已保存的那份；返回是否真的变了。
    ///
    /// 空字段不覆盖（沿用面板「留空不改」的语义）：备注只写邮箱 / 密码时，
    /// 不会把原先存好的 TOTP 清掉。
    pub fn apply_material(&self, account_id: i64, secret: AccountSecret, path: &str) -> bool {
        let changed = {
            let mut guard = self.lock();
            let entry = guard.secrets.entry(account_id).or_default();
            let mut changed = false;
            if !secret.email.trim().is_empty() && entry.email != secret.email.trim() {
                entry.email = secret.email.trim().to_string();
                changed = true;
            }
            if !secret.password.is_empty() && entry.password != secret.password {
                entry.password = secret.password.clone();
                changed = true;
            }
            if !secret.totp_secret.trim().is_empty()
                && entry.totp_secret != secret.totp_secret.trim()
            {
                entry.totp_secret = secret.totp_secret.trim().to_string();
                changed = true;
            }
            if !secret.proxy_url.trim().is_empty() && entry.proxy_url != secret.proxy_url.trim() {
                entry.proxy_url = secret.proxy_url.trim().to_string();
                changed = true;
            }
            if changed {
                entry.updated_at_ms = now_ms();
            }
            changed
        };
        if changed {
            self.persist(path);
        }
        changed
    }

    /// 凭据是否已就绪（存在，且不在提前刷新窗口内）。
    pub fn credential_ready(&self, account_id: i64, now: u64, margin_ms: u64) -> bool {
        self.lock()
            .credentials
            .get(&account_id)
            .map(|credential| {
                !credential.access_token.trim().is_empty()
                    && credential.expires_at_ms > now.saturating_add(margin_ms)
            })
            .unwrap_or(false)
    }

    /// 面板展示用的备注同步结论。
    pub fn notes_sync_report(&self) -> NotesSyncReport {
        self.lock().notes_sync.clone()
    }

    fn set_notes_sync_report(&self, report: NotesSyncReport) {
        self.lock().notes_sync = report;
    }

    /// 记一次「已排队去授权」；同一个账号在去重窗口内只成功一次。
    fn mark_login_queued(&self, account_id: i64) -> bool {
        let now = now_ms();
        let mut guard = self.lock();
        // 顺手清掉过期的排队记录，免得这张表无限长。
        guard
            .login_queue
            .retain(|_, stamp| now.saturating_sub(*stamp) < LOGIN_QUEUE_DEDUPE_MS);
        if guard.login_queue.contains_key(&account_id) {
            return false;
        }
        guard.login_queue.insert(account_id, now);
        true
    }

    /// 授权成功后清掉排队记录：同一账号下次真缺凭据时能立刻重新排队。
    fn clear_login_queued(&self, account_id: i64) {
        self.lock().login_queue.remove(&account_id);
    }

    pub fn view(&self, account_id: i64, now: u64) -> AccountAuthView {
        let guard = self.lock();
        let secret = guard.secrets.get(&account_id);
        let credential = guard.credentials.get(&account_id);
        let pending_error = guard.errors.get(&account_id);
        let proxy_note = guard.proxy_notes.get(&account_id);
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
            last_error: credential
                .map(|c| c.last_error.clone())
                .filter(|value| !value.is_empty())
                .or_else(|| pending_error.map(|(message, _)| message.clone()))
                .unwrap_or_default(),
            last_error_at_ms: credential
                .map(|c| c.last_error_at_ms)
                .filter(|value| *value > 0)
                .or_else(|| pending_error.map(|(_, at)| *at))
                .unwrap_or(0),
            workspace_hint: credential
                .map(|c| c.chatgpt_account_id.chars().take(8).collect())
                .unwrap_or_default(),
            proxy_note: proxy_note.map(|(text, _)| text.clone()).unwrap_or_default(),
            proxy_note_at_ms: proxy_note.map(|(_, at)| *at).unwrap_or(0),
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

/// 邮箱允许出现的字符（`----` 左边那一段按它切）。
fn is_email_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '@' | '.' | '_' | '%' | '+' | '-' | '\'')
}

/// TOTP 密钥是 base32：只取开头连续合法的字符，长度够就当密钥用。
fn clean_totp_secret(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        let upper = ch.to_ascii_uppercase();
        if matches!(upper, 'A'..='Z' | '2'..='7') {
            out.push(upper);
        } else {
            break;
        }
    }
    if out.len() >= 8 {
        out
    } else {
        String::new()
    }
}

/// 从账号备注（简介）里解析 BPS 登录材料：`邮箱----密码[----TOTP]`。
///
/// 备注是运营手写的，所以只要能在一段里找出这个「以 `----` 分隔、左边是邮箱」的模式
/// 就认，允许前后夹着说明文字（中英文、标点都行）。TOTP 段可以缺省：账号没开两步验证
/// 时本来就没有；真正需要 TOTP 的账号登录会失败，插件按「没材料」处理，
/// 该账号这一轮直接走正常通道。
pub fn parse_account_material(notes: &str) -> Option<AccountSecret> {
    if notes.trim().is_empty() {
        return None;
    }
    let mut search_from = 0usize;
    while let Some(offset) = notes[search_from..].find("----") {
        let idx = search_from + offset;
        // 分隔符左边只取尾部连续的邮箱字符，允许备注里写着「账号：xxx@yyy」。
        let head = &notes[..idx];
        let start = head
            .char_indices()
            .rev()
            .find(|(_, ch)| !is_email_char(*ch))
            .map(|(index, ch)| index + ch.len_utf8())
            .unwrap_or(0);
        let email = head[start..].trim();
        // 右边第一段是密码：到下一个 `----` / 换行 / 行尾为止（密码里可以带空格）。
        let rest = notes[idx..].trim_start_matches('-');
        let (password_raw, tail) = cut_material_segment(rest);
        let password = password_raw.trim();
        if !email.is_empty() && email.contains('@') && !password.is_empty() {
            let after = tail.trim_start_matches('-').trim_start();
            let (totp_raw, _) = cut_material_segment(after);
            return Some(AccountSecret {
                email: email.to_string(),
                password: password.to_string(),
                totp_secret: pick_totp_secret(totp_raw),
                proxy_url: String::new(),
                updated_at_ms: now_ms(),
            });
        }
        search_from = idx + 4;
    }
    None
}

/// 切出一段材料：遇到下一个 `----` 分隔符、换行或字符串结尾就停。
fn cut_material_segment(text: &str) -> (&str, &str) {
    let stop = ["----", "\n", "\r"]
        .iter()
        .filter_map(|sep| text.find(sep))
        .min()
        .unwrap_or(text.len());
    text.split_at(stop)
}

/// 从 TOTP 那一段里挑出 Base32 密钥：备注常写成「2FA 密钥 xxxx」，所以按非字母数字切词，
/// 取第一个长度够的候选。
fn pick_totp_secret(segment: &str) -> String {
    for token in segment.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        let cleaned = clean_totp_secret(token);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    String::new()
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

/// 这次授权可以用的出口候选，按优先级排列；空串 = 直连（永远排最后）。
fn proxy_candidates(
    config: &PluginConfig,
    secret: &AccountSecret,
    account_id: i64,
) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let own = secret.proxy_url.trim();
    if !own.is_empty() {
        out.push((own.to_string(), "账号自己的代理"));
    }
    let template = config.bps_auth_proxy_template.trim();
    if !template.is_empty() {
        out.push((expand_proxy_template(template, account_id), "一账号一出口"));
    }
    let global = config.bps_auth_proxy_url.trim();
    if !global.is_empty() {
        out.push((global.to_string(), "全局授权代理"));
    }
    out.push((String::new(), "直连"));
    out
}

/// 代理串里去掉用户名 / 密码，只留 `<scheme>host:port`：面板要能看，但不能回显凭据。
fn mask_proxy(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return "直连".to_string();
    }
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), trimmed),
    };
    let host = match rest.rsplit_once('@') {
        Some((_, host)) => host,
        None => rest,
    };
    format!("{scheme}{host}")
}

/// 探一次出口：能通过它读到出口 IP 才算可用（返回值就是探测到的出口地址）。
async fn preflight_proxy(proxy_url: &str, timeout: Duration) -> Result<String, String> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if proxy_url.trim().is_empty() {
        builder = builder.no_proxy();
    } else {
        let proxy = reqwest::Proxy::all(proxy_url.trim())
            .map_err(|err| format!("代理格式不对: {err}"))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|err| format!("构造探测客户端失败: {err}"))?;
    let response = client
        .get("https://api64.ipify.org")
        .send()
        .await
        .map_err(|err| crate::transport::safe_reqwest_error(&err))?;
    if !response.status().is_success() {
        return Err(format!("探测返回 HTTP {}", response.status().as_u16()));
    }
    let body = response
        .text()
        .await
        .map_err(|err| format!("读取探测结果失败: {err}"))?;
    let ip = body.trim().to_string();
    if ip.is_empty() || ip.len() > 64 {
        return Err("探测结果为空".to_string());
    }
    Ok(ip)
}

/// 定这次授权的出口：默认先探一遍候选，用第一个能连通的，并把结论记到面板。
async fn resolve_login_proxy_checked(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    secret: &AccountSecret,
    account_id: i64,
) -> String {
    let candidates = proxy_candidates(config, secret, account_id);
    if !config.bps_auth_proxy_fallback {
        return candidates
            .first()
            .map(|(url, _)| url.clone())
            .unwrap_or_default();
    }
    let mut failures: Vec<String> = Vec::new();
    for (url, label) in &candidates {
        match preflight_proxy(url, PROXY_PREFLIGHT_TIMEOUT).await {
            Ok(ip) => {
                let note = if failures.is_empty() {
                    format!("授权出口：{label} {}（探测 {ip}）", mask_proxy(url))
                } else {
                    format!(
                        "授权出口：{label} {}（探测 {ip}；已回退，{}）",
                        mask_proxy(url),
                        failures.join("；")
                    )
                };
                state.bps_auth.set_proxy_note(account_id, note);
                return url.clone();
            }
            Err(err) => failures.push(format!("{label}不可用：{err}")),
        }
    }
    // 候选全不通：照旧拿优先级最高的那个去试，让真正的失败原因冒到面板上。
    state.bps_auth.set_proxy_note(
        account_id,
        format!("授权出口全部探测失败（{}）", failures.join("；")),
    );
    candidates
        .first()
        .map(|(url, _)| url.clone())
        .unwrap_or_default()
}

/// 按模板现算这个账号的登录出口，例：
/// `http://acct{account}-{rand}:worinima@142.54.187.42:49000`。
///
/// `{account}` 换成账号 id，`{rand}` 换成 8 位随机十六进制：我们的代理容器按用户名
/// 分配出口 IP，随机后缀让每个账号（每次授权）拿到不同出口，不至于几个账号挤在同一个
/// IP 上过 Cloudflare 校验。没写占位符时原样返回（只有前后空白被去掉）。
pub fn expand_proxy_template(template: &str, account_id: i64) -> String {
    let mut out = template.trim().replace("{account}", &account_id.to_string());
    while out.contains("{rand}") {
        out = out.replacen("{rand}", &random_hex(8), 1);
    }
    out
}

fn random_hex(len: usize) -> String {
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(len)
        .collect()
}

/// 出站门禁：这个账号此刻有没有一枚能打的 Excel 凭据。
///
/// **只看「出站改用 Excel 凭据」这一个开关，不看 BPS 授权总开关**：以前总开关关着时
/// 门禁整体失效，插件就会拿宿主 bearer 去打 BPS —— 结果必然是 403（实测），白撞一次
/// 还会触发「403 自动摘除模型」，把账号上的 gpt-6-astra / gpt-5.6-sol 摘掉 6 小时。
/// 现在只要没在配置里明确要求「沿用宿主 bearer」（`bps_auth_use_for_bps = false`，A/B 对照
/// 或特殊场景才用），没有可用 Excel 凭据的账号一律不放行。
pub fn outbound_credential_ready(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    account_id: i64,
    now: u64,
) -> bool {
    let path = state_file(config);
    state.bps_auth.ensure_loaded(&path);
    let ready = state.bps_auth.credential_ready(account_id, now, 0);
    !outbound_gate_blocks(config.bps_auth_use_for_bps, ready)
}

/// 门禁的纯逻辑部分（好写测试）：只有「明确要求沿用宿主 bearer」这一种情况放行，
/// 其余情况没有可用的 Excel 凭据就拦。BPS 授权总开关**不参与**这个判断。
fn outbound_gate_blocks(use_excel_for_bps: bool, credential_ready: bool) -> bool {
    use_excel_for_bps && !credential_ready
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
    login_account_with(state, config, account_id, override_secret, LoginSlot::Manual).await
}

/// 批量授权里跑一个账号：和手动授权共用同一套材料 / 日志，但允许多个账号并行
/// （同一个账号仍不会同时登两次）。
pub async fn login_account_batch(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    account_id: i64,
) -> Result<(), String> {
    login_account_with(state, config, account_id, None, LoginSlot::Batch).await
}

async fn login_account_with(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    account_id: i64,
    override_secret: Option<AccountSecret>,
    slot: LoginSlot,
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
    // 出口候选先探一遍再定（默认行为）：写错的模板不会把整批授权拖死，
    // 实际用了哪个出口会记在面板的账号行上（不含用户名密码）。
    state.bps_auth.set_stage("探测授权出口");
    let proxy = resolve_login_proxy_checked(state, config, &secret, account_id).await;
    if !state.bps_auth.begin_run_slot(account_id, slot) {
        return Err(if slot == LoginSlot::Manual {
            "已有一次授权在进行中，请等它结束".to_string()
        } else {
            "该账号正在授权中".to_string()
        });
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
    state.bps_auth.finish_run_slot(account_id, slot, error);
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

/// 批量跑一轮授权：按 `options.concurrency` 分批并行，同一个账号一次（不会重叠）。
///
/// 没有登录材料的账号直接算「跳过」——它们授权本来也拿不到凭据，面板上不该显示成失败。
/// 进度写进 `state.bps_auth.batch_state()`（面板轮询它显示进度），返回值给调用方
/// （备注同步用它填自己的报表）。
pub async fn run_batch_login(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    ids: Vec<i64>,
    options: BatchOptions,
) -> BatchOutcome {
    let path = state_file(config);
    state.bps_auth.ensure_loaded(&path);
    let mut todo: Vec<i64> = Vec::new();
    let mut outcome = BatchOutcome::default();
    for account_id in ids {
        if state.bps_auth.has_secret(account_id) {
            todo.push(account_id);
        } else {
            outcome.skipped += 1;
        }
    }
    outcome.total = todo.len();
    let concurrency = options.concurrency.clamp(1, MAX_BATCH_CONCURRENCY);
    state
        .bps_auth
        .begin_batch(todo.len(), outcome.skipped, concurrency as u32);
    if todo.is_empty() {
        state.bps_auth.finish_batch();
        return outcome;
    }
    for chunk in todo.chunks(concurrency) {
        let futures = chunk.iter().map(|account_id| {
            let account_id = *account_id;
            async move {
                let config = state.current_config();
                (
                    account_id,
                    login_account_batch(state, &config, account_id).await,
                )
            }
        });
        for (account_id, result) in futures_util::future::join_all(futures).await {
            let error = match result {
                Ok(()) => {
                    outcome.ok += 1;
                    // 授权成功后清掉排队记录：这个号下次真缺凭据时能立刻重排。
                    state.bps_auth.clear_login_queued(account_id);
                    String::new()
                }
                Err(err) => {
                    outcome.failed += 1;
                    outcome.error = format!("#{account_id} {err}").chars().take(300).collect();
                    crate::bps::note_throttled(
                        config,
                        account_id,
                        "bps-auth-batch",
                        NOTE_DEDUPE_MS,
                        &format!("批量 BPS 授权失败: {err}"),
                    );
                    err
                }
            };
            state.bps_auth.batch_add_result(account_id, &error);
        }
    }
    state.bps_auth.finish_batch();
    outcome
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

/// 只做「账号备注 → 登录材料」这一步（不授权）：抓回备注、解析、写进凭据仓。
///
/// 返回 (本轮报表, 有材料但还没有可用 Excel 凭据的账号)。备注里没有材料的账号原样跳过，
/// 已存的材料不会被清空。
pub async fn sync_notes_materials(
    state: &Arc<SharedState>,
    config: &PluginConfig,
) -> (NotesSyncReport, Vec<i64>) {
    let path = state_file(config);
    state.bps_auth.ensure_loaded(&path);
    let mut report = NotesSyncReport {
        at_ms: now_ms(),
        ..NotesSyncReport::default()
    };
    let mut pending: Vec<i64> = Vec::new();
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    match crate::admin::fetch_account_notes(&base, &key).await {
        Ok(rows) => {
            report.ok = true;
            for row in rows {
                report.scanned += 1;
                let Some(secret) = parse_account_material(&row.notes) else {
                    report.skipped += 1;
                    continue;
                };
                report.with_material += 1;
                if state.bps_auth.apply_material(row.account_id, secret, &path) {
                    report.changed += 1;
                }
                if !state.bps_auth.credential_ready(row.account_id, now_ms(), 0) {
                    pending.push(row.account_id);
                }
            }
        }
        Err(err) => report.error = err,
    }
    (report, pending)
}

/// 跑一轮「账号备注 → 登录材料 → 自动授权」同步，并把结论缓存给面板。
///
/// 有材料但还没有可用 Excel 凭据的账号排队授权，每轮最多 `NOTES_LOGIN_PER_PASS` 个
/// （授权要过 Cloudflare 校验，一次太多只会让每个都更慢）；面板上的「批量登录」可以
/// 一次把缺凭据的号都排上（走同一条批量通道）。
pub async fn sync_notes_once(state: &Arc<SharedState>, config: &PluginConfig) -> NotesSyncReport {
    let (mut report, pending) = sync_notes_materials(state, config).await;
    // 已经有一轮批量在跑就别再插队：排队记录留到下一轮再打。
    if !config.bps_auth_enabled
        || !config.bps_auth_notes_auto_login
        || state.bps_auth.batch_state().running
    {
        state.bps_auth.set_notes_sync_report(report.clone());
        return report;
    }
    let mut queue: Vec<i64> = Vec::new();
    for account_id in pending {
        if queue.len() >= NOTES_LOGIN_PER_PASS {
            break;
        }
        if state.bps_auth.mark_login_queued(account_id) {
            queue.push(account_id);
        }
    }
    report.queued = queue.len();
    state.bps_auth.set_notes_sync_report(report.clone());
    if queue.is_empty() {
        return report;
    }
    // 走和面板「批量登录 BPS」同一条通道：并发按配置来，每个账号单独取出口
    // （配了 `bps_auth_proxy_template` 的话），省得几个号挤一个 IP 过 Cloudflare。
    let outcome = run_batch_login(state, config, queue, BatchOptions::from_config(config)).await;
    report.login_ok = outcome.ok;
    report.login_failed = outcome.failed;
    report.login_error = outcome.error;
    state.bps_auth.set_notes_sync_report(report.clone());
    report
}

/// 后台循环：备注同步 + 探活 + 到点把临近过期的 Excel 凭据刷一遍。
pub async fn refresh_loop(state: Arc<SharedState>) {
    let mut last_health_ms = 0u64;
    let mut last_sweep_ms = 0u64;
    let mut last_notes_ms = 0u64;
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
        // 备注同步：把运营写在账号「简介 / 备注」里的登录材料（`邮箱----密码----TOTP`）
        // 抓进来，顺手给还没有 Excel 凭据的账号排队授权。没有材料的账号不会被碰，
        // 出站门禁会让它们直接走正常通道。
        if config.bps_auth_notes_enabled {
            let notes_ms = config.bps_auth_notes_sync_seconds.max(60) as u64 * 1000;
            if now.saturating_sub(last_notes_ms) >= notes_ms {
                last_notes_ms = now_ms();
                sync_notes_once(&state, &config).await;
            }
        }
        if !config.bps_auth_auto_refresh {
            continue;
        }
        let now = now_ms();
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

    /// 没有 Excel 凭据时，除了「明确要求沿用宿主 bearer」，一律拦下 BPS —— 与 BPS 授权
    /// 总开关无关。以前总开关关着门禁整体失效，插件会拿宿主 bearer 去打 BPS，换回 403
    /// 并连带把账号上的模型摘掉 6 小时。
    #[test]
    fn outbound_gate_ignores_bps_auth_master_switch() {
        assert!(outbound_gate_blocks(true, false));
        assert!(!outbound_gate_blocks(true, true));
        assert!(!outbound_gate_blocks(false, false));
        assert!(!outbound_gate_blocks(false, true));
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

    #[test]
    fn notes_material_is_parsed() {
        // 运营写在「简介 / 备注」里的标准形态。
        let secret = parse_account_material(
            "burnettean9778@aol.com----uIRLKoJwouwBkhIL----DJTEFLBMN53SBSHSFL5RVSGWC2C2ZCQM",
        )
        .expect("标准材料");
        assert_eq!(secret.email, "burnettean9778@aol.com");
        assert_eq!(secret.password, "uIRLKoJwouwBkhIL");
        assert_eq!(secret.totp_secret, "DJTEFLBMN53SBSHSFL5RVSGWC2C2ZCQM");

        // 夹着中文说明、没有 TOTP 的账号。
        let secret =
            parse_account_material("这号带 2FA 关掉了，材料：foo.bar+1@aol.com----Passw0rd!2026---- 无")
                .expect("夹说明文字");
        assert_eq!(secret.email, "foo.bar+1@aol.com");
        assert_eq!(secret.password, "Passw0rd!2026");
        assert!(secret.totp_secret.is_empty());

        // TOTP 写在下一行、前面还带中文时也要认出来。
        let secret = parse_account_material("x@y.com----pwd2026----\n2FA 密钥 jbsWY3DPEHPK3PXP")
            .expect("换行 + 中文前缀");
        assert_eq!(secret.totp_secret, "JBSWY3DPEHPK3PXP");

        // 密码里带空格也不该被截断。
        let secret = parse_account_material("a b account: z9@aol.com----pa ss word----").expect("密码带空格");
        assert_eq!(secret.email, "z9@aol.com");
        assert_eq!(secret.password, "pa ss word");
    }

    #[test]
    fn notes_without_material_is_rejected() {
        assert!(parse_account_material("").is_none());
        assert!(parse_account_material("   ").is_none());
        assert!(parse_account_material("号商给的，还没测过").is_none());
        assert!(parse_account_material("----没有邮箱----").is_none());
        assert!(parse_account_material("no-at-sign----pwd").is_none());
        assert!(parse_account_material("only@email.com----").is_none());
    }

    #[test]
    fn notes_material_merge_keeps_unknown_fields() {
        let store = BpsAuthStore::default();
        store.ensure_loaded("");
        let full = AccountSecret {
            email: "a@b.com".to_string(),
            password: "p1".to_string(),
            totp_secret: "JBSWY3DPEHPK3PXP".to_string(),
            ..AccountSecret::default()
        };
        assert!(store.apply_material(1, full, ""));
        // 备注里只写「邮箱----密码」时，已经存好的 TOTP 不能被清掉。
        let partial = parse_account_material("a@b.com----p1").expect("partial");
        assert!(partial.totp_secret.is_empty());
        assert!(!store.apply_material(1, partial, ""));
        assert_eq!(
            store.secret(1).map(|s| s.totp_secret).unwrap_or_default(),
            "JBSWY3DPEHPK3PXP"
        );
        assert!(store.has_secret(1));
        assert!(!store.has_secret(2));
    }

    #[test]
    fn login_proxy_template_is_expanded_per_account() {
        let template = "http://acct{account}-{rand}:65423140@142.54.187.42:49000";
        let first = expand_proxy_template(template, 1234);
        assert!(first.starts_with("http://acct1234-"), "{first}");
        assert!(first.ends_with(":65423140@142.54.187.42:49000"), "{first}");
        assert_ne!(
            first,
            expand_proxy_template(template, 1234),
            "每次授权的随机后缀应该不同"
        );
        assert_eq!(
            expand_proxy_template("  http://a:b@h:1  ", 7),
            "http://a:b@h:1"
        );
    }

    #[test]
    fn login_proxy_candidates_follow_priority_and_end_with_direct() {
        let mut config = PluginConfig::default();
        config.bps_auth_proxy_template = "http://acct{account}:65423140@142.54.187.42:49000".to_string();
        config.bps_auth_proxy_url = "http://randbpsauth:65423140@142.54.187.42:49000".to_string();
        let mut own = AccountSecret::default();
        own.proxy_url = "http://own:1@10.0.0.1:1".to_string();
        let labels: Vec<&str> = proxy_candidates(&config, &own, 98)
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        assert_eq!(
            labels,
            vec!["账号自己的代理", "一账号一出口", "全局授权代理", "直连"]
        );
        // 没有账号自己的代理：模板在前、全局在后，最后一定是直连。
        let plain = AccountSecret::default();
        let urls: Vec<String> = proxy_candidates(&config, &plain, 98)
            .into_iter()
            .map(|(url, _)| url)
            .collect();
        assert_eq!(urls[0], "http://acct98:65423140@142.54.187.42:49000");
        assert_eq!(urls[1], "http://randbpsauth:65423140@142.54.187.42:49000");
        assert!(urls[2].is_empty(), "直连兜底");
        // 什么都没配：只剩直连。
        config.bps_auth_proxy_template.clear();
        config.bps_auth_proxy_url.clear();
        assert_eq!(proxy_candidates(&config, &plain, 98).len(), 1);
    }

    #[test]
    fn proxy_note_hides_credentials_and_shows_up_in_view() {
        assert_eq!(mask_proxy(""), "直连");
        assert_eq!(
            mask_proxy("http://acct98:65423140@142.54.187.42:49000"),
            "http://142.54.187.42:49000"
        );
        assert_eq!(mask_proxy("142.54.187.42:49000"), "142.54.187.42:49000");

        let store = BpsAuthStore::default();
        store.ensure_loaded("");
        assert!(store.proxy_note(3).is_none());
        store.set_proxy_note(3, "授权出口：一账号一出口 http://142.54.187.42:49000".to_string());
        let view = store.view(3, 1_000);
        assert!(view.proxy_note.contains("一账号一出口"));
        assert!(view.proxy_note_at_ms > 0);
        assert!(!view.proxy_note.contains("65423140"), "面板不能回显口令");
        store.set_proxy_note(3, String::new());
        assert!(store.proxy_note(3).is_none(), "空结论 = 清掉");
    }

    #[test]
    fn batch_slots_never_overlap_the_same_account() {
        let store = BpsAuthStore::default();
        assert!(store.begin_run_slot(5, LoginSlot::Batch));
        assert!(!store.begin_run_slot(5, LoginSlot::Batch), "同账号不能并发");
        assert!(store.begin_run_slot(6, LoginSlot::Batch), "不同账号可以并发");
        assert!(!store.begin_run_slot(6, LoginSlot::Manual), "同账号仍被占着");
        assert!(store.begin_run_slot(7, LoginSlot::Manual), "批量不挡别的手动");
        assert!(!store.begin_run_slot(8, LoginSlot::Manual), "手动同时只跑一个");
        store.finish_run_slot(7, LoginSlot::Manual, String::new());
        store.begin_batch(2, 1, 2);
        assert!(store.is_running());
        store.finish_run_slot(5, LoginSlot::Batch, String::new());
        store.batch_add_result(5, "");
        store.finish_run_slot(6, LoginSlot::Batch, "boom".to_string());
        store.batch_add_result(6, "boom");
        store.finish_batch();
        let batch = store.batch_state();
        assert_eq!(batch.total, 2);
        assert_eq!(batch.skipped, 1);
        assert_eq!(batch.done, 2);
        assert_eq!(batch.ok, 1);
        assert_eq!(batch.failed, 1);
        assert!(batch.error.starts_with("#6 "), "{}", batch.error);
        assert!(!batch.running);
        assert!(!store.is_running());
    }
}
