//! `nashcode doctor` — one entry per check, and never a false pass.
//!
//! A check that cannot run reports `skip` with the reason. Reporting a skipped
//! check as a pass is how a health command becomes useless, so the three
//! statuses stay distinct and only a real failure drives the exit code.
//!
//! The checks themselves live here; agcli owns the command, the report shape,
//! and the exit code it carries. [`checks`] is the seam between them.

use crate::api::{AuthProbe, Client, Reach, classify};
use crate::profile::{Profile, Store};
use crate::remote;
use crate::ssh::{Ssh, parse_kv};
use agcli::{CheckResult, ExitCode};
use serde::Serialize;
use std::sync::{Arc, OnceLock};

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

    /// The same finding, in the shape agcli reports. A failure carries the
    /// command to run next, because "the token is rejected" without "rotate it
    /// like this" is a diagnosis an agent cannot act on.
    fn to_result(&self) -> CheckResult {
        match self.status {
            Status::Ok => CheckResult::pass_with(self.detail.clone()),
            Status::Skip => CheckResult::skip(self.detail.clone()),
            Status::Fail => CheckResult::fail(self.detail.clone(), fix_for(self.id)),
        }
    }
}

/// What to run when a check fails. One runnable line per check id.
///
/// Shared with the handler boundary: a rejected token is the same problem
/// whether `doctor` found it or a push did, so it gets the same answer.
pub fn fix_for(id: &str) -> &'static str {
    match id {
        "profile" => "nashcode setup --host <user@host> --provider <aws-s3|r2|tigris> --bucket <name>",
        "server" => "tailscale status   # then, on the host: tailscale serve --bg --https=443 http://127.0.0.1:8080",
        "tls" => "ssh <host> tailscale serve status",
        "token" => "nashcode token   # compare it with GIT_TOKEN in ~/dgit/wrangler.celld.jsonc on the host",
        "celld" => "ssh <host> sudo systemctl restart celld && ssh <host> systemctl status celld",
        "loopback" => "ssh <host> journalctl -u celld -n 50",
        "tailscale-headers" => "ssh <host> sudo tailscale serve --bg --https=443 http://127.0.0.1:8080",
        "bucket" => "ssh <host> sudo celld diagnose   # check the bucket and credentials in /etc/celld/celld.env",
        "viewer" => "ssh <host> sudo systemctl restart nashcode-viewer",
        _ => "nashcode doctor",
    }
}

/// The checks, as agcli runs them.
///
/// agcli asks for one closure per check, but nashcode's checks come in batches:
/// four of them share one SSH round trip, three share one HTTP request. Running
/// each closure independently would multiply those calls by four. So the whole
/// sweep runs once, behind a `OnceLock`, and each closure reads its own row out
/// of the cached answer — the first check to run pays for all of them.
pub fn checks() -> Vec<agcli::Check> {
    // Every check, by its stable id, in report order, with the exit code its
    // failure should drive. A rejected token is an auth failure; everything
    // else that can break is the deployment being unreachable or wrong, which
    // is `API`. A missing profile is not a broken deployment — it is no
    // deployment, which is `NOT_FOUND`.
    const ORDER: [(&str, i32); 9] = [
        ("profile", ExitCode::NOT_FOUND),
        ("server", ExitCode::API),
        ("tls", ExitCode::API),
        ("token", ExitCode::AUTH),
        ("celld", ExitCode::API),
        ("loopback", ExitCode::API),
        ("tailscale-headers", ExitCode::API),
        ("bucket", ExitCode::API),
        ("viewer", ExitCode::API),
    ];

    let sweep: Arc<OnceLock<Vec<Check>>> = Arc::new(OnceLock::new());
    ORDER
        .iter()
        .map(|(id, code)| {
            let sweep = Arc::clone(&sweep);
            let id = *id;
            agcli::Check::with_request(id, move |req| {
                let sweep = Arc::clone(&sweep);
                let profile = req.flag("profile").map(str::to_string);
                Box::pin(async move {
                    let all = sweep.get_or_init(|| sweep_all(profile.as_deref()));
                    match all.iter().find(|c| c.id == id) {
                        Some(c) => c.to_result(),
                        // The profile check failed, so nothing after it ran.
                        None => CheckResult::skip("skipped: the profile could not be read"),
                    }
                })
            })
            .exit_code(*code)
        })
        .collect()
}

/// Run every check once, in order, and stop where a failure makes the rest
/// meaningless: with no profile there is no server to ask about.
fn sweep_all(profile: Option<&str>) -> Vec<Check> {
    let mut checks = Vec::new();
    let p = match Store::load().and_then(|store| {
        let (n, p) = store.resolve(profile)?;
        Ok((n, p.clone()))
    }) {
        Ok((n, p)) => {
            checks.push(Check::ok("profile", "profile", format!("{n} -> {}", p.url)));
            p
        }
        Err(e) => {
            checks.push(Check::fail("profile", "profile", e.to_string()));
            return checks;
        }
    };

    checks.extend(server_checks(&p));
    checks.extend(host_checks(&p));
    checks.push(viewer_check(&p));
    checks
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
        let existing = dgit_index::parse(&reply.body)
            .into_iter()
            .next()
            .map(|r| r.name)
            .unwrap_or_else(|| "nashcode-doctor-probe".to_string());
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
    checks.push(match get("NASHCODE_SERVICE") {
        "active" => Check::ok("celld", "celld service", "active"),
        "" => Check::fail("celld", "celld service", "no answer from the host"),
        other => Check::fail("celld", "celld service", format!("systemd says `{other}`")),
    });
    checks.push(match get("NASHCODE_LOOPBACK") {
        "200" => Check::ok("loopback", "celld loopback", format!("HTTP 200 on {listen}")),
        code => Check::fail(
            "loopback",
            "celld loopback",
            format!("HTTP {code} on {listen}"),
        ),
    });
    checks.push(match (get("NASHCODE_SERVE"), get("NASHCODE_TS_STATE")) {
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
    checks.push(match get("NASHCODE_BUCKET") {
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
    fn a_skip_is_not_a_pass_and_a_fail_carries_a_runnable_fix() {
        let results: Vec<CheckResult> = sample().iter().map(Check::to_result).collect();
        assert!(results[0].is_ok() && !results[0].skipped());
        assert!(results[1].failed());
        assert!(results[2].skipped(), "a skip must never read as a pass");
        // agcli leaves `healthy` true for a skip, so the skip must say why.
        assert_eq!(
            results[2].detail.as_deref(),
            Some("no viewer configured")
        );
        // A failure without a next command is a diagnosis an agent cannot act on.
        let fix = results[1].fix.as_deref().unwrap();
        assert!(fix.contains("tailscale"), "{fix}");
    }

    #[test]
    fn every_check_carries_a_stable_id_a_detail_and_a_fix() {
        for c in sample() {
            assert!(!c.id.is_empty());
            assert!(!c.detail.is_empty(), "{} has no detail", c.id);
            assert!(!fix_for(c.id).is_empty(), "{} has no fix", c.id);
        }
    }

    #[test]
    fn the_registered_checks_cover_every_id_the_sweep_can_produce() {
        let names: Vec<String> = checks().iter().map(|c| c.name().to_string()).collect();
        assert_eq!(names.len(), 9);
        for id in [
            "profile", "server", "tls", "token", "celld", "loopback",
            "tailscale-headers", "bucket", "viewer",
        ] {
            assert!(names.iter().any(|n| n == id), "{id} is not registered");
            assert_ne!(fix_for(id), "nashcode doctor", "{id} has only the fallback fix");
        }
    }
}
