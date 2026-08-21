//! Mirror management: `git clone --mirror` copies of every configured repo, refreshed
//! behind the page load rather than in front of it.
//!
//! The rule this module exists to enforce is that **a page never fails because dgit is
//! down**. A fetch that cannot reach the server leaves the existing mirror in place and
//! marks it stale; the page renders from what is on disk and says so. Only a repo that
//! has never been cloned is genuinely unavailable, and even that is an error card, not
//! a 500.
//!
//! The second rule is that **a page never waits for dgit either**. A fetch of a real repo
//! costs seconds. So [`Mirrors::refresh`] serves the mirror already on disk and starts the
//! fetch as a background task; the next page load sees its result. The debounce still
//! applies, and the per-repo lock still guarantees one fetch at a time. The one request
//! that blocks is the first view of a repo with no mirror, which has nothing to render.
//! Write paths use [`Mirrors::refresh_now`], which fetches inline: after a push the caller
//! must see its own write.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::Config;
use crate::db::Db;
use crate::git::{Auth, Repo, clone_mirror};

/// How long a fetch stays fresh. A burst of page loads costs one fetch.
const DEBOUNCE: Duration = Duration::from_secs(10);

/// What the UI needs to know about a mirror's health.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorStatus {
    /// A mirror exists on disk, so git questions can be answered.
    pub available: bool,
    /// The last refresh attempt failed. Content is real but possibly behind.
    pub stale: bool,
    /// Why it is stale or unavailable, in words a person can act on.
    pub message: Option<String>,
    /// When the mirror last refreshed successfully.
    pub last_fetched: Option<String>,
}

impl MirrorStatus {
    fn fresh(last_fetched: Option<String>) -> Self {
        Self { available: true, stale: false, message: None, last_fetched }
    }
}

#[derive(Debug, Default, Clone)]
struct RepoState {
    last_attempt: Option<Instant>,
    last_success: Option<String>,
    last_error: Option<String>,
    cloned: bool,
}

/// Something that wants to hear about a branch tip nobody has seen before.
pub type TipObserver = Arc<dyn Fn(NewTip) + Send + Sync>;

/// A branch tip observed for the first time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTip {
    pub repo: String,
    pub branch: String,
    pub commit: String,
}

/// The mirror pool. Shared through Topcoat's app context.
#[derive(Clone)]
pub struct Mirrors {
    config: Arc<Config>,
    db: Db,
    state: Arc<Mutex<HashMap<String, RepoState>>>,
    /// One lock per repo so two concurrent page loads never fetch the same repo twice.
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    observer: Option<TipObserver>,
}

impl std::fmt::Debug for Mirrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Mirrors")
    }
}

impl Mirrors {
    pub fn new(config: Arc<Config>, db: Db) -> Self {
        Self {
            config,
            db,
            state: Arc::new(Mutex::new(HashMap::new())),
            locks: Arc::new(Mutex::new(HashMap::new())),
            observer: None,
        }
    }

    /// Register the callback that receives every newly seen branch tip. The CI queue
    /// and the `push` webhook both hang off this.
    pub fn with_observer(mut self, observer: TipObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn auth(&self) -> Auth {
        Auth::new(self.config.git_token.clone())
    }

    /// A handle to a repo's mirror. Does not check that it exists.
    pub fn repo(&self, repo: &str) -> Repo {
        Repo::mirror(self.config.mirror_path(repo), self.auth())
    }

    /// Report a mirror's health, and bring it up to date behind the caller's back.
    ///
    /// Returns as soon as it has read the shared state: the page renders from the mirror
    /// on disk. If the debounce has lapsed and no fetch for this repo is already running,
    /// the fetch is spawned as a background task, and whatever it learns shows up on the
    /// next request. The exception is a repo with no mirror at all, where there is nothing
    /// to render: that request blocks on the clone.
    ///
    /// This never returns an error: every failure mode becomes a status the page can
    /// render.
    pub async fn refresh(&self, repo: &str) -> MirrorStatus {
        if !self.config.mirror_path(repo).exists() {
            // Nothing on disk. The first view of a repo has to wait for the clone.
            return self.refresh_inline(repo).await;
        }

        let state = self.state.lock().await.get(repo).cloned().unwrap_or_default();
        let due = state.last_attempt.is_none_or(|at| at.elapsed() >= DEBOUNCE);
        if due {
            self.spawn_fetch(repo).await;
        }
        Self::status_of(&state)
    }

    /// The health the shared state already knows about, with no fetch involved.
    ///
    /// `stale` means the last attempt failed, and nothing else. A fetch that is merely
    /// in flight leaves the page alone: it has not learned anything yet.
    fn status_of(state: &RepoState) -> MirrorStatus {
        match &state.last_error {
            Some(message) => MirrorStatus {
                available: true,
                stale: true,
                message: Some(message.clone()),
                last_fetched: state.last_success.clone(),
            },
            None => MirrorStatus::fresh(state.last_success.clone()),
        }
    }

    /// The repo's fetch lock, created on first use.
    async fn lock_for(&self, repo: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks.entry(repo.to_owned()).or_default().clone()
    }

    /// Start a background fetch, unless this repo already has one running.
    ///
    /// The in-flight guard is the repo's own lock: it is taken here, without waiting, and
    /// moved into the task, so it is held for exactly as long as the fetch runs. A second
    /// caller finds it taken and does nothing.
    async fn spawn_fetch(&self, repo: &str) {
        let Ok(guard) = self.lock_for(repo).await.try_lock_owned() else {
            return; // A fetch for this repo is already in flight.
        };
        let mirrors = self.clone();
        let repo = repo.to_owned();
        tokio::spawn(async move {
            let _guard = guard;
            mirrors.fetch(&repo).await;
        });
    }

    /// Fetch on the caller's own time: take the lock, honour the debounce, and wait for
    /// the result. Startup and the write paths want this; a page load does not.
    async fn refresh_inline(&self, repo: &str) -> MirrorStatus {
        let lock = self.lock_for(repo).await;
        let _guard = lock.lock().await;

        // Debounce: a recent attempt means the answer on disk is good enough. Checked
        // under the lock, so a fetch we queued behind does not run twice.
        if self.config.mirror_path(repo).exists() {
            let state = self.state.lock().await.get(repo).cloned().unwrap_or_default();
            if let Some(attempted) = state.last_attempt
                && attempted.elapsed() < DEBOUNCE
            {
                return Self::status_of(&state);
            }
        }

        self.fetch(repo).await
    }

    /// The fetch itself. The caller holds the repo's lock; this never takes it.
    ///
    /// Whether it runs inline or on a background task changes nothing here: the shared
    /// state is updated the same way either way, and the state mutex is never held across
    /// the git call.
    async fn fetch(&self, repo: &str) -> MirrorStatus {
        let path = self.config.mirror_path(repo);
        let exists = path.exists();
        let remote = self.config.remote_url(repo);
        let outcome = if exists {
            let handle = self.repo(repo);
            // Fetch the configured URL explicitly rather than `remote update`, which
            // would use whatever URL the mirror happens to have stored. `DGIT_URL` is
            // the source of truth: point it somewhere else and the next refresh goes
            // there, or fails and leaves the mirror stale.
            handle
                .run_remote(
                    &remote,
                    &[
                        "-c",
                        // Never repack on the request path. Maintenance runs on its own
                        // schedule; a gc triggered by a page load is a latency spike at
                        // the worst moment.
                        "gc.auto=0",
                        "-c",
                        // Give up on a stalled transfer instead of hanging to the
                        // timeout: a tailnet peer that vanishes mid-fetch is common.
                        "http.lowSpeedLimit=1000",
                        "-c",
                        "http.lowSpeedTime=30",
                        "fetch",
                        // One ref transaction, so a reader never sees half a fetch.
                        "--atomic",
                        "--prune",
                        "--prune-tags",
                        "--no-write-fetch-head",
                        &remote,
                        "+refs/heads/*:refs/heads/*",
                        // Forced, unlike `--tags`. A retagged ref on the server would
                        // otherwise fail every future fetch and strand the mirror.
                        "+refs/tags/*:refs/tags/*",
                    ],
                )
                .await
                .map_err(|error| error.to_string())
                .and_then(|out| {
                    if out.ok() { Ok(()) } else { Err(out.stderr.trim().to_owned()) }
                })
        } else {
            clone_mirror(&remote, &path, &self.auth())
                .await
                .map_err(|error| error.to_string())
        };

        let now = crate::db::now();
        let mut states = self.state.lock().await;
        let state = states.entry(repo.to_owned()).or_default();
        state.last_attempt = Some(Instant::now());

        let status = match outcome {
            Ok(()) => {
                state.last_error = None;
                state.last_success = Some(now.clone());
                state.cloned = true;
                MirrorStatus::fresh(Some(now))
            }
            Err(message) => {
                tracing::warn!(repo, %message, "mirror refresh failed");
                state.last_error = Some(message.clone());
                if path.exists() {
                    // The mirror is still readable. Degrade, do not fail.
                    MirrorStatus {
                        available: true,
                        stale: true,
                        message: Some(message),
                        last_fetched: state.last_success.clone(),
                    }
                } else {
                    MirrorStatus {
                        available: false,
                        stale: true,
                        message: Some(message),
                        last_fetched: None,
                    }
                }
            }
        };
        drop(states);

        if status.available && !status.stale {
            self.observe_tips(repo).await;
        }
        status
    }

    /// Record every branch tip and tell the observer about the ones that are new.
    async fn observe_tips(&self, repo: &str) {
        let handle = self.repo(repo);
        let Ok(branches) = handle.branches().await else { return };
        for branch in branches {
            let Ok(commit) = handle.tip(&branch).await else { continue };
            match self.db.observe_tip(repo, &branch, &commit) {
                Ok(true) => {
                    if let Some(observer) = &self.observer {
                        observer(NewTip {
                            repo: repo.to_owned(),
                            branch: branch.clone(),
                            commit: commit.clone(),
                        });
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(repo, branch, %error, "cannot record tip"),
            }
        }
    }

    /// Refresh every configured repo and wait for each. Startup calls this to warm the
    /// mirrors before the first request arrives.
    pub async fn refresh_all(&self) -> HashMap<String, MirrorStatus> {
        let mut all = HashMap::new();
        for repo in self.config.repos.names() {
            let status = self.refresh_inline(&repo).await;
            all.insert(repo, status);
        }
        all
    }

    /// Force a refresh, ignoring the debounce, and wait for it. A write path calls this
    /// so the next page render already reflects the push it just made.
    ///
    /// The debounce is cleared under the lock, after any fetch already in flight has
    /// finished. That fetch may have read the server before this caller's push, so its
    /// result does not count: this one must go to the wire.
    pub async fn refresh_now(&self, repo: &str) -> MirrorStatus {
        let lock = self.lock_for(repo).await;
        let _guard = lock.lock().await;
        self.state.lock().await.entry(repo.to_owned()).or_default().last_attempt = None;
        self.fetch(repo).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_repo_that_was_never_cloned_reports_unavailable_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            dgit_url: "http://127.0.0.1:1/nope".to_owned(),
            git_token: String::new(),
            repos: ["demo"].into_iter().collect(),
            mirrors: dir.path().to_path_buf(),
            bind: "127.0.0.1:0".to_owned(),
            db_path: dir.path().join("db.sqlite"),
            ci_logs: dir.path().join("logs"),
            traces: dir.path().join("traces"),
            webhooks: Default::default(),
            anthropic_key: None,
            anthropic_url: "https://api.anthropic.com".to_owned(),
            brain_model: "claude-opus-5".to_owned(),
            bugs_bucket: None,
            bugs_s3_endpoint: None,
            bugs_ingest_url: "http://127.0.0.1:0".to_owned(),
            bugs_drain: None,
            pushover: None,
            public_url: "http://127.0.0.1:0".to_owned(),
            bugs_self_dsn: None,
        };
        let mirrors = Mirrors::new(Arc::new(config), Db::in_memory().unwrap());

        let status = mirrors.refresh("demo").await;
        assert!(!status.available);
        assert!(status.stale);
        assert!(status.message.is_some());
    }
}
