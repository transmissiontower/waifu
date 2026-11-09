//! Shared plumbing for all Waifu services: config, telemetry, health/metrics HTTP,
//! gRPC helpers, resilience primitives, event bus and audio utilities.
pub mod audio;
pub mod bus;
pub mod config;
pub mod grpc;
pub mod http;
pub mod metrics;
pub mod resilience;
pub mod telemetry;

pub use config::{Config, ServiceCfg};
pub use http::ServiceState;
pub use metrics::Metrics;
pub use tokio_util::sync::CancellationToken;

/// Boilerplate every service `main` shares: load config, init logging, build state.
pub struct Boot {
    pub cfg: Config,
    pub me: ServiceCfg,
    pub state: ServiceState,
}

impl Boot {
    /// `role` is the key in `config/services.yaml` (e.g. "llm"). `WAIFU_CONFIG` overrides the file path.
    pub fn init(role: &str) -> anyhow::Result<Boot> {
        let cfg = Config::load()?;
        telemetry::init(role, &cfg.global.log_level);
        let me = cfg.service(role)?.clone();
        let state = ServiceState::new(role);
        tracing::info!(role, addr = %me.addr, implementation = ?me.name, "starting");
        Ok(Boot { cfg, me, state })
    }

    /// Spawn the health/metrics/admin HTTP server and wire the shutdown token to signals.
    pub fn spawn_admin(&self) {
        if let Some(addr) = self.me.http.clone() {
            let st = self.state.clone();
            tokio::spawn(async move {
                if let Err(e) = http::serve_admin(&addr, st).await {
                    tracing::error!(error = %e, "admin server failed");
                }
            });
        }
        let token = self.state.shutdown.clone();
        tokio::spawn(async move {
            telemetry::wait_for_signal().await;
            token.cancel();
        });
    }
}
