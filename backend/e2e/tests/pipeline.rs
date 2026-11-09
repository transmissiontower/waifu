//! End-to-end test of the full pipeline using the REAL service binaries started by the REAL supervisor:
//!
//!   browser-like WebSocket client -> gateway -> orchestrator -> (stt, memory, persona, llm, tts, avatar)
//!
//! The only fake is the LLM backend: a tiny OpenAI-compatible server that streams tagged replies and
//! echoes the facts it finds in the system prompt - which proves memory -> persona -> LLM wiring.
//!
//! Prerequisite: `cargo build --workspace` (scripts/test.sh does it). Set WAIFU_BIN_DIR to test release builds.
use axum::{
    response::sse::{Event, Sse},
    routing::post,
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

const BASE: u16 = 41000;
fn port(off: u16) -> u16 {
    BASE + off
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

// ───────────────────────── mock OpenAI-compatible LLM ─────────────────────────
async fn mock_chat(Json(body): Json<Value>) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let system = msgs.iter().find(|m| m["role"] == "system").and_then(|m| m["content"].as_str()).unwrap_or("").to_string();
    let user = msgs.iter().rev().find(|m| m["role"] == "user").and_then(|m| m["content"].as_str()).unwrap_or("").to_lowercase();
    let name = system.lines().find_map(|l| l.strip_prefix("- name: ")).map(|s| s.trim().to_string());
    let (text, delay) = if user.contains("slow") {
        ("[thinking] Once upon a time there was a very long story. ".repeat(40), 120)
    } else if let Some(n) = name {
        (format!("[happy] Welcome back, {n}! [shy] Of course I remember you."), 4)
    } else {
        ("[happy] Nice to meet you! [thinking] Tell me about yourself, please.".to_string(), 4)
    };
    let chars: Vec<char> = text.chars().collect();
    let chunks: Vec<String> = chars.chunks(6).map(|c| c.iter().collect()).collect();
    let s = futures::stream::unfold((chunks.into_iter(), false), move |(mut it, sent_done)| async move {
        if sent_done {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(delay)).await;
        match it.next() {
            Some(c) => Some((Ok(Event::default().data(json!({"choices":[{"delta":{"content":c}}]}).to_string())), (it, false))),
            None => Some((Ok(Event::default().data("[DONE]")), (it, true))),
        }
    });
    Sse::new(s)
}

// ───────────────────────────── test harness ─────────────────────────────
struct System {
    child: std::process::Child,
    dir: PathBuf,
}

impl Drop for System {
    fn drop(&mut self) {
        // Graceful first (so the supervisor stops its children), SIGKILL only as a last resort.
        #[cfg(unix)]
        {
            let _ = std::process::Command::new("kill").arg("-INT").arg(self.child.id().to_string()).status();
            for _ in 0..80 {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bin_dir() -> PathBuf {
    std::env::var("WAIFU_BIN_DIR").map(PathBuf::from).unwrap_or_else(|_| root().join("target/debug"))
}

fn write_config(dir: &std::path::Path, mock_port: u16) -> PathBuf {
    let r = root();
    let mut cfg: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(r.join("config/services.yaml")).unwrap()).unwrap();
    let abs = |p: &str| serde_yaml::Value::String(r.join(p).display().to_string());
    cfg["global"]["models_dir"] = abs("assets/models");
    cfg["global"]["avatars_dir"] = abs("assets/avatars");
    cfg["global"]["voices_dir"] = abs("assets/voices");
    cfg["global"]["data_dir"] = serde_yaml::Value::String(dir.join("data").display().to_string());
    // ports
    let roles = ["gateway", "orchestrator", "llm", "stt", "tts", "memory", "persona", "avatar"];
    for (i, role) in roles.iter().enumerate() {
        let s = &mut cfg["services"][*role];
        if *role == "gateway" {
            s["addr"] = format!("127.0.0.1:{}", port(0)).into();
        } else {
            s["addr"] = format!("127.0.0.1:{}", port(10 + i as u16)).into();
            s["http"] = format!("127.0.0.1:{}", port(30 + i as u16)).into();
        }
    }
    cfg["services"]["gateway"]["settings"]["static_dir"] = abs("frontend/dist");
    // persona: a private copy so the hot-reload test can edit it
    std::fs::copy(r.join("config/personas/mio.yaml"), dir.join("persona.yaml")).unwrap();
    cfg["services"]["persona"]["settings"]["persona_file"] = dir.join("persona.yaml").display().to_string().into();
    // LLM: mock OpenAI-compatible server, then the built-in fallback
    cfg["services"]["llm"]["settings"]["providers"] = serde_yaml::from_str("[openai, fallback]").unwrap();
    cfg["services"]["llm"]["settings"]["openai"]["base_url"] = format!("http://127.0.0.1:{mock_port}/v1").into();
    // WAIFU_E2E_EXTERNAL_TTS=1: replace tts-service with the Python template - config-only swap, no rebuild
    if std::env::var("WAIFU_E2E_EXTERNAL_TTS").is_ok() {
        let tts_port = port(10 + 4);
        let script = r.join("backend/engines/template-python/tts_service.py").display().to_string();
        cfg["services"]["tts"] = serde_yaml::from_str(&format!(
            "{{impl: external, name: my-python-tts, addr: '127.0.0.1:{tts_port}', command: [python3, '{script}', --addr, '127.0.0.1:{tts_port}']}}"
        ))
        .unwrap();
    }
    // STT: echo engine (audio bytes carry UTF-8 text) so the audio path is testable without a model
    cfg["services"]["stt"]["settings"]["engine"] = "echo-test".into();
    let p = dir.join("config/services.yaml");
    std::fs::write(&p, serde_yaml::to_string(&cfg).unwrap()).unwrap();
    p
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Everything seen from the browser's point of view during one exchange.
#[derive(Default, Debug)]
struct Seen {
    text: String,
    audio_frames: usize,
    audio_peak: i16,
    sample_rate: u32,
    emotions: Vec<String>,
    states: Vec<String>,
    notices: Vec<String>,
    transcript: Option<String>,
    turn_done: bool,
}

async fn pump(ws: &mut Ws, seen: &mut Seen, until: Instant, mut stop: impl FnMut(&Seen) -> bool) {
    while Instant::now() < until && !stop(seen) {
        let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_millis(250), ws.next()).await else { continue };
        match m {
            Message::Binary(b) if b.first() == Some(&1) && b.len() > 9 => {
                seen.audio_frames += 1;
                seen.sample_rate = u32::from_le_bytes([b[1], b[2], b[3], b[4]]);
                for c in b[9..].chunks_exact(2) {
                    seen.audio_peak = seen.audio_peak.max(i16::from_le_bytes([c[0], c[1]]).abs());
                }
            }
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                match v["type"].as_str().unwrap_or("") {
                    "assistant_text" => seen.text += v["delta"].as_str().unwrap_or(""),
                    "emotion" => seen.emotions.push(v["name"].as_str().unwrap_or("").into()),
                    "state" => seen.states.push(v["state"].as_str().unwrap_or("").into()),
                    "notice" => seen.notices.push(v["message"].as_str().unwrap_or("").into()),
                    "transcript" => seen.transcript = v["text"].as_str().map(String::from),
                    "turn_done" => seen.turn_done = true,
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Collect events until `stop` is true (or timeout). If the turn finished, keep listening briefly:
/// avatar-service events (state/emotion) take one extra hop and may trail `turn_done`.
async fn recv_until(ws: &mut Ws, secs: u64, stop: impl FnMut(&Seen) -> bool) -> Seen {
    let mut seen = Seen::default();
    pump(ws, &mut seen, Instant::now() + Duration::from_secs(secs), stop).await;
    if seen.turn_done {
        pump(ws, &mut seen, Instant::now() + Duration::from_millis(600), |_| false).await;
    }
    seen
}

async fn say(ws: &mut Ws, text: &str) {
    ws.send(Message::Text(json!({"type":"text","text":text}).to_string())).await.unwrap();
}

async fn wait_ok(http: &reqwest::Client, secs: u64, mut ok: impl FnMut(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        if let Ok(r) = http.get(format!("http://127.0.0.1:{}/api/status", port(0))).send().await {
            if let Ok(v) = r.json::<Value>().await {
                if ok(&v) {
                    return v;
                }
                last = v;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("condition not reached in {secs}s; last status: {last}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_pipeline_voice_memory_avatar_degradation_and_shutdown() {
    let sup = bin_dir().join("supervisor");
    assert!(sup.exists(), "build first: `cargo build --workspace` (missing {})", sup.display());

    // mock LLM backend
    let mock = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_port = mock.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(mock, Router::new().route("/v1/chat/completions", post(mock_chat))).await.unwrap() });

    let dir = std::env::temp_dir().join(format!("waifu-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = write_config(&dir, mock_port);
    let child = std::process::Command::new(&sup)
        .arg("--config")
        .arg(&cfg)
        .arg("--root")
        .arg(&dir)
        .arg("--bin-dir")
        .arg(bin_dir())
        .spawn()
        .expect("spawn supervisor");
    let mut system = System { child, dir: dir.clone() };

    let http = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().unwrap();
    let status = wait_ok(&http, 120, |v| {
        v["services"].as_object().map(|m| m.len() == 8 && m.values().all(|s| s["status"] != "down")).unwrap_or(false)
    })
    .await;
    eprintln!(
        "services up: {}",
        status["services"].as_object().unwrap().iter().map(|(k, v)| format!("{k}={}", v["status"])).collect::<Vec<_>>().join(" ")
    );

    // ── connect like the browser does ──
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws", port(0))).await.expect("ws connect");
    ws.send(Message::Text(json!({"type":"hello","user_id":"e2e-user","greet":false}).to_string())).await.unwrap();
    // persona arrives on connect (hot-reload channel)
    let first = recv_until(&mut ws, 5, |_| false).await;
    let _ = first;

    // ── 1. text turn: tokens -> emotion tags -> TTS audio -> avatar events ──
    say(&mut ws, "Hi! My name is Ken and I love ramen.").await;
    let s = recv_until(&mut ws, 60, |s| s.turn_done).await;
    eprintln!(
        "turn1: text={:?} frames={} peak={} sr={} emotions={:?} states={:?} notices={:?}",
        s.text, s.audio_frames, s.audio_peak, s.sample_rate, s.emotions, s.states, s.notices
    );
    assert!(s.turn_done, "turn must complete");
    assert!(s.text.contains("Nice to meet you"), "streamed text: {:?}", s.text);
    assert!(!s.text.contains('['), "emotion tags must never reach the screen/voice: {:?}", s.text);
    assert!(s.audio_frames >= 1 && s.audio_peak > 500 && s.sample_rate >= 16000, "audio must stream with real samples");
    assert!(s.emotions.iter().any(|e| e == "happy"), "emotion from tag drives avatar: {:?}", s.emotions);
    assert!(s.states.iter().any(|x| x == "speaking") && s.states.iter().any(|x| x == "idle"), "avatar states: {:?}", s.states);

    // ── 2. memory: fact extracted + stored ──
    let mem_ch = common::grpc::channel(&format!("127.0.0.1:{}", port(10 + 5)));
    let mut mem = contracts::memory_client::MemoryClient::new(mem_ch);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut facts = vec![];
    while Instant::now() < deadline {
        facts = mem
            .list_facts(contracts::ListFactsRequest { user_id: "e2e-user".into() })
            .await
            .map(|r| r.into_inner().facts)
            .unwrap_or_default();
        if facts.iter().any(|f| f.key == "name" && f.value == "Ken") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(facts.iter().any(|f| f.key == "name" && f.value == "Ken"), "fact extraction: {facts:?}");
    let rec = mem
        .recall(contracts::RecallRequest { user_id: "e2e-user".into(), query: "ramen".into(), top_k: 3, recent: 0 })
        .await
        .unwrap()
        .into_inner();
    assert!(rec.recent.len() >= 2, "turns persisted: {:?}", rec.recent);

    // ── 3. memory reaches the LLM prompt via persona-service ──
    say(&mut ws, "Do you remember me?").await;
    let s = recv_until(&mut ws, 60, |s| s.turn_done).await;
    eprintln!("turn2: {:?}", s.text);
    assert!(s.text.contains("Welcome back, Ken"), "memory -> persona prompt -> LLM: {:?}", s.text);

    // ── 4. voice input path: audio -> STT -> pipeline ──
    let mut audio = vec![2u8];
    let mut payload = b"what is the moon made of".to_vec();
    payload.resize(3600, 0);
    audio.extend(payload);
    ws.send(Message::Binary(audio)).await.unwrap();
    ws.send(Message::Text(json!({"type":"audio_end"}).to_string())).await.unwrap();
    let s = recv_until(&mut ws, 60, |s| s.turn_done).await;
    assert_eq!(s.transcript.as_deref(), Some("what is the moon made of"), "STT transcript event");
    assert!(s.turn_done && !s.text.is_empty(), "reply after voice input");

    // ── 5. interruption (barge-in) stops speech quickly ──
    say(&mut ws, "tell me a slow story").await;
    let s = recv_until(&mut ws, 30, |s| !s.text.is_empty() && s.audio_frames > 0).await;
    assert!(!s.text.is_empty(), "story started");
    ws.send(Message::Text(json!({"type":"interrupt"}).to_string())).await.unwrap();
    let s = recv_until(&mut ws, 5, |s| s.states.iter().any(|x| x == "idle")).await;
    assert!(s.states.iter().any(|x| x == "idle"), "idle after interrupt: {:?}", s.states);
    let tail = recv_until(&mut ws, 2, |_| false).await;
    assert!(tail.text.len() < 80 && !tail.turn_done, "generation must stop after interrupt, got {:?}", tail.text);

    // ── 6. graceful degradation: kill memory-service mid-session, chat continues ──
    let r = http.post(format!("http://127.0.0.1:{}/admin/shutdown", port(30 + 5))).send().await.unwrap();
    assert!(r.status().is_success());
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&mut ws, "Are you still there?").await;
    let s = recv_until(&mut ws, 60, |s| s.turn_done).await;
    eprintln!("degraded turn: text={:?} notices={:?}", s.text, s.notices);
    assert!(s.turn_done && !s.text.is_empty(), "chat must continue without memory-service");
    // the supervisor brings it back
    wait_ok(&http, 60, |v| v["services"]["memory"]["status"] == "ok").await;

    // ── 7. persona hot-reload ──
    let p = dir.join("persona.yaml");
    let y = std::fs::read_to_string(&p).unwrap().replace("name: Mio", "name: Aiko");
    std::fs::write(&p, y).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut name = String::new();
    while Instant::now() < deadline {
        name = http.get(format!("http://127.0.0.1:{}/api/persona", port(0))).send().await.unwrap().json::<Value>().await.unwrap()["name"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if name == "Aiko" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(name, "Aiko", "persona.yaml edit must hot-reload");

    // ── 8. metrics + health endpoints ──
    let m = http.get(format!("http://127.0.0.1:{}/metrics", port(30 + 1))).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("waifu_turns_total{service=\"orchestrator\"}"), "metrics: {m}");

    // ── 9. graceful shutdown of the whole system on SIGINT ──
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill").arg("-INT").arg(system.child.id().to_string()).status();
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut exited = false;
        while Instant::now() < deadline {
            if system.child.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(exited, "supervisor must exit after SIGINT");
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", port(0))).await.is_err(), "gateway must be down");
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", port(30 + 5))).await.is_err(), "memory must be down");
    }
    let _ = std::fs::remove_dir_all(&system.dir);
}
