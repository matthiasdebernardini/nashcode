//! `nashcode invite` — per-person push access through dgit's GIT_TOKENS var.
//!
//! dgit sees only a flat, comma-separated token list, so the names live in a
//! mapping file the CLI owns on the host (`~/git-invites.toml`, 0600). Every
//! change regenerates GIT_TOKENS from that file, redeploys the Worker, and
//! restarts celld — then proves the change with an auth probe against the
//! loopback listener. Only `invite <name>` ever prints a token, once.

use super::Ctx;
use crate::cli::InviteArgs;
use crate::profile::Profile;
use crate::remote;
use crate::ssh::{Ssh, parse_kv};
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub fn run(ctx: &Ctx, args: &InviteArgs) -> Result<Value> {
    let (pname, p) = ctx.profile()?;
    if p.ssh.is_empty() {
        bail!(
            "profile `{pname}` has no ssh destination, and invites change the \
             server's token set over SSH"
        );
    }
    let ssh = Ssh::new(&p.ssh);
    if args.list {
        return list(ctx, &ssh);
    }
    if let Some(name) = &args.revoke {
        return revoke(ctx, &p, &ssh, name);
    }
    match &args.name {
        Some(name) => invite(ctx, &p, &ssh, name),
        None => bail!("say what to do: `nashcode invite <name>`, `--list`, or `--revoke <name>`"),
    }
}

/// Names may end up in shell patterns on the host, so the alphabet is small
/// and holds nothing a regex or the TOML-ish mapping file cares about.
fn valid_invite_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn listen(p: &Profile) -> String {
    format!("127.0.0.1:{}", p.listen_port())
}

fn invite(ctx: &Ctx, p: &Profile, ssh: &Ssh, name: &str) -> Result<Value> {
    if !valid_invite_name(name) {
        bail!(
            "`{name}` is not a usable invite name. Letters, digits, dash, and \
             underscore, starting with a letter or digit."
        );
    }
    let token = super::setup::generate_token();
    ctx.out.step(format!("rotating GIT_TOKENS on {} and redeploying", ssh.dest));
    let out = ssh
        .script(&remote::invite_script(name, &token, p, &listen(p)))?
        .require("invite")?;
    let kv = parse_kv(&out.stdout);
    let probe = kv
        .get("NASHCODE_INVITE_PROBE")
        .cloned()
        .unwrap_or_default();

    let host = p.url_host()?;
    let credential_line = format!(
        "printf 'protocol=https\\nhost=%s\\nusername=%s\\npassword=%s\\n\\n' \
         '{host}' '{name}' '{token}' | git credential approve"
    );
    Ok(json!({
        "name": name,
        "token": token,
        "url": p.url,
        "credential_line": credential_line,
        "probe": probe,
        "verified": kv.get("NASHCODE_INVITE").map(String::as_str) == Some("ok"),
        "note": "The token is push access, not network access. Add them to your \
                 tailnet or share the node with them; nashcode does not manage \
                 Tailscale ACLs.",
    }))
}

fn revoke(ctx: &Ctx, p: &Profile, ssh: &Ssh, name: &str) -> Result<Value> {
    // Same gate as invite: the name lands in a grep pattern on the host, so
    // `--revoke '.*'` must die here, not wipe the mapping there.
    if !valid_invite_name(name) {
        bail!(
            "`{name}` is not a usable invite name. Letters, digits, dash, and \
             underscore, starting with a letter or digit. See `nashcode invite --list`."
        );
    }
    ctx.out.step(format!("removing `{name}` and redeploying"));
    let out = ssh.script(&remote::revoke_script(name, p, &listen(p)))?;
    let kv = parse_kv(&out.stdout);
    let probe = kv
        .get("NASHCODE_REVOKE_PROBE")
        .cloned()
        .unwrap_or_default();
    if !out.ok() {
        // The script only probes after the mapping change and redeploy went
        // through, so a probe code means the revocation itself succeeded —
        // say exactly which promise is broken.
        match probe.as_str() {
            "400" => bail!(
                "`{name}` was removed from the mapping and the redeploy ran, but the \
                 revoked token is STILL accepted (HTTP 400). Run `nashcode doctor` and \
                 check the celld service."
            ),
            "" => {
                out.require("revoke")?;
            }
            code => bail!(
                "`{name}` was revoked and the new token set deployed, but the check \
                 could not confirm it (HTTP {code} from the loopback probe). \
                 Re-run `nashcode doctor` to see why the server is not answering."
            ),
        }
    }
    Ok(json!({
        "revoked": name,
        "probe": probe,
        "verified": kv.get("NASHCODE_REVOKE").map(String::as_str) == Some("ok"),
    }))
}

fn list(_ctx: &Ctx, ssh: &Ssh) -> Result<Value> {
    let out = ssh.script(&remote::invites_list_script())?.require("list invites")?;
    // Names only — one NASHCODE_INVITE_NAME line each. parse_kv would collapse
    // the repeated key, so read them by hand.
    let names: Vec<String> = out
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("NASHCODE_INVITE_NAME="))
        .map(str::to_string)
        .collect();
    Ok(json!({ "names": names }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_names_are_a_small_safe_alphabet() {
        assert!(valid_invite_name("alice"));
        assert!(valid_invite_name("bob-2_x"));
        assert!(!valid_invite_name(""));
        assert!(!valid_invite_name("-lead"));
        assert!(!valid_invite_name("a.b")); // dot would be a regex metachar on the host
        assert!(!valid_invite_name("a b"));
        assert!(!valid_invite_name("a'b"));
    }
}
