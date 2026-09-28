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
    /// 跨机同步用的共享密钥（请求头 `x-cnt-sync-token`）。
    sync_token: Option<String>,
    /// 请求体（只给 /api/state/push 用；面板其它端点都是 GET/无体）。
    body: Vec<u8>,
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
    // 对端推送走独立密钥（sync_token，空则复用 panel_token），不要求带面板 token。
    let sync_want = config.sync_auth_token();
    let via_sync = req.path == "/api/state/push"
        && !sync_want.is_empty()
        && req.sync_token.as_deref() == Some(sync_want.as_str());
    if req.token.as_deref() != Some(want.as_str()) && !via_sync {
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
                    // 本地改动立刻同步给对端（sync_enabled 时才真正推）。
                    crate::sync::notify();
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
        ("POST", "/api/state/push") => {
            let cfg = state.current_config();
            let body = if !cfg.sync_enabled {
                "{\"ok\":false,\"error\":\"本机未开启 sync_enabled\"}".to_string()
            } else {
                crate::sync::apply_remote_json(state, &req.body)
            };
            write_response(&mut stream, 200, "application/json", body.as_bytes())
        }
        ("GET", "/api/state/snapshot") => {
            let cfg = state.current_config();
            let body = crate::sync::snapshot_json(state, &cfg).to_string();
            write_response(&mut stream, 200, "application/json", body.as_bytes())
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
        // BPS 403 自动摘除模型的「立刻恢复」：摘除记录的到点恢复由后台循环做，
        // 这里给面板一个手动按钮（也能在配置关掉后用来提前放行）。
        ("POST", "/api/models/restore") => {
            let account_id = query_get(&req.query, "id").and_then(|v| v.parse::<i64>().ok());
            let cfg = state.current_config();
            let path = cfg.model_drop_state_file();
            state.model_drop.ensure_loaded(&path);
            let record = account_id.and_then(|id| state.model_drop.get(id));
            let body = match (account_id, record) {
                (Some(_), Some(record)) => {
                    let base = cfg.admin_api_base.trim().to_string();
                    let key = cfg.admin_api_key.trim().to_string();
                    match handle.block_on(crate::model_drop::restore_one(
                        state, &cfg, &base, &key, &path, &record,
                    )) {
                        Ok(()) => "{\"ok\":true}".to_string(),
                        Err(err) => format!("{{\"ok\":false,\"error\":{}}}", json_string(&err)),
                    }
                }
                (Some(_), None) => "{\"ok\":true,\"note\":\"没有待恢复的模型\"}".to_string(),
                (None, _) => json_error("id required"),
            };
            write_response(&mut stream, 200, "application/json", body.as_bytes())
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
    let mut sync_hdr: Option<String> = None;
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
            } else if name == crate::sync::SYNC_TOKEN_HEADER {
                sync_hdr = Some(value.to_string());
            }
        }
    }
    let mut body = Vec::new();
    if content_length > 0 {
        let mut buf = vec![0u8; content_length.min(crate::sync::MAX_SNAPSHOT_BYTES)];
        if reader.read_exact(&mut buf).is_ok() {
            body = buf;
        }
    }

    let token = token_hdr.or_else(|| query_get(&query, "token"));
    Ok(Some(Request {
        method,
        path,
        query,
        token,
        sync_token: sync_hdr,
        body,
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
        "bps_official_client_only": config.bps_official_client_only,
        "bps_origin": config.bps_origin,
        "bps_user_agent": config.bps_user_agent,
        "bps_pseudonym_prompt_cache_key": config.bps_pseudonym_prompt_cache_key,
        "bps_metadata_agent_iteration": config.bps_metadata_agent_iteration,
        "bps_cooldown_timeout_seconds": config.bps_cooldown_timeout_seconds,
        "bps_cooldown_400_seconds": config.bps_cooldown_400_seconds,
        "bps_cooldown_401_seconds": config.bps_cooldown_401_seconds,
        "bps_cooldown_403_seconds": config.bps_cooldown_403_seconds,
        "bps_cooldown_429_seconds": config.bps_cooldown_429_seconds,
        "bps_fallback_cooldown_seconds": config.bps_fallback_cooldown_seconds,
        "bps_daily_limit_per_account": config.bps_daily_limit_per_account,
        "effort_retry_enabled": config.effort_retry_enabled,
        "admin_api_base": config.admin_api_base,
        "admin_api_key_configured": !config.admin_api_key.trim().is_empty(),
                "sync_enabled": config.sync_enabled,
                "sync_peers": config.sync_peer_list(),
                "sync_push": config.sync_push,
        "sync": crate::sync::status_json(),
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
    // 打开页面就先把套餐默认开关套上：页面看到的即真实生效的。
    crate::intel::apply_target_plan_defaults(state, &config, &targets);
    let degrade = state.degrade.snapshot();
    let accounts = crate::intel::merge_rows(
        &targets,
        &results,
        &degrade,
        &state.channels,
        &config,
        crate::donor::now_ms(),
    );
    // BPS 403 自动摘除模型：面板按账号展示「已摘模型 + 剩余恢复时间」。
    let drop_path = config.model_drop_state_file();
    state.model_drop.ensure_loaded(&drop_path);
    let drop_now = crate::donor::now_ms();
    let drops = state.model_drop.snapshot();
    let mut accounts = accounts;
    for row in accounts.iter_mut() {
        let Some(id) = row.get("id").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Some(record) = drops.iter().find(|record| record.account_id == id) else {
            continue;
        };
        let Some(object) = row.as_object_mut() else {
            continue;
        };
        object.insert(
            "models_dropped".to_string(),
            serde_json::json!({
                "models": record.models,
                "form": record.form,
                "until_ms": record.until_ms,
                "remaining_ms": record.until_ms.saturating_sub(drop_now),
                "dropped_at_ms": record.dropped_at_ms,
                "last_error": record.last_error,
            }),
        );
    }
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
        "bps_skip_on_previous_response_id": config.bps_skip_on_previous_response_id,
        "bps_previous_response_pin_seconds": config.bps_previous_response_pin_seconds,
        "bps_cooldown_timeout_seconds": config.bps_cooldown_timeout_seconds,
        "bps_cooldown_400_seconds": config.bps_cooldown_400_seconds,
        "bps_cooldown_401_seconds": config.bps_cooldown_401_seconds,
        "bps_cooldown_403_seconds": config.bps_cooldown_403_seconds,
        "bps_cooldown_429_seconds": config.bps_cooldown_429_seconds,
        "bps_fallback_cooldown_seconds": config.bps_fallback_cooldown_seconds,
        "bps_daily_limit_per_account": config.bps_daily_limit_per_account,
        "bps_403_drop_models_enabled": config.bps_403_drop_models_enabled,
        "bps_403_drop_models": config.bps_403_drop_model_list(),
        "bps_403_drop_seconds": config.bps_403_drop_seconds(),
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
  .wrap { max-width: 1920px; margin: 0 auto; padding: 20px 16px 60px; }
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
  .tablewrap { overflow-x:auto; }
  table { width:100%; min-width:1800px; border-collapse:collapse; font-size:13px; }
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
  .badge.warn { background:#fff4e5; color:#b45309; }
  .ans { color:#4b5563; max-width:460px; word-break:break-all; font-size:12.5px; }
  .id { color:#9aa4b6; font-size:12px; }
  .id.err { color:#c0392b; }
  .name { word-break:break-all; overflow-wrap:anywhere; }
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
  <p class="hint">Business Premium（self_serve_business_prolite）类型账号：插件配置里「降智账号 BPS 通道」总开关打开时，默认就勾上「降智处理」（新增账号约 1 分钟内自动套上）；手动关掉的账号不会被自动打开。</p>
    <div class="chips" id="chips"></div>
  </div>
  <div class="card" id="errbox" style="display:none"></div>
  <div class="card">
    <div class="tablewrap">
    <table>
      <thead>
        <tr>
          <th style="width:34px"><input type="checkbox" id="check-all"></th>
          <th style="width:64px">ID</th>
          <th style="width:240px">账号</th>
          <th style="width:118px">套餐</th>
          <th style="width:100px">调度</th>
          <th style="width:108px">智力</th>
          <th style="width:66px">耗时</th>
          <th>回答 / 错误</th>
          <th style="width:110px">降智处理</th>
          <th style="width:122px">当前通道</th>
          <th style="width:140px">线路统计</th>
          <th style="width:112px">当前线路</th>
          <th style="width:152px">BPS 冷却 / 今日</th>
          <th style="width:188px">操作</th>
        </tr>
      </thead>
      <tbody id="rows"></tbody>
    </table>
    <div class="empty" id="empty" style="display:none">没有匹配的账号</div>
    </div>
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
function fmtLeft(ms){
  const s = Math.round(Math.max(0, ms) / 1000);
  if(s < 60) return s + 's';
  if(s < 3600) return Math.round(s/60) + ' 分钟';
  return (s/3600).toFixed(1) + ' 小时';
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

function routeStatsCell(r){
  const nOk = r.normal_ok || 0, nBad = r.normal_fail || 0;
  const bOk = r.bps_ok || 0, bBad = r.bps_fail || 0;
  const ok = nOk + bOk, bad = nBad + bBad, total = ok + bad;
  if(!total) return '<span class="badge off">暂无请求</span>';
  const rate = Math.round(ok * 100 / total);
  const cls = bad === 0 ? 'good' : (rate >= 95 ? 'active' : (rate >= 80 ? 'warn' : 'bad'));
  return `<span class="badge ${cls}">成功 ${ok} · 失败 ${bad}</span>` +
         `<div class="id">正常 ${nOk}/${nBad} · BPS ${bOk}/${bBad}（成功/失败）</div>` +
         `<div class="id">成功率 ${rate}% · 共 ${total} 次</div>`;
}

function routeNowCell(r){
  if(!r.last_route) return '<span class="badge off">未走流量</span>';
  const isBps = r.last_route === 'bps';
  const st = r.last_status || 0;
  const ok = st >= 200 && st < 400;
  const stTxt = st ? ('HTTP ' + st) : '连接失败';
  return `<span class="badge ${isBps ? 'bps' : 'normal'}">${isBps ? 'BPS' : '正常'}</span>` +
         `<div class="${ok ? 'id' : 'id err'}">${stTxt}</div>` +
         `<div class="id">${fmtAgo(r.last_route_at_ms)}</div>`;
}

function bpsCooldownCell(r){
  const left = r.bps_cooldown_remaining_ms || 0;
  const limit = r.bps_daily_limit || 0;
  const today = r.bps_today || 0;
  const limitTxt = limit ? ('今日 ' + today + '/' + limit) : ('今日 ' + today + ' 次（不限）');
  const badge = left > 0
    ? ('<span class="badge warn">冷却 ' + fmtLeft(left) + '</span>')
    : '<span class="badge off">不在冷却</span>';
  const reason = r.bps_cooldown_reason ? ('<div class="id' + (left > 0 ? ' err' : '') + '">' + esc(r.bps_cooldown_reason) + '</div>') : '';
  const at = (!left && r.bps_cooldown_at_ms) ? ('<div class="id">' + fmtAgo(r.bps_cooldown_at_ms) + '</div>') : '';
  return badge + '<div class="id">' + limitTxt + '</div>' + reason + at + dropLine(r);
}

// BPS 403 自动摘除模型：显示这个号当前被摘掉的模型与剩余恢复时间。
function dropLine(r){
  const d = r.models_dropped;
  if(!d || !d.models || !d.models.length) return '';
  const left = d.remaining_ms || 0;
  return '<div class="id err">已摘模型 ' + esc(d.models.join(', ')) +
         (left > 0 ? ('（' + fmtLeft(left) + '后恢复）') : '（待恢复）') + '</div>' +
         (d.last_error ? ('<div class="id err">恢复失败：' + esc(d.last_error) + '</div>') : '');
}

function renderMeta(){
  const allRows = data.accounts || [];
  const bpsTodayMax = allRows.reduce((a, r) => Math.max(a, r.bps_today || 0), 0);
  const d = data;
  const interval = d.loop_interval_seconds >= 3600 ? (d.loop_interval_seconds/3600).toFixed(1) + ' 小时' : d.loop_interval_seconds + ' 秒';
  const run = d.run || {};
  const runTxt = run.running ? `<b>巡检中 ${run.done}/${run.total}</b>` : (run.finished_at_ms ? `上次巡检 ${fmtAgo(run.finished_at_ms)}` : '尚未巡检');
  document.getElementById('meta').innerHTML =
    `总开关 <b>${d.enabled ? '开' : '关'}</b> · 自动循环 <b>${d.loop_enabled ? '开' : '关'}</b>（每 ${interval}） · 自动暂停/恢复 <b>${d.auto_pause ? '开' : '关'}</b><br>` +
    `模型 <b>${esc(d.model)}</b> · 提问 <b>${esc(d.prompt)}</b> · 不合格标记 <b>${esc(d.fail_marker)}</b> · 并发 <b>${d.concurrency}</b><br>` +
    `BPS 降智通道 <b>${d.bps_enabled ? '开' : '关'}</b>（模型 ${esc((d.bps_models||[]).join(', '))}） · 流量模板 <b>${d.template_ready ? '已捕获' : '未捕获'}</b><br>` +
    `BPS 403 自动摘模型 <b>${d.bps_403_drop_models_enabled ? '开' : '关'}</b>（模型 ${esc((d.bps_403_drop_models||[]).join(', '))} · 摘除 ${d.bps_403_drop_seconds}s 后恢复）<br>` +
    `出站特征：Origin <b>${esc(d.bps_origin || '不发')}</b> · UA <b>${esc(d.bps_user_agent || '保持客户端')}</b> · 会话键假名 <b>${d.bps_pseudonym_prompt_cache_key ? '开' : '关'}</b> · metadata.agent_iteration <b>${d.bps_metadata_agent_iteration ? '开' : '关'}</b> · 推理挡位自愈 <b>${d.effort_retry_enabled ? '开' : '关'}</b><br>` +
    `切换节奏：连续合格 <b>${d.intel_confirmations}</b> 次 · 恢复后保持 <b>${d.bps_hold_after_healthy_seconds}</b>s · 会话粘滞 <b>${d.bps_session_sticky_seconds}</b>s · BPS 冷却(失败后) 超时 <b>${d.bps_cooldown_timeout_seconds}</b>s / 400 <b>${d.bps_cooldown_400_seconds}</b>s / 401 <b>${d.bps_cooldown_401_seconds}</b>s / 403 <b>${d.bps_cooldown_403_seconds}</b>s / 429 <b>${d.bps_cooldown_429_seconds}</b>s / 其它 <b>${d.bps_fallback_cooldown_seconds}</b>s · 每账号每日上限 <b>${d.bps_daily_limit_per_account ? d.bps_daily_limit_per_account + ' 次（今日最多 ' + bpsTodayMax + ' 次）' : '不限'}</b> · 提问超时 <b>${d.intel_prompt_timeout_seconds}</b>s + <b>${d.intel_prompt_retries}</b> 次重试${d.intel_timeout_is_failed ? '（超时算不合格）' : ''}<br>` +
    `previous_response_id 门禁 <b>${d.bps_skip_on_previous_response_id !== false ? '开' : '关'}</b>（命中后整条会话钉在正常通道 <b>${d.bps_previous_response_pin_seconds || 0}</b>s）<br>` +
    `判定口径：不合格关键词 <b>${esc(d.fail_marker)}</b>${d.intel_require_year ? ' · 回答里必须出现年份' : ''}<br>` +
    `${runTxt}${run.note ? ' · ' + esc(run.note) : ''}<br>` +
    (() => {
      const rows = d.accounts || [];
      const ok = rows.reduce((a, r) => a + (r.normal_ok || 0) + (r.bps_ok || 0), 0);
      const bad = rows.reduce((a, r) => a + (r.normal_fail || 0) + (r.bps_fail || 0), 0);
      const onBps = rows.filter(r => r.last_route === 'bps').length;
      const onNormal = rows.filter(r => r.last_route === 'normal').length;
      const cooling = rows.filter(r => (r.bps_cooldown_remaining_ms || 0) > 0).length;
      return `线路统计（插件启动以来累计）：成功 <b>${ok}</b> · 失败 <b>${bad}</b> · 最近走 BPS <b>${onBps}</b> 个 · 最近走正常 <b>${onNormal}</b> 个 · BPS 冷却中 <b>${cooling}</b> 个 · 暂无流量 <b>${rows.length - onBps - onNormal}</b> 个`;
    })();
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
    const detail = (r.error ? `<span class="err">${esc(r.error)}</span>` : esc(r.answer || '')) +
                   (r.note ? `<div class="id">${esc(r.note)}</div>` : '');
    tr.innerHTML =
      `<td><input type="checkbox" data-id="${r.id}"${selected.has(r.id) ? ' checked' : ''}></td>` +
      `<td class="id">#${r.id}</td>` +
      `<td class="name">${esc(r.name)}</td>` +
      `<td>${esc(labelFor(r.plan_type || ''))}</td>` +
      `<td><span class="badge ${r.schedulable ? 'active' : 'paused'}">${r.schedulable ? '调度中' : '已暂停'}</span><div class="id">${esc(r.status || '')}</div></td>` +
      `<td><span class="badge ${intelCls}">${intel}</span><div class="id">${fmtAgo(r.checked_at_ms)}</div></td>` +
      `<td class="id">${r.latency_ms ? (r.latency_ms / 1000).toFixed(1) + 's' : '—'}</td>` +
      `<td class="ans">${detail}</td>` +
      `<td>${r.degrade ? '<span class="badge active">已开启</span>' + (r.degrade_default ? '<div class="id">套餐默认</div>' : '') : '<span class="badge unknown">关闭</span>'}</td>` +
      `<td>${channelCell(r)}</td>` +
      `<td>${routeStatsCell(r)}</td>` +
      `<td>${routeNowCell(r)}</td>` +
      `<td>${bpsCooldownCell(r)}</td>` +
      `<td>
         <button data-act="probe" data-id="${r.id}">复检</button>
         <button data-act="degrade" data-id="${r.id}" data-on="${r.degrade ? 0 : 1}">${r.degrade ? '关闭处理' : '降智处理'}</button>
         <button data-act="bps" data-id="${r.id}"${data.bps_enabled ? '' : ' style="display:none"'}>BPS自检</button>
         ${r.models_dropped && (r.models_dropped.models||[]).length ? `<button data-act="restore-models" data-id="${r.id}">恢复模型</button>` : ''}
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
    } else if(act === 'restore-models'){
      const resp = await api('/api/models/restore?id=' + id, {method:'POST'});
      const j = await resp.json();
      if(j && j.ok === false) alert('恢复失败：' + (j.error || ''));
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
