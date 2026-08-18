//! The local profile store.
//!
//! One TOML file holds every deployment this machine knows about, plus which
//! one is active. The file holds push tokens, so it is written with mode 0600
//! and the parent directory with 0700.
//!
//! Location, in order of precedence:
//!   1. `$NASHGIT_CONFIG`            (a full path to the file; used by tests)
//!   2. `$XDG_CONFIG_HOME/nashgit/config.toml`
//!   3. `~/.config/nashgit/config.toml`

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One deployment: a dgit server, the box it runs on, and the push token.
///
/// Everything here is non-secret except `token`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Base URL of the dgit server, no trailing slash. e.g. `https://box.tailnet.ts.net`
    pub url: String,
    /// SSH destination of the host, e.g. `user@box`. Empty when the profile was
    /// added by hand against a server the user does not administer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh: String,
    /// dgit `GIT_TOKEN`: the Basic-auth password for pushes and admin calls.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// Base URL of the nashgit viewer, when one is deployed alongside dgit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer_url: Option<String>,
    /// Loopback port celld listens on, from `setup --listen-port`. Absent in
    /// profiles written before this field existed; read it via
    /// [`Profile::listen_port`], which falls back to 8080.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
    /// Where dgit's checkout lives on the host, from setup. Absent in older
    /// profiles; consumers fall back to `~/dgit`, which setup always used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dgit_dir: Option<String>,

    // --- celld facts, kept so `doctor` can check the fleet without re-asking ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Bucket in celld's own form, e.g. `s3://my-cells` or `s3://my-cells/prefix`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_owner: Option<String>,
}

impl Profile {
    /// The URL a repository lives at, e.g. `https://host/myrepo.git`.
    pub fn repo_url(&self, name: &str) -> String {
        format!("{}/{}.git", self.url.trim_end_matches('/'), name)
    }

    /// Host part of `url`, used as the key for `git credential approve`.
    pub fn url_host(&self) -> Result<String> {
        url_host(&self.url)
    }

    /// The loopback port celld listens on. 8080 when the profile predates the
    /// field.
    pub fn listen_port(&self) -> u16 {
        self.listen_port.unwrap_or(8080)
    }
}

/// The whole store: named profiles plus the active selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    /// Name of the active profile. `None` when the store is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

impl Store {
    /// Resolve which profile a command should act on.
    ///
    /// `override_name` is the global `--profile` flag.
    pub fn resolve(&self, override_name: Option<&str>) -> Result<(String, &Profile)> {
        let name = match override_name {
            Some(n) => n.to_string(),
            None => self.active.clone().ok_or_else(|| {
                anyhow!("no active profile. Run `nashgit setup`, or `nashgit use <profile>`")
            })?,
        };
        let p = self
            .profiles
            .get(&name)
            .ok_or_else(|| anyhow!("no profile named `{name}`. See `nashgit profiles`"))?;
        Ok((name, p))
    }

    pub fn insert(&mut self, name: &str, profile: Profile) {
        self.profiles.insert(name.to_string(), profile);
        if self.active.is_none() {
            self.active = Some(name.to_string());
        }
    }

    pub fn set_active(&mut self, name: &str) -> Result<()> {
        if !self.profiles.contains_key(name) {
            bail!("no profile named `{name}`. See `nashgit profiles`");
        }
        self.active = Some(name.to_string());
        Ok(())
    }

    /// Read the store, returning an empty one when the file does not exist.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read profile store {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse profile store {}", path.display()))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = config_path()?;
        self.save_to(&path)?;
        Ok(path)
    }

    /// Write the store with 0600 on the file and 0700 on its directory.
    ///
    /// The write is atomic: the content goes to a locked-down temp file in
    /// the same directory, which then renames over the real one. A crash
    /// mid-write leaves the old store intact, never a truncated one — and the
    /// token never exists on disk under a wider mode, even briefly.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create config dir {}", dir.display()))?;
            set_mode(dir, 0o700)?;
        }
        let text = toml::to_string_pretty(self).context("serialize profile store")?;
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow!("`{}` has no file name", path.display()))?
            .to_string_lossy();
        let tmp = path.with_file_name(format!(".{file_name}.tmp.{}", std::process::id()));
        let write = || -> Result<()> {
            std::fs::write(&tmp, "").with_context(|| format!("create {}", tmp.display()))?;
            set_mode(&tmp, 0o600)?;
            std::fs::write(&tmp, text.as_bytes())
                .with_context(|| format!("write {}", tmp.display()))?;
            std::fs::rename(&tmp, path)
                .with_context(|| format!("rename {} into place", tmp.display()))?;
            Ok(())
        };
        write().inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Where the profile store lives on this machine.
pub fn config_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("NASHGIT_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    // Not dirs::config_dir(): on macOS that is ~/Library/Application Support,
    // while every doc and dev-CLI convention here says ~/.config.
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("nashgit").join("config.toml"));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("cannot locate a home directory; set $NASHGIT_CONFIG"))?;
    Ok(home.join(".config").join("nashgit").join("config.toml"))
}

/// Pull the host (with port, without scheme, userinfo, or path) out of a URL.
///
/// Small and hand-rolled on purpose: it is the only URL parsing the CLI needs,
/// and it keeps a URL crate out of the dependency tree.
pub fn url_host(url: &str) -> Result<String> {
    let rest = url
        .split_once("://")
        .map(|(_, r)| r)
        .ok_or_else(|| anyhow!("`{url}` has no scheme; expected https://host"))?;
    let rest = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    if host.is_empty() {
        bail!("`{url}` has no host");
    }
    Ok(host.to_string())
}

/// Suggest a profile name from an SSH destination or URL: `me@build-box` -> `build-box`.
pub fn suggest_name(ssh_or_url: &str) -> String {
    let s = ssh_or_url
        .rsplit_once("://")
        .map(|(_, r)| r)
        .unwrap_or(ssh_or_url);
    let s = s.rsplit_once('@').map(|(_, h)| h).unwrap_or(s);
    let s = s.split(['/', ':']).next().unwrap_or(s);
    // An IP host keeps every octet: `192` names nothing.
    let host = if s.parse::<std::net::IpAddr>().is_ok() {
        s
    } else {
        s.split('.').next().unwrap_or(s)
    };
    let cleaned: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_lowercase();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_host_strips_scheme_userinfo_and_path() {
        assert_eq!(url_host("https://box.example.ts.net").unwrap(), "box.example.ts.net");
        assert_eq!(url_host("https://box.example.ts.net/a/b").unwrap(), "box.example.ts.net");
        assert_eq!(url_host("https://x:tok@box.example.ts.net:8443/").unwrap(), "box.example.ts.net:8443");
        assert!(url_host("box.example.ts.net").is_err());
    }

    #[test]
    fn suggest_name_takes_the_short_host() {
        assert_eq!(suggest_name("me@build-box.example.ts.net"), "build-box");
        assert_eq!(suggest_name("https://Build_Box.example.net"), "build-box");
        assert_eq!(suggest_name(""), "default");
    }

    #[test]
    fn an_ip_host_keeps_all_its_octets() {
        assert_eq!(suggest_name("me@203.0.113.7"), "203-0-113-7");
        assert_eq!(suggest_name("203.0.113.7"), "203-0-113-7");
    }

    #[test]
    fn repo_url_appends_dot_git() {
        let p = Profile { url: "https://h/".into(), ..Default::default() };
        assert_eq!(p.repo_url("x"), "https://h/x.git");
    }
}
