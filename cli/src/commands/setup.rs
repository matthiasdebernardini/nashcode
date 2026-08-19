//! `nashcode setup` — the wizard that builds a deployment from nothing.
//!
//! Seven steps, in the order a person would do them by hand. Each step is
//! separately idempotent, so a run that dies at step 5 can be restarted from
//! the top without undoing step 4.

use super::Ctx;
use crate::cli::{Provider, SetupArgs};
use crate::output::Out;
use crate::profile::{Profile, Store, suggest_name};
use crate::remote::{self, Deploy};
use crate::ssh::{Output, Ssh, parse_kv};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The one warning this CLI insists on showing.
///
/// A store without conditional writes does not fail loudly. celld hands the
/// same repository to two nodes, both write, and the loser's commits vanish.
const FENCING_WARNING: &str = "\
celld stores every repository in your bucket and uses that bucket to decide
which node owns which repository. That needs conditional writes and
read-after-write consistency. Amazon S3, Cloudflare R2, Google Cloud Storage,
Azure Blob Storage, and Tigris provide them. MinIO community edition,
Backblaze B2, Hetzner Object Storage, and DigitalOcean Spaces do not, and they
fail silently: two nodes can own one repository and acknowledged pushes can
disappear. Those four are not offered here.
Read the mechanism: https://celld.dev/docs/fencing";

pub fn run(ctx: &Ctx, args: &SetupArgs) -> Result<Value> {
    let out = &ctx.out;
    // Under --dry-run every remote script is collected instead of run, and comes
    // back in the result: stdout belongs to the envelope, so a preview cannot be
    // printed, it has to be *returned*.
    let mut scripts: Vec<Value> = Vec::new();

    // ---- 1. host ---------------------------------------------------------
    out.step("[1/7] host");
    let host = ask(
        "SSH destination of the host (user@host)",
        "--host",
        args.host.clone(),
        None,
    )?;
    let ssh = Ssh::new(&host).dry_run(args.dry_run);
    let facts = preflight(out, &mut scripts, &ssh, args.dry_run)?;

    // ---- 2. bucket -------------------------------------------------------
    out.step("[2/7] bucket");
    if !args.dry_run {
        for line in FENCING_WARNING.lines() {
            out.step(line);
        }
    }
    let Some(provider) = args.provider else {
        bail!(
            "--provider is required: one of {}",
            Provider::all()
                .iter()
                .map(|p| p.id())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let bucket_input = ask(
        "bucket name (or a full s3://bucket/prefix URL)",
        "--bucket",
        args.bucket.clone(),
        None,
    )?;
    let bucket = normalise_bucket(&bucket_input);
    let region = ask(
        "storage region",
        "--region",
        args.region.clone(),
        Some(provider.default_region().to_string()),
    )?;
    let endpoint = resolve_endpoint(args, provider)?;
    // These three land raw in the systemd unit, the EnvironmentFile, and the
    // celld command line; reject anything those places cannot carry.
    reject_unsafe_value("bucket", &bucket)?;
    reject_unsafe_value("region", &region)?;
    if let Some(e) = &endpoint {
        reject_unsafe_value("endpoint", e)?;
    }
    let (access_key_id, secret_access_key) = resolve_credentials(out, args)?;

    // ---- 3. install ------------------------------------------------------
    out.step("[3/7] install tailscale, celld, node, esbuild, git");
    let install = run_script(
        &mut scripts,
        &ssh,
        args.dry_run,
        "install",
        &remote::install_script(),
        true,
    )?;
    let tools = if args.dry_run {
        BTreeMap::new()
    } else {
        let captured = ssh
            .script(&remote::preflight_script())?
            .require("re-reading the host's tools")?;
        parse_kv(&captured.stdout)
    };
    let _ = install;

    let celld_bin = tools
        .get("NASHCODE_HAS_CELLD")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.local/bin/celld", facts.home));
    let esbuild_bin = tools
        .get("NASHCODE_HAS_ESBUILD")
        .cloned()
        .unwrap_or_default();

    // ---- 4. deploy dgit --------------------------------------------------
    out.step("[4/7] deploy dgit on celld");
    let token = match &args.token {
        Some(t) => t.clone(),
        None => generate_token(),
    };
    let site_name = ask(
        "site name shown in the web interface",
        "--site-name",
        args.site_name.clone(),
        Some(short_host(&host)),
    )?;
    let site_owner = ask(
        "default owner shown against repositories",
        "--site-owner",
        args.site_owner.clone(),
        Some(facts.user.clone()),
    )?;
    let site_desc = args
        .site_desc
        .clone()
        .unwrap_or_else(|| "a fast webinterface for the git dscm, on celld".to_string());

    let deploy = Deploy {
        bucket: bucket.clone(),
        endpoint: endpoint.clone(),
        region: region.clone(),
        access_key_id,
        secret_access_key,
        session_token: std::env::var("AWS_SESSION_TOKEN").ok().filter(|s| !s.is_empty()),
        site_name: site_name.clone(),
        site_desc,
        site_owner: site_owner.clone(),
        token: token.clone(),
        dgit_dir: format!("{}/dgit", facts.home),
        service_user: facts.user.clone(),
        listen: format!("127.0.0.1:{}", args.listen_port),
        celld_bin,
        esbuild_bin,
        viewer: args.viewer,
        viewer_port: args.viewer_port,
        https_port: 443,
        viewer_https_port: 8443,
    };

    run_script(&mut scripts, &ssh, args.dry_run, "deploy", &remote::deploy_script(&deploy), true)?;
    run_script(&mut scripts, &ssh, args.dry_run, "service", &remote::service_script(&deploy), true)?;

    // ---- 5. tailnet ------------------------------------------------------
    out.step("[5/7] tailnet");
    let authkey = args
        .tailscale_authkey
        .clone()
        .or_else(|| std::env::var("TS_AUTHKEY").ok().filter(|s| !s.is_empty()));
    let dns_name = join_tailnet(out, &mut scripts, &ssh, args.dry_run, authkey.as_deref())?;
    run_script(&mut scripts, &ssh, args.dry_run, "serve", &remote::serve_script(&deploy), false)?;

    let url = if args.dry_run || dns_name.is_empty() {
        if !args.dry_run {
            out.warn(format!(
                "the node reported no MagicDNS name, so the profile URL is a guess \
                 (https://{}). Enable MagicDNS in the Tailscale admin console, or edit \
                 `url` in the profile by hand.",
                short_host(&host)
            ));
        }
        format!("https://{}", short_host(&host))
    } else {
        format!("https://{dns_name}")
    };
    let viewer_url = args
        .viewer
        .then(|| format!("{url}:{}", deploy.viewer_https_port));

    // ---- 6. verify -------------------------------------------------------
    if args.skip_verify {
        out.step("[6/7] verify — skipped");
    } else {
        out.step("[6/7] verify: push with the token, clone anonymously, delete");
        run_script(
            &mut scripts,
            &ssh,
            args.dry_run,
            "verify",
            &remote::verify_script(&token, &deploy.listen),
            true,
        )?;
    }

    // ---- 7. profile ------------------------------------------------------
    out.step("[7/7] save the profile");
    let profile_name = args
        .name
        .clone()
        .unwrap_or_else(|| suggest_name(&host));
    let profile = Profile {
        url: url.clone(),
        ssh: host.clone(),
        token: token.clone(),
        viewer_url: viewer_url.clone(),
        listen_port: Some(args.listen_port),
        dgit_dir: Some(deploy.dgit_dir.clone()),
        provider: Some(provider.id().to_string()),
        bucket: Some(bucket.clone()),
        endpoint: endpoint.clone(),
        region: Some(region.clone()),
        site_name: Some(site_name),
        site_owner: Some(site_owner),
    };
    let path = if args.dry_run {
        out.step("dry run: the profile was not written");
        crate::profile::config_path()?
    } else {
        let mut store = Store::load()?;
        store.insert(&profile_name, profile);
        store.set_active(&profile_name)?;
        store.save()?
    };

    Ok(json!({
        "profile": profile_name,
        "url": url,
        "viewer_url": viewer_url,
        "ssh": host,
        "bucket": bucket,
        "endpoint": endpoint,
        "region": region,
        "config": path,
        "dry_run": args.dry_run,
        "scripts": scripts,
    }))
}

// --- steps -----------------------------------------------------------------

struct Facts {
    user: String,
    home: String,
}

fn preflight(out: &Out, scripts: &mut Vec<Value>, ssh: &Ssh, dry_run: bool) -> Result<Facts> {
    if dry_run {
        record(scripts, "preflight", &remote::preflight_script());
        return Ok(Facts {
            user: "USER".into(),
            home: "/home/USER".into(),
        });
    }
    let res = ssh
        .script(&remote::preflight_script())
        .with_context(|| format!("cannot reach {} over ssh", ssh.dest))?;
    if !res.ok() {
        bail!(
            "cannot run a shell on {} (ssh exited {}).\n{}",
            ssh.dest,
            res.code,
            res.stderr.trim()
        );
    }
    let kv = parse_kv(&res.stdout);
    let get = |k: &str| kv.get(k).map(String::as_str).unwrap_or("");

    if get("NASHCODE_OS") != "Linux" {
        bail!(
            "the host reports `{}`; nashcode sets up Linux hosts with systemd only",
            get("NASHCODE_OS")
        );
    }
    if get("NASHCODE_SYSTEMD") != "yes" {
        bail!("the host has no systemctl; nashcode needs systemd to run celld as a service");
    }
    match get("NASHCODE_ROOT") {
        "root" | "sudo" => {}
        "sudo-password" => bail!(
            "sudo on {} asks for a password.\n\
             nashcode runs unattended scripts over ssh, so it needs either a root login or \
             passwordless sudo. Add a sudoers rule for this user, or connect as root.",
            ssh.dest
        ),
        _ => bail!("no way to reach root on {}: install sudo or connect as root", ssh.dest),
    }
    out.step(format!(
        "{} — {} {}, {} access as {}",
        ssh.dest,
        get("NASHCODE_DISTRO"),
        get("NASHCODE_ARCH"),
        get("NASHCODE_ROOT"),
        get("NASHCODE_USER"),
    ));
    Ok(Facts {
        user: get("NASHCODE_USER").to_string(),
        home: get("NASHCODE_HOME").to_string(),
    })
}

/// `tailscale up`, then wait for the node to come up, and report its DNS name.
fn join_tailnet(
    out: &Out,
    scripts: &mut Vec<Value>,
    ssh: &Ssh,
    dry_run: bool,
    authkey: Option<&str>,
) -> Result<String> {
    if dry_run {
        record(scripts, "tailscale up", &remote::tailscale_up_script(authkey));
        record(scripts, "tailscale status", &remote::tailscale_status_script());
        return Ok(String::new());
    }
    let up = ssh
        .script(&remote::tailscale_up_script(authkey))?
        .require("tailscale up")?;
    let kv = parse_kv(&up.stdout);
    if let Some(url) = kv.get("NASHCODE_AUTH_URL").filter(|u| !u.is_empty()) {
        // stderr, so it survives --json and shows up even when piped.
        eprintln!("\n  Authorise this machine on your tailnet:\n  {url}\n");
    }

    for attempt in 0..60 {
        let res = ssh.script(&remote::tailscale_status_script())?;
        let kv = parse_kv(&res.stdout);
        if kv.get("NASHCODE_TS_STATE").map(String::as_str) == Some("Running") {
            let dns = kv.get("NASHCODE_DNSNAME").cloned().unwrap_or_default();
            out.step(format!("on the tailnet as {dns}"));
            return Ok(dns);
        }
        if attempt == 0 {
            out.step("waiting for the node to join the tailnet");
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    bail!("the host did not join the tailnet within five minutes");
}

// --- plumbing --------------------------------------------------------------

fn run_script(
    scripts: &mut Vec<Value>,
    ssh: &Ssh,
    dry_run: bool,
    what: &str,
    script: &str,
    stream: bool,
) -> Result<Output> {
    if dry_run {
        record(scripts, what, script);
        return Ok(Output {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        });
    }
    let res = if stream {
        ssh.script_streaming(script)?
    } else {
        ssh.script(script)?
    };
    res.require(what)
}

/// A script a dry run would have executed, kept for the result.
fn record(scripts: &mut Vec<Value>, what: &str, script: &str) {
    scripts.push(json!({ "step": what, "script": script.trim_end() }));
}

/// Take the flag, fall back to the default, or name the flag that is missing.
///
/// Nothing is ever asked. Nobody is at a terminal: an agent runs this, and an
/// agent cannot answer a prompt, so a missing answer is a usage error naming the
/// flag that would have supplied it.
fn ask(
    label: &str,
    flag: &str,
    value: Option<String>,
    default: Option<String>,
) -> Result<String> {
    let _ = label;
    if let Some(v) = value.filter(|v| !v.is_empty()) {
        return Ok(v);
    }
    if let Some(d) = default {
        return Ok(d);
    }
    bail!("{flag} is required")
}

fn resolve_endpoint(args: &SetupArgs, provider: Provider) -> Result<Option<String>> {
    if let Some(e) = args.endpoint.clone().filter(|e| !e.is_empty()) {
        return Ok(Some(e));
    }
    if let Some(d) = provider.default_endpoint() {
        return Ok(Some(d.to_string()));
    }
    if provider == Provider::AwsS3 {
        return Ok(None); // the AWS SDK default is right
    }
    bail!(
        "--endpoint is required for {} ({})",
        provider.label(),
        provider.endpoint_hint()
    )
}

/// Credentials, in the order that keeps them out of shell history: the flag if
/// given, then the environment.
fn resolve_credentials(out: &Out, args: &SetupArgs) -> Result<(Option<String>, Option<String>)> {
    if args.creds_on_host {
        out.step("using the credentials already on the host");
        return Ok((None, None));
    }
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());

    let id = args
        .access_key_id
        .clone()
        .or_else(|| env("AWS_ACCESS_KEY_ID"));
    let secret = args
        .secret_access_key
        .clone()
        .or_else(|| env("AWS_SECRET_ACCESS_KEY"));

    if let (Some(id), Some(secret)) = (&id, &secret) {
        return Ok((Some(id.clone()), Some(secret.clone())));
    }
    bail!(
        "no bucket credentials. Set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, \
         or pass --creds-on-host if the box already has them."
    )
}

/// Bucket, region, and endpoint go into the systemd unit's ExecStart (no
/// shell, split on whitespace), the EnvironmentFile (one KEY=value per line),
/// and systemd's `%` specifier expansion. None of those can carry whitespace,
/// a newline, or a percent sign, so refuse them up front.
pub fn reject_unsafe_value(what: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("the {what} is empty");
    }
    if let Some(c) = value.chars().find(|c| c.is_whitespace() || *c == '%') {
        let shown = if c.is_whitespace() {
            "whitespace".to_string()
        } else {
            format!("`{c}`")
        };
        bail!(
            "the {what} `{}` contains {shown}, which the systemd unit and its \
             EnvironmentFile cannot carry safely",
            value.escape_default()
        );
    }
    Ok(())
}

/// Accept `my-bucket`, `s3://my-bucket`, or `s3://my-bucket/prefix`.
pub fn normalise_bucket(input: &str) -> String {
    let s = input.trim();
    if s.contains("://") {
        s.to_string()
    } else {
        format!("s3://{}", s.trim_start_matches('/'))
    }
}

/// `me@build-box.example.ts.net` -> `build-box`. An IP address stays whole:
/// its dots are not subdomains, and `192` is not a hostname.
pub fn short_host(dest: &str) -> String {
    let host = dest.rsplit_once('@').map(|(_, h)| h).unwrap_or(dest);
    let host = host.split(':').next().unwrap_or(host);
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_string();
    }
    host.split('.').next().unwrap_or(host).to_string()
}

/// 24 random bytes, hex. Same strength as `openssl rand -hex 24`.
///
/// Generated here rather than on the host so it never depends on the host's
/// openssl, and so the CLI can save it to the profile without reading it back
/// over the wire.
pub fn generate_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_normalise_to_celld_urls() {
        assert_eq!(normalise_bucket("my-cells"), "s3://my-cells");
        assert_eq!(normalise_bucket(" my-cells "), "s3://my-cells");
        assert_eq!(normalise_bucket("s3://my-cells/dgit"), "s3://my-cells/dgit");
        assert_eq!(normalise_bucket("gs://my-cells"), "gs://my-cells");
    }

    #[test]
    fn short_host_drops_user_domain_and_port() {
        assert_eq!(short_host("me@build-box.example.ts.net"), "build-box");
        assert_eq!(short_host("build-box"), "build-box");
        assert_eq!(short_host("me@build-box:22"), "build-box");
        assert_eq!(short_host("me@203.0.113.7"), "203.0.113.7");
    }

    #[test]
    fn tokens_are_48_hex_characters_and_not_repeated() {
        let a = generate_token();
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, generate_token());
    }

    #[test]
    fn unsafe_config_values_are_refused_before_they_reach_systemd() {
        assert!(reject_unsafe_value("bucket", "s3://my-cells/prefix").is_ok());
        assert!(reject_unsafe_value("region", "us-east-1").is_ok());
        assert!(reject_unsafe_value("endpoint", "https://t3.storage.dev").is_ok());

        assert!(reject_unsafe_value("bucket", "s3://has space").is_err());
        assert!(reject_unsafe_value("bucket", "s3://has\nnewline").is_err());
        assert!(reject_unsafe_value("region", "us%E").is_err());
        assert!(reject_unsafe_value("endpoint", "").is_err());
    }

    #[test]
    fn the_fencing_warning_names_every_store_that_fails() {
        for bad in ["MinIO", "Backblaze B2", "Hetzner", "DigitalOcean Spaces"] {
            assert!(FENCING_WARNING.contains(bad), "missing {bad}");
        }
        assert!(FENCING_WARNING.contains("https://celld.dev/docs/fencing"));
    }
}
