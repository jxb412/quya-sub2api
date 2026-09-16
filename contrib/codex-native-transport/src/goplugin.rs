//! hashicorp/go-plugin 内部服务的最小实现。
//!
//! - GRPCController: 宿主 Kill 时先调用 Shutdown，实现优雅退出。
//! - GRPCStdio: 宿主会打开该流镜像插件 stdout/stderr（Sub2API 侧丢弃），
//!   返回一个永不产出的挂起流即可。
//! - GRPCBroker: 传输契约不使用子连接，泊住流即可。

use std::pin::Pin;

use futures_util::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::proto::hashicorp::{
    grpc_broker_server::GrpcBroker, grpc_controller_server::GrpcController,
    grpc_stdio_server::GrpcStdio, ConnInfo, Empty, StdioData,
};

pub struct Controller {
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Controller {
    pub fn new(shutdown: tokio::sync::watch::Sender<bool>) -> Self {
        Self { shutdown }
    }
}

#[tonic::async_trait]
impl GrpcController for Controller {
    async fn shutdown(&self, _request: Request<Empty>) -> Result<Response<Empty>, Status> {
        let _ = self.shutdown.send(true);
        Ok(Response::new(Empty {}))
    }
}

#[derive(Default)]
pub struct Stdio;

#[tonic::async_trait]
impl GrpcStdio for Stdio {
    type StreamStdioStream =
        Pin<Box<dyn Stream<Item = Result<StdioData, Status>> + Send + 'static>>;

    async fn stream_stdio(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::StreamStdioStream>, Status> {
        // 永不产出数据的挂起流：sender 被 move 进流内保持存活，宿主断开时随流释放。
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StdioData, Status>>(1);
        let stream = ReceiverStream::new(rx);
        let guarded = GuardedStream {
            _tx: tx,
            inner: stream,
        };
        Ok(Response::new(Box::pin(guarded)))
    }
}

struct GuardedStream<T> {
    _tx: tokio::sync::mpsc::Sender<T>,
    inner: ReceiverStream<T>,
}

impl<T> Stream for GuardedStream<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[derive(Default)]
pub struct Broker;

#[tonic::async_trait]
impl GrpcBroker for Broker {
    type StartStreamStream = Pin<Box<dyn Stream<Item = Result<ConnInfo, Status>> + Send + 'static>>;

    async fn start_stream(
        &self,
        request: Request<Streaming<ConnInfo>>,
    ) -> Result<Response<Self::StartStreamStream>, Status> {
        // 泊住入站流（宿主保持连接），出站流永不产出。
        let mut inbound = request.into_inner();
        tokio::spawn(async move { while let Ok(Some(_)) = inbound.message().await {} });
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ConnInfo, Status>>(1);
        let stream = ReceiverStream::new(rx);
        let guarded = GuardedStream {
            _tx: tx,
            inner: stream,
        };
        Ok(Response::new(Box::pin(guarded)))
    }
}
