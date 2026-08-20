//! Error tracking. nashcode is the Sentry server.
//!
//! The contract is `goals/error-tracking/goal.md`; `viewer/SPEC.md` binds the surface.
//! The shape in one paragraph: a project is a DSN, an unmodified official SDK posts
//! envelopes to `/api/<project_id>/envelope/`, the raw bytes go straight to an object
//! bucket and the request is answered, and a single digest task then groups the
//! events into issues. SQLite holds only the index. Losing it loses no payloads.
//!
//! Everything here is off unless `NASHCODE_BUGS_BUCKET` names a bucket. That is not a
//! degraded mode: with no bucket there is nowhere durable to put a payload, so the
//! honest answer is 404 and one line at startup.

pub mod context;
pub mod digest;
pub mod drain;
pub mod envelope;
pub mod group;
pub mod index;
pub mod ingest;
#[cfg(feature = "drain-iroh")]
pub mod iroh;
pub mod logs;
pub mod pushover;
pub mod selfreport;
pub mod store;

/// The iroh transport with the feature off. It exists so [`drain::transport_for`] can
/// name it unconditionally and so an operator who configured an EndpointId hears why
/// nothing is happening, rather than watching a drainer that never drains.
#[cfg(not(feature = "drain-iroh"))]
pub mod iroh {
    use std::sync::Arc;

    use crate::bugs::drain::Transport;
    use crate::config::Drain as DrainConfig;

    pub async fn transport(_config: &DrainConfig) -> Result<Arc<dyn Transport>, String> {
        Err("NASHCODE_BUGS_DRAIN looks like an iroh EndpointId, and this binary was \
             built without the `drain-iroh` feature. Rebuild it with \
             `--features drain-iroh`, or point NASHCODE_BUGS_DRAIN at an http:// URL."
            .to_owned())
    }
}

use std::sync::{Arc, Mutex};

use object_store::{ObjectStore, ObjectStoreExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

use crate::config::Config;
use crate::db::{Db, DbResult};

pub use index::{EventRow, Issue, Landing, Project, ProjectSummary, state};
pub use logs::{LogRow, Query as LogQuery, Record as LogRecord};

/// The error-tracking feature: the bucket, the index, and the digest queue.
///
/// Cheap to clone and always constructible — a build with no bucket configured makes
/// a disabled one rather than an `Option`, so every call site reads the same.
#[derive(Clone)]
pub struct Bugs {
    inner: Arc<Inner>,
}

/// How many accepted envelopes may be waiting for the digest at once.
///
/// The queue used to be unbounded, which turns a burst into two copies of itself in
/// memory — one in the bucket write, one waiting here — and hides the backlog until
/// the box swaps. Bounded, a full queue is a fact the client is told: 429, which is
/// the one answer every SDK already knows how to obey.
pub const QUEUE_DEPTH: usize = 1024;

/// How many bytes of undigested envelope may be in memory at once.
///
/// The slot count alone bounds nothing that matters: 1024 slots times a 100 MiB
/// decompressed body is 100 GiB. Bodies are what cost memory, so bodies are what the
/// budget is denominated in. It has to exceed [`ingest::MAX_DECOMPRESSED`] or a single
/// legal envelope could never be admitted at all.
pub const QUEUE_BYTES: usize = 256 * 1024 * 1024;

/// How many unfinished envelopes one sweep picks up. A backlog larger than this is
/// not a crash, it is a broken bucket, and re-reading a million objects at startup
/// would keep the viewer from answering anything at all.
pub const SWEEP_LIMIT: i64 = 10_000;

struct Inner {
    db: Db,
    store: Option<Arc<dyn ObjectStore>>,
    /// The origin that goes into a DSN.
    ingest_url: String,
    jobs: mpsc::Sender<digest::Job>,
    /// The digest task's receiver, until the task takes it. The worker starts on the
    /// first envelope rather than at construction, because construction happens
    /// outside the async runtime in both `main` and the tests.
    pending: Mutex<Option<mpsc::Receiver<digest::Job>>>,
    /// One permit per byte of queued body. Held by the job and released when it is
    /// dropped, which is after the digest is done with it.
    bytes: Arc<Semaphore>,
    digested: Arc<watch::Sender<u64>>,
    /// Decides which state changes are worth a person's attention. Off when no
    /// Pushover credentials are configured, which costs the digest one branch.
    notify: pushover::Notifier,
}

/// Why an envelope was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptError {
    /// The digest queue is full. The client is asked to come back.
    Busy,
    /// The bucket refused the write, so there is nowhere durable to put the payload.
    Store(String),
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("the digest queue is full"),
            Self::Store(detail) => f.write_str(detail),
        }
    }
}

impl std::fmt::Debug for Bugs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.enabled() { "Bugs(on)" } else { "Bugs(off)" })
    }
}

impl Bugs {
    /// Wire the feature from configuration. A bucket that will not open is a
    /// configuration error, logged and then treated as "off" — the viewer's other
    /// twenty jobs are not worth failing to start over.
    pub fn new(config: &Config, db: Db) -> DbResult<Self> {
        index::migrate(&db)?;
        logs::migrate(&db)?;
        drain::migrate(&db)?;
        pushover::migrate(&db)?;
        let store = match &config.bugs_bucket {
            None => None,
            Some(bucket) => match store::open(bucket, config.bugs_s3_endpoint.as_deref()) {
                Ok(store) => Some(store),
                Err(error) => {
                    tracing::error!(%error, "bugs: the bucket will not open; error tracking is off");
                    None
                }
            },
        };
        let (jobs, pending) = mpsc::channel(QUEUE_DEPTH);
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                store,
                ingest_url: config.bugs_ingest_url.clone(),
                jobs,
                pending: Mutex::new(Some(pending)),
                bytes: Arc::new(Semaphore::new(QUEUE_BYTES)),
                digested: Arc::new(watch::channel(0).0),
                notify: pushover::Notifier::new(config),
            }),
        })
    }

    /// Is there a bucket? Every route answers 404 when there is not.
    pub fn enabled(&self) -> bool {
        self.inner.store.is_some()
    }

    fn db(&self) -> &Db {
        &self.inner.db
    }

    // ---- projects ----------------------------------------------------------------

    /// Create a project and mint its key. The name is the URL segment and part of
    /// every object key, so it has to be a plain slug.
    pub fn create_project(&self, name: &str, repo: Option<&str>) -> Result<Project, String> {
        let name = name.trim();
        if !valid_project_name(name) {
            return Err(
                "a project name is 1-64 characters of lowercase letters, digits, `-`, `_` or `.`"
                    .to_owned(),
            );
        }
        if index::project_by_name(self.db(), name).map_err(|e| e.to_string())?.is_some() {
            return Err(format!("there is already a project called {name}"));
        }
        index::create_project(self.db(), name, &mint_key(), repo).map_err(|error| error.to_string())
    }

    pub fn projects(&self) -> DbResult<Vec<ProjectSummary>> {
        index::projects(self.db())
    }

    pub fn project(&self, name: &str) -> DbResult<Option<Project>> {
        index::project_by_name(self.db(), name)
    }

    pub fn project_by_id(&self, id: i64) -> DbResult<Option<Project>> {
        index::project_by_id(self.db(), id)
    }

    /// Revoke a project's key, or give it back. The public ingester learns about it on
    /// the next registry push; the tailnet door sees it immediately.
    pub fn set_project_active(&self, id: i64, active: bool) -> DbResult<bool> {
        index::set_project_active(self.db(), id, active)
    }

    /// The connection string an SDK is configured with. No secret half: it has been
    /// deprecated for years and minting one would only invite an SDK to send it.
    pub fn dsn(&self, project: &Project) -> String {
        let origin = self.inner.ingest_url.trim_end_matches('/');
        match origin.split_once("://") {
            Some((scheme, host)) => format!("{scheme}://{}@{host}/{}", project.key, project.id),
            None => format!("https://{}@{origin}/{}", project.key, project.id),
        }
    }

    // ---- issues ------------------------------------------------------------------

    pub fn issues(&self, project_id: i64, state: Option<&str>) -> DbResult<Vec<Issue>> {
        index::issues(self.db(), project_id, state)
    }

    pub fn issue(&self, project_id: i64, id: i64) -> DbResult<Option<Issue>> {
        index::issue(self.db(), project_id, id)
    }

    pub fn set_state(
        &self,
        project_id: i64,
        id: i64,
        state: &str,
        actor: &str,
    ) -> DbResult<Option<Issue>> {
        index::set_state(self.db(), project_id, id, state, actor)
    }

    pub fn events(&self, issue_id: i64, limit: i64) -> DbResult<Vec<EventRow>> {
        index::events(self.db(), issue_id, limit)
    }

    pub fn latest_event(&self, issue_id: i64) -> DbResult<Option<EventRow>> {
        index::latest_event(self.db(), issue_id)
    }

    /// Read a payload back out of the bucket.
    pub async fn payload(&self, object_key: &str) -> Result<Vec<u8>, String> {
        let store = self.inner.store.as_ref().ok_or("error tracking is off")?;
        let path = object_store::path::Path::from(object_key);
        let got = store.get(&path).await.map_err(|error| error.to_string())?;
        Ok(got.bytes().await.map_err(|error| error.to_string())?.to_vec())
    }

    // ---- ingest ------------------------------------------------------------------

    /// Put the raw envelope in the bucket and queue it for digest. The bucket write
    /// is the durable step; everything after it can be redone from the object.
    ///
    /// The queue slot is taken *first*. A full queue means the box is behind, and
    /// answering 429 before writing anything keeps the bucket free of objects nobody
    /// asked for; the SDK retries, which is what it does with a 429 anyway.
    pub async fn accept(&self, project_id: i64, body: Vec<u8>) -> Result<(), AcceptError> {
        self.start_digest();
        let Ok(slot) = self.inner.jobs.try_reserve() else {
            return Err(AcceptError::Busy);
        };
        let Some(bytes) = self.reserve_bytes(body.len()) else {
            return Err(AcceptError::Busy);
        };
        let (envelope_id, envelope_key, body) = self.store(project_id, body).await?;
        slot.send(digest::Job {
            project_id,
            envelope_id: Some(envelope_id),
            envelope_key,
            body,
            _bytes: Some(bytes),
        });
        Ok(())
    }

    /// Take `len` bytes of the queue's memory budget, or refuse.
    fn reserve_bytes(&self, len: usize) -> Option<OwnedSemaphorePermit> {
        let wanted = u32::try_from(len).ok()?;
        self.inner.bytes.clone().try_acquire_many_owned(wanted).ok()
    }

    /// The durable half of [`Self::accept`] on its own: the object goes to the bucket
    /// and the row is recorded, with nothing queued.
    ///
    /// A live request always wants `accept`. This is for the paths that mean to leave
    /// the digest to the sweep — an imported backlog, and the crash a sweep exists to
    /// repair. Hands back the row id and the body.
    ///
    /// **The index write is part of durability, not bookkeeping.** It used to be
    /// logged and swallowed, on the reasoning that the object was safe. It is not: the
    /// object with no `bugs_envelopes` row is invisible to the sweep, which is the only
    /// thing that would ever look at it again, so a crash before the in-memory digest
    /// finishes loses the event outright. Under the drainer that is worse still — the
    /// caller would ack, and the edge would delete the only other copy. Failing here
    /// makes the drainer defer instead, and the row comes back next cycle.
    pub async fn store(
        &self,
        project_id: i64,
        body: Vec<u8>,
    ) -> Result<(i64, String, Vec<u8>), AcceptError> {
        let store = self
            .inner
            .store
            .as_ref()
            .ok_or_else(|| AcceptError::Store("error tracking is off".to_owned()))?;
        let key = store::envelope_key(project_id, &crate::db::now(), &mint_key());
        store
            .put(&key, body.clone().into())
            .await
            .map_err(|error| AcceptError::Store(format!("cannot write {key}: {error}")))?;
        match index::record_envelope(self.db(), project_id, key.as_ref()) {
            Ok(id) => Ok((id, key.to_string(), body)),
            Err(error) => Err(AcceptError::Store(format!(
                "wrote {key} but cannot record it, so nothing would ever digest it: {error}"
            ))),
        }
    }

    /// Re-digest every envelope the digest never finished.
    ///
    /// A crash between the bucket write and the index write leaves the object in the
    /// bucket and the row without a `digested_at`; nothing else would ever look at it
    /// again. Run at startup. `all` re-reads every envelope instead, which is what
    /// rebuilding the index from the bucket would need.
    pub async fn sweep(&self, all: bool) -> usize {
        let Some(store) = self.inner.store.clone() else { return 0 };
        let pending = match index::undigested(self.db(), all, SWEEP_LIMIT) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "bugs: cannot list the undigested envelopes");
                return 0;
            }
        };
        if pending.is_empty() {
            return 0;
        }
        self.start_digest();

        let mut queued = 0;
        for row in pending {
            let path = object_store::path::Path::from(row.object_key.as_str());
            let body = match store.get(&path).await {
                Ok(got) => match got.bytes().await {
                    Ok(bytes) => bytes.to_vec(),
                    Err(error) => {
                        tracing::warn!(%error, key = row.object_key, "bugs: cannot read an envelope object");
                        continue;
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, key = row.object_key, "bugs: cannot read an envelope object");
                    continue;
                }
            };
            // `send`, not `try_send`: the worker is draining alongside this loop, so
            // waiting for a slot is the point rather than a failure.
            // `acquire`, not `try_acquire`: the worker drains alongside this loop, so
            // waiting for the memory is the point rather than a failure.
            let bytes = match u32::try_from(body.len()) {
                Ok(wanted) => self.inner.bytes.clone().acquire_many_owned(wanted).await.ok(),
                Err(_) => None,
            };
            if self
                .inner
                .jobs
                .send(digest::Job {
                    project_id: row.project_id,
                    envelope_id: Some(row.id),
                    envelope_key: row.object_key.clone(),
                    body,
                    _bytes: bytes,
                })
                .await
                .is_err()
            {
                break;
            }
            queued += 1;
        }
        tracing::info!(queued, "bugs: re-queued envelopes the digest never finished");
        queued
    }

    // ---- logs --------------------------------------------------------------------

    /// Archive a batch of log lines and index them. The NDJSON door calls this
    /// directly; the envelope door reaches it through the digest.
    pub async fn accept_logs(
        &self,
        project_id: i64,
        records: &[logs::Record],
        from: &str,
    ) -> Result<usize, String> {
        self.accept_logs_with_origin(project_id, records, from, None).await
    }

    /// The same, with the batch's dedupe identity named by the caller.
    ///
    /// `origin` is what a log row's `dedupe_key` is built from. `None` falls back to the
    /// freshly minted archive key, which is right for the direct NDJSON door — each POST
    /// is its own event and there is nothing stable to name it by — and catastrophic for
    /// anything that can be replayed. The drainer passes `drain/<project>/<seq>`, which
    /// the edge guarantees never repeats and never rewinds, so a redelivered batch lands
    /// on the same key and `INSERT OR IGNORE` throws it away. Without it every
    /// at-least-once redelivery duplicated every line.
    pub async fn accept_logs_with_origin(
        &self,
        project_id: i64,
        records: &[logs::Record],
        from: &str,
        origin: Option<&str>,
    ) -> Result<usize, String> {
        let store = self.inner.store.as_ref().ok_or("error tracking is off")?;
        logs::store_batch(self.db(), store, project_id, records, from, origin).await
    }

    pub fn logs(&self, project_id: i64, query: &logs::Query) -> DbResult<Vec<logs::LogRow>> {
        logs::search(self.db(), project_id, query)
    }

    pub fn log_count(&self, project_id: i64) -> DbResult<i64> {
        logs::count(self.db(), project_id)
    }

    /// Drop hot rows past each project's `retention_days`. The nightly job; the
    /// bucket archive is untouched.
    pub fn prune_logs(&self) -> DbResult<usize> {
        logs::prune(self.db())
    }

    // ---- notifications ---------------------------------------------------------------

    /// What is left of the Pushover allowance, what is parked, and what is waiting.
    /// Rendered on `/bugs` and carried in its JSON: the monthly budget is the number
    /// that decides whether the next real incident reaches a phone.
    pub fn push_budget(&self) -> DbResult<pushover::Budget> {
        pushover::budget(self.db())
    }

    /// True when credentials are configured. The page says so either way, because
    /// "nothing has been sent" and "nothing can be sent" are different problems.
    pub fn notifies(&self) -> bool {
        self.inner.notify.enabled()
    }

    /// The notifier the digest uses, for the paths outside the digest that also change
    /// an issue's state.
    pub fn notifier(&self) -> &pushover::Notifier {
        &self.inner.notify
    }

    /// Start the digest task, once.
    fn start_digest(&self) {
        let Some(store) = self.inner.store.clone() else { return };
        let taken = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(jobs) = taken {
            let worker = digest::Worker {
                db: self.inner.db.clone(),
                store,
                digested: self.inner.digested.clone(),
                notify: self.inner.notify.clone(),
            };
            // Nothing this task logs, at any depth, is ever reported to the self-DSN.
            // An error inside the digest, reported into the same digest, is a loop with
            // an HTTP request in it. See `selfreport`.
            tokio::spawn(selfreport::quietly(worker.run(jobs)));
        }
    }

    /// Wait until the digest task has finished `count` envelopes in total.
    ///
    /// Ingest answers before the digest runs, so a caller that wants to observe the
    /// result — a test, or `nashcode bugs reindex` later — has to wait for it. This
    /// is the only way to do that without sleeping.
    pub async fn digested(&self, count: u64) {
        let mut watcher = self.inner.digested.subscribe();
        while *watcher.borrow_and_update() < count {
            if watcher.changed().await.is_err() {
                return;
            }
        }
    }
}

/// The `bugs` stanza of `/brain`, for one repo or for the whole box.
///
/// `repo` names a nashcode repo and answers with the projects that declare it; `None`
/// answers with everything, plus the notification budget, which belongs to the box
/// rather than to any one repo.
///
/// `None` back means there is nothing to say — no bucket, or no project declaring this
/// repo — and the key is then absent from the stanza rather than present and empty.
/// That is the convention `architecture` already set: "does this repo have any" is
/// answered by whether the key is there at all.
///
/// This is a data provider, deliberately. `brain.rs` belongs to another work stream, so
/// the one line that mounts it lives there and not here. See `COORDINATION.md`.
pub fn brain_stanza(db: &Db, repo: Option<&str>) -> Option<serde_json::Value> {
    let projects = index::brain_projects(db, repo).ok()?;
    if projects.is_empty() {
        return None;
    }
    let unresolved: i64 = projects.iter().map(|project| project.unresolved).sum();
    let events: i64 = projects.iter().map(|project| project.events).sum();
    let last_event_at = projects.iter().filter_map(|project| project.last_event_at.clone()).max();

    let mut stanza = serde_json::json!({
        "projects": projects,
        "unresolved": unresolved,
        "events": events,
        "last_event_at": last_event_at,
    });
    // The notification budget is a property of the box, so it joins the global stanza
    // and not the per-repo one. It is the number that says whether the next real
    // incident will reach a phone, which is worth one key in the digest an agent reads
    // before it starts work.
    if repo.is_none()
        && let Ok(budget) = pushover::budget(db)
        && let Some(object) = stanza.as_object_mut()
        && let Ok(budget) = serde_json::to_value(&budget)
    {
        object.insert("notifications".to_owned(), budget);
    }
    Some(stanza)
}

/// A 32-character lowercase hex key, the shape every SDK's DSN parser expects.
fn mint_key() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Project names appear in URLs and in object keys. Keep them boring.
pub fn valid_project_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte))
        && name != "."
        && name != ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(bucket: Option<&str>) -> Config {
        Config {
            bugs_bucket: bucket.map(str::to_owned),
            bugs_ingest_url: "https://bugs.example.invalid".to_owned(),
            ..Config::from_env()
        }
    }

    #[test]
    fn no_bucket_means_the_feature_is_off() {
        let bugs = Bugs::new(&config(None), Db::in_memory().unwrap()).unwrap();
        assert!(!bugs.enabled());
    }

    #[test]
    fn a_bucket_that_will_not_open_is_off_rather_than_fatal() {
        let bugs = Bugs::new(&config(Some("gs://nope")), Db::in_memory().unwrap()).unwrap();
        assert!(!bugs.enabled());
    }

    #[test]
    fn a_dsn_carries_the_key_the_ingest_host_and_the_numeric_id() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = format!("file://{}", dir.path().display());
        let bugs = Bugs::new(&config(Some(&bucket)), Db::in_memory().unwrap()).unwrap();
        assert!(bugs.enabled());

        let project = bugs.create_project("demo", None).unwrap();
        assert_eq!(project.key.len(), 32);
        assert!(project.key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            bugs.dsn(&project),
            format!("https://{}@bugs.example.invalid/{}", project.key, project.id)
        );
    }

    #[tokio::test]
    async fn a_full_queue_refuses_the_envelope_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = dir.path().join("bucket");
        let url = format!("file://{}", bucket.display());
        let bugs = Bugs::new(&config(Some(&url)), Db::in_memory().unwrap()).unwrap();
        let project = bugs.create_project("demo", None).unwrap();

        // Hold every slot. Reserved permits are exactly what a backlog looks like to
        // the sender, and holding them keeps the worker from draining behind us.
        let held: Vec<_> =
            std::iter::from_fn(|| bugs.inner.jobs.try_reserve().ok()).collect();
        assert_eq!(held.len(), QUEUE_DEPTH);

        let body = b"{}\n{\"type\":\"event\"}\n{\"event_id\":\"a\"}\n".to_vec();
        assert_eq!(bugs.accept(project.id, body.clone()).await, Err(AcceptError::Busy));
        // Nothing was stored: a refusal that wrote the payload anyway would grow the
        // backlog every time the client retried.
        assert!(!bucket.join("projects").exists(), "the refused envelope is not in the bucket");

        // Give one slot back and the same envelope is taken.
        drop(held);
        assert_eq!(bugs.accept(project.id, body).await, Ok(()));
    }

    #[tokio::test]
    async fn a_queue_full_by_bytes_refuses_even_with_every_slot_free() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = dir.path().join("bucket");
        let url = format!("file://{}", bucket.display());
        let bugs = Bugs::new(&config(Some(&url)), Db::in_memory().unwrap()).unwrap();
        let project = bugs.create_project("demo", None).unwrap();

        // Slots are a poor proxy for memory: 1024 of them times a 100 MiB body is
        // 100 GiB. Hold the byte budget and leave every slot free.
        let held = bugs
            .inner
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(QUEUE_BYTES).unwrap() - 8)
            .expect("the budget starts empty");
        assert_eq!(bugs.inner.jobs.capacity(), QUEUE_DEPTH, "every slot is free");

        let body = b"{}\n{\"type\":\"event\"}\n{\"event_id\":\"a\"}\n".to_vec();
        assert!(body.len() > 8, "the body is bigger than what is left");
        assert_eq!(bugs.accept(project.id, body.clone()).await, Err(AcceptError::Busy));
        assert!(!bucket.join("projects").exists(), "and nothing was written");

        // Give the memory back and the same envelope is taken.
        drop(held);
        assert_eq!(bugs.accept(project.id, body).await, Ok(()));
    }

    /// A body larger than the whole budget could never be admitted, so the queue would
    /// answer 429 to a legal envelope forever. Checked at compile time: this is a
    /// relationship between two constants, and it should fail the build rather than a
    /// test run.
    const _: () = {
        assert!(QUEUE_BYTES > ingest::MAX_DECOMPRESSED);
        assert!(QUEUE_BYTES <= u32::MAX as usize, "a semaphore counts permits in u32");
    };

    #[test]
    fn the_brain_stanza_is_absent_rather_than_empty_and_scopes_to_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = format!("file://{}", dir.path().display());
        let db = Db::in_memory().unwrap();
        let bugs = Bugs::new(&config(Some(&bucket)), db.clone()).unwrap();

        // Nothing to say yet: the key is absent, not present and zero.
        assert!(brain_stanza(&db, None).is_none());
        assert!(brain_stanza(&db, Some("demo")).is_none());

        bugs.create_project("api", Some("demo")).unwrap();
        bugs.create_project("worker", None).unwrap();

        let global = brain_stanza(&db, None).expect("a global stanza");
        assert_eq!(global["projects"].as_array().expect("a list").len(), 2);
        assert_eq!(global["unresolved"], serde_json::json!(0));
        // The notification budget belongs to the box, so it is on the global stanza.
        assert!(global["notifications"].is_object());

        let scoped = brain_stanza(&db, Some("demo")).expect("a repo stanza");
        assert_eq!(scoped["projects"].as_array().expect("a list").len(), 1);
        assert_eq!(scoped["projects"][0]["name"], serde_json::json!("api"));
        assert!(scoped["notifications"].is_null(), "the budget is not a repo's business");
        // A repo no project declares says nothing at all.
        assert!(brain_stanza(&db, Some("other")).is_none());
    }

    #[test]
    fn a_project_name_that_could_leave_its_path_is_refused() {
        let bugs = Bugs::new(&config(None), Db::in_memory().unwrap()).unwrap();
        for hostile in ["../etc", "a/b", "", "Demo", "a b", &"x".repeat(65), ".", ".."] {
            assert!(bugs.create_project(hostile, None).is_err(), "{hostile} must be refused");
        }
        assert!(bugs.create_project("demo-1.api_v2", None).is_ok());
        assert!(bugs.create_project("demo-1.api_v2", None).is_err(), "names are unique");
    }
}
