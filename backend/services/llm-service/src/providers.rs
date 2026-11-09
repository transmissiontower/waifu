use crate::fallback;
use common::resilience::CircuitBreaker;
use common::ServiceState;
use contracts::*;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tonic::Status;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub providers: Vec<String>,
    pub temperature: f32,
    pub max_tokens: u32,
    pub ollama: Ollama,
    pub openai: OpenAi,
    pub breaker: Breaker,
    /// seconds without any chunk before a stream is considered stalled
    pub stall_timeout_s: u64,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Ollama {
    pub base_url: String,
    /// "auto" picks the first installed entry of `model_candidates`. `WAIFU_LLM_MODEL` env overrides.
    pub model: String,
    pub model_candidates: Vec<String>,
    pub embed_model: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OpenAi {
    pub base_url: String,
    pub model: String,
    /// Name of the env var holding the key (optional - local servers need none).
    pub api_key_env: String,
    pub embed_model: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Breaker {
    pub threshold: u32,
    pub cooldown_s: u64,
}
impl Default for Breaker {
    fn default() -> Self {
        Self { threshold: 2, cooldown_s: 15 }
    }
}
impl Default for Ollama {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434".into(),
            model: "auto".into(),
            model_candidates: vec![
                "qwen2.5:7b".into(),
                "qwen2.5:3b".into(),
                "qwen2.5:1.5b".into(),
                "llama3.2:3b".into(),
                "llama3.2:1b".into(),
            ],
            embed_model: "nomic-embed-text".into(),
        }
    }
}
impl Default for OpenAi {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:1234/v1".into(),
            model: "local-model".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            embed_model: "text-embedding-3-small".into(),
        }
    }
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            providers: vec!["ollama".into(), "fallback".into()],
            temperature: 0.8,
            max_tokens: 300,
            ollama: Ollama::default(),
            openai: OpenAi::default(),
            breaker: Breaker::default(),
            stall_timeout_s: 45,
        }
    }
}

pub struct Shared {
    pub settings: Settings,
    pub breakers: HashMap<String, CircuitBreaker>,
    pub http: reqwest::Client,
    pub state: ServiceState,
    model_cache: Mutex<Option<(Instant, Result<String, String>)>>,
    last_ok: Mutex<Option<String>>,
}

impl Shared {
    pub fn new(settings: Settings, breakers: HashMap<String, CircuitBreaker>, state: ServiceState) -> Self {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build().expect("http client");
        Self { settings, breakers, http, state, model_cache: Mutex::new(None), last_ok: Mutex::new(None) }
    }
}

/// Failure of one provider. `emitted` = some tokens were already sent to the caller.
pub struct PErr {
    pub emitted: bool,
    pub msg: String,
}
fn perr<E: std::fmt::Display>(e: E) -> PErr {
    PErr { emitted: false, msg: e.to_string() }
}

/// Where tokens go; tracks whether anything was sent and attaches the pending notice once.
struct Sink {
    tx: mpsc::Sender<Result<ChatToken, Status>>,
    provider: String,
    notice: Option<String>,
    emitted: bool,
}
impl Sink {
    /// returns false when the consumer went away (interruption) -> stop generating.
    async fn text(&mut self, text: String) -> bool {
        if text.is_empty() {
            return true;
        }
        self.emitted = true;
        let notice = self.notice.take().unwrap_or_default();
        self.tx.send(Ok(ChatToken { text, done: false, provider: self.provider.clone(), notice })).await.is_ok()
    }
    async fn done(&mut self, notice: Option<String>) {
        let n = notice.or_else(|| self.notice.take()).unwrap_or_default();
        let _ = self.tx.send(Ok(ChatToken { text: String::new(), done: true, provider: self.provider.clone(), notice: n })).await;
    }
}

pub async fn run_chat(sh: std::sync::Arc<Shared>, req: ChatRequest, tx: mpsc::Sender<Result<ChatToken, Status>>) {
    let mut problems: Vec<String> = vec![];
    for name in &sh.settings.providers {
        let Some(br) = sh.breakers.get(name) else { continue };
        if br.is_open() && name != "fallback" {
            problems.push(format!("{name}: temporarily disabled after repeated failures"));
            continue;
        }
        let notice =
            if problems.is_empty() { None } else { Some(format!("Using built-in fallback brain because: {}", problems.join("; "))) };
        let mut sink = Sink { tx: tx.clone(), provider: name.clone(), notice, emitted: false };
        let res = match name.as_str() {
            "ollama" => chat_ollama(&sh, &req, &mut sink).await,
            "openai" => chat_openai(&sh, &req, &mut sink).await,
            "fallback" => chat_fallback(&req, &mut sink).await,
            other => Err(perr(format!("unknown provider '{other}'"))),
        };
        match res {
            Ok(()) => {
                br.record(true);
                sh.state.recover(&format!("provider_{name}"));
                *sh.last_ok.lock().unwrap() = Some(name.clone());
                sh.state.metrics.inc_l("chat_ok_total", &[("provider", name)]);
                if name == "fallback" && sh.settings.providers.len() > 1 {
                    sh.state.degrade("llm", "primary LLM unavailable, serving fallback brain");
                } else if name != "fallback" {
                    sh.state.recover("llm");
                }
                return;
            }
            Err(e) => {
                br.record(false);
                sh.state.metrics.inc_l("chat_errors_total", &[("provider", name)]);
                tracing::warn!(provider = %name, error = %e.msg, emitted = e.emitted, "provider failed");
                sh.state.degrade(&format!("provider_{name}"), e.msg.clone());
                if e.emitted {
                    sink.done(Some(format!("The language model connection dropped mid-reply ({}).", e.msg))).await;
                    return;
                }
                problems.push(format!("{name}: {}", e.msg));
            }
        }
    }
    let _ = tx.send(Err(Status::unavailable(format!("no LLM provider available: {}", problems.join("; "))))).await;
}

fn messages_json(req: &ChatRequest) -> Vec<Value> {
    req.messages.iter().map(|m| json!({"role": m.role, "content": m.content})).collect()
}

async fn chat_fallback(req: &ChatRequest, sink: &mut Sink) -> Result<(), PErr> {
    let system = req.messages.iter().find(|m| m.role == "system").map(|m| m.content.as_str()).unwrap_or("");
    let user = req.messages.iter().rev().find(|m| m.role == "user").map(|m| m.content.as_str()).unwrap_or("");
    let text = fallback::reply(system, user);
    for w in text.split_inclusive(' ') {
        if !sink.text(w.to_string()).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    sink.done(None).await;
    Ok(())
}

async fn resolve_ollama_model(sh: &Shared) -> Result<String, String> {
    if let Ok(m) = std::env::var("WAIFU_LLM_MODEL") {
        if !m.is_empty() {
            return Ok(m);
        }
    }
    if let Some((t, r)) = &*sh.model_cache.lock().unwrap() {
        // a resolved model name is stable: don't ask Ollama again for 5 minutes (failures are retried after 3 s)
        let ttl = if r.is_ok() { 300 } else { 3 };
        if t.elapsed() < Duration::from_secs(ttl) {
            return r.clone();
        }
    }
    let cfg = &sh.settings.ollama;
    let res = async {
        let tags: Value = sh
            .http
            .get(format!("{}/api/tags", cfg.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("Ollama is not reachable at {} ({e}). Start it with `ollama serve`.", cfg.base_url))?
            .json()
            .await
            .map_err(|e| format!("bad reply from Ollama: {e}"))?;
        let installed: Vec<String> =
            tags["models"].as_array().map(|a| a.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect()).unwrap_or_default();
        let has = |want: &str| {
            installed
                .iter()
                .any(|i| i == want || i == &format!("{want}:latest") || i.starts_with(&format!("{want}:")) && !want.contains(':'))
        };
        let wanted: Vec<&String> = if cfg.model == "auto" { cfg.model_candidates.iter().collect() } else { vec![&cfg.model] };
        wanted.iter().find(|m| has(m)).map(|m| m.to_string()).ok_or_else(|| {
            format!(
                "no suitable model installed (wanted {:?}). Run: ollama pull {}",
                wanted,
                wanted.last().map(|s| s.as_str()).unwrap_or("qwen2.5:1.5b")
            )
        })
    }
    .await;
    *sh.model_cache.lock().unwrap() = Some((Instant::now(), res.clone()));
    res
}

async fn chat_ollama(sh: &Shared, req: &ChatRequest, sink: &mut Sink) -> Result<(), PErr> {
    let model = if req.model.is_empty() { resolve_ollama_model(sh).await.map_err(perr)? } else { req.model.clone() };
    let temp = if req.temperature > 0.0 { req.temperature } else { sh.settings.temperature };
    let max = if req.max_tokens > 0 { req.max_tokens } else { sh.settings.max_tokens };
    let body = json!({
        "model": model, "messages": messages_json(req), "stream": true, "keep_alive": "24h",
        "options": {"temperature": temp, "num_predict": max}
    });
    let resp = sh.http.post(format!("{}/api/chat", sh.settings.ollama.base_url)).json(&body).send().await.map_err(perr)?;
    if !resp.status().is_success() {
        let st = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(perr(format!("ollama HTTP {st}: {}", t.chars().take(200).collect::<String>())));
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = vec![];
    let stall = Duration::from_secs(sh.settings.stall_timeout_s);
    loop {
        let chunk = match tokio::time::timeout(stall, stream.next()).await {
            Err(_) => return Err(PErr { emitted: sink.emitted, msg: "stream stalled".into() }),
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(PErr { emitted: sink.emitted, msg: e.to_string() }),
            Ok(Some(Ok(c))) => c,
        };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
            if let Some(e) = v["error"].as_str() {
                return Err(PErr { emitted: sink.emitted, msg: e.to_string() });
            }
            if let Some(t) = v["message"]["content"].as_str() {
                if !sink.text(t.to_string()).await {
                    return Ok(()); // consumer gone (interrupted); dropping the response aborts generation
                }
            }
            if v["done"].as_bool() == Some(true) {
                sink.done(None).await;
                return Ok(());
            }
        }
    }
    sink.done(None).await;
    Ok(())
}

async fn chat_openai(sh: &Shared, req: &ChatRequest, sink: &mut Sink) -> Result<(), PErr> {
    let c = &sh.settings.openai;
    let model = if req.model.is_empty() { c.model.clone() } else { req.model.clone() };
    let body = json!({
        "model": model, "messages": messages_json(req), "stream": true,
        "temperature": if req.temperature > 0.0 { req.temperature } else { sh.settings.temperature },
        "max_tokens": if req.max_tokens > 0 { req.max_tokens } else { sh.settings.max_tokens },
    });
    let mut rb = sh.http.post(format!("{}/chat/completions", c.base_url)).json(&body);
    if let Ok(k) = std::env::var(&c.api_key_env) {
        if !k.is_empty() {
            rb = rb.bearer_auth(k);
        }
    }
    let resp = rb.send().await.map_err(perr)?;
    if !resp.status().is_success() {
        let st = resp.status();
        return Err(perr(format!(
            "openai-compatible HTTP {st}: {}",
            resp.text().await.unwrap_or_default().chars().take(200).collect::<String>()
        )));
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = vec![];
    let stall = Duration::from_secs(sh.settings.stall_timeout_s);
    loop {
        let chunk = match tokio::time::timeout(stall, stream.next()).await {
            Err(_) => return Err(PErr { emitted: sink.emitted, msg: "stream stalled".into() }),
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(PErr { emitted: sink.emitted, msg: e.to_string() }),
            Ok(Some(Ok(c))) => c,
        };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                sink.done(None).await;
                return Ok(());
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                    if !sink.text(t.to_string()).await {
                        return Ok(());
                    }
                }
            }
        }
    }
    sink.done(None).await;
    Ok(())
}

pub async fn embed(sh: &Shared, inputs: Vec<String>) -> Result<EmbedResponse, Status> {
    let mut errors = vec![];
    for name in &sh.settings.providers {
        match name.as_str() {
            "ollama" => {
                let c = &sh.settings.ollama;
                let r = sh
                    .http
                    .post(format!("{}/api/embed", c.base_url))
                    .timeout(Duration::from_secs(20))
                    .json(&json!({"model": c.embed_model, "input": inputs}))
                    .send()
                    .await;
                match r {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(v) = resp.json::<Value>().await {
                            if let Some(arr) = v["embeddings"].as_array() {
                                let vectors = arr
                                    .iter()
                                    .map(|e| Vector {
                                        values: e
                                            .as_array()
                                            .map(|a| a.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect())
                                            .unwrap_or_default(),
                                    })
                                    .collect();
                                return Ok(EmbedResponse { vectors, model: format!("ollama:{}", c.embed_model) });
                            }
                        }
                        errors.push("ollama: malformed embed response".to_string());
                    }
                    Ok(resp) => errors.push(format!("ollama: HTTP {} (is '{}' pulled?)", resp.status(), c.embed_model)),
                    Err(e) => errors.push(format!("ollama: {e}")),
                }
            }
            "openai" => {
                let c = &sh.settings.openai;
                let mut rb = sh
                    .http
                    .post(format!("{}/embeddings", c.base_url))
                    .timeout(Duration::from_secs(20))
                    .json(&json!({"model": c.embed_model, "input": inputs}));
                if let Ok(k) = std::env::var(&c.api_key_env) {
                    rb = rb.bearer_auth(k);
                }
                match rb.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(v) = resp.json::<Value>().await {
                            if let Some(arr) = v["data"].as_array() {
                                let vectors = arr
                                    .iter()
                                    .map(|e| Vector {
                                        values: e["embedding"]
                                            .as_array()
                                            .map(|a| a.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect())
                                            .unwrap_or_default(),
                                    })
                                    .collect();
                                return Ok(EmbedResponse { vectors, model: format!("openai:{}", c.embed_model) });
                            }
                        }
                    }
                    Ok(resp) => errors.push(format!("openai: HTTP {}", resp.status())),
                    Err(e) => errors.push(format!("openai: {e}")),
                }
            }
            _ => {}
        }
    }
    Err(Status::unavailable(format!("no embedding provider available: {}", errors.join("; "))))
}

pub async fn info(sh: &Shared) -> LlmInfo {
    let active = sh.last_ok.lock().unwrap().clone().unwrap_or_default();
    let primary = sh.settings.providers.first().cloned().unwrap_or_default();
    let primary_healthy = match primary.as_str() {
        "ollama" => resolve_ollama_model(sh).await.is_ok(),
        "fallback" => true,
        _ => sh.breakers.get(&primary).map(|b| !b.is_open()).unwrap_or(false),
    };
    let model = match primary.as_str() {
        "ollama" => resolve_ollama_model(sh).await.unwrap_or_default(),
        "openai" => sh.settings.openai.model.clone(),
        _ => "builtin".into(),
    };
    LlmInfo { active_provider: active, model, providers: sh.settings.providers.clone(), primary_healthy }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn shared(providers: &[&str], base: &str) -> Arc<Shared> {
        let mut s = Settings { providers: providers.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        s.ollama.base_url = base.into();
        s.ollama.model = "x".into();
        let br = s.providers.iter().map(|p| (p.clone(), CircuitBreaker::new(p, 2, Duration::from_secs(5)))).collect();
        Arc::new(Shared::new(s, br, ServiceState::new("llm-test")))
    }

    async fn collect(sh: Arc<Shared>) -> (String, Vec<String>, Option<String>) {
        let (tx, mut rx) = mpsc::channel(64);
        let req = ChatRequest { messages: vec![ChatMessage { role: "user".into(), content: "hello".into() }], ..Default::default() };
        tokio::spawn(run_chat(sh, req, tx));
        let (mut text, mut notices, mut err) = (String::new(), vec![], None);
        while let Some(m) = rx.recv().await {
            match m {
                Ok(t) => {
                    text += &t.text;
                    if !t.notice.is_empty() {
                        notices.push(t.notice);
                    }
                }
                Err(s) => err = Some(s.message().to_string()),
            }
        }
        (text, notices, err)
    }

    #[tokio::test]
    async fn falls_back_when_ollama_down_with_clear_notice() {
        let (text, notices, err) = collect(shared(&["ollama", "fallback"], "http://127.0.0.1:9")).await;
        assert!(err.is_none());
        assert!(text.contains("[happy]"));
        assert!(notices.iter().any(|n| n.contains("fallback brain") && n.contains("not reachable")), "{notices:?}");
    }

    #[tokio::test]
    async fn errors_clearly_without_fallback() {
        let (_, _, err) = collect(shared(&["ollama"], "http://127.0.0.1:9")).await;
        assert!(err.unwrap().contains("no LLM provider available"));
    }

    #[tokio::test]
    async fn breaker_opens_after_repeated_failures() {
        let sh = shared(&["ollama", "fallback"], "http://127.0.0.1:9");
        for _ in 0..3 {
            collect(sh.clone()).await;
        }
        assert!(sh.breakers["ollama"].is_open());
    }
}
