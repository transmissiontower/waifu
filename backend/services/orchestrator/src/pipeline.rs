use crate::tags::{Piece, Segmenter};
use crate::{Ctx, Session};
use common::audio;
use common::bus::Event;
use common::grpc;
use common::telemetry::new_trace_id;
use contracts::server_event::Kind as SK;
use contracts::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub enum Input {
    Text(String),
    /// Speak the persona's greeting (no LLM call).
    Greeting,
}

enum Item {
    Emotion(String),
    Speak(String),
}

const DEFAULT_EMOTIONS: [&str; 8] = ["neutral", "happy", "sad", "angry", "surprised", "shy", "thinking", "smug"];

fn fallback_prompt(name: &str, emotions: &[String]) -> String {
    format!(
        "You are {name}, a companion in a spoken conversation. Reply in one to three short sentences, plain spoken words only.\n\
CRITICAL: Every single sentence MUST start with an emotion tag in square brackets from: {}.\n\
Example: \"[smug] Oh, really? [thinking] Let me think.\"",
        emotions.iter().map(|e| format!("[{e}]")).collect::<Vec<_>>().join(" ")
    )
}

/// speech-to-text for a finished utterance, then start a turn with the transcript.
pub async fn stt_flow(ctx: Arc<Ctx>, sess: Arc<Session>, pcm: Vec<u8>) {
    let trace = new_trace_id();
    let span = tracing::info_span!("stt_flow", trace_id = %trace);
    async move {
        if pcm.len() < 3200 {
            sess.send(&trace, SK::State("idle".into())).await;
            return;
        }
        sess.send(&trace, SK::State("thinking".into())).await;
        ctx.avatar(&trace, avatar_event::Kind::State(StateEvent { state: "thinking".into() }));
        let Some(stt) = ctx.c.stt.clone() else {
            sess.notice(
                &trace,
                "stt",
                "error",
                "Speech recognition is disabled (stt.enabled in config/settings.yaml) - please type your message instead.",
            )
            .await;
            sess.send(&trace, SK::State("idle".into())).await;
            return;
        };
        let req = TranscribeRequest { pcm16: pcm, sample_rate: 16000, language: String::new() };
        let mut stt = stt;
        let d = Duration::from_secs(ctx.cfg.stt_s);
        let r = ctx.guarded(&ctx.br.stt, d, || stt.transcribe(grpc::traced(req, &trace, d))).await;
        match r {
            Ok(t) if !t.text.trim().is_empty() => {
                ctx.state.recover("stt");
                sess.send(&trace, SK::Transcript(TranscriptEvent { text: t.text.clone(), r#final: true })).await;
                crate::start_turn(&ctx, &sess, Input::Text(t.text));
            }
            Ok(_) => {
                sess.notice(&trace, "stt_empty", "info", "I didn't catch that - could you say it again?").await;
                sess.send(&trace, SK::State("idle".into())).await;
            }
            Err(e) => {
                ctx.state.degrade("stt", e.clone());
                sess.notice(&trace, "stt", "error", format!("Voice input is unavailable ({e}). You can type to me instead.")).await;
                sess.send(&trace, SK::State("idle".into())).await;
            }
        }
    }
    .instrument(span)
    .await
}

pub async fn run_turn(ctx: Arc<Ctx>, sess: Arc<Session>, input: Input, cancel: CancellationToken) {
    let trace = new_trace_id();
    let span = tracing::info_span!("turn", trace_id = %trace);
    run_turn_inner(ctx, sess, input, cancel, trace).instrument(span).await
}

async fn run_turn_inner(ctx: Arc<Ctx>, sess: Arc<Session>, input: Input, cancel: CancellationToken, trace: String) {
    let t0 = Instant::now();
    let _turn = ctx.activity.begin_turn(); // background LLM work (fact extraction) waits until this turn is over
    ctx.state.metrics.inc("turns_total");
    sess.send(&trace, SK::State("thinking".into())).await;
    ctx.avatar(&trace, avatar_event::Kind::State(StateEvent { state: "thinking".into() }));
    let (user_id, user_name) = sess.user();
    let user_text = match &input {
        Input::Text(t) => {
            ctx.bus.publish(Event::UserSpoke { trace_id: trace.clone(), text: t.clone() });
            t.clone()
        }
        Input::Greeting => String::new(),
    };

    // 1. memory recall (optional: chat continues without it)
    let recall = recall(&ctx, &sess, &trace, &user_id, &user_text).await;
    if sess.history.lock().unwrap().is_empty() && !recall.recent.is_empty() {
        *sess.history.lock().unwrap() = recall.recent.clone(); // resume after restart
    }

    // 2. persona prompt (optional: built-in default if persona-service is down)
    let emotion_now = sess.emotion.lock().unwrap().clone();
    let (system_prompt, info) = persona(&ctx, &sess, &trace, &user_name, &recall, &emotion_now).await;
    let emotions: Vec<String> = info
        .as_ref()
        .map(|i| i.emotions.clone())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| DEFAULT_EMOTIONS.iter().map(|s| s.to_string()).collect());
    if let Some(i) = &info {
        *sess.voice.lock().unwrap() = i.voice.clone();
    }

    // 3. speaking pipeline: segmenter -> TTS worker
    let (items_tx, items_rx) = mpsc::channel::<Item>(32);
    let worker = tokio::spawn(tts_worker(ctx.clone(), sess.clone(), trace.clone(), cancel.clone(), items_rx, t0).in_current_span());
    let mut speaker = Speaker { seg: Segmenter::new(&emotions), items: items_tx, sess: sess.clone(), trace: trace.clone() };

    let mut interrupted = false;
    let mut llm_failed = false;
    match input {
        Input::Greeting => {
            let g = info
                .as_ref()
                .map(|i| i.greeting.clone())
                .filter(|g| !g.is_empty())
                .unwrap_or_else(|| "[neutral] Hello! I'm glad you're here.".into());
            let g = g.trim();
            if g.starts_with('[') {
                speaker.feed(g).await;
            } else {
                speaker.feed(&format!("[neutral] {g}")).await;
            }
            speaker.finish().await;
        }
        Input::Text(text) => {
            let mut messages = vec![ChatMessage { role: "system".into(), content: system_prompt }];
            {
                let h = sess.history.lock().unwrap();
                let skip = h.len().saturating_sub(ctx.cfg.history_messages);
                messages.extend(h.iter().skip(skip).cloned());
            }
            // 2b. internet (optional: only when the message needs fresh info; the results ride on this one request,
            // history and memory below keep the plain `text`)
            let llm_text = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                r = ctx.web.augment(&text) => r,
            }
            .unwrap_or_else(|| text.clone());
            messages.push(ChatMessage { role: "user".into(), content: llm_text });
            match llm_phase(&ctx, &sess, &trace, &cancel, messages, &mut speaker).await {
                Ok(()) => {}
                Err(LlmEnd::Cancelled) => interrupted = true,
                Err(LlmEnd::Failed(e)) => {
                    llm_failed = true;
                    ctx.state.degrade("llm", e.clone());
                    sess.notice(&trace, "llm_down", "error", format!("The language model is unavailable: {e}. Start Ollama (`ollama serve`) or change the llm section of config/settings.yaml.")).await;
                    speaker.feed("[sad] Sorry, I can't think straight right now. Could you try again in a moment?").await;
                }
            }
            if !interrupted {
                speaker.finish().await;
            }
            let reply = if llm_failed { String::new() } else { speaker.seg.clean_total.trim().to_string() };
            // history + long-term memory (also stores what was said before an interruption)
            if !reply.is_empty() {
                let mut h = sess.history.lock().unwrap();
                h.push(ChatMessage { role: "user".into(), content: text.clone() });
                h.push(ChatMessage { role: "assistant".into(), content: reply.clone() });
                let excess = h.len().saturating_sub(40);
                h.drain(..excess);
            }
            store_turns(&ctx, &trace, &user_id, &text, &reply);
            if !reply.is_empty() && !interrupted {
                ctx.bus.publish(Event::TurnCompleted {
                    trace_id: trace.clone(),
                    user_id: user_id.clone(),
                    user_text: text,
                    assistant_text: reply,
                });
            }
        }
    }
    drop(speaker); // closes the TTS queue
    if interrupted || cancel.is_cancelled() {
        worker.abort();
        tracing::info!("turn interrupted");
        ctx.bus.publish(Event::Interrupted { trace_id: trace.clone() });
        return; // new state is announced by whoever interrupted
    }
    let _ = worker.await;
    ctx.state.metrics.observe("turn", t0.elapsed().as_secs_f64());
    sess.send(&trace, SK::State("idle".into())).await;
    ctx.avatar(&trace, avatar_event::Kind::State(StateEvent { state: "idle".into() }));
    sess.send(&trace, SK::TurnDone(Empty {})).await;
    tracing::info!(ms = t0.elapsed().as_millis() as u64, "turn complete");
}

struct Speaker {
    seg: Segmenter,
    items: mpsc::Sender<Item>,
    sess: Arc<Session>,
    trace: String,
}

impl Speaker {
    async fn feed(&mut self, delta: &str) {
        let pieces = self.seg.push(delta);
        self.handle(pieces).await;
    }
    async fn finish(&mut self) {
        let pieces = self.seg.finish();
        self.handle(pieces).await;
    }
    async fn handle(&mut self, pieces: Vec<Piece>) {
        for p in pieces {
            match p {
                Piece::Emotion(e) => {
                    let _ = self.items.send(Item::Emotion(e)).await;
                }
                Piece::Sentence(s) => {
                    // text goes to the screen immediately; audio follows via the TTS queue
                    self.sess.send(&self.trace, SK::AssistantDelta(format!("{s} "))).await;
                    let _ = self.items.send(Item::Speak(s)).await;
                }
            }
        }
    }
}

async fn recall(ctx: &Arc<Ctx>, sess: &Session, trace: &str, user_id: &str, query: &str) -> RecallResponse {
    let Some(m) = ctx.c.memory.clone() else { return RecallResponse::default() };
    // One short attempt, no retry/sleep: memory must never delay the reply (a missed recall costs one turn of context).
    let d = Duration::from_millis(ctx.cfg.memory_ms);
    let mut m = m;
    let req = RecallRequest { user_id: user_id.into(), query: query.into(), top_k: ctx.cfg.recall_top_k, recent: 12 };
    match ctx.guarded(&ctx.br.memory, d, || m.recall(grpc::traced(req, trace, d))).await {
        Ok(r) => {
            ctx.state.recover("memory");
            r
        }
        Err(e) => {
            let slow = e.starts_with("timed out");
            ctx.state.degrade("memory", e);
            if !slow {
                // a mere slow answer is not "offline": no alarming notice for that
                sess.notice(
                    trace,
                    "memory",
                    "warn",
                    "My memory is offline right now - I'll keep chatting, but I won't remember this conversation until it's back.",
                )
                .await;
            }
            RecallResponse::default()
        }
    }
}

async fn persona(
    ctx: &Arc<Ctx>,
    sess: &Session,
    trace: &str,
    user_name: &str,
    r: &RecallResponse,
    emotion: &str,
) -> (String, Option<PersonaInfo>) {
    let Some(p) = ctx.c.persona.clone() else {
        return (fallback_prompt(&ctx.cfg.fallback_persona_name, &DEFAULT_EMOTIONS.map(String::from)), None);
    };
    let d = Duration::from_millis(ctx.cfg.persona_ms);
    let mut p = p;
    let req = BuildPromptRequest {
        user_name: user_name.into(),
        summary: r.summary.clone(),
        facts: r.facts.clone(),
        memories: r.memories.clone(),
        current_emotion: emotion.into(),
    };
    match ctx.guarded(&ctx.br.persona, d, || p.build_prompt(grpc::traced(req, trace, d))).await {
        Ok(resp) => {
            ctx.state.recover("persona");
            (resp.system_prompt, resp.info)
        }
        Err(e) => {
            ctx.state.degrade("persona", e);
            sess.notice(trace, "persona", "warn", "The persona service is unreachable - using a default personality for now.").await;
            (fallback_prompt(&ctx.cfg.fallback_persona_name, &DEFAULT_EMOTIONS.map(String::from)), None)
        }
    }
}

enum LlmEnd {
    Cancelled,
    Failed(String),
}

async fn llm_phase(
    ctx: &Arc<Ctx>,
    sess: &Session,
    trace: &str,
    cancel: &CancellationToken,
    messages: Vec<ChatMessage>,
    speaker: &mut Speaker,
) -> Result<(), LlmEnd> {
    let Some(llm) = ctx.c.llm.clone() else {
        return Err(LlmEnd::Failed("llm service disabled (llm.enabled in config/settings.yaml)".into()));
    };
    let mut llm = llm;
    let req = ChatRequest { messages, temperature: 0.0, max_tokens: 0, model: String::new() };
    let mut stream = ctx
        .guarded(&ctx.br.llm, Duration::from_secs(10), || llm.chat(grpc::traced(req, trace, Duration::from_secs(300))))
        .await
        .map_err(LlmEnd::Failed)?;
    let mut first = true;
    loop {
        let wait = Duration::from_secs(if first { ctx.cfg.llm_first_token_s } else { 30 });
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(LlmEnd::Cancelled),
            r = tokio::time::timeout(wait, stream.message()) => r,
        };
        match item {
            Err(_) => {
                ctx.br.llm.record(false);
                return Err(LlmEnd::Failed(format!("no response from the language model within {wait:?}")));
            }
            Ok(Err(status)) => {
                ctx.br.llm.record(false);
                return Err(LlmEnd::Failed(status.message().to_string()));
            }
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(tok))) => {
                if first {
                    first = false;
                    ctx.state.metrics.observe("first_token", 0.0);
                }
                if !tok.notice.is_empty() {
                    sess.notice(trace, "llm_notice", "warn", tok.notice.clone()).await;
                }
                if tok.provider == "fallback" {
                    ctx.state.degrade("llm", "using built-in fallback brain");
                } else if !tok.provider.is_empty() {
                    ctx.state.recover("llm");
                }
                if !tok.text.is_empty() {
                    speaker.feed(&tok.text).await;
                }
                if tok.done {
                    return Ok(());
                }
            }
        }
    }
}

fn store_turns(ctx: &Arc<Ctx>, trace: &str, user_id: &str, user_text: &str, reply: &str) {
    let Some(m) = ctx.c.memory.clone() else { return };
    let (ctx, trace, user_id) = (ctx.clone(), trace.to_string(), user_id.to_string());
    let (u, a) = (user_text.to_string(), reply.to_string());
    tokio::spawn(async move {
        for (role, text) in [("user", u), ("assistant", a)] {
            if text.is_empty() {
                continue;
            }
            let mut m = m.clone();
            let req = StoreTurnRequest { user_id: user_id.clone(), role: role.into(), text };
            let d = Duration::from_secs(5);
            if let Err(e) = ctx.guarded(&ctx.br.memory, d, || m.store_turn(grpc::traced(req, &trace, d))).await {
                tracing::warn!(%trace, error = %e, "could not store turn");
                ctx.state.degrade("memory", e);
                return;
            }
        }
    });
}

async fn tts_worker(
    ctx: Arc<Ctx>,
    sess: Arc<Session>,
    trace: String,
    cancel: CancellationToken,
    mut rx: mpsc::Receiver<Item>,
    t0: Instant,
) {
    let mut speaking = false;
    let mut first_audio = true;
    let mut audio_ms: u64 = 0;
    let mut started: Option<Instant> = None;
    let fps = ctx.cfg.lipsync_fps.max(10);
    while let Some(item) = rx.recv().await {
        if cancel.is_cancelled() {
            return;
        }
        match item {
            Item::Emotion(e) => {
                *sess.emotion.lock().unwrap() = e.clone();
                sess.send(&trace, SK::Emotion(EmotionOut { name: e.clone(), intensity: 1.0 })).await;
                ctx.bus.publish(Event::EmotionChanged { trace_id: trace.clone(), emotion: e });
            }
            Item::Speak(text) => {
                if !speaking {
                    speaking = true;
                    sess.send(&trace, SK::State("speaking".into())).await;
                    ctx.avatar(&trace, avatar_event::Kind::State(StateEvent { state: "speaking".into() }));
                }
                let Some(tts) = ctx.c.tts.clone() else {
                    sess.notice(
                        &trace,
                        "tts",
                        "warn",
                        "Voice output is disabled (tts.enabled in config/settings.yaml) - showing text only.",
                    )
                    .await;
                    continue;
                };
                let voice = sess.voice.lock().unwrap().clone();
                let emotion = sess.emotion.lock().unwrap().clone();
                let req = SynthRequest { text, voice, speed: 0.0, emotion };
                let mut tts = tts;
                let d = Duration::from_secs(ctx.cfg.tts_open_s);
                let opened = ctx.guarded(&ctx.br.tts, d, || tts.synthesize(grpc::traced(req, &trace, Duration::from_secs(60)))).await;
                let mut stream = match opened {
                    Ok(s) => s,
                    Err(e) => {
                        ctx.state.degrade("tts", e.clone());
                        sess.notice(&trace, "tts", "warn", format!("My voice is unavailable ({e}) - showing text only.")).await;
                        continue;
                    }
                };
                loop {
                    let m = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return,
                        m = tokio::time::timeout(Duration::from_secs(20), stream.message()) => m,
                    };
                    match m {
                        Ok(Ok(Some(chunk))) => {
                            ctx.state.recover("tts");
                            if chunk.pcm16.is_empty() {
                                continue;
                            }
                            if first_audio {
                                first_audio = false;
                                ctx.state.metrics.observe("first_audio", t0.elapsed().as_secs_f64());
                                tracing::info!(ms = t0.elapsed().as_millis() as u64, "first audio");
                            }
                            let pcm = audio::bytes_to_i16(&chunk.pcm16);
                            let dur_ms = pcm.len() as u64 * 1000 / chunk.sample_rate.max(1) as u64;
                            let begin = *started.get_or_insert_with(Instant::now);
                            let lead = audio_ms.saturating_sub(begin.elapsed().as_millis() as u64);
                            ctx.avatar(
                                &trace,
                                avatar_event::Kind::Lipsync(LipsyncEvent {
                                    start_ms: lead as u32,
                                    fps,
                                    mouth: audio::mouth_envelope(&pcm, chunk.sample_rate, fps),
                                }),
                            );
                            audio_ms += dur_ms;
                            sess.send(&trace, SK::Audio(chunk)).await;
                        }
                        Ok(Ok(None)) => break,
                        Ok(Err(s)) => {
                            ctx.br.tts.record(false);
                            ctx.state.degrade("tts", s.message().to_string());
                            sess.notice(&trace, "tts", "warn", format!("My voice stopped working ({}) - showing text only.", s.message()))
                                .await;
                            break;
                        }
                        Err(_) => {
                            ctx.br.tts.record(false);
                            sess.notice(&trace, "tts", "warn", "My voice timed out - showing text only.").await;
                            break;
                        }
                    }
                }
            }
        }
    }
}
