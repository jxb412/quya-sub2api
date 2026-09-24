//! 内嵌管理面板：极简、零额外依赖的 HTTP/1.1 服务，提供账号智力巡检页，
//! 以及每账号「自动降智处理」开关。仅在 `panel_addr` 配置后监听，强制 token 鉴权。
//!
//! 安全：面板会暴露账号列表与巡检结论（**不含 bearer**）。绑回环最安全；
//! 若确需外网访问，必须使用强随机 token。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::config::PluginConfig;
use crate::service::SharedState;

/// 在独立线程启动面板：轮询配置，等 panel_addr + panel_token 就绪后绑定并 serve。
pub fn spawn(state: Arc<SharedState>, handle: Handle) {
    std::thread::spawn(move || {
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
}

fn handle_conn(
    mut stream: TcpStream,
    state: &Arc<SharedState>,
    handle: &Handle,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let req = match parse_request(&mut stream)? {
        Some(req) => req,
        None => return Ok(()),
    };

    let config = state.current_config();
    let want = config.panel_token.trim().to_string();
    if want.is_empty() {
        return write_response(&mut stream, 503, "text/plain", b"panel disabled");
    }
    if req.token.as_deref() != Some(want.as_str()) {
        return write_response(&mut stream, 401, "text/plain", b"unauthorized");
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => write_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        ("GET", "/accounts") => write_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            ACCOUNTS_HTML.as_bytes(),
        ),
        ("GET", "/api/status") => {
            let body = status_json(state);
            write_response(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("GET", "/api/intel/accounts") => {
            let body = intel_accounts_json(state, handle);
            write_response(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("POST", "/api/intel/probe") => {
            let ids = parse_id_list(&req.query);
            let cfg = state.current_config();
            let body = if !cfg.intel_enabled {
                json_error("智力巡检未开启（插件配置里打开 intel_enabled）")
            } else if state.intel.is_running() {
                json_error("已有巡检在进行中，请稍候")
            } else {
                let spawned = Arc::clone(state);
                let only = if ids.is_empty() { None } else { Some(ids) };
                handle.spawn(async move {
                    if let Err(err) = crate::intel::run_sweep(&spawned, &cfg, only).await {
                        eprintln!("[codex-native-transport] intel sweep failed: {err}");
                    }
                });
                "{\"started\":true}".to_string()
            };
            write_response(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("POST", "/api/intel/set-schedulable") => {
            let account_id = query_get(&req.query, "id").and_then(|v| v.parse::<i64>().ok());
            let want = matches!(
                query_get(&req.query, "schedulable").as_deref(),
                Some("1") | Some("true")
            );
            match account_id {
                Some(account_id) => {
                    let cfg = state.current_config();
                    let base = cfg.admin_api_base.trim().to_string();
                    let key = cfg.admin_api_key.trim().to_string();
                    let outcome = handle.block_on(crate::admin::set_account_schedulable(
                        &base, &key, account_id, want,
                    ));
                    let body = match outcome {
                        Ok(()) => "{\"ok\":true}".to_string(),
                        Err(err) => format!("{{\"ok\":false,\"error\":{}}}", json_string(&err)),
                    };
                    write_response(&mut stream, 200, "application/json", body.as_bytes())
                }
                None => write_response(
                    &mut stream,
                    400,
                    "application/json",
                    b"{\"error\":\"id required\"}",
                ),
            }
        }
        ("POST", "/api/degrade") => {
            let account_id = query_get(&req.query, "id").and_then(|v| v.parse::<i64>().ok());
            let enabled = matches!(
                query_get(&req.query, "enabled").as_deref(),
                Some("1") | Some("true")
            );
            match account_id {
                Some(account_id) => {
                    let cfg = state.current_config();
                    state
                        .degrade
                        .set(account_id, enabled, &cfg.degrade_state_file());
                    write_response(&mut stream, 200, "application/json", b"{\"ok\":true}")
                }
                None => write_response(
                    &mut stream,
                    400,
                    "application/json",
                    b"{\"error\":\"id required\"}",
                ),
            }
        }
        ("POST", "/api/bps/check") => {
            let account_id = query_get(&req.query, "id").and_then(|v| v.parse::<i64>().ok());
            match account_id {
                Some(account_id) => {
                    let cfg = state.current_config();
                    let report = handle.block_on(crate::bps::check(&state, &cfg, account_id));
                    write_response(&mut stream, 200, "application/json", report.as_bytes())
                }
                None => write_response(
                    &mut stream,
                    400,
                    "application/json",
                    b"{\"error\":\"id required\"}",
                ),
            }
        }
        ("GET", "/api/channel") => {
            let account_id = query_get(&req.query, "id").and_then(|v| v.parse::<i64>().ok());
            match account_id {
                Some(account_id) => {
                    let cfg = state.current_config();
                    state.degrade.ensure_loaded(&cfg.degrade_state_file());
                    let decision = state
                        .degrade
                        .decision(account_id, &cfg, crate::donor::now_ms());
                    let body = serde_json::to_string(&decision).unwrap_or_default();
                    write_response(&mut stream, 200, "application/json", body.as_bytes())
                }
                None => write_response(
                    &mut stream,
                    400,
                    "application/json",
                    b"{\"error\":\"id required\"}",
                ),
            }
        }
        ("GET", "/api/bps/diagnose") => {
            let body = bps_diagnose_json(state);
            write_response(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("GET", "/api/bps/stream") => {
            let body = crate::bps::last_stream_trace()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "{}".to_string());
            write_response(&mut stream, 200, "application/json", body.as_bytes())
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
        None => (target, String::new()),
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
    if content_length > 0 {
        let mut buf = vec![0u8; content_length.min(64 * 1024)];
        let _ = reader.read_exact(&mut buf);
    }

    let token = token_hdr.or_else(|| query_get(&query, "token"));
    Ok(Some(Request {
        method,
        path,
        query,
        token,
    }))
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

fn parse_id_list(query: &str) -> Vec<i64> {
    query_get(query, "ids")
        .map(|raw| {
            raw.split(',')
                .filter_map(|value| value.trim().parse::<i64>().ok())
                .collect::<Vec<i64>>()
        })
        .unwrap_or_default()
}

fn json_error(message: &str) -> String {
    format!("{{\"started\":false,\"error\":{}}}", json_string(message))
}

fn json_string(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// 套餐原始值 → 人类可读标签。未知值原样显示，不做猜测。
fn plan_label(value: &str) -> String {
    match value {
        "" => "未识别".to_string(),
        "pro" => "Pro".to_string(),
        "plus" => "Plus".to_string(),
        "free" => "Free".to_string(),
        "team" => "Team".to_string(),
        "self_serve_business_prolite" => "Business Premium".to_string(),
        other => other.to_string(),
    }
}

fn status_json(state: &Arc<SharedState>) -> String {
    let config = state.current_config();
    let (_, run) = state.intel.snapshot();
    let donor = state.template.any_recent();
    serde_json::json!({
        "version": crate::service::PLUGIN_VERSION,
        "intel_enabled": config.intel_enabled,
        "intel_loop_enabled": config.intel_loop_enabled,
        "intel_auto_pause": config.intel_auto_pause,
        "bps_enabled": config.bps_enabled,
        "bps_endpoint": config.bps_endpoint,
        "bps_models": config.bps_model_list(),
        "admin_api_base": config.admin_api_base,
        "admin_api_key_configured": !config.admin_api_key.trim().is_empty(),
        "degrade_accounts": state.degrade.snapshot(),
        "channels": channel_summary(state, &config),
        "template_ready": donor.is_some(),
        "template_at_ms": donor.map(|t| t.at_ms).unwrap_or(0),
        "run": run,
    })
    .to_string()
}

/// 面板「账号智力巡检」页的数据源：管理 API 账号 + 上次结论 + 降智处理开关。
/// 每个参与降智处理的账号当前走哪条通道（供 /api/status 展示）。
fn channel_summary(state: &Arc<SharedState>, config: &PluginConfig) -> serde_json::Value {
    let now = crate::donor::now_ms();
    let mut map = serde_json::Map::new();
    for account_id in state.degrade.snapshot().keys() {
        let decision = state.degrade.decision(*account_id, config, now);
        map.insert(
            account_id.to_string(),
            serde_json::to_value(decision).unwrap_or(serde_json::Value::Null),
        );
    }
    serde_json::Value::Object(map)
}

/// BPS 工具桥接诊断：最近一次 BPS 改写看到哪些客户端工具，以及当前流量模板的工具形状。
///
/// 用于排查「模型说它不能执行命令」这类问题：如果 `template.tools.catalog_entries`
/// 明显小于 `declared`，说明客户端的工具形状没被目录收录（模型看不到工具）。
fn bps_diagnose_json(state: &Arc<SharedState>) -> String {
    let template = state.template.any_recent().map(|template| {
        serde_json::json!({
            "at_ms": template.at_ms,
            "url": template.url,
            "header_count": template.headers.len(),
            "body_bytes": template.body.len(),
            "tools": crate::bps::describe_body_tools(&template.body),
        })
    });
    serde_json::json!({
        "last_bps_rewrite": crate::bps::last_rewrite(),
        "template": template,
    })
    .to_string()
}

fn intel_accounts_json(state: &Arc<SharedState>, handle: &Handle) -> String {
    let config = state.current_config();
    state.intel.ensure_loaded(&config.intel_state_path);
    state.degrade.ensure_loaded(&config.degrade_state_file());
    let (results, run) = state.intel.snapshot();
    let degrade = state.degrade.snapshot();
    let base = config.admin_api_base.trim().to_string();
    let key = config.admin_api_key.trim().to_string();
    let master = config.intel_enabled;
    let mut error = String::new();
    let targets = if !master {
        error = "智力巡检总开关未开启（插件配置里打开「开启智力巡检」）".to_string();
        Vec::new()
    } else if base.is_empty() || key.is_empty() {
        error = "未配置 Sub2API 管理 API（admin_api_base / admin_api_key）".to_string();
        Vec::new()
    } else {
        match handle.block_on(crate::admin::fetch_all_targets(&base, &key)) {
            Ok(list) => list,
            Err(err) => {
                error = err;
                Vec::new()
            }
        }
    };
    let accounts = crate::intel::merge_rows(
        &targets,
        &results,
        &degrade,
        &config,
        crate::donor::now_ms(),
    );
    let plans: Vec<serde_json::Value> = crate::intel::plan_type_counts(&targets)
        .into_iter()
        .map(|(value, count)| {
            serde_json::json!({
                "value": value,
                "label": plan_label(&value),
                "count": count,
            })
        })
        .collect();
    serde_json::json!({
        "enabled": config.intel_enabled,
        "loop_enabled": config.intel_loop_enabled,
        "loop_interval_seconds": config.intel_loop_interval_seconds,
        "auto_pause": config.intel_auto_pause,
        "model": crate::intel::intel_model(&config),
        "prompt": crate::intel::intel_question(&config),
        "fail_marker": crate::intel::intel_marker(&config),
        "plan_types": config.intel_plan_types.clone(),
        "concurrency": config.intel_concurrency,
        "intel_prompt_timeout_seconds": config.intel_prompt_timeout_seconds,
        "intel_prompt_retries": config.intel_prompt_retries,
        "intel_timeout_is_failed": config.intel_timeout_is_failed,
        "intel_confirmations": config.intel_confirmations.clamp(1, 10),
        "intel_require_year": config.intel_require_year,
        "bps_hold_after_healthy_seconds": config.bps_hold_after_healthy_seconds,
        "bps_session_sticky_seconds": config.bps_session_sticky_seconds,
        "bps_fallback_cooldown_seconds": config.bps_fallback_cooldown_seconds,
        "bps_enabled": config.bps_enabled,
        "bps_endpoint": config.bps_endpoint,
        "bps_models": config.bps_model_list(),
        "template_ready": state.template.any_recent().is_some(),
        "run": run,
        "error": error,
        "plans": plans,
        "accounts": accounts,
    })
    .to_string()
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            503 => "Service Unavailable",
            _ => "OK",
        },
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Codex Native Transport</title>
<style>
  body { font: 14px/1.6 -apple-system, "PingFang SC", system-ui, sans-serif; margin:0; background:#f4f5f7; color:#1f2430; }
  .wrap { max-width: 860px; margin: 0 auto; padding: 24px 16px 60px; }
  h1 { font-size: 20px; margin: 0 0 4px; }
  .sub { color:#6b7280; font-size: 13px; margin-bottom: 18px; }
  .card { background:#fff; border:1px solid #e5e7eb; border-radius:12px; padding:16px 18px; margin-bottom:14px; }
  .k { color:#6b7280; }
  .v { font-weight:600; }
  a.btn { display:inline-block; margin-top:8px; border:1px solid #2b6cff; background:#2b6cff; color:#fff; text-decoration:none; border-radius:8px; padding:7px 16px; }
  pre { background:#f8fafc; border:1px solid #e5e7eb; border-radius:8px; padding:10px; font-size:12px; overflow:auto; }
</style>
</head>
<body>
<div class="wrap">
  <h1>Codex Native Transport</h1>
  <div class="sub">账号智力巡检 / 降智处理面板</div>
  <div class="card" id="status">加载中…</div>
  <div class="card">
    <a class="btn" id="accounts" href="#">打开智力巡检页</a>
    <a class="btn" id="diag" href="#" style="margin-left:8px">BPS 工具桥接诊断</a>
  </div>
  <div class="card" id="diagcard" style="display:none">
    <div class="k">最近一次 BPS 改写看到的客户端工具（declared = 客户端声明的工具数，catalog_entries = 桥接实际写进提示词的工具数；两者差得多就是模型看不到工具的原因）</div>
    <pre id="diagout"></pre>
  </div>
  <div class="card"><pre id="raw"></pre></div>
</div>
<script>
const token = new URLSearchParams(location.search).get('token') || '';
document.getElementById('accounts').href = '/accounts?token=' + encodeURIComponent(token);
document.getElementById('accounts').setAttribute('href', '/accounts?token=' + encodeURIComponent(token));
fetch('/api/status?token=' + encodeURIComponent(token))
  .then(r => r.json())
  .then(d => {
    const chans = d.channels || {};
    const chanIds = Object.keys(chans);
    const onBps = chanIds.filter(k => (chans[k] || {}).use_bps).length;
    const inHold = chanIds.filter(k => { const c = chans[k] || {}; return c.use_bps && c.hold_remaining_ms > 0; }).length;
    const degradeIds = Object.keys(d.degrade_accounts || {});
    const degradeOn = degradeIds.filter(k => (d.degrade_accounts[k] || {}).enabled).length;
    document.getElementById('status').innerHTML =
      '版本 <span class="v">' + d.version + '</span><br>' +
      '智力巡检 <span class="v">' + (d.intel_enabled ? '开' : '关') + '</span> · 自动循环 <span class="v">' + (d.intel_loop_enabled ? '开' : '关') + '</span> · 自动暂停 <span class="v">' + (d.intel_auto_pause ? '开' : '关') + '</span><br>' +
      'BPS 降智通道 <span class="v">' + (d.bps_enabled ? '开' : '关') + '</span><br>' +
      '流量模板 <span class="v">' + (d.template_ready ? '已捕获' : '未捕获') + '</span><br>' +
      '降智处理账号数 <span class="v">' + degradeOn + '</span>' +
      ' · 当前走 BPS <span class="v">' + onBps + '</span>' +
      ' · 其中在恢复保持期 <span class="v">' + inHold + '</span>';
    document.getElementById('raw').textContent = JSON.stringify(d, null, 2);
  })
  .catch(e => { document.getElementById('status').textContent = '加载失败: ' + e.message; });
document.getElementById('diag').addEventListener('click', function (ev) {
  ev.preventDefault();
  fetch('/api/bps/diagnose?token=' + encodeURIComponent(token))
    .then(r => r.json())
    .then(d => {
      document.getElementById('diagcard').style.display = 'block';
      document.getElementById('diagout').textContent = JSON.stringify(d, null, 2);
    })
    .catch(e => {
      document.getElementById('diagcard').style.display = 'block';
      document.getElementById('diagout').textContent = '加载失败: ' + e.message;
    });
});
</script>
</body>
</html>"##;

const ACCOUNTS_HTML: &str = r##"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>账号智力巡检 · codex-native-transport</title>
<style>
  :root { color-scheme: light; }
  * { box-sizing: border-box; }
  body { font: 14px/1.55 -apple-system, "PingFang SC", system-ui, sans-serif; margin:0; background:#f4f5f7; color:#1f2430; }
  .wrap { max-width: 1240px; margin: 0 auto; padding: 20px 16px 60px; }
  .top { display:flex; align-items:center; gap:10px; flex-wrap:wrap; }
  .top h1 { font-size:19px; margin:0; font-weight:700; margin-right:auto; }
  .card { background:#fff; border:1px solid #e5e7eb; border-radius:12px; padding:12px 14px; margin-bottom:14px; box-shadow:0 1px 2px rgba(16,24,40,.04); }
  .meta { color:#6b7280; font-size:12.5px; }
  .meta b { color:#374151; }
  button, a.btn { border:1px solid #e5e7eb; background:#fff; color:#374151; border-radius:8px; padding:6px 14px; cursor:pointer; font-size:13px; text-decoration:none; display:inline-block; }
  button:hover, a.btn:hover { background:#f3f4f6; }
  button.primary { background:#2b6cff; border-color:#2b6cff; color:#fff; }
  button.danger { background:#fff1f1; border-color:#f3c3c3; color:#b42318; }
  button:disabled { opacity:.55; cursor:default; }
  .chips { display:flex; gap:8px; flex-wrap:wrap; margin:10px 0 4px; }
  .chip { border:1px solid #dfe3ea; background:#fff; border-radius:999px; padding:4px 12px; font-size:12.5px; cursor:pointer; user-select:none; }
  .chip.on { background:#eaf1ff; border-color:#2b6cff; color:#1d4ed8; font-weight:600; }
  table { width:100%; border-collapse:collapse; font-size:13px; }
  th, td { text-align:left; padding:8px 8px; border-bottom:1px solid #eef0f4; vertical-align:top; }
  th { color:#6b7280; font-weight:600; font-size:12.5px; background:#fafbfc; }
  tr.off td { background:#fcfcfd; color:#6b7280; }
  .badge { display:inline-block; border-radius:6px; padding:1px 8px; font-size:12px; font-weight:600; white-space:nowrap; }
  .badge.good { background:#e9f9ef; color:#0f7a3d; }
  .badge.bad { background:#fdeaea; color:#c0392b; }
  .badge.error { background:#fff4e5; color:#b45309; }
  .badge.unknown { background:#f1f2f4; color:#6b7280; }
  .badge.paused { background:#eef1f6; color:#475569; }
  .badge.active { background:#eaf1ff; color:#1d4ed8; }
  .badge.bps { background:#f3ecff; color:#6d28d9; }
  .badge.normal { background:#e9f9ef; color:#0f7a3d; }
  .badge.off { background:#f1f2f4; color:#6b7280; }
  .ans { color:#4b5563; max-width:360px; word-break:break-all; font-size:12.5px; }
  .id { color:#9aa4b6; font-size:12px; }
  .name { word-break:break-all; }
  .err { color:#c0392b; font-size:12.5px; }
  .empty { color:#9aa4b6; padding:22px 0; text-align:center; }
</style>
</head>
<body>
<div class="wrap">
  <div class="top">
    <h1>账号智力巡检</h1>
    <a class="btn" href="/" id="back">返回首页</a>
    <button class="primary" id="probe-sel">检测选中</button>
    <button class="primary" id="probe-view">检测当前筛选</button>
    <button id="pause-sel">暂停选中</button>
    <button id="resume-sel">开启选中</button>
    <button id="degrade-sel">开启降智处理</button>
    <button id="degrade-off-sel">关闭降智处理</button>
    <button id="reload">刷新列表</button>
  </div>
  <div class="card">
    <div class="meta" id="meta">加载中…</div>
    <div class="chips" id="chips"></div>
  </div>
  <div class="card" id="errbox" style="display:none"></div>
  <div class="card">
    <table>
      <thead>
        <tr>
          <th style="width:34px"><input type="checkbox" id="check-all"></th>
          <th style="width:74px">ID</th>
          <th>账号</th>
          <th style="width:110px">套餐</th>
          <th style="width:104px">调度</th>
          <th style="width:118px">智力</th>
          <th style="width:78px">耗时</th>
          <th>回答 / 错误</th>
          <th style="width:120px">降智处理</th>
          <th style="width:158px">当前通道</th>
          <th style="width:170px">操作</th>
        </tr>
      </thead>
      <tbody id="rows"></tbody>
    </table>
    <div class="empty" id="empty" style="display:none">没有匹配的账号</div>
  </div>
</div>
<script>
const token = new URLSearchParams(location.search).get('token') || '';
document.getElementById('back').href = '/?token=' + encodeURIComponent(token);
function api(p, o){ o = o || {}; o.headers = Object.assign({'Authorization':'Bearer ' + token}, o.headers || {}); return fetch(p, o); }
function esc(s){ return (s == null ? '' : String(s)).replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }
function fmtAgo(ms){
  if(!ms) return '—';
  const d = Math.max(0, Date.now() - ms);
  if(d < 60000) return Math.round(d/1000) + ' 秒前';
  if(d < 3600000) return Math.round(d/60000) + ' 分钟前';
  if(d < 86400000) return Math.round(d/3600000) + ' 小时前';
  return Math.round(d/86400000) + ' 天前';
}
let data = null;
let selected = new Set();
let activePlans = new Set();
let pollTimer = null;

function visibleRows(){
  if(!data) return [];
  const rows = data.accounts || [];
  if(activePlans.size === 0) return rows;
  return rows.filter(r => activePlans.has(r.plan_type || ''));
}
function labelFor(value){
  const hit = (data.plans || []).find(p => p.value === value);
  return hit ? hit.label : (value || '未识别');
}

function channelCell(r){
  const reason = r.channel_reason ? `<div class="id">${esc(r.channel_reason)}</div>` : '';
  if(r.channel === 'bps') return `<span class="badge bps">BPS</span>${reason}`;
  if(r.channel === 'normal') return `<span class="badge normal">正常</span>${reason}`;
  return `<span class="badge off">—</span>${r.degrade ? '<div class="id">等待巡检结论</div>' : ''}`;
}

function renderMeta(){
  const d = data;
  const interval = d.loop_interval_seconds >= 3600 ? (d.loop_interval_seconds/3600).toFixed(1) + ' 小时' : d.loop_interval_seconds + ' 秒';
  const run = d.run || {};
  const runTxt = run.running ? `<b>巡检中 ${run.done}/${run.total}</b>` : (run.finished_at_ms ? `上次巡检 ${fmtAgo(run.finished_at_ms)}` : '尚未巡检');
  document.getElementById('meta').innerHTML =
    `总开关 <b>${d.enabled ? '开' : '关'}</b> · 自动循环 <b>${d.loop_enabled ? '开' : '关'}</b>（每 ${interval}） · 自动暂停/恢复 <b>${d.auto_pause ? '开' : '关'}</b><br>` +
    `模型 <b>${esc(d.model)}</b> · 提问 <b>${esc(d.prompt)}</b> · 不合格标记 <b>${esc(d.fail_marker)}</b> · 并发 <b>${d.concurrency}</b><br>` +
    `BPS 降智通道 <b>${d.bps_enabled ? '开' : '关'}</b>（模型 ${esc((d.bps_models||[]).join(', '))}） · 流量模板 <b>${d.template_ready ? '已捕获' : '未捕获'}</b><br>` +
    `切换节奏：连续合格 <b>${d.intel_confirmations}</b> 次 · 恢复后保持 <b>${d.bps_hold_after_healthy_seconds}</b>s · 会话粘滞 <b>${d.bps_session_sticky_seconds}</b>s · BPS 失败冷却 <b>${d.bps_fallback_cooldown_seconds}</b>s · 提问超时 <b>${d.intel_prompt_timeout_seconds}</b>s + <b>${d.intel_prompt_retries}</b> 次重试${d.intel_timeout_is_failed ? '（超时算不合格）' : ''}<br>` +
    `判定口径：不合格关键词 <b>${esc(d.fail_marker)}</b>${d.intel_require_year ? ' · 回答里必须出现年份' : ''}<br>` +
    `${runTxt}${run.note ? ' · ' + esc(run.note) : ''}`;
  const chips = document.getElementById('chips');
  chips.innerHTML = '';
  const all = document.createElement('span');
  all.className = 'chip' + (activePlans.size === 0 ? ' on' : '');
  all.textContent = `全部（${(data.accounts || []).length}）`;
  all.onclick = () => { activePlans.clear(); selected.clear(); render(); };
  chips.appendChild(all);
  for(const p of (data.plans || [])){
    const chip = document.createElement('span');
    chip.className = 'chip' + (activePlans.has(p.value) ? ' on' : '');
    chip.textContent = `${p.label}（${p.count}）`;
    chip.onclick = () => {
      if(activePlans.has(p.value)) activePlans.delete(p.value); else activePlans.add(p.value);
      selected.clear(); render();
    };
    chips.appendChild(chip);
  }
}

function render(){
  if(!data) return;
  document.getElementById('errbox').style.display = data.error ? '' : 'none';
  document.getElementById('errbox').innerHTML = data.error ? `<span class="err">${esc(data.error)}</span>` : '';
  renderMeta();
  const body = document.getElementById('rows');
  body.innerHTML = '';
  const rows = visibleRows();
  document.getElementById('empty').style.display = rows.length ? 'none' : '';
  for(const r of rows){
    const tr = document.createElement('tr');
    if(!r.schedulable) tr.className = 'off';
    const intel = r.intel === 'good' ? '合格' : (r.intel === 'bad' ? (r.timed_out ? '超时' : '不合格') : (r.intel === 'error' ? '检测失败' : '未检测'));
    const intelCls = r.intel === 'good' ? 'good' : (r.timed_out ? 'error' : (r.intel || 'unknown'));
    const detail = r.error ? `<span class="err">${esc(r.error)}</span>` : esc(r.answer || '');
    tr.innerHTML =
      `<td><input type="checkbox" data-id="${r.id}"${selected.has(r.id) ? ' checked' : ''}></td>` +
      `<td class="id">#${r.id}</td>` +
      `<td class="name">${esc(r.name)}</td>` +
      `<td>${esc(labelFor(r.plan_type || ''))}</td>` +
      `<td><span class="badge ${r.schedulable ? 'active' : 'paused'}">${r.schedulable ? '调度中' : '已暂停'}</span><div class="id">${esc(r.status || '')}</div></td>` +
      `<td><span class="badge ${intelCls}">${intel}</span><div class="id">${fmtAgo(r.checked_at_ms)}</div></td>` +
      `<td class="id">${r.latency_ms ? (r.latency_ms / 1000).toFixed(1) + 's' : '—'}</td>` +
      `<td class="ans">${detail}</td>` +
      `<td>${r.degrade ? '<span class="badge active">已开启</span>' : '<span class="badge unknown">关闭</span>'}</td>` +
      `<td>${channelCell(r)}</td>` +
      `<td>
         <button data-act="probe" data-id="${r.id}">复检</button>
         <button data-act="degrade" data-id="${r.id}" data-on="${r.degrade ? 0 : 1}">${r.degrade ? '关闭处理' : '降智处理'}</button>
         <button data-act="bps" data-id="${r.id}"${data.bps_enabled ? '' : ' style="display:none"'}>BPS自检</button>
         ${r.schedulable ? `<button class="danger" data-act="off" data-id="${r.id}">暂停</button>`
                         : `<button data-act="on" data-id="${r.id}">开启</button>`}
       </td>`;
    body.appendChild(tr);
  }
  body.querySelectorAll('input[type=checkbox]').forEach(cb => {
    cb.onchange = () => {
      const id = Number(cb.dataset.id);
      if(cb.checked) selected.add(id); else selected.delete(id);
    };
  });
  body.querySelectorAll('button[data-act]').forEach(btn => {
    btn.onclick = () => rowAction(btn);
  });
  const poll = (data.run || {}).running;
  if(poll && !pollTimer) pollTimer = setInterval(load, 4000);
  if(!poll && pollTimer){ clearInterval(pollTimer); pollTimer = null; }
}

async function rowAction(btn){
  const id = Number(btn.dataset.id);
  const act = btn.dataset.act;
  btn.disabled = true;
  try{
    if(act === 'probe'){
      await api('/api/intel/probe?ids=' + id, {method:'POST'});
    } else if(act === 'degrade'){
      await api(`/api/degrade?id=${id}&enabled=${btn.dataset.on}`, {method:'POST'});
    } else if(act === 'bps'){
      const resp = await api('/api/bps/check?id=' + id, {method:'POST'});
      const report = await resp.json();
      alert('HTTP ' + (report.status || '-') + '\n' + (report.error || '') + '\n' + (report.snippet || ''));
      btn.disabled = false;
      return;
    } else {
      await api(`/api/intel/set-schedulable?id=${id}&schedulable=${act === 'on' ? 1 : 0}`, {method:'POST'});
    }
  }catch(ex){}
  setTimeout(() => { btn.disabled = false; load(); }, 600);
}

async function batch(action){
  const rows = visibleRows();
  let ids = Array.from(selected);
  if(action === 'probe-view') ids = rows.map(r => r.id);
  if(!ids.length){ alert('先勾选账号'); return; }
  if(action === 'probe-sel' || action === 'probe-view'){
    await api('/api/intel/probe?ids=' + ids.join(','), {method:'POST'});
  } else if(action === 'degrade-sel' || action === 'degrade-off-sel'){
    const want = action === 'degrade-sel' ? 1 : 0;
    for(const id of ids){
      await api(`/api/degrade?id=${id}&enabled=${want}`, {method:'POST'});
    }
  } else {
    const want = action === 'resume-sel' ? 1 : 0;
    for(const id of ids){
      await api(`/api/intel/set-schedulable?id=${id}&schedulable=${want}`, {method:'POST'});
    }
  }
  load();
}

async function load(){
  try{
    const r = await api('/api/intel/accounts');
    if(!r.ok) throw new Error('HTTP ' + r.status);
    data = await r.json();
    render();
  }catch(ex){
    document.getElementById('errbox').style.display = '';
    document.getElementById('errbox').innerHTML = `<span class="err">加载失败: ${esc(ex.message)}（检查 URL 里的 ?token=）</span>`;
  }
}

document.getElementById('probe-sel').onclick = () => batch('probe-sel');
document.getElementById('probe-view').onclick = () => batch('probe-view');
document.getElementById('pause-sel').onclick = () => batch('pause-sel');
document.getElementById('resume-sel').onclick = () => batch('resume-sel');
document.getElementById('degrade-sel').onclick = () => batch('degrade-sel');
document.getElementById('degrade-off-sel').onclick = () => batch('degrade-off-sel');
document.getElementById('reload').onclick = () => load();
document.getElementById('check-all').onchange = (e) => {
  selected.clear();
  if(e.target.checked) visibleRows().forEach(r => selected.add(r.id));
  render();
};
load(); setInterval(load, 20000);
</script>
</body>
</html>"##;
