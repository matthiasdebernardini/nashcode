//! The upstream column: `.nashcode/stack.toml`, and the read-only mirrors it names.
//!
//! A repo declares the code it is built on, and the viewer keeps a `git clone --mirror`
//! copy of each dependency beside its own mirrors. Three rules shape the module.
//!
//! **Nothing is fetched unless the repo said something we understand.** A URL that is
//! not `http` or `https`, a `[[dep]]` that pins and tracks at once, a name that could be
//! a path — each becomes an error recorded against that dep, and no request is ever made
//! for it. A manifest that will not parse at all leaves every other feature of the repo
//! alone: the error travels in the brain stanza and nothing else changes. A repo with no
//! manifest has no stack, and pays one lookup for the privilege.
//!
//! **One mirror per clone URL, for the whole box.** The mirror directory is derived from
//! the URL — `<mirrors>/up/<host>/<path>.git` — so two repos declaring the same
//! dependency share one copy. Every path component is checked against a conservative
//! ASCII set rather than rewritten: a URL that cannot be spelled as a directory is
//! refused, because rewriting it would let two different URLs collapse onto one mirror.
//!
//! **Never block, never fail.** [`Upstreams::stack`] reports what is on disk and starts
//! whatever is due behind the caller's back, the way [`crate::mirror`] does; a fetch that
//! cannot reach the server leaves the mirror in place and records why. Only
//! [`Upstreams::sync`] waits, because its caller asked for the wire.
//!
//! Upstream mirrors are read-only everywhere. Nothing here pushes, and they are not in
//! `config.repos`, so no repo route, listing, or write path can name one. They are also
//! fetched anonymously: `GIT_TOKEN` is the dgit push token and has no business being
//! sent to github.com.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;
use url::Url;

use crate::config::Config;
use crate::git::{Auth, Repo, clone_mirror};

/// Where the manifest lives, in the repo, at the default-branch tip.
pub const MANIFEST_PATH: &str = ".nashcode/stack.toml";

/// How often a `track` dep goes back to the wire, and how long a dep whose commit is
/// still missing waits before trying again. Upstreams are somebody else's repo: half an
/// hour is fresh enough to notice drift and slow enough to be a good neighbour.
pub const TRACK_INTERVAL: Duration = Duration::from_secs(30 * 60);

// ---- the manifest ----------------------------------------------------------------

/// How a dep says which commit it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// An exact commit. Fetched until it is on disk, then never again.
    Pin(String),
    /// A branch. Refreshed on [`TRACK_INTERVAL`], and on demand.
    Track(String),
}

impl Mode {
    /// The word the brain stanza uses.
    pub fn kind(&self) -> &'static str {
        match self {
            Mode::Pin(_) => "pin",
            Mode::Track(_) => "track",
        }
    }

    /// The revision as declared: a commit for a pin, a branch name for a track.
    pub fn want(&self) -> &str {
        match self {
            Mode::Pin(rev) | Mode::Track(rev) => rev,
        }
    }
}

/// One `[[dep]]`, after validation.
///
/// A dep that failed validation keeps its place in the list — the point is to say so in
/// the brain stanza — but carries `error` and no [`Mode`] or path, and so is never
/// fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub name: String,
    pub url: String,
    pub layer: Option<String>,
    /// `None` when the declaration is unusable; `error` says why.
    pub mode: Option<Mode>,
    /// Where this URL's mirror lives. `None` when the URL was refused.
    pub path: Option<PathBuf>,
    pub error: Option<String>,
}

/// A parsed `.nashcode/stack.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub deps: Vec<Dep>,
    /// The file itself would not parse. `deps` is empty.
    pub error: Option<String>,
}

/// The manifest as TOML sees it. Every field is optional so that one bad `[[dep]]`
/// becomes one error in the stanza rather than a parse failure that hides the rest.
#[derive(Debug, serde::Deserialize)]
struct RawManifest {
    #[serde(default)]
    dep: Vec<RawDep>,
}

#[derive(Debug, serde::Deserialize)]
struct RawDep {
    name: Option<String>,
    url: Option<String>,
    pin: Option<String>,
    track: Option<String>,
    layer: Option<String>,
}

/// Parse a manifest, resolving each dep's mirror directory under `mirrors`.
pub fn parse(mirrors: &Path, raw: &str) -> Manifest {
    let parsed: RawManifest = match toml::from_str(raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Manifest { deps: Vec::new(), error: Some(first_line(&error.to_string())) };
        }
    };

    let mut deps: Vec<Dep> = Vec::with_capacity(parsed.dep.len());
    for raw in parsed.dep {
        let name = raw.name.unwrap_or_default().trim().to_owned();
        let url = raw.url.unwrap_or_default().trim().to_owned();
        let layer = raw.layer.map(|layer| layer.trim().to_owned()).filter(|l| !l.is_empty());
        let duplicate = deps.iter().any(|other| other.name == name);

        let mut dep = Dep { name, url, layer, mode: None, path: None, error: None };
        dep.error = validate(&mut dep, mirrors, raw.pin, raw.track, duplicate);
        if dep.error.is_some() {
            // Belt and braces: nothing downstream may act on a dep we refused.
            dep.mode = None;
            dep.path = None;
        }
        deps.push(dep);
    }

    Manifest { deps, error: None }
}

/// Fill in `mode` and `path`, or say why neither could be had.
fn validate(
    dep: &mut Dep,
    mirrors: &Path,
    pin: Option<String>,
    track: Option<String>,
    duplicate: bool,
) -> Option<String> {
    if !is_plain_name(&dep.name) {
        return Some(format!(
            "name {:?} is not a plain name: no separators, no traversal, no leading dash",
            dep.name
        ));
    }
    if duplicate {
        return Some(format!("name {:?} is declared twice", dep.name));
    }

    dep.mode = match (pin.as_deref().map(str::trim), track.as_deref().map(str::trim)) {
        (Some(pin), None) if !pin.is_empty() => {
            if !is_commit_ish(pin) {
                return Some(format!("pin {pin:?} is not a commit id"));
            }
            Some(Mode::Pin(pin.to_ascii_lowercase()))
        }
        (None, Some(track)) if !track.is_empty() => {
            if !is_safe_ref(track) {
                return Some(format!("track {track:?} is not a branch name"));
            }
            Some(Mode::Track(track.to_owned()))
        }
        (Some(_), Some(_)) => return Some("declares both pin and track; pick one".to_owned()),
        _ => return Some("declares neither pin nor track; pick one".to_owned()),
    };

    match mirror_path(mirrors, &dep.url) {
        Ok(path) => dep.path = Some(path),
        Err(why) => return Some(why),
    }
    None
}

/// Where a clone URL's mirror lives, under `<mirrors>/up/`.
///
/// The URL is the key: the host and its path become the directory, so two repos naming
/// the same dependency share one mirror and the box never holds two copies of celld.
/// Normalization is the small, documented set — the scheme and host are lowered by the
/// URL parser, a trailing `/` and a trailing `.git` come off — so
/// `https://GitHub.com/a/b.git/` and `https://github.com/a/b` are one mirror.
///
/// Every component is then checked, not cleaned. A URL carrying `..`, a leading dash, a
/// percent escape or anything else outside a conservative ASCII set is refused: the dep
/// records the error and is never fetched. Cleaning would be worse than refusing, since
/// two different URLs could clean down to the same directory.
pub fn mirror_path(mirrors: &Path, url: &str) -> Result<PathBuf, String> {
    let parsed = Url::parse(url).map_err(|error| format!("url {url:?} does not parse: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "url {url:?} is {}, and only http and https are ever fetched",
            parsed.scheme()
        ));
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if !is_safe_segment(&host) {
        return Err(format!("url {url:?} has no host a directory can be named after"));
    }
    // The port is part of the identity — two servers on one host are two upstreams —
    // and it is a number, so it cannot carry anything into the path.
    let authority = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };

    let path = parsed.path().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if path.is_empty() {
        return Err(format!("url {url:?} names a host but no repository"));
    }

    let mut full = mirrors.join("up").join(authority);
    for segment in path.split('/') {
        if !is_safe_segment(segment) {
            return Err(format!("url {url:?} has a path segment {segment:?} that cannot be a directory"));
        }
        full.push(segment);
    }
    full.as_mut_os_string().push(".git");
    Ok(full)
}

/// A directory component that cannot be anything but itself: no separator, no traversal,
/// no leading dash for a git subcommand to read as a flag, and nothing outside the ASCII
/// set real clone URLs use.
fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.starts_with('-')
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'~'))
}

/// The same property [`crate::config`] enforces on repo names, applied to dep names.
/// Names are display only — the mirror is keyed by URL — but a name reaches a page and
/// a JSON key, so it stays a name.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// An abbreviated or full commit id. Anything else — a tag, a branch, a flag — is
/// refused, so a pin can be handed to `git cat-file` without an escape hatch.
fn is_commit_ish(pin: &str) -> bool {
    (4..=40).contains(&pin.len()) && pin.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// A branch name git would accept, minus the parts of the grammar that would let a
/// manifest reach past the branch it named.
fn is_safe_ref(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("@{")
        && !name.bytes().any(|byte| {
            byte.is_ascii_whitespace()
                || byte.is_ascii_control()
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\' | b'\0')
        })
}

/// Errors from a parser are paragraphs. A stanza wants a sentence.
fn first_line(message: &str) -> String {
    message.lines().find(|line| !line.trim().is_empty()).unwrap_or(message).trim().to_owned()
}

// ---- what the brain reports ------------------------------------------------------

/// One dependency, as declared and as found on disk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DepState {
    pub name: String,
    pub url: String,
    pub layer: Option<String>,
    /// `"pin"` or `"track"`, or null for a dep that declared neither or both.
    pub mode: Option<&'static str>,
    /// The revision the manifest asked for.
    pub want: Option<String>,
    /// The full commit id the mirror actually answers with, or null when the mirror
    /// does not have it yet.
    pub have: Option<String>,
    /// The mirror answers the declared revision and the last fetch did not fail.
    pub fresh: bool,
    /// When this mirror last fetched successfully, RFC3339.
    pub last_fetched: Option<String>,
    /// Why this dep is not usable: a bad declaration, or the last fetch's own words.
    pub error: Option<String>,
}

/// A repo's upstream column.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Stack {
    /// The manifest would not parse. `deps` is empty and nothing was fetched.
    pub error: Option<String>,
    pub deps: Vec<DepState>,
}

// ---- the mirror pool -------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct MirrorState {
    last_attempt: Option<Instant>,
    last_success: Option<String>,
    last_error: Option<String>,
}

/// The upstream mirror pool. Shared through Topcoat's app context, like [`crate::mirror::Mirrors`].
#[derive(Clone)]
pub struct Upstreams {
    config: Arc<Config>,
    /// Fetch state per mirror directory, which is also the per-URL dedup key.
    state: Arc<Mutex<HashMap<PathBuf, MirrorState>>>,
    /// One lock per mirror, so two repos declaring the same dep never fetch it twice.
    locks: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
}

impl std::fmt::Debug for Upstreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Upstreams")
    }
}

impl Upstreams {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(HashMap::new())),
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The manifest a repo declares at its default-branch tip. `None` when there is no
    /// `.nashcode/stack.toml`: the repo has no stack and pays one `git show` to say so.
    pub async fn manifest(&self, repo: &Repo) -> Option<Manifest> {
        let branch = repo.default_branch().await.ok()?;
        let raw = repo.show_file(&branch, MANIFEST_PATH).await.ok()??;
        let raw = String::from_utf8_lossy(&raw).into_owned();
        Some(parse(&self.config.mirrors, &raw))
    }

    /// The repo's stack as it stands on disk, with anything due started in the
    /// background. Never blocks on the wire: the caller renders what is there, and the
    /// next request sees whatever the fetch learned.
    pub async fn stack(&self, repo: &Repo) -> Option<Stack> {
        let manifest = self.manifest(repo).await?;
        for dep in &manifest.deps {
            self.spawn_refresh(dep).await;
        }
        Some(self.report(&manifest).await)
    }

    /// The same, on the wire and on the caller's own time. `POST /{repo}/stack/sync`
    /// and the background clock use this; a page load must not.
    pub async fn sync(&self, repo: &Repo) -> Option<Stack> {
        let manifest = self.manifest(repo).await?;
        for dep in &manifest.deps {
            self.refresh(dep, true).await;
        }
        Some(self.report(&manifest).await)
    }

    /// The background clock. A `track` dep goes stale on its own — no push and no page
    /// load of ours would ever notice a branch moving in a repo we do not own — so one
    /// task walks every configured repo's manifest on [`TRACK_INTERVAL`].
    ///
    /// The first tick fires immediately, which is what warms the pins on a cold box. A
    /// repo whose own mirror has not cloned yet simply has no manifest to read; the
    /// stanza's opportunistic refresh picks it up long before the next tick.
    pub async fn watch(self) {
        let mut ticker = tokio::time::interval(TRACK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            for name in &self.config.repos {
                self.sync(&self.own_mirror(name)).await;
            }
        }
    }

    /// A read handle on one of our own repo mirrors.
    fn own_mirror(&self, repo: &str) -> Repo {
        Repo::mirror(self.config.mirror_path(repo), Auth::new(self.config.git_token.clone()))
    }

    /// Read every dep's state off disk, without touching the network.
    async fn report(&self, manifest: &Manifest) -> Stack {
        let mut deps = Vec::with_capacity(manifest.deps.len());
        for dep in &manifest.deps {
            deps.push(self.dep_state(dep).await);
        }
        Stack { error: manifest.error.clone(), deps }
    }

    async fn dep_state(&self, dep: &Dep) -> DepState {
        let mut state = DepState {
            name: dep.name.clone(),
            url: dep.url.clone(),
            layer: dep.layer.clone(),
            mode: dep.mode.as_ref().map(Mode::kind),
            want: dep.mode.as_ref().map(|mode| mode.want().to_owned()),
            have: None,
            fresh: false,
            last_fetched: None,
            error: dep.error.clone(),
        };
        let (Some(path), Some(mode)) = (&dep.path, &dep.mode) else { return state };

        state.have = resolve(path, mode).await;
        let mirror = self.state.lock().await.get(path).cloned().unwrap_or_default();
        state.last_fetched = mirror.last_success;
        if state.error.is_none() {
            state.error = mirror.last_error;
        }
        state.fresh = state.have.is_some() && state.error.is_none();
        state
    }

    /// Start a refresh unless one is already running for this mirror. The in-flight
    /// guard is the mirror's own lock, taken without waiting and held for the length of
    /// the fetch, exactly as [`crate::mirror::Mirrors`] does it.
    async fn spawn_refresh(&self, dep: &Dep) {
        let Some(path) = dep.path.clone() else { return };
        if !self.due(dep).await {
            return;
        }
        let Ok(guard) = self.lock_for(&path).await.try_lock_owned() else {
            return;
        };
        let upstreams = self.clone();
        let dep = dep.clone();
        tokio::spawn(async move {
            let _guard = guard;
            upstreams.fetch_dep(&dep).await;
        });
    }

    /// Refresh one dep and wait. `force` skips the interval, which is what "sync now"
    /// means; it never skips the pin rule, because a pin already on disk is as fresh as
    /// a pin can ever be.
    async fn refresh(&self, dep: &Dep, force: bool) {
        let Some(path) = dep.path.clone() else { return };
        if !force && !self.due(dep).await {
            return;
        }
        let lock = self.lock_for(&path).await;
        let _guard = lock.lock().await;
        // Checked again under the lock: a fetch we queued behind may have satisfied the
        // pin, and then this one has nothing to ask for.
        if !force && !self.due(dep).await {
            return;
        }
        if let Some(Mode::Pin(pin)) = &dep.mode
            && has_commit(&path, pin).await
        {
            return;
        }
        self.fetch_dep(dep).await;
    }

    /// Is this dep worth a request right now?
    ///
    /// A pin whose commit is on disk never is: upstream cannot change what that commit
    /// says. Everything else waits out [`TRACK_INTERVAL`] since the last attempt — a
    /// tracked branch because half an hour is fresh enough, a pin still missing because
    /// hammering a server that does not have the commit will not conjure it.
    async fn due(&self, dep: &Dep) -> bool {
        if dep.error.is_some() {
            return false;
        }
        let (Some(path), Some(mode)) = (&dep.path, &dep.mode) else { return false };
        if let Mode::Pin(pin) = mode
            && has_commit(path, pin).await
        {
            return false;
        }
        let state = self.state.lock().await.get(path).cloned().unwrap_or_default();
        state.last_attempt.is_none_or(|at| at.elapsed() >= TRACK_INTERVAL)
    }

    async fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks.entry(path.to_path_buf()).or_default().clone()
    }

    /// The fetch itself. The caller holds the mirror's lock; this never takes it.
    ///
    /// Every failure becomes state, not an error: the mirror on disk keeps answering and
    /// the stanza says why it may be behind.
    async fn fetch_dep(&self, dep: &Dep) {
        let Some(path) = &dep.path else { return };
        if dep.error.is_some() {
            return;
        }
        let outcome = fetch(&dep.url, path).await;

        let now = crate::db::now();
        let mut states = self.state.lock().await;
        let state = states.entry(path.clone()).or_default();
        state.last_attempt = Some(Instant::now());
        match outcome {
            Ok(()) => {
                state.last_error = None;
                state.last_success = Some(now);
            }
            Err(message) => {
                tracing::warn!(dep = dep.name, url = dep.url, %message, "upstream fetch failed");
                state.last_error = Some(message);
            }
        }
    }
}

/// A read handle on an upstream mirror.
///
/// Deliberately unauthenticated. `GIT_TOKEN` is the dgit push token; an upstream is
/// somebody else's server, and the token has no business being offered to it.
fn upstream_repo(path: &Path) -> Repo {
    Repo::mirror(path, Auth::default())
}

/// Is this commit already in the mirror? A pin that answers yes is finished forever.
async fn has_commit(path: &Path, pin: &str) -> bool {
    if !path.exists() {
        return false;
    }
    upstream_repo(path)
        .try_run(&["cat-file", "-e", &format!("{pin}^{{commit}}")])
        .await
        .is_ok_and(|out| out.ok())
}

/// The full commit id the mirror answers the declared revision with, or `None` when it
/// does not have it yet.
async fn resolve(path: &Path, mode: &Mode) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let rev = match mode {
        // `^{commit}` both resolves an abbreviated pin and refuses anything that is on
        // disk but is not a commit.
        Mode::Pin(pin) => format!("{pin}^{{commit}}"),
        // The mirror's own branch, not `origin/`: `--mirror` keeps upstream's heads as
        // its heads.
        Mode::Track(branch) => format!("refs/heads/{branch}^{{commit}}"),
    };
    // `--verify` rather than [`Repo::rev_parse`]: plain `git rev-parse` echoes
    // `--end-of-options` back on its own line before the id, and this answer is a
    // commit somebody reads. `--verify` also turns "no such revision" into a nonzero
    // exit instead of a string that looks like an answer.
    let resolved = upstream_repo(path)
        .run(&["rev-parse", "--verify", "--end-of-options", &rev])
        .await
        .ok()?;
    let resolved = resolved.trim().to_owned();
    (!resolved.is_empty()).then_some(resolved)
}

/// Clone the mirror, or bring it up to date. The fetch discipline is
/// [`crate::mirror`]'s, for the same reasons: no repacking on somebody else's clock, a
/// stalled transfer gives up instead of hanging, and one ref transaction so a reader
/// never sees half a fetch.
async fn fetch(url: &str, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return clone_mirror(url, path, &Auth::default()).await.map_err(|error| error.to_string());
    }
    upstream_repo(path)
        .run_remote(
            url,
            &[
                "-c",
                "gc.auto=0",
                "-c",
                "http.lowSpeedLimit=1000",
                "-c",
                "http.lowSpeedTime=30",
                "fetch",
                "--atomic",
                "--prune",
                "--prune-tags",
                "--no-write-fetch-head",
                url,
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ],
        )
        .await
        .map_err(|error| error.to_string())
        .and_then(|out| if out.ok() { Ok(()) } else { Err(out.stderr.trim().to_owned()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIRRORS: &str = "/srv/mirrors";

    #[test]
    fn a_hostile_url_never_becomes_a_path_outside_the_up_directory() {
        // `git clone` would happily take any of these.
        let hostile = [
            "https://github.com/../../../../srv/git/private",
            "https://github.com/%2e%2e/%2e%2e/etc",
            "https://github.com/-c/payload",
            "https://../etc/passwd",
            "https://github.com/a/b%00c",
            "https://github.com/a b/c",
            "https://github.com/",
            "https://github.com",
            "https://-evil.example/a/b",
            "file:///etc/passwd",
            "ssh://git@github.com/a/b",
            "git://github.com/a/b",
            "/srv/git/private",
            "",
        ];
        for url in hostile {
            match mirror_path(Path::new(MIRRORS), url) {
                Err(_) => {}
                Ok(path) => {
                    let shown = path.to_string_lossy().into_owned();
                    assert!(
                        path.starts_with("/srv/mirrors/up") && !shown.contains(".."),
                        "{url} escaped to {shown}"
                    );
                }
            }
        }
    }

    #[test]
    fn one_url_is_one_mirror_however_it_is_spelled() {
        let of = |url: &str| mirror_path(Path::new(MIRRORS), url).expect(url);
        let want = PathBuf::from("/srv/mirrors/up/github.com/littledivy/dgit.git");
        assert_eq!(of("https://github.com/littledivy/dgit"), want);
        assert_eq!(of("https://GitHub.com/littledivy/dgit.git"), want);
        assert_eq!(of("https://github.com/littledivy/dgit.git/"), want);
        assert_eq!(of("http://github.com/littledivy/dgit"), want);

        // A port is part of the identity: two servers on one host are two upstreams.
        assert_eq!(
            of("http://127.0.0.1:8199/srv.git"),
            PathBuf::from("/srv/mirrors/up/127.0.0.1:8199/srv.git")
        );
    }

    #[test]
    fn a_good_manifest_parses_into_deps_that_can_be_fetched() {
        let manifest = parse(
            Path::new(MIRRORS),
            r#"
[[dep]]
name  = "dgit"
url   = "https://github.com/littledivy/dgit"
pin   = "1a2b3c4"
layer = "server"

[[dep]]
name  = "celld"
url   = "https://github.com/denoland/celld"
track = "main"
layer = "runtime"
"#,
        );
        assert_eq!(manifest.error, None);
        assert_eq!(manifest.deps.len(), 2);

        let dgit = &manifest.deps[0];
        assert_eq!(dgit.error, None);
        assert_eq!(dgit.mode, Some(Mode::Pin("1a2b3c4".to_owned())));
        assert_eq!(dgit.layer.as_deref(), Some("server"));
        assert_eq!(dgit.path, Some(PathBuf::from("/srv/mirrors/up/github.com/littledivy/dgit.git")));

        let celld = &manifest.deps[1];
        assert_eq!(celld.mode, Some(Mode::Track("main".to_owned())));
        assert_eq!(celld.error, None);
    }

    #[test]
    fn every_refused_declaration_says_why_and_carries_no_path() {
        let cases = [
            (r#"name = "a"
url = "https://h/a"
pin = "1a2b3c4"
track = "main""#, "both"),
            (r#"name = "a"
url = "https://h/a""#, "neither"),
            (r#"name = "../escape"
url = "https://h/a"
pin = "1a2b3c4""#, "plain name"),
            (r#"name = "a"
url = "ssh://git@h/a"
pin = "1a2b3c4""#, "only http"),
            (r#"name = "a"
url = "https://h/a"
pin = "main""#, "commit id"),
            (r#"name = "a"
url = "https://h/a"
track = "--upload-pack=touch""#, "branch name"),
        ];
        for (body, expected) in cases {
            let manifest = parse(Path::new(MIRRORS), &format!("[[dep]]\n{body}\n"));
            let dep = &manifest.deps[0];
            let error = dep.error.as_deref().unwrap_or_default();
            assert!(error.contains(expected), "{body}\ngave {error:?}, wanted {expected:?}");
            assert_eq!(dep.path, None, "{body} kept a mirror path");
        }
    }

    #[test]
    fn the_second_dep_of_a_name_is_the_one_refused() {
        let manifest = parse(
            Path::new(MIRRORS),
            r#"
[[dep]]
name = "dgit"
url  = "https://h/a"
pin  = "1a2b3c4"

[[dep]]
name = "dgit"
url  = "https://h/b"
pin  = "deadbee"
"#,
        );
        assert_eq!(manifest.deps[0].error, None);
        assert!(manifest.deps[1].error.as_deref().unwrap_or_default().contains("twice"));
    }

    #[test]
    fn a_manifest_that_is_not_toml_is_one_error_and_no_deps() {
        let manifest = parse(Path::new(MIRRORS), "[[dep]\nname = ");
        assert!(manifest.error.is_some());
        assert!(manifest.deps.is_empty());
    }
}
