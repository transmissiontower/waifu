//! memory-service: SQLite persistence, local embeddings + vector search, automatic
//! summarization of old conversation, and fact extraction about the user.
mod embed;
mod facts;
mod store;

use common::{grpc, Boot};
use contracts::llm_client::LlmClient;
use contracts::memory_server::{Memory, MemoryServer};
use contracts::*;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use store::Store;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Settings {
    db_path: String,
    /// "hash" (local, default) or "llm" (semantic embeddings via llm-service/Ollama)
    embedder: String,
    summarize_after: i64,
    keep_recent: i64,
    min_score: f32,
    extract_with_llm: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            db_path: String::new(),
            embedder: "hash".into(),
            summarize_after: 30,
            keep_recent: 10,
            min_score: 0.22,
            extract_with_llm: true,
        }
    }
}

struct Svc {
    db: Arc<Mutex<Store>>,
    st: Settings,
    llm: Option<LlmClient<tonic::transport::Channel>>,
    summarizing: Arc<AtomicBool>,
    state: common::ServiceState,
}

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

impl Svc {
    /// Embed with the configured embedder, falling back to the local hash embedder.
    async fn embed(&self, text: &str, trace: &str) -> (String, Vec<f32>) {
        if self.st.embedder == "llm" {
            if let Some(llm) = &self.llm {
                let mut c = llm.clone();
                if let Ok(Ok(r)) = tokio::time::timeout(
                    Duration::from_secs(8),
                    c.embed(grpc::traced(EmbedRequest { inputs: vec![text.into()] }, trace, Duration::from_secs(8))),
                )
                .await
                {
                    let r = r.into_inner();
                    if let Some(v) = r.vectors.into_iter().next() {
                        let mut vals = v.values;
                        embed::normalize(&mut vals);
                        return (r.model, vals);
                    }
                }
                self.state.degrade("embedder", "llm embeddings unavailable, using local hash embedder");
            }
        }
        ("hash".into(), embed::hash_embed(text))
    }

    async fn llm_collect(&self, system: &str, user: &str, trace: &str, secs: u64) -> Option<String> {
        let llm = self.llm.clone()?;
        let mut c = llm;
        let req = ChatRequest {
            messages: vec![
                ChatMessage { role: "system".into(), content: system.into() },
                ChatMessage { role: "user".into(), content: user.into() },
            ],
            temperature: 0.2,
            max_tokens: 400,
            model: String::new(),
        };
        let fut = async {
            let mut s = c.chat(grpc::traced(req, trace, Duration::from_secs(secs))).await.ok()?.into_inner();
            let mut text = String::new();
            let mut provider = String::new();
            while let Some(t) = s.next().await {
                let t = t.ok()?;
                text += &t.text;
                provider = t.provider;
            }
            // The built-in fallback brain cannot summarize or extract; treat as unavailable.
            (provider != "fallback").then_some(text)
        };
        tokio::time::timeout(Duration::from_secs(secs), fut).await.ok().flatten()
    }

    fn maybe_summarize(self_: Arc<Self>, user: String, trace: String) {
        let count = self_.db.lock().unwrap().unsummarized_count(&user).unwrap_or(0);
        if count <= self_.st.summarize_after || self_.summarizing.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            let r = self_.summarize(&user, &trace).await;
            self_.summarizing.store(false, Ordering::SeqCst);
            if let Err(e) = r {
                tracing::warn!(error = %e, "auto-summarization failed");
            }
        });
    }

    async fn summarize(&self, user: &str, trace: &str) -> anyhow::Result<Option<String>> {
        let (old, prev) = {
            let db = self.db.lock().unwrap();
            (db.summarizable(user, self.st.keep_recent)?, db.latest_summary(user)?)
        };
        if old.len() < 4 {
            return Ok(None);
        }
        let upto = old.last().unwrap().id;
        let transcript: String =
            old.iter().map(|m| format!("{}: {}\n", if m.role == "user" { "User" } else { "Companion" }, m.text)).collect();
        let prompt = format!("Previous summary:\n{prev}\n\nNew conversation:\n{transcript}");
        let summary = match self
            .llm_collect("Update the running summary of a conversation between a user and their companion. Keep it under 120 words, third person, focus on lasting facts, feelings, plans and shared jokes. Output only the summary.", &prompt, trace, 40)
            .await
        {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                self.state.degrade("summarizer", "LLM unavailable; using extractive summary");
                let mut s = prev.clone();
                for m in old.iter().filter(|m| m.role == "user") {
                    let snippet: String = m.text.chars().take(90).collect();
                    s += &format!(" User said: {snippet}.");
                }
                let n = s.chars().count();
                s.chars().skip(n.saturating_sub(1200)).collect::<String>().trim().to_string()
            }
        };
        let (tag, emb) = self.embed(&summary, trace).await;
        {
            let db = self.db.lock().unwrap();
            db.save_summary(user, &summary, upto)?;
            db.add_memory(user, None, "summary", &summary, &tag, &emb)?;
        }
        self.state.metrics.inc("summaries_total");
        tracing::info!(messages = old.len(), "conversation summarized");
        Ok(Some(summary))
    }
}

#[tonic::async_trait]
impl Memory for Svc {
    async fn recall(&self, req: Request<RecallRequest>) -> Result<Response<RecallResponse>, Status> {
        let trace = grpc::trace_id(&req);
        let r = req.into_inner();
        self.state.metrics.inc("recall_total");
        let (tag, qv) = self.embed(&r.query, &trace).await;
        let recent_n = if r.recent == 0 { 8 } else { r.recent };
        let top_k = if r.top_k == 0 { 4 } else { r.top_k as usize };
        let db = self.db.lock().unwrap();
        let recent = db.recent(&r.user_id, recent_n).map_err(internal)?;
        let exclude: Vec<i64> = recent.iter().map(|m| m.id).collect();
        let mems = if r.query.trim().is_empty() {
            vec![]
        } else {
            db.search(&r.user_id, &tag, &qv, top_k, self.st.min_score, &exclude).map_err(internal)?
        };
        Ok(Response::new(RecallResponse {
            memories: mems.into_iter().map(|m| contracts::MemoryItem { text: m.text, score: m.score, kind: m.kind, ts: m.ts }).collect(),
            facts: db.facts(&r.user_id).map_err(internal)?.into_iter().map(|(key, value)| Fact { key, value }).collect(),
            summary: db.latest_summary(&r.user_id).map_err(internal)?,
            recent: recent.into_iter().map(|m| ChatMessage { role: m.role, content: m.text }).collect(),
        }))
    }

    async fn store_turn(&self, req: Request<StoreTurnRequest>) -> Result<Response<Ack>, Status> {
        let trace = grpc::trace_id(&req);
        let r = req.into_inner();
        if r.text.trim().is_empty() {
            return Ok(Response::new(Ack { ok: true, message: "empty".into() }));
        }
        let id = self.db.lock().unwrap().add_message(&r.user_id, &r.role, &r.text).map_err(internal)?;
        if r.text.chars().count() >= 12 {
            let label = if r.role == "user" { "User" } else { "Companion" };
            let text = format!("{label}: {}", r.text);
            let (tag, emb) = self.embed(&text, &trace).await;
            self.db.lock().unwrap().add_memory(&r.user_id, Some(id), "message", &text, &tag, &emb).map_err(internal)?;
        }
        self.state.metrics.inc("turns_stored_total");
        // schedule summarization without blocking the reply
        let me = Arc::new(Svc {
            db: self.db.clone(),
            st: self.st.clone(),
            llm: self.llm.clone(),
            summarizing: self.summarizing.clone(),
            state: self.state.clone(),
        });
        Svc::maybe_summarize(me, r.user_id, trace);
        Ok(Response::new(Ack { ok: true, message: String::new() }))
    }

    async fn extract_facts(&self, req: Request<ExtractRequest>) -> Result<Response<ExtractResponse>, Status> {
        let trace = grpc::trace_id(&req);
        let r = req.into_inner();
        let mut found: Vec<facts::Fact> = vec![];
        if self.st.extract_with_llm {
            let sys = "Extract durable personal facts about the USER from the exchange (name, age, location, job, pets, likes, dislikes, plans, relationships). Reply with ONLY a JSON array like [{\"key\":\"name\",\"value\":\"Ken\"}]. Use short snake_case keys. Reply [] if there is nothing.";
            let user = format!("User: {}\nCompanion: {}", r.user_text, r.assistant_text);
            if let Some(out) = self.llm_collect(sys, &user, &trace, 25).await {
                found = facts::parse_llm_facts(&out);
            }
        }
        if found.is_empty() {
            found = facts::heuristic_facts(&r.user_text);
        }
        let db = self.db.lock().unwrap();
        for (k, v) in &found {
            db.upsert_fact(&r.user_id, k, v).map_err(internal)?;
        }
        self.state.metrics.add("facts_extracted_total", &[], found.len() as f64);
        Ok(Response::new(ExtractResponse { facts: found.into_iter().map(|(key, value)| Fact { key, value }).collect() }))
    }

    async fn list_facts(&self, req: Request<ListFactsRequest>) -> Result<Response<ExtractResponse>, Status> {
        let f = self.db.lock().unwrap().facts(&req.into_inner().user_id).map_err(internal)?;
        Ok(Response::new(ExtractResponse { facts: f.into_iter().map(|(key, value)| Fact { key, value }).collect() }))
    }

    async fn summarize(&self, req: Request<SummarizeRequest>) -> Result<Response<SummarizeResponse>, Status> {
        let trace = grpc::trace_id(&req);
        let s = self.summarize(&req.into_inner().user_id, &trace).await.map_err(internal)?;
        Ok(Response::new(SummarizeResponse { summarized: s.is_some(), summary: s.unwrap_or_default() }))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = Boot::init("memory")?;
    let mut st: Settings = boot.cfg.settings("memory");
    if st.db_path.is_empty() {
        st.db_path = format!("{}/memory.sqlite", boot.cfg.global.data_dir);
    }
    let db = Store::open(&st.db_path)?;
    tracing::info!(db = %st.db_path, embedder = %st.embedder, "memory store opened");
    let llm = boot.cfg.addr("llm").ok().filter(|_| boot.cfg.is_enabled("llm")).map(|a| LlmClient::new(grpc::channel(&a)));
    let svc = Svc { db: Arc::new(Mutex::new(db)), st, llm, summarizing: Arc::new(AtomicBool::new(false)), state: boot.state.clone() };
    boot.spawn_admin();
    let addr = boot.me.addr.parse()?;
    let token = boot.state.shutdown.clone();
    tracing::info!(%addr, "memory gRPC listening");
    tonic::transport::Server::builder()
        .add_service(MemoryServer::new(svc))
        .serve_with_shutdown(addr, async move { token.cancelled().await })
        .await?;
    tracing::info!("memory-service stopped");
    Ok(())
}
