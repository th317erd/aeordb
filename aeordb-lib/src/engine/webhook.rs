use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use crate::engine::engine_event::EngineEvent;
use crate::engine::event_bus::EventBus;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::directory_ops::DirectoryOps;

const WEBHOOK_CONFIG_PATH: &str = "/.config/webhooks.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    pub secret: String,
    #[serde(default = "default_active")]
    pub active: bool,
}

fn default_active() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookRegistry {
    pub webhooks: Vec<WebhookConfig>,
}

/// Load webhook configuration from the database.
pub fn load_webhook_config(engine: &StorageEngine) -> Option<WebhookRegistry> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file(WEBHOOK_CONFIG_PATH) {
        Ok(data) => {
            let text = std::str::from_utf8(&data).ok()?;
            serde_json::from_str(text).ok()
        }
        Err(_) => None,
    }
}

/// Compute HMAC-SHA256 signature for a payload.
pub fn compute_signature(secret: &str, payload: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key size");
    mac.update(payload);
    let result = mac.finalize();
    format!("sha256={}", hex::encode(result.into_bytes()))
}

/// Check if an event matches a webhook's filters.
pub fn event_matches_webhook(event: &EngineEvent, webhook: &WebhookConfig) -> bool {
    if !webhook.active {
        return false;
    }

    // Check event type filter
    if !webhook.events.contains(&event.event_type) {
        return false;
    }

    // Check path prefix filter
    if let Some(ref prefix) = webhook.path_prefix {
        if let Some(entries) = event.payload.get("entries") {
            if let Some(arr) = entries.as_array() {
                let any_match = arr.iter().any(|e| {
                    e.get("path")
                        .and_then(|p| p.as_str())
                        .map(|p| p.starts_with(prefix.as_str()))
                        .unwrap_or(false)
                });
                if !any_match {
                    return false;
                }
            }
        }
    }

    true
}

/// Deliver an event to a webhook URL via HTTP POST.
async fn deliver_webhook(webhook: &WebhookConfig, event: &EngineEvent) {
    let payload = match serde_json::to_vec(event) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(webhook_id = %webhook.id, error = %e, "Failed to serialize webhook payload");
            return;
        }
    };

    let signature = compute_signature(&webhook.secret, &payload);

    let client = reqwest::Client::new();
    let result = client
        .post(&webhook.url)
        .header("Content-Type", "application/json")
        .header("X-AeorDB-Signature", &signature)
        .header("X-AeorDB-Event", &event.event_type)
        .header("X-AeorDB-Delivery", &event.event_id)
        .body(payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match result {
        Ok(response) => {
            tracing::debug!(
                webhook_id = %webhook.id,
                status = %response.status(),
                "Webhook delivered"
            );
        }
        Err(e) => {
            tracing::warn!(
                webhook_id = %webhook.id,
                url = %webhook.url,
                error = %e,
                "Webhook delivery failed"
            );
        }
    }
}

/// Spawn a background task that subscribes to the EventBus and delivers
/// matching events to registered webhook URLs.
///
/// The dispatcher loads webhook config from `/.config/webhooks.json` on start
/// and reloads it when that file changes.
pub fn spawn_webhook_dispatcher(
    bus: Arc<EventBus>,
    engine: Arc<StorageEngine>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    // Subscribe before spawning so we don't capture the Arc<EventBus> in the task.
    // This allows the channel to close when all external senders are dropped.
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        let mut config = load_webhook_config(&engine);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Webhook dispatcher shutting down (cancelled)");
                    break;
                }
                recv_result = rx.recv() => {
                    match recv_result {
                        Ok(event) => {
                            // Check if webhook config file changed -- reload if so
                            if event.event_type == "entries_created" || event.event_type == "entries_updated" {
                                if let Some(entries) = event.payload.get("entries") {
                                    if let Some(arr) = entries.as_array() {
                                        let config_changed = arr.iter().any(|e| {
                                            e.get("path")
                                                .and_then(|p| p.as_str())
                                                .map(|p| p == WEBHOOK_CONFIG_PATH)
                                                .unwrap_or(false)
                                        });
                                        if config_changed {
                                            config = load_webhook_config(&engine);
                                            tracing::info!("Webhook config reloaded");
                                            continue; // Don't deliver the config change itself
                                        }
                                    }
                                }
                            }

                            // Deliver to matching webhooks
                            if let Some(ref registry) = config {
                                for webhook in &registry.webhooks {
                                    if event_matches_webhook(&event, webhook) {
                                        let wh = webhook.clone();
                                        let evt = event.clone();
                                        tokio::spawn(async move {
                                            deliver_webhook(&wh, &evt).await;
                                        });
                                    }
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(missed = n, "Webhook dispatcher lagged, skipped events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!("EventBus closed, webhook dispatcher shutting down");
                            break;
                        }
                    }
                }
            }
        }
    })
}
