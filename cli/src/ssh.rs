//! Everything server-side goes through the system `ssh`.
//!
//! No SSH library: shelling out means the user's `~/.ssh/config`, their agent,
//! their jump hosts, and their hardware keys all keep working, and nashcode
//! never holds a private key.
//!
//! Remote work is sent as a *script on stdin* (`ssh dest bash -s`) rather than
//! as command arguments. Two reasons: secrets never reach the remote `argv`, so
//! they never appear in the host's process list; and a multi-step script can be
//! written plainly, with `set -eu`, instead of escaped into one line.
//!
//! `$NASHCODE_SSH_BIN` replaces the `ssh` executable. Tests point it at a shim.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct Ssh {
    /// SSH destination, e.g. `me@build-box`.
    pub dest: String,
    /// When true, print scripts instead of running them.
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// Trimmed stdout, the common case for a one-line probe.
    pub fn trimmed(&self) -> &str {
        self.stdout.trim()
    }

    /// Turn a non-zero exit into an error carrying the remote stderr.
    pub fn require(self, what: &str) -> Result<Output> {
        if self.ok() {
            return Ok(self);
        }
        let tail: String = self
            .stderr
            .lines()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        bail!("{what} failed on the host (exit {})\n{}", self.code, tail.trim_end());
    }
}

/// Path of the `ssh` binary, overridable for tests.
pub fn ssh_bin() -> String {
    std::env::var("NASHCODE_SSH_BIN").unwrap_or_else(|_| "ssh".to_string())
}

impl Ssh {
    pub fn new(dest: impl Into<String>) -> Self {
        Self {
            dest: dest.into(),
            dry_run: false,
        }
    }

    pub fn dry_run(mut self, on: bool) -> Self {
        self.dry_run = on;
        self
    }

    fn base_command(&self) -> Command {
        let mut c = Command::new(ssh_bin());
        c.arg("-o").arg("ConnectTimeout=15");
        // Multiplex when the user already has a control master; harmless otherwise.
        c.arg("-o").arg("ServerAliveInterval=30");
        c.arg(&self.dest);
        c
    }

    /// Run a shell script on the host. The script is piped in on stdin.
    ///
    /// The script always runs under `bash` because every script this CLI
    /// generates uses `set -eu` plus bash-only conveniences.
    pub fn script(&self, script: &str) -> Result<Output> {
        if self.dry_run {
            return Ok(Output {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let mut cmd = self.base_command();
        cmd.arg("bash").arg("-s");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {} {}", ssh_bin(), self.dest))?;
        child
            .stdin
            .take()
            .context("ssh stdin")?
            .write_all(script.as_bytes())
            .context("write remote script to ssh stdin")?;
        let out = child.wait_with_output().context("wait for ssh")?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Run a script and stream its output to the terminal as it happens.
    ///
    /// Used for the long steps of `setup` (package installs, `npm install`),
    /// where silence for two minutes reads as a hang. stdout of the remote
    /// script is forwarded to *stderr* locally so `--json` stdout stays clean.
    pub fn script_streaming(&self, script: &str) -> Result<Output> {
        if self.dry_run {
            return Ok(Output {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let mut cmd = self.base_command();
        cmd.arg("bash").arg("-s");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {} {}", ssh_bin(), self.dest))?;
        child
            .stdin
            .take()
            .context("ssh stdin")?
            .write_all(script.as_bytes())
            .context("write remote script to ssh stdin")?;
        let status = child.wait().context("wait for ssh")?;
        Ok(Output {
            code: status.code().unwrap_or(-1),
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    /// Cheap liveness probe. Returns the remote `uname -sm`.
    pub fn probe(&self) -> Result<Output> {
        self.script("set -eu\nuname -sm\n")
    }
}

/// Parse `KEY=value` lines out of a probe script's stdout, ignoring anything else.
pub fn parse_kv(stdout: &str) -> std::collections::BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kv_reads_probe_output() {
        let kv = parse_kv("noise\nNASHCODE_ARCH=aarch64\nNASHCODE_SUDO=nopasswd\n");
        assert_eq!(kv["NASHCODE_ARCH"], "aarch64");
        assert_eq!(kv["NASHCODE_SUDO"], "nopasswd");
        assert_eq!(kv.len(), 2);
    }

    #[test]
    fn require_reports_the_remote_tail() {
        let out = Output {
            code: 3,
            stdout: String::new(),
            stderr: "boom\n".into(),
        };
        let err = out.require("install").unwrap_err().to_string();
        assert!(err.contains("install failed"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }
}
