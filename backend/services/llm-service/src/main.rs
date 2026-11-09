//! llm-service: streaming chat + embeddings over Ollama (default), any OpenAI-compatible API,
//! with an automatic built-in fallback so the companion keeps talking when the model is down.
mod fallback;
mod providers;

use common::{resilience::CircuitBreaker, Boot};
use contracts::llm_server::{Llm, LlmServer};
use contracts::*;
use providers::{Settings, Shared};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

struct Svc {
    shared: Arc<Shared>,
}

#[tonic::async_trait]
impl Llm for Svc {
    type ChatStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ChatToken, Status>> + Send>>;

    async fn chat(&self, req: Request<ChatRequest>) -> Result<Response<Self::ChatStream>, Status> {
        let trace = common::grpc::trace_id(&req);
        let (tx, rx) = mpsc::channel(64);
        let shared = self.shared.clone();
        let msg = req.into_inner();
        shared.state.metrics.inc("chat_requests_total");
        let span = tracing::info_span!("llm.chat", trace_id = %trace);
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                let t0 = std::time::Instant::now();
                providers::run_chat(shared.clone(), msg, tx).await;
                shared.state.metrics.observe("chat", t0.elapsed().as_secs_f64());
            },
            span,
        ));
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }

    async fn embed(&self, req: Request<EmbedRequest>) -> Result<Response<EmbedResponse>, Status> {
        self.shared.state.metrics.inc("embed_requests_total");
        providers::embed(&self.shared, req.into_inner().inputs).await.map(Response::new)
    }

    async fn info(&self, _: Request<Empty>) -> Result<Response<LlmInfo>, Status> {
        Ok(Response::new(providers::info(&self.shared).await))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = Boot::init("llm")?;
    let mut settings: Settings = boot.cfg.settings("llm");
    if let Ok(url) = std::env::var("WAIFU_OLLAMA_URL") {
        settings.ollama.base_url = url; // e.g. docker-compose: http://ollama:11434
    }
    tracing::info!(providers = ?settings.providers, "provider chain");
    let breakers = settings
        .providers
        .iter()
        .map(|p| (p.clone(), CircuitBreaker::new(p, settings.breaker.threshold, Duration::from_secs(settings.breaker.cooldown_s))))
        .collect();
    let shared = Arc::new(Shared::new(settings, breakers, boot.state.clone()));
    boot.spawn_admin();
    let addr = boot.me.addr.parse()?;
    let token = boot.state.shutdown.clone();
    tracing::info!(%addr, "llm gRPC listening");
    tonic::transport::Server::builder()
        .add_service(LlmServer::new(Svc { shared }))
        .serve_with_shutdown(addr, async move { token.cancelled().await })
        .await?;
    tracing::info!("llm-service stopped");
    Ok(())
}
