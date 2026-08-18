//! Outgoing webhooks. dgit emits nothing, so the viewer's own poller and write paths
//! are the event source.
//!
//! Delivery is fire-and-forget: POST the JSON, 10 second timeout, one retry, failures
//! logged and dropped.
//! // ponytail: fire-and-forget; add a delivery table if a consumer ever needs replay

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub const PUSH: &str = "push";
pub const CI_FINISHED: &str = "ci_finished";
pub const MERGED: &str = "merged";
pub const RESTACKED: &str = "restacked";

/// The webhook sender. Cheap to clone; shared through the app context.
#[derive(Clone)]
pub struct Webhooks {
    hooks: Arc<BTreeMap<String, Vec<String>>>,
    client: reqwest::Client,
}

impl std::fmt::Debug for Webhooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Webhooks")
    }
}

impl Webhooks {
    pub fn new(hooks: BTreeMap<String, Vec<String>>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builds");
        Self { hooks: Arc::new(hooks), client }
    }

    /// Deliver `event` to every URL subscribed to it, in the background. The payload
    /// always carries an `event` field naming itself.
    pub fn send(&self, event: &str, mut payload: serde_json::Value) {
        let Some(urls) = self.hooks.get(event) else { return };
        if let Some(object) = payload.as_object_mut() {
            object.insert("event".to_owned(), serde_json::Value::String(event.to_owned()));
        }
        for url in urls.clone() {
            let client = self.client.clone();
            let body = payload.clone();
            let event = event.to_owned();
            tokio::spawn(async move {
                for attempt in 0..2 {
                    match client.post(&url).json(&body).send().await {
                        Ok(response) if response.status().is_success() => return,
                        Ok(response) => {
                            tracing::warn!(event, url, status = %response.status(), attempt, "webhook rejected");
                        }
                        Err(error) => {
                            tracing::warn!(event, url, %error, attempt, "webhook delivery failed");
                        }
                    }
                }
            });
        }
    }
}
