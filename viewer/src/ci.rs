//! Polling CI: one global worker runs `.nashcode/ci` for each newly seen branch tip,
//! and rebuilds the code index for each new tip of a default branch.
//! // ponytail: serial queue; parallelize per-repo if it ever backs up
//!
//! A CI job checks the commit out into a scratch clone, runs `.nashcode/ci` from the
//! repo root when it is present and executable, captures the combined output to a log
//! file, and records the result in SQLite. CD is not a separate system: the script
//! deploys if it wants to.
//!
//! CI is opt-in, and the opt-in is read from the **default branch**, never from the
//! commit under test. Without that split, anyone who can push a branch can run code on
//! the box, and a push is an invite away from being remote code execution.
//!
//! Code indexing shares this queue rather than getting one of its own. Both are slow,
//! both are triggered by a commit arriving, and neither may ever run on a request
//! path; a second queue would only be a second way to fall behind.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::code::Indexer;
use crate::config::Config;
use crate::db::{Db, status};
use crate::git::{Repo, clone_local};
use crate::hooks::{self, Webhooks};

/// The script a job runs, relative to the repo root.
pub const CI_SCRIPT: &str = ".nashcode/ci";

/// The file a repo opts into CI with, relative to its root. Only the copy on the
/// default branch counts.
pub const CI_CONFIG: &str = ".nashcode/ci.toml";

/// The log of a run that had no opt-in to run under.
pub const NOT_ENABLED: &str = "ci not enabled on default branch";

/// How long a job may run before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the default branch says CI may do. Every field is off by default, and every
/// way of failing to read the file answers with this, so the gate fails closed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// `enabled = true`. Without it no branch of this repo runs anything.
    pub enabled: bool,
    /// `git_token = true`. The only way `GIT_TOKEN` reaches a job's environment.
    pub git_token: bool,
}

#[derive(serde::Deserialize)]
struct RawPolicy {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    git_token: bool,
}

/// Read the opt-in from the default branch tip of a mirror.
///
/// The branch under test is never consulted: that is the whole point. A missing,
/// unreadable, or unparseable file leaves CI off rather than guessing.
pub async fn policy(mirror: &std::path::Path) -> Policy {
    let repo = Repo::mirror(mirror, Default::default());
    let Ok(branch) = repo.default_branch().await else { return Policy::default() };
    let Ok(Some(bytes)) = repo.show_file(&branch, CI_CONFIG).await else {
        return Policy::default();
    };
    let Ok(text) = String::from_utf8(bytes) else { return Policy::default() };
    match toml::from_str::<RawPolicy>(&text) {
        Ok(raw) => Policy { enabled: raw.enabled, git_token: raw.git_token },
        Err(error) => {
            tracing::warn!(%error, "cannot parse {CI_CONFIG}; CI stays off");
            Policy::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub run_id: i64,
    pub repo: String,
    pub branch: String,
    pub commit: String,
}

/// A code index rebuild: "this repo moved, look at it again".
///
/// Deliberately carries no branch and no commit. The run resolves the default branch
/// tip when it starts, which is what lets duplicate jobs be coalesced — a job dropped
/// because one was already queued cannot have seen anything the survivor will miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexJob {
    pub repo: String,
}

/// What the worker takes off the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Task {
    Ci(Job),
    Index(IndexJob),
}

/// The enqueue side. Cheap to clone; shared through the app context.
#[derive(Clone)]
pub struct CiQueue {
    db: Db,
    tx: mpsc::UnboundedSender<Task>,
    /// Repos with an index job already waiting. An index run reads the default branch
    /// tip *when it starts*, so a second job queued behind the first would read the
    /// same tip and do nothing — and a push of five branches would queue five of them.
    /// One pending job per repo always produces the same answer as many.
    pending_index: Arc<Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for CiQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CiQueue")
    }
}

impl CiQueue {
    pub fn new(db: Db) -> (Self, mpsc::UnboundedReceiver<Task>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { db, tx, pending_index: Arc::new(Mutex::new(HashSet::new())) }, rx)
    }

    /// Tell the queue an index job has been taken off it, so the next one may queue.
    ///
    /// Called by the worker as it starts the job, not as it finishes: work that
    /// arrives *during* a run has to be able to queue a run that will see it.
    pub fn index_started(&self, repo: &str) {
        self.lock_pending().remove(repo);
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.pending_index.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record a queued run and hand it to the worker. Returns the run id.
    pub fn enqueue(&self, repo: &str, branch: &str, commit: &str) -> Option<i64> {
        let run_id = match self.db.enqueue_run(repo, branch, commit) {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(repo, branch, %error, "cannot record CI run");
                return None;
            }
        };
        let job = Job {
            run_id,
            repo: repo.to_owned(),
            branch: branch.to_owned(),
            commit: commit.to_owned(),
        };
        if self.tx.send(Task::Ci(job)).is_err() {
            tracing::warn!(repo, branch, "CI worker is gone; run stays queued");
        }
        Some(run_id)
    }

    /// Queue a code index rebuild. `branch` narrows it to the default branch — pass
    /// `None` for a manual run, which always indexes whatever the default branch
    /// points at now.
    ///
    /// Nothing is recorded here: an index run records itself when it finishes, and a
    /// row saying "queued" that no query reads would only be a second source of truth.
    pub fn enqueue_index(&self, repo: &str) {
        // Coalesce. A push of several branches fires the observer once per branch, and
        // a merge fires it again; queueing one job per event would run the indexer
        // repeatedly over a tree that has not moved between them. One pending job per
        // repo gives the same answer as many, because the run reads the tip itself.
        if !self.lock_pending().insert(repo.to_owned()) {
            return;
        }
        if self.tx.send(Task::Index(IndexJob { repo: repo.to_owned() })).is_err() {
            tracing::warn!(repo, "CI worker is gone; the index run is dropped");
            self.lock_pending().remove(repo);
        }
    }
}

/// The run side. One instance loops over the queue for the life of the process.
pub struct CiWorker {
    pub config: Arc<Config>,
    pub db: Db,
    pub hooks: Webhooks,
    pub timeout: Duration,
    /// `None` compiles the code index out of the loop entirely, which is what the
    /// tests that only care about CI use.
    pub indexer: Option<Indexer>,
    /// The enqueue side, so a run can release its repo's coalescing slot as it starts.
    pub queue: Option<CiQueue>,
}

impl CiWorker {
    pub async fn run(self, mut rx: mpsc::UnboundedReceiver<Task>) {
        while let Some(task) = rx.recv().await {
            match task {
                Task::Ci(job) => self.run_job(&job).await,
                Task::Index(job) => self.run_index(&job).await,
            }
        }
    }

    /// Rebuild a repo's code index. Every failure is one log line: nothing downstream
    /// of this waits on it, and a stale index answers queries perfectly well.
    ///
    /// A job always indexes the repo's **default branch tip as it stands right now**,
    /// whichever branch's movement caused the job. That is what makes coalescing safe:
    /// a job dropped because another was already queued cannot be a job that would
    /// have seen something the survivor will not, because the survivor resolves the
    /// tip later. A job whose tree is already indexed costs one tree read and returns.
    pub async fn run_index(&self, job: &IndexJob) {
        let Some(indexer) = &self.indexer else { return };
        // Let the next job queue. Done as the run *starts*, not as it ends: a push
        // that lands mid-run must be able to queue the run that will see it.
        if let Some(queue) = &self.queue {
            queue.index_started(&job.repo);
        }

        let mirror = indexer.mirrors.repo(&job.repo);
        let Ok(default_branch) = mirror.default_branch().await else {
            tracing::warn!(repo = %job.repo, "cannot index: the mirror has no branches");
            return;
        };

        let commit = match mirror.tip(&default_branch).await {
            Ok(tip) => tip,
            Err(error) => {
                tracing::warn!(repo = %job.repo, %error, "cannot index: no tip");
                return;
            }
        };

        let started = Instant::now();
        match indexer.index(&job.repo, &commit).await {
            Ok(run) => tracing::info!(
                repo = %job.repo,
                commit = %commit,
                files = run.files_indexed,
                chunks = run.chunks,
                symbols = run.symbols,
                embedded = run.embedded,
                ms = started.elapsed().as_millis() as u64,
                "code index updated"
            ),
            Err(error) => {
                tracing::warn!(repo = %job.repo, commit = %commit, %error, "code index failed")
            }
        }
    }

    /// Run one job start to finish. Every failure mode becomes a recorded status.
    pub async fn run_job(&self, job: &Job) {
        let started = Instant::now();
        let _ = self.db.set_run_status(job.run_id, status::RUNNING, 0, None);

        let (outcome, log) = self.execute(job).await;
        let duration_ms = started.elapsed().as_millis() as i64;

        let log_path = if log.is_empty() { None } else { Some(self.write_log(job.run_id, &log)) };
        let _ = self.db.set_run_status(
            job.run_id,
            outcome,
            duration_ms,
            log_path.as_deref().and_then(std::path::Path::to_str),
        );

        if outcome != status::SKIPPED {
            let tail: String = log.chars().skip(log.chars().count().saturating_sub(2000)).collect();
            self.hooks.send(
                hooks::CI_FINISHED,
                serde_json::json!({
                    "repo": job.repo,
                    "branch": job.branch,
                    "commit": job.commit,
                    "status": outcome,
                    "log_tail": tail,
                }),
            );
        }
    }

    /// Check the commit out and run the script. Returns (status, combined output).
    async fn execute(&self, job: &Job) -> (&'static str, String) {
        let mirror = self.config.mirror_path(&job.repo);
        if !mirror.exists() {
            return (status::ERROR, format!("no mirror for {}", job.repo));
        }
        // Ask the default branch before cloning anything: a repo that never opted in
        // must not have a pushed commit written to disk on its behalf.
        let policy = policy(&mirror).await;
        if !policy.enabled {
            return (status::SKIPPED, NOT_ENABLED.to_owned());
        }

        let scratch = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(error) => return (status::ERROR, format!("cannot create scratch dir: {error}")),
        };
        let checkout = scratch.path().join("repo");
        if let Err(error) = clone_local(&mirror, &checkout).await {
            return (status::ERROR, format!("scratch clone failed: {error}"));
        }
        let worktree = Repo::worktree(&checkout, Default::default());
        if let Err(error) = worktree.run(&["checkout", "--detach", &job.commit]).await {
            return (status::ERROR, format!("cannot check out {}: {error}", job.commit));
        }

        let script = checkout.join(CI_SCRIPT);
        if !is_executable(&script) {
            return (status::SKIPPED, String::new());
        }

        // The job's whole environment: the coordinates and enough of a shell to run
        // (`PATH`, `HOME`). Nothing else leaks from the server. The push token is not
        // in it unless the default branch asked for it by name.
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!("exec ./{CI_SCRIPT} 2>&1"))
            .current_dir(&checkout)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
            .env("HOME", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
            .env("NASHCODE_REPO", &job.repo)
            .env("NASHCODE_BRANCH", &job.branch)
            .env("NASHCODE_COMMIT", &job.commit)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if policy.git_token {
            command.env("GIT_TOKEN", &self.config.git_token);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return (status::ERROR, format!("cannot run {CI_SCRIPT}: {error}")),
        };

        // Stream the output against a deadline, so a timed-out job still keeps
        // everything it printed before the axe fell.
        use tokio::io::AsyncReadExt as _;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut collected: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 8192];
        let timed_out = loop {
            match tokio::time::timeout_at(deadline, stdout.read(&mut buffer)).await {
                Err(_elapsed) => break true,
                Ok(Ok(0)) => break false,
                Ok(Ok(read)) => collected.extend_from_slice(&buffer[..read]),
                Ok(Err(error)) => {
                    collected
                        .extend_from_slice(format!("\n[read error: {error}]").as_bytes());
                    break false;
                }
            }
        };

        if timed_out {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let mut log = String::from_utf8_lossy(&collected).into_owned();
            log.push_str(&format!(
                "\n\n[nashcode: killed after the {}s timeout; output above is partial]",
                self.timeout.as_secs()
            ));
            return (status::TIMEOUT, log);
        }

        let log = String::from_utf8_lossy(&collected).into_owned();
        match child.wait().await {
            Err(error) => (status::ERROR, format!("{log}\n[cannot collect status: {error}]")),
            Ok(exit) if exit.success() => (status::PASSED, log),
            Ok(_) => (status::FAILED, log),
        }
    }

    fn write_log(&self, run_id: i64, log: &str) -> PathBuf {
        let dir = &self.config.ci_logs;
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join(format!("{run_id}.log"));
        if let Err(error) = std::fs::write(&path, log) {
            tracing::warn!(run_id, %error, "cannot write CI log");
        }
        path
    }
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Drop ANSI escape sequences so a log renders in a plain `<pre>`.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: ESC [ ... final byte in @-~. Other escapes: skip one char.
        if chars.peek() == Some(&'[') {
            chars.next();
            for follow in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&follow) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_colors_are_stripped_for_the_pre_tag() {
        let raw = "\u{1b}[31merror\u{1b}[0m: plain\n";
        assert_eq!(strip_ansi(raw), "error: plain\n");
    }

    #[test]
    fn text_without_escapes_is_untouched(){
        assert_eq!(strip_ansi("all good"), "all good");
    }
}
