use tracing_subscriber::{fmt, EnvFilter};

/// Structured logging. `LOG_FORMAT=json` switches to JSON lines. Spans carry `trace_id`.
pub fn init(role: &str, default_level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let json = std::env::var("LOG_FORMAT").map(|v| v == "json").unwrap_or(false);
    let _ = if json {
        fmt().with_env_filter(filter).json().with_current_span(true).try_init()
    } else {
        fmt().with_env_filter(filter).with_ansi(false).with_target(false).compact().try_init()
    };
    tracing::debug!(role, "telemetry ready");
}

/// Resolves on Ctrl-C or SIGTERM.
pub async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("sigterm");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutdown signal received");
}

pub fn new_trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}
