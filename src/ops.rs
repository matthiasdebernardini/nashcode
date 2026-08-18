//! The write paths to git: merge, restack, and single-file commits (board moves).
//!
//! Every mutation happens in a scratch clone of the mirror, is pushed to dgit with the
//! configured token, and only then reported as done — the mirror is refreshed right
//! after the push so it never disagrees with the server. Nothing is ever force-pushed
//! until every step that could fail has already succeeded (`git push --atomic`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use crate::config::Config;
use crate::db::{Db, NewAudit, status};
use crate::docs;
use crate::git::{GitError, Repo, clone_local};
use crate::hooks::{self, Webhooks};
use crate::mirror::Mirrors;
use crate::stack::StackGraph;

/// Who is doing the writing, from the Tailscale headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// `Tailscale-User-Login`, or `local` for direct loopback hits.
    pub login: String,
    /// `Tailscale-User-Name`, falling back to the login.
    pub name: String,
}

impl Actor {
    pub fn local() -> Self {
        Self { login: "local".to_owned(), name: "local".to_owned() }
    }
}

/// Why a write path stopped.
#[derive(Debug)]
pub enum OpError {
    /// The request names something that does not exist.
    NotFound(String),
    /// The request is understood but not allowed as asked.
    Blocked(String),
    /// A rebase hit conflicts; nothing was pushed.
    Conflict { branch: String, files: Vec<String> },
    /// git itself failed (including the push).
    Git(String),
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(what) => write!(f, "not found: {what}"),
            Self::Blocked(why) => write!(f, "blocked: {why}"),
            Self::Conflict { branch, files } => {
                write!(f, "rebase of {branch} conflicts in: {}", files.join(", "))
            }
            Self::Git(message) => write!(f, "git failed: {message}"),
        }
    }
}

impl std::error::Error for OpError {}

impl From<GitError> for OpError {
    fn from(error: GitError) -> Self {
        Self::Git(error.to_string())
    }
}

pub type OpResult<T> = Result<T, OpError>;

/// Everything the write paths need. Bundled so route handlers stay one-liners.
#[derive(Clone)]
pub struct Ops {
    pub config: std::sync::Arc<Config>,
    pub db: Db,
    pub mirrors: Mirrors,
    pub hooks: Webhooks,
}

/// What a merge did, for the audit line and the UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MergeOutcome {
    pub branch: String,
    pub into: String,
    pub old_tip: String,
    pub new_tip: String,
    pub fast_forward: bool,
    pub cards_done: Vec<String>,
    pub deleted_branch: bool,
}

/// What a restack did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RestackOutcome {
    pub branch: String,
    /// `(branch, old tip, new tip)` for every rebased descendant, in order.
    pub rebased: Vec<(String, String, String)>,
}

/// A scratch clone with the actor's identity wired into every commit.
struct Scratch {
    _dir: tempfile::TempDir,
    repo: Repo,
    identity: [String; 4],
}

impl Scratch {
    async fn of_mirror(mirror_path: &Path, auth: crate::git::Auth, actor: &Actor) -> OpResult<Self> {
        if !mirror_path.exists() {
            return Err(OpError::NotFound("mirror does not exist yet".to_owned()));
        }
        let dir = tempfile::tempdir().map_err(|e| OpError::Git(e.to_string()))?;
        let checkout = dir.path().join("repo");
        clone_local(mirror_path, &checkout).await?;
        let identity = [
            "-c".to_owned(),
            format!("user.name={}", actor.name),
            "-c".to_owned(),
            format!("user.email={}", email_for(&actor.login)),
        ];
        Ok(Self { _dir: dir, repo: Repo::worktree(checkout, auth), identity })
    }

    /// Run git with the actor's identity applied.
    async fn git(&self, args: &[&str]) -> OpResult<String> {
        let out = self.repo.try_run_with(&self.identity, args, None).await?;
        if out.ok() {
            Ok(out.stdout)
        } else {
            Err(OpError::Git(format!("git {} failed: {}", args.join(" "), out.stderr.trim())))
        }
    }

    /// Run git, keeping the failure for the caller to interpret.
    async fn try_git(&self, args: &[&str]) -> OpResult<crate::git::GitOutput> {
        Ok(self.repo.try_run_with(&self.identity, args, None).await?)
    }

    /// Check a remote branch out as a local branch of the same name.
    async fn checkout(&self, branch: &str) -> OpResult<()> {
        // The clone's default branch already exists locally; everything else comes
        // from origin. `-B` covers both without caring which case this is.
        self.git(&["checkout", "-B", branch, &format!("origin/{branch}")]).await?;
        Ok(())
    }

    /// Push refspecs to the real remote, atomically, with auth.
    async fn push(&self, remote: &str, force: bool, refspecs: &[String]) -> OpResult<()> {
        let mut args = vec!["push", "--atomic"];
        if force {
            args.push("--force");
        }
        args.push(remote);
        args.extend(refspecs.iter().map(String::as_str));
        let with_identity: Vec<&str> =
            self.identity.iter().map(String::as_str).chain(args).collect();
        let out = self
            .repo
            .try_run_with(&with_identity[..4], &with_identity[4..], Some(remote))
            .await?;
        if out.ok() {
            Ok(())
        } else {
            Err(OpError::Git(format!("push rejected: {}", out.stderr.trim())))
        }
    }
}

fn email_for(login: &str) -> String {
    if login.contains('@') { login.to_owned() } else { format!("{login}@tailnet") }
}

impl Ops {
    fn remote(&self, repo: &str) -> String {
        self.config.remote_url(repo)
    }

    /// Merge `branch` into its stack parent.
    ///
    /// Fast-forwards when the parent has not moved since the branch forked; otherwise
    /// creates a `--no-ff` merge commit. Blocked while CI for the tip is red or
    /// running, unless `allow_red`. Cards declaring `branch: <branch>` that are not
    /// `done` are flipped in the same push as a separate commit.
    pub async fn merge(
        &self,
        repo_name: &str,
        branch: &str,
        actor: &Actor,
        allow_red: bool,
        delete_branch: bool,
    ) -> OpResult<MergeOutcome> {
        self.mirrors.refresh_now(repo_name).await;
        let mirror = self.mirrors.repo(repo_name);
        let graph = StackGraph::infer(&mirror).await?;
        let node = graph
            .get(branch)
            .ok_or_else(|| OpError::NotFound(format!("branch {branch}")))?;
        let parent = node
            .parent
            .clone()
            .ok_or_else(|| OpError::Blocked("the default branch has no parent to merge into".into()))?;

        let ci = self.db.latest_run(repo_name, &node.tip).ok().flatten();
        let ci_status = ci.as_ref().map(|run| run.status.clone());
        if status::blocks_merge(ci_status.as_deref()) && !allow_red {
            return Err(OpError::Blocked(format!(
                "CI for {} is {}; confirm to merge anyway",
                &node.tip[..node.tip.len().min(8)],
                ci_status.as_deref().unwrap_or("unknown")
            )));
        }

        let branch_tip = node.tip.clone();
        let scratch =
            Scratch::of_mirror(&self.config.mirror_path(repo_name), self.mirrors.auth(), actor)
                .await?;
        scratch.checkout(&parent).await?;
        let old_tip = scratch.git(&["rev-parse", "HEAD"]).await?.trim().to_owned();

        // Parent unchanged since the fork means its tip is an ancestor of the branch.
        let ff = scratch
            .try_git(&["merge-base", "--is-ancestor", &old_tip, &branch_tip])
            .await?
            .ok();

        let merge = if ff {
            scratch.try_git(&["merge", "--ff-only", &branch_tip]).await?
        } else {
            let message = format!("Merge {branch} into {parent}");
            scratch.try_git(&["merge", "--no-ff", "-m", &message, &branch_tip]).await?
        };
        if !merge.ok() {
            let files = scratch
                .git(&["diff", "--name-only", "--diff-filter=U"])
                .await
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect();
            let _ = scratch.try_git(&["merge", "--abort"]).await;
            return Err(OpError::Conflict { branch: branch.to_owned(), files });
        }
        let new_tip = scratch.git(&["rev-parse", "HEAD"]).await?.trim().to_owned();

        // Card automation: flip `branch: <branch>` cards to done on the default branch.
        let default_branch = graph.default_branch.clone();
        let cards_done = self
            .flip_cards_done(&scratch, &mirror, &default_branch, &parent, repo_name, branch)
            .await?;

        let mut refspecs = vec![format!("refs/heads/{parent}:refs/heads/{parent}")];
        if !cards_done.is_empty() && default_branch != parent {
            refspecs.push(format!("refs/heads/{default_branch}:refs/heads/{default_branch}"));
        }
        if delete_branch && branch != default_branch {
            refspecs.push(format!(":refs/heads/{branch}"));
        }
        scratch.push(&self.remote(repo_name), false, &refspecs).await?;
        self.mirrors.refresh_now(repo_name).await;

        let mut detail = if ff {
            format!("fast-forwarded {parent} to {branch}")
        } else {
            format!("merged {branch} into {parent}")
        };
        if !cards_done.is_empty() {
            detail.push_str(&format!("; marked done: {}", cards_done.join(", ")));
        }
        if delete_branch {
            detail.push_str(&format!("; deleted {branch}"));
        }
        let _ = self.db.record_audit(NewAudit {
            repo: repo_name.to_owned(),
            actor: actor.login.clone(),
            action: "merge".to_owned(),
            branch: branch.to_owned(),
            old_tip: old_tip.clone(),
            new_tip: new_tip.clone(),
            detail,
        });
        self.hooks.send(
            hooks::MERGED,
            serde_json::json!({
                "repo": repo_name,
                "branch": branch,
                "into": parent,
                "actor": actor.login,
                "old_tip": old_tip,
                "new_tip": new_tip,
            }),
        );

        Ok(MergeOutcome {
            branch: branch.to_owned(),
            into: parent,
            old_tip,
            new_tip,
            fast_forward: ff,
            cards_done,
            deleted_branch: delete_branch,
        })
    }

    /// Rewrite not-done cards declaring `branch: <branch>` to `done`, committing on the
    /// default branch inside the scratch clone. Returns the card paths flipped.
    async fn flip_cards_done(
        &self,
        scratch: &Scratch,
        mirror: &Repo,
        default_branch: &str,
        parent: &str,
        repo_name: &str,
        branch: &str,
    ) -> OpResult<Vec<String>> {
        let tip = mirror.tip(default_branch).await?;
        let paths = mirror.list_files(&tip, docs::TASKS_DIR).await?;

        // Find the rewrites first: checking out the default branch would clobber a
        // merge commit sitting on it, so only switch branches when there is work.
        let mut rewrites: Vec<(String, String)> = Vec::new();
        for path in paths {
            if !(path.ends_with(".md") || path.ends_with(".markdown")) {
                continue;
            }
            let Some(bytes) = mirror.show_file(&tip, &path).await? else { continue };
            let source = String::from_utf8_lossy(&bytes).into_owned();
            let card = docs::parse_document(&path, &source);
            if card.refs.branch.as_deref() != Some(branch)
                || card.status.as_deref() == Some("done")
                || card.front_matter_error.is_some()
            {
                continue;
            }
            let Some(rewritten) = docs::rewrite_status(&source, "done") else { continue };
            rewrites.push((path, rewritten));
        }
        if rewrites.is_empty() {
            return Ok(Vec::new());
        }

        // When the merge target IS the default branch, the card commit goes on top of
        // the merge we just made; switching branches would throw the merge away.
        if parent != default_branch {
            scratch.checkout(default_branch).await?;
        }
        let mut flipped = Vec::new();
        for (path, rewritten) in rewrites {
            let on_disk = scratch.repo.path().join(&path);
            std::fs::write(&on_disk, rewritten).map_err(|e| OpError::Git(e.to_string()))?;
            scratch.git(&["add", &path]).await?;
            flipped.push(path);
        }
        let message = format!("Mark done: {} ({} merged)", flipped.join(", "), branch);
        scratch.git(&["commit", "-m", &message]).await?;
        tracing::info!(repo_name, branch, ?flipped, "cards flipped to done");
        // Back to the parent so the push refspec sees the right tips.
        if parent != default_branch {
            scratch.git(&["checkout", parent]).await?;
        }
        Ok(flipped)
    }

    /// Rebase every descendant of `branch` onto its (moved) parent, in stack order,
    /// then force-push all the new tips atomically. Any conflict aborts the whole
    /// restack before anything is pushed.
    pub async fn restack(
        &self,
        repo_name: &str,
        branch: &str,
        actor: &Actor,
    ) -> OpResult<RestackOutcome> {
        self.mirrors.refresh_now(repo_name).await;
        let mirror = self.mirrors.repo(repo_name);
        let graph = StackGraph::infer(&mirror).await?;
        if graph.get(branch).is_none() {
            return Err(OpError::NotFound(format!("branch {branch}")));
        }
        let descendants = graph.descendants(branch);
        if descendants.is_empty() {
            return Err(OpError::Blocked(format!("{branch} has no descendants to restack")));
        }

        let scratch =
            Scratch::of_mirror(&self.config.mirror_path(repo_name), self.mirrors.auth(), actor)
                .await?;

        // Tips move as the restack walks down the stack; children rebase onto the
        // parent's *new* tip.
        let mut new_tips: BTreeMap<String, String> = BTreeMap::new();
        let mut rebased: Vec<(String, String, String)> = Vec::new();

        for name in &descendants {
            let node = graph.get(name).expect("descendant exists in graph");
            let parent = node.parent.clone().expect("descendants always have a parent");
            let parent_mirror_tip =
                graph.get(&parent).map(|p| p.tip.clone()).unwrap_or_else(|| parent.clone());
            let onto = new_tips.get(&parent).cloned().unwrap_or_else(|| parent_mirror_tip.clone());

            scratch.checkout(name).await?;
            let old_tip = scratch.git(&["rev-parse", "HEAD"]).await?.trim().to_owned();
            // The commits that belong to this branch are fork..tip; the fork point is
            // the merge base with the parent as the mirror knows it.
            let base = scratch
                .git(&["merge-base", &parent_mirror_tip, &old_tip])
                .await?
                .trim()
                .to_owned();

            let rebase = scratch.try_git(&["rebase", "--onto", &onto, &base, name]).await?;
            if !rebase.ok() {
                let files: Vec<String> = scratch
                    .git(&["diff", "--name-only", "--diff-filter=U"])
                    .await
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_owned)
                    .collect();
                let _ = scratch.try_git(&["rebase", "--abort"]).await;
                return Err(OpError::Conflict { branch: name.clone(), files });
            }
            let new_tip = scratch.git(&["rev-parse", name]).await?.trim().to_owned();
            new_tips.insert(name.clone(), new_tip.clone());
            rebased.push((name.clone(), old_tip, new_tip));
        }

        // Nothing was pushed until here; one atomic force-push moves every branch.
        let refspecs: Vec<String> = rebased
            .iter()
            .map(|(name, _, _)| format!("refs/heads/{name}:refs/heads/{name}"))
            .collect();
        scratch.push(&self.remote(repo_name), true, &refspecs).await?;
        self.mirrors.refresh_now(repo_name).await;

        for (name, old_tip, new_tip) in &rebased {
            let _ = self.db.record_audit(NewAudit {
                repo: repo_name.to_owned(),
                actor: actor.login.clone(),
                action: "restack".to_owned(),
                branch: name.clone(),
                old_tip: old_tip.clone(),
                new_tip: new_tip.clone(),
                detail: format!("restacked after {branch} moved"),
            });
        }
        self.hooks.send(
            hooks::RESTACKED,
            serde_json::json!({
                "repo": repo_name,
                "branch": branch,
                "actor": actor.login,
                "branches": rebased.iter().map(|(n, _, _)| n.clone()).collect::<Vec<_>>(),
            }),
        );

        Ok(RestackOutcome { branch: branch.to_owned(), rebased })
    }

    /// Delete a branch on dgit (the post-merge offer). The default branch is refused.
    pub async fn delete_branch(&self, repo_name: &str, branch: &str, actor: &Actor) -> OpResult<()> {
        let mirror = self.mirrors.repo(repo_name);
        let default_branch = mirror.default_branch().await?;
        if branch == default_branch {
            return Err(OpError::Blocked("refusing to delete the default branch".into()));
        }
        let old_tip = mirror.tip(branch).await.map_err(|_| {
            OpError::NotFound(format!("branch {branch}"))
        })?;
        let remote = self.remote(repo_name);
        let out = mirror.run_remote(&remote, &["push", &remote, &format!(":refs/heads/{branch}")]).await?;
        if !out.ok() {
            return Err(OpError::Git(format!("delete rejected: {}", out.stderr.trim())));
        }
        self.mirrors.refresh_now(repo_name).await;
        let _ = self.db.record_audit(NewAudit {
            repo: repo_name.to_owned(),
            actor: actor.login.clone(),
            action: "merge".to_owned(),
            branch: branch.to_owned(),
            old_tip,
            new_tip: String::new(),
            detail: format!("deleted {branch}"),
        });
        Ok(())
    }

    /// Commit one file's new content to the default branch and push. The board's move
    /// endpoint sits on this; the push must succeed before the caller reports success.
    pub async fn commit_file(
        &self,
        repo_name: &str,
        path: &str,
        content: &str,
        message: &str,
        actor: &Actor,
    ) -> OpResult<String> {
        let mirror = self.mirrors.repo(repo_name);
        let default_branch = mirror.default_branch().await?;

        let scratch =
            Scratch::of_mirror(&self.config.mirror_path(repo_name), self.mirrors.auth(), actor)
                .await?;
        scratch.checkout(&default_branch).await?;

        let on_disk = scratch.repo.path().join(path);
        if let Some(parent) = on_disk.parent() {
            std::fs::create_dir_all(parent).map_err(|e| OpError::Git(e.to_string()))?;
        }
        std::fs::write(&on_disk, content).map_err(|e| OpError::Git(e.to_string()))?;
        scratch.git(&["add", path]).await?;
        scratch.git(&["commit", "-m", message]).await?;
        let new_tip = scratch.git(&["rev-parse", "HEAD"]).await?.trim().to_owned();

        let refspec = format!("refs/heads/{default_branch}:refs/heads/{default_branch}");
        scratch.push(&self.remote(repo_name), false, &[refspec]).await?;
        self.mirrors.refresh_now(repo_name).await;
        Ok(new_tip)
    }
}
