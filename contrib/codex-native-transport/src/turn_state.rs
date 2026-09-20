//! turn-state 诊断与(后续)养池。
//!
//! 当前阶段只做**观测**：把每次上游响应里的 x-codex-turn-state / openai-model
//! 与本次出站的 model / session / 已带 turn-state 落成 JSONL，用于实证：
//! - 上游到底在什么条件下才在响应头里铸 x-codex-turn-state；
//! - "跨 session 重放同一 turn-state"上游认不认（方案乙）。
//!
//! 只读观测、不改写请求。后续 pin 养池会在此模块扩展。

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 一条 turn-state 观测记录（JSONL 一行）。
#[derive(Debug, serde::Serialize)]
pub struct Observation {
    /// 观测时刻（unix 毫秒）。
    pub ts_ms: u128,
    /// 命中的上游账号（宿主在 ForwardRequestStart.account_id 给出）。
    pub account_id: i64,
    /// 本次出站请求体里的 model 字段（发给上游的模型名）。
    pub outbound_model: Option<String>,
    /// 上游 HTTP 状态码。
    pub status_code: i32,
    /// 本次出站请求携带的 session-id（真实 codex 的会话标识；空表示没带）。
    pub req_session_id: Option<String>,
    /// 宿主交给插件时（入站）就带的 x-codex-turn-state 长度
    /// （对比 req_turn_state_len 可看出插件是否在出站前剥掉了它）。
    pub req_in_turn_state_len: usize,
    /// 本次出站请求是否已带 x-codex-turn-state（以及其前缀/长度）。
    pub req_turn_state_prefix: Option<String>,
    pub req_turn_state_len: usize,
    /// 本次是否由实验通道(x-cnt-pin-turn-state)强行注入了 turn-state。
    pub pinned_injected: bool,
    /// 上游响应头里的 x-codex-turn-state（完整值，用于方案乙重放实验）。
    pub resp_turn_state: Option<String>,
    pub resp_turn_state_len: usize,
    /// 上游响应头里的 openai-model（服务端实际服务的模型）。
    pub resp_openai_model: Option<String>,
    /// 上游是否回了一个与出站不同的新 turn-state
    /// （自时钟：有新值通常意味着旧值已失效/被换发）。
    pub resp_turn_state_changed: bool,
    /// 来源路径：forward（真实流量）/ warm（主动养池）/ admin_warm（全池养池）。
    #[serde(default)]
    pub path: String,
    /// 本次走的铸票出口（host:port，隐去账密）；None = 宿主原代理 / 直连。
    #[serde(default)]
    pub egress: Option<String>,
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

/// 从请求体 JSON 中取 `model` 字段（非 JSON 或无该字段时返回 None）。
pub fn parse_model(body: &[u8]) -> Option<String> {
    let root: serde_json::Value = serde_json::from_slice(body).ok()?;
    root.get("model")?.as_str().map(str::to_string)
}

/// 请求是否要求推理（reasoning.effort ∈ {medium, high}）。用于门控实时降智判定，
/// 避免对本就不带推理(low/minimal/无)的请求误判为降智。
pub fn wants_reasoning(body: &[u8]) -> bool {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    root.get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str())
        .map(|e| matches!(e, "medium" | "high"))
        .unwrap_or(false)
}

/// 取字符串前缀（最多 n 字符），用于日志里标识 blob 而不落全量前缀。
pub fn prefix(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

fn resolve_path(configured: &str) -> PathBuf {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return PathBuf::from(trimmed);
    }
    std::env::temp_dir()
        .join("codex-native-transport")
        .join("turnstate-diag.jsonl")
}

/// 实验注入文件路径（与诊断文件同目录下的 pin-inject.txt）。
/// 文件内容两行：第一行 account_id，第二行要注入的 turn-state。
pub fn pin_inject_path(configured_diag_path: &str) -> PathBuf {
    let base = resolve_path(configured_diag_path);
    base.parent()
        .map(|dir| dir.join("pin-inject.txt"))
        .unwrap_or_else(|| PathBuf::from("pin-inject.txt"))
}

/// 读实验注入文件；返回 (account_id, turn_state)。用于方案乙实证：
/// 宿主会剥掉客户端自定义头，所以改由服务器端文件驱动注入，且按 account 限定
/// 只对目标号生效，把对其它流量的影响压到最小。
pub fn read_pin_inject(configured_diag_path: &str) -> Option<(i64, String)> {
    let content = std::fs::read_to_string(pin_inject_path(configured_diag_path)).ok()?;
    let mut lines = content.lines();
    let account_id: i64 = lines.next()?.trim().parse().ok()?;
    let turn_state = lines.next()?.trim().to_string();
    if turn_state.is_empty() {
        None
    } else {
        Some((account_id, turn_state))
    }
}

/// 追加一条观测到诊断文件（best-effort：失败静默，绝不影响转发）。
pub fn append_observation(configured_path: &str, obs: &Observation) {
    let path = resolve_path(configured_path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut line) = serde_json::to_vec(obs) else {
        return;
    };
    line.push(b'\n');
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(&line);
    }
}

// ---------------------------------------------------------------------------
// pin 养池：按 (account_id × 上游模型名) 维护一个 turn-state，出站注入以命中同后端。
// 被动引擎：overload 即刷新（清 turn-state 重新捕获），连续/高占比 overload 判失效；
// 一次干净成功即恢复。bearer/代理绝不进池。
// ---------------------------------------------------------------------------

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// 本次请求的结果分类（由 SSE 体扫描 / canary 判定得出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 正常完成（response.completed，无 overload）。
    Success,
    /// 命中 overload / 风控（server_is_overloaded / slow_down / 429 / 529 …）。
    Overload,
    /// 静默降智（canary 判定：reasoning_tokens==0 且无 reasoning 输出项 = 被换 gpt6）。
    /// 立即判失效，交由上层换号 / 冷却。
    Degraded,
    /// 其它错误（不据此判失效，仅计数）。
    OtherError,
}

/// canary 响应的降智判定：正常号 = 有 reasoning 输出项 或 reasoning_tokens>0。
/// 传入的是整段 SSE 文本（含 response.completed 事件）。
pub fn is_degraded_from_sse(sse: &str) -> bool {
    // 有 reasoning 输出项（"type":"reasoning" 且带 encrypted_content）= 正常。
    let has_reasoning_item =
        sse.contains("\"type\":\"reasoning\"") || sse.contains("\"type\": \"reasoning\"");
    // reasoning_tokens 的值（取最后一次出现）。
    let reasoning_tokens = last_reasoning_tokens(sse);
    match reasoning_tokens {
        Some(n) => n == 0 && !has_reasoning_item,
        None => !has_reasoning_item,
    }
}

/// 实时(生产流量)降智判定：比 canary 更严格，避免误伤本就不带推理的请求。
/// 仅当 reasoning_tokens 明确出现且为 0、且无 reasoning 输出项时判降智。
/// （reasoning_tokens 键完全缺失 = 非推理响应类型，不据此判定。）
pub fn reasoning_degraded_live(sse: &str) -> bool {
    let has_item =
        sse.contains("\"type\":\"reasoning\"") || sse.contains("\"type\": \"reasoning\"");
    matches!(last_reasoning_tokens(sse), Some(0)) && !has_item
}

/// 从 SSE 文本里取最后一次 "reasoning_tokens": N 的数值。
fn last_reasoning_tokens(sse: &str) -> Option<u64> {
    let key = "\"reasoning_tokens\"";
    let mut result = None;
    let mut from = 0usize;
    while let Some(pos) = sse[from..].find(key) {
        let start = from + pos + key.len();
        let rest = &sse[start..];
        // 跳过 ':' 和空白，读数字。
        let digits: String = rest
            .chars()
            .skip_while(|c| *c == ':' || c.is_whitespace())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse::<u64>() {
            result = Some(n);
        }
        from = start;
    }
    result
}

/// 兼容保留的经典 turn-state 票长。
pub const REAL6_TICKET_LEN: usize = 292;
/// 新版上游也会签发 332 字节的可用票。
pub const REAL6_TICKET_LEN_V2: usize = 332;

/// 只有已确认的可用票长才能进入锁池；其它长度继续放行并等待重新签发。
pub fn is_lockable_ticket_len(len: usize) -> bool {
    matches!(len, REAL6_TICKET_LEN | REAL6_TICKET_LEN_V2)
}

/// 一个 (account × model) 格的「智商档位」——通知只在它翻转时触发。
/// 由格的 degraded/failed 标记派生（与面板徽章同源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 真6可用（健康）。
    Ok,
    /// 降智（degraded 标记置位）。
    Degraded,
    /// 过载失效（failed 但非 degraded = overload 打的）。
    Overloaded,
    /// 放弃铸票：多轮「出口池转满仍无 292 → 休息」后判定当前铸不出真6，真实流量退回直通。
    Abandoned,
}

fn cell_tier(cell: &Cell) -> Tier {
    if cell.abandoned_until_ms > now_ms() as u64 {
        Tier::Abandoned
    } else if cell.degraded {
        Tier::Degraded
    } else if cell.failed {
        Tier::Overloaded
    } else {
        Tier::Ok
    }
}

/// 智商档位跃迁事件（池 → 通知任务，走无界 channel）。
#[derive(Debug, Clone)]
pub struct TierEvent {
    pub account_id: i64,
    pub model: String,
    pub from: Tier,
    pub to: Tier,
    pub ts_ms: u64,
}

/// pin 判失/过期参数（来自配置）。
#[derive(Debug, Clone, Copy)]
pub struct PinParams {
    pub max_age_ms: u64,
    pub fail_threshold: u32,
    pub fail_ratio_pct: u32,
    pub window_ms: u64,
    pub min_samples: usize,
}

impl PinParams {
    pub fn from_config(max_age_seconds: u32, fail_threshold: u32, fail_ratio_pct: u32) -> Self {
        Self {
            max_age_ms: max_age_seconds as u64 * 1000,
            fail_threshold,
            fail_ratio_pct,
            window_ms: 10 * 60 * 1000,
            min_samples: 6,
        }
    }
}

/// 一个 (account × model) 格的运行态 + 统计（可序列化用于持久化；不含任何 bearer）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cell {
    pub account_id: i64,
    pub model: String,
    /// 当前钉住的 turn-state（None = 空，待捕获）。
    pub turn_state: Option<String>,
    /// 我们钉住它的墙钟时刻（ms）。
    pub pinned_at_ms: u64,
    /// 从 token 解析出的服务端铸造时刻（unix 秒；解析失败为 None）。
    pub minted_at_s: Option<u64>,
    /// 是否已判失效（失效后停止注入，等一次干净成功恢复）。
    pub failed: bool,
    /// 是否被 canary 判定为静默降智（换号信号；一次干净成功恢复）。
    #[serde(default)]
    pub degraded: bool,
    pub ok_count: u64,
    pub ov_count: u64,
    #[serde(default)]
    pub deg_count: u64,
    pub err_count: u64,
    /// 连续 overload 计数（成功清零）。
    pub fail_streak: u32,
    /// 最近一次结果时刻（ms）。
    pub last_outcome_ms: u64,
    /// 最近一次 TPS（tokens/s，来自采样；0 表示未知）。
    pub last_tps: f64,
    /// 最近一次从上游看到的 turn-state 字节长度（无论是否锁定；0 = 尚未见到）。
    /// 用于一眼区分真6(292)/假6(312)：即使因票长闸门没锁，也能看出上游此刻发的是哪种票。
    #[serde(default)]
    pub last_seen_ticket_len: u32,
    /// 铸出当前锁票的出口池槽位（0 起）。None 表示来自账号原代理、手工录入，
    /// 或由不记录出口槽位的旧版本创建。
    #[serde(default)]
    pub locked_egress_idx: Option<usize>,
    /// 近窗口内的 (时刻ms, 是否overload) 采样，用于占比判失。
    #[serde(default)]
    pub recent: VecDeque<(u64, bool)>,
    /// 连续经历的「出口池转满一圈仍无 292 → 休息」轮数；锁到 292 清零。
    #[serde(default)]
    pub stuck_rounds: u32,
    /// 放弃态截止时刻（ms）；0 = 未放弃。到点后允许再试一圈，仍转满即立即重新放弃。
    #[serde(default)]
    pub abandoned_until_ms: u64,
    /// 是否曾经放弃过（到期重试时只给一圈机会的依据）。
    #[serde(default)]
    pub ever_abandoned: bool,
    /// 格级休息截止时刻（ms）；0 = 未休息。休息中该格不探铸、真实流量直通，不碰账号优先级。
    /// 用于「同一账号别的模型手里有 292」时，只让卡住的这一格歇着。
    #[serde(default)]
    pub rest_until_ms: u64,
}

impl Cell {
    fn new(account_id: i64, model: String) -> Self {
        Self {
            account_id,
            model,
            turn_state: None,
            pinned_at_ms: 0,
            minted_at_s: None,
            failed: false,
            degraded: false,
            ok_count: 0,
            ov_count: 0,
            deg_count: 0,
            err_count: 0,
            fail_streak: 0,
            last_outcome_ms: 0,
            last_tps: 0.0,
            last_seen_ticket_len: 0,
            locked_egress_idx: None,
            recent: VecDeque::new(),
            stuck_rounds: 0,
            abandoned_until_ms: 0,
            ever_abandoned: false,
            rest_until_ms: 0,
        }
    }

    /// 锁到 292 真票：清放弃/卡住计数（真票是最强的恢复信号）。
    fn clear_stuck(&mut self) {
        self.stuck_rounds = 0;
        self.abandoned_until_ms = 0;
        self.ever_abandoned = false;
        self.rest_until_ms = 0;
    }

    /// 当前是否处于放弃态（未到期）。
    pub fn is_abandoned(&self, now_ms: u64) -> bool {
        self.abandoned_until_ms > now_ms
    }

    /// 当前是否处于格级休息（未到期）。
    pub fn is_resting(&self, now_ms: u64) -> bool {
        self.rest_until_ms > now_ms
    }

    /// 是否「停铸」：放弃态或格级休息中。停铸的格不探、真实流量直通不注入。
    pub fn is_parked(&self, now_ms: u64) -> bool {
        self.is_abandoned(now_ms) || self.is_resting(now_ms)
    }

    /// 当前是否持有可注入的有效 turn-state（未失效、非空、未过期）。
    fn injectable(&self, now_ms: u64, params: &PinParams) -> Option<String> {
        if self.failed {
            return None;
        }
        let ts = self.turn_state.as_ref()?;
        if self.expired(now_ms, params) {
            return None;
        }
        Some(ts.clone())
    }

    fn expired(&self, now_ms: u64, params: &PinParams) -> bool {
        self.turn_state.is_some() && now_ms.saturating_sub(self.pinned_at_ms) > params.max_age_ms
    }

    fn clear_turn_state(&mut self) {
        self.turn_state = None;
        self.pinned_at_ms = 0;
        self.minted_at_s = None;
        self.locked_egress_idx = None;
    }

    fn overload_ratio_pct(&self, now_ms: u64, window_ms: u64) -> (usize, u32) {
        let cutoff = now_ms.saturating_sub(window_ms);
        let mut total = 0usize;
        let mut ov = 0usize;
        for (ts, is_ov) in &self.recent {
            if *ts >= cutoff {
                total += 1;
                if *is_ov {
                    ov += 1;
                }
            }
        }
        if total == 0 {
            (0, 0)
        } else {
            (total, ((ov * 100) / total) as u32)
        }
    }
}

/// (account × model) turn-state 池。
pub struct TurnStatePool {
    cells: Mutex<HashMap<(i64, String), Cell>>,
    /// 上次落盘时刻（ms），用于限流。
    last_persist_ms: std::sync::atomic::AtomicU64,
    /// 是否已从磁盘载入过（只载一次）。
    loaded: std::sync::atomic::AtomicBool,
    /// 智商跃迁事件发送端（可选；main 里接线到 TG 通知任务）。
    notifier: Mutex<Option<tokio::sync::mpsc::UnboundedSender<TierEvent>>>,
}

impl Default for TurnStatePool {
    fn default() -> Self {
        Self {
            cells: Mutex::new(HashMap::new()),
            last_persist_ms: std::sync::atomic::AtomicU64::new(0),
            loaded: std::sync::atomic::AtomicBool::new(false),
            notifier: Mutex::new(None),
        }
    }
}

impl TurnStatePool {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(i64, String), Cell>> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 接线智商跃迁通知发送端（main 启动时调用一次）。
    pub fn set_notifier(&self, tx: tokio::sync::mpsc::UnboundedSender<TierEvent>) {
        *self
            .notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    }

    /// 档位变了就投一个事件（best-effort；无订阅者/发送失败静默）。
    fn emit_tier(&self, account_id: i64, model: &str, from: Tier, to: Tier) {
        if from == to {
            return;
        }
        if let Some(tx) = self
            .notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(TierEvent {
                account_id,
                model: model.to_string(),
                from,
                to,
                ts_ms: now_ms() as u64,
            });
        }
    }

    /// 取某格快照（通知任务发送时读近况用）。
    pub fn get_cell(&self, account_id: i64, model: &str) -> Option<Cell> {
        self.lock().get(&(account_id, model.to_string())).cloned()
    }

    /// 出站前查询：该 (account, model) 是否有可注入的 turn-state。
    pub fn injectable(&self, account_id: i64, model: &str, params: &PinParams) -> Option<String> {
        self.injectable_with_egress(account_id, model, params)
            .map(|(turn_state, _)| turn_state)
    }

    /// 返回当前可注入锁票及铸票出口池槽位。旧票或账号原代理铸出的票槽位为 None。
    pub fn injectable_with_egress(
        &self,
        account_id: i64,
        model: &str,
        params: &PinParams,
    ) -> Option<(String, Option<usize>)> {
        let now = now_ms() as u64;
        let cells = self.lock();
        cells
            .get(&(account_id, model.to_string()))
            .and_then(|cell| {
                cell.injectable(now, params)
                    .map(|turn_state| (turn_state, cell.locked_egress_idx))
            })
    }

    /// 收到上游响应头后：若该格当前没有钉住的 turn-state（空/过期），把响应里的 turn-state 捕获钉住。
    pub fn capture_if_empty(
        &self,
        account_id: i64,
        model: &str,
        resp_turn_state: Option<&str>,
        params: &PinParams,
    ) {
        self.capture_if_empty_with_egress(account_id, model, resp_turn_state, None, params);
    }

    /// 与 capture_if_empty 相同，并记录铸出新锁票的代理池槽位。
    pub fn capture_if_empty_with_egress(
        &self,
        account_id: i64,
        model: &str,
        resp_turn_state: Option<&str>,
        locked_egress_idx: Option<usize>,
        params: &PinParams,
    ) {
        let Some(ts) = resp_turn_state else {
            return;
        };
        if ts.is_empty() {
            return;
        }
        let now = now_ms() as u64;
        let (before, after) = {
            let mut cells = self.lock();
            let cell = cells
                .entry((account_id, model.to_string()))
                .or_insert_with(|| Cell::new(account_id, model.to_string()));
            let before = cell_tier(cell);
            // 无论是否锁定，先记录“上次看到的票长”，供面板区分真6(292)/假6(312)。
            cell.last_seen_ticket_len = ts.len() as u32;
            // 真6(292)票是可信恢复信号：无视 failed/degraded 闸门——只要铸到真6就先治愈该格
            // （清失效/降智/连败标记），再锁定。这样一张长期被降智打成 failed 的账号，一旦偶尔
            // 撞上干净出口铸出 292，就能立刻把真票钉住养进池子，而不是白白丢掉。
            if is_lockable_ticket_len(ts.len()) {
                if cell.failed || cell.degraded {
                    cell.failed = false;
                    cell.degraded = false;
                    cell.fail_streak = 0;
                }
                let need = cell.turn_state.is_none() || cell.expired(now, params);
                if need {
                    cell.turn_state = Some(ts.to_string());
                    cell.pinned_at_ms = now;
                    cell.minted_at_s = parse_fernet_timestamp(ts);
                    cell.locked_egress_idx = locked_egress_idx;
                }
                cell.clear_stuck();
            }
            // 非真6票（如 312 的“酱汁”票）：失效格不动；健康格也只记 last_seen 不锁——
            // 保持空/过期，等下一次请求继续试，直到再出现 292 才锁。
            (before, cell_tier(cell))
        };
        self.emit_tier(account_id, model, before, after);
    }

    /// 注入 pin 票后收到响应：若服务端在响应里**回带了新的 turn-state**（非空），说明我们
    /// 注入的那张已被判死/换发——即用户说的“票费了”。这是比硬 TTL 精确得多的过期信号：
    /// 正常复用有效票时上游不回 turn-state（实测 8200/8200 全空），只有票费了才换发。
    ///
    /// 处理：丢弃旧钉票（它已死）；若换发来的新票恰是真6(292)，就地重锁，省掉一次重铸往返；
    /// 否则清空待下一次探铸/被动重养。最坏情况退化为“丢票重铸”，与旧 TTL 到期行为一致，
    /// 无回归风险。返回 true 表示确实检测到换发（票费了）。
    pub fn note_injected_reissue(
        &self,
        account_id: i64,
        model: &str,
        resp_turn_state: Option<&str>,
        params: &PinParams,
    ) -> bool {
        self.note_injected_reissue_with_egress(account_id, model, resp_turn_state, None, params)
    }

    /// 与 note_injected_reissue 相同，并记录换发票实际使用的代理池槽位。
    pub fn note_injected_reissue_with_egress(
        &self,
        account_id: i64,
        model: &str,
        resp_turn_state: Option<&str>,
        locked_egress_idx: Option<usize>,
        _params: &PinParams,
    ) -> bool {
        let Some(ts) = resp_turn_state else {
            return false;
        };
        if ts.is_empty() {
            return false;
        }
        let now = now_ms() as u64;
        let (before, after) = {
            let mut cells = self.lock();
            let cell = cells
                .entry((account_id, model.to_string()))
                .or_insert_with(|| Cell::new(account_id, model.to_string()));
            let before = cell_tier(cell);
            cell.last_seen_ticket_len = ts.len() as u32;
            // 旧钉票被换发 = 费了，先无条件丢弃。
            cell.clear_turn_state();
            // 新票若是真6(292)：就地治愈并重锁，无缝续命，不必等下一次重铸。
            if is_lockable_ticket_len(ts.len()) {
                cell.failed = false;
                cell.degraded = false;
                cell.fail_streak = 0;
                cell.turn_state = Some(ts.to_string());
                cell.pinned_at_ms = now;
                cell.minted_at_s = parse_fernet_timestamp(ts);
                cell.locked_egress_idx = locked_egress_idx;
                cell.clear_stuck();
            }
            (before, cell_tier(cell))
        };
        self.emit_tier(account_id, model, before, after);
        true
    }

    /// 该格是否处于放弃态（真实流量直通、养池不探铸）。未知格返回 false。
    pub fn is_abandoned(&self, account_id: i64, model: &str, now_ms: u64) -> bool {
        self.lock()
            .get(&(account_id, model.to_string()))
            .map(|c| c.is_abandoned(now_ms))
            .unwrap_or(false)
    }

    /// 该格是否停铸（放弃态或格级休息）。未知格返回 false。
    pub fn is_parked(&self, account_id: i64, model: &str, now_ms: u64) -> bool {
        self.lock()
            .get(&(account_id, model.to_string()))
            .map(|c| c.is_parked(now_ms))
            .unwrap_or(false)
    }

    /// 格级休息剩余秒数（供面板）；未休息返回 None。
    pub fn rest_remaining_s(&self, account_id: i64, model: &str, now_ms: u64) -> Option<u64> {
        self.lock()
            .get(&(account_id, model.to_string()))
            .filter(|c| c.is_resting(now_ms))
            .map(|c| c.rest_until_ms.saturating_sub(now_ms) / 1000)
    }

    /// 面板手动解除停铸：清放弃态 / 格级休息 / 卡住轮数，让该格立刻重新参与铸票。
    /// 返回 true 表示该格存在且确实处于停铸或有卡住计数。
    pub fn clear_parked(&self, account_id: i64, model: &str) -> bool {
        let (before, after, changed) = {
            let mut cells = self.lock();
            let Some(cell) = cells.get_mut(&(account_id, model.to_string())) else {
                return false;
            };
            let before = cell_tier(cell);
            let changed = cell.abandoned_until_ms > 0
                || cell.rest_until_ms > 0
                || cell.stuck_rounds > 0
                || cell.ever_abandoned;
            cell.clear_stuck();
            (before, cell_tier(cell), changed)
        };
        self.emit_tier(account_id, model, before, after);
        changed
    }

    /// 解除所有格的停铸。返回被清理的格数。
    pub fn clear_parked_all(&self) -> usize {
        let keys: Vec<(i64, String)> = self.lock().keys().cloned().collect();
        keys.into_iter()
            .filter(|(a, m)| self.clear_parked(*a, m))
            .count()
    }

    /// 让某格进入格级休息 rest_ms（不碰账号优先级）。
    pub fn park(&self, account_id: i64, model: &str, rest_ms: u64) {
        let now = now_ms() as u64;
        let mut cells = self.lock();
        let cell = cells
            .entry((account_id, model.to_string()))
            .or_insert_with(|| Cell::new(account_id, model.to_string()));
        cell.rest_until_ms = now + rest_ms;
    }

    /// 该格是否需要铸票：没有有效票、且不在停铸态。养池循环用它决定探不探。
    pub fn needs_mint(&self, account_id: i64, model: &str, params: &PinParams) -> bool {
        let now = now_ms() as u64;
        let cells = self.lock();
        match cells.get(&(account_id, model.to_string())) {
            Some(cell) => cell.injectable(now, params).is_none() && !cell.is_parked(now),
            None => true,
        }
    }

    /// 休息编排登记一轮「出口池转满仍无 292」：stuck_rounds +1。达到 giveup_rounds
    /// （0 = 永不放弃）、或该格是放弃到期后的重试圈，即进入放弃态 retry_ms。
    /// 返回 true 表示本次进入了放弃态。
    pub fn note_stuck_round(
        &self,
        account_id: i64,
        model: &str,
        giveup_rounds: u32,
        retry_ms: u64,
    ) -> bool {
        let now = now_ms() as u64;
        let (before, after, abandoned) = {
            let mut cells = self.lock();
            let cell = cells
                .entry((account_id, model.to_string()))
                .or_insert_with(|| Cell::new(account_id, model.to_string()));
            let before = cell_tier(cell);
            cell.stuck_rounds = cell.stuck_rounds.saturating_add(1);
            let give_up =
                giveup_rounds > 0 && (cell.stuck_rounds >= giveup_rounds || cell.ever_abandoned);
            if give_up {
                cell.abandoned_until_ms = now + retry_ms;
                cell.ever_abandoned = true;
            }
            (before, cell_tier(cell), give_up)
        };
        self.emit_tier(account_id, model, before, after);
        abandoned
    }

    /// 请求结束后：登记结果，驱动刷新/判失/恢复状态机。
    pub fn record_outcome(
        &self,
        account_id: i64,
        model: &str,
        outcome: Outcome,
        tps: f64,
        params: &PinParams,
    ) {
        let now = now_ms() as u64;
        let (before, after) = {
            let mut cells = self.lock();
            let cell = cells
                .entry((account_id, model.to_string()))
                .or_insert_with(|| Cell::new(account_id, model.to_string()));
            let before = cell_tier(cell);
            cell.last_outcome_ms = now;
            if tps > 0.0 {
                cell.last_tps = tps;
            }
            cell.recent.push_back((now, outcome == Outcome::Overload));
            while cell.recent.len() > 256 {
                cell.recent.pop_front();
            }
            match outcome {
                Outcome::Success => {
                    cell.ok_count += 1;
                    cell.fail_streak = 0;
                    if cell.failed || cell.degraded {
                        // 恢复：清失效/降智标记，丢弃旧 turn-state，等下一次捕获新的。
                        cell.failed = false;
                        cell.degraded = false;
                        cell.clear_turn_state();
                    }
                }
                Outcome::Overload => {
                    cell.ov_count += 1;
                    cell.fail_streak += 1;
                    // overload 即刷新：清掉当前钉住的 turn-state，下次重新捕获。
                    cell.clear_turn_state();
                    let (samples, ratio) = cell.overload_ratio_pct(now, params.window_ms);
                    if cell.fail_streak >= params.fail_threshold
                        || (samples >= params.min_samples && ratio >= params.fail_ratio_pct)
                    {
                        cell.failed = true;
                    }
                }
                Outcome::Degraded => {
                    cell.deg_count += 1;
                    // 已锁定、未过期的 292 真6票是"黏"的：TTL 内单个 312 响应不清票、不打降智，
                    // 继续注入到 TTL 过期再重养。养池的意义就在于抓到那张稀有真6票后把它用满。
                    let holds_locked_real6 = cell
                        .turn_state
                        .as_deref()
                        .is_some_and(|ts| is_lockable_ticket_len(ts.len()))
                        && !cell.expired(now, params);
                    if holds_locked_real6 {
                        // 保留锁票，仅计降智数（供面板观察服务端真实命中率）。
                    } else {
                        // 没有有效锁票（或已过期）：判失效 + 打降智标记（换号/重养信号），清 turn-state。
                        cell.fail_streak += 1;
                        cell.degraded = true;
                        cell.failed = true;
                        cell.clear_turn_state();
                    }
                }
                Outcome::OtherError => {
                    cell.err_count += 1;
                }
            }
            (before, cell_tier(cell))
        };
        self.emit_tier(account_id, model, before, after);
    }

    /// 面板"保存"：手动设置某格的 turn-state。空串/None = 清空(空=不注入)。
    /// 手动设置视为可信来源：重算铸造时刻、清除失效/降智标记并重新锁定。
    pub fn set_turn_state(&self, account_id: i64, model: &str, turn_state: Option<&str>) {
        let now = now_ms() as u64;
        let mut cells = self.lock();
        let cell = cells
            .entry((account_id, model.to_string()))
            .or_insert_with(|| Cell::new(account_id, model.to_string()));
        match turn_state {
            Some(ts) if !ts.trim().is_empty() => {
                let ts = ts.trim().to_string();
                cell.minted_at_s = parse_fernet_timestamp(&ts);
                cell.turn_state = Some(ts);
                cell.pinned_at_ms = now;
                cell.locked_egress_idx = None;
                cell.failed = false;
                cell.degraded = false;
                cell.fail_streak = 0;
                cell.clear_stuck();
            }
            _ => {
                cell.clear_turn_state();
            }
        }
    }

    /// 面板"切片"：丢掉本号该模型的旧分片，重置失效/降智，等下一次请求重新捕获铸造。
    pub fn cut(&self, account_id: i64, model: &str) {
        let mut cells = self.lock();
        if let Some(cell) = cells.get_mut(&(account_id, model.to_string())) {
            cell.clear_turn_state();
            cell.failed = false;
            cell.degraded = false;
            cell.fail_streak = 0;
            cell.clear_stuck();
        }
    }

    /// 面板/持久化用：导出所有格的快照。
    pub fn export(&self) -> Vec<Cell> {
        self.lock().values().cloned().collect()
    }

    /// 删除宿主已不存在账号的全部模型格。返回删除的格数。
    pub fn retain_accounts(&self, existing: &std::collections::HashSet<i64>) -> usize {
        let mut cells = self.lock();
        let before = cells.len();
        cells.retain(|(account_id, _), _| existing.contains(account_id));
        before.saturating_sub(cells.len())
    }

    /// 从持久化恢复：仅导入未过期的格，且丢弃已失效格的 turn-state。
    pub fn import(&self, cells: Vec<Cell>, params: &PinParams) {
        let now = now_ms() as u64;
        let mut guard = self.lock();
        for mut cell in cells {
            if cell.expired(now, params) {
                cell.clear_turn_state();
            }
            guard.insert((cell.account_id, cell.model.clone()), cell);
        }
    }

    /// 启动载入（只执行一次；path 空则跳过）。best-effort，失败静默。
    pub fn load_once(&self, path: &str, params: &PinParams) {
        use std::sync::atomic::Ordering;
        if path.trim().is_empty() {
            return;
        }
        if self.loaded.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(bytes) = std::fs::read(path.trim()) {
            if let Ok(cells) = serde_json::from_slice::<Vec<Cell>>(&bytes) {
                self.import(cells, params);
            }
        }
    }

    /// 限流落盘（默认每 30s 最多一次；path 空则跳过）。best-effort，失败静默。
    pub fn persist_throttled(&self, path: &str) {
        use std::sync::atomic::Ordering;
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let now = now_ms() as u64;
        let last = self.last_persist_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 30_000 {
            return;
        }
        // 抢占落盘窗口，避免多请求并发重复写。
        if self
            .last_persist_ms
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.persist_now(path);
    }

    /// 立即落盘（不受限流约束；面板手动保存/切片后调用）。best-effort，失败静默。
    pub fn persist_now(&self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let snapshot = self.export();
        if let Ok(bytes) = serde_json::to_vec(&snapshot) {
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // 先写临时文件再重命名，避免半截文件。
            let tmp = format!("{path}.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

/// 解析 Fernet token 里内嵌的时间戳（version 0x80 后 8 字节 big-endian unix 秒）。
/// 只解前 12 个 base64url 字符 → 9 字节即可，失败返回 None。
fn parse_fernet_timestamp(token: &str) -> Option<u64> {
    let head: String = token.chars().take(12).collect();
    let bytes = base64url_decode_prefix(&head)?;
    if bytes.len() < 9 || bytes[0] != 0x80 {
        return None;
    }
    let mut ts: u64 = 0;
    for b in &bytes[1..9] {
        ts = (ts << 8) | (*b as u64);
    }
    Some(ts)
}

/// 解码 base64url 前缀（长度需为 4 的倍数，无填充），只用于取 token 头 9 字节。
fn base64url_decode_prefix(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let n = bytes.len() - (bytes.len() % 4);
    let mut out = Vec::with_capacity(n / 4 * 3);
    let mut i = 0;
    while i < n {
        let a = val(bytes[i])?;
        let b = val(bytes[i + 1])?;
        let c = val(bytes[i + 2])?;
        let d = val(bytes[i + 3])?;
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        out.push((c << 6) | d);
        i += 4;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> PinParams {
        PinParams::from_config(900, 3, 50)
    }

    /// 生成一张 292 字节的“真6”票（前缀 gAAAAAB + 标签 + 填充），供捕获测试用。
    fn t(tag: &str) -> String {
        let mut s = format!("gAAAAAB{tag}");
        while s.len() < REAL6_TICKET_LEN {
            s.push('A');
        }
        s.truncate(REAL6_TICKET_LEN);
        s
    }

    #[test]
    fn pool_captures_injects_and_refreshes_on_overload() {
        let pool = TurnStatePool::new();
        let p = params();
        // 空池：无可注入。
        assert!(pool.injectable(20, "gpt-6-astra", &p).is_none());
        // 捕获一个 292 字节的“真6”票。
        let token = t("token");
        let other = t("other");
        pool.capture_if_empty(20, "gpt-6-astra", Some(&token), &p);
        assert_eq!(
            pool.injectable(20, "gpt-6-astra", &p).as_deref(),
            Some(token.as_str())
        );
        // 已有钉住值时不被后续响应覆盖（保持粘连）。
        pool.capture_if_empty(20, "gpt-6-astra", Some(&other), &p);
        assert_eq!(
            pool.injectable(20, "gpt-6-astra", &p).as_deref(),
            Some(token.as_str())
        );
        // overload：刷新（清空），下次需重新捕获。
        pool.record_outcome(20, "gpt-6-astra", Outcome::Overload, 0.0, &p);
        assert!(pool.injectable(20, "gpt-6-astra", &p).is_none());
    }

    #[test]
    fn pool_fails_after_consecutive_overloads_and_recovers() {
        let pool = TurnStatePool::new();
        let p = params();
        let (x, z) = (t("x"), t("z"));
        let sauce = "s".repeat(312); // 312 酱汁票：失效格不锁
        pool.capture_if_empty(20, "m", Some(&x), &p);
        for _ in 0..3 {
            pool.record_outcome(20, "m", Outcome::Overload, 0.0, &p);
        }
        // 连续 3 次 overload → 失效：捕获非真6(312)票也不注入。
        pool.capture_if_empty(20, "m", Some(&sauce), &p);
        assert!(pool.injectable(20, "m", &p).is_none());
        // 一次成功 → 恢复。
        pool.record_outcome(20, "m", Outcome::Success, 0.0, &p);
        pool.capture_if_empty(20, "m", Some(&z), &p);
        assert_eq!(pool.injectable(20, "m", &p).as_deref(), Some(z.as_str()));
    }

    #[test]
    fn capture_only_locks_real6_292_ticket() {
        // 写死只锁 292 的“真6”票；312 的“酱汁”票不锁，但记录 last_seen。
        let pool = TurnStatePool::new();
        let p = params();
        let good = "g".repeat(REAL6_TICKET_LEN); // 292
        let sauce = "g".repeat(312);
        // 先来一张 312：不锁，但 last_seen=312。
        pool.capture_if_empty(20, "m", Some(&sauce), &p);
        assert!(pool.injectable(20, "m", &p).is_none());
        {
            let cells = pool.lock();
            assert_eq!(
                cells
                    .get(&(20, "m".to_string()))
                    .unwrap()
                    .last_seen_ticket_len,
                312
            );
        }
        // 再来一张 292：锁定，last_seen=292。
        pool.capture_if_empty(20, "m", Some(&good), &p);
        assert_eq!(pool.injectable(20, "m", &p).as_deref(), Some(good.as_str()));
        {
            let cells = pool.lock();
            assert_eq!(
                cells
                    .get(&(20, "m".to_string()))
                    .unwrap()
                    .last_seen_ticket_len,
                292
            );
        }
    }

    #[test]
    fn capture_and_reissue_also_lock_332_ticket() {
        let pool = TurnStatePool::new();
        let p = params();
        let first = "v".repeat(REAL6_TICKET_LEN_V2);
        let replacement = "w".repeat(REAL6_TICKET_LEN_V2);

        assert!(is_lockable_ticket_len(REAL6_TICKET_LEN));
        assert!(is_lockable_ticket_len(REAL6_TICKET_LEN_V2));
        assert!(!is_lockable_ticket_len(312));

        pool.capture_if_empty(21, "m", Some(&first), &p);
        assert_eq!(
            pool.injectable(21, "m", &p).as_deref(),
            Some(first.as_str())
        );
        assert!(pool.note_injected_reissue(21, "m", Some(&replacement), &p));
        assert_eq!(
            pool.injectable(21, "m", &p).as_deref(),
            Some(replacement.as_str())
        );
        let cell = pool.get_cell(21, "m").unwrap();
        assert_eq!(cell.last_seen_ticket_len, REAL6_TICKET_LEN_V2 as u32);
        assert!(!cell.failed && !cell.degraded);
    }

    #[test]
    fn locked_ticket_keeps_its_minting_egress_until_cleared() {
        let pool = TurnStatePool::new();
        let p = params();
        let first = "v".repeat(REAL6_TICKET_LEN);
        let replacement = "w".repeat(REAL6_TICKET_LEN_V2);

        pool.capture_if_empty_with_egress(22, "m", Some(&first), Some(7), &p);
        assert_eq!(
            pool.injectable_with_egress(22, "m", &p),
            Some((first, Some(7)))
        );

        assert!(pool.note_injected_reissue_with_egress(22, "m", Some(&replacement), Some(3), &p));
        assert_eq!(
            pool.injectable_with_egress(22, "m", &p),
            Some((replacement, Some(3)))
        );

        pool.record_outcome(22, "m", Outcome::Overload, 0.0, &p);
        let cell = pool.get_cell(22, "m").unwrap();
        assert!(cell.turn_state.is_none());
        assert_eq!(cell.pinned_at_ms, 0);
        assert_eq!(cell.minted_at_s, None);
        assert_eq!(cell.locked_egress_idx, None);
        assert_eq!(cell.last_seen_ticket_len, REAL6_TICKET_LEN_V2 as u32);
    }

    #[test]
    fn real6_292_heals_and_locks_even_when_failed() {
        // 长期被降智/过载打成 failed 的格子：312 依旧不锁；
        // 一旦撞上 292 真6票，应当立刻治愈（清 failed/degraded）并锁定养进池子。
        let pool = TurnStatePool::new();
        let p = params();
        let good = "g".repeat(REAL6_TICKET_LEN); // 292
        let sauce = "g".repeat(312);
        // 打成 failed（降智一次即 failed）。
        pool.capture_if_empty(20, "m", Some(&sauce), &p);
        pool.record_outcome(20, "m", Outcome::Degraded, 0.0, &p);
        {
            let cells = pool.lock();
            let cell = cells.get(&(20, "m".to_string())).unwrap();
            assert!(cell.failed && cell.degraded);
        }
        // failed 状态下来一张 312：仍不锁。
        pool.capture_if_empty(20, "m", Some(&sauce), &p);
        assert!(pool.injectable(20, "m", &p).is_none());
        // failed 状态下来一张 292：治愈 + 锁定 + 可注入。
        pool.capture_if_empty(20, "m", Some(&good), &p);
        assert_eq!(pool.injectable(20, "m", &p).as_deref(), Some(good.as_str()));
        {
            let cells = pool.lock();
            let cell = cells.get(&(20, "m".to_string())).unwrap();
            assert!(!cell.failed && !cell.degraded && cell.fail_streak == 0);
            assert_eq!(cell.last_seen_ticket_len, 292);
        }
    }

    #[test]
    fn locked_real6_survives_degraded_within_ttl() {
        // 锁到 292 真6票后：TTL 内即便连续收到 312 降智响应，也不能清票、不能打降智，
        // 必须继续注入到 TTL 过期再重养。
        let pool = TurnStatePool::new();
        let p = params();
        let good = "g".repeat(REAL6_TICKET_LEN); // 292
        pool.capture_if_empty(20, "m", Some(&good), &p);
        assert_eq!(pool.injectable(20, "m", &p).as_deref(), Some(good.as_str()));
        // 连续 6 次 312 降智：锁票仍在、仍可注入、格子未被打成 failed/degraded。
        for _ in 0..6 {
            pool.record_outcome(20, "m", Outcome::Degraded, 0.0, &p);
        }
        assert_eq!(pool.injectable(20, "m", &p).as_deref(), Some(good.as_str()));
        {
            let cells = pool.lock();
            let cell = cells.get(&(20, "m".to_string())).unwrap();
            assert!(!cell.failed && !cell.degraded);
            assert!(cell.deg_count >= 6, "降智次数应照常计数供面板观察");
        }
    }

    #[test]
    fn reissue_signal_drops_spent_ticket_and_relocks_on_292() {
        // 注入的真6票被服务端换发（响应回带新 turn-state）= 票费了。
        let pool = TurnStatePool::new();
        let p = params();
        let old = t("old");
        pool.capture_if_empty(20, "m", Some(&old), &p);
        assert_eq!(pool.injectable(20, "m", &p).as_deref(), Some(old.as_str()));

        // 换发来的新票是 292：丢旧、就地重锁新票，无缝续命。
        let fresh = t("fresh");
        assert!(pool.note_injected_reissue(20, "m", Some(&fresh), &p));
        assert_eq!(
            pool.injectable(20, "m", &p).as_deref(),
            Some(fresh.as_str())
        );

        // 换发来的是 312：丢票、清空待重养（不锁）。
        let sauce = "s".repeat(312);
        assert!(pool.note_injected_reissue(20, "m", Some(&sauce), &p));
        assert!(pool.injectable(20, "m", &p).is_none());
        {
            let cells = pool.lock();
            let cell = cells.get(&(20, "m".to_string())).unwrap();
            assert_eq!(cell.last_seen_ticket_len, 312);
            assert!(cell.turn_state.is_none());
        }
    }

    #[test]
    fn reissue_signal_noop_when_response_has_no_turn_state() {
        // 正常复用有效票：上游不回 turn-state（None/空）→ 不动锁票。
        let pool = TurnStatePool::new();
        let p = params();
        let good = t("good");
        pool.capture_if_empty(20, "m", Some(&good), &p);
        assert!(!pool.note_injected_reissue(20, "m", None, &p));
        assert!(!pool.note_injected_reissue(20, "m", Some(""), &p));
        assert_eq!(pool.injectable(20, "m", &p).as_deref(), Some(good.as_str()));
    }

    #[test]
    fn expired_real6_lock_cleared_by_degraded() {
        // TTL 过期后再遇 312：锁票不再"黏"，应清票 + 打失效，驱动重养。
        let pool = TurnStatePool::new();
        let p = PinParams::from_config(0, 3, 50); // max_age=0
        let good = "g".repeat(REAL6_TICKET_LEN);
        pool.capture_if_empty(20, "m", Some(&good), &p);
        std::thread::sleep(std::time::Duration::from_millis(5));
        pool.record_outcome(20, "m", Outcome::Degraded, 0.0, &p);
        assert!(pool.injectable(20, "m", &p).is_none());
        {
            let cells = pool.lock();
            let cell = cells.get(&(20, "m".to_string())).unwrap();
            assert!(cell.failed && cell.degraded);
            assert!(cell.turn_state.is_none());
        }
    }

    #[test]
    fn capture_rejects_non_292_ticket() {
        // 短票 / 312 票一律不锁（写死 292）。
        let pool = TurnStatePool::new();
        let p = params();
        pool.capture_if_empty(20, "m", Some("gAAAAABshort"), &p);
        assert!(pool.injectable(20, "m", &p).is_none());
        pool.capture_if_empty(20, "m", Some(&"g".repeat(312)), &p);
        assert!(pool.injectable(20, "m", &p).is_none());
    }

    #[test]
    fn degrade_detection_reads_reasoning_signal() {
        // 正常号：有 reasoning 输出项 + reasoning_tokens>0。
        let normal = r#"...{"type":"reasoning","encrypted_content":"gAAA"}...
"output_tokens_details":{"reasoning_tokens":89}..."#;
        assert!(!is_degraded_from_sse(normal));
        // 降智号：无 reasoning 项，reasoning_tokens 0。
        let degraded =
            r#"...{"type":"message"}..."output_tokens_details":{"reasoning_tokens":0}..."#;
        assert!(is_degraded_from_sse(degraded));
        // 无任何 reasoning 迹象也判降智。
        assert!(is_degraded_from_sse("{\"type\":\"message\"}"));
    }

    #[test]
    fn tier_events_emitted_only_on_transitions() {
        let pool = TurnStatePool::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        pool.set_notifier(tx);
        let p = params();
        // 掉智：Ok → Degraded 投一条。
        pool.record_outcome(1, "m", Outcome::Degraded, 0.0, &p);
        let ev = rx.try_recv().expect("degrade event");
        assert_eq!((ev.from, ev.to), (Tier::Ok, Tier::Degraded));
        assert_eq!((ev.account_id, ev.model.as_str()), (1, "m"));
        // 恢复：Degraded → Ok 投一条。
        pool.record_outcome(1, "m", Outcome::Success, 0.0, &p);
        let ev = rx.try_recv().expect("recover event");
        assert_eq!((ev.from, ev.to), (Tier::Degraded, Tier::Ok));
        // 档位不变（连续成功）不投。
        pool.record_outcome(1, "m", Outcome::Success, 0.0, &p);
        assert!(rx.try_recv().is_err());
        // 捕获 292 真6票治愈失效格也会投恢复事件。
        pool.record_outcome(2, "m", Outcome::Degraded, 0.0, &p);
        let _ = rx.try_recv().expect("degrade event acct2");
        let real6 = "g".repeat(REAL6_TICKET_LEN);
        pool.capture_if_empty(2, "m", Some(&real6), &p);
        let ev = rx.try_recv().expect("heal event acct2");
        assert_eq!((ev.from, ev.to), (Tier::Degraded, Tier::Ok));
    }

    #[test]
    fn retain_accounts_removes_all_models_for_deleted_account() {
        let pool = TurnStatePool::new();
        pool.set_turn_state(10, "gpt-a", Some(&"a".repeat(REAL6_TICKET_LEN)));
        pool.set_turn_state(10, "gpt-b", Some(&"b".repeat(REAL6_TICKET_LEN)));
        pool.set_turn_state(11, "gpt-a", Some(&"c".repeat(REAL6_TICKET_LEN)));

        let existing = std::collections::HashSet::from([11]);
        assert_eq!(pool.retain_accounts(&existing), 2);
        assert!(pool.get_cell(10, "gpt-a").is_none());
        assert!(pool.get_cell(10, "gpt-b").is_none());
        assert!(pool.get_cell(11, "gpt-a").is_some());
    }

    #[test]
    fn removed_account_does_not_return_after_persist_and_reload() {
        let path = std::env::temp_dir()
            .join(format!("cnt-prune-test-{}.json", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&path);
        let params = params();
        let pool = TurnStatePool::new();
        pool.set_turn_state(10, "gpt-a", Some(&"a".repeat(REAL6_TICKET_LEN)));
        pool.set_turn_state(11, "gpt-a", Some(&"b".repeat(REAL6_TICKET_LEN)));
        pool.retain_accounts(&std::collections::HashSet::from([11]));
        pool.persist_now(&path);

        let restored = TurnStatePool::new();
        restored.load_once(&path, &params);
        assert!(restored.get_cell(10, "gpt-a").is_none());
        assert!(restored.get_cell(11, "gpt-a").is_some());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn degraded_outcome_fails_cell_until_success() {
        let pool = TurnStatePool::new();
        let p = params();
        pool.capture_if_empty(9, "m", Some("gAAAAABd"), &p);
        pool.record_outcome(9, "m", Outcome::Degraded, 0.0, &p);
        assert!(pool.injectable(9, "m", &p).is_none());
        {
            let cells = pool.lock();
            let cell = cells.get(&(9, "m".to_string())).unwrap();
            assert!(cell.degraded && cell.failed && cell.deg_count == 1);
        }
        // 一次干净成功恢复。
        pool.record_outcome(9, "m", Outcome::Success, 0.0, &p);
        {
            let cells = pool.lock();
            let cell = cells.get(&(9, "m".to_string())).unwrap();
            assert!(!cell.degraded && !cell.failed);
        }
    }

    #[test]
    fn stuck_rounds_abandon_and_real6_heals() {
        let pool = TurnStatePool::new();
        let params = PinParams::from_config(3600, 3, 50);
        let now = now_ms() as u64;
        // 未知格：需要铸票、未放弃。
        assert!(pool.needs_mint(1, "m", &params));
        assert!(!pool.is_abandoned(1, "m", now));
        // 两轮卡住不放弃，第三轮放弃（giveup_rounds=3）。
        assert!(!pool.note_stuck_round(1, "m", 3, 60_000));
        assert!(!pool.note_stuck_round(1, "m", 3, 60_000));
        assert!(pool.note_stuck_round(1, "m", 3, 60_000));
        assert!(pool.is_abandoned(1, "m", now));
        assert!(!pool.needs_mint(1, "m", &params));
        let cell = pool.get_cell(1, "m").unwrap();
        assert_eq!(cell.stuck_rounds, 3);
        assert!(cell.abandoned_until_ms > now);
        // 到期后：不再放弃、需要铸票（给一圈机会）；再卡一轮立即重新放弃。
        assert!(!pool.is_abandoned(1, "m", cell.abandoned_until_ms + 1));
        assert!(pool.note_stuck_round(1, "m", 3, 60_000));
        // 格级休息：停铸但不放弃；到期自动结束。
        pool.park(3, "m", 60_000);
        assert!(pool.is_parked(3, "m", now));
        assert!(!pool.is_abandoned(3, "m", now));
        assert!(!pool.needs_mint(3, "m", &params));
        assert_eq!(
            pool.rest_remaining_s(3, "m", now).map(|s| s <= 60),
            Some(true)
        );
        assert!(!pool.is_parked(3, "m", now + 61_000)); // needs_mint 用墙钟，这里只验到期判定
                                                        // 锁到 292 也清格级休息。
        let real6 = "A".repeat(REAL6_TICKET_LEN);
        pool.capture_if_empty(3, "m", Some(&real6), &params);
        assert!(!pool.is_parked(3, "m", now));
        // giveup_rounds=0 永不放弃。
        for _ in 0..10 {
            assert!(!pool.note_stuck_round(2, "m", 0, 60_000));
        }
        assert!(!pool.is_abandoned(2, "m", now));
        // 锁到 292 真票：清放弃与计数。
        let real6 = "A".repeat(REAL6_TICKET_LEN);
        pool.capture_if_empty(1, "m", Some(&real6), &params);
        let cell = pool.get_cell(1, "m").unwrap();
        assert_eq!(cell.stuck_rounds, 0);
        assert_eq!(cell.abandoned_until_ms, 0);
        assert!(!cell.ever_abandoned);
        assert!(!pool.is_abandoned(1, "m", now));
    }

    #[test]
    fn set_turn_state_pins_and_clears() {
        let pool = TurnStatePool::new();
        let p = params();
        // 保存一个值：锁定可注入。
        pool.set_turn_state(20, "m", Some("gAAAAABmanual"));
        assert_eq!(
            pool.injectable(20, "m", &p).as_deref(),
            Some("gAAAAABmanual")
        );
        // 空串保存：清空(不注入)。
        pool.set_turn_state(20, "m", Some("   "));
        assert!(pool.injectable(20, "m", &p).is_none());
        // 手动保存能把失效格救活。
        pool.record_outcome(20, "m", Outcome::Degraded, 0.0, &p);
        pool.set_turn_state(20, "m", Some("gAAAAABrevive"));
        assert_eq!(
            pool.injectable(20, "m", &p).as_deref(),
            Some("gAAAAABrevive")
        );
        let cells = pool.lock();
        let cell = cells.get(&(20, "m".to_string())).unwrap();
        assert!(!cell.failed && !cell.degraded);
    }

    #[test]
    fn cut_drops_shard_and_resets() {
        let pool = TurnStatePool::new();
        let p = params();
        pool.set_turn_state(9, "m", Some("gAAAAABcut"));
        pool.record_outcome(9, "m", Outcome::Degraded, 0.0, &p);
        // 切片：清分片 + 复位，等下次捕获。
        pool.cut(9, "m");
        assert!(pool.injectable(9, "m", &p).is_none());
        let fresh = t("fresh");
        pool.capture_if_empty(9, "m", Some(&fresh), &p);
        assert_eq!(pool.injectable(9, "m", &p).as_deref(), Some(fresh.as_str()));
    }

    #[test]
    fn live_degrade_strict_requires_zero_reasoning() {
        // 明确 reasoning_tokens:0 且无 reasoning 项 → 降智。
        assert!(reasoning_degraded_live(
            r#"..."type":"message"..."reasoning_tokens":0..."#
        ));
        // 有 reasoning 项 → 正常。
        assert!(!reasoning_degraded_live(
            r#"...{"type":"reasoning"}..."reasoning_tokens":0..."#
        ));
        // reasoning_tokens 键缺失 → 不据此判(非推理响应)，返回 false。
        assert!(!reasoning_degraded_live(r#"..."type":"message"...}"#));
        // reasoning_tokens>0 → 正常。
        assert!(!reasoning_degraded_live(r#"..."reasoning_tokens":89..."#));
    }

    #[test]
    fn expired_turn_state_not_injected() {
        let pool = TurnStatePool::new();
        let p = PinParams::from_config(30, 3, 50);
        let e = t("e");
        pool.capture_if_empty(1, "m", Some(&e), &p);
        // 手动把 pinned_at 拨到很久以前。
        {
            let mut cells = pool.lock();
            let cell = cells.get_mut(&(1, "m".to_string())).unwrap();
            cell.pinned_at_ms = 1;
        }
        assert!(pool.injectable(1, "m", &p).is_none());
    }

    #[test]
    fn export_import_roundtrip_drops_expired() {
        let pool = TurnStatePool::new();
        let p = PinParams::from_config(900, 3, 50);
        let r = t("r");
        pool.capture_if_empty(7, "m", Some(&r), &p);
        let snap = pool.export();
        assert_eq!(snap.len(), 1);
        let pool2 = TurnStatePool::new();
        pool2.import(snap, &p);
        assert_eq!(pool2.injectable(7, "m", &p).as_deref(), Some(r.as_str()));
    }

    #[test]
    fn fernet_timestamp_parses_known_prefix() {
        // 构造一个 version=0x80 + ts 的 token 前缀并 base64url 编码前 9 字节。
        // 这里只验证解析不 panic 且对真实前缀返回 Some。
        let real = "gAAAAABqo6YGUdEq"; // 来自实测 token
        let parsed = parse_fernet_timestamp(real);
        assert!(parsed.is_some());
        // 实测 token 铸于 2025-2026 区间：秒数应在合理范围。
        let secs = parsed.unwrap();
        assert!(secs > 1_600_000_000 && secs < 2_000_000_000, "got {secs}");
    }

    #[test]
    fn parse_model_reads_field() {
        assert_eq!(
            parse_model(br#"{"model":"gpt-6-astra","stream":true}"#),
            Some("gpt-6-astra".to_string())
        );
        assert_eq!(parse_model(br#"{"stream":true}"#), None);
        assert_eq!(parse_model(b"not json"), None);
    }

    #[test]
    fn wants_reasoning_reads_effort() {
        assert!(wants_reasoning(br#"{"reasoning":{"effort":"high"}}"#));
        assert!(wants_reasoning(br#"{"reasoning":{"effort":"medium"}}"#));
        assert!(!wants_reasoning(br#"{"reasoning":{"effort":"low"}}"#));
        assert!(!wants_reasoning(br#"{"reasoning":{"effort":"minimal"}}"#));
        assert!(!wants_reasoning(br#"{"model":"gpt-6-astra"}"#));
        assert!(!wants_reasoning(b"not json"));
    }

    #[test]
    fn prefix_is_char_safe() {
        assert_eq!(prefix("gAAAAABxyz", 6), "gAAAAA");
        assert_eq!(prefix("ab", 6), "ab");
    }

    #[test]
    fn read_pin_inject_parses_account_and_turnstate() {
        let dir = std::env::temp_dir().join(format!("cnt-pin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let diag = dir.join("turnstate-diag.jsonl");
        let pin = dir.join("pin-inject.txt");
        std::fs::write(&pin, "20\ngAAAAABblobvalue\n").unwrap();
        assert_eq!(
            read_pin_inject(diag.to_str().unwrap()),
            Some((20, "gAAAAABblobvalue".to_string()))
        );
        std::fs::write(&pin, "20\n\n").unwrap();
        assert_eq!(read_pin_inject(diag.to_str().unwrap()), None);
        std::fs::remove_file(&pin).unwrap();
        assert_eq!(read_pin_inject(diag.to_str().unwrap()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_writes_jsonl_line() {
        let dir = std::env::temp_dir().join(format!("cnt-diag-test-{}", std::process::id()));
        let path = dir.join("obs.jsonl");
        let _ = std::fs::remove_file(&path);
        let obs = Observation {
            ts_ms: 123,
            account_id: 20,
            outbound_model: Some("gpt-6-astra".to_string()),
            status_code: 200,
            req_session_id: Some("s1".to_string()),
            req_in_turn_state_len: 0,
            req_turn_state_prefix: None,
            req_turn_state_len: 0,
            pinned_injected: false,
            resp_turn_state: Some("gAAAAABblob".to_string()),
            resp_turn_state_len: 11,
            resp_openai_model: Some("gpt-6-astra".to_string()),
            resp_turn_state_changed: true,
            path: "forward".to_string(),
            egress: None,
        };
        append_observation(path.to_str().unwrap(), &obs);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"account_id\":20"));
        assert!(content.trim_end().ends_with('}'));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
