//! Configuration. Environment only — nothing about a deployment lives in source.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Everything the viewer needs to know about the world around it.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the dgit server. Repo `foo` lives at `<dgit_url>/foo.git`.
    pub dgit_url: String,
    /// Push token. Sent as basic auth `x:<token>`. Empty means anonymous.
    pub git_token: String,
    /// The repos to mirror. Discovered, not declared — see [`Repos`].
    pub repos: Repos,
    /// Where `git clone --mirror` copies live.
    pub mirrors: PathBuf,
    /// Listen address.
    pub bind: String,
    /// SQLite file.
    pub db_path: PathBuf,
    /// Directory holding CI log files.
    pub ci_logs: PathBuf,
    /// Directory holding raw trace transcripts.
    pub traces: PathBuf,
    /// The pushed copy of the operator's `people.json`, written by `PUT /people`.
    /// `NASHCODE_PEOPLE`, the same variable the CLI reads, so a viewer and a CLI on
    /// one box can be pointed at one file.
    pub people_path: PathBuf,
    /// Event name -> webhook URLs.
    pub webhooks: BTreeMap<String, Vec<String>>,
    /// `ANTHROPIC_API_KEY`. `/brain/ask` answers 404 without it.
    pub anthropic_key: Option<String>,
    /// Claude API base URL. `NASHCODE_ANTHROPIC_URL` overrides it so tests can point at
    /// a stub.
    pub anthropic_url: String,
    /// `NASHCODE_BRAIN_MODEL`.
    pub brain_model: String,
    /// `NASHCODE_BUGS_BUCKET`. `s3://name` for a real bucket, `file:///path` for a
    /// directory. Unset means error tracking is off: `/bugs` and the ingest routes
    /// answer 404.
    pub bugs_bucket: Option<String>,
    /// `S3_ENDPOINT`, for MinIO or Garage. Ignored by a `file://` bucket.
    pub bugs_s3_endpoint: Option<String>,
    /// `NASHCODE_BUGS_INGEST_URL`: the origin SDKs post envelopes to, which is what
    /// goes into a DSN. Not the bind address — the public ingest domain for projects
    /// on public infra, the tailnet URL for tailnet ones.
    pub bugs_ingest_url: String,
    /// `NASHCODE_BUGS_DRAIN` and friends. `None` means the drainer never starts, which
    /// is the shipped default: without a public ingester there is nothing to drain.
    pub bugs_drain: Option<Drain>,
    /// `NASHCODE_PUSHOVER_TOKEN` + `NASHCODE_PUSHOVER_USER`. `None` means no
    /// notification ever leaves the box; everything else works unchanged.
    pub pushover: Option<Pushover>,
    /// `NASHCODE_URL`: where this viewer is, from outside. Every absolute link nashcode
    /// puts in a message hangs off it, because a notification whose link only works on
    /// the box that sent it is a notification nobody can act on.
    pub public_url: String,
    /// `NASHCODE_BUGS_SELF_DSN`: the DSN nashcode reports its own errors to. Unset means
    /// it reports nothing about itself.
    pub bugs_self_dsn: Option<String>,
}

/// The set of repos the viewer serves.
///
/// Shared and mutable, because the set is discovered rather than declared: every
/// mirror poll reads dgit's index page and inserts the names it has not seen, so a
/// `git push` to a new name is visible within one cycle with no restart.
/// `NASHCODE_REPOS` only seeds it, and nothing ever removes a name — a repo that
/// drops off dgit's index still has a mirror on disk and pages that render from it.
///
/// The lock is `std::sync::RwLock` and every method returns owned data, so no guard
/// can survive into an `.await`. Reads are the hot path: [`Config::knows_repo`] takes
/// one per request.
#[derive(Debug, Clone, Default)]
pub struct Repos(Arc<RwLock<BTreeSet<String>>>);

impl Repos {
    /// Every known name, alphabetically. A snapshot the caller owns.
    pub fn names(&self) -> Vec<String> {
        self.read().iter().cloned().collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.read().contains(name)
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// Learn a name. True when it was new.
    ///
    /// `is_plain_name` is enforced here rather than at each caller, so the set can
    /// never hold a name [`Config::knows_repo`] would refuse: whatever put it there —
    /// `NASHCODE_REPOS`, discovery, a caller not written yet — the invariant holds.
    pub fn insert(&self, name: &str) -> bool {
        if !is_plain_name(name) {
            return false;
        }
        let mut set = self.0.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        set.insert(name.to_owned())
    }

    /// A poisoned lock is recovered rather than propagated: a panic somewhere else
    /// must not turn every repo into a 404.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeSet<String>> {
        self.0.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<S: Into<String>> FromIterator<S> for Repos {
    fn from_iter<I: IntoIterator<Item = S>>(names: I) -> Self {
        let repos = Self::default();
        for name in names {
            repos.insert(&name.into());
        }
        repos
    }
}

/// The Pushover application credentials and where to send them.
///
/// Both halves or neither: a token with no user key produces a 4xx per message and a
/// queue that never drains, which is a worse failure than being off.
#[derive(Clone)]
pub struct Pushover {
    /// `NASHCODE_PUSHOVER_TOKEN`, the application token.
    pub token: String,
    /// `NASHCODE_PUSHOVER_USER`, the user or group key.
    pub user: String,
    /// `NASHCODE_PUSHOVER_URL`, the API origin. Overridable so a test can point the
    /// sender at a listener it started rather than at a real phone.
    pub api_url: String,
}

/// Hand-written for the same reason [`Drain`]'s is: a derived one would print the
/// application token in every panic message that carries a `Config`.
impl std::fmt::Debug for Pushover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pushover")
            .field("token", &"<redacted>")
            .field("user", &"<redacted>")
            .field("api_url", &self.api_url)
            .finish()
    }
}

/// Where the drainer pulls buffered envelopes and log batches from.
///
/// The protocol is `ingester/README.md`; nothing here knows what serves it. That is the
/// hedge the design asks for: an EndpointId dials iroh, an `http://host:port` dials TCP,
/// and the loop above the transport cannot tell which it got.
#[derive(Clone)]
pub struct Drain {
    /// `NASHCODE_BUGS_DRAIN`: an iroh EndpointId, or an `http://host:port` base URL.
    pub target: String,
    /// `NASHCODE_BUGS_DRAIN_TOKEN`: the bearer token every control route wants. Empty
    /// is legal and always wrong — the routes answer 404 without it, on purpose.
    pub token: String,
    /// `NASHCODE_BUGS_DRAIN_KEY`: the file holding the persistent iroh secret key. Its
    /// EndpointId is what the ingester's allow-file has to name. Unused by a TCP target.
    pub key_path: PathBuf,
    /// `NASHCODE_BUGS_DRAIN_INTERVAL`, seconds. The design says 15 to 30.
    pub interval: Duration,
}

/// Hand-written: `Config` derives `Debug`, and a derived one here would print the drain
/// token in every `?config` and every panic message that carries one.
impl std::fmt::Debug for Drain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drain")
            .field("target", &self.target)
            .field("token", &"<redacted>")
            .field("key_path", &self.key_path)
            .field("interval", &self.interval)
            .finish()
    }
}

impl Drain {
    /// True when the target is a URL rather than an iroh EndpointId.
    pub fn is_url(&self) -> bool {
        self.target.starts_with("http://") || self.target.starts_with("https://")
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// An environment variable that is either set to something useful or absent. An empty
/// value reads as absent, so `FOO=` in a unit file turns a feature off rather than
/// half-configuring it.
fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

impl Config {
    /// Read the configuration from the environment, applying the documented defaults.
    pub fn from_env() -> Self {
        let bind = env_or("NASHCODE_BIND", "127.0.0.1:8090");
        let mirrors = PathBuf::from(env_or(
            "NASHCODE_MIRRORS",
            &home().join("mirrors").to_string_lossy(),
        ));

        // A seed, not the whole list: discovery adds to it on every mirror poll.
        // `Repos::insert` refuses anything that is not a plain name; a typo in the
        // variable is worth a word, because a silently dropped repo looks like a
        // viewer that lost it. stderr, not `tracing`: configuration is read before
        // the subscriber exists.
        let repos = Repos::default();
        for name in
            env_or("NASHCODE_REPOS", "").split(',').map(str::trim).filter(|name| !name.is_empty())
        {
            if !is_plain_name(name) {
                eprintln!("NASHCODE_REPOS lists {name:?}, which is not a repo name; ignoring it");
            }
            repos.insert(name);
        }

        let db_path = PathBuf::from(env_or(
            "NASHCODE_DB",
            &mirrors.join("nashcode.db").to_string_lossy(),
        ));

        let ci_logs = PathBuf::from(env_or(
            "NASHCODE_CI_LOGS",
            &mirrors.join("ci-logs").to_string_lossy(),
        ));

        let traces = PathBuf::from(env_or(
            "NASHCODE_TRACES",
            &mirrors.join("traces").to_string_lossy(),
        ));

        let people_path = PathBuf::from(env_or(
            "NASHCODE_PEOPLE",
            &mirrors.join("people.json").to_string_lossy(),
        ));

        let webhooks = match std::env::var("NASHCODE_WEBHOOKS") {
            Ok(path) if !path.trim().is_empty() => load_webhooks(Path::new(path.trim())),
            _ => BTreeMap::new(),
        };

        // Seconds, floored at one rather than at fifteen: the design interval is 15 to
        // 30, but a test that drives a cycle by hand should not have to wait for it.
        let drain_interval = optional("NASHCODE_BUGS_DRAIN_INTERVAL")
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| Duration::from_secs(seconds.max(1)))
            .unwrap_or_else(|| Duration::from_secs(30));
        let bugs_drain = optional("NASHCODE_BUGS_DRAIN").map(|target| Drain {
            target,
            token: env_or("NASHCODE_BUGS_DRAIN_TOKEN", ""),
            key_path: PathBuf::from(env_or(
                "NASHCODE_BUGS_DRAIN_KEY",
                &mirrors.join("bugs-drain.key").to_string_lossy(),
            )),
            interval: drain_interval,
        });

        // Both or neither. Half-configured is the state that looks like it works and
        // never delivers, so it is refused loudly and then treated as off.
        let pushover = match (optional("NASHCODE_PUSHOVER_TOKEN"), optional("NASHCODE_PUSHOVER_USER"))
        {
            (Some(token), Some(user)) => Some(Pushover {
                token,
                user,
                api_url: env_or("NASHCODE_PUSHOVER_URL", "https://api.pushover.net")
                    .trim_end_matches('/')
                    .to_owned(),
            }),
            (None, None) => None,
            (token, _) => {
                let missing =
                    if token.is_some() { "NASHCODE_PUSHOVER_USER" } else { "NASHCODE_PUSHOVER_TOKEN" };
                // stderr, not `tracing`: configuration is read before the subscriber
                // exists, so a `tracing::error!` here would be shouted into nothing.
                eprintln!("{missing} is unset and its other half is not; Pushover stays off");
                None
            }
        };

        Self {
            dgit_url: env_or("DGIT_URL", "").trim_end_matches('/').to_owned(),
            git_token: env_or("GIT_TOKEN", ""),
            repos,
            mirrors,
            bugs_bucket: optional("NASHCODE_BUGS_BUCKET"),
            bugs_s3_endpoint: optional("S3_ENDPOINT"),
            bugs_ingest_url: env_or("NASHCODE_BUGS_INGEST_URL", &format!("http://{bind}"))
                .trim_end_matches('/')
                .to_owned(),
            bugs_drain,
            pushover,
            public_url: env_or("NASHCODE_URL", &format!("http://{bind}"))
                .trim_end_matches('/')
                .to_owned(),
            bugs_self_dsn: optional("NASHCODE_BUGS_SELF_DSN"),
            bind,
            db_path,
            ci_logs,
            traces,
            people_path,
            webhooks,
            anthropic_key: std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty()),
            anthropic_url: env_or("NASHCODE_ANTHROPIC_URL", "https://api.anthropic.com")
                .trim_end_matches('/')
                .to_owned(),
            brain_model: env_or("NASHCODE_BRAIN_MODEL", "claude-opus-5"),
        }
    }

    /// The clone URL for a repo on the dgit server. A `DGIT_URL` that is a filesystem
    /// path points straight at a directory of bare repos, which is what the tests use and
    /// what a local dgit-less setup can use too.
    pub fn remote_url(&self, repo: &str) -> String {
        format!("{}/{repo}.git", self.dgit_url)
    }

    /// Path of the `--mirror` clone for a repo.
    ///
    /// A repo name that is not a plain name yields a path *inside* the mirror
    /// directory that cannot exist, so it resolves to nothing rather than escaping.
    /// [`knows_repo`](Self::knows_repo) is the real gate and every handler goes
    /// through it; this is the second lock on the same door, for the day a caller
    /// forgets. `git --git-dir` takes whatever it is handed, so "forgot" would
    /// otherwise mean "read any repository on the box".
    pub fn mirror_path(&self, repo: &str) -> PathBuf {
        if !is_plain_name(repo) {
            return self.mirrors.join("__rejected__.invalid");
        }
        self.mirrors.join(format!("{repo}.git"))
    }

    /// True when `repo` is one of the known repos. Guards every path parameter.
    pub fn knows_repo(&self, repo: &str) -> bool {
        is_plain_name(repo) && self.repos.contains(repo)
    }
}

/// A name with no path in it: no separator, no traversal, no leading dash that a git
/// subcommand would read as a flag. dgit's own rule is narrower still, but this is the
/// property that matters here.
fn is_plain_name(repo: &str) -> bool {
    !repo.is_empty()
        && repo != "."
        && repo != ".."
        && !repo.starts_with('-')
        && !repo.contains('/')
        && !repo.contains('\\')
        && !repo.contains('\0')
}

/// Read the webhook map. A missing or malformed file is not fatal: webhooks are a
/// convenience, and the viewer must still start without them.
fn load_webhooks(path: &Path) -> BTreeMap<String, Vec<String>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(?path, %error, "cannot read NASHCODE_WEBHOOKS, continuing without");
            return BTreeMap::new();
        }
    };

    // Accept both {"push": "url"} and {"push": ["url", ...]}.
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(?path, %error, "NASHCODE_WEBHOOKS is not valid JSON, continuing without");
            return BTreeMap::new();
        }
    };

    let mut map = BTreeMap::new();
    if let Some(object) = parsed.as_object() {
        for (event, value) in object {
            let urls = match value {
                serde_json::Value::String(url) => vec![url.clone()],
                serde_json::Value::Array(items) => items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            };
            if !urls.is_empty() {
                map.insert(event.clone(), urls);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_map_accepts_a_string_or_a_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(
            &path,
            r#"{"push": "http://a/1", "merged": ["http://b/1", "http://b/2"]}"#,
        )
        .unwrap();

        let map = load_webhooks(&path);
        assert_eq!(map["push"], vec!["http://a/1"]);
        assert_eq!(map["merged"], vec!["http://b/1", "http://b/2"]);
    }

    #[test]
    fn a_missing_webhook_file_is_not_fatal() {
        assert!(load_webhooks(Path::new("/nonexistent/hooks.json")).is_empty());
    }

    #[test]
    fn a_repo_name_carrying_a_path_is_never_known_and_never_becomes_one() {
        let config = Config {
            repos: ["demo"].into_iter().collect(),
            mirrors: PathBuf::from("/srv/mirrors"),
            ..Config::from_env()
        };
        assert!(config.knows_repo("demo"));
        assert_eq!(config.mirror_path("demo"), PathBuf::from("/srv/mirrors/demo.git"));

        // `git --git-dir` would happily take any of these.
        for hostile in ["../../../../srv/git/private", "..", ".", "a/b", "-c", "x\0y", ""] {
            assert!(!config.knows_repo(hostile), "{hostile} must not be known");
            let path = config.mirror_path(hostile);
            assert!(
                path.starts_with("/srv/mirrors") && !path.to_string_lossy().contains(".."),
                "{hostile} escaped to {path:?}"
            );
        }
    }

    #[test]
    fn the_default_bind_is_loopback_only() {
        // No public listener: unless the operator overrides it, we bind 127.0.0.1.
        if std::env::var("NASHCODE_BIND").is_err() {
            assert_eq!(Config::from_env().bind, "127.0.0.1:8090");
        }
    }
}
