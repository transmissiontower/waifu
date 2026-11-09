use std::time::Duration;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

/// Lazily-connected channel: services may start in any order and reconnect automatically.
pub fn channel(addr: &str) -> Channel {
    let uri = if addr.starts_with("http") { addr.to_string() } else { format!("http://{addr}") };
    Endpoint::from_shared(uri)
        .expect("valid endpoint")
        .connect_timeout(Duration::from_secs(2))
        .tcp_nodelay(true)
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_while_idle(true)
        .connect_lazy()
}

/// Wrap a message in a request carrying the trace id and a deadline.
pub fn traced<T>(msg: T, trace_id: &str, timeout: Duration) -> Request<T> {
    let mut r = Request::new(msg);
    if let Ok(v) = trace_id.parse() {
        r.metadata_mut().insert(contracts::TRACE_HEADER, v);
    }
    r.set_timeout(timeout);
    r
}

/// Extract the trace id from an incoming request (or mint one).
pub fn trace_id<T>(req: &Request<T>) -> String {
    req.metadata()
        .get(contracts::TRACE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(crate::telemetry::new_trace_id)
}

pub fn unavailable(msg: impl Into<String>) -> Status {
    Status::unavailable(msg.into())
}
