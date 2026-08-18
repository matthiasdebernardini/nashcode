//! The dgit HTTP surface.
//!
//! dgit authenticates with HTTP Basic where the *password* is the shared
//! `GIT_TOKEN`; the username is ignored, so nashgit sends `x`. Reads of public
//! repositories need no auth at all.
//!
//! Endpoints used here (the whole admin API):
//!
//! | request                    | effect                                        |
//! |----------------------------|-----------------------------------------------|
//! | `GET /`                    | the index page, HTML (see `index_page`)       |
//! | `PUT /<repo>/config`       | create or re-describe a repository            |
//! | `POST /<repo>/gc`          | prune unreachable objects                     |
//! | `DELETE /<repo>`           | delete a repository                           |

use crate::index_page::Repo;
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
}

impl RepoConfig {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// What dgit echoes back from `PUT /<repo>/config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEcho {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub section: String,
    #[serde(default)]
    pub private: bool,
}

/// What dgit returns from `POST /<repo>/gc`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcResult {
    #[serde(default)]
    pub removed: u64,
    #[serde(default)]
    pub kept: u64,
    /// dgit skips the sweep on very large repositories.
    #[serde(default)]
    pub skipped: bool,
}

/// Why a request could not be made at all, as distinct from an HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    Tls,
    Dns,
    Timeout,
    Connect,
    Other,
}

impl Reach {
    pub fn as_str(self) -> &'static str {
        match self {
            Reach::Tls => "tls",
            Reach::Dns => "dns",
            Reach::Timeout => "timeout",
            Reach::Connect => "connect",
            Reach::Other => "other",
        }
    }
}

/// Classify a transport failure so `doctor` can tell "bad certificate" from
/// "nothing listening" from "not on the tailnet".
pub fn classify(err: &ureq::Error) -> Reach {
    match err {
        ureq::Error::Timeout(_) => Reach::Timeout,
        ureq::Error::HostNotFound => Reach::Dns,
        ureq::Error::Io(e) => {
            let text = e.to_string().to_lowercase();
            if text.contains("certificate")
                || text.contains("tls")
                || text.contains("handshake")
                || text.contains("invalidcertificate")
                || text.contains("unknownissuer")
            {
                Reach::Tls
            } else {
                Reach::Connect
            }
        }
        other => {
            let text = other.to_string().to_lowercase();
            if text.contains("certificate") || text.contains("tls") {
                Reach::Tls
            } else {
                Reach::Other
            }
        }
    }
}

pub struct Client {
    base: String,
    auth: Option<String>,
    agent: ureq::Agent,
}

/// One HTTP answer, reduced to what the CLI cares about.
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl Client {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self::with_timeout(base_url, token, Duration::from_secs(60))
    }

    pub fn with_timeout(base_url: &str, token: &str, timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Read statuses ourselves: a 401 is information, not an error.
            .http_status_as_error(false)
            .user_agent(concat!("nashgit/", env!("CARGO_PKG_VERSION")))
            .build();
        let auth = (!token.is_empty()).then(|| {
            let raw = format!("x:{token}");
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(raw)
            )
        });
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            auth,
            agent: config.new_agent(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn finish(r: http::Response<ureq::Body>) -> Result<Reply> {
        let status = r.status().as_u16();
        let body = r
            .into_body()
            .read_to_string()
            .unwrap_or_else(|e| format!("<unreadable body: {e}>"));
        Ok(Reply { status, body })
    }

    /// `GET /` — the index page, as HTML.
    pub fn index_html(&self) -> Result<Reply> {
        let mut req = self.agent.get(self.url("/"));
        // Send auth so a server with every repository private still lists.
        if let Some(a) = &self.auth {
            req = req.header("authorization", a);
        }
        Self::finish(req.call().context("GET / on the dgit server")?)
    }

    /// `GET /` without credentials — how an anonymous visitor sees the site.
    pub fn index_html_anonymous(&self) -> Result<Reply> {
        Self::finish(
            self.agent
                .get(self.url("/"))
                .call()
                .context("anonymous GET /")?,
        )
    }

    /// The repository list, parsed.
    pub fn list(&self) -> Result<Vec<Repo>> {
        let reply = self.index_html()?;
        if !reply.ok() {
            bail!("{} returned HTTP {}", self.base, reply.status);
        }
        Ok(crate::index_page::parse(&reply.body))
    }

    /// `PUT /<repo>/config`. Creates the repository when it does not exist.
    pub fn put_config(&self, repo: &str, cfg: &RepoConfig) -> Result<ConfigEcho> {
        let body = serde_json::to_string(cfg).context("serialize repo config")?;
        let reply = self.send("PUT", &format!("/{repo}/config"), Some(body))?;
        self.expect_ok(&reply, repo)?;
        serde_json::from_str(&reply.body)
            .with_context(|| format!("parse the config response from {}", self.base))
    }

    /// `POST /<repo>/gc`.
    pub fn gc(&self, repo: &str) -> Result<GcResult> {
        let reply = self.send("POST", &format!("/{repo}/gc"), None)?;
        self.expect_ok(&reply, repo)?;
        serde_json::from_str(&reply.body).with_context(|| format!("parse the gc response for {repo}"))
    }

    /// `DELETE /<repo>`.
    pub fn delete(&self, repo: &str) -> Result<()> {
        let reply = self.send("DELETE", &format!("/{repo}"), None)?;
        self.expect_ok(&reply, repo)?;
        Ok(())
    }

    /// Does the server accept our token?
    ///
    /// The probe is a `PUT /<repo>/config` carrying a body that is not JSON.
    /// dgit checks the token before it looks at the body, so a rejected token
    /// answers 401 and an accepted one answers 400 — and because the 400 is not
    /// a success, dgit writes nothing to the registry and changes no metadata.
    /// The probe is therefore read-only whichever way it lands.
    pub fn probe_auth(&self, repo: &str) -> Result<AuthProbe> {
        let reply = self.send("PUT", &format!("/{repo}/config"), Some("~".to_string()))?;
        Ok(match reply.status {
            400 => AuthProbe::Accepted,
            401 => AuthProbe::Rejected,
            403 => AuthProbe::ServerHasNoToken,
            s => AuthProbe::Unexpected(s),
        })
    }

    fn expect_ok(&self, reply: &Reply, repo: &str) -> Result<()> {
        if reply.ok() {
            return Ok(());
        }
        let hint = match reply.status {
            401 => " — the profile's token was rejected; check `nashgit token`",
            403 => " — the server has no GIT_TOKEN configured",
            404 => " — no such repository",
            _ => "",
        };
        bail!(
            "{} {} returned HTTP {}{}\n{}",
            self.base,
            repo,
            reply.status,
            hint,
            reply.body.trim()
        );
    }

    fn send(&self, method: &str, path: &str, body: Option<String>) -> Result<Reply> {
        let url = self.url(path);
        let ctx = format!("{method} {url}");
        let r = match method {
            "PUT" => {
                let mut req = self.agent.put(&url);
                if let Some(a) = &self.auth {
                    req = req.header("authorization", a);
                }
                req.header("content-type", "application/json")
                    .send(body.unwrap_or_default())
            }
            "POST" => {
                let mut req = self.agent.post(&url);
                if let Some(a) = &self.auth {
                    req = req.header("authorization", a);
                }
                req.send(body.unwrap_or_default())
            }
            "DELETE" => {
                let mut req = self.agent.delete(&url);
                if let Some(a) = &self.auth {
                    req = req.header("authorization", a);
                }
                req.call()
            }
            _ => {
                let mut req = self.agent.get(&url);
                if let Some(a) = &self.auth {
                    req = req.header("authorization", a);
                }
                req.call()
            }
        };
        Self::finish(r.with_context(|| ctx)?)
    }

    /// A plain GET, used for the viewer's JSON endpoints.
    pub fn get(&self, url: &str) -> Result<Reply> {
        Self::finish(
            self.agent
                .get(url)
                .call()
                .with_context(|| format!("GET {url}"))?,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProbe {
    Accepted,
    Rejected,
    ServerHasNoToken,
    Unexpected(u16),
}

impl AuthProbe {
    pub fn describe(self) -> String {
        match self {
            AuthProbe::Accepted => "token accepted".into(),
            AuthProbe::Rejected => "token rejected (401)".into(),
            AuthProbe::ServerHasNoToken => "server has no GIT_TOKEN set (403)".into(),
            AuthProbe::Unexpected(s) => format!("unexpected HTTP {s}"),
        }
    }
}

/// dgit's own repository-name rule. Reject bad names before a round trip.
pub fn valid_repo_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_follow_dgits_rule() {
        assert!(valid_repo_name("a"));
        assert!(valid_repo_name("my-repo_1.x"));
        assert!(!valid_repo_name(""));
        assert!(!valid_repo_name("-leading"));
        assert!(!valid_repo_name("has/slash"));
        assert!(!valid_repo_name("has space"));
    }

    #[test]
    fn config_serializes_only_the_fields_that_were_set() {
        let cfg = RepoConfig {
            description: Some("hi".into()),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&cfg).unwrap(), r#"{"description":"hi"}"#);
        assert!(RepoConfig::default().is_empty());
    }
}
