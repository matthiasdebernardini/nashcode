//! `nashgit doctor` — one line per check, and never a false pass.
//!
//! A check that cannot run reports `skip` with the reason. Reporting a skipped
//! check as a pass is how a health command becomes useless, so the three
//! statuses stay distinct in both output modes and the exit code counts only
//! real failures.

use super::Ctx;
use crate::api::{AuthProbe, Client, Reach, classify};
use crate::output::mark;
use crate::profile::Profile;
use crate::remote;
use crate::ssh::{Ssh, parse_kv};
use anyhow::Result;
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Fail,
    Skip,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Stable machine name, safe to match on in a script.
    pub id: &'static str,
    /// Short human label, printed as-is.
    pub label: &'static str,
    pub status: Status,
    /// Why, in a few words. Always present, including on a pass.
    pub detail: String,
}

impl Check {
    fn new(id: &'static str, label: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            id,
            label,
            status,
            detail: detail.into(),
        }
    }
    fn ok(id: &'static str, label: &'static str, detail: impl Into<String>) -> Self {
        Self::new(id, label, Status::Ok, detail)
    }
    fn fail(id: &'static str, label: &'static str, detail: impl Into<String>) -> Self {
        Self::new(id, label, Status::Fail, detail)
    }
    fn skip(id: &'static str, label: &'static str, detail: impl Into<String>) -> Self {
        Self::new(id, label, Status::Skip, detail)
    }
}

/// The whole report. `ok` is false when any check failed; skips do not count.
pub fn report(profile: Option<&str>, checks: &[Check]) -> serde_json::Value {
    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    json!({
        "profile": profile,
        "ok": failed == 0,
        "failed": failed,
        "checks": checks,
    })
}

/// One line per check: mark, label padded to a common width, then the detail.
pub fn render(checks: &[Check]) -> Vec<String> {
    let width = checks.iter().map(|c| c.label.len()).max().unwrap_or(0);
    checks
        .iter()
        .map(|c| {
            format!(
                "{} {:width$}  {}",
                mark(c.status.as_str()),
                c.label,
                c.detail,
                width = width
            )
        })
        .collect()
}

pub fn run(ctx: &Ctx) -> Result<()> {
    let mut checks = Vec::new();

    let loaded = ctx.profile();
    let (name, p) = match loaded {
        Ok((n, p)) => {
            checks.push(Check::ok("profile", "profile", format!("{n} -> {}", p.url)));
            (n, p)
        }
        Err(e) => {
            checks.push(Check::fail("profile", "profile", e.to_string()));
            let value = report(None, &checks);
            ctx.out.emit(value, || {
                for line in render(&checks) {
                    ctx.out.line(line);
                }
            });
            std::process::exit(1);
        }
    };

    checks.extend(server_checks(&p));
    checks.extend(host_checks(&p));
    checks.push(viewer_check(&p));

    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    let value = report(Some(&name), &checks);
    ctx.out.emit(value, || {
        for line in render(&checks) {
            ctx.out.line(line);
        }
    });
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Reachability, certificate, and credential, from this machine.
fn server_checks(p: &Profile) -> Vec<Check> {
    let client = Client::with_timeout(&p.url, &p.token, std::time::Duration::from_secs(15));
    let mut out = Vec::new();

    let reply = match client.index_html() {
        Ok(r) => r,
        Err(e) => {
            let reach = e
                .downcast_ref::<ureq::Error>()
                .map(classify)
                .unwrap_or(Reach::Other);
            let detail = match reach {
                Reach::Tls => "certificate rejected — is `tailscale serve` still running?",
                Reach::Dns => "hostname does not resolve — are you on the tailnet?",
                Reach::Timeout => "no answer before the timeout",
                _ => "cannot connect",
            };
            out.push(Check::fail("server", "server", format!("{detail} ({})", p.url)));
            out.push(Check::skip("tls", "tls cert", "server unreachable"));
            out.push(Check::skip("token", "push token", "server unreachable"));
            out.push(Check::skip(
                "tailscale-headers",
                "tailnet headers",
                "server unreachable",
            ));
            return out;
        }
    };

    if reply.ok() {
        out.push(Check::ok("server", "server", format!("HTTP 200 from {}", p.url)));
    } else {
        out.push(Check::fail(
            "server",
            "server",
            format!("HTTP {} from {}", reply.status, p.url),
        ));
    }

    // Reaching an https:// URL at all means rustls accepted the chain.
    if p.url.starts_with("https://") {
        out.push(Check::ok("tls", "tls cert", "trusted"));
    } else {
        out.push(Check::skip("tls", "tls cert", "profile URL is not https"));
    }

    if p.token.is_empty() {
        out.push(Check::skip("token", "push token", "profile holds no token"));
    } else {
        // Probe against a repository that already exists when there is one, so
        // the request cannot even instantiate a new cell.
        let existing = crate::index_page::parse(&reply.body)
            .into_iter()
            .next()
            .map(|r| r.name)
            .unwrap_or_else(|| "nashgit-doctor-probe".to_string());
        match client.probe_auth(&existing) {
            Ok(AuthProbe::Accepted) => out.push(Check::ok("token", "push token", "accepted")),
            Ok(other) => out.push(Check::fail("token", "push token", other.describe())),
            Err(e) => out.push(Check::fail("token", "push token", e.to_string())),
        }
    }
    out
}

/// Everything that needs a shell on the box.
fn host_checks(p: &Profile) -> Vec<Check> {
    const IDS: [(&str, &str); 4] = [
        ("celld", "celld service"),
        ("loopback", "celld loopback"),
        ("tailscale-headers", "tailnet headers"),
        ("bucket", "bucket"),
    ];
    if p.ssh.is_empty() {
        return IDS
            .iter()
            .map(|(id, label)| Check::skip(id, label, "profile has no ssh destination"))
            .collect();
    }

    let listen = format!("127.0.0.1:{}", p.listen_port());
    let out = match Ssh::new(&p.ssh).script(&remote::doctor_script(p, &listen)) {
        Ok(o) if o.ok() => o,
        Ok(o) => {
            let reason = format!("ssh {} exited {}", p.ssh, o.code);
            return IDS
                .iter()
                .map(|(id, label)| Check::fail(id, label, reason.clone()))
                .collect();
        }
        Err(e) => {
            let reason = e.to_string();
            return IDS
                .iter()
                .map(|(id, label)| Check::fail(id, label, reason.clone()))
                .collect();
        }
    };
    let kv = parse_kv(&out.stdout);
    let get = |k: &str| kv.get(k).map(String::as_str).unwrap_or("");

    let mut checks = Vec::new();
    checks.push(match get("NASHGIT_SERVICE") {
        "active" => Check::ok("celld", "celld service", "active"),
        "" => Check::fail("celld", "celld service", "no answer from the host"),
        other => Check::fail("celld", "celld service", format!("systemd says `{other}`")),
    });
    checks.push(match get("NASHGIT_LOOPBACK") {
        "200" => Check::ok("loopback", "celld loopback", format!("HTTP 200 on {listen}")),
        code => Check::fail(
            "loopback",
            "celld loopback",
            format!("HTTP {code} on {listen}"),
        ),
    });
    checks.push(match (get("NASHGIT_SERVE"), get("NASHGIT_TS_STATE")) {
        ("ok", "Running") => Check::ok(
            "tailscale-headers",
            "tailnet headers",
            "tailscale serve is fronting celld and injecting identity headers",
        ),
        ("ok", state) => Check::fail(
            "tailscale-headers",
            "tailnet headers",
            format!("serve is configured but tailscaled is `{state}`"),
        ),
        (_, state) => Check::fail(
            "tailscale-headers",
            "tailnet headers",
            format!("no serve handler for {listen} (tailscaled is `{state}`)"),
        ),
    });
    checks.push(match get("NASHGIT_BUCKET") {
        "ok" => Check::ok("bucket", "bucket", "celld diagnose reached the store"),
        "skip" => Check::skip(
            "bucket",
            "bucket",
            "cannot check the bucket without passwordless sudo on the host",
        ),
        "missing" => Check::fail(
            "bucket",
            "bucket",
            "no /etc/celld/celld.env on the host — did setup finish?",
        ),
        _ => Check::fail("bucket", "bucket", "celld diagnose could not reach the store"),
    });
    checks
}

fn viewer_check(p: &Profile) -> Check {
    let Some(url) = p.viewer_url.as_deref() else {
        return Check::skip("viewer", "viewer", "no viewer configured");
    };
    let client = Client::with_timeout(url, "", std::time::Duration::from_secs(15));
    match client.get(&format!("{}/", url.trim_end_matches('/'))) {
        Ok(r) if r.status < 500 => Check::ok("viewer", "viewer", format!("HTTP {} from {url}", r.status)),
        Ok(r) => Check::fail("viewer", "viewer", format!("HTTP {} from {url}", r.status)),
        Err(e) => Check::fail("viewer", "viewer", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Check> {
        vec![
            Check::ok("profile", "profile", "box -> https://h"),
            Check::fail("server", "server", "cannot connect"),
            Check::skip("viewer", "viewer", "no viewer configured"),
        ]
    }

    #[test]
    fn render_is_one_aligned_line_per_check() {
        let lines = render(&sample());
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("✓ profile"));
        assert!(lines[1].starts_with("✗ server "));
        assert!(lines[2].starts_with("- viewer  "));
        // Details are aligned into one column.
        assert_eq!(
            lines[0].find("box ->").unwrap(),
            lines[1].find("cannot").unwrap()
        );
    }

    #[test]
    fn a_skip_is_not_a_pass_and_a_fail_sinks_the_report() {
        let value = report(Some("box"), &sample());
        assert_eq!(value["ok"], false);
        assert_eq!(value["failed"], 1);
        assert_eq!(value["checks"][2]["status"], "skip");

        let clean = vec![Check::ok("a", "a", "fine"), Check::skip("b", "b", "n/a")];
        let value = report(Some("box"), &clean);
        assert_eq!(value["ok"], true);
        assert_eq!(value["failed"], 0);
    }

    #[test]
    fn every_check_carries_a_stable_id_and_a_detail() {
        for c in sample() {
            assert!(!c.id.is_empty());
            assert!(!c.detail.is_empty(), "{} has no detail", c.id);
        }
    }
}
