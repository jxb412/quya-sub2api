//! 内嵌管理面板：一个极简、零额外依赖的 HTTP/1.1 服务，展示养池状态、
//! 手动触发 canary 重铸。仅在 `panel_addr` 配置后监听，且强制 token 鉴权。
//!
//! 安全：面板会暴露 turn-state 池状态（不含 bearer）。务必绑内网/回环并配强 token。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::service::SharedState;

/// 在独立线程启动面板：轮询配置，等 panel_addr 就绪后绑定并 serve。
pub fn spawn(state: Arc<SharedState>, handle: Handle) {
    std::thread::spawn(move || {
        // 等待配置里出现 panel_addr（ApplyConfig 之后）。
        let listener = loop {
            let config = state.current_config();
            let addr = config.panel_addr.trim().to_string();
            if !addr.is_empty() && !config.panel_token.trim().is_empty() {
                match TcpListener::bind(&addr) {
                    Ok(listener) => {
                        eprintln!("[codex-native-transport] panel listening on {addr}");
                        break listener;
                    }
                    Err(err) => {
                        eprintln!("[codex-native-transport] panel bind {addr} failed: {err}");
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(5));
        };
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let state = Arc::clone(&state);
                    let handle = handle.clone();
                    std::thread::spawn(move || {
                        let _ = handle_conn(stream, &state, &handle);
                    });
                }
                Err(_) => continue,
            }
        }
    });
}

struct Request {
    method: String,
    path: String,
    query: String,
    token: Option<String>,
    body: String,
}

fn handle_conn(
    mut stream: TcpStream,
    state: &Arc<SharedState>,
    handle: &Handle,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let req = match parse_request(&mut stream)? {
        Some(req) => req,
        None => return Ok(()),
    };

    let config = state.current_config();
    let want = config.panel_token.trim();
    if want.is_empty() {
        return write_response(&mut stream, 503, "text/plain", b"panel disabled");
    }
    if req.token.as_deref() != Some(want) {
        return write_response(&mut stream, 401, "text/plain", b"unauthorized");
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => write_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        ("GET", "/api/status") => {
            let body = status_json(state);
            write_response(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("POST", "/api/refresh") => {
            let (account, model) = parse_account_model(&req.query);
            match (account, model) {
                (Some(account), Some(model)) => {
                    let report =
                        handle.block_on(crate::refresh::run_canary(state, account, &model));
                    let body = format!("{{\"report\":\"{report:?}\"}}");
                    write_response(&mut stream, 200, "application/json", body.as_bytes())
                }
                _ => write_response(
                    &mut stream,
                    400,
                    "application/json",
                    b"{\"error\":\"account and model required\"}",
                ),
            }
        }
        ("POST", "/api/refresh-all") => {
            let cells = state.pool.export();
            let mut n = 0usize;
            for c in &cells {
                let _ = handle.block_on(crate::refresh::run_canary(state, c.account_id, &c.model));
                n += 1;
            }
            let body = format!("{{\"report\":\"refreshed {n}\"}}");
            write_response(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("POST", "/api/set") => {
            let (account, model) = parse_account_model(&req.query);
            match (account, model) {
                (Some(account), Some(model)) => {
                    let ts = req.body.trim();
                    let value = if ts.is_empty() { None } else { Some(ts) };
                    state.pool.set_turn_state(account, &model, value);
                    let cfg = state.current_config();
                    state.pool.persist_now(&cfg.pin_persist_path);
                    write_response(&mut stream, 200, "application/json", b"{\"ok\":true}")
                }
                _ => write_response(
                    &mut stream,
                    400,
                    "application/json",
                    b"{\"error\":\"account and model required\"}",
                ),
            }
        }
        ("POST", "/api/unpark") => {
            let (account, model) = parse_account_model(&req.query);
            match (account, model) {
                (Some(account), Some(model)) => {
                    let changed = state.pool.clear_parked(account, &model);
                    state.egress.reset_lap(account, &model);
                    let cfg = state.current_config();
                    state.pool.persist_now(&cfg.pin_persist_path);
                    let body = format!("{{\"ok\":true,\"changed\":{changed}}}");
                    write_response(&mut stream, 200, "application/json", body.as_bytes())
                }
                _ => write_response(
                    &mut stream,
                    400,
                    "application/json",
                    b"{\"error\":\"account and model required\"}",
                ),
            }
        }
        ("POST", "/api/unpark-all") => {
            let n = state.pool.clear_parked_all();
            for c in state.pool.export() {
                state.egress.reset_lap(c.account_id, &c.model);
            }
            let cfg = state.current_config();
            state.pool.persist_now(&cfg.pin_persist_path);
            let body = format!("{{\"report\":\"unparked {n}\"}}");
            write_response(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("POST", "/api/cut") => {
            let (account, model) = parse_account_model(&req.query);
            match (account, model) {
                (Some(account), Some(model)) => {
                    state.pool.cut(account, &model);
                    let cfg = state.current_config();
                    state.pool.persist_now(&cfg.pin_persist_path);
                    write_response(&mut stream, 200, "application/json", b"{\"ok\":true}")
                }
                _ => write_response(
                    &mut stream,
                    400,
                    "application/json",
                    b"{\"error\":\"account and model required\"}",
                ),
            }
        }
        _ => write_response(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn parse_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    let mut token_hdr: Option<String> = None;
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "authorization" {
                token_hdr = value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
                    .map(str::to_string);
            } else if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
        }
    }
    // 读取 body（保存 turn-state 用；上限 64KB）。
    let mut body = String::new();
    if content_length > 0 {
        let mut buf = vec![0u8; content_length.min(64 * 1024)];
        if reader.read_exact(&mut buf).is_ok() {
            body = String::from_utf8_lossy(&buf).into_owned();
        }
    }

    let token = token_hdr.or_else(|| query_get(&query, "token"));
    Ok(Some(Request {
        method,
        path,
        query,
        token,
        body,
    }))
}

fn parse_account_model(query: &str) -> (Option<i64>, Option<String>) {
    let account = query_get(query, "account").and_then(|v| v.parse::<i64>().ok());
    let model = query_get(query, "model");
    (account, model)
}

fn query_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 读取账号 id→邮箱标签映射：位于池文件同目录下的 account-labels.json。
/// 格式：{"17":"a@example.com","20":"b@example.com"}。缺文件/解析失败返回空表。
fn load_labels(pin_persist_path: &str) -> std::collections::HashMap<String, String> {
    let p = pin_persist_path.trim();
    if p.is_empty() {
        return Default::default();
    }
    let Some(dir) = std::path::Path::new(p).parent() else {
        return Default::default();
    };
    let file = dir.join("account-labels.json");
    match std::fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Default::default(),
    }
}

fn status_json(state: &Arc<SharedState>) -> String {
    use std::collections::BTreeMap;
    let config = state.current_config();
    let params = crate::turn_state::PinParams::from_config(
        config.pin_max_age_seconds,
        config.pin_fail_threshold,
        config.pin_fail_ratio_pct,
    );
    let now = crate::turn_state::now_ms() as u64;
    let egress_pool = config.egress_pool_list();
    // 「池大小」= 一圈长度 = 池条目数（每行一个独立出口）。
    let egress_pool_size = config.egress_lap_len();
    // 账号 id → 邮箱标签（可选）：从池目录下的 account-labels.json 读取，
    // 由宿主侧用数据库生成；缺文件则只显示 #id。每次读取，改文件即时生效。
    let labels = load_labels(&config.pin_persist_path);
    // 只展示需要铸票的养池模型（warming_models）。其它模型的格来自真实用户流量，
    // 不参与养池管理，展示出来只会造成“为什么有的号 5 个有的 7/9 个”的困惑，这里过滤掉。
    let warming: std::collections::HashSet<String> =
        config.warming_models_list().into_iter().collect();
    // 按账号分组，账号内按模型名排序。
    let mut by_acct: BTreeMap<i64, Vec<crate::turn_state::Cell>> = BTreeMap::new();
    for c in state.pool.export() {
        if !warming.contains(&c.model) {
            continue;
        }
        by_acct.entry(c.account_id).or_default().push(c);
    }
    let mut accts = Vec::with_capacity(by_acct.len());
    for (id, mut models) in by_acct {
        models.sort_by(|a, b| a.model.cmp(&b.model));
        let mut mrows = Vec::with_capacity(models.len());
        for c in &models {
            let locked = state
                .pool
                .injectable(c.account_id, &c.model, &params)
                .is_some();
            let age_s = if c.pinned_at_ms > 0 {
                now.saturating_sub(c.pinned_at_ms) / 1000
            } else {
                0
            };
            let state_label = if c.degraded {
                "degraded"
            } else if c.failed {
                "failed"
            } else {
                "ok"
            };
            let ts = c.turn_state.as_deref().unwrap_or("");
            let ticket_len = ts.len();
            // 当前该格用代理池里第几个（仅在池非空且该格还没锁到 292/332 时有意义；否则 -1）。
            let egress_idx: i64 = if egress_pool_size > 0 && !locked {
                state
                    .egress
                    .current_index(c.account_id, &c.model, egress_pool_size) as i64
            } else {
                -1
            };
            // 当前该格实际走的出口地址（host:port，隐去账号密码），方便直接看到是哪个 IP。
            let egress_host = if egress_idx >= 0 {
                egress_pool
                    .get(egress_idx as usize)
                    .map(|u| proxy_host(u))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            // 放弃铸票（直通）状态与出口池本圈是否已转满：休息编排的两个关键信号。
            let abandoned = c.is_abandoned(now);
            let abandoned_s = if abandoned {
                c.abandoned_until_ms.saturating_sub(now) / 1000
            } else {
                0
            };
            let lap_exhausted = egress_pool_size > 0
                && !locked
                && state.egress.lap_exhausted(c.account_id, &c.model);
            let cell_rest_s = state.pool.rest_remaining_s(c.account_id, &c.model, now);
            mrows.push(format!(
                "{{\"model\":{},\"turn_state\":{},\"state\":\"{}\",\"locked\":{},\"ok\":{},\"ov\":{},\"deg\":{},\"err\":{},\"tps\":{:.2},\"age_s\":{},\"fail_streak\":{},\"minted_at_s\":{},\"ticket_len\":{},\"last_seen_len\":{},\"egress_idx\":{},\"egress_host\":{},\"abandoned\":{},\"abandoned_s\":{},\"stuck_rounds\":{},\"lap_exhausted\":{},\"resting\":{},\"rest_s\":{}}}",
                json_string(&c.model),
                json_string(ts),
                state_label,
                locked,
                c.ok_count,
                c.ov_count,
                c.deg_count,
                c.err_count,
                c.last_tps,
                age_s,
                c.fail_streak,
                c.minted_at_s.unwrap_or(0),
                ticket_len,
                c.last_seen_ticket_len,
                egress_idx,
                json_string(&egress_host),
                abandoned,
                abandoned_s,
                c.stuck_rounds,
                lap_exhausted,
                cell_rest_s.is_some(),
                cell_rest_s.unwrap_or(0),
            ));
        }
        let email = labels
            .get(&id.to_string())
            .map(String::as_str)
            .unwrap_or("");
        let rest_s = state.acct_rest.rest_remaining_s(id, now);
        let orig_priority = state.acct_rest.orig_priority(id);
        accts.push(format!(
            "{{\"id\":{},\"email\":{},\"resting\":{},\"rest_s\":{},\"orig_priority\":{},\"models\":[{}]}}",
            id,
            json_string(email),
            rest_s.is_some(),
            rest_s.unwrap_or(0),
            orig_priority
                .map(|p| p.to_string())
                .unwrap_or_else(|| "null".to_string()),
            mrows.join(",")
        ));
    }
    format!(
        "{{\"mode\":{},\"strategy\":{},\"canary_enabled\":{},\"active_warming\":{},\"passive_warming\":{},\"warm_interval_s\":{},\"admin_warming\":{},\"admin_warm_interval_s\":{},\"warming_models\":{},\"warming_model_names\":{},\"rest_s_cfg\":{},\"drain_priority\":{},\"giveup_rounds\":{},\"max_age_s\":{},\"egress_pool_size\":{},\"accounts\":[{}]}}",
        json_string(&config.turn_state_mode),
        json_string(&config.pin_identity_strategy),
        config.canary_enabled,
        config.active_warming_enabled,
        config.passive_warming_enabled,
        config.active_warming_interval_seconds,
        config.admin_warming_enabled,
        config.admin_warming_interval_seconds,
        config.warming_models_list().len(),
        json_string(&config.warming_models_list().join(", ")),
        config.warming_rest_seconds,
        config.warming_drain_priority,
        config.pin_giveup_rounds,
        config.pin_max_age_seconds,
        egress_pool_size,
        accts.join(",")
    )
}

/// 从代理 URL 抽取 host:port，隐去 scheme 与账号密码。
/// 例: "socks5://user:pass@1.2.3.4:7523" -> "1.2.3.4:7523"。
fn proxy_host(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after_scheme
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(after_scheme);
    host.trim_end_matches('/').to_string()
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>账号 Turn-State · codex-native-transport</title>
<style>
  :root { color-scheme: light; }
  * { box-sizing: border-box; }
  body { font: 14px/1.55 -apple-system, "PingFang SC", system-ui, sans-serif; margin:0; background:#f4f5f7; color:#1f2430; }
  .wrap { max-width: 860px; margin: 0 auto; padding: 20px 16px 60px; }
  .top { display:flex; align-items:center; gap:12px; }
  .top h1 { font-size:19px; margin:0; font-weight:700; flex:1; }
  .sub { color:#6b7280; font-size:12.5px; margin:10px 0 18px; }
  .meta { color:#8a93a6; font-size:12px; margin:2px 0 16px; }
  .card { background:#fff; border:1px solid #e5e7eb; border-radius:12px; padding:14px 16px; margin-bottom:14px; box-shadow:0 1px 2px rgba(16,24,40,.04); }
  .card-head { display:flex; align-items:center; cursor:pointer; user-select:none; }
  .card-head .acct { font-weight:700; font-size:15px; }
  .card-head .email { color:#3b82f6; font-size:12.5px; margin-left:8px; word-break:break-all; }
  .card-head .rest { color:#b45309; background:#fff7ed; border:1px solid #fed7aa; border-radius:6px; padding:1px 7px; font-size:12px; margin-left:8px; }
  .card-head .count { color:#9aa4b6; font-size:12.5px; margin-left:auto; }
  .card-body.hidden { display:none; }
  .model { padding:14px 0; border-top:1px dashed #eceef2; }
  .model:first-child { border-top:0; padding-top:12px; }
  .model-head { display:flex; align-items:baseline; gap:10px; flex-wrap:wrap; margin-bottom:8px; }
  .mname { font-weight:600; }
  .status { margin-left:auto; font-size:12.5px; font-weight:600; }
  .status.ok { color:#12a150; }
  .status.bad { color:#e5484d; }
  textarea { width:100%; min-height:72px; resize:vertical; font:12px/1.5 ui-monospace, Menlo, Consolas, monospace;
    color:#374151; background:#f9fafb; border:1px solid #e5e7eb; border-radius:8px; padding:8px 10px; word-break:break-all; }
  textarea::placeholder { color:#b6bcc7; }
  .btns { display:flex; gap:8px; margin-top:8px; }
  button { border:1px solid #e5e7eb; background:#fff; color:#374151; border-radius:8px; padding:6px 14px; cursor:pointer; font-size:13px; }
  button:hover { background:#f3f4f6; }
  button.primary { background:#2b6cff; border-color:#2b6cff; color:#fff; }
  button.primary:hover { background:#1f5cf0; }
  button.top-btn { padding:6px 14px; }
  button:disabled { opacity:.6; cursor:default; }
  .muted { color:#9aa4b6; padding:20px 0; text-align:center; }
  #err { color:#e5484d; font-size:12.5px; margin-bottom:10px; }
</style>
</head>
<body>
<div class="wrap">
  <div class="top">
    <h1>账号 Turn-State</h1>
    <button class="top-btn" id="refresh-all">全部刷新</button>
    <button class="top-btn" id="unpark-all">全部解除放弃/休息</button>
    <button class="top-btn" id="collapse">收起</button>
  </div>
  <div class="sub">每个账号独立铸自己的 Turn-State，禁止把别的号的 ID 抄过来。刷新时临时只开本号。收到 292/332 字节有效票自动锁定。切片=丢掉本号旧分片再重铸。TPS≥100 或 gpt-6 无 reasoning 当降智。</div>
  <div class="meta" id="meta">加载中…</div>
  <div id="err"></div>
  <div id="accts"></div>
</div>
<script>
const token = new URLSearchParams(location.search).get('token') || '';
function api(p,o){ o=o||{}; o.headers=Object.assign({'Authorization':'Bearer '+token}, o.headers||{}); return fetch(p,o); }
function esc(s){ return (s||'').replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c])); }
let collapsed=false, editing=false;
async function load(){
  if(editing) return;
  try{
    const r=await api('/api/status'); if(!r.ok) throw new Error('HTTP '+r.status);
    const d=await r.json();
    document.getElementById('err').textContent='';
    const poolTxt = (d.egress_pool_size>0) ? ` · 代理池 ${d.egress_pool_size} 个出口` : '';
    const warmTxt = ` · 主动养池 ${d.active_warming?('on/'+d.warm_interval_s+'s'):'off'} · 被动养池 ${d.passive_warming?'on':'off'}`;
    const adminTxt = d.admin_warming?(` · 全池养池 on/${d.admin_warm_interval_s}s`):' · 全池养池 off';
    const modelsTxt = ` · 关注模型 [${esc(d.warming_model_names||'')}]`;
    const restTxt = (d.rest_s_cfg>0) ? ` · 休息 ${d.rest_s_cfg}s/排空优先级 ${d.drain_priority}/放弃 ${d.giveup_rounds}轮` : ' · 休息 off';
    document.getElementById('meta').textContent=`模式 ${d.mode} · 策略 ${d.strategy}${modelsTxt} · canary ${d.canary_enabled?'on':'off'}${warmTxt}${adminTxt}${restTxt} · TTL ${d.max_age_s}s${poolTxt}`;
    const box=document.getElementById('accts'); box.innerHTML='';
    if(!d.accounts||!d.accounts.length){ box.innerHTML='<div class="muted">暂无养池格（尚无 codex 流量经过）</div>'; return; }
    for(const a of d.accounts){ box.appendChild(renderAcct(a)); }
  }catch(ex){ document.getElementById('err').textContent='加载失败: '+ex.message+'（检查 URL 里的 ?token=）'; }
}
function renderAcct(a){
  const card=document.createElement('section'); card.className='card';
  const head=document.createElement('div'); head.className='card-head';
  const restBadge=a.resting?`<span class="rest">休息中(优先级排空${a.orig_priority!=null?'，原优先级 '+a.orig_priority:''}) ${a.rest_s||0}s</span>`:'';
  head.innerHTML=`<span class="acct">#${a.id}</span>${a.email?`<span class="email">${esc(a.email)}</span>`:''}${restBadge}<span class="count">${a.models.length} 个模型</span>`;
  const body=document.createElement('div'); body.className='card-body'+(collapsed?' hidden':'');
  head.onclick=()=>body.classList.toggle('hidden');
  for(const m of a.models){ body.appendChild(renderModel(a.id,m)); }
  card.appendChild(head); card.appendChild(body); return card;
}
function renderModel(acct,m){
  const w=document.createElement('div'); w.className='model';
  const ok=m.state==='ok';
  const label = m.abandoned?('已放弃铸票·直通 '+(m.abandoned_s||0)+'s后重试'):(m.resting?('格休息中·直通 '+(m.rest_s||0)+'s'):(ok?'可用':(m.state==='degraded'?'降智':'失效')));
  const lock = m.locked?'锁定':(ok?'空':'fail');
  const tps=(m.tps||0).toFixed(2);
  const tl=m.ticket_len||0, seen=m.last_seen_len||0;
  // 票长徽章：锁住时显示锁定的票长；否则显示上次看到的票长。
  const shown = m.locked ? tl : seen;
  const ticket = shown ? ('票'+shown) : '票—';
  const egress = (typeof m.egress_idx==='number' && m.egress_idx>=0 && !m.abandoned && !m.resting) ? ` · 出口#${m.egress_idx}${m.egress_host?' '+esc(m.egress_host):''}${m.lap_exhausted?' · 本圈已转满':''}` : '';
  const stuck = (m.stuck_rounds>0) ? ` · 卡住${m.stuck_rounds}轮` : '';
  w.innerHTML=`
    <div class="model-head">
      <span class="mname">X-Codex-Turn-State · ${esc(m.model)}</span>
      <span class="status ${(ok&&!m.abandoned&&!m.resting)?'ok':'bad'}">${label} · ok ${m.ok} / ov ${m.ov} · TPS ${tps} · ${lock} · ${ticket}${egress}${stuck}</span>
    </div>
    <textarea placeholder="gAAAAAB… 空=不注入">${esc(m.turn_state)}</textarea>
    <div class="btns">
      <button class="act-refresh">刷新</button>
      ${(m.abandoned||m.resting||m.stuck_rounds>0)?'<button class="act-unpark">解除放弃/休息</button>':''}
      <button class="act-cut">切片</button>
      <button class="primary act-save">保存</button>
    </div>`;
  const ta=w.querySelector('textarea');
  ta.addEventListener('focus',()=>editing=true);
  ta.addEventListener('blur',()=>{editing=false;});
  const q=`account=${acct}&model=${encodeURIComponent(m.model)}`;
  w.querySelector('.act-refresh').onclick=e=>act(e.target,'/api/refresh?'+q);
  w.querySelector('.act-cut').onclick=e=>act(e.target,'/api/cut?'+q);
  const up=w.querySelector('.act-unpark'); if(up) up.onclick=e=>act(e.target,'/api/unpark?'+q);
  w.querySelector('.act-save').onclick=e=>act(e.target,'/api/set?'+q, ta.value);
  return w;
}
async function act(btn,url,body){
  const t=btn.textContent; btn.disabled=true; btn.textContent='…';
  try{ const r=await api(url,{method:'POST',body:body}); const j=await r.json().catch(()=>({})); btn.textContent=j.report||(j.ok?'ok':'done'); }
  catch(ex){ btn.textContent='err'; }
  editing=false;
  setTimeout(()=>{ btn.textContent=t; btn.disabled=false; load(); }, 700);
}
document.getElementById('refresh-all').onclick=e=>act(e.target,'/api/refresh-all');
document.getElementById('unpark-all').onclick=e=>act(e.target,'/api/unpark-all');
document.getElementById('collapse').onclick=()=>{
  collapsed=!collapsed;
  document.querySelectorAll('.card-body').forEach(b=>b.classList.toggle('hidden',collapsed));
  document.getElementById('collapse').textContent=collapsed?'展开':'收起';
};
load(); setInterval(load, 15000);
</script>
</body>
</html>"##;
