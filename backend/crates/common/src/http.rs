use crate::Metrics;
use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Per-service runtime state shared with the admin HTTP server.
#[derive(Clone)]
pub struct ServiceState {
    pub name: String,
    pub started: Instant,
    pub metrics: Arc<Metrics>,
    pub shutdown: CancellationToken,
    degraded: Arc<Mutex<BTreeMap<String, String>>>,
}

impl ServiceState {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            started: Instant::now(),
            metrics: Arc::new(Metrics::default()),
            shutdown: CancellationToken::new(),
            degraded: Default::default(),
        }
    }
    /// Mark a capability as degraded (service stays up, health reports "degraded").
    pub fn degrade(&self, key: &str, reason: impl Into<String>) {
        let reason = reason.into();
        let mut d = self.degraded.lock().unwrap();
        if d.insert(key.into(), reason.clone()).as_deref() != Some(reason.as_str()) {
            tracing::warn!(key, %reason, "degraded");
        }
    }
    pub fn recover(&self, key: &str) {
        if self.degraded.lock().unwrap().remove(key).is_some() {
            tracing::info!(key, "recovered");
        }
    }
    pub fn health(&self) -> serde_json::Value {
        let d = self.degraded.lock().unwrap().clone();
        json!({
            "service": self.name,
            "status": if d.is_empty() { "ok" } else { "degraded" },
            "uptime_s": self.started.elapsed().as_secs(),
            "degraded": d,
        })
    }
}

/// Routes shared by every service: /healthz /readyz /metrics and POST /admin/shutdown (loopback only).
pub fn admin_router(state: ServiceState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/admin/shutdown", post(shutdown))
        .with_state(state)
}

async fn healthz(State(s): State<ServiceState>) -> Json<serde_json::Value> {
    Json(s.health())
}
async fn metrics(State(s): State<ServiceState>) -> String {
    s.metrics.set("uptime_seconds", s.started.elapsed().as_secs_f64());
    s.metrics.render(&s.name)
}
async fn shutdown(State(s): State<ServiceState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> StatusCode {
    if !peer.ip().is_loopback() {
        return StatusCode::FORBIDDEN;
    }
    tracing::info!("admin shutdown requested");
    s.shutdown.cancel();
    StatusCode::ACCEPTED
}

pub async fn serve_admin(addr: &str, state: ServiceState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "admin http (healthz, metrics) listening");
    let token = state.shutdown.clone();
    axum::serve(listener, admin_router(state).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { token.cancelled().await })
        .await?;
    Ok(())
}
