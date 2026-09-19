//! Sub2API OpenAI OAuth outbound transport 插件入口。
//!
//! 实现 hashicorp/go-plugin 的子进程握手协议（stdout 一行握手 + 本机 unix
//! socket 上的明文 gRPC），并注册 Sub2API 的 `TransportPlugin` 服务。
//! 上游请求通过与官方 Codex CLI（codex-rs rust-v0.153.4）逐版本一致的
//! reqwest/native-tls/hyper/h2 栈发出，使 TLS ClientHello 与 HTTP/2 帧特征
//! 与真实 Codex 客户端一致。

mod admin;
mod config;
mod goplugin;
mod identity;
mod notify;
mod panel;
mod refresh;
mod service;
mod transport;
mod turn_state;
mod version_sync;

pub mod proto {
    pub mod sub2api {
        pub mod plugin {
            pub mod v1 {
                tonic::include_proto!("sub2api.plugin.v1");
            }
        }
    }
    /// hashicorp/go-plugin 内部服务（package `plugin`）。
    pub mod hashicorp {
        tonic::include_proto!("plugin");
    }
}

use std::io::Write;
use std::path::PathBuf;

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

use proto::hashicorp::grpc_broker_server::GrpcBrokerServer;
use proto::hashicorp::grpc_controller_server::GrpcControllerServer;
use proto::hashicorp::grpc_stdio_server::GrpcStdioServer;
use proto::sub2api::plugin::v1::transport_plugin_server::TransportPluginServer;

const MAGIC_COOKIE_KEY: &str = "SUB2API_PLUGIN_MAGIC_COOKIE";
const MAGIC_COOKIE_VALUE: &str = "sub2api-plugin-v1";
/// go-plugin 核心握手协议版本（hashicorp CoreProtocolVersion，恒为 1）。
const CORE_PROTOCOL_VERSION: u32 = 1;
/// Sub2API 应用层握手协议版本（pluginv1.ProtocolVersion）。
const APP_PROTOCOL_VERSION: u32 = 1;

fn main() {
    // 防止插件被当作普通程序直接运行（go-plugin 魔法 cookie 约定）。
    if std::env::var(MAGIC_COOKIE_KEY).as_deref() != Ok(MAGIC_COOKIE_VALUE) {
        eprintln!(
            "This binary is a Sub2API plugin. It must be launched by the Sub2API host, \
             not executed directly."
        );
        std::process::exit(1);
    }

    // Sub2API 以 SkipHostEnv 启动插件（环境变量几乎为空）。Linux 上 vendored
    // OpenSSL 的默认 CA 路径不可用，按系统实际路径探测并设置 SSL_CERT_FILE/DIR。
    #[cfg(target_os = "linux")]
    openssl_probe::init_ssl_cert_env_vars();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    if let Err(err) = runtime.block_on(serve()) {
        eprintln!("plugin serve error: {err}");
        std::process::exit(1);
    }
}

fn socket_path() -> PathBuf {
    // 宿主通过 UnixSocketConfig.TempDir 传入专用 socket 目录。
    let dir = std::env::var("PLUGIN_UNIX_SOCKET_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!(
        "codex-native-transport-{}.sock",
        std::process::id()
    ))
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let incoming = UnixListenerStream::new(listener);

    let state = service::SharedState::new();
    // 后台版本自动同步（配置热更新后自动生效，无需重启任务）。
    tokio::spawn(version_sync::run(std::sync::Arc::clone(&state)));
    // 后台 canary 循环（按配置间隔重铸 turn-state / 判降智）。
    tokio::spawn(refresh::canary_loop(std::sync::Arc::clone(&state)));
    // 后台主动养池循环（最小 hi 探铸 + 出口池轮换，锁到 292 即停）。
    tokio::spawn(refresh::warm_loop(std::sync::Arc::clone(&state)));
    // 内置 admin key 的全池主动养池：调 admin API 枚举全部可调度 openai oauth 号造票。
    tokio::spawn(refresh::admin_warm_loop(std::sync::Arc::clone(&state)));
    // 每账号休息编排：出口池转满一圈仍无 292 才休息；休息 = 优先级排空（老会话不打散），到点恢复。
    tokio::spawn(refresh::rest_loop(std::sync::Arc::clone(&state)));
    // 智商切换 TG 通知：池投出档位跃迁事件 → 通知任务直连 Telegram 推送。
    let (tier_tx, tier_rx) = tokio::sync::mpsc::unbounded_channel();
    state.pool.set_notifier(tier_tx);
    tokio::spawn(notify::notify_loop(std::sync::Arc::clone(&state), tier_rx));
    // 内嵌管理面板（仅在 panel_addr 配置后监听；带 token 鉴权）。
    panel::spawn(
        std::sync::Arc::clone(&state),
        tokio::runtime::Handle::current(),
    );
    let sigterm_state = std::sync::Arc::clone(&state);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let controller = goplugin::Controller::new(shutdown_tx, std::sync::Arc::clone(&state));
    let transport_service = service::TransportService::new(state);

    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    // go-plugin 客户端按服务名 "plugin" 做健康检查。
    health_reporter
        .set_service_status("plugin", tonic_health::ServingStatus::Serving)
        .await;
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    // 握手行必须在监听就绪后输出：
    // CORE-PROTOCOL-VERSION|APP-PROTOCOL-VERSION|NETWORK|ADDRESS|PROTOCOL
    println!(
        "{CORE_PROTOCOL_VERSION}|{APP_PROTOCOL_VERSION}|unix|{}|grpc",
        path.display()
    );
    std::io::stdout().flush()?;

    let shutdown = async move {
        let sigterm = async {
            #[cfg(unix)]
            {
                let mut signal =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("install SIGTERM handler");
                signal.recv().await;
            }
            #[cfg(not(unix))]
            std::future::pending::<()>().await;
        };
        tokio::select! {
            _ = shutdown_rx.changed() => {}
            _ = sigterm => {
                // SIGTERM 路径（宿主未走 Shutdown RPC 直接发信号）：同样先把休息中账号的优先级写回。
                refresh::restore_on_shutdown(&sigterm_state).await;
            }
        }
    };

    Server::builder()
        .add_service(TransportPluginServer::new(transport_service))
        .add_service(health_service)
        .add_service(GrpcControllerServer::new(controller))
        .add_service(GrpcStdioServer::new(goplugin::Stdio::default()))
        .add_service(GrpcBrokerServer::new(goplugin::Broker::default()))
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await?;

    let _ = std::fs::remove_file(&path);
    Ok(())
}
