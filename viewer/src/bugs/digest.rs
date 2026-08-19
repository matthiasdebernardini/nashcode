//! The digest: one writer, behind the ingest response.
//!
//! Ingest's job is to get the bytes into the bucket and answer. Everything that
//! costs thought — splitting, parsing, grouping, indexing — happens here, on a single
//! task, so two events that group the same way can never race each other into two
//! issues. It is Bugsink's ingest/digest split, which is what lets a small box take a
//! large burst.
//!
//! Parsing is deliberately forgiving. The protocol requires only `event_id`,
//! `timestamp` and `platform` of a client, and says nothing else may be assumed, so
//! nothing here refuses an event for a missing field. The raw JSON in the bucket is
//! the source of truth; these rows are an index over it.

use std::sync::Arc;

use object_store::{ObjectStore, ObjectStoreExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::bugs::{envelope, group, index, store};
use crate::db::Db;

/// One accepted envelope, waiting to be understood.
#[derive(Debug)]
pub struct Job {
    pub project_id: i64,
    /// The envelope exactly as it arrived, decompressed.
    pub body: Vec<u8>,
}

/// What one envelope did. Returned for tests and for the counters slice 2 needs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub events: usize,
    /// Items whose type this build does not handle. Counted, never an error.
    pub skipped: usize,
    /// Items that failed on the way to the bucket or the index. Counted so the item
    /// beside them still lands — a transient error on item two used to drop item
    /// three with it.
    pub failed: usize,
}

pub struct Worker {
    pub db: Db,
    pub store: Arc<dyn ObjectStore>,
    pub digested: Arc<watch::Sender<u64>>,
}

impl Worker {
    /// Drain the queue until every sender is gone.
    pub async fn run(self, mut jobs: mpsc::UnboundedReceiver<Job>) {
        while let Some(job) = jobs.recv().await {
            let project_id = job.project_id;
            match self.digest(&job).await {
                Ok(outcome) => tracing::debug!(
                    project_id,
                    events = outcome.events,
                    skipped = outcome.skipped,
                    failed = outcome.failed,
                    "digested"
                ),
                // A digest failure loses an index row, never a payload: the envelope
                // is already in the bucket and a reindex re-reads it.
                Err(error) => tracing::warn!(project_id, %error, "cannot digest envelope"),
            }
            self.digested.send_modify(|done| *done += 1);
        }
    }

    async fn digest(&self, job: &Job) -> Result<Outcome, String> {
        let split = envelope::split(&job.body).map_err(|error| error.to_string())?;
        let mut outcome = Outcome::default();

        for item in &split.items {
            if item.ty != "event" {
                // `log`, `check_in`, `client_report` and the rest land in slice 2.
                // Everything else is counted and dropped, which the protocol requires.
                outcome.skipped += 1;
                continue;
            }
            let Ok(event) = serde_json::from_slice::<Value>(item.payload) else {
                outcome.skipped += 1;
                continue;
            };
            // One item's failure is that item's failure. The envelope is already in
            // the bucket, so the sweep can retry the whole thing later; abandoning
            // the items after this one would lose them for nothing.
            match self.record(job.project_id, &split, &event, item.payload).await {
                Ok(()) => outcome.events += 1,
                Err(error) => {
                    outcome.failed += 1;
                    tracing::warn!(project_id = job.project_id, %error, "cannot record an event");
                }
            }
        }

        Ok(outcome)
    }

    async fn record(
        &self,
        project_id: i64,
        split: &envelope::Envelope<'_>,
        event: &Value,
        raw: &[u8],
    ) -> Result<(), String> {
        let event_id = event
            .get("event_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| split.event_id())
            .unwrap_or_else(new_event_id);
        let event_id = sanitize_id(&event_id);

        // The detail view reads this object, not the database.
        let key = store::event_key(project_id, &event_id);
        self.store
            .put(&key, raw.to_vec().into())
            .await
            .map_err(|error| format!("cannot write {key}: {error}"))?;

        let grouped = group::group(event);
        index::record(
            &self.db,
            &index::NewEvent {
                project_id,
                event_id,
                object_key: key.to_string(),
                grouping_key: grouped.key,
                grouping_hash: grouped.hash,
                mechanism: grouped.mechanism.to_owned(),
                title: grouped.title,
                culprit: grouped.culprit,
                level: grouped.level,
                platform: event.get("platform").and_then(Value::as_str).map(str::to_owned),
                timestamp: timestamp(event),
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// `timestamp` arrives as RFC3339 or as Unix seconds, and both are legal. Keep
/// whichever the client sent, as text — this is an index field, not a clock.
fn timestamp(event: &Value) -> Option<String> {
    match event.get("timestamp")? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

pub fn new_event_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// An event id becomes part of an object key, so it may only be what the protocol
/// says it is: lowercase hex. Anything else gets a fresh id rather than a rejection.
fn sanitize_id(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.is_empty() { new_event_id() } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_timestamp_is_kept_in_whichever_form_it_arrived() {
        assert_eq!(
            timestamp(&json!({"timestamp": "2026-08-19T00:00:00Z"})).as_deref(),
            Some("2026-08-19T00:00:00Z")
        );
        assert_eq!(timestamp(&json!({"timestamp": 1755561600})).as_deref(), Some("1755561600"));
        assert_eq!(timestamp(&json!({})), None);
    }

    #[test]
    fn an_event_id_can_never_escape_its_object_key() {
        assert_eq!(sanitize_id("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_id("ABCD1234"), "abcd1234");
        for hostile in ["../../etc/passwd", "a/b", "..", "%2e%2e", "x\0y"] {
            let clean = sanitize_id(hostile);
            assert!(
                !clean.contains('/') && !clean.contains('.') && !clean.is_empty(),
                "{hostile} became {clean}"
            );
        }
        // Nothing usable left means a fresh id rather than an empty object key.
        assert_eq!(sanitize_id("").len(), 32);
        assert_eq!(sanitize_id("///").len(), 32);
    }
}
