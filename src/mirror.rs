//! Mirror management: `git clone --mirror` copies of every configured repo, refreshed
//! on page load behind a short debounce.
//!
//! The rule this module exists to enforce is that **a page never fails because dgit is
//! down**. A fetch that cannot reach the server leaves the existing mirror in place and
//! marks it stale; the page renders from what is on disk and says so. Only a repo that
//! has never been cloned is genuinely unavailable, and even that is an error card, not
//! a 500.

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

    /// Bring a mirror up to date, subject to the debounce, and report its health.
    ///
    /// This never returns an error: every failure mode becomes a status the page can
    /// render.
    pub async fn refresh(&self, repo: &str) -> MirrorStatus {
        let lock = {
            let mut locks = self.locks.lock().await;
            locks.entry(repo.to_owned()).or_default().clone()
        };
        let _guard = lock.lock().await;

        let path = self.config.mirror_path(repo);
        let exists = path.exists();

        // Debounce: a recent attempt means the answer on disk is good enough.
        if exists {
            let state = self.state.lock().await.get(repo).cloned().unwrap_or_default();
            if let Some(attempted) = state.last_attempt
                && attempted.elapsed() < DEBOUNCE
            {
                return match state.last_error {
                    Some(message) => MirrorStatus {
                        available: true,
                        stale: true,
                        message: Some(message),
                        last_fetched: state.last_success,
                    },
                    None => MirrorStatus::fresh(state.last_success),
                };
            }
        }

        let remote = self.config.remote_url(repo);
        let outcome = if exists {
            self.repo(repo)
                .run_remote(&remote, &["remote", "update", "--prune"])
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

    /// Refresh every configured repo. Used at startup and by the merge/restack path,
    /// which must leave the mirror agreeing with dgit.
    pub async fn refresh_all(&self) -> HashMap<String, MirrorStatus> {
        let mut all = HashMap::new();
        for repo in &self.config.repos {
            all.insert(repo.clone(), self.refresh(repo).await);
        }
        all
    }

    /// Force a refresh, ignoring the debounce. A write path calls this so the next
    /// page render already reflects the push it just made.
    pub async fn refresh_now(&self, repo: &str) -> MirrorStatus {
        self.state.lock().await.entry(repo.to_owned()).or_default().last_attempt = None;
        self.refresh(repo).await
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
            repos: vec!["demo".to_owned()],
            mirrors: dir.path().to_path_buf(),
            bind: "127.0.0.1:0".to_owned(),
            db_path: dir.path().join("db.sqlite"),
            ci_logs: dir.path().join("logs"),
            webhooks: Default::default(),
            anthropic_key: None,
            anthropic_url: "https://api.anthropic.com".to_owned(),
            brain_model: "claude-opus-5".to_owned(),
        };
        let mirrors = Mirrors::new(Arc::new(config), Db::in_memory().unwrap());

        let status = mirrors.refresh("demo").await;
        assert!(!status.available);
        assert!(status.stale);
        assert!(status.message.is_some());
    }
}
