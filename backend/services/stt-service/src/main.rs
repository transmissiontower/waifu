//! stt-service: speech-to-text with whisper.cpp (whisper-rs).
//! Input: PCM16 mono 16 kHz. If the ggml model is missing the service stays up, reports
//! "degraded" and answers UNAVAILABLE with a clear message so the app falls back to text chat.
use common::{audio, Boot};
use contracts::stt_server::{Stt, SttServer};
use contracts::*;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Settings {
    /// ggml model file name inside the models dir (`WAIFU_STT_MODEL` env overrides).
    model: String,
    language: String,
    threads: i32,
    /// "whisper" or "echo-test" (test-only: treats the audio bytes as UTF-8 text).
    engine: String,
    /// utterances shorter than this are ignored
    min_ms: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self { model: "ggml-base.en.bin".into(), language: "en".into(), threads: 4, engine: "whisper".into(), min_ms: 300 }
    }
}

enum Engine {
    #[cfg(feature = "whisper")]
    Whisper(Mutex<whisper_rs::WhisperContext>),
    EchoTest,
    Unavailable(String),
}

struct Svc {
    engine: Arc<Engine>,
    st: Settings,
    state: common::ServiceState,
}

/// Whisper emits bracketed non-speech tokens and a few classic hallucinations on silence.
pub fn clean_transcript(raw: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in raw.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let t = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = t.to_lowercase();
    if ["thanks for watching", "thank you for watching", "you"].contains(&lower.trim_matches(|c: char| !c.is_alphanumeric() && c != ' ')) {
        return String::new();
    }
    t
}

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        0.0
    } else {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
    }
}

impl Svc {
    #[allow(clippy::result_large_err)]
    fn run(&self, pcm: Vec<i16>, lang: String) -> Result<String, Status> {
        match &*self.engine {
            Engine::Unavailable(why) => Err(Status::unavailable(why.clone())),
            Engine::EchoTest => Ok(String::from_utf8_lossy(&audio::i16_to_bytes(&pcm)).trim_matches('\0').to_string()),
            #[cfg(feature = "whisper")]
            Engine::Whisper(ctx) => {
                use whisper_rs::{FullParams, SamplingStrategy};
                let samples = audio::i16_to_f32(&pcm);
                if rms(&samples) < 0.003 {
                    return Ok(String::new()); // silence
                }
                let ctx = ctx.lock().unwrap();
                let mut state = ctx.create_state().map_err(|e| Status::internal(e.to_string()))?;
                let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                p.set_n_threads(self.st.threads);
                p.set_language(Some(if lang.is_empty() { self.st.language.as_str() } else { lang.as_str() }));
                p.set_print_progress(false);
                p.set_print_realtime(false);
                p.set_print_special(false);
                p.set_print_timestamps(false);
                p.set_suppress_blank(true);
                p.set_no_context(true);
                state.full(p, &samples).map_err(|e| Status::internal(format!("whisper failed: {e}")))?;
                let n = state.full_n_segments().map_err(|e| Status::internal(e.to_string()))?;
                let mut text = String::new();
                for i in 0..n {
                    text += &state.full_get_segment_text(i).map_err(|e| Status::internal(e.to_string()))?;
                }
                Ok(text)
            }
        }
    }
}

#[tonic::async_trait]
impl Stt for Svc {
    async fn transcribe(&self, req: Request<TranscribeRequest>) -> Result<Response<Transcript>, Status> {
        let trace = common::grpc::trace_id(&req);
        let r = req.into_inner();
        self.state.metrics.inc("transcribe_requests_total");
        let mut pcm = audio::bytes_to_i16(&r.pcm16);
        let sr = if r.sample_rate == 0 { 16000 } else { r.sample_rate };
        if sr != 16000 {
            pcm = audio::f32_to_i16(&audio::resample(&audio::i16_to_f32(&pcm), sr, 16000));
        }
        let duration = pcm.len() as f32 / 16000.0;
        if duration * 1000.0 < self.st.min_ms as f32 && !matches!(&*self.engine, Engine::EchoTest) {
            return Ok(Response::new(Transcript { text: String::new(), language: r.language, duration_s: duration }));
        }
        let me = Svc { engine: self.engine.clone(), st: self.st.clone(), state: self.state.clone() };
        let t0 = std::time::Instant::now();
        let lang = r.language.clone();
        let raw = tokio::task::spawn_blocking(move || me.run(pcm, lang)).await.map_err(|e| Status::internal(e.to_string()))??;
        let text = clean_transcript(&raw);
        self.state.metrics.observe("transcribe", t0.elapsed().as_secs_f64());
        tracing::info!(%trace, audio_s = duration, ms = t0.elapsed().as_millis() as u64, chars = text.len(), "transcribed");
        Ok(Response::new(Transcript { text, language: r.language, duration_s: duration }))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = Boot::init("stt")?;
    let st: Settings = boot.cfg.settings("stt");
    let model_name = std::env::var("WAIFU_STT_MODEL").unwrap_or_else(|_| st.model.clone());
    let model_path = PathBuf::from(&boot.cfg.global.models_dir).join(&model_name);
    let engine = if st.engine == "echo-test" {
        tracing::warn!("STT engine 'echo-test' is for tests only");
        Engine::EchoTest
    } else {
        #[cfg(feature = "whisper")]
        {
            if model_path.exists() {
                let t0 = std::time::Instant::now();
                match whisper_rs::WhisperContext::new_with_params(
                    &model_path.to_string_lossy(),
                    whisper_rs::WhisperContextParameters::default(),
                ) {
                    Ok(c) => {
                        tracing::info!(model = %model_path.display(), ms = t0.elapsed().as_millis() as u64, "whisper model loaded");
                        Engine::Whisper(Mutex::new(c))
                    }
                    Err(e) => Engine::Unavailable(format!("failed to load whisper model {}: {e}", model_path.display())),
                }
            } else {
                Engine::Unavailable(format!("whisper model {} not found - run the model downloader (scripts/download-models) or put a ggml model in ./assets/models. Use text chat meanwhile.", model_path.display()))
            }
        }
        #[cfg(not(feature = "whisper"))]
        Engine::Unavailable("stt-service built without the `whisper` feature".into())
    };
    if let Engine::Unavailable(why) = &engine {
        boot.state.degrade("stt_model", why.clone());
    }
    let svc = Svc { engine: Arc::new(engine), st, state: boot.state.clone() };
    // Warm-up: the first inference pays for lazy initialisation (GPU kernels / buffers) - do it now, before the
    // health endpoint exists, so "healthy" means "fast". (A modulated tone, because pure silence is gated out.)
    #[cfg(feature = "whisper")]
    {
        if matches!(&*svc.engine, Engine::Whisper(_)) {
            println!("@@loading warming up speech recognition");
            let t0 = std::time::Instant::now();
            let tone: Vec<i16> = (0..16000).map(|i| ((i as f32 * 0.05).sin() * (i as f32 * 0.002).sin() * 9000.0) as i16).collect();
            let _ = svc.run(tone, svc.st.language.clone());
            tracing::info!(ms = t0.elapsed().as_millis() as u64, "whisper warmed up");
            println!("@@ready");
        }
    }
    boot.spawn_admin();
    let addr = boot.me.addr.parse()?;
    let token = boot.state.shutdown.clone();
    tracing::info!(%addr, "stt gRPC listening");
    tonic::transport::Server::builder()
        .add_service(SttServer::new(svc))
        .serve_with_shutdown(addr, async move { token.cancelled().await })
        .await?;
    tracing::info!("stt-service stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_whisper_noise() {
        assert_eq!(clean_transcript(" [BLANK_AUDIO] "), "");
        assert_eq!(clean_transcript(" Hello (laughs) there. "), "Hello there.");
        assert_eq!(clean_transcript("Thanks for watching!"), "");
    }

    #[tokio::test]
    async fn missing_model_is_unavailable_not_crash() {
        let svc = Svc {
            engine: Arc::new(Engine::Unavailable("no model".into())),
            st: Settings::default(),
            state: common::ServiceState::new("stt"),
        };
        let r =
            svc.transcribe(Request::new(TranscribeRequest { pcm16: vec![1; 32000], sample_rate: 16000, language: String::new() })).await;
        assert_eq!(r.unwrap_err().code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn echo_engine_roundtrips_text() {
        let svc = Svc { engine: Arc::new(Engine::EchoTest), st: Settings::default(), state: common::ServiceState::new("stt") };
        let mut bytes = b"hello mio".to_vec();
        if bytes.len() % 2 == 1 {
            bytes.push(0);
        }
        let r =
            svc.transcribe(Request::new(TranscribeRequest { pcm16: bytes, sample_rate: 16000, language: String::new() })).await.unwrap();
        assert_eq!(r.into_inner().text, "hello mio");
    }

    /// Opt-in: `WAIFU_TEST_WHISPER_MODEL=/path/to/ggml-*.bin cargo test -p stt-service`.
    /// Proves the whisper-rs load + inference path runs (with a real model the text is the transcript).
    #[cfg(feature = "whisper")]
    #[test]
    fn whisper_model_loads_and_infers_when_configured() {
        let Ok(path) = std::env::var("WAIFU_TEST_WHISPER_MODEL") else {
            eprintln!("skipping: WAIFU_TEST_WHISPER_MODEL not set");
            return;
        };
        let ctx = whisper_rs::WhisperContext::new_with_params(&path, whisper_rs::WhisperContextParameters::default()).expect("model loads");
        let svc =
            Svc { engine: Arc::new(Engine::Whisper(Mutex::new(ctx))), st: Settings::default(), state: common::ServiceState::new("stt") };
        // 1.5 s of a modulated tone (not silence, so the energy gate lets it through)
        let pcm: Vec<i16> = (0..24000).map(|i| ((i as f32 * 0.05).sin() * (i as f32 * 0.002).sin() * 9000.0) as i16).collect();
        let out = svc.run(pcm, "en".into());
        assert!(out.is_ok(), "inference must not fail: {out:?}");
        eprintln!("whisper output: {:?}", out.unwrap());
    }
}
