//! gateway: the only public entry point. Serves the frontend, a WebSocket for the conversation,
//! and a small JSON API (status, persona, avatars, voices). Routes everything else over gRPC.
mod ws;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use common::{grpc, Boot, Config};
use contracts::avatar_client::AvatarClient;
use contracts::persona_client::PersonaClient;
use contracts::tts_client::TtsClient;
use contracts::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tonic::transport::Channel;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Settings {
    static_dir: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self { static_dir: "frontend/dist".into() }
    }
}

#[derive(Clone)]
pub struct App {
    pub cfg: Arc<Config>,
    pub orch: Channel,
    pub persona: Option<PersonaClient<Channel>>,
    pub tts: Option<TtsClient<Channel>>,
    pub avatar_connected: Arc<AtomicBool>,
    /// avatar-service events (as JSON text) fanned out to every WebSocket client
    pub avatar_tx: broadcast::Sender<String>,
    /// persona hot-reload notifications (JSON text)
    pub persona_tx: broadcast::Sender<String>,
    pub persona_now: Arc<Mutex<Option<PersonaInfo>>>,
    pub http: reqwest::Client,
    pub state: common::ServiceState,
}

pub fn persona_json(p: &PersonaInfo) -> Value {
    json!({"type":"persona","name":p.name,"age":p.age,"greeting":p.greeting,"voice":p.voice,"avatar":p.avatar,"language":p.language,"emotions":p.emotions,"version":p.version})
}

async fn api_status(State(app): State<App>) -> Json<Value> {
    let futs = app.cfg.services.iter().map(|(role, s)| {
        let (role, s, http) = (role.clone(), s.clone(), app.http.clone());
        async move {
            if !s.enabled {
                return (role, json!({"status":"disabled","impl":s.implementation,"name":s.name}));
            }
            let mut v = if let (Some(h), "builtin") = (&s.http, s.implementation.as_str()) {
                match http.get(format!("http://{h}/healthz")).send().await {
                    Ok(r) => r.json::<Value>().await.unwrap_or(json!({"status":"unknown"})),
                    Err(_) => json!({"status":"down"}),
                }
            } else if role == "gateway" {
                json!({"status":"ok"})
            } else {
                // external implementation: only a TCP probe is possible
                match tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(&s.addr)).await {
                    Ok(Ok(_)) => json!({"status":"ok"}),
                    _ => json!({"status":"down"}),
                }
            };
            v["impl"] = json!(s.implementation);
            v["name"] = json!(s.name);
            v["addr"] = json!(s.addr);
            (role, v)
        }
    });
    let results = futures::future::join_all(futs).await;
    let all_ok = results.iter().all(|(_, v)| matches!(v["status"].as_str(), Some("ok") | Some("disabled")));
    let map: serde_json::Map<String, Value> = results.into_iter().collect();
    Json(json!({"overall": if all_ok {"ok"} else {"degraded"}, "services": map}))
}

async fn api_persona(State(app): State<App>) -> impl IntoResponse {
    match app.persona_now.lock().unwrap().clone() {
        Some(p) => Json(persona_json(&p)).into_response(),
        None => (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"persona-service unavailable"}))).into_response(),
    }
}

async fn api_avatars(State(app): State<App>) -> Json<Value> {
    let mut files: Vec<String> = std::fs::read_dir(&app.cfg.global.avatars_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.to_lowercase().ends_with(".vrm"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    Json(json!({"avatars": files}))
}

async fn api_voices(State(app): State<App>) -> impl IntoResponse {
    let Some(mut tts) = app.tts.clone() else { return Json(json!({"voices": [], "error": "tts disabled"})) };
    match tokio::time::timeout(Duration::from_secs(2), tts.list_voices(grpc::traced(Empty {}, "", Duration::from_secs(2)))).await {
        Ok(Ok(r)) => {
            let r = r.into_inner();
            Json(json!({"voices": r.voices, "default": r.default_voice, "engine": r.engine}))
        }
        _ => Json(json!({"voices": [], "error": "tts-service unreachable"})),
    }
}

/// Keep a subscription to avatar-service and fan its events out to browsers.
async fn avatar_bridge(app: App) {
    let Some(addr) = app.cfg.addr("avatar").ok().filter(|_| app.cfg.is_enabled("avatar")) else { return };
    let mut client = AvatarClient::new(grpc::channel(&addr));
    let mut backoff = Duration::from_millis(500);
    loop {
        if app.state.shutdown.is_cancelled() {
            return;
        }
        match client.subscribe(Empty {}).await {
            Ok(resp) => {
                app.avatar_connected.store(true, std::sync::atomic::Ordering::SeqCst);
                app.state.recover("avatar");
                backoff = Duration::from_millis(500);
                let mut s = resp.into_inner();
                loop {
                    tokio::select! {
                        _ = app.state.shutdown.cancelled() => return,
                        m = s.message() => match m {
                            Ok(Some(ev)) => { if let Some(j) = ws::avatar_event_json(&ev) { let _ = app.avatar_tx.send(j); } }
                            _ => break,
                        }
                    }
                }
            }
            Err(e) => app.state.degrade("avatar", format!("avatar-service unreachable: {}", e.message())),
        }
        app.avatar_connected.store(false, std::sync::atomic::Ordering::SeqCst);
        tokio::select! {
            _ = app.state.shutdown.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// Track persona hot reloads and tell every browser (new name/avatar/voice without refresh).
async fn persona_bridge(app: App) {
    let Some(mut client) = app.persona.clone() else { return };
    let mut backoff = Duration::from_millis(500);
    loop {
        if app.state.shutdown.is_cancelled() {
            return;
        }
        if let Ok(resp) = client.watch(Empty {}).await {
            backoff = Duration::from_millis(500);
            let mut s = resp.into_inner();
            loop {
                tokio::select! {
                    _ = app.state.shutdown.cancelled() => return,
                    m = s.message() => match m {
                        Ok(Some(p)) => {
                            *app.persona_now.lock().unwrap() = Some(p.clone());
                            let _ = app.persona_tx.send(persona_json(&p).to_string());
                        }
                        _ => break,
                    }
                }
            }
        }
        tokio::select! {
            _ = app.state.shutdown.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = Boot::init("gateway")?;
    let st: Settings = boot.cfg.settings("gateway");
    let cfg = Arc::new(boot.cfg.clone());
    let mk = |role: &str| cfg.addr(role).ok().filter(|_| cfg.is_enabled(role)).map(|a| grpc::channel(&a));
    let app = App {
        orch: mk("orchestrator").unwrap_or_else(|| grpc::channel("127.0.0.1:1")),
        persona: mk("persona").map(PersonaClient::new),
        tts: mk("tts").map(TtsClient::new),
        avatar_connected: Arc::new(AtomicBool::new(false)),
        avatar_tx: broadcast::channel(256).0,
        persona_tx: broadcast::channel(16).0,
        persona_now: Default::default(),
        http: reqwest::Client::builder().timeout(Duration::from_millis(800)).build()?,
        state: boot.state.clone(),
        cfg: cfg.clone(),
    };
    tokio::spawn(avatar_bridge(app.clone()));
    tokio::spawn(persona_bridge(app.clone()));

    let index = format!("{}/index.html", st.static_dir);
    let router = Router::new()
        .route("/ws", get(ws::handler))
        .route("/api/status", get(api_status))
        .route("/api/persona", get(api_persona))
        .route("/api/avatars", get(api_avatars))
        .route("/api/voices", get(api_voices))
        .nest_service("/avatars", ServeDir::new(&cfg.global.avatars_dir))
        .fallback_service(ServeDir::new(&st.static_dir).fallback(ServeFile::new(index)))
        .with_state(app)
        .merge(common::http::admin_router(boot.state.clone()));

    let addr: SocketAddr = boot.me.addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "gateway listening - open http://{addr}");
    let token = boot.state.shutdown.clone();
    tokio::spawn({
        let t = token.clone();
        async move {
            common::telemetry::wait_for_signal().await;
            t.cancel();
        }
    });
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { token.cancelled().await })
        .await?;
    tracing::info!("gateway stopped");
    Ok(())
}
