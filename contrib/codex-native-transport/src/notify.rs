//! Telegram 智商切换通知：消费池投出的 TierEvent（degraded 档位翻转），
//! 直连 api.telegram.org 推送。best-effort——发送失败/网络不通静默丢弃，
//! 绝不阻塞转发与养池。
//!
//! 两种投递模式：
//! - immediate（默认）：每条事件即时推，同一 (账号×模型) 带冷却防抖；
//! - digest：按窗口把多条事件汇总成一条摘要推。
//!
//! 文案不提“票”，账号邮箱以第一个 @ 切割、@ 及之后打码（aaa@bb.com → aaa*******）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::service::SharedState;
use crate::turn_state::{now_ms, Tier, TierEvent};

/// 巡检节拍：用于 digest 到窗触发（比窗口细，保证及时）。
const TICK_SECONDS: u64 = 15;
/// digest 缓冲上限（超出丢最旧，防无界增长）。
const DIGEST_CAP: usize = 200;
/// digest 模式下 cooldown=0 时的兜底窗口（秒）。
const DIGEST_DEFAULT_WINDOW_S: u64 = 300;

/// 后台通知循环：从池的事件 channel 收 TierEvent，按配置过滤/防抖/汇总后推送。
pub async fn notify_loop(
    state: Arc<SharedState>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<TierEvent>,
) {
    // 直连客户端（不过任何代理，短超时）。构建失败则退化为默认客户端。
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let mut last_sent: HashMap<(i64, String), u64> = HashMap::new();
    let mut digest: Vec<TierEvent> = Vec::new();
    let mut last_flush = now_ms() as u64;
    let mut ticker = tokio::time::interval(Duration::from_secs(TICK_SECONDS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(ev) = maybe else {
                    break; // 发送端全部释放（不会发生，池与 state 同生命周期）。
                };
                let cfg = state.current_config();
                if !cfg.tg_notify_enabled() {
                    continue;
                }
                if !should_notify(&cfg.tg_notify_events, ev.from, ev.to) {
                    continue;
                }
                if cfg.tg_notify_mode == "digest" {
                    digest.push(ev);
                    if digest.len() > DIGEST_CAP {
                        digest.remove(0);
                    }
                    continue;
                }
                // immediate：同格冷却防抖。
                let cd_ms = cfg.tg_notify_cooldown_seconds as u64 * 1000;
                let key = (ev.account_id, ev.model.clone());
                let now = now_ms() as u64;
                if cd_ms > 0 {
                    if let Some(&t) = last_sent.get(&key) {
                        if now.saturating_sub(t) < cd_ms {
                            continue;
                        }
                    }
                }
                last_sent.insert(key, now);
                let text = format_immediate(&state, &cfg, &ev);
                send_telegram(&client, &cfg, &text).await;
            }
            _ = ticker.tick() => {
                if digest.is_empty() {
                    continue;
                }
                let cfg = state.current_config();
                let now = now_ms() as u64;
                let window_ms = digest_window_s(&cfg) * 1000;
                // 到窗、或已切出 digest 模式（把残留一次性清出去）即冲刷。
                let due = cfg.tg_notify_mode != "digest"
                    || now.saturating_sub(last_flush) >= window_ms;
                if !due {
                    continue;
                }
                last_flush = now;
                if cfg.tg_notify_enabled() {
                    let text = format_digest(&state, &cfg, &digest, digest_window_s(&cfg));
                    send_telegram(&client, &cfg, &text).await;
                }
                digest.clear();
            }
        }
    }
}

fn digest_window_s(cfg: &crate::config::PluginConfig) -> u64 {
    if cfg.tg_notify_cooldown_seconds == 0 {
        DIGEST_DEFAULT_WINDOW_S
    } else {
        cfg.tg_notify_cooldown_seconds as u64
    }
}

/// 按事件过滤配置判断是否推送。
fn should_notify(events: &str, from: Tier, to: Tier) -> bool {
    match events {
        // 放弃铸票与掉智同级：都是「这格现在拿不到真6」的信号。
        "degrade_only" => to == Tier::Degraded || to == Tier::Abandoned,
        "all" => true, // 任何被投出的档位跃迁（from!=to 已在池侧保证）。
        // degrade_recover（默认）：掉智/放弃 + 回真6。
        _ => {
            to == Tier::Degraded
                || to == Tier::Abandoned
                || ((from == Tier::Degraded || from == Tier::Abandoned) && to == Tier::Ok)
        }
    }
}

/// 跃迁 → 标题（含图标）。
fn title(from: Tier, to: Tier) -> &'static str {
    match (from, to) {
        (_, Tier::Abandoned) => "⚫ 放弃铸票(退回直通)",
        (Tier::Abandoned, Tier::Ok) => "🟢 重新锁到真6",
        (_, Tier::Degraded) => "🔴 掉智商",
        (Tier::Degraded, Tier::Ok) => "🟢 回真6",
        (Tier::Overloaded, Tier::Ok) => "🟢 恢复可用",
        (_, Tier::Overloaded) => "🟡 过载失效",
        _ => "ℹ️ 状态变化",
    }
}

/// digest 用的短标记。
fn short_mark(from: Tier, to: Tier) -> &'static str {
    match (from, to) {
        (_, Tier::Abandoned) => "⚫",
        (Tier::Abandoned, Tier::Ok) => "🟢",
        (_, Tier::Degraded) => "🔴",
        (Tier::Degraded, Tier::Ok) => "🟢",
        (Tier::Overloaded, Tier::Ok) => "🟢",
        (_, Tier::Overloaded) => "🟡",
        _ => "ℹ️",
    }
}

fn format_immediate(
    state: &Arc<SharedState>,
    cfg: &crate::config::PluginConfig,
    ev: &TierEvent,
) -> String {
    let acct = acct_display(state, cfg, ev.account_id);
    let mut s = format!(
        "{}  账号 {}\n模型  {}\n",
        title(ev.from, ev.to),
        acct,
        ev.model
    );
    if let Some(eg) = egress_line(state, cfg, ev.account_id, &ev.model) {
        s.push_str(&format!("出口  {eg}\n"));
    }
    if let Some(cell) = state.pool.get_cell(ev.account_id, &ev.model) {
        s.push_str(&format!(
            "近况  ok {} · ov {} · TPS {:.2}\n",
            cell.ok_count, cell.ov_count, cell.last_tps
        ));
    }
    s.push_str(&format!("时间  {} (UTC+8)", fmt_time_utc8(ev.ts_ms)));
    s
}

fn format_digest(
    state: &Arc<SharedState>,
    cfg: &crate::config::PluginConfig,
    events: &[TierEvent],
    window_s: u64,
) -> String {
    let deg = events.iter().filter(|e| e.to == Tier::Degraded).count();
    let rec = events
        .iter()
        .filter(|e| e.from == Tier::Degraded && e.to == Tier::Ok)
        .count();
    let mins = window_s / 60;
    let mut s = format!("📊 养池智商变更摘要 · 近 {mins} 分钟\n🔴 掉智 {deg}   🟢 回真6 {rec}\n\n");
    let show = events.len().min(20);
    for ev in events.iter().take(show) {
        let acct = acct_display(state, cfg, ev.account_id);
        let eg = egress_line(state, cfg, ev.account_id, &ev.model)
            .map(|e| format!("  ({})", e.split_whitespace().next().unwrap_or("")))
            .unwrap_or_default();
        s.push_str(&format!(
            "{} {}  {}{}\n",
            short_mark(ev.from, ev.to),
            acct,
            ev.model,
            eg
        ));
    }
    if events.len() > show {
        s.push_str(&format!("… 共 {} 条\n", events.len()));
    }
    s.push_str(&format!("时间 {} (UTC+8)", fmt_time_utc8(now_ms() as u64)));
    s
}

/// 账号展示：`#id · 打码邮箱`（无标签则只 `#id`）。
fn acct_display(state: &Arc<SharedState>, cfg: &crate::config::PluginConfig, id: i64) -> String {
    let labels = load_labels(&cfg.pin_persist_path);
    match labels.get(&id.to_string()) {
        Some(email) if !email.trim().is_empty() => format!("#{id} · {}", mask_email(email)),
        _ => {
            let _ = state; // 保留签名一致
            format!("#{id}")
        }
    }
}

/// 邮箱打码：以第一个 @ 切割，保留本地部分，@ 及之后全替换为等长的 *。
/// 例：aaa@bb.com → aaa*******（@bb.com 共 7 字符 → 7 个 *）。无 @ 原样返回。
fn mask_email(email: &str) -> String {
    let e = email.trim();
    match e.find('@') {
        Some(pos) => {
            let local = &e[..pos];
            let rest_len = e[pos..].chars().count();
            format!("{local}{}", "*".repeat(rest_len))
        }
        None => e.to_string(),
    }
}

/// 该格当前出口行：`#idx · host:port`（出口池为空则 None）。
fn egress_line(
    state: &Arc<SharedState>,
    cfg: &crate::config::PluginConfig,
    account_id: i64,
    model: &str,
) -> Option<String> {
    let pool = cfg.effective_egress_pool_list();
    if pool.is_empty() {
        return None;
    }
    let idx = state.egress.current_index(account_id, model, pool.len());
    let host = pool.get(idx).map(|u| proxy_host(u)).unwrap_or_default();
    Some(format!("#{idx} · {host}"))
}

/// 从代理 URL 抽取 host:port，隐去 scheme 与账号密码。
fn proxy_host(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after_scheme
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(after_scheme);
    host.trim_end_matches('/').to_string()
}

/// 读账号 id→邮箱标签（与面板同源：池目录下 account-labels.json）。
fn load_labels(pin_persist_path: &str) -> HashMap<String, String> {
    let p = pin_persist_path.trim();
    if p.is_empty() {
        return HashMap::new();
    }
    let Some(dir) = std::path::Path::new(p).parent() else {
        return HashMap::new();
    };
    match std::fs::read(dir.join("account-labels.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// 直连 Telegram 推送一条纯文本消息（best-effort）。
async fn send_telegram(client: &reqwest::Client, cfg: &crate::config::PluginConfig, text: &str) {
    let token = cfg.tg_bot_token.trim();
    let chat = cfg.tg_chat_id.trim();
    if token.is_empty() || chat.is_empty() {
        return;
    }
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    // chat_id 支持数字 id 或 @频道名：能解析成整数就发数字，否则发字符串。
    let chat_val: serde_json::Value = match chat.parse::<i64>() {
        Ok(n) => serde_json::Value::from(n),
        Err(_) => serde_json::Value::from(chat),
    };
    let body = serde_json::json!({
        "chat_id": chat_val,
        "text": text,
        "disable_web_page_preview": true,
    });
    match client.post(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            eprintln!(
                "[codex-native-transport] tg notify: status {}",
                resp.status().as_u16()
            );
        }
        Err(e) => {
            eprintln!("[codex-native-transport] tg notify: send failed: {e}");
        }
    }
}

/// unix 毫秒 → UTC+8 的 "YYYY-MM-DD HH:MM:SS"。无外部依赖，手算民用日历。
fn fmt_time_utc8(ts_ms: u64) -> String {
    let secs = ts_ms / 1000 + 8 * 3600; // 平移到 UTC+8
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Howard Hinnant 的 civil_from_days：unix epoch 起的天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_email_masks_from_at() {
        assert_eq!(mask_email("aaa@bb.com"), "aaa*******");
        assert_eq!(mask_email("a@b"), "a**");
        // 无 @ 原样。
        assert_eq!(mask_email("plainlabel"), "plainlabel");
        // 前后空白裁剪。
        assert_eq!(mask_email("  x@y.z  "), "x****");
    }

    #[test]
    fn should_notify_filters() {
        // 仅掉智。
        assert!(should_notify("degrade_only", Tier::Ok, Tier::Degraded));
        assert!(should_notify("degrade_only", Tier::Ok, Tier::Abandoned));
        assert!(should_notify("degrade_recover", Tier::Abandoned, Tier::Ok));
        assert_eq!(title(Tier::Ok, Tier::Abandoned), "⚫ 放弃铸票(退回直通)");
        assert!(!should_notify("degrade_only", Tier::Degraded, Tier::Ok));
        // 掉智+回真6。
        assert!(should_notify("degrade_recover", Tier::Ok, Tier::Degraded));
        assert!(should_notify("degrade_recover", Tier::Degraded, Tier::Ok));
        assert!(!should_notify(
            "degrade_recover",
            Tier::Overloaded,
            Tier::Ok
        ));
        // all：任何跃迁。
        assert!(should_notify("all", Tier::Overloaded, Tier::Ok));
        assert!(should_notify("all", Tier::Ok, Tier::Overloaded));
    }

    #[test]
    fn titles_map_transitions() {
        assert_eq!(title(Tier::Ok, Tier::Degraded), "🔴 掉智商");
        assert_eq!(title(Tier::Degraded, Tier::Ok), "🟢 回真6");
        assert_eq!(title(Tier::Ok, Tier::Overloaded), "🟡 过载失效");
        assert_eq!(title(Tier::Overloaded, Tier::Ok), "🟢 恢复可用");
    }

    #[test]
    fn proxy_host_strips_scheme_and_creds() {
        assert_eq!(
            proxy_host("socks5h://user:pass@1.2.3.4:1080"),
            "1.2.3.4:1080"
        );
        assert_eq!(proxy_host("http://5.6.7.8:8080/"), "5.6.7.8:8080");
    }

    #[test]
    fn fmt_time_utc8_known_epoch() {
        // 2021-01-01 00:00:00 UTC = 1609459200s → UTC+8 08:00:00。
        assert_eq!(fmt_time_utc8(1_609_459_200_000), "2021-01-01 08:00:00");
    }
}
