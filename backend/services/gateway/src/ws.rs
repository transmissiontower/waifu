//! WebSocket bridge: browser <-> orchestrator (gRPC bidi stream).
//!
//! Browser -> gateway: JSON text {"type":"hello"|"text"|"interrupt"|"audio_end"}, binary [0x02][pcm16 16kHz mono]
//! Gateway -> browser: JSON text events, binary [0x01][u32 sample_rate LE][u32 seq LE][pcm16]
use crate::App;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use contracts::avatar_event::Kind as AK;
use contracts::client_event::Kind as CK;
use contracts::orchestrator_client::OrchestratorClient;
use contracts::server_event::Kind as SK;
use contracts::*;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;

pub async fn handler(ws: WebSocketUpgrade, State(app): State<App>) -> impl IntoResponse {
    ws.max_message_size(4 << 20).on_upgrade(move |s| session(app, s))
}

pub fn avatar_event_json(ev: &AvatarEvent) -> Option<String> {
    let v = match ev.kind.as_ref()? {
        AK::Emotion(e) => json!({"type":"emotion","name":e.name,"intensity":e.intensity,"trace_id":ev.trace_id}),
        AK::State(s) => json!({"type":"state","state":s.state,"trace_id":ev.trace_id}),
        AK::Animation(a) => json!({"type":"animation","name":a.name}),
        AK::Lipsync(_) => return None, // the browser lip-syncs from the audio it actually plays
    };
    Some(v.to_string())
}

/// Translate an orchestrator event for the browser. Emotion/state are skipped when the avatar-service
/// stream is live (it is the single source of truth then), and used as a fallback otherwise.
pub fn to_ws(ev: ServerEvent, avatar_live: bool) -> Option<Message> {
    let trace = ev.trace_id;
    Some(match ev.kind? {
        SK::Transcript(t) => Message::Text(json!({"type":"transcript","text":t.text,"final":t.r#final,"trace_id":trace}).to_string()),
        SK::AssistantDelta(d) => Message::Text(json!({"type":"assistant_text","delta":d,"trace_id":trace}).to_string()),
        SK::Audio(a) => {
            let mut b = Vec::with_capacity(9 + a.pcm16.len());
            b.push(1u8);
            b.extend_from_slice(&a.sample_rate.to_le_bytes());
            b.extend_from_slice(&a.seq.to_le_bytes());
            b.extend_from_slice(&a.pcm16);
            Message::Binary(b)
        }
        SK::Emotion(e) if !avatar_live => Message::Text(json!({"type":"emotion","name":e.name,"intensity":e.intensity}).to_string()),
        SK::State(s) if !avatar_live => Message::Text(json!({"type":"state","state":s}).to_string()),
        SK::Emotion(_) | SK::State(_) => return None,
        SK::Notice(n) => Message::Text(json!({"type":"notice","level":n.level,"message":n.message}).to_string()),
        SK::TurnDone(_) => Message::Text(json!({"type":"turn_done","trace_id":trace}).to_string()),
    })
}

fn client_event(text: &str) -> Option<ClientEvent> {
    let v: Value = serde_json::from_str(text).ok()?;
    let kind = match v["type"].as_str()? {
        "hello" => CK::Hello(Hello {
            user_id: v["user_id"].as_str().unwrap_or("").into(),
            user_name: v["user_name"].as_str().unwrap_or("").into(),
            greet: v["greet"].as_bool().unwrap_or(false),
        }),
        "text" => CK::UserText(v["text"].as_str()?.chars().take(4000).collect()),
        "interrupt" => CK::Interrupt(Empty {}),
        "audio_end" => CK::AudioEnd(Empty {}),
        _ => return None,
    };
    Some(ClientEvent { kind: Some(kind) })
}

async fn note(tx: &mut futures::stream::SplitSink<WebSocket, Message>, level: &str, msg: &str) -> bool {
    tx.send(Message::Text(json!({"type":"notice","level":level,"message":msg}).to_string())).await.is_ok()
}

async fn session(app: App, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    app.state.metrics.inc("ws_connections_total");
    tracing::info!("websocket client connected");
    let persona_now = app.persona_now.lock().unwrap().clone();
    if let Some(p) = persona_now {
        let _ = ws_tx.send(Message::Text(crate::persona_json(&p).to_string())).await;
    }
    let mut avatar_rx = app.avatar_tx.subscribe();
    let mut persona_rx = app.persona_tx.subscribe();
    let mut last_hello: Option<ClientEvent> = None;
    let mut warned = false;

    'outer: loop {
        // (re)connect to the orchestrator
        let (to_orch, orch_rx) = mpsc::channel::<ClientEvent>(256);
        let mut client = OrchestratorClient::new(app.orch.clone()).max_decoding_message_size(16 << 20);
        let opened =
            tokio::time::timeout(Duration::from_secs(3), client.converse(tokio_stream::wrappers::ReceiverStream::new(orch_rx))).await;
        let mut inbound = match opened {
            Ok(Ok(r)) => {
                if warned {
                    app.state.recover("orchestrator");
                    let _ = note(&mut ws_tx, "info", "Reconnected to the conversation engine.").await;
                    warned = false;
                }
                r.into_inner()
            }
            _ => {
                app.state.degrade("orchestrator", "orchestrator unreachable");
                if !warned {
                    warned = true;
                    if !note(
                        &mut ws_tx,
                        "error",
                        "The conversation engine (orchestrator) is not reachable - retrying. Check the supervisor logs.",
                    )
                    .await
                    {
                        break;
                    }
                }
                // keep the socket alive while we retry; drop user input meanwhile
                let sleep = tokio::time::sleep(Duration::from_secs(2));
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => continue 'outer,
                        m = ws_rx.next() => match m {
                            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break 'outer,
                            Some(Ok(Message::Text(t))) => {
                                if let Some(ev) = client_event(&t) {
                                    if matches!(ev.kind, Some(CK::Hello(_))) { last_hello = Some(ev); }
                                    else { let _ = note(&mut ws_tx, "warn", "Not connected to the conversation engine yet - message dropped.").await; }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        };
        if let Some(h) = &last_hello {
            let _ = to_orch.send(h.clone()).await;
        }
        loop {
            tokio::select! {
                _ = app.state.shutdown.cancelled() => break 'outer,
                m = ws_rx.next() => match m {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break 'outer,
                    Some(Ok(Message::Text(t))) => {
                        if let Some(ev) = client_event(&t) {
                            if matches!(ev.kind, Some(CK::Hello(_))) { last_hello = Some(ev.clone()); }
                            if to_orch.send(ev).await.is_err() { continue 'outer; }
                        }
                    }
                    Some(Ok(Message::Binary(b))) => {
                        if b.first() == Some(&2) && b.len() > 1 {
                            let ev = ClientEvent { kind: Some(CK::UserAudio(AudioChunk { pcm16: b[1..].to_vec(), sample_rate: 16000, channels: 1, seq: 0, last: false })) };
                            if to_orch.send(ev).await.is_err() { continue 'outer; }
                        }
                    }
                    _ => {}
                },
                ev = inbound.message() => match ev {
                    Ok(Some(ev)) => {
                        if let Some(m) = to_ws(ev, app.avatar_connected.load(Ordering::SeqCst)) {
                            if ws_tx.send(m).await.is_err() { break 'outer; }
                        }
                    }
                    _ => {
                        app.state.degrade("orchestrator", "stream to orchestrator ended");
                        if !note(&mut ws_tx, "warn", "Lost the conversation engine - reconnecting...").await { break 'outer; }
                        warned = true;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue 'outer;
                    }
                },
                a = avatar_rx.recv() => {
                    if let Ok(j) = a { if ws_tx.send(Message::Text(j)).await.is_err() { break 'outer; } }
                },
                p = persona_rx.recv() => {
                    if let Ok(j) = p { if ws_tx.send(Message::Text(j)).await.is_err() { break 'outer; } }
                },
            }
        }
    }
    tracing::info!("websocket client disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_framing() {
        let m = to_ws(
            ServerEvent {
                trace_id: "t".into(),
                kind: Some(SK::Audio(AudioChunk { pcm16: vec![1, 2, 3, 4], sample_rate: 22050, channels: 1, seq: 7, last: false })),
            },
            true,
        )
        .unwrap();
        let Message::Binary(b) = m else { panic!() };
        assert_eq!(b[0], 1);
        assert_eq!(u32::from_le_bytes([b[1], b[2], b[3], b[4]]), 22050);
        assert_eq!(u32::from_le_bytes([b[5], b[6], b[7], b[8]]), 7);
        assert_eq!(&b[9..], &[1, 2, 3, 4]);
    }

    #[test]
    fn emotion_forwarded_only_when_avatar_service_is_down() {
        let ev = || ServerEvent { trace_id: String::new(), kind: Some(SK::Emotion(EmotionOut { name: "happy".into(), intensity: 1.0 })) };
        assert!(to_ws(ev(), true).is_none());
        assert!(matches!(to_ws(ev(), false), Some(Message::Text(t)) if t.contains("happy")));
    }

    #[test]
    fn parses_client_messages() {
        assert!(matches!(client_event(r#"{"type":"text","text":"hi"}"#).unwrap().kind, Some(CK::UserText(t)) if t == "hi"));
        assert!(matches!(client_event(r#"{"type":"hello","greet":true}"#).unwrap().kind, Some(CK::Hello(h)) if h.greet));
        assert!(client_event("garbage").is_none());
        assert!(client_event(r#"{"type":"nope"}"#).is_none());
    }
}
