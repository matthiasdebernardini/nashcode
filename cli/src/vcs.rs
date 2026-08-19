//! The working copy in the current directory: git, jj, or neither.
//!
//! jj (Jujutsu) pushes to git remotes natively, so dgit needs no server-side
//! change for it. The only difference is client politeness: a jj repo wants
//! `jj git remote add`, not `git remote add`. Credentials are the same either
//! way — jj uses git's credential machinery — so the `git credential approve`
//! path runs for both.
//!
//! `$NASHCODE_GIT_BIN` and `$NASHCODE_JJ_BIN` replace the executables. Detection
//! itself reads only the directory layout, so it needs neither binary.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A plain git working copy: `.git` and no `.jj`.
    Git,
    /// A colocated jj repo: both `.jj` and `.git`. git tooling still works.
    JjColocated,
    /// A jj-native repo: `.jj` and no `.git`.
    JjOnly,
}

impl Kind {
    pub fn is_jj(self) -> bool {
        matches!(self, Kind::JjColocated | Kind::JjOnly)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Git => "git",
            Kind::JjColocated => "jj-colocated",
            Kind::JjOnly => "jj",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub root: PathBuf,
    pub kind: Kind,
}

impl Workspace {
    /// The repository name to use when the user did not name one: the
    /// directory name of the workspace root. Same rule for git and jj.
    pub fn default_repo_name(&self) -> Option<String> {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
    }
}

/// Classify one directory. `None` when it is not a repository root.
///
/// `.jj` wins over `.git` because a colocated repo has both, and the user
/// drives it with jj.
pub fn classify(dir: &Path) -> Option<Kind> {
    let jj = dir.join(".jj").exists();
    let git = dir.join(".git").exists();
    match (jj, git) {
        (true, true) => Some(Kind::JjColocated),
        (true, false) => Some(Kind::JjOnly),
        (false, true) => Some(Kind::Git),
        (false, false) => None,
    }
}

/// Walk up from `start` to the first directory that is a repository root.
pub fn detect(start: &Path) -> Option<Workspace> {
    let mut dir = start;
    loop {
        if let Some(kind) = classify(dir) {
            return Some(Workspace {
                root: dir.to_path_buf(),
                kind,
            });
        }
        dir = dir.parent()?;
    }
}

/// Detect from the current directory.
pub fn detect_cwd() -> Result<Option<Workspace>> {
    let cwd = std::env::current_dir().context("read current directory")?;
    Ok(detect(&cwd))
}

/// Same, but an error instead of `None`.
pub fn require_cwd() -> Result<Workspace> {
    detect_cwd()?.ok_or_else(|| {
        anyhow::anyhow!("not inside a git or jj repository (looked for .git / .jj from the cwd up)")
    })
}

fn git_bin() -> String {
    std::env::var("NASHCODE_GIT_BIN").unwrap_or_else(|_| "git".to_string())
}

fn jj_bin() -> String {
    std::env::var("NASHCODE_JJ_BIN").unwrap_or_else(|_| "jj".to_string())
}

#[derive(Debug, Clone)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

fn run_in(dir: &Path, bin: &str, args: &[&str], stdin: Option<&str>) -> Result<Run> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(dir)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn `{bin} {}`", args.join(" ")))?;
    if let Some(data) = stdin {
        child
            .stdin
            .take()
            .context("stdin")?
            .write_all(data.as_bytes())
            .context("write stdin")?;
    }
    let out = child.wait_with_output().context("wait for child")?;
    Ok(Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run git in a directory.
pub fn git(dir: &Path, args: &[&str]) -> Result<Run> {
    run_in(dir, &git_bin(), args, None)
}

/// Run jj in a directory.
pub fn jj(dir: &Path, args: &[&str]) -> Result<Run> {
    run_in(dir, &jj_bin(), args, None)
}

impl Workspace {
    /// Point `origin` at `url`, creating it or replacing an existing one.
    ///
    /// Returns the command that was run, for the caller to echo.
    pub fn set_origin(&self, url: &str) -> Result<String> {
        if self.kind.is_jj() {
            let existing = jj(&self.root, &["git", "remote", "list"])?;
            let has_origin = existing
                .stdout
                .lines()
                .any(|l| l.split_whitespace().next() == Some("origin"));
            let verb = if has_origin { "set-url" } else { "add" };
            let r = jj(&self.root, &["git", "remote", verb, "origin", url])?;
            if !r.ok() {
                bail!("jj git remote {verb} origin failed: {}", r.stderr.trim());
            }
            Ok(format!("jj git remote {verb} origin {url}"))
        } else {
            let existing = git(&self.root, &["remote"])?;
            let has_origin = existing.stdout.lines().any(|l| l.trim() == "origin");
            let args: Vec<&str> = if has_origin {
                vec!["remote", "set-url", "origin", url]
            } else {
                vec!["remote", "add", "origin", url]
            };
            let r = git(&self.root, &args)?;
            if !r.ok() {
                bail!("git {} failed: {}", args.join(" "), r.stderr.trim());
            }
            Ok(format!("git {}", args.join(" ")))
        }
    }

    /// The branch the working copy is on, or `None` when it is on none.
    ///
    /// git's `symbolic-ref` answers for a plain repo and for a colocated jj one,
    /// where jj keeps git's HEAD pointed at the checked-out bookmark. A detached
    /// HEAD, a jj-only repo, or no git at all all mean "no branch to name" —
    /// callers treat the branch as optional, so a missing answer is not an error.
    pub fn current_branch(&self) -> Result<Option<String>> {
        if self.kind == Kind::JjOnly {
            return Ok(None);
        }
        let r = git(&self.root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        Ok(r.ok()
            .then(|| r.stdout.trim().to_string())
            .filter(|b| !b.is_empty()))
    }

    /// The name `origin` currently points at, e.g. `myrepo` from
    /// `https://host/myrepo.git`. Used as the default for `nashcode comments`.
    pub fn origin_repo_name(&self) -> Result<Option<String>> {
        let url = if self.kind.is_jj() {
            let r = jj(&self.root, &["git", "remote", "list"])?;
            r.stdout
                .lines()
                .find_map(|l| {
                    let mut parts = l.split_whitespace();
                    (parts.next()? == "origin").then(|| parts.next().unwrap_or("").to_string())
                })
                .filter(|u| !u.is_empty())
        } else {
            let r = git(&self.root, &["remote", "get-url", "origin"])?;
            r.ok().then(|| r.stdout.trim().to_string()).filter(|u| !u.is_empty())
        };
        Ok(url.as_deref().and_then(repo_name_from_url))
    }
}

/// `https://host/myrepo.git` -> `myrepo`. Also handles `scp`-style git URLs.
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()?
        .trim_end_matches(".git");
    (!last.is_empty()).then(|| last.to_string())
}

/// Hand a token to git's credential store so pushes are authenticated without
/// the token ever appearing in a remote URL or in `argv`.
///
/// Works for jj too: jj delegates to git's credential helpers.
pub fn credential_approve(host: &str, username: &str, password: &str) -> Result<()> {
    let dir = std::env::current_dir().context("read current directory")?;
    let input = format!("protocol=https\nhost={host}\nusername={username}\npassword={password}\n\n");
    let r = run_in(&dir, &git_bin(), &["credential", "approve"], Some(&input))?;
    if !r.ok() {
        bail!("git credential approve failed: {}", r.stderr.trim());
    }
    Ok(())
}

/// Which credential helper git will use for `url`, if any. Empty means none,
/// and then `credential approve` silently discards the token.
///
/// `--get-urlmatch` rather than `--get`: it sees URL-scoped configuration
/// (`credential.https://host.helper`) and resolves multi-valued helpers the
/// way git itself will.
pub fn credential_helper(url: &str) -> Result<Option<String>> {
    let dir = std::env::current_dir().context("read current directory")?;
    let r = run_in(
        &dir,
        &git_bin(),
        &["config", "--get-urlmatch", "credential.helper", url],
        None,
    )?;
    let v = r.stdout.trim().to_string();
    Ok((r.ok() && !v.is_empty()).then_some(v))
}

/// Is jj usable on this machine?
///
/// Goes through the same seam as every other jj call, so a test can make the
/// answer "no" without uninstalling anything.
pub fn jj_available() -> bool {
    if let Ok(v) = std::env::var("NASHCODE_JJ_AVAILABLE") {
        return v == "1";
    }
    Command::new(jj_bin())
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Did the user ask for jj here? `--jj` or `$NASHCODE_JJ=1`, nothing else.
///
/// This is the rule for `new` and `clone`, which act on repositories that
/// already have (or will get) a git working copy: colocating jj on top is
/// opt-in, so having jj installed does not change what `clone` produces.
pub fn jj_requested(force_jj: bool) -> bool {
    force_jj || std::env::var("NASHCODE_JJ").as_deref() == Ok("1")
}

/// Should a brand-new working copy be jj? `--jj` and `--git` decide it
/// outright; otherwise jj wins when it is installed, or when `$NASHCODE_JJ=1`.
/// This is the rule for `init`, which creates a working copy from nothing.
pub fn prefer_jj(force_jj: bool, force_git: bool) -> bool {
    if force_git {
        return false;
    }
    if force_jj || std::env::var("NASHCODE_JJ").as_deref() == Ok("1") {
        return true;
    }
    jj_available()
}

/// `jj git init --colocate`, so both jj and git tooling work in the directory.
pub fn jj_init_colocate(dir: &Path) -> Result<()> {
    let r = jj(dir, &["git", "init", "--colocate"])?;
    if !r.ok() {
        bail!("jj git init --colocate failed: {}", r.stderr.trim());
    }
    Ok(())
}

/// `git init -b <branch>`.
pub fn git_init(dir: &Path, branch: &str) -> Result<()> {
    let r = git(dir, &["init", "--quiet", "-b", branch])?;
    if !r.ok() {
        bail!("git init failed: {}", r.stderr.trim());
    }
    Ok(())
}

/// The platform's usual credential helper, for the hint we print when none is set.
pub fn suggested_helper() -> &'static str {
    if cfg!(target_os = "macos") {
        "osxkeychain"
    } else {
        "store"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_from_url_handles_the_usual_shapes() {
        assert_eq!(repo_name_from_url("https://h/myrepo.git").unwrap(), "myrepo");
        assert_eq!(repo_name_from_url("https://h/myrepo").unwrap(), "myrepo");
        assert_eq!(repo_name_from_url("git@h:myrepo.git").unwrap(), "myrepo");
        assert_eq!(repo_name_from_url("https://h/a/b/c.git/").unwrap(), "c");
    }

    #[test]
    fn jj_wins_over_git_in_a_colocated_repo() {
        assert!(Kind::JjColocated.is_jj());
        assert!(Kind::JjOnly.is_jj());
        assert!(!Kind::Git.is_jj());
    }
}
