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
use std::sync::atomic::{AtomicU64, Ordering};
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
/// 套餐默认开启降智处理的套餐类型：Business Premium（`self_serve_business_prolite`）。
/// 规则只在 BPS 通道总开关（`bps_enabled`）打开时生效；总开关关掉时，
/// 由本规则自己置上的账号会被自动收回（用户手动勾过的账号一律不碰）。
pub const DEFAULT_DEGRADE_PLAN_TYPES: &[&str] = &["self_serve_business_prolite"];
/// 套餐默认规则的后台检查间隔（秒）：新增账号最迟这么久被套上默认开关。
const PLAN_DEFAULT_INTERVAL_SECONDS: u64 = 60;

/// 线路统计落盘节流：最多每 10s 写一次 degrade-handling.json。
const ROUTE_STATS_PERSIST_INTERVAL_MS: u64 = 10_000;

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
    /// 本次巡检替「借来的模板」兜的底（例如模板的推理挡位被归一化、结构化输出
    /// 被降级）。面板会在结论旁边显示，避免把模板问题误读成账号问题。
    #[serde(default)]
    pub note: String,
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
    /// 线路统计的落盘节流时间（ms）。真实流量下每条请求都写盘不可接受。
    stats_persisted_ms: AtomicU64,
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
    /// 这个开关最近一次被改写的逻辑时间（ms；0 = 从未改写过，按最旧处理）。
    ///
    /// 跨机同步按它做 last-write-wins：本机面板改动写 now_ms()，从对端同步来的
    /// 值沿用对端的时间戳，这样两台对同一个账号的勾选最终收敛到最新那一次。
    #[serde(default)]
    pub enabled_at_ms: u64,
    /// true = 这个开关是「套餐默认」规则自动置上的（当前规则：Business Premium），
    /// 不是用户点出来的。用户 / 对端同步显式改动过（enabled_at_ms > 1）的账号
    /// 永远不会被规则回收；只有 defaulted 的号在规则不再适用（套餐变了 /
    /// BPS 总开关关掉）时才自动关掉，避免面板显示与配置口径不一致。
    #[serde(default)]
    pub defaulted: bool,
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
    /// 线路统计：正常通道成功 / 失败次数。
    #[serde(default)]
    pub normal_ok: u64,
    #[serde(default)]
    pub normal_fail: u64,
    /// 线路统计：BPS 通道成功 / 失败次数。
    #[serde(default)]
    pub bps_ok: u64,
    #[serde(default)]
    pub bps_fail: u64,
    /// 最近一次实际使用的线路（"normal" / "bps"）与时间。
    #[serde(default)]
    pub last_route: String,
    #[serde(default)]
    pub last_route_at_ms: u64,
    /// 最近一次上游状态码（0 = 传输层就没连上）。
    #[serde(default)]
    pub last_status: u32,
    /// 累计记账的请求数（正常 + BPS），面板用来算成功率。
    #[serde(default)]
    pub route_events: u64,
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
                                // 旧格式没有时间戳：按「最旧但已存在」处理（1ms），
                                // 这样对端任何一次显式改动（now_ms）都能盖过它。
                                enabled_at_ms: 1,
                                ..ChannelState::default()
                            },
                            other => {
                                serde_json::from_value::<ChannelState>(other).unwrap_or_default()
                            }
                        };
                        let state = ChannelState {
                            enabled_at_ms: if state.enabled_at_ms == 0 {
                                1
                            } else {
                                state.enabled_at_ms
                            },
                            ..state
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

    /// 按套餐规则套用默认开关：BPS 总开关打开时，Business Premium 默认参与降智处理。
    ///
    /// 只对**从未被显式勾选过**的账号生效（`enabled_at_ms <= 1` 表示面板没点过、
    /// 对端也没同步过）。用户手动关掉的号时间戳是 now_ms，所以规则不会把它翻回来。
    /// 规则自己置上的号带 `defaulted` 标记，规则不再适用时能被自动收回。
    /// 返回是否真的改动了本机状态。
    pub fn apply_plan_default(&self, account_id: i64, should_enable: bool, path: &str) -> bool {
        self.ensure_loaded(path);
        let now = now_ms();
        let mut changed = false;
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = guard.states.entry(account_id).or_default();
            // 显式改动过（面板 / 对端同步）：套餐规则不介入，只顺手清掉可能残留的
            // 「套餐默认」来源标记（例如规则置上后又被面板手动开关过）。
            if entry.enabled_at_ms > 1 {
                if entry.defaulted {
                    entry.defaulted = false;
                    changed = true;
                }
            } else if should_enable {
                if !entry.enabled {
                    entry.enabled = true;
                    entry.failed = false;
                    entry.healthy_streak = 0;
                    entry.healthy_since_ms = 0;
                    if entry.bps_since_ms == 0 {
                        entry.bps_since_ms = now;
                    }
                    entry.last_switch_ms = now;
                    entry.last_reason =
                        "Business Premium 套餐默认开启降智处理，先按 BPS 兜住".to_string();
                    changed = true;
                }
                if !entry.defaulted {
                    entry.defaulted = true;
                    changed = true;
                }
            } else if entry.enabled && entry.defaulted {
                entry.enabled = false;
                entry.defaulted = false;
                entry.bps_since_ms = 0;
                entry.last_switch_ms = now;
                entry.last_reason = "套餐默认规则不再适用，回到正常通道".to_string();
                changed = true;
            } else if entry.defaulted {
                // 规则置上后又不再适用（但账号本来就没开着）：只清来源标记。
                entry.defaulted = false;
                changed = true;
            }
        }
        if changed {
            self.persist(path);
        }
        changed
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
            // 本机改动总是刷新时间戳：它是同步时的权威版本。
            entry.enabled_at_ms = now;
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

    /// 应用对端推来的开关状态（跨机同步）。
    ///
    /// 只在时间戳更新时采纳（last-write-wins）；时间戳相同则「开」优先——两台
    /// 都是旧格式（时间戳 1ms）时，谁勾了降智处理就以谁为准，不会被空状态抹掉。
    /// 返回是否真的改动了本机状态（调用方不需要再转发，避免来回震荡）。
    pub fn set_remote(
        &self,
        account_id: i64,
        enabled: bool,
        at_ms: u64,
        reason: &str,
        path: &str,
    ) -> bool {
        self.ensure_loaded(path);
        let now = now_ms();
        let mut changed = false;
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = guard.states.entry(account_id).or_default();
            let newer = at_ms > entry.enabled_at_ms
                || (at_ms == entry.enabled_at_ms && enabled && !entry.enabled);
            if newer {
                if entry.enabled != enabled {
                    changed = true;
                    entry.last_switch_ms = now;
                    entry.last_reason = if reason.trim().is_empty() {
                        if enabled {
                            "对端同步：开启降智处理".to_string()
                        } else {
                            "对端同步：关闭降智处理".to_string()
                        }
                    } else {
                        format!("对端同步：{}", reason.trim())
                    };
                }
                entry.enabled = enabled;
                entry.enabled_at_ms = at_ms;
                if enabled {
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
        }
        if changed {
            self.persist(path);
        }
        changed
    }

    /// 跨机同步用的快照：只带开关本身（线路统计是各机自己的流量，不参与同步）。
    pub fn sync_entries(&self, path: &str) -> Vec<(i64, bool, u64, String)> {
        self.ensure_loaded(path);
        self.snapshot()
            .into_iter()
            .filter(|(_, state)| state.enabled || state.enabled_at_ms > 0)
            .map(|(id, state)| (id, state.enabled, state.enabled_at_ms, state.last_reason))
            .collect()
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

    /// 记一次真实流量的线路结果（面板「线路统计」）。
    ///
    /// `route` = "bps" / "normal"，`status` = 上游状态码（0 = 传输层失败）。
    /// 计数在内存里累加，落盘按 `ROUTE_STATS_PERSIST_INTERVAL_MS` 节流，
    /// 避免每条请求都写一次 JSON。
    pub fn record_route(&self, account_id: i64, route: &str, status: u16, path: &str) {
        self.ensure_loaded(path);
        let bps = route == "bps";
        let ok = (200..400).contains(&status);
        let now = now_ms();
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = guard.states.entry(account_id).or_default();
            if bps {
                if ok {
                    entry.bps_ok = entry.bps_ok.saturating_add(1);
                } else {
                    entry.bps_fail = entry.bps_fail.saturating_add(1);
                }
            } else if ok {
                entry.normal_ok = entry.normal_ok.saturating_add(1);
            } else {
                entry.normal_fail = entry.normal_fail.saturating_add(1);
            }
            entry.last_route = if bps { "bps" } else { "normal" }.to_string();
            entry.last_route_at_ms = now;
            entry.last_status = status as u32;
            entry.route_events = entry.route_events.saturating_add(1);
        }
        let last = self.stats_persisted_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= ROUTE_STATS_PERSIST_INTERVAL_MS
            && self
                .stats_persisted_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.persist(path);
        }
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

/// [`build_intel_body_with_note`] 的薄封装：只要请求体。
pub fn build_intel_body(template_body: &[u8], model: &str, question: &str) -> Vec<u8> {
    build_intel_body_with_note(template_body, model, question).0
}

/// 用模板 body 构造巡检 body：只换 model / input / 推理挡位，强制 stream +
/// store:false，其余字段（instructions / reasoning / include / tools …）原样保留。
///
/// 返回 `(body, note)`：`note` 非空表示模板里有东西被我们兜了底（目前是推理挡位），
/// 面板会跟着这次巡检一起显示。
pub fn build_intel_body_with_note(
    template_body: &[u8],
    model: &str,
    question: &str,
) -> (Vec<u8>, Option<String>) {
    let fallback = || {
        let body = format!(
            r#"{{"model":{},"instructions":"You are a careful reasoning assistant.","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":{}}}]}}],"stream":true,"store":false}}"#,
            serde_json::to_string(model).unwrap_or_else(|_| "\"gpt-6-astra\"".to_string()),
            serde_json::to_string(question).unwrap_or_else(|_| "\"?\"".to_string())
        )
        .into_bytes();
        (body, None)
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
    // 推理挡位：模板来自某个会员的真实请求，挡位可能是 Codex 的弱挡位
    // （`minimal` / `none`）。巡检模型（默认 gpt-6-astra）不认 `minimal`，
    // 原样发出去就是整池 400，然后被误读成「智力不合格」——142 上一轮 53 个号
    // 正是这么被误判的。归一化函数与 BPS 通道**共用同一个**；
    // 模板给了完全不认识的值时退回 `medium` 并写进 note（探针不能因为模板怪值
    // 把整池巡检带崩，但也不能瞒着不说）。
    let requested = crate::bps::requested_effort(obj);
    let mut note = None;
    if !requested.is_empty() {
        let normalized = match crate::bps::normalize_effort(&requested) {
            Ok(value) => value,
            Err(err) => {
                note = Some(format!(
                    "模板推理挡位 {requested} 无法归一化（{err}），本次巡检改用 medium"
                ));
                "medium".to_string()
            }
        };
        if normalized != requested {
            note.get_or_insert_with(|| format!("模板推理挡位 {requested} 已归一化为 {normalized}"));
            set_requested_effort(obj, &normalized);
        }
    }
    // 巡检是独立探针，绝不接续任何会员的响应链。
    obj.remove("previous_response_id");
    // 真实 Codex 模板带 15 个工具（exec_command / apply_patch / mcp__codex_app.* …）。
    // 原样转发时模型经常不去回答「知识库日期」，而是先发一个 `function_call`
    // 输出项（甚至整轮都用来调工具），SSE 里没有任何 `output_text` 帧，
    // `extract_output_text` 拿不到正文，面板上就表现为大面积
    // 「上游未返回回答正文」。巡检只想要一句自然语言回答，这里把工具声明
    // 一并剥掉。
    strip_probe_tools(obj);
    // 模板可能来自「某个会员要求结构化输出」的真实请求（`text.format` =
    // json_object / json_schema）。上游对这两种格式有硬校验：input 里必须出现
    // json 字样，而巡检问的是自然语言的「知识库日期」问题 → 整池账号齐刷刷
    // 400 `Response input messages must contain the word 'json' …`。巡检只要
    // 纯文本回答，这里把结构化约束降级成 text。
    downgrade_structured_text_format(obj);
    match serde_json::to_vec(&value) {
        Ok(body) => (body, note),
        Err(_) => fallback(),
    }
}

/// 把请求体里的推理挡位写成 `value`（`reasoning.effort` 优先，其次顶层
/// `reasoning_effort`）；两个位置都没给就保持原样（不硬塞字段）。
fn set_requested_effort(obj: &mut serde_json::Map<String, serde_json::Value>, value: &str) {
    if let Some(reasoning) = obj
        .get_mut("reasoning")
        .and_then(serde_json::Value::as_object_mut)
    {
        if reasoning
            .get("effort")
            .and_then(serde_json::Value::as_str)
            .is_some()
        {
            reasoning.insert(
                "effort".to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
    if obj
        .get("reasoning_effort")
        .and_then(serde_json::Value::as_str)
        .is_some()
    {
        obj.insert(
            "reasoning_effort".to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
}

/// 剥掉巡检探针用不到的字段：`tools` / `tool_choice` / `parallel_tool_calls`。
///
/// `input` 已经在上面被整体换成单条问题，所以 Responses-Lite 挂在 input 上的
/// `additional_tools` 声明也随之消失；这里只清顶层字段。注意 lite 请求头还要求
/// `parallel_tool_calls` **显式**为 false —— 需要它在场时由
/// [`apply_lite_probe_contract`] 补回来，所以剥和补是分开的两件事。
fn strip_probe_tools(obj: &mut serde_json::Map<String, serde_json::Value>) {
    obj.remove("tools");
    obj.remove("tool_choice");
    obj.remove("parallel_tool_calls");
}

/// Responses-Lite 请求头（小写）。模板一旦从这个头漂到 lite 形状，请求体就必须
/// 同时满足两条上游硬校验，缺一条整池 400。
const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// 给探针请求体补上 Responses-Lite 契约，两条都是实测出来的硬校验：
///
/// * `parallel_tool_calls` 必须**显式**是 `false`
///   → `X-OpenAI-Internal-Codex-Responses-Lite requires 'parallel_tool_calls' to be false.`
///   （不是"没有这个字段"就行：把工具字段整块剥掉之后，142 上 67/72 个号
///   齐刷刷撞上了这条 400，所以剥完还要按需补回 `false`。）
/// * `reasoning.context` 必须是 `all_turns`
///   → `X-OpenAI-Internal-Codex-Responses-Lite requires 'reasoning.context' to be 'all_turns'`
///
/// 只有模板确实带 lite 头时才改请求体，正常通道一字不动。返回 `None` = 无需改动。
pub fn apply_lite_probe_contract(body: &[u8]) -> Option<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = value.as_object_mut()?;
    let mut changed = false;
    if obj.get("parallel_tool_calls") != Some(&serde_json::Value::Bool(false)) {
        obj.insert(
            "parallel_tool_calls".to_string(),
            serde_json::Value::Bool(false),
        );
        changed = true;
    }
    let reasoning = obj
        .entry("reasoning".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if let Some(reasoning) = reasoning.as_object_mut() {
        if reasoning.get("context").and_then(serde_json::Value::as_str) != Some("all_turns") {
            reasoning.insert(
                "context".to_string(),
                serde_json::Value::String("all_turns".to_string()),
            );
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

/// 上游是因为 Responses-Lite 契约拒绝探针吗？
///
/// lite 的校验是随版本往上加的（先是 `reasoning.context`，后来又有
/// `parallel_tool_calls`），以后还可能再加。只要 400 的正文点名了
/// `Responses-Lite`，就值得摘掉 lite 头、退回普通 Codex 形状再试一次；
/// 其它 400（模型不存在、参数非法…）是账号本身的问题，重试没意义。
pub fn is_lite_contract_rejection(status: u16, body: &str) -> bool {
    status == 400 && body.contains("Responses-Lite")
}

/// 把 `text.format` 里的结构化输出约束（json_object / json_schema）降级为 text。
fn downgrade_structured_text_format(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(text) = obj
        .get_mut("text")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let structured = text
        .get("format")
        .and_then(serde_json::Value::as_object)
        .and_then(|format| format.get("type"))
        .and_then(serde_json::Value::as_str)
        .map(|kind| matches!(kind, "json_object" | "json_schema"))
        .unwrap_or(false);
    if structured {
        // 整体替换，别把 json_schema 的 name / schema / strict 残留下来。
        text.insert("format".to_string(), serde_json::json!({"type": "text"}));
    }
}

/// 从 Responses SSE 文本里抽出助手回答正文。
pub fn extract_output_text(sse: &str) -> String {
    let mut out = String::new();
    // 增量帧一旦出现过，后续的 `output_item.done` / `output_text.done` 都只是
    // 同一段正文的完整回放；再收一遍会让面板里的回答变成「xxx xxx」两遍。
    let mut saw_delta = false;
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
                    saw_delta = true;
                }
            }
            // 有些上游（含转发链路）不吐增量，直接给完整文本。
            "response.output_text.done" => {
                if !saw_delta {
                    if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
                        out.push_str(text);
                    }
                }
            }
            "response.output_item.done" => {
                if !saw_delta {
                    if let Some(item) = value.get("item") {
                        collect_item_text(item, &mut out);
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
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

/// 一次 200 响应的诊断摘要：SSE 事件序列 + 输出项类型 + 上游错误帧 + 断流原因。
///
/// 以前这里是直接 `clip(sse, 400)`，但那段永远只是 `response.created` 里回显的
/// `instructions`（真实 Codex 系统提示词上万字符），把失败原因挡在了 400 字符
/// 之外，根本查不出「上游未返回回答正文」到底是断流、截断还是工具调用。
pub fn sse_digest(sse: &str, stream_error: Option<&str>) -> String {
    let mut hist: Vec<(String, usize)> = Vec::new();
    let mut items: Vec<String> = Vec::new();
    let mut upstream_error = String::new();
    let mut status_note = String::new();
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
            .unwrap_or("?")
            .to_string();
        match hist.last_mut() {
            Some(last) if last.0 == kind => last.1 += 1,
            _ => hist.push((kind.clone(), 1)),
        }
        if let Some(item) = value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(serde_json::Value::as_str)
        {
            if !items.iter().any(|seen| seen == item) {
                items.push(item.to_string());
            }
        }
        if upstream_error.is_empty() && matches!(kind.as_str(), "error" | "response.failed") {
            let err = value
                .get("error")
                .filter(|err| !err.is_null())
                .cloned()
                .or_else(|| value.get("response").and_then(|r| r.get("error")).cloned())
                .unwrap_or(serde_json::Value::Null);
            upstream_error = clip(&err.to_string(), 220);
        }
        if status_note.is_empty() {
            if let Some(status) = value
                .get("response")
                .and_then(|r| r.get("status"))
                .and_then(serde_json::Value::as_str)
            {
                if matches!(status, "failed" | "incomplete") {
                    let detail = value
                        .get("response")
                        .and_then(|r| r.get("incomplete_details"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    status_note = if detail.is_null() {
                        format!("response.status={status}")
                    } else {
                        format!("response.status={status} {detail}")
                    };
                }
            }
        }
    }
    let mut out = if hist.is_empty() {
        "SSE 没有任何事件帧".to_string()
    } else {
        format!(
            "SSE 事件: {}",
            hist.iter()
                .map(|(kind, count)| if *count > 1 {
                    format!("{kind}×{count}")
                } else {
                    kind.clone()
                })
                .collect::<Vec<_>>()
                .join(" → ")
        )
    };
    if !items.is_empty() {
        out.push_str("；输出项: ");
        out.push_str(&items.join(","));
    }
    if !upstream_error.is_empty() {
        out.push_str("；上游错误: ");
        out.push_str(&upstream_error);
    }
    if !status_note.is_empty() {
        out.push('；');
        out.push_str(&status_note);
    }
    if let Some(err) = stream_error {
        out.push_str("；断流原因: ");
        out.push_str(err);
    }
    clip(&out, 600)
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
    let (mut plain_body, template_note) =
        build_intel_body_with_note(&donor.body, &model, &question);
    if let Some(note) = template_note {
        // 模板兜底的说明跟着这次结论一起落盘 / 上屏。
        result.note = note;
    }
    if let Some(rewritten) = crate::identity::apply_one_id_body(&plain_body, &ids) {
        plain_body = rewritten;
    }
    // 模板带 lite 头时，请求体必须满足 lite 契约；同时留一份「摘掉 lite 头」的
    // 普通形状做兜底 —— lite 的校验是随上游版本往上加的，将来再加一条也能自愈。
    let mut plain_headers = headers.clone();
    plain_headers.remove(LITE_HEADER);
    let mut use_headers = headers;
    let mut body = plain_body.clone();
    let mut lite_mode = use_headers.contains_key(LITE_HEADER);
    if lite_mode {
        if let Some(rewritten) = apply_lite_probe_contract(&body) {
            body = rewritten;
        }
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
    let mut last_digest = String::new();
    for attempt in 1..=attempts {
        if attempt > 1 {
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        result.checked_at_ms = now_ms();
        let send = client
            .request(reqwest::Method::POST, &donor.url)
            .headers(use_headers.clone())
            .body(body.clone())
            .send();
        let response = match tokio::time::timeout(timeout, send).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                let classified = crate::transport::classify_reqwest_error(&err);
                timed_out = false;
                last_error = format!("请求失败 [{}]: {}", classified.code, classified.message);
                continue;
            }
            Err(_) => {
                timed_out = true;
                last_error = format!("请求超时（{}s）", timeout.as_secs());
                continue;
            }
        };

        let status = response.status();
        let mut sse = String::new();
        // 上游中途断流（h2 GOAWAY / RST_STREAM / 分块截断）以前是静默 break，
        // 于是被判成「上游未返回回答正文」，日志里查不出半点线索。
        let mut stream_error: Option<String> = None;
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
                    Err(err) => {
                        stream_error = Some(crate::transport::classify_reqwest_error(&err).message);
                        break;
                    }
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
            // lite 契约被拒：摘掉 lite 头、换回普通 Codex 形状再试一次，
            // 免得上游又加一条校验就让整池巡检瘫掉。
            if lite_mode && is_lite_contract_rejection(status.as_u16(), &sse) && attempt < attempts
            {
                lite_mode = false;
                use_headers = plain_headers.clone();
                body = plain_body.clone();
                last_error = format!(
                    "上游返回 {}（Responses-Lite 契约被拒），已改用普通通道重试",
                    status.as_u16()
                );
                continue;
            }
            // 状态码本身就是结论（401/429/5xx），重试没有意义。
            result.error = format!("上游返回 {}: {}", status.as_u16(), clip(&sse, 200));
            return result;
        }

        let answer = extract_output_text(&sse);
        if !answer.trim().is_empty() {
            result.answer = clip(&answer, 400);
            result.ok = !answer_is_unqualified(&answer, &markers);
            if result.ok && config.intel_require_year && !answer_has_year(&answer) {
                // 没有年份证据 = 没回答到「知识库日期」这个点，按降智处理。
                result.ok = false;
            }
            return result;
        }

        // 200 但一个正文帧都没有：断流/截断大多是瞬时的，换一次时机重试。
        last_digest = sse_digest(&sse, stream_error.as_deref());
        last_error = match stream_error.as_deref() {
            Some(err) => format!("上游流中断: {err}"),
            None => "上游未返回回答正文".to_string(),
        };
        timed_out = false;
    }

    result.error = if attempts > 1 {
        format!("{last_error}（已尝试 {attempts} 次）")
    } else {
        last_error
    };
    result.answer = last_digest;
    result.timed_out = timed_out;
    if timed_out && config.intel_timeout_is_failed {
        result.ok = false;
    }
    result.latency_ms = began.elapsed().as_millis() as i64;
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
    // 套餐默认规则先套一遍：新增的 Business Premium 账号不用等下一轮巡检。
    apply_target_plan_defaults(state, config, &all);
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

/// 套餐默认规则：BPS 总开关打开时，Business Premium（`self_serve_business_prolite`）
/// 默认参与降智处理；其它套餐保持现状（默认不参与，只有面板勾选才走 BPS）。
pub fn plan_default_degrade(config: &PluginConfig, plan_type: &str) -> bool {
    if !config.bps_enabled {
        return false;
    }
    let normalized = normalize_plan_type(plan_type);
    DEFAULT_DEGRADE_PLAN_TYPES
        .iter()
        .any(|want| *want == normalized)
}

/// 把套餐默认规则套到一批账号上（巡检 / 面板取到账号列表时调用）。
/// 返回真正被改动的账号数；有改动时通知跨机同步，两台尽快收敛到同一份状态。
pub fn apply_target_plan_defaults(
    state: &Arc<SharedState>,
    config: &PluginConfig,
    targets: &[AdminTarget],
) -> usize {
    let path = config.degrade_state_file();
    state.degrade.ensure_loaded(&path);
    let mut changed = 0usize;
    for target in targets {
        let should_enable = plan_default_degrade(config, &target.plan_type);
        if state
            .degrade
            .apply_plan_default(target.account_id, should_enable, &path)
        {
            changed += 1;
        }
    }
    if changed > 0 {
        crate::sync::notify();
    }
    changed
}

/// 套餐默认规则的后台循环：只读账号列表（不拉凭据导出），最多每 60s 对一次，
/// 让**新增**的 Business Premium 账号自动带上默认开关，不用等下一轮全量巡检。
/// BPS 总开关关掉时，同一段代码负责把规则自己开的号收回。
pub async fn plan_default_loop(state: Arc<SharedState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(PLAN_DEFAULT_INTERVAL_SECONDS)).await;
        let config = state.current_config();
        let base = config.admin_api_base.trim().to_string();
        let key = config.admin_api_key.trim().to_string();
        if base.is_empty() || key.is_empty() {
            continue;
        }
        let rows = match admin::fetch_plan_rows(&base, &key).await {
            Ok(rows) => rows,
            Err(_) => continue,
        };
        let path = config.degrade_state_file();
        state.degrade.ensure_loaded(&path);
        let mut changed = 0usize;
        for row in rows {
            let should_enable = plan_default_degrade(&config, &row.plan_type);
            if state
                .degrade
                .apply_plan_default(row.account_id, should_enable, &path)
            {
                changed += 1;
            }
        }
        if changed > 0 {
            eprintln!("[codex-native-transport] plan default degrade updated {changed} account(s)");
            crate::sync::notify();
        }
    }
}

/// 面板用：账号 + 上一次巡检结论 + 降智处理开关 的合并视图。
pub fn merge_rows(
    targets: &[AdminTarget],
    results: &BTreeMap<i64, IntelResult>,
    channels: &BTreeMap<i64, ChannelState>,
    sticky: &crate::service::ChannelSticky,
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
        // BPS 回退冷却与当日次数：由出站路径（service.rs）维护，这里只读。
        let cooldown_left_ms = sticky.cooldown_remaining_ms(target.account_id, now);
        let (cooldown_reason, cooldown_at_ms) = sticky
            .last_reason(target.account_id)
            .map(|(at, text)| (text, at))
            .unwrap_or_default();
        let bps_today = sticky.daily_count(target.account_id, now);
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
            "note": result.map(|r| r.note.clone()).unwrap_or_default(),
            "latency_ms": result.map(|r| r.latency_ms).unwrap_or(0),
            "checked_at_ms": result.map(|r| r.checked_at_ms).unwrap_or(0),
            "model": result.map(|r| r.model.clone()).unwrap_or_default(),
            "timed_out": result.map(|r| r.timed_out).unwrap_or(false),
            "degrade": state.enabled,
            // true = 这个「已开启」是套餐默认规则置上的，不是人工勾的。
            "degrade_default": state.defaulted,
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
            // 线路统计：正常 / BPS 各自的成功、失败次数，以及最近一次实际线路。
            "normal_ok": state.normal_ok,
            "normal_fail": state.normal_fail,
            "bps_ok": state.bps_ok,
            "bps_fail": state.bps_fail,
            "total_ok": state.normal_ok.saturating_add(state.bps_ok),
            "total_fail": state.normal_fail.saturating_add(state.bps_fail),
            "route_events": state.route_events,
            "last_route": state.last_route,
            "last_route_at_ms": state.last_route_at_ms,
            "last_status": state.last_status,
            // BPS 回退冷却：剩余毫秒 + 最近一次回退原因；bps_today 是今天（UTC）
            // 已经打到 BPS 端点的次数，bps_daily_limit 是配置上限（0 = 不限）。
            "bps_cooldown_remaining_ms": cooldown_left_ms,
            "bps_cooldown_reason": cooldown_reason,
            "bps_cooldown_at_ms": cooldown_at_ms,
            "bps_today": bps_today,
            "bps_daily_limit": config.bps_daily_limit_per_account,
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
    fn intel_body_downgrades_structured_output_format() {
        // 模板里带 json_object：巡检问题不含 json 字样，原样转发会被上游 400。
        let tpl = br#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"return json"}]}],"text":{"format":{"type":"json_object"}},"stream":false,"store":true}"#;
        let body = build_intel_body(tpl, "gpt-6-astra", "你在知识库日期 不允许联网快速回答");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["text"]["format"]["type"], "text");

        // json_schema 同样降级，且不残留 name / schema / strict。
        let tpl2 = br#"{"model":"m","input":[],"text":{"format":{"type":"json_schema","name":"x","strict":true,"schema":{"type":"object"}}}}"#;
        let body2 = build_intel_body(tpl2, "gpt-6-astra", "q");
        let v2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
        assert_eq!(v2["text"]["format"]["type"], "text");
        assert!(v2["text"]["format"].get("schema").is_none());
        assert!(v2["text"]["format"].get("name").is_none());

        // 已经是纯文本（或没有 text 字段）的模板保持原样。
        let tpl3 =
            br#"{"model":"m","input":[],"text":{"verbosity":"low","format":{"type":"text"}}}"#;
        let body3 = build_intel_body(tpl3, "gpt-6-astra", "q");
        let v3: serde_json::Value = serde_json::from_slice(&body3).unwrap();
        assert_eq!(v3["text"]["format"]["type"], "text");
        assert_eq!(v3["text"]["verbosity"], "low");
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
    fn intel_body_normalizes_template_effort() {
        // 模板里是 Codex 的弱挡位：巡检模型（gpt-6-astra）不认 minimal，原样发出去
        // 就是整池 400，然后被误读成「智力不合格」——142 上一轮 53 个号正是如此。
        // 归一化与 BPS 通道共用同一份规则。
        let tpl = br#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"reasoning":{"effort":"minimal","summary":"concise"}}"#;
        let (body, note) = build_intel_body_with_note(tpl, "gpt-6-astra", "q");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["reasoning"]["effort"], "low");
        assert_eq!(v["reasoning"]["summary"], "concise");
        assert!(note.unwrap().contains("minimal"));

        // 强挡位封顶：上游没有 max。
        let tpl = br#"{"model":"m","input":[],"reasoning_effort":"max"}"#;
        let (body, note) = build_intel_body_with_note(tpl, "gpt-6-astra", "q");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["reasoning_effort"], "xhigh");
        assert!(note.unwrap().contains("max"));

        // 完全不认识的值：不能让模板怪值把整池巡检带崩，退 medium 并写进 note。
        let tpl = br#"{"model":"m","input":[],"reasoning":{"effort":"off"}}"#;
        let (body, note) = build_intel_body_with_note(tpl, "gpt-6-astra", "q");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["reasoning"]["effort"], "medium");
        assert!(note.unwrap().contains("off"));

        // 模板没给挡位：不硬塞字段，也不留 note。
        let tpl = br#"{"model":"m","input":[]}"#;
        let (body, note) = build_intel_body_with_note(tpl, "gpt-6-astra", "q");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("reasoning").is_none());
        assert!(v.get("reasoning_effort").is_none());
        assert!(note.is_none());
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
    fn extract_output_text_does_not_duplicate_replayed_items() {
        // 增量帧之后再收到 output_item.done / output_text.done 的完整回放，
        // 只能算一次（否则面板里会出现「2024 年6 月2024 年6 月」）。
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"2024 年\"}\n\n\
                   data: {\"type\":\"response.output_text.done\",\"text\":\"2024 年 6 月\"}\n\n\
                   data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"2024 年 6 月\"}]}}\n\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"2024 年 6 月\"}]}]}}\n\n";
        assert_eq!(extract_output_text(sse), "2024 年");

        // 完全没有增量帧时，done 帧仍然要兜住正文。
        let no_delta =
            "data: {\"type\":\"response.output_text.done\",\"text\":\"2025 年 1 月\"}\n\n";
        assert_eq!(extract_output_text(no_delta), "2025 年 1 月");

        // incomplete 的最终响应也要能兜住（有些上游只在这里给正文）。
        let incomplete = "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"2024\"}]}]}}\n\n";
        assert_eq!(extract_output_text(incomplete), "2024");
    }

    #[test]
    fn intel_body_strips_probe_tools() {
        let tpl = br#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]}],"tools":[{"type":"function","name":"exec_command"}],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":{"effort":"xhigh"}}"#;
        let body = build_intel_body(tpl, "gpt-6-astra", "你的知识库日期");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("tools").is_none());
        assert!(v.get("tool_choice").is_none());
        assert!(v.get("parallel_tool_calls").is_none());
        // reasoning / instructions 这类形状字段照旧保留。
        assert_eq!(v["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn lite_contract_adds_both_required_fields() {
        // 模板被剥掉工具后既没有 parallel_tool_calls 也没有 reasoning.context：
        // 两条上游硬校验都会踩，必须一起补齐。
        let body = br#"{"model":"m","input":[],"reasoning":{"effort":"high"}}"#;
        let rewritten = apply_lite_probe_contract(body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["parallel_tool_calls"], false);
        assert_eq!(v["reasoning"]["context"], "all_turns");
        assert_eq!(v["reasoning"]["effort"], "high");
        // 已经满足了 → 不再重写（返回 None，省一次序列化）。
        assert!(apply_lite_probe_contract(&rewritten).is_none());
        // 完全没有 reasoning 字段也要能补出来。
        let bare = apply_lite_probe_contract(br#"{"model":"m","input":[]}"#).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bare).unwrap();
        assert_eq!(v["reasoning"]["context"], "all_turns");
        assert_eq!(v["parallel_tool_calls"], false);
    }

    #[test]
    fn strip_then_lite_contract_keeps_parallel_tool_calls_false() {
        // 真实模板（lite 形状）带着 tools + parallel_tool_calls:false；
        // build_intel_body 会把工具字段剥掉，随后 lite 契约必须把 false 放回去，
        // 否则上游 400 `requires 'parallel_tool_calls' to be false.`。
        let tpl = br#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]}],"tools":[{"type":"function","name":"exec_command"}],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":{"effort":"xhigh"}}"#;
        let body = build_intel_body(tpl, "gpt-6-astra", "你的知识库日期");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("tools").is_none());
        assert!(v.get("parallel_tool_calls").is_none());
        let fixed = apply_lite_probe_contract(&body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fixed).unwrap();
        assert_eq!(v["parallel_tool_calls"], false);
        assert_eq!(v["reasoning"]["context"], "all_turns");
    }

    #[test]
    fn lite_contract_rejection_is_recognised_narrowly() {
        let lite_400 = "{\n  \"error\": {\n    \"message\": \"X-OpenAI-Internal-Codex-Responses-Lite requires `parallel_tool_calls` to be false.\"";
        assert!(is_lite_contract_rejection(400, lite_400));
        // 其它 400 是账号本身的问题，不该被误当成 lite 契约问题去摘头重试。
        assert!(!is_lite_contract_rejection(
            400,
            "{\"error\":{\"message\":\"model not found\"}}"
        ));
        assert!(!is_lite_contract_rejection(429, lite_400));
    }

    #[test]
    fn sse_digest_names_the_failure_reason() {
        let truncated = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"instructions\":\"You are Codex\"}}\n\n\
                         event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_1\"}}\n\n";
        let digest = sse_digest(
            truncated,
            Some("connection closed before message completed"),
        );
        assert!(digest.contains("response.created"), "{digest}");
        assert!(digest.contains("response.in_progress"), "{digest}");
        assert!(digest.contains("断流原因"), "{digest}");
        // 以前的实现只会截到 response.created 里回显的 instructions，看不到上面这些。
        assert!(!digest.contains("You are Codex"), "{digest}");

        let failed = "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"boom\"}}}\n\n";
        let digest = sse_digest(failed, None);
        assert!(digest.contains("server_error"), "{digest}");
        assert!(digest.contains("response.status=failed"), "{digest}");

        let empty = sse_digest("", None);
        assert!(empty.contains("没有任何事件帧"), "{empty}");
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

    #[test]
    fn degrade_store_reads_legacy_json_without_route_stats() {
        let dir = std::env::temp_dir().join(format!("cnt-legacy-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("degrade-handling.json");
        // 旧版格式：布尔开关 / 只有老字段的状态对象，都必须能读。
        std::fs::write(
            &path,
            br#"{"54":true,"55":{"enabled":true,"failed":true,"bps_since_ms":123}}"#,
        )
        .unwrap();
        let store = DegradeStore::default();
        let path = path.to_string_lossy().to_string();
        store.ensure_loaded(&path);
        assert!(store.is_enabled(54));
        assert!(store.is_enabled(55));
        let states = store.snapshot();
        let s = states.get(&55).unwrap();
        assert_eq!(s.bps_since_ms, 123);
        // 老文件没有统计字段 → 默认 0 / 空，不算反序列化失败。
        assert_eq!(s.normal_ok, 0);
        assert!(s.last_route.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn route_stats_count_success_and_failure_per_route() {
        let dir = std::env::temp_dir().join(format!("cnt-route-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("degrade-handling.json");
        let path = path.to_string_lossy().to_string();
        let store = DegradeStore::default();
        store.ensure_loaded(&path);
        store.record_route(88, "normal", 200, &path);
        store.record_route(88, "normal", 200, &path);
        store.record_route(88, "normal", 429, &path);
        store.record_route(88, "bps", 200, &path);
        store.record_route(88, "bps", 0, &path);
        let s = store.snapshot().get(&88).cloned().unwrap();
        assert_eq!(s.normal_ok, 2);
        assert_eq!(s.normal_fail, 1);
        assert_eq!(s.bps_ok, 1);
        assert_eq!(s.bps_fail, 1);
        assert_eq!(s.route_events, 5);
        assert_eq!(s.last_route, "bps");
        assert_eq!(s.last_status, 0);
        assert!(!s.enabled);
        // 统计不改变通道判定（BPS 未开启的号永远走正常通道）。
        let config = channel_test_config();
        assert!(!channel_decision(&s, &config, now_ms()).use_bps);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_default_covers_business_premium_only_when_bps_on() {
        let mut config = channel_test_config();
        config.bps_enabled = false;
        assert!(!plan_default_degrade(
            &config,
            "self_serve_business_prolite"
        ));
        config.bps_enabled = true;
        // 宿主展示口径的大小写 / 连字符写法都要认。
        assert!(plan_default_degrade(&config, "self_serve_business_prolite"));
        assert!(plan_default_degrade(&config, "self-serve-business-prolite"));
        assert!(plan_default_degrade(
            &config,
            " Self Serve Business ProLite "
        ));
        // 其它套餐保持现状：默认不参与，只有面板勾选才走 BPS。
        assert!(!plan_default_degrade(&config, "pro"));
        assert!(!plan_default_degrade(&config, "plus"));
        assert!(!plan_default_degrade(&config, ""));
    }

    #[test]
    fn plan_default_enables_new_business_premium_and_respects_manual_choice() {
        let store = DegradeStore::default();
        store.ensure_loaded("");

        // 新增（从未勾过）的号：默认打开，并带上 defaulted 标记。
        assert!(store.apply_plan_default(101, true, ""));
        assert!(store.is_enabled(101));
        assert!(store.snapshot().get(&101).unwrap().defaulted);
        // 状态已经是目标值：不再重复改动 / 落盘。
        assert!(!store.apply_plan_default(101, true, ""));

        // 用户手动关掉（时间戳 = now_ms）：规则不会把它翻回来，
        // 但会顺手清掉残留的「套餐默认」来源标记。
        store.set(101, false, "");
        assert!(store.apply_plan_default(101, true, ""));
        assert!(!store.is_enabled(101));
        let manual_off = store.snapshot().get(&101).cloned().unwrap();
        assert!(!manual_off.enabled && !manual_off.defaulted);
        // 标记清掉之后再跑一次：状态已经是目标值，不再改动。
        assert!(!store.apply_plan_default(101, true, ""));

        // 用户手动开启的号：BPS 总开关关掉时也不会被规则收回。
        store.set(102, true, "");
        assert!(!store.apply_plan_default(102, false, ""));
        assert!(store.is_enabled(102));

        // 规则自己开的号：规则不再适用时自动收回，且不再保留 defaulted。
        assert!(store.apply_plan_default(103, true, ""));
        assert!(store.is_enabled(103));
        assert!(store.apply_plan_default(103, false, ""));
        assert!(!store.is_enabled(103));
        let state = store.snapshot().get(&103).cloned().unwrap();
        assert!(!state.enabled && !state.defaulted);
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
