//! Mirror management: `git clone --mirror` copies of every known repo, refreshed behind
//! the page load rather than in front of it.
//!
//! Which repos are known is decided here too. [`Mirrors::watch`] runs a cycle a minute,
//! and each cycle first asks dgit what it is serving — [`Mirrors::discover`] — then
//! refreshes everything. So a `git push` to a name nobody configured produces a mirror,
//! an index row, and a browsable repo within one cycle. `NASHCODE_REPOS` is a seed for
//! that set, not the whole of it, and nothing here ever drops a name.
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
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::Config;
use crate::db::Db;
use crate::git::{Auth, Repo, clone_mirror};

/// How long a fetch stays fresh. A burst of page loads costs one fetch.
const DEBOUNCE: Duration = Duration::from_secs(10);

/// How often [`Mirrors::watch`] runs a cycle. Discovery rides on that cycle, so this
/// is also the longest a repo can exist on dgit without appearing in the viewer.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How long the index-page fetch may take. Short: a slow git server must not hold a
/// cycle open, and the next one is a minute away.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Top-level paths the router owns outright. `/{repo}` is matched after these, so a
/// repo with one of these names would be shadowed on every page it has — half-served,
/// and confusingly. Discovery refuses the name and says so; `NASHCODE_REPOS` is the
/// override for an operator who knows what they are asking for.
const RESERVED_ROUTES: &[&str] = &["api", "assets", "brain", "bugs", "favicon.svg"];

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
    /// Reads dgit's index page, once a cycle. Built here rather than per call so a
    /// rustls configuration is not assembled every minute.
    client: reqwest::Client,
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
            client: reqwest::Client::builder()
                .timeout(DISCOVERY_TIMEOUT)
                .build()
                .expect("reqwest client builds"),
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

    /// The poll clock. One cycle immediately, then one every [`POLL_INTERVAL`].
    ///
    /// This is the whole of repo discovery's schedule: a repo pushed to dgit under a
    /// name nobody configured is mirrored, listed, and browsable one cycle later, with
    /// no environment change and no restart.
    pub async fn watch(self) {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.refresh_all().await;
        }
    }

    /// Learn what dgit is serving, then refresh every repo we know and wait for each.
    ///
    /// One cycle of [`watch`](Self::watch). A repo seen for the first time has no
    /// mirror, so its refresh is the clone.
    pub async fn refresh_all(&self) -> HashMap<String, MirrorStatus> {
        self.discover().await;
        let mut all = HashMap::new();
        for repo in self.config.repos.names() {
            let status = self.refresh_inline(&repo).await;
            all.insert(repo, status);
        }
        all
    }

    /// Add every repo dgit is serving that we do not already know.
    ///
    /// dgit has no list API, so its list is the HTML index page and `dgit_index` is the
    /// parser — the same one `nashcode ls` uses. A `DGIT_URL` that is a filesystem path
    /// is a directory of bare repos with no index page to fetch, so the `*.git`
    /// directories in it are the list instead.
    ///
    /// Names are only ever added, and [`RESERVED_ROUTES`] are not added at all. A fetch
    /// that fails is one `warn!` and no change: a git server that is down or a page whose
    /// markup moved must not empty the index.
    ///
    /// ponytail: discovery sees the repos dgit lists, which are its public ones. A
    /// private repo needs the operator to say so, and the endpoint for that
    /// (`PUT /:repo/track`) is not built.
    async fn discover(&self) {
        let url = self.config.dgit_url.trim();
        if url.is_empty() {
            return;
        }
        let found = if url.starts_with("http://") || url.starts_with("https://") {
            self.index_page(url).await
        } else {
            listed_bare_repos(Path::new(url))
        };
        for name in found {
            if RESERVED_ROUTES.contains(&name.as_str()) {
                tracing::warn!(repo = %name, "the git server lists a repo whose name is a viewer route; not mirroring it");
                continue;
            }
            if self.config.repos.insert(&name) {
                tracing::info!(repo = %name, "discovered a repo on the git server");
            }
        }
    }

    /// The repo names on dgit's index page. Empty on any failure.
    async fn index_page(&self, url: &str) -> Vec<String> {
        let mut request = self.client.get(format!("{url}/"));
        // The same credentials the clones use. dgit ignores the username.
        if !self.config.git_token.is_empty() {
            request = request.basic_auth("x", Some(&self.config.git_token));
        }
        let html = match request.send().await.and_then(|reply| reply.error_for_status()) {
            Ok(reply) => match reply.text().await {
                Ok(html) => html,
                Err(error) => {
                    tracing::warn!(%error, "cannot read the git server's index page");
                    return Vec::new();
                }
            },
            Err(error) => {
                tracing::warn!(%error, "cannot fetch the git server's index page");
                return Vec::new();
            }
        };
        dgit_index::parse(&html).into_iter().map(|repo| repo.name).collect()
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

/// The repo names in a directory of bare repos: every `<name>.git` directory in it.
///
/// This is what a `DGIT_URL` that is a filesystem path means — the tests use one, and
/// so does a local setup with no dgit in front of it. An unreadable directory yields
/// nothing, the same way an unreachable server does.
fn listed_bare_repos(dir: &Path) -> Vec<String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(?dir, %error, "cannot list the git server's directory");
            return Vec::new();
        }
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            entry.file_name().to_str()?.strip_suffix(".git").map(str::to_owned)
        })
        .filter(|name| !name.is_empty())
        .collect()
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
