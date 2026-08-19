//! The only thing in nashcode that knows what git is: a thin shell-out layer.
//!
//! Every git question is answered by running the real `git` binary against a
//! `--mirror` clone or a scratch worktree. Nothing here parses packfiles, refs, or
//! any other on-disk git format.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use base64::Engine as _;

/// A git invocation that did not work out.
#[derive(Debug)]
pub enum GitError {
    /// The binary could not be run at all.
    Spawn(std::io::Error),
    /// git ran and exited nonzero.
    Failed {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "cannot run git: {error}"),
            Self::Failed { args, code, stderr } => {
                let code = code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_owned());
                write!(f, "git {} exited {code}: {}", args.join(" "), stderr.trim())
            }
        }
    }
}

impl std::error::Error for GitError {}

impl From<std::io::Error> for GitError {
    fn from(error: std::io::Error) -> Self {
        Self::Spawn(error)
    }
}

pub type GitResult<T> = Result<T, GitError>;

/// The captured result of a git run that was allowed to fail.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// How to talk to a remote. Basic auth `x:<token>` is what dgit expects for pushes;
/// reads are anonymous.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    token: String,
}

impl Auth {
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into() }
    }

    /// Global `-c` arguments that authenticate an HTTP remote.
    ///
    /// The credential goes in a header rather than in the URL, so it never reaches the
    /// reflog, the remote config, or a process listing of the URL.
    fn config_args(&self, remote: &str) -> Vec<String> {
        let http_remote = remote.starts_with("http://") || remote.starts_with("https://");
        if self.token.is_empty() || !http_remote {
            return Vec::new();
        }
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("x:{}", self.token));
        vec![
            "-c".to_owned(),
            format!("http.extraHeader=Authorization: Basic {encoded}"),
        ]
    }
}

/// A git repository on disk: either a bare mirror or a worktree.
#[derive(Debug, Clone)]
pub struct Repo {
    dir: PathBuf,
    bare: bool,
    auth: Auth,
}

impl Repo {
    /// A bare `--mirror` clone.
    pub fn mirror(dir: impl Into<PathBuf>, auth: Auth) -> Self {
        Self { dir: dir.into(), bare: true, auth }
    }

    /// A checked-out worktree.
    pub fn worktree(dir: impl Into<PathBuf>, auth: Auth) -> Self {
        Self { dir: dir.into(), bare: false, auth }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn exists(&self) -> bool {
        self.dir.exists()
    }

    fn base_args(&self) -> Vec<String> {
        if self.bare {
            vec!["--git-dir".to_owned(), self.dir.to_string_lossy().into_owned()]
        } else {
            vec!["-C".to_owned(), self.dir.to_string_lossy().into_owned()]
        }
    }

    /// Run git, returning stdout on success and an error otherwise.
    pub async fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> GitResult<String> {
        let output = self.try_run(args).await?;
        if output.ok() {
            Ok(output.stdout)
        } else {
            Err(GitError::Failed {
                args: args.iter().map(|a| a.as_ref().to_string_lossy().into_owned()).collect(),
                code: output.code,
                stderr: output.stderr,
            })
        }
    }

    /// Run git and hand back the exit status without treating nonzero as an error.
    /// Plumbing like `merge-base --is-ancestor` answers a question through its status.
    pub async fn try_run<S: AsRef<OsStr>>(&self, args: &[S]) -> GitResult<GitOutput> {
        self.try_run_with(&[] as &[&str], args, None).await
    }

    /// Run git with extra leading `-c` config and an optional remote whose auth the
    /// config should carry.
    pub async fn try_run_with<C: AsRef<OsStr>, S: AsRef<OsStr>>(
        &self,
        config: &[C],
        args: &[S],
        remote: Option<&str>,
    ) -> GitResult<GitOutput> {
        let mut command = tokio::process::Command::new("git");
        command.args(self.base_args());
        for arg in config {
            command.arg(arg);
        }
        if let Some(remote) = remote {
            command.args(self.auth.config_args(remote));
        }
        command.args(args);
        let limit = if remote.is_some() { REMOTE_TIMEOUT } else { LOCAL_TIMEOUT };
        run_command_within(command, limit).await
    }

    /// Run git against a remote with credentials applied.
    pub async fn run_remote<S: AsRef<OsStr>>(
        &self,
        remote: &str,
        args: &[S],
    ) -> GitResult<GitOutput> {
        self.try_run_with(&[] as &[&str], args, Some(remote)).await
    }

    // ---- questions -------------------------------------------------------------

    /// Local branch names, sorted. On a mirror these are the server's branches.
    pub async fn branches(&self) -> GitResult<Vec<String>> {
        let out = self
            .run(&["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
            .await?;
        let mut names: Vec<String> =
            out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
        names.sort();
        Ok(names)
    }

    /// Every branch and its tip, read in one command.
    ///
    /// Reading branches and then asking for each tip separately can straddle a fetch and
    /// return a mix of old and new tips, which yields wrong merge bases and wrong stack
    /// parents. One `for-each-ref` is a single consistent view, and one process instead
    /// of N.
    pub async fn tips(&self) -> GitResult<std::collections::BTreeMap<String, String>> {
        let out = self
            .run(&[
                "for-each-ref",
                "--format=%(refname:short)%09%(objectname)",
                "refs/heads/",
            ])
            .await?;
        Ok(out
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(name, id)| (name.trim().to_owned(), id.trim().to_owned()))
            .collect())
    }

    /// The commit a branch points at.
    pub async fn tip(&self, branch: &str) -> GitResult<String> {
        let out = self.run(&["rev-parse", &format!("refs/heads/{branch}")]).await?;
        Ok(out.trim().to_owned())
    }

    /// Resolve any revision to a commit id.
    ///
    /// `--verify` is what makes the answer one line. Without it `git rev-parse` echoes
    /// back every argument it does not recognise as a revision — `--end-of-options`
    /// included — so callers were getting two lines with the commit on the second.
    /// `--verify` promises exactly one object id and nothing else.
    pub async fn rev_parse(&self, rev: &str) -> GitResult<String> {
        Ok(self
            .run(&["rev-parse", "--verify", "--end-of-options", rev])
            .await?
            .trim()
            .to_owned())
    }

    /// The repo's default branch, as recorded in `HEAD`. Falls back to whichever of
    /// `main` or `master` exists, then to the first branch.
    pub async fn default_branch(&self) -> GitResult<String> {
        if let Ok(head) = self.run(&["symbolic-ref", "--short", "HEAD"]).await {
            let head = head.trim().to_owned();
            if !head.is_empty() {
                return Ok(head);
            }
        }
        let branches = self.branches().await?;
        for candidate in ["main", "master"] {
            if branches.iter().any(|b| b == candidate) {
                return Ok(candidate.to_owned());
            }
        }
        branches.into_iter().next().ok_or_else(|| GitError::Failed {
            args: vec!["default-branch".to_owned()],
            code: None,
            stderr: "repository has no branches".to_owned(),
        })
    }

    /// Is `ancestor` an ancestor of `descendant`? (A commit is its own ancestor.)
    pub async fn is_ancestor(&self, ancestor: &str, descendant: &str) -> GitResult<bool> {
        let out = self
            .try_run(&["merge-base", "--is-ancestor", ancestor, descendant])
            .await?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(GitError::Failed {
                args: vec!["merge-base".into(), "--is-ancestor".into()],
                code: out.code,
                stderr: out.stderr,
            }),
        }
    }

    /// The best common ancestor of two revisions.
    pub async fn merge_base(&self, a: &str, b: &str) -> GitResult<String> {
        Ok(self.run(&["merge-base", a, b]).await?.trim().to_owned())
    }

    /// How many commits `head` has that `base` does not.
    pub async fn count_ahead(&self, base: &str, head: &str) -> GitResult<usize> {
        let out = self
            .run(&["rev-list", "--count", &format!("{base}..{head}")])
            .await?;
        Ok(out.trim().parse().unwrap_or(0))
    }

    /// Commits in `base..head`, newest first.
    pub async fn commits(&self, base: &str, head: &str) -> GitResult<Vec<Commit>> {
        // %x1f separates fields, %x1e separates records: neither appears in commit text.
        let format = "--format=%H%x1f%h%x1f%an%x1f%aI%x1f%s%x1e";
        let out = self.run(&["log", format, &format!("{base}..{head}")]).await?;
        Ok(parse_commits(&out))
    }

    /// The `count` most recent commits reachable from a revision, newest first.
    /// Works on histories shorter than `count`.
    pub async fn recent_commits(&self, rev: &str, count: usize) -> GitResult<Vec<Commit>> {
        let format = "--format=%H%x1f%h%x1f%an%x1f%aI%x1f%s%x1e";
        let out = self
            .run(&["log", &format!("--max-count={count}"), format, "--end-of-options", rev])
            .await?;
        Ok(parse_commits(&out))
    }

    /// The most recent commit reachable from a revision.
    pub async fn last_commit(&self, rev: &str) -> GitResult<Option<Commit>> {
        let format = "--format=%H%x1f%h%x1f%an%x1f%aI%x1f%s%x1e";
        let out = self.run(&["log", "-1", format, rev]).await?;
        Ok(parse_commits(&out).into_iter().next())
    }

    /// Paths changed between two revisions, using three-dot semantics.
    pub async fn changed_files(&self, base: &str, head: &str) -> GitResult<Vec<ChangedFile>> {
        let out = self
            .run(&[
                "diff",
                "--name-status",
                "--find-renames",
                "-z",
                &format!("{base}...{head}"),
            ])
            .await?;
        Ok(parse_name_status(&out))
    }

    /// The unified diff of one path between two revisions, three-dot semantics.
    pub async fn file_diff(&self, base: &str, head: &str, path: &str) -> GitResult<String> {
        self.run(&[
            "diff",
            "--no-color",
            "--find-renames",
            &format!("{base}...{head}"),
            "--",
            path,
        ])
        .await
    }

    /// True when the path changed at all between two revisions (two-dot).
    pub async fn path_changed(&self, from: &str, to: &str, path: &str) -> GitResult<bool> {
        let out = self
            .try_run(&["diff", "--quiet", from, to, "--", path])
            .await?;
        Ok(out.code != Some(0))
    }

    /// Read a blob at a revision. `None` when the path does not exist there.
    pub async fn show_file(&self, rev: &str, path: &str) -> GitResult<Option<Vec<u8>>> {
        let mut command = tokio::process::Command::new("git");
        command.args(self.base_args());
        // `--end-of-options` stops a revision that starts with `-` from being parsed
        // as a flag: these strings arrive from URL segments.
        command.args(["show", "--end-of-options", &format!("{rev}:{path}")]);
        let output = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        if output.status.success() {
            Ok(Some(output.stdout))
        } else {
            Ok(None)
        }
    }

    /// Run git and stop reading after `max_bytes` of output, killing the child.
    ///
    /// `git grep` is the reason this exists. Its `--max-count` is *per file*, so a
    /// common word in a large repo produces that many matches per file across every
    /// file, and `output()` would buffer the lot before anyone could truncate it. The
    /// bool is whether the cap was hit, which the caller reports rather than hides.
    pub async fn run_capped<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        max_bytes: usize,
    ) -> GitResult<(GitOutput, bool)> {
        let (code, bytes, capped) = self.run_capped_bytes(args, max_bytes).await?;
        Ok((
            GitOutput {
                code,
                stdout: String::from_utf8_lossy(&bytes).into_owned(),
                stderr: String::new(),
            },
            capped,
        ))
    }

    /// [`run_capped`](Self::run_capped) without the lossy string conversion.
    ///
    /// Whether output is text is the caller's question here: replacing invalid
    /// sequences with `U+FFFD` would turn "this is not text" into "this is text with
    /// odd characters in it", which is exactly the decision the code index has to make.
    pub async fn run_capped_bytes<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        max_bytes: usize,
    ) -> GitResult<(Option<i32>, Vec<u8>, bool)> {
        use tokio::io::AsyncReadExt as _;

        let mut command = tokio::process::Command::new("git");
        command.args(self.base_args());
        command.args(args);
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = command.spawn()?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let deadline = tokio::time::Instant::now() + LOCAL_TIMEOUT;
        let mut collected: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 16 * 1024];
        let mut capped = false;

        loop {
            match tokio::time::timeout_at(deadline, stdout.read(&mut buffer)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(read)) => {
                    collected.extend_from_slice(&buffer[..read]);
                    if collected.len() >= max_bytes {
                        capped = true;
                        break;
                    }
                }
                Ok(Err(_)) => break,
            }
        }

        let code = if capped {
            // The reader has what it asked for; the writer is now talking to a closed
            // pipe and would otherwise run to completion for nothing.
            let _ = child.start_kill();
            let _ = child.wait().await;
            None
        } else {
            child.wait().await.ok().and_then(|status| status.code())
        };

        collected.truncate(max_bytes);
        Ok((code, collected, capped))
    }

    /// A blob's bytes by object id, with no path and no revision involved.
    ///
    /// `show_file` addresses content as `<rev>:<path>`, which is what a page needs.
    /// The code index works the other way round — it already has the object id and
    /// does not care which path it came from — so it asks git for the object itself.
    /// `None` when the object is absent or is not a blob.
    /// Bytes rather than a `String`, because the caller has to decide whether this is
    /// text at all; the byte cap is the size `ls-tree` already reported plus slack.
    pub async fn read_blob(&self, id: &str, max_bytes: usize) -> GitResult<Option<Vec<u8>>> {
        // Through the shared runner so a blob read gets the same timeout,
        // kill-on-drop, and GIT_TERMINAL_PROMPT=0 as every other git call: an index
        // run walks thousands of these and must not be the one path that can hang.
        let (code, bytes, _capped) =
            self.run_capped_bytes(&["cat-file", "blob", id], max_bytes).await?;
        // A missing object exits nonzero with nothing on stdout.
        if code.is_some_and(|code| code != 0) && bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// One directory level at a revision, in git's own tree order. `None` when `dir`
    /// names nothing, or names something that is not a directory.
    ///
    /// The tree is addressed as `<rev>:<dir>` rather than through a pathspec. Path
    /// segments arrive from a URL, and a pathspec would read `*` or a leading `:` in
    /// one of them as a pattern; object addressing has no such syntax and cannot
    /// escape the tree.
    pub async fn ls_tree(&self, rev: &str, dir: &str) -> GitResult<Option<Vec<TreeEntry>>> {
        let dir = dir.trim_matches('/');
        let treeish =
            if dir.is_empty() { rev.to_owned() } else { format!("{rev}:{dir}") };
        let out = self
            .try_run(&["ls-tree", "-z", "--long", "--end-of-options", &treeish])
            .await?;
        if !out.ok() {
            return Ok(None);
        }
        Ok(Some(parse_ls_tree(&out.stdout, dir)))
    }

    /// When the last commit reachable from `rev` touched `path` (author date,
    /// RFC3339). `None` when no commit touched it.
    pub async fn last_touched(&self, rev: &str, path: &str) -> GitResult<Option<String>> {
        let out = self.run(&["log", "-1", "--format=%aI", rev, "--", path]).await?;
        let when = out.trim();
        Ok((!when.is_empty()).then(|| when.to_owned()))
    }

    /// List files under a directory prefix at a revision. An empty prefix lists the
    /// whole tree (git rejects an empty pathspec, so it is simply omitted).
    pub async fn list_files(&self, rev: &str, prefix: &str) -> GitResult<Vec<String>> {
        let mut args = vec!["ls-tree", "-r", "--name-only", "-z", rev];
        if !prefix.is_empty() {
            args.push("--");
            args.push(prefix);
        }
        let out = self.try_run(&args).await?;
        if !out.ok() {
            return Ok(Vec::new());
        }
        Ok(out
            .stdout
            .split('\0')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

/// One commit, flattened for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub short: String,
    pub author: String,
    /// Author date, RFC3339.
    pub date: String,
    pub subject: String,
}

/// What a tree entry is. A submodule is a `commit` entry: it has no content here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Submodule,
}

/// One entry in a single directory level of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The entry's own name, without the directory it sits in.
    pub name: String,
    /// The full repo-relative path.
    pub path: String,
    pub kind: EntryKind,
    /// Blob size in bytes. `None` for directories and submodules.
    pub size: Option<u64>,
}

impl TreeEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

/// `ls-tree -z --long` emits `<mode> SP <type> SP <object> SP..SP <size> TAB <path>\0`.
/// `-z` means the path is literal — never quoted — so the first tab always ends the
/// metadata and everything after it is the name, tabs included.
fn parse_ls_tree(raw: &str, dir: &str) -> Vec<TreeEntry> {
    raw.split('\0')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let (meta, name) = record.split_once('\t')?;
            let mut fields = meta.split_whitespace();
            let mode = fields.next()?;
            let kind = match (fields.next()?, mode) {
                ("tree", _) => EntryKind::Dir,
                ("commit", _) => EntryKind::Submodule,
                ("blob", "120000") => EntryKind::Symlink,
                ("blob", _) => EntryKind::File,
                _ => return None,
            };
            let _object = fields.next()?;
            // `-` for anything that is not a blob.
            let size = fields.next().and_then(|size| size.parse().ok());
            let path =
                if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") };
            Some(TreeEntry { name: name.to_owned(), path, kind, size })
        })
        .collect()
}

/// One path in a diff, with the status letter git reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// `A`dded, `M`odified, `D`eleted, `R`enamed, ...
    pub status: String,
    /// Set for renames and copies.
    pub old_path: Option<String>,
}

fn parse_commits(raw: &str) -> Vec<Commit> {
    raw.split('\u{1e}')
        .map(str::trim)
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let mut fields = record.split('\u{1f}');
            Some(Commit {
                id: fields.next()?.to_owned(),
                short: fields.next()?.to_owned(),
                author: fields.next()?.to_owned(),
                date: fields.next()?.to_owned(),
                subject: fields.next().unwrap_or("").to_owned(),
            })
        })
        .collect()
}

/// `--name-status -z` emits `STATUS\0path\0`, and for renames
/// `R100\0old\0new\0`. Walk the NUL-separated stream as a small state machine.
fn parse_name_status(raw: &str) -> Vec<ChangedFile> {
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let letter = status.chars().next().unwrap_or('M');
        if letter == 'R' || letter == 'C' {
            let (Some(old), Some(new)) = (fields.next(), fields.next()) else {
                break;
            };
            files.push(ChangedFile {
                path: new.to_owned(),
                status: letter.to_string(),
                old_path: Some(old.to_owned()),
            });
        } else {
            let Some(path) = fields.next() else { break };
            files.push(ChangedFile {
                path: path.to_owned(),
                status: letter.to_string(),
                old_path: None,
            });
        }
    }
    files
}

/// How long a local git command may run before it is treated as wedged.
pub const LOCAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a command that talks to the network may run. Longer, because a first
/// clone of a real repo is not fast.
pub const REMOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

async fn run_command(command: tokio::process::Command) -> GitResult<GitOutput> {
    run_command_within(command, LOCAL_TIMEOUT).await
}

/// Run git, killing it if it outlives `limit`.
///
/// Without this a blackholed peer hangs the fetch forever, and because the caller holds
/// the repo's lock across the call, every later request for that repo queues behind it.
/// A hang is worse than an error: the error path already degrades gracefully.
async fn run_command_within(
    mut command: tokio::process::Command,
    limit: std::time::Duration,
) -> GitResult<GitOutput> {
    // A hung credential or editor prompt would wedge the request forever.
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    match tokio::time::timeout(limit, command.output()).await {
        Ok(output) => {
            let output = output?;
            Ok(GitOutput {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
        Err(_) => Err(GitError::Failed {
            args: vec!["<timed out>".to_owned()],
            code: None,
            stderr: format!("git did not finish within {}s", limit.as_secs()),
        }),
    }
}

/// `git clone --mirror <remote> <dir>`.
pub async fn clone_mirror(remote: &str, dir: &Path, auth: &Auth) -> GitResult<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut command = tokio::process::Command::new("git");
    command.args(auth.config_args(remote));
    command.args(["clone", "--mirror", remote]);
    command.arg(dir);
    let output = run_command_within(command, REMOTE_TIMEOUT).await?;
    if output.ok() {
        Ok(())
    } else {
        Err(GitError::Failed {
            args: vec!["clone".to_owned(), "--mirror".to_owned()],
            code: output.code,
            stderr: output.stderr,
        })
    }
}

/// `git clone --no-checkout <src> <dst>` from one local path to another.
pub async fn clone_local(src: &Path, dst: &Path) -> GitResult<()> {
    let mut command = tokio::process::Command::new("git");
    command.arg("clone").arg("--no-checkout").arg(src).arg(dst);
    let output = run_command(command).await?;
    if output.ok() {
        Ok(())
    } else {
        Err(GitError::Failed {
            args: vec!["clone".to_owned()],
            code: output.code,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_parses_renames_and_plain_changes() {
        let raw = "M\0src/a.rs\0R100\0old.rs\0new.rs\0A\0added.rs\0";
        let files = parse_name_status(raw);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].status, "M");
        assert_eq!(files[1].path, "new.rs");
        assert_eq!(files[1].old_path.as_deref(), Some("old.rs"));
        assert_eq!(files[2].status, "A");
    }

    #[test]
    fn ls_tree_records_carry_kind_size_and_full_path() {
        let raw = "040000 tree aaa       -\tsrc\0\
                   100644 blob bbb      12\tREADME.md\0\
                   120000 blob ccc       7\tlink\0\
                   160000 commit ddd       -\tvendor\0";
        let entries = parse_ls_tree(raw, "docs");
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].path, "docs/src");
        assert_eq!(entries[0].size, None);
        assert_eq!(entries[1].kind, EntryKind::File);
        assert_eq!(entries[1].size, Some(12));
        assert_eq!(entries[2].kind, EntryKind::Symlink);
        assert_eq!(entries[3].kind, EntryKind::Submodule);
    }

    #[test]
    fn a_tab_in_a_path_stays_part_of_the_name() {
        let entries = parse_ls_tree("100644 blob abc       3\tod\td\0", "");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "od\td");
        assert_eq!(entries[0].path, "od\td");
    }

    #[test]
    fn commit_records_survive_subjects_containing_separators() {
        let raw = "abc\u{1f}abc\u{1f}Ada\u{1f}2026-01-01T00:00:00Z\u{1f}fix: a, b\u{1e}";
        let commits = parse_commits(raw);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "fix: a, b");
    }

    #[test]
    fn auth_headers_apply_only_to_http_remotes() {
        let auth = Auth::new("secret");
        assert!(auth.config_args("/srv/git/repo.git").is_empty());
        let args = auth.config_args("https://example.invalid/repo.git");
        assert_eq!(args.len(), 2);
        assert!(args[1].starts_with("http.extraHeader=Authorization: Basic "));
        assert!(!args[1].contains("secret"), "token must be encoded, not literal");
    }

    #[test]
    fn empty_token_adds_no_config() {
        assert!(Auth::default().config_args("https://example.invalid/r.git").is_empty());
    }
}
