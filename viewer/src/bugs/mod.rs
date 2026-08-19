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

pub mod digest;
pub mod envelope;
pub mod group;
pub mod index;
pub mod ingest;
pub mod store;

use std::sync::{Arc, Mutex};

use object_store::{ObjectStore, ObjectStoreExt};
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::db::{Db, DbResult};

pub use index::{EventRow, Issue, Landing, Project, ProjectSummary, state};

/// The error-tracking feature: the bucket, the index, and the digest queue.
///
/// Cheap to clone and always constructible — a build with no bucket configured makes
/// a disabled one rather than an `Option`, so every call site reads the same.
#[derive(Clone)]
pub struct Bugs {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    store: Option<Arc<dyn ObjectStore>>,
    /// The origin that goes into a DSN.
    ingest_url: String,
    jobs: mpsc::UnboundedSender<digest::Job>,
    /// The digest task's receiver, until the task takes it. The worker starts on the
    /// first envelope rather than at construction, because construction happens
    /// outside the async runtime in both `main` and the tests.
    pending: Mutex<Option<mpsc::UnboundedReceiver<digest::Job>>>,
    digested: Arc<watch::Sender<u64>>,
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
        let (jobs, pending) = mpsc::unbounded_channel();
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                store,
                ingest_url: config.bugs_ingest_url.clone(),
                jobs,
                pending: Mutex::new(Some(pending)),
                digested: Arc::new(watch::channel(0).0),
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
    pub async fn accept(&self, project_id: i64, body: Vec<u8>) -> Result<(), String> {
        let store = self.inner.store.as_ref().ok_or("error tracking is off")?;
        let key = store::envelope_key(project_id, &crate::db::now(), &mint_key());
        store
            .put(&key, body.clone().into())
            .await
            .map_err(|error| format!("cannot write {key}: {error}"))?;
        if let Err(error) = index::record_envelope(self.db(), project_id, key.as_ref()) {
            // The object is safe; only the reindex shortcut is lost.
            tracing::warn!(%error, "bugs: cannot record the envelope object");
        }
        self.enqueue(digest::Job { project_id, body });
        Ok(())
    }

    fn enqueue(&self, job: digest::Job) {
        self.start_digest();
        // The only receiver is the digest task, which lives as long as this handle.
        let _ = self.inner.jobs.send(job);
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
            };
            tokio::spawn(worker.run(jobs));
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
