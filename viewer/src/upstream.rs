//! The upstream column: `.nashcode/stack.toml`, and the read-only mirrors it names.
//!
//! A repo declares the code it is built on, and the viewer keeps a `git clone --mirror`
//! copy of each dependency beside its own mirrors. Three rules shape the module.
//!
//! **Nothing is fetched unless the repo said something we understand.** A URL that is
//! not `https` — or plain `http` to anywhere but this box — a `[[dep]]` that pins and
//! tracks at once, a name that could be a path: each becomes an error recorded against
//! that dep, and no request is ever made for it. A manifest that will not parse at all
//! leaves every other feature of the repo alone: the error travels in the brain stanza
//! and nothing else changes. A repo with no manifest has no stack, and pays one lookup
//! for the privilege.
//!
//! **One mirror per clone URL, for the whole box.** The mirror directory is derived from
//! the URL — `<mirrors>/up/<host>/<path>.git` — so two repos declaring the same
//! dependency share one copy. Every path component is checked against a conservative
//! ASCII set rather than rewritten: a URL that cannot be spelled as a directory is
//! refused, because rewriting it would let two different URLs collapse onto one mirror,
//! and a shared mirror is a shared answer. See [`locate`] for the collisions that rule
//! exists to prevent.
//!
//! **Never block, never fail.** [`Upstreams::stack`] reports what is on disk and starts
//! whatever is due behind the caller's back, the way [`crate::mirror`] does; a fetch that
//! cannot reach the server leaves the mirror in place and records why. Only
//! [`Upstreams::sync`] waits, because its caller asked for the wire — and even it keeps
//! a budget, since one route anyone can call in a loop is otherwise an amplifier aimed
//! at somebody else's server.
//!
//! Upstream mirrors are read-only everywhere. Nothing here pushes, and they are not in
//! `config.repos`, so no repo route, listing, or write path can name one. They are also
//! fetched anonymously: `GIT_TOKEN` is the dgit push token and has no business being
//! sent to github.com.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;
use url::Url;

use crate::config::Config;
use crate::git::{Auth, Repo, clone_mirror};

/// Where the manifest lives, in the repo, at the default-branch tip.
pub const MANIFEST_PATH: &str = ".nashcode/stack.toml";

/// The one dep name the routes cannot spell: `POST /{repo}/stack/sync` is a static
/// route, so `/{repo}/stack/sync` can never be a dep's page. A manifest using the name
/// would get a link on the column page that no page answers. Refusing the name where
/// the manifest is read says so at the source instead.
pub const RESERVED_DEP_NAME: &str = "sync";

/// How often a `track` dep goes back to the wire, and how long a dep whose commit is
/// still missing waits before trying again. Upstreams are somebody else's repo: half an
/// hour is fresh enough to notice drift and slow enough to be a good neighbour.
pub const TRACK_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// How long `POST /{repo}/stack/sync` waits before it will go to the wire again for the
/// same mirror.
///
/// Sync exists because half an hour is sometimes too long. Without a budget it is also
/// an amplifier: one endpoint anyone on the tailnet can call in a loop, pointed at a
/// third party's server. A minute keeps "I need it now" honest and keeps us a good
/// neighbour.
pub const SYNC_DEBOUNCE: Duration = Duration::from_secs(60);

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
    /// Where this URL's mirror lives and how git reaches it. `None` when the URL was
    /// refused.
    pub location: Option<Location>,
    pub error: Option<String>,
}

/// A resolved upstream: the mirror directory, and the URL git is handed to fill it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// `<mirrors>/up/<host>/<path>.git` — also the per-URL dedup key.
    pub path: PathBuf,
    /// The clone URL as the parser normalized it. The manifest's own string never
    /// reaches git's argv.
    pub remote: String,
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

        let mut dep = Dep { name, url, layer, mode: None, location: None, error: None };
        dep.error = validate(&mut dep, mirrors, raw.pin, raw.track, duplicate);
        if dep.error.is_some() {
            // Belt and braces: nothing downstream may act on a dep we refused.
            dep.mode = None;
            dep.location = None;
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
    if dep.name == RESERVED_DEP_NAME {
        return Some(format!(
            "name {RESERVED_DEP_NAME:?} is reserved: /{{repo}}/stack/{RESERVED_DEP_NAME} \
             is the sync route, so a dep by that name would have no page"
        ));
    }
    if duplicate {
        return Some(format!("name {:?} is declared twice", dep.name));
    }

    dep.mode = match (pin.as_deref().map(str::trim), track.as_deref().map(str::trim)) {
        (Some(pin), None) if !pin.is_empty() => {
            if !is_commit_id(pin) {
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

    match locate(mirrors, &dep.url) {
        Ok(location) => dep.location = Some(location),
        Err(why) => return Some(why),
    }
    None
}

/// Resolve a clone URL into its mirror directory under `<mirrors>/up/`.
///
/// The URL is the key: the host and its path become the directory, so two repos naming
/// the same dependency share one mirror and the box never holds two copies of celld.
/// Normalization is the small, documented set — the scheme and host are lowered by the
/// URL parser, one trailing `/` and then a trailing `.git` come off — so
/// `https://GitHub.com/a/b.git/` and `https://github.com/a/b` are one mirror.
///
/// Everything else is checked, not cleaned. A URL carrying `..`, a leading dash, a
/// percent escape or anything else outside a conservative ASCII set is refused: the dep
/// records the error and is never fetched. Cleaning would be worse than refusing, since
/// two different URLs could clean down to the same directory, and a shared mirror is a
/// shared answer.
///
/// Three refusals are worth naming, because each one is a collision the earlier, looser
/// rule allowed:
///
/// - A path whose last segment is empty after the suffixes come off — `.../a/.git` —
///   would land on `.../a`, which belongs to a different URL.
/// - A path segment that itself ends in `.git` — `.../a/b.git/c` — would put a plain
///   directory exactly where `.../a/b`'s mirror goes, and nothing could ever clone there
///   again.
/// - Credentials in the URL. Mirrors are shared between repos and fetched anonymously,
///   so a password in one repo's manifest has no business keying a mirror everyone uses.
///
/// Plain `http` is accepted only for loopback, which is what the tests and a local dgit
/// use. Everything off this box must be `https`: without that rule an `http` spelling of
/// a URL somebody else declared over `https` would silently downgrade the transport of
/// the mirror they share.
pub fn locate(mirrors: &Path, url: &str) -> Result<Location, String> {
    let mut parsed =
        Url::parse(url).map_err(|error| format!("url {url:?} does not parse: {error}"))?;
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback(&parsed) => {}
        "http" => {
            return Err(format!(
                "url {url:?} is plain http to a host that is not this one; an upstream \
                 off this box must be https"
            ));
        }
        other => {
            return Err(format!(
                "url {url:?} is {other}, and only http and https are ever fetched"
            ));
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "url {url:?} carries credentials, and an upstream mirror is shared and \
             fetched anonymously"
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

    // One trailing slash, then the `.git` suffix, in that order: `.../b.git/` is both.
    // Exactly one slash at each end, never a run of them — `.../a/.git` has to keep the
    // empty last segment that gets it refused below, and `//a` has to keep the empty
    // first one, rather than either quietly becoming `.../a`.
    let path = parsed.path();
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.strip_prefix('/').unwrap_or(path);
    if path.is_empty() {
        return Err(format!("url {url:?} names a host but no repository"));
    }

    let mut full = mirrors.join("up").join(authority);
    for segment in path.split('/') {
        if !is_safe_segment(segment) {
            return Err(format!(
                "url {url:?} has a path segment {segment:?} that cannot be a directory"
            ));
        }
        if segment.ends_with(".git") {
            return Err(format!(
                "url {url:?} has a path segment {segment:?} that would sit where \
                 another mirror goes"
            ));
        }
        full.push(segment);
    }
    full.as_mut_os_string().push(".git");

    // A clone URL is a path, not a query. Dropping both keeps two spellings of one repo
    // from reaching git as two different remotes.
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(Location { path: full, remote: parsed.into() })
}

/// Is this host on the box we are running on? The only place plain `http` is allowed.
fn is_loopback(parsed: &Url) -> bool {
    match parsed.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
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
///
/// Seven digits is the floor because four is ambiguous in any repo worth pinning, and a
/// pin that resolves to the wrong commit is worse than one that will not parse.
///
/// `?rev=` on the browse routes is held to this same grammar, before it reaches an
/// argv: one rule for "this string names a commit", written once.
pub fn is_commit_id(pin: &str) -> bool {
    (7..=40).contains(&pin.len()) && pin.bytes().all(|byte| byte.is_ascii_hexdigit())
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

// ---- browsing the column ---------------------------------------------------------

/// One dep, opened for reading: where its mirror is and which commit it points at.
///
/// Nothing here goes to the wire. A page built from a `Browse` shows what is on disk,
/// or says the commit is not there yet — the fetch that would change that is the
/// clock's job and the sync route's, never a reader's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browse {
    pub name: String,
    pub url: String,
    pub layer: Option<String>,
    pub mode: Mode,
    /// The mirror directory. Also the key a `.gitmodules` URL is matched against, since
    /// it is what [`locate`] turns every spelling of one upstream into.
    pub path: PathBuf,
    /// The commit the declaration resolves to, `None` while the mirror lacks it.
    pub commit: Option<String>,
}

impl Browse {
    /// Resolve a validated dep against its mirror. `None` for a dep that was refused:
    /// it has no mode and no mirror, so there is nothing to open.
    async fn open(dep: Dep) -> Option<Self> {
        if dep.error.is_some() {
            return None;
        }
        let (location, mode) = (dep.location?, dep.mode?);
        let commit = resolve(&location.path, &mode).await;
        Some(Self {
            name: dep.name,
            url: dep.url,
            layer: dep.layer,
            mode,
            path: location.path,
            commit,
        })
    }

    /// A read handle on the mirror — unauthenticated, like every other read of one.
    pub fn mirror(&self) -> Repo {
        upstream_repo(&self.path)
    }
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
    /// [`SYNC_DEBOUNCE`] in milliseconds, shared by every clone of the handle. A knob
    /// rather than a constant only so a test can stand on both sides of it.
    sync_debounce: Arc<AtomicU64>,
    /// [`TRACK_INTERVAL`] in milliseconds, the same way. A test that has to prove a page
    /// did *not* fetch needs the interval out of the way first, or the assertion passes
    /// for the wrong reason.
    track_interval: Arc<AtomicU64>,
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
            sync_debounce: Arc::new(AtomicU64::new(SYNC_DEBOUNCE.as_millis() as u64)),
            track_interval: Arc::new(AtomicU64::new(TRACK_INTERVAL.as_millis() as u64)),
        }
    }

    /// How long `sync` waits before going back to the wire for one mirror.
    pub fn sync_debounce(&self) -> Duration {
        Duration::from_millis(self.sync_debounce.load(Ordering::Relaxed))
    }

    /// Move the sync rate limit, on every clone of this handle at once.
    pub fn set_sync_debounce(&self, window: Duration) {
        self.sync_debounce.store(window.as_millis() as u64, Ordering::Relaxed);
    }

    /// How long a dep waits after an attempt before it is worth another one.
    pub fn track_interval(&self) -> Duration {
        Duration::from_millis(self.track_interval.load(Ordering::Relaxed))
    }

    /// Move the refresh interval, on every clone of this handle at once. Zero means
    /// every read that is allowed to refresh does: the setting a test wants when the
    /// claim under test is "this page did not go to the wire".
    ///
    /// [`watch`](Self::watch)'s own clock keeps the constant — a ticker cannot have a
    /// zero period — so this moves the "is it due" question only.
    pub fn set_track_interval(&self, window: Duration) {
        self.track_interval.store(window.as_millis() as u64, Ordering::Relaxed);
    }

    /// The manifest a repo declares at its default-branch tip.
    ///
    /// `None` means the repo has no `.nashcode/stack.toml` — no stack, no cost beyond
    /// the lookup that said so. A git error is *not* that: it comes back as a manifest
    /// whose `error` says what git said, because a mirror having a bad minute must not
    /// look like a repo that never declared anything.
    pub async fn manifest(&self, repo: &Repo) -> Option<Manifest> {
        let failed = |error: crate::git::GitError| {
            Some(Manifest {
                deps: Vec::new(),
                error: Some(format!("cannot read {MANIFEST_PATH}: {error}")),
            })
        };
        let branch = match repo.default_branch().await {
            Ok(branch) => branch,
            Err(error) => return failed(error),
        };
        match repo.show_file(&branch, MANIFEST_PATH).await {
            Ok(None) => None,
            Ok(Some(raw)) => {
                Some(parse(&self.config.mirrors, &String::from_utf8_lossy(&raw)))
            }
            Err(error) => failed(error),
        }
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
    /// and the background clock use this; a page load must not. It skips the half-hour
    /// interval but keeps [`SYNC_DEBOUNCE`]: inside that window the answer is what is
    /// already on disk.
    pub async fn sync(&self, repo: &Repo) -> Option<Stack> {
        let manifest = self.manifest(repo).await?;
        for dep in &manifest.deps {
            self.refresh(dep, true).await;
        }
        Some(self.report(&manifest).await)
    }

    /// One named dep of a repo, opened for reading.
    ///
    /// `None` when the repo declares no dep by that name, or declared one we refused:
    /// neither has a mirror, so neither has a page. The name is the manifest's, and it
    /// is looked up in *this* repo's manifest only — a dep is browsable under the repo
    /// that declared it and nowhere else.
    pub async fn browse(&self, repo: &Repo, name: &str) -> Option<Browse> {
        let manifest = self.manifest(repo).await?;
        let dep = manifest.deps.into_iter().find(|dep| dep.name == name)?;
        Browse::open(dep).await
    }

    /// Every dep of a repo that has a mirror, in manifest order. Nothing fetches: this
    /// is what is on disk, which is what a page renders and what a `.gitmodules` URL is
    /// matched against.
    pub async fn column(&self, repo: &Repo) -> Vec<Browse> {
        let Some(manifest) = self.manifest(repo).await else { return Vec::new() };
        let mut column = Vec::with_capacity(manifest.deps.len());
        for dep in manifest.deps {
            if let Some(browse) = Browse::open(dep).await {
                column.push(browse);
            }
        }
        column
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
            for name in self.config.repos.names() {
                self.sync(&self.own_mirror(&name)).await;
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
        let (Some(location), Some(mode)) = (&dep.location, &dep.mode) else { return state };

        state.have = resolve(&location.path, mode).await;
        let mirror = self.state.lock().await.get(&location.path).cloned().unwrap_or_default();
        state.last_fetched = mirror.last_success.clone();

        let pinned = matches!(mode, Mode::Pin(_));
        if state.error.is_none() {
            match (pinned, &state.have) {
                // A pin that is on disk is finished. Another dep tracking a branch in
                // the same mirror can fail all it likes; this one already has its
                // commit, and "then never again" would mean nothing otherwise.
                (true, Some(_)) => {}
                // A pin the mirror has fetched and still cannot answer is not stale, it
                // is wrong: upstream does not publish that commit on any branch or tag.
                (true, None) if mirror.last_success.is_some() && mirror.last_error.is_none() => {
                    state.error = Some(format!(
                        "pin {} is not in the upstream's branches or tags",
                        mode.want()
                    ));
                }
                _ => state.error = mirror.last_error,
            }
        }
        state.fresh = state.have.is_some() && state.error.is_none();
        state
    }

    /// Start a refresh unless one is already running for this mirror. The in-flight
    /// guard is the mirror's own lock, taken without waiting and held for the length of
    /// the fetch, exactly as [`crate::mirror::Mirrors`] does it.
    async fn spawn_refresh(&self, dep: &Dep) {
        let Some(location) = dep.location.clone() else { return };
        if !self.due(dep).await {
            return;
        }
        let Ok(guard) = self.lock_for(&location.path).await.try_lock_owned() else {
            return;
        };
        let upstreams = self.clone();
        let dep = dep.clone();
        tokio::spawn(async move {
            let _guard = guard;
            upstreams.fetch_dep(&dep).await;
        });
    }

    /// Refresh one dep and wait. `force` is "sync now": it skips the half-hour interval,
    /// but not the pin rule and not the sync budget.
    async fn refresh(&self, dep: &Dep, force: bool) {
        let Some(location) = dep.location.clone() else { return };
        if !force && !self.due(dep).await {
            return;
        }
        let lock = self.lock_for(&location.path).await;
        let _guard = lock.lock().await;

        // Checked again under the lock: a fetch we queued behind may have satisfied the
        // pin, and then this one has nothing to ask for.
        if let Some(Mode::Pin(pin)) = &dep.mode
            && has_commit(&location.path, pin).await
        {
            return;
        }
        if force {
            let state = self.state.lock().await.get(&location.path).cloned().unwrap_or_default();
            if state.last_attempt.is_some_and(|at| at.elapsed() < self.sync_debounce()) {
                return; // Inside the budget. The answer is what is already on disk.
            }
        } else if !self.due(dep).await {
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
        let (Some(location), Some(mode)) = (&dep.location, &dep.mode) else { return false };
        if let Mode::Pin(pin) = mode
            && has_commit(&location.path, pin).await
        {
            return false;
        }
        let state = self.state.lock().await.get(&location.path).cloned().unwrap_or_default();
        state.last_attempt.is_none_or(|at| at.elapsed() >= self.track_interval())
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
        let Some(location) = &dep.location else { return };
        if dep.error.is_some() {
            return;
        }
        // The normalized remote, never `dep.url`: the manifest's own string is raw input
        // and this is a git argument.
        let outcome = fetch(&location.remote, &location.path).await;

        let now = crate::db::now();
        let mut states = self.state.lock().await;
        let state = states.entry(location.path.clone()).or_default();
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

/// Is there a real repository here?
///
/// A bare mirror always has a `HEAD` file, and a plain directory does not. Deciding
/// clone-versus-fetch on the directory alone would mean anything that ever left a
/// directory at a mirror's address — a URL rule looser than today's, an interrupted
/// clone — is fetched into forever, fails forever, and never heals. This way it fails a
/// clone, says so in the stanza, and can be removed by hand.
fn is_repo(path: &Path) -> bool {
    path.join("HEAD").is_file()
}

/// Is this commit already in the mirror? A pin that answers yes is finished forever.
async fn has_commit(path: &Path, pin: &str) -> bool {
    if !is_repo(path) {
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
    if !is_repo(path) {
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
    let resolved = upstream_repo(path).rev_parse(&rev).await.ok()?;
    (!resolved.is_empty()).then_some(resolved)
}

/// Clone the mirror, or bring it up to date. The fetch discipline is
/// [`crate::mirror`]'s, for the same reasons: no repacking on somebody else's clock, a
/// stalled transfer gives up instead of hanging, and one ref transaction so a reader
/// never sees half a fetch.
async fn fetch(url: &str, path: &Path) -> Result<(), String> {
    if !is_repo(path) {
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

    /// The mirror directory a URL resolves to, or `None` if it is refused.
    fn path_of(url: &str) -> Option<PathBuf> {
        locate(Path::new(MIRRORS), url).ok().map(|location| location.path)
    }

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
            let Some(path) = path_of(url) else { continue };
            let shown = path.to_string_lossy().into_owned();
            assert!(
                path.starts_with("/srv/mirrors/up") && !shown.contains(".."),
                "{url} escaped to {shown}"
            );
        }
    }

    #[test]
    fn one_url_is_one_mirror_however_it_is_spelled() {
        let of = |url: &str| path_of(url).expect(url);
        let want = PathBuf::from("/srv/mirrors/up/github.com/littledivy/dgit.git");
        assert_eq!(of("https://github.com/littledivy/dgit"), want);
        assert_eq!(of("https://GitHub.com/littledivy/dgit.git"), want);
        assert_eq!(of("https://github.com/littledivy/dgit.git/"), want);
        assert_eq!(of("https://github.com/littledivy/dgit/"), want);
        assert_eq!(of("https://github.com/littledivy/dgit?ref=x#frag"), want);

        // A port is part of the identity: two servers on one host are two upstreams.
        assert_eq!(
            of("http://127.0.0.1:8199/srv.git"),
            PathBuf::from("/srv/mirrors/up/127.0.0.1:8199/srv.git")
        );
    }

    #[test]
    fn git_is_handed_the_normalized_url_not_the_manifests_own_string() {
        let location = locate(Path::new(MIRRORS), "https://GitHub.com/a/b.git?ref=x#frag")
            .expect("locates");
        assert_eq!(location.remote, "https://github.com/a/b.git");
    }

    #[test]
    fn no_spelling_of_one_repo_can_land_on_another_repos_mirror() {
        // Each of these used to collapse onto a directory that belongs to the URL
        // beside it. The last one is the worst: a plain directory where a mirror goes
        // fails every clone after it, forever.
        assert_ne!(path_of("https://github.com/a/.git"), path_of("https://github.com/a"));
        assert_eq!(path_of("https://github.com/a/.git"), None);

        assert_ne!(path_of("https://github.com/a/b/.."), path_of("https://github.com/a/b"));

        assert_eq!(path_of("https://github.com/a/b.git/c"), None);
        assert_eq!(path_of("https://github.com//a"), None);
        assert_eq!(path_of("https://github.com/a//b"), None);
        assert_eq!(
            path_of("https://github.com/a/b"),
            Some(PathBuf::from("/srv/mirrors/up/github.com/a/b.git"))
        );
    }

    #[test]
    fn plain_http_is_for_this_box_only_and_credentials_are_never_a_key() {
        for local in ["http://127.0.0.1:9/a/b", "http://localhost/a/b", "http://127.9.9.9/a/b"] {
            assert!(path_of(local).is_some(), "{local} was refused");
        }
        // Off the box, `http` would silently downgrade the transport of a mirror that
        // another repo declared over `https`. They share one directory.
        for remote in ["http://github.com/a/b", "http://10.1.2.3/a/b", "http://[2001:db8::1]/a/b"]
        {
            let why = locate(Path::new(MIRRORS), remote).expect_err(remote);
            assert!(why.contains("must be https"), "{remote} gave {why:?}");
        }
        let why = locate(Path::new(MIRRORS), "https://user:pass@github.com/a/b")
            .expect_err("credentials");
        assert!(why.contains("credentials"), "{why:?}");
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
        assert_eq!(
            dgit.location.as_ref().map(|location| location.path.clone()),
            Some(PathBuf::from("/srv/mirrors/up/github.com/littledivy/dgit.git"))
        );

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
            (r#"name = "a"
url = "https://h/a"
pin = "1a2b3c""#, "commit id"),
            (r#"name = "a"
url = "http://github.com/a/b"
track = "main""#, "must be https"),
            (r#"name = "a"
url = "https://user:pass@h/a"
track = "main""#, "credentials"),
        ];
        for (body, expected) in cases {
            let manifest = parse(Path::new(MIRRORS), &format!("[[dep]]\n{body}\n"));
            let dep = &manifest.deps[0];
            let error = dep.error.as_deref().unwrap_or_default();
            assert!(error.contains(expected), "{body}\ngave {error:?}, wanted {expected:?}");
            assert_eq!(dep.location, None, "{body} kept a mirror path");
        }
    }

    #[test]
    fn the_one_name_a_route_already_owns_is_refused() {
        // `POST /{repo}/stack/sync` is a static route, so a dep called `sync` would get
        // a link on the column page that lands on no page at all. The name is refused
        // where the manifest is read rather than left to surprise somebody.
        let manifest = parse(
            Path::new(MIRRORS),
            "[[dep]]\nname = \"sync\"\nurl = \"https://h/a\"\ntrack = \"main\"\n",
        );
        let dep = &manifest.deps[0];
        assert!(dep.error.as_deref().unwrap_or_default().contains("reserved"), "{dep:?}");
        assert_eq!(dep.location, None);
        assert_eq!(dep.mode, None);
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
