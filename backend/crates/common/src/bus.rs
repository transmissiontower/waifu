//! Lightweight event bus for non-blocking notifications ("user spoke", "emotion changed"...).
//! In-process `broadcast` by default; mirrors to NATS when built with `--features nats` and
//! `global.bus: nats` is configured.
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    UserSpoke { trace_id: String, text: String },
    EmotionChanged { trace_id: String, emotion: String },
    TurnCompleted { trace_id: String, user_id: String, user_text: String, assistant_text: String },
    Interrupted { trace_id: String },
    PersonaChanged { version: u64 },
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(not(feature = "nats"), allow(dead_code))]
struct Envelope {
    origin: String,
    event: Event,
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    #[cfg(feature = "nats")]
    nats: Option<(async_nats::Client, String)>,
}

impl EventBus {
    pub fn in_process() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            tx,
            #[cfg(feature = "nats")]
            nats: None,
        }
    }

    /// Build from `global.bus` / `global.nats_url`. Falls back to in-process if NATS is unavailable.
    pub async fn from_config(g: &crate::config::Global) -> Self {
        let bus = Self::in_process();
        if g.bus != "nats" {
            return bus;
        }
        #[cfg(feature = "nats")]
        {
            let url = g.nats_url.clone().unwrap_or_else(|| "nats://127.0.0.1:4222".into());
            match async_nats::connect(&url).await {
                Ok(client) => {
                    let origin = crate::telemetry::new_trace_id();
                    let mut b = bus.clone();
                    b.nats = Some((client.clone(), origin.clone()));
                    use futures::StreamExt;
                    if let Ok(mut sub) = client.subscribe("waifu.events").await {
                        let tx = bus.tx.clone();
                        tokio::spawn(async move {
                            while let Some(m) = sub.next().await {
                                if let Ok(env) = serde_json::from_slice::<Envelope>(&m.payload) {
                                    if env.origin != origin {
                                        let _ = tx.send(env.event);
                                    }
                                }
                            }
                        });
                    }
                    tracing::info!(%url, "event bus: NATS connected");
                    return b;
                }
                Err(e) => tracing::warn!(error = %e, "NATS unavailable, using in-process bus"),
            }
        }
        #[cfg(not(feature = "nats"))]
        tracing::warn!("bus=nats requested but binary built without the `nats` feature; using in-process bus");
        bus
    }

    pub fn publish(&self, ev: Event) {
        #[cfg(feature = "nats")]
        if let Some((client, origin)) = &self.nats {
            if let Ok(payload) = serde_json::to_vec(&Envelope { origin: origin.clone(), event: ev.clone() }) {
                let c = client.clone();
                tokio::spawn(async move {
                    let _ = c.publish("waifu.events", payload.into()).await;
                });
            }
        }
        let _ = self.tx.send(ev); // no subscribers is fine
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn broadcast_delivers() {
        let bus = EventBus::in_process();
        let mut rx = bus.subscribe();
        bus.publish(Event::Interrupted { trace_id: "t".into() });
        assert_eq!(rx.recv().await.unwrap(), Event::Interrupted { trace_id: "t".into() });
    }
}
