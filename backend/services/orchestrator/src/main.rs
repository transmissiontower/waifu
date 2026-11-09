//! orchestrator: runs the conversation pipeline
//! STT -> memory recall -> persona prompt -> LLM stream -> emotion tags -> sentence TTS -> avatar events,
//! with streaming, barge-in (interruption) and graceful degradation at every hop.
mod pipeline;
mod tags;
mod websearch;

use common::bus::{Event, EventBus};
use common::resilience::{BreakerError, CircuitBreaker};
use common::{grpc, Boot, ServiceState};
use contracts::avatar_client::AvatarClient;
use contracts::client_event::Kind as CK;
use contracts::llm_client::LlmClient;
use contracts::memory_client::MemoryClient;
use contracts::orchestrator_server::{Orchestrator, OrchestratorServer};
use contracts::persona_client::PersonaClient;
use contracts::server_event::Kind as SK;
use contracts::stt_client::SttClient;
use contracts::tts_client::TtsClient;
use contracts::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub history_messages: usize,
    pub recall_top_k: u32,
    pub memory_ms: u64,
    pub persona_ms: u64,
    pub stt_s: u64,
    pub llm_first_token_s: u64,
    pub tts_open_s: u64,
    /// at start-up: load the LLM and prefill its cache with the persona prompt, so the first reply is not slow
    pub prewarm: bool,
    pub breaker_threshold: u32,
    pub breaker_cooldown_s: u64,
    pub lipsync_fps: u32,
    pub fallback_persona_name: String,
    pub user_name: String,
    /// Internet access (see `web_search:` in config/settings.yaml)
    pub web_search: websearch::WebSearchCfg,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            history_messages: 16,
            recall_top_k: 4,
            memory_ms: 400,
            persona_ms: 300,
            stt_s: 30,
            llm_first_token_s: 90,
            tts_open_s: 10,
            prewarm: true,
            breaker_threshold: 3,
            breaker_cooldown_s: 10,
            lipsync_fps: 50,
            fallback_persona_name: "Mio".into(),
            user_name: String::new(),
            web_search: websearch::WebSearchCfg::default(),
        }
    }
}

pub struct Clients {
    pub llm: Option<LlmClient<Channel>>,
    pub memory: Option<MemoryClient<Channel>>,
    pub persona: Option<PersonaClient<Channel>>,
    pub tts: Option<TtsClient<Channel>>,
    pub stt: Option<SttClient<Channel>>,
    pub avatar: Option<AvatarClient<Channel>>,
}

pub struct Breakers {
    pub llm: CircuitBreaker,
    pub memory: CircuitBreaker,
    pub persona: CircuitBreaker,
    pub tts: CircuitBreaker,
    pub stt: CircuitBreaker,
    pub avatar: CircuitBreaker,
}

pub struct Ctx {
    pub cfg: Settings,
    pub c: Clients,
    pub br: Breakers,
    pub bus: EventBus,
    pub state: ServiceState,
    pub default_user: String,
    pub activity: Arc<Activity>,
    pub web: websearch::WebSearch,
}

/// Tracks whether the user is talking / a reply is being produced, so background work (fact extraction,
/// which is another LLM call) never competes with a live reply for the model.
pub struct Activity {
    turns: AtomicUsize,
    last: Mutex<Instant>,
    pub wake: tokio::sync::Notify,
}

impl Activity {
    fn new() -> Self {
        Self { turns: AtomicUsize::new(0), last: Mutex::new(Instant::now()), wake: tokio::sync::Notify::new() }
    }
    /// Something user-facing just happened (speech started, text arrived, a turn began or ended).
    pub fn touch(&self) {
        *self.last.lock().unwrap() = Instant::now();
        self.wake.notify_waiters();
    }
    /// Zero while a turn is running, else the time since the last activity.
    pub fn idle_for(&self) -> Duration {
        if self.turns.load(Ordering::SeqCst) > 0 {
            Duration::ZERO
        } else {
            self.last.lock().unwrap().elapsed()
        }
    }
    pub fn begin_turn(self: &Arc<Self>) -> TurnGuard {
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.touch();
        TurnGuard(self.clone())
    }
}

pub struct TurnGuard(Arc<Activity>);

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0.turns.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

impl Ctx {
    /// Call a gRPC method under a deadline and a circuit breaker; flattens errors to strings.
    pub async fn guarded<T, F, Fut>(&self, br: &CircuitBreaker, d: Duration, f: F) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Response<T>, Status>>,
    {
        let r = br
            .call(|| async {
                match tokio::time::timeout(d, f()).await {
                    Ok(Ok(r)) => Ok(r.into_inner()),
                    Ok(Err(s)) => Err(s.message().to_string()),
                    Err(_) => Err(format!("timed out after {d:?}")),
                }
            })
            .await;
        match r {
            Ok(v) => Ok(v),
            Err(BreakerError::Open) => Err("service recently failing (circuit open)".into()),
            Err(BreakerError::Inner(e)) => Err(e),
        }
    }

    /// Fire-and-forget avatar event; never blocks or fails the conversation.
    pub fn avatar(&self, trace: &str, kind: avatar_event::Kind) {
        let Some(c) = self.c.avatar.clone() else { return };
        if self.br.avatar.is_open() {
            return;
        }
        let ev = AvatarEvent { trace_id: trace.into(), kind: Some(kind) };
        let trace = trace.to_string();
        let ok = self.br.avatar.is_open();
        let _ = ok;
        tokio::spawn(async move {
            let mut c = c;
            let _ = tokio::time::timeout(Duration::from_millis(500), c.publish(grpc::traced(ev, &trace, Duration::from_millis(500)))).await;
        });
    }
}

/// One connected client conversation (a gateway WebSocket).
pub struct Session {
    pub ids: Mutex<(String, String)>, // (user_id, user_name)
    pub history: Mutex<Vec<ChatMessage>>,
    pub emotion: Mutex<String>,
    pub voice: Mutex<String>,
    pub notices: Mutex<HashMap<String, Instant>>,
    pub audio: Mutex<Vec<u8>>,
    pub turn: Mutex<Option<CancellationToken>>,
    pub listening: AtomicBool,
    pub out: mpsc::Sender<ServerEvent>,
}

impl Session {
    pub fn new(user: &str, name: &str, out: mpsc::Sender<ServerEvent>) -> Self {
        Self {
            ids: Mutex::new((user.into(), name.into())),
            history: Default::default(),
            emotion: Mutex::new("neutral".into()),
            voice: Default::default(),
            notices: Default::default(),
            audio: Default::default(),
            turn: Default::default(),
            listening: AtomicBool::new(false),
            out,
        }
    }
    pub async fn send(&self, trace: &str, kind: SK) {
        let _ = self.out.send(ServerEvent { trace_id: trace.into(), kind: Some(kind) }).await;
    }
    /// Send a notice at most once per minute per key (avoids spamming the user).
    pub async fn notice(&self, trace: &str, key: &str, level: &str, msg: impl Into<String>) {
        {
            let mut n = self.notices.lock().unwrap();
            if let Some(t) = n.get(key) {
                if t.elapsed() < Duration::from_secs(60) {
                    return;
                }
            }
            n.insert(key.into(), Instant::now());
        }
        self.send(trace, SK::Notice(Notice { level: level.into(), message: msg.into() })).await;
    }
    pub fn cancel_turn(&self) -> bool {
        match self.turn.lock().unwrap().take() {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }
    pub fn user(&self) -> (String, String) {
        self.ids.lock().unwrap().clone()
    }
}

struct Svc {
    ctx: Arc<Ctx>,
}

type OutStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ServerEvent, Status>> + Send>>;

#[tonic::async_trait]
impl Orchestrator for Svc {
    type ConverseStream = OutStream;

    async fn converse(&self, req: Request<tonic::Streaming<ClientEvent>>) -> Result<Response<OutStream>, Status> {
        let mut inbound = req.into_inner();
        let (tx, rx) = mpsc::channel::<ServerEvent>(512);
        let ctx = self.ctx.clone();
        ctx.state.metrics.inc("sessions_total");
        tokio::spawn(async move {
            let sess = Arc::new(Session::new(&ctx.default_user, &ctx.cfg.user_name, tx));
            tracing::info!("session opened");
            loop {
                tokio::select! {
                    _ = ctx.state.shutdown.cancelled() => break,
                    msg = inbound.next() => match msg {
                        Some(Ok(ev)) => handle_event(&ctx, &sess, ev).await,
                        _ => break,
                    }
                }
            }
            sess.cancel_turn();
            tracing::info!("session closed");
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok))))
    }
}

pub fn start_turn(ctx: &Arc<Ctx>, sess: &Arc<Session>, input: pipeline::Input) {
    let token = CancellationToken::new();
    {
        let mut t = sess.turn.lock().unwrap();
        if let Some(old) = t.replace(token.clone()) {
            old.cancel(); // a new request supersedes the running one
        }
    }
    ctx.activity.touch();
    let (ctx, sess) = (ctx.clone(), sess.clone());
    tokio::spawn(pipeline::run_turn(ctx, sess, input, token));
}

async fn handle_event(ctx: &Arc<Ctx>, sess: &Arc<Session>, ev: ClientEvent) {
    let Some(kind) = ev.kind else { return };
    match kind {
        CK::Hello(h) => {
            {
                let mut ids = sess.ids.lock().unwrap();
                if !h.user_id.trim().is_empty() {
                    ids.0 = h.user_id.trim().to_string();
                }
                if !h.user_name.trim().is_empty() {
                    ids.1 = h.user_name.trim().to_string();
                }
            }
            tracing::info!(user = %sess.user().0, greet = h.greet, "hello");
            if h.greet {
                start_turn(ctx, sess, pipeline::Input::Greeting);
            }
        }
        CK::UserText(t) => {
            let t = t.trim().to_string();
            if !t.is_empty() {
                start_turn(ctx, sess, pipeline::Input::Text(t));
            }
        }
        CK::UserAudio(chunk) => {
            if !sess.listening.swap(true, Ordering::SeqCst) {
                ctx.activity.touch(); // the user is talking: background LLM work must yield
                sess.cancel_turn(); // barge-in by voice
                sess.send("", SK::State("listening".into())).await;
                ctx.avatar("", avatar_event::Kind::State(StateEvent { state: "listening".into() }));
            }
            let mut a = sess.audio.lock().unwrap();
            if a.len() < 16000 * 2 * 60 {
                a.extend_from_slice(&chunk.pcm16);
            }
        }
        CK::AudioEnd(_) => {
            sess.listening.store(false, Ordering::SeqCst);
            let pcm = std::mem::take(&mut *sess.audio.lock().unwrap());
            let (ctx, sess) = (ctx.clone(), sess.clone());
            tokio::spawn(async move { pipeline::stt_flow(ctx, sess, pcm).await });
        }
        CK::Interrupt(_) => {
            let had = sess.cancel_turn();
            sess.audio.lock().unwrap().clear();
            sess.listening.store(false, Ordering::SeqCst);
            if had {
                ctx.state.metrics.inc("interruptions_total");
                ctx.bus.publish(Event::Interrupted { trace_id: String::new() });
            }
            sess.send("", SK::State("idle".into())).await;
            ctx.avatar("", avatar_event::Kind::State(StateEvent { state: "idle".into() }));
        }
    }
}

/// Fact extraction is a second LLM call. Run it only while nothing else is going on, and drop it (cancelling the
/// call) the moment the user talks again - otherwise it would slow down the very next reply.
async fn extract_when_idle(ctx: Arc<Ctx>, trace_id: String, user_id: String, user_text: String, assistant_text: String) {
    let Some(m) = ctx.c.memory.clone() else { return };
    loop {
        while ctx.activity.idle_for() < Duration::from_secs(2) {
            if ctx.state.shutdown.is_cancelled() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let mut m = m.clone();
        let req = ExtractRequest { user_id: user_id.clone(), user_text: user_text.clone(), assistant_text: assistant_text.clone() };
        let call =
            ctx.guarded(&ctx.br.memory, Duration::from_secs(60), || m.extract_facts(grpc::traced(req, &trace_id, Duration::from_secs(60))));
        tokio::select! {
            r = call => {
                match r {
                    Ok(f) if !f.facts.is_empty() => tracing::info!(%trace_id, facts = ?f.facts.iter().map(|x| format!("{}={}", x.key, x.value)).collect::<Vec<_>>(), "facts learned"),
                    Ok(_) => {}
                    Err(e) => tracing::debug!(%trace_id, error = %e, "fact extraction skipped"),
                }
                return;
            }
            _ = ctx.activity.wake.notified() => {} // user is back: the call is dropped; try again when idle
        }
    }
}

/// At start-up: make the LLM load its weights and prefill its cache with the (real) persona prompt, so the first
/// reply does not pay for model loading and prompt evaluation. Prints `@@loading` / `@@ready` for the loading screen.
async fn prewarm(ctx: Arc<Ctx>) {
    prewarm_inner(&ctx).await;
    println!("@@ready");
}

async fn prewarm_inner(ctx: &Arc<Ctx>) {
    let (Some(persona), Some(llm)) = (ctx.c.persona.clone(), ctx.c.llm.clone()) else { return };
    let t0 = Instant::now();
    let trace = common::telemetry::new_trace_id();
    for _ in 0..12 {
        if ctx.state.shutdown.is_cancelled() {
            return;
        }
        let mut recall = RecallResponse::default();
        if let Some(mut m) = ctx.c.memory.clone() {
            let d = Duration::from_secs(2);
            let req = RecallRequest { user_id: ctx.default_user.clone(), query: String::new(), top_k: 0, recent: 1 };
            if let Ok(Ok(r)) = tokio::time::timeout(d, m.recall(grpc::traced(req, &trace, d))).await {
                recall = r.into_inner();
            }
        }
        let mut p = persona.clone();
        let d = Duration::from_secs(2);
        let req = BuildPromptRequest {
            user_name: ctx.cfg.user_name.clone(),
            summary: recall.summary.clone(),
            facts: recall.facts.clone(),
            memories: vec![],
            current_emotion: "neutral".into(),
        };
        let prompt = match tokio::time::timeout(d, p.build_prompt(grpc::traced(req, &trace, d))).await {
            Ok(Ok(r)) => r.into_inner().system_prompt,
            _ => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let mut l = llm.clone();
        let d = Duration::from_secs(240);
        let req = ChatRequest {
            messages: vec![
                ChatMessage { role: "system".into(), content: prompt },
                ChatMessage { role: "user".into(), content: "Hi".into() },
            ],
            temperature: 0.0,
            max_tokens: 1,
            model: String::new(),
        };
        let run = async {
            let mut s = l.chat(grpc::traced(req, &trace, d)).await?.into_inner();
            while let Some(t) = s.message().await? {
                if t.done {
                    break;
                }
            }
            Ok::<(), Status>(())
        };
        match tokio::time::timeout(d, run).await {
            Ok(Ok(())) => {
                tracing::info!(ms = t0.elapsed().as_millis() as u64, "language model warmed up");
                return;
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await, // llm-service still starting
        }
    }
    tracing::warn!("language model warm-up gave up (first reply may be slow)");
}

/// Reacts to bus events: forwards emotions to the avatar-service and extracts facts after each turn.
async fn reactors(ctx: Arc<Ctx>) {
    let mut rx = ctx.bus.subscribe();
    loop {
        tokio::select! {
            _ = ctx.state.shutdown.cancelled() => return,
            ev = rx.recv() => {
                let ev = match ev { Ok(e) => e, Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue, Err(_) => return };
                match ev {
                    Event::EmotionChanged { trace_id, emotion } => {
                        ctx.avatar(&trace_id, avatar_event::Kind::Emotion(EmotionEvent { name: emotion, intensity: 1.0 }));
                    }
                    Event::TurnCompleted { trace_id, user_id, user_text, assistant_text } => {
                        tokio::spawn(extract_when_idle(ctx.clone(), trace_id, user_id, user_text, assistant_text));
                    }
                    Event::UserSpoke { .. } | Event::Interrupted { .. } | Event::PersonaChanged { .. } => {}
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = Boot::init("orchestrator")?;
    let cfg: Settings = boot.cfg.settings("orchestrator");
    let mk = |role: &str| boot.cfg.addr(role).ok().filter(|_| boot.cfg.is_enabled(role)).map(|a| grpc::channel(&a));
    let c = Clients {
        llm: mk("llm").map(LlmClient::new),
        memory: mk("memory").map(MemoryClient::new),
        persona: mk("persona").map(PersonaClient::new),
        tts: mk("tts").map(TtsClient::new),
        stt: mk("stt").map(SttClient::new),
        avatar: mk("avatar").map(AvatarClient::new),
    };
    // Raise message size limits: audio chunks can be large.
    let mk_br = |n: &str| CircuitBreaker::new(n, cfg.breaker_threshold, Duration::from_secs(cfg.breaker_cooldown_s));
    let br = Breakers {
        llm: mk_br("llm"),
        memory: mk_br("memory"),
        persona: mk_br("persona"),
        tts: mk_br("tts"),
        stt: mk_br("stt"),
        avatar: mk_br("avatar"),
    };
    let bus = EventBus::from_config(&boot.cfg.global).await;
    let web = websearch::WebSearch::new(cfg.web_search.clone());
    let ctx = Arc::new(Ctx {
        cfg,
        c,
        br,
        bus,
        state: boot.state.clone(),
        default_user: boot.cfg.global.default_user.clone(),
        activity: Arc::new(Activity::new()),
        web,
    });
    tokio::spawn(reactors(ctx.clone()));
    if ctx.cfg.prewarm && ctx.c.llm.is_some() {
        println!("@@loading warming up the language model"); // printed before the health endpoint exists
        tokio::spawn(prewarm(ctx.clone()));
    }
    boot.spawn_admin();
    let addr = boot.me.addr.parse()?;
    let token = boot.state.shutdown.clone();
    tracing::info!(%addr, "orchestrator gRPC listening");
    tonic::transport::Server::builder()
        .add_service(OrchestratorServer::new(Svc { ctx }).max_decoding_message_size(16 << 20))
        .serve_with_shutdown(addr, async move { token.cancelled().await })
        .await?;
    tracing::info!("orchestrator stopped");
    Ok(())
}
