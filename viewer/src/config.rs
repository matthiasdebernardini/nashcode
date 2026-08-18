//! Configuration. Environment only — nothing about a deployment lives in source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Everything the viewer needs to know about the world around it.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the dgit server. Repo `foo` lives at `<dgit_url>/foo.git`.
    pub dgit_url: String,
    /// Push token. Sent as basic auth `x:<token>`. Empty means anonymous.
    pub git_token: String,
    /// Repo names to mirror, in the order they should be listed.
    pub repos: Vec<String>,
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
    /// Event name -> webhook URLs.
    pub webhooks: BTreeMap<String, Vec<String>>,
    /// `ANTHROPIC_API_KEY`. `/brain/ask` answers 404 without it.
    pub anthropic_key: Option<String>,
    /// Claude API base URL. `NASHCODE_ANTHROPIC_URL` overrides it so tests can point at
    /// a stub.
    pub anthropic_url: String,
    /// `NASHCODE_BRAIN_MODEL`.
    pub brain_model: String,
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

impl Config {
    /// Read the configuration from the environment, applying the documented defaults.
    pub fn from_env() -> Self {
        let mirrors = PathBuf::from(env_or(
            "NASHCODE_MIRRORS",
            &home().join("mirrors").to_string_lossy(),
        ));

        let repos = env_or("NASHCODE_REPOS", "")
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();

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

        let webhooks = match std::env::var("NASHCODE_WEBHOOKS") {
            Ok(path) if !path.trim().is_empty() => load_webhooks(Path::new(path.trim())),
            _ => BTreeMap::new(),
        };

        Self {
            dgit_url: env_or("DGIT_URL", "").trim_end_matches('/').to_owned(),
            git_token: env_or("GIT_TOKEN", ""),
            repos,
            mirrors,
            bind: env_or("NASHCODE_BIND", "127.0.0.1:8090"),
            db_path,
            ci_logs,
            traces,
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
    pub fn mirror_path(&self, repo: &str) -> PathBuf {
        self.mirrors.join(format!("{repo}.git"))
    }

    /// True when `repo` is one of the configured repos. Guards every path parameter.
    pub fn knows_repo(&self, repo: &str) -> bool {
        self.repos.iter().any(|known| known == repo)
    }
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
    fn the_default_bind_is_loopback_only() {
        // No public listener: unless the operator overrides it, we bind 127.0.0.1.
        if std::env::var("NASHCODE_BIND").is_err() {
            assert_eq!(Config::from_env().bind, "127.0.0.1:8090");
        }
    }
}
