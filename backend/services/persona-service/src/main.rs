//! persona-service: loads the persona file (config/personas/*.yaml), hot-reloads it (mtime polling - works on every OS and
//! on Docker bind mounts) and builds system prompts.
use anyhow::Context;
use common::Boot;
use contracts::persona_server::{Persona, PersonaServer};
use contracts::*;
use serde::Deserialize;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tonic::{Request, Response, Status};

#[derive(Debug, Clone, Deserialize)]
pub struct PersonaFile {
    pub name: String,
    #[serde(default = "d_age")]
    pub age: u32,
    #[serde(default = "d_lang")]
    pub language: String,
    #[serde(default)]
    pub voice: String,
    #[serde(default)]
    pub avatar: String,
    #[serde(default)]
    pub greeting: String,
    #[serde(default)]
    pub speech_style: String,
    #[serde(default)]
    pub backstory: String,
    #[serde(default)]
    pub behavior_rules: Vec<String>,
    #[serde(default = "d_emotions")]
    pub emotions: Vec<String>,
}
fn d_age() -> u32 {
    20
}
fn d_lang() -> String {
    "en".into()
}
fn d_emotions() -> Vec<String> {
    ["neutral", "happy", "sad", "angry", "surprised", "shy", "thinking", "smug"].iter().map(|s| s.to_string()).collect()
}

impl PersonaFile {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let p: PersonaFile = serde_yaml::from_str(text).context("invalid persona yaml")?;
        if p.name.trim().is_empty() {
            anyhow::bail!("persona 'name' must not be empty");
        }
        Ok(p)
    }

    /// `tts.voice` / `avatar.file` from settings.yaml win over the persona's own values (blank = keep).
    pub fn with_overrides(mut self, o: &Overrides) -> Self {
        if let Some(v) = o.voice.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            self.voice = v.to_string();
        }
        if let Some(a) = o.avatar.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            self.avatar = a.to_string();
        }
        self
    }

    pub fn info(&self, version: u64) -> PersonaInfo {
        PersonaInfo {
            name: self.name.clone(),
            age: self.age,
            greeting: self.greeting.clone(),
            voice: self.voice.clone(),
            avatar: self.avatar.clone(),
            language: self.language.clone(),
            emotions: self.emotions.clone(),
            version,
        }
    }

    pub fn build_prompt(&self, req: &BuildPromptRequest) -> String {
        let user = if req.user_name.is_empty() { "the user" } else { req.user_name.as_str() };
        let mut s = format!("You are {}, a {}-year-old companion having a spoken conversation with {}.\n\n", self.name, self.age, user);
        if !self.backstory.trim().is_empty() {
            s += &format!("# Backstory\n{}\n\n", self.backstory.trim());
        }
        if !self.speech_style.trim().is_empty() {
            s += &format!("# Speech style\n{}\n\n", self.speech_style.trim());
        }
        if !self.behavior_rules.is_empty() {
            s += "# Rules\n";
            for r in &self.behavior_rules {
                s += &format!("- {r}\n");
            }
            s += "\n";
        }
        if !req.facts.is_empty() {
            s += &format!("# What you know about {user}\n");
            for f in &req.facts {
                s += &format!("- {}: {}\n", f.key, f.value);
            }
            s += "\n";
        }
        if !req.summary.trim().is_empty() {
            s += &format!("# Earlier conversations (summary)\n{}\n\n", req.summary.trim());
        }
        if !req.memories.is_empty() {
            s += "# Possibly relevant memories\n";
            for m in &req.memories {
                s += &format!("- {}\n", m.text);
            }
            s += "\n";
        }
        s += &format!(
            "# Output format (CRITICAL REQUIREMENT)\n\
MANDATORY: You MUST start EVERY single sentence with exactly one emotion tag in square brackets chosen from: {}.\n\
Example:\n\
\"[smug] Oh, you finally made it back. [thinking] What took you so long? [shy] Not that I was waiting or anything.\"\n\
Rules:\n\
1. Every sentence MUST begin with an emotion tag like [smug], [happy], [thinking], etc. Never omit it.\n\
2. Allowed emotions ONLY: {}.\n\
3. These tags drive your 3D avatar's facial expressions and body pose in real-time.\n\
4. Spoken conversation only: no asterisks, no markdown, no stage directions, no emojis. Reply in {}.\n\
Keep it to one to three short sentences, plain text only.",
            self.emotions.iter().map(|e| format!("[{e}]")).collect::<Vec<_>>().join(" "),
            self.emotions.iter().map(|e| format!("[{e}]")).collect::<Vec<_>>().join(", "),
            self.language
        );
        if !req.current_emotion.is_empty() {
            s += &format!(" Your current mood is {}.", req.current_emotion);
        }
        s
    }
}

struct Svc {
    rx: watch::Receiver<(Arc<PersonaFile>, u64)>,
}

type Stream<T> = Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl Persona for Svc {
    async fn get(&self, _: Request<Empty>) -> Result<Response<PersonaInfo>, Status> {
        let (p, v) = self.rx.borrow().clone();
        Ok(Response::new(p.info(v)))
    }
    async fn build_prompt(&self, req: Request<BuildPromptRequest>) -> Result<Response<BuildPromptResponse>, Status> {
        let (p, v) = self.rx.borrow().clone();
        Ok(Response::new(BuildPromptResponse { system_prompt: p.build_prompt(req.get_ref()), info: Some(p.info(v)) }))
    }
    type WatchStream = Stream<PersonaInfo>;
    async fn watch(&self, _: Request<Empty>) -> Result<Response<Self::WatchStream>, Status> {
        Ok(Response::new(watch_stream(self.rx.clone())))
    }
}

/// Stream that yields the current persona immediately, then every change.
fn watch_stream(mut w: watch::Receiver<(Arc<PersonaFile>, u64)>) -> Stream<PersonaInfo> {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        let (p, v) = w.borrow_and_update().clone();
        if tx.send(Ok(p.info(v))).await.is_err() {
            return;
        }
        while w.changed().await.is_ok() {
            let (p, v) = w.borrow_and_update().clone();
            if tx.send(Ok(p.info(v))).await.is_err() {
                return;
            }
        }
    });
    Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
}

#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub voice: Option<String>,
    pub avatar: Option<String>,
}

#[derive(Deserialize)]
#[serde(default)]
struct Settings {
    persona_file: String,
    poll_ms: u64,
    /// filled from settings.yaml (`tts.voice`, `avatar.file`) by common::Config
    voice_override: Option<String>,
    avatar_override: Option<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self { persona_file: "config/personas/mio.yaml".into(), poll_ms: 1000, voice_override: None, avatar_override: None }
    }
}

fn mtime(p: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = Boot::init("persona")?;
    let settings: Settings = boot.cfg.settings("persona");
    let path = PathBuf::from(&settings.persona_file);
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let over = Overrides { voice: settings.voice_override.clone(), avatar: settings.avatar_override.clone() };
    let first = PersonaFile::parse(&text)?.with_overrides(&over);
    tracing::info!(name = %first.name, file = %path.display(), "persona loaded");
    let (tx, rx) = watch::channel((Arc::new(first), 1u64));

    // hot reload task
    {
        let state = boot.state.clone();
        let path = path.clone();
        let over = over.clone();
        tokio::spawn(async move {
            let mut last = mtime(&path);
            let mut version = 1u64;
            loop {
                tokio::select! {
                    _ = state.shutdown.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(settings.poll_ms)) => {}
                }
                let m = mtime(&path);
                if m == last {
                    continue;
                }
                last = m;
                match std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|t| PersonaFile::parse(&t)) {
                    Ok(p) => {
                        let p = p.with_overrides(&over);
                        version += 1;
                        tracing::info!(name = %p.name, version, "persona hot-reloaded");
                        state.recover("persona_file");
                        state.metrics.inc("reloads_total");
                        let _ = tx.send((Arc::new(p), version));
                    }
                    Err(e) => {
                        state.degrade("persona_file", format!("reload failed, keeping previous persona: {e}"));
                    }
                }
            }
        });
    }

    boot.spawn_admin();
    let addr = boot.me.addr.parse()?;
    let token = boot.state.shutdown.clone();
    tracing::info!(%addr, "persona gRPC listening");
    tonic::transport::Server::builder()
        .add_service(PersonaServer::new(Svc { rx }))
        .serve_with_shutdown(addr, async move { token.cancelled().await })
        .await?;
    tracing::info!("persona-service stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const Y: &str = "name: Test\nage: 21\nbehavior_rules: [be nice]\nbackstory: likes tea\n";

    #[test]
    fn parses_with_defaults() {
        let p = PersonaFile::parse(Y).unwrap();
        assert_eq!(p.name, "Test");
        assert_eq!(p.emotions.len(), 8);
    }
    #[test]
    fn rejects_bad_yaml() {
        assert!(PersonaFile::parse("age: 3").is_err());
        assert!(PersonaFile::parse("name: ''").is_err());
    }
    #[test]
    fn prompt_includes_memory_and_tags() {
        let p = PersonaFile::parse(Y).unwrap();
        let s = p.build_prompt(&BuildPromptRequest {
            user_name: "Ken".into(),
            summary: "we talked about stars".into(),
            facts: vec![Fact { key: "pet".into(), value: "cat named Tofu".into() }],
            memories: vec![],
            current_emotion: "happy".into(),
        });
        assert!(s.contains("Test") && s.contains("Ken") && s.contains("cat named Tofu"));
        assert!(s.contains("[happy]") && s.contains("likes tea") && s.contains("we talked about stars"));
    }
    #[test]
    fn settings_override_voice_and_avatar() {
        let p = PersonaFile::parse("name: T\nvoice: amy\navatar: a.vrm\n").unwrap();
        let o = Overrides { voice: Some(" ref ".into()), avatar: Some("".into()) };
        let p = p.with_overrides(&o);
        assert_eq!(p.voice, "ref");
        assert_eq!(p.avatar, "a.vrm"); // blank override keeps the persona's value
    }
    #[test]
    fn shipped_persona_is_valid() {
        let t = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/personas/mio.yaml")).unwrap();
        assert_eq!(PersonaFile::parse(&t).unwrap().name, "Mio");
    }
}
