//! Optional VTube Studio bridge (https://github.com/DenchiSoft/VTubeStudio).
//! Emotions -> hotkey triggers, lip-sync envelope -> `MouthOpen` parameter injection.
use contracts::avatar_event::Kind;
use contracts::AvatarEvent;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Vts {
    pub enabled: bool,
    pub url: String,
    pub token_file: String,
    /// emotion name -> VTube Studio hotkey name or id
    pub emotion_hotkeys: BTreeMap<String, String>,
    pub mouth_parameter: String,
}
impl Default for Vts {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "ws://127.0.0.1:8001".into(),
            token_file: "data/vts_token.txt".into(),
            emotion_hotkeys: BTreeMap::new(),
            mouth_parameter: "MouthOpen".into(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub vtube_studio: Vts,
}

pub fn request(kind: &str, data: Value) -> String {
    json!({"apiName":"VTubeStudioPublicAPI","apiVersion":"1.0","requestID":kind,"messageType":kind,"data":data}).to_string()
}

/// Translate an avatar event into zero or more VTube Studio requests (lip-sync is handled separately).
pub fn messages_for(ev: &AvatarEvent, cfg: &Vts) -> Vec<String> {
    match &ev.kind {
        Some(Kind::Emotion(e)) => {
            cfg.emotion_hotkeys.get(&e.name).map(|hk| vec![request("HotkeyTriggerRequest", json!({ "hotkeyID": hk }))]).unwrap_or_default()
        }
        _ => vec![],
    }
}

pub async fn run(cfg: Vts, tx: broadcast::Sender<AvatarEvent>, state: common::ServiceState) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if state.shutdown.is_cancelled() {
            return;
        }
        match session(&cfg, &tx, &state).await {
            Ok(()) => return,
            Err(e) => {
                state.degrade("vtube_studio", format!("bridge disconnected: {e}"));
                tokio::select! {
                    _ = state.shutdown.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn session(cfg: &Vts, tx: &broadcast::Sender<AvatarEvent>, state: &common::ServiceState) -> anyhow::Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(&cfg.url).await?;
    let token = std::fs::read_to_string(&cfg.token_file).unwrap_or_default().trim().to_string();
    let token = if token.is_empty() {
        tracing::info!("requesting VTube Studio plugin token - click 'Allow' in VTube Studio");
        ws.send(Message::text(request("AuthenticationTokenRequest", json!({"pluginName":"Waifu","pluginDeveloper":"waifu"})))).await?;
        let reply = next_json(&mut ws).await?;
        let t = reply["data"]["authenticationToken"].as_str().ok_or_else(|| anyhow::anyhow!("token denied"))?.to_string();
        if let Some(parent) = std::path::Path::new(&cfg.token_file).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&cfg.token_file, &t);
        t
    } else {
        token
    };
    ws.send(Message::text(request(
        "AuthenticationRequest",
        json!({"pluginName":"Waifu","pluginDeveloper":"waifu","authenticationToken":token}),
    )))
    .await?;
    let reply = next_json(&mut ws).await?;
    if reply["data"]["authenticated"] != json!(true) {
        let _ = std::fs::remove_file(&cfg.token_file);
        anyhow::bail!("VTube Studio authentication failed (token cleared; will re-request)");
    }
    state.recover("vtube_studio");
    tracing::info!(url = %cfg.url, "VTube Studio bridge connected");
    let mut rx = tx.subscribe();
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return Ok(()),
            ev = rx.recv() => {
                let Ok(ev) = ev else { continue };
                for m in messages_for(&ev, cfg) { ws.send(Message::text(m)).await?; }
                if let Some(Kind::Lipsync(l)) = &ev.kind {
                    // Play the envelope in real time.
                    tokio::time::sleep(Duration::from_millis(l.start_ms as u64)).await;
                    let step = Duration::from_millis(1000 / l.fps.max(1) as u64);
                    for v in l.mouth.iter() {
                        let msg = request("InjectParameterDataRequest", json!({"mode":"set","parameterValues":[{"id": cfg.mouth_parameter, "value": *v as f32 / 255.0}]}));
                        ws.send(Message::text(msg)).await?;
                        tokio::time::sleep(step).await;
                    }
                }
            }
            incoming = ws.next() => { if incoming.is_none() { anyhow::bail!("closed by VTube Studio"); } }
        }
    }
}

async fn next_json<S>(ws: &mut S) -> anyhow::Result<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(m) = ws.next().await {
        if let Message::Text(t) = m? {
            return Ok(serde_json::from_str(t.as_str())?);
        }
    }
    anyhow::bail!("connection closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use contracts::EmotionEvent;

    #[test]
    fn emotion_maps_to_hotkey() {
        let mut cfg = Vts::default();
        cfg.emotion_hotkeys.insert("happy".into(), "Smile".into());
        let ev = AvatarEvent { trace_id: "t".into(), kind: Some(Kind::Emotion(EmotionEvent { name: "happy".into(), intensity: 1.0 })) };
        let m = messages_for(&ev, &cfg);
        assert_eq!(m.len(), 1);
        assert!(m[0].contains("HotkeyTriggerRequest") && m[0].contains("Smile"));
        let ev2 = AvatarEvent { trace_id: "t".into(), kind: Some(Kind::Emotion(EmotionEvent { name: "sad".into(), intensity: 1.0 })) };
        assert!(messages_for(&ev2, &cfg).is_empty());
    }
}
