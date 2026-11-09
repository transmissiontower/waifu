//! tts-service: local text-to-speech, streamed as PCM16 chunks.
//! Engines: Piper/VITS or Kokoro through sherpa-onnx (voice folders in ./voices), with a
//! formant babble fallback when no voice is installed.
mod babble;

use common::{audio, grpc, Boot};
use contracts::tts_server::{Tts, TtsServer};
use contracts::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Settings {
    default_voice: String,
    /// "auto" (neural if a voice is available, else babble) | "babble"
    engine: String,
    threads: i32,
    speed: f32,
    chunk_ms: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self { default_voice: "amy".into(), engine: "auto".into(), threads: 2, speed: 1.0, chunk_ms: 200 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct VoiceYaml {
    speaker: i32,
    speed: f32,
}
impl Default for VoiceYaml {
    fn default() -> Self {
        Self { speaker: 0, speed: 1.0 }
    }
}

/// A loaded neural voice.
#[cfg(feature = "sherpa")]
enum Neural {
    Vits(sherpa_rs::tts::VitsTts),
    Kokoro(sherpa_rs::tts::KokoroTts),
}

#[cfg(feature = "sherpa")]
struct Voice {
    engine: Neural,
    cfg: VoiceYaml,
}

struct Svc {
    st: Settings,
    voices_dir: PathBuf,
    #[cfg(feature = "sherpa")]
    cache: Arc<Mutex<HashMap<String, Arc<Mutex<Voice>>>>>,
    #[cfg(not(feature = "sherpa"))]
    #[allow(dead_code)]
    cache: Arc<Mutex<HashMap<String, ()>>>,
    state: common::ServiceState,
}

/// Remove markdown/emoji/stage-direction noise that TTS engines read aloud.
fn clean_text(t: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in t.chars() {
        match c {
            '*' | '_' | '`' | '#' | '~' => {}
            '(' | '[' if false => depth += 1,
            _ if depth > 0 => {}
            c if (c as u32) >= 0x1F000 || matches!(c as u32, 0x2600..=0x27BF) => {}
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn list_voices(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

impl Svc {
    #[cfg(feature = "sherpa")]
    fn load_voice(&self, name: &str) -> anyhow::Result<Arc<Mutex<Voice>>> {
        use sherpa_rs::tts::*;
        if let Some(v) = self.cache.lock().unwrap().get(name) {
            return Ok(v.clone());
        }
        let dir = self.voices_dir.join(name);
        anyhow::ensure!(dir.is_dir(), "voice folder '{}' not found", dir.display());
        let cfg: VoiceYaml =
            std::fs::read_to_string(dir.join("voice.yaml")).ok().and_then(|t| serde_yaml::from_str(&t).ok()).unwrap_or_default();
        let model = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().map(|e| e == "onnx").unwrap_or(false))
            .ok_or_else(|| anyhow::anyhow!("no .onnx model in {}", dir.display()))?;
        let s = |p: PathBuf| p.to_string_lossy().to_string();
        let tokens = s(dir.join("tokens.txt"));
        let data_dir = s(dir.join("espeak-ng-data"));
        anyhow::ensure!(dir.join("tokens.txt").exists(), "tokens.txt missing in {}", dir.display());
        let onnx = sherpa_rs::OnnxConfig { num_threads: self.st.threads, ..Default::default() };
        let engine = if dir.join("voices.bin").exists() {
            Neural::Kokoro(KokoroTts::new(KokoroTtsConfig {
                model: s(model),
                voices: s(dir.join("voices.bin")),
                tokens,
                data_dir,
                length_scale: 1.0,
                onnx_config: onnx,
                ..Default::default()
            }))
        } else {
            Neural::Vits(VitsTts::new(VitsTtsConfig {
                model: s(model),
                tokens,
                data_dir,
                length_scale: 1.0,
                noise_scale: 0.667,
                noise_scale_w: 0.8,
                silence_scale: 0.2,
                onnx_config: onnx,
                ..Default::default()
            }))
        };
        tracing::info!(voice = name, "neural voice loaded");
        let v = Arc::new(Mutex::new(Voice { engine, cfg }));
        self.cache.lock().unwrap().insert(name.to_string(), v.clone());
        Ok(v)
    }

    /// Returns (samples, sample_rate, engine_name).
    fn render(&self, voice: &str, text: &str, speed: f32) -> (Vec<f32>, u32, &'static str) {
        let speed = if speed > 0.0 { speed } else { self.st.speed };
        #[cfg(feature = "sherpa")]
        if self.st.engine != "babble" {
            match self.load_voice(voice) {
                Ok(v) => {
                    let mut v = v.lock().unwrap();
                    let (sid, vs) = (v.cfg.speaker, v.cfg.speed);
                    let r = match &mut v.engine {
                        Neural::Vits(t) => t.create(text, sid, speed * vs),
                        Neural::Kokoro(t) => t.create(text, sid, speed * vs),
                    };
                    match r {
                        Ok(a) => {
                            self.state.recover("voice");
                            return (a.samples, a.sample_rate, "sherpa");
                        }
                        Err(e) => self.state.degrade("voice", format!("synthesis failed: {e}")),
                    }
                }
                Err(e) => self.state.degrade("voice", format!("{e}; using babble fallback (run scripts/download-models to get a voice)")),
            }
        }
        let _ = voice;
        (babble::synth(text, speed), babble::SAMPLE_RATE, "babble")
    }
}

#[tonic::async_trait]
impl Tts for Svc {
    type SynthesizeStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<AudioChunk, Status>> + Send>>;

    async fn synthesize(&self, req: Request<SynthRequest>) -> Result<Response<Self::SynthesizeStream>, Status> {
        let trace = grpc::trace_id(&req);
        let r = req.into_inner();
        let text = clean_text(&r.text);
        let voice = if r.voice.is_empty() { self.st.default_voice.clone() } else { r.voice.clone() };
        self.state.metrics.inc("synth_requests_total");
        let (tx, rx) = mpsc::channel(32);
        let me = Arc::new(Svc {
            st: self.st.clone(),
            voices_dir: self.voices_dir.clone(),
            cache: self.cache.clone(),
            state: self.state.clone(),
        });
        let chunk_ms = self.st.chunk_ms.max(50);
        tokio::spawn(async move {
            let t0 = std::time::Instant::now();
            let me2 = me.clone();
            let speed = r.speed;
            let res = tokio::task::spawn_blocking(
                move || if text.is_empty() { (vec![], 22050, "none") } else { me2.render(&voice, &text, speed) },
            )
            .await;
            let Ok((samples, sr, engine)) = res else {
                let _ = tx.send(Err(Status::internal("synthesis task panicked"))).await;
                return;
            };
            me.state.metrics.observe("synth", t0.elapsed().as_secs_f64());
            tracing::info!(%trace, engine, samples = samples.len(), ms = t0.elapsed().as_millis() as u64, "synthesized");
            let pcm = audio::f32_to_i16(&samples);
            let per = (sr as usize * chunk_ms as usize / 1000).max(1);
            let n = pcm.len().div_ceil(per).max(1);
            for (i, c) in pcm.chunks(per).enumerate() {
                let chunk = AudioChunk { pcm16: audio::i16_to_bytes(c), sample_rate: sr, channels: 1, seq: i as u32, last: i + 1 == n };
                if tx.send(Ok(chunk)).await.is_err() {
                    return; // client interrupted
                }
            }
            if pcm.is_empty() {
                let _ = tx.send(Ok(AudioChunk { pcm16: vec![], sample_rate: sr, channels: 1, seq: 0, last: true })).await;
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }

    async fn list_voices(&self, _: Request<Empty>) -> Result<Response<VoiceList>, Status> {
        Ok(Response::new(VoiceList {
            voices: list_voices(&self.voices_dir),
            default_voice: self.st.default_voice.clone(),
            engine: self.st.engine.clone(),
        }))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = Boot::init("tts")?;
    let st: Settings = boot.cfg.settings("tts");
    let voices_dir = PathBuf::from(&boot.cfg.global.voices_dir);
    tracing::info!(voices = ?list_voices(&voices_dir), default = %st.default_voice, "voices available");
    if list_voices(&voices_dir).is_empty() {
        boot.state.degrade("voice", "no voices in ./voices - using babble fallback");
    }
    let svc = Svc { st, voices_dir, cache: Default::default(), state: boot.state.clone() };
    // Warm up the default voice so the first reply is fast.
    #[cfg(feature = "sherpa")]
    {
        let warm =
            Arc::new(Svc { st: svc.st.clone(), voices_dir: svc.voices_dir.clone(), cache: svc.cache.clone(), state: svc.state.clone() });
        tokio::task::spawn_blocking(move || {
            let name = warm.st.default_voice.clone();
            if warm.voices_dir.join(&name).is_dir() {
                let _ = warm.render(&name, "Hello.", 1.0);
            }
        });
    }
    boot.spawn_admin();
    let addr = boot.me.addr.parse()?;
    let token = boot.state.shutdown.clone();
    tracing::info!(%addr, "tts gRPC listening");
    tonic::transport::Server::builder()
        .add_service(TtsServer::new(svc))
        .serve_with_shutdown(addr, async move { token.cancelled().await })
        .await?;
    tracing::info!("tts-service stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_markdown_and_emoji() {
        assert_eq!(clean_text("**Hello**  _there_ \u{1F600}!"), "Hello there !");
    }

    #[test]
    fn babble_fallback_when_voice_missing() {
        let svc = Svc {
            st: Settings::default(),
            voices_dir: PathBuf::from("/nonexistent"),
            cache: Default::default(),
            state: common::ServiceState::new("tts"),
        };
        let (samples, sr, engine) = svc.render("nope", "hello there", 1.0);
        assert_eq!(engine, "babble");
        assert_eq!(sr, babble::SAMPLE_RATE);
        assert!(!samples.is_empty());
    }

    /// Runs only when a real voice is installed (./voices/amy); proves the neural path works.
    #[cfg(feature = "sherpa")]
    #[test]
    fn neural_voice_synthesizes_when_installed() {
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../assets/voices"));
        if !dir.join("amy").is_dir() {
            eprintln!("skipping: no voice installed");
            return;
        }
        let svc = Svc { st: Settings::default(), voices_dir: dir, cache: Default::default(), state: common::ServiceState::new("tts") };
        let (samples, sr, engine) = svc.render("amy", "Hello, my name is Mio.", 1.0);
        assert_eq!(engine, "sherpa");
        assert!(sr >= 16000);
        assert!(samples.len() as f32 / sr as f32 > 0.8, "expected >0.8s of speech, got {}", samples.len());
        let peak = samples.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.1, "audio is silent");
    }
}
