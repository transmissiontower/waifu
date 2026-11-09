//! avatar-service: fan-out hub for avatar events (emotion, state, lip-sync, animation).
//! The web avatar receives them through the gateway; an optional bridge mirrors them to
//! VTube Studio over its WebSocket API.
mod vts;

use common::Boot;
use contracts::avatar_server::{Avatar, AvatarServer};
use contracts::*;
use std::pin::Pin;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

struct Svc {
    tx: broadcast::Sender<AvatarEvent>,
    metrics: std::sync::Arc<common::Metrics>,
}

#[tonic::async_trait]
impl Avatar for Svc {
    async fn publish(&self, req: Request<AvatarEvent>) -> Result<Response<Ack>, Status> {
        self.metrics.inc("events_published_total");
        let _ = self.tx.send(req.into_inner());
        Ok(Response::new(Ack { ok: true, message: String::new() }))
    }
    type SubscribeStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<AvatarEvent, Status>> + Send>>;
    async fn subscribe(&self, _: Request<Empty>) -> Result<Response<Self::SubscribeStream>, Status> {
        self.metrics.inc("subscriptions_total");
        let s = BroadcastStream::new(self.tx.subscribe()).filter_map(|r| r.ok().map(Ok));
        Ok(Response::new(Box::pin(s)))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = Boot::init("avatar")?;
    let (tx, _) = broadcast::channel::<AvatarEvent>(512);
    let settings: vts::Settings = boot.cfg.settings("avatar");
    if settings.vtube_studio.enabled {
        tokio::spawn(vts::run(settings.vtube_studio, tx.clone(), boot.state.clone()));
    }
    boot.spawn_admin();
    let addr = boot.me.addr.parse()?;
    let token = boot.state.shutdown.clone();
    tracing::info!(%addr, "avatar gRPC listening");
    tonic::transport::Server::builder()
        .add_service(AvatarServer::new(Svc { tx, metrics: boot.state.metrics.clone() }))
        .serve_with_shutdown(addr, async move { token.cancelled().await })
        .await?;
    tracing::info!("avatar-service stopped");
    Ok(())
}
