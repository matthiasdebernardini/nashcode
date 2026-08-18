//! The shell scripts nashgit runs on the host.
//!
//! Every function here is pure: parameters in, script text out. That keeps the
//! interesting logic testable without a host, and it lets `--dry-run` print
//! exactly what would run.
//!
//! Three rules hold for all of them:
//!
//! * **`set -eu`.** A step that fails stops the script rather than leaving a
//!   half-configured box behind.
//! * **Idempotent.** Every script may be run again on a box it already
//!   configured. Installs check first, `systemctl enable --now` is naturally
//!   repeatable, and the Wrangler config is rewritten from its own parsed
//!   contents rather than appended to.
//! * **Secrets in the body, never in `argv`.** The script is piped to
//!   `bash -s` over stdin, so tokens and bucket credentials never appear in
//!   the host's process list. Where a secret must reach another program, it
//!   goes through a 0600 file or an environment variable, not a command line.

use crate::profile::Profile;

/// Quote a value for safe interpolation into single-quoted shell context.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Prelude shared by every script: strict mode, celld's install path, and a
/// root-runner that works whether we are root already or must use sudo.
const PRELUDE: &str = r#"set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"
export DEBIAN_FRONTEND=noninteractive
if [ "$(id -u)" = 0 ]; then
  as_root() { "$@"; }
  as_root_env() { "$@"; }
else
  as_root() { sudo -n "$@"; }
  as_root_env() { sudo -n -E "$@"; }
fi
have() { command -v "$1" >/dev/null 2>&1; }
log() { printf 'nashgit: %s\n' "$*"; }
"#;

/// What `setup` needs to know about the host before it touches anything.
///
/// Emits `KEY=value` lines; see [`crate::ssh::parse_kv`].
pub fn preflight_script() -> String {
    format!(
        r#"{PRELUDE}
echo "NASHGIT_OS=$(uname -s)"
echo "NASHGIT_ARCH=$(uname -m)"
echo "NASHGIT_USER=$(id -un)"
echo "NASHGIT_HOME=$HOME"
if have systemctl; then echo "NASHGIT_SYSTEMD=yes"; else echo "NASHGIT_SYSTEMD=no"; fi
if [ "$(id -u)" = 0 ]; then echo "NASHGIT_ROOT=root"
elif sudo -n true 2>/dev/null; then echo "NASHGIT_ROOT=sudo"
elif have sudo; then echo "NASHGIT_ROOT=sudo-password"
else echo "NASHGIT_ROOT=none"; fi
echo "NASHGIT_DISTRO=$( . /etc/os-release 2>/dev/null; echo "${{ID:-unknown}}" )"
for b in git curl node npm esbuild tailscale celld; do
  echo "NASHGIT_HAS_$(echo "$b" | tr 'a-z-' 'A-Z_')=$(command -v "$b" 2>/dev/null || echo '')"
done
"#
    )
}

/// Install tailscale, celld, node+npm+esbuild, and git. Skips what is present.
///
/// The version of this script is what the idempotency test runs twice: on the
/// second run every branch takes the "present" path and nothing is installed.
pub fn install_script() -> String {
    format!(
        r#"{PRELUDE}
PM=""
if have apt-get; then PM=apt
elif have dnf; then PM=dnf
elif have yum; then PM=yum
elif have pacman; then PM=pacman
elif have apk; then PM=apk
fi

APT_REFRESHED=0
pkg() {{
  case "$PM" in
    apt)
      if [ "$APT_REFRESHED" = 0 ]; then as_root apt-get update -qq; APT_REFRESHED=1; fi
      as_root apt-get install -y -qq "$@" ;;
    dnf)    as_root dnf install -y -q "$@" ;;
    yum)    as_root yum install -y -q "$@" ;;
    pacman) as_root pacman -Sy --noconfirm --needed "$@" ;;
    apk)    as_root apk add --no-cache "$@" ;;
    *)      log "no supported package manager found; install these by hand: $*"; exit 1 ;;
  esac
}}

# --- git, curl, CA roots ---------------------------------------------------
if have git; then log "skip git (present)"; else log "install git"; pkg git; fi
if have curl; then log "skip curl (present)"; else log "install curl"; pkg curl ca-certificates; fi

# --- node + npm ------------------------------------------------------------
# celld's deploy step bundles the Worker with esbuild, which needs a modern
# node. Debian and Ubuntu ship one too old, so those use NodeSource.
if have node && have npm; then
  log "skip node (present: $(node --version 2>/dev/null || echo unknown))"
else
  log "install node"
  case "$PM" in
    apt)
      curl -fsSL https://deb.nodesource.com/setup_22.x | as_root_env bash -
      pkg nodejs ;;
    dnf|yum|pacman|apk)
      pkg nodejs npm ;;
    *)
      log "install node 20 or newer by hand, then run setup again"; exit 1 ;;
  esac
fi

# --- esbuild ---------------------------------------------------------------
if have esbuild; then
  log "skip esbuild (present)"
else
  log "install esbuild"
  as_root "$(command -v npm)" install -g --no-fund --no-audit esbuild
fi

# --- tailscale -------------------------------------------------------------
if have tailscale; then
  log "skip tailscale (present)"
else
  log "install tailscale"
  curl -fsSL https://tailscale.com/install.sh | as_root sh
fi
if have systemctl; then
  log "enable tailscaled"
  as_root systemctl enable --now tailscaled
fi

# --- celld -----------------------------------------------------------------
if have celld; then
  log "skip celld (present)"
else
  log "install celld"
  curl -fsSL https://celld.dev/install.sh | sh
  export PATH="$HOME/.local/bin:$PATH"
fi

echo "NASHGIT_CELLD=$(command -v celld 2>/dev/null || echo '')"
echo "NASHGIT_ESBUILD=$(command -v esbuild 2>/dev/null || echo '')"
echo "NASHGIT_NODE=$(command -v node 2>/dev/null || echo '')"
echo "NASHGIT_TAILSCALE=$(command -v tailscale 2>/dev/null || echo '')"
"#
    )
}

/// Everything the deploy and service steps need. Non-secret fields are also
/// what gets written into the local profile.
#[derive(Debug, Clone)]
pub struct Deploy {
    /// celld bucket URL, e.g. `s3://my-cells` or `s3://my-cells/dgit`.
    pub bucket: String,
    /// S3-compatible endpoint. `None` for Amazon S3 and for `gs://` buckets.
    pub endpoint: Option<String>,
    pub region: String,
    /// `None` when the host already holds credentials (instance role, an
    /// existing profile, or ADC for a `gs://` bucket).
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,

    pub site_name: String,
    pub site_desc: String,
    pub site_owner: String,
    /// dgit's push password.
    pub token: String,

    /// Where dgit's checkout lives on the host.
    pub dgit_dir: String,
    /// Unix user the celld service runs as.
    pub service_user: String,
    /// Loopback address celld serves dgit on.
    pub listen: String,
    /// Absolute path of the celld binary on the host.
    pub celld_bin: String,
    /// Absolute path of esbuild, pinned into the service environment.
    pub esbuild_bin: String,

    /// Wire `--https=8443` to the viewer as well.
    pub viewer: bool,
    /// Loopback port the viewer listens on.
    pub viewer_port: u16,
    pub https_port: u16,
    pub viewer_https_port: u16,
}

impl Deploy {
    fn celld_flags(&self) -> String {
        let mut s = format!("--bucket {}", sq(&self.bucket));
        if let Some(ep) = &self.endpoint {
            s.push_str(&format!(" --endpoint {}", sq(ep)));
        }
        if !self.region.is_empty() {
            s.push_str(&format!(" --region {}", sq(&self.region)));
        }
        s
    }

    /// Flags as they appear in the systemd `ExecStart` line: no shell quoting,
    /// because systemd does not run a shell.
    fn celld_flags_unquoted(&self) -> String {
        let mut s = format!("--bucket {}", self.bucket);
        if let Some(ep) = &self.endpoint {
            s.push_str(&format!(" --endpoint {ep}"));
        }
        if !self.region.is_empty() {
            s.push_str(&format!(" --region {}", self.region));
        }
        s
    }

    /// The `KEY=value` body of `/etc/celld/celld.env`.
    pub fn env_file(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("CELLD_BUCKET={}", self.bucket));
        if let Some(ep) = &self.endpoint {
            lines.push(format!("S3_ENDPOINT={ep}"));
        }
        if !self.region.is_empty() {
            lines.push(format!("AWS_REGION={}", self.region));
            lines.push(format!("AWS_DEFAULT_REGION={}", self.region));
        }
        if let Some(k) = &self.access_key_id {
            lines.push(format!("AWS_ACCESS_KEY_ID={k}"));
        }
        if let Some(k) = &self.secret_access_key {
            lines.push(format!("AWS_SECRET_ACCESS_KEY={k}"));
        }
        if let Some(k) = &self.session_token {
            lines.push(format!("AWS_SESSION_TOKEN={k}"));
        }
        if !self.esbuild_bin.is_empty() {
            lines.push(format!("CELLD_ESBUILD={}", self.esbuild_bin));
        }
        lines.join("\n") + "\n"
    }

    /// The systemd unit.
    ///
    /// celld binds loopback only: the tailnet is the perimeter, and
    /// `tailscale serve` is the one thing allowed to reach it.
    pub fn unit_file(&self) -> String {
        format!(
            "[Unit]\n\
             Description=celld (nashgit: dgit on celld)\n\
             Documentation=https://celld.dev/docs\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             User={user}\n\
             StateDirectory=celld\n\
             Environment=CELLD_WATCH=/var/lib/celld\n\
             Environment=CELLD_V8_HEAP_LIMIT_MB=4096\n\
             Environment=CELLD_LTX_DURABILITY_TIMEOUT_SECS=180\n\
             Environment=CELLD_SHUTDOWN_DRAIN_MS=25000\n\
             EnvironmentFile=/etc/celld/celld.env\n\
             ExecStart={celld} {flags} --listen {listen}\n\
             Restart=always\n\
             RestartSec=2\n\
             KillSignal=SIGTERM\n\
             TimeoutStopSec=40\n\
             NoNewPrivileges=true\n\
             PrivateTmp=true\n\
             ProtectSystem=full\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            user = self.service_user,
            celld = self.celld_bin,
            flags = self.celld_flags_unquoted(),
            listen = self.listen,
        )
    }

    /// The `vars` block patched into `wrangler.celld.jsonc`.
    pub fn wrangler_vars(&self) -> serde_json::Value {
        serde_json::json!({
            "SITE_NAME": self.site_name,
            "SITE_DESC": self.site_desc,
            "SITE_OWNER": self.site_owner,
            "GIT_TOKEN": self.token,
        })
    }
}

/// A tiny JSONC reader, run on the host with the node we just installed.
///
/// dgit ships `wrangler.celld.jsonc` with comments, so `JSON.parse` alone will
/// not read it. This strips comments and trailing commas, merges our four
/// `vars` over whatever is there, and writes the file back. Every other key
/// survives untouched, so an upstream addition is not lost on re-run — and
/// re-running produces the same file, which is what makes the step idempotent.
const PATCH_WRANGLER_JS: &str = r#"const fs = require('fs');
const [file, varsFile] = process.argv.slice(2);
function strip(s) {
  let out = '', i = 0;
  while (i < s.length) {
    const c = s[i];
    if (c === '"') {
      out += c; i++;
      while (i < s.length) {
        if (s[i] === '\\') { out += s[i] + (s[i + 1] || ''); i += 2; continue; }
        out += s[i];
        if (s[i] === '"') { i++; break; }
        i++;
      }
      continue;
    }
    if (c === '/' && s[i + 1] === '/') { while (i < s.length && s[i] !== '\n') i++; continue; }
    if (c === '/' && s[i + 1] === '*') { i += 2; while (i < s.length && !(s[i] === '*' && s[i + 1] === '/')) i++; i += 2; continue; }
    out += c; i++;
  }
  return out;
}
const cfg = JSON.parse(strip(fs.readFileSync(file, 'utf8')).replace(/,(\s*[}\]])/g, '$1'));
cfg.vars = Object.assign({}, cfg.vars, JSON.parse(fs.readFileSync(varsFile, 'utf8')));
fs.writeFileSync(file, JSON.stringify(cfg, null, 2) + '\n');
console.log('nashgit: wrangler.celld.jsonc vars set (' + Object.keys(cfg.vars).join(', ') + ')');
"#;

/// Clone dgit, install its one dependency, patch its config, deploy to celld.
pub fn deploy_script(d: &Deploy) -> String {
    let vars = serde_json::to_string(&d.wrangler_vars()).unwrap_or_else(|_| "{}".into());
    format!(
        r#"{PRELUDE}
umask 077
DGIT_DIR={dir}

if [ -d "$DGIT_DIR/.git" ]; then
  log "update dgit checkout"
  git -C "$DGIT_DIR" fetch --quiet --depth 1 origin main
  # --force: the previous deploy left wrangler.celld.jsonc patched, so the
  # tree is dirty and a plain checkout refuses. The patch below rebuilds the
  # file from upstream, invites included, so nothing is lost.
  git -C "$DGIT_DIR" checkout --quiet --force --detach FETCH_HEAD
else
  log "clone dgit"
  git clone --quiet --depth 1 https://github.com/littledivy/dgit "$DGIT_DIR"
fi
cd "$DGIT_DIR"

log "npm install"
npm install --no-audit --no-fund --silent

log "patch wrangler.celld.jsonc"
VARS_FILE="$(mktemp "$DGIT_DIR/.nashgit-vars.XXXXXX")"
PATCH_FILE="$(mktemp "$DGIT_DIR/.nashgit-patch.XXXXXX")"
trap 'rm -f "$VARS_FILE" "$PATCH_FILE"' EXIT
cat >"$PATCH_FILE" <<'NASHGIT_PATCH_EOF'
{patch}
NASHGIT_PATCH_EOF
cat >"$VARS_FILE" <<'NASHGIT_VARS_EOF'
{vars}
NASHGIT_VARS_EOF
node "$PATCH_FILE" "$DGIT_DIR/wrangler.celld.jsonc" "$VARS_FILE"

# A re-run starts from a fresh upstream file, which would drop the invited
# tokens; restore GIT_TOKENS from the mapping the invites live in.
INVITES="$HOME/git-invites.toml"
if [ -s "$INVITES" ]; then
  TOKENS="$(sed -n 's/^[^=]* = "\(.*\)"$/\1/p' "$INVITES" | paste -sd, -)"
  printf '{{"GIT_TOKENS": "%s"}}' "$TOKENS" >"$VARS_FILE"
  node "$PATCH_FILE" "$DGIT_DIR/wrangler.celld.jsonc" "$VARS_FILE"
  log "restored GIT_TOKENS from $INVITES"
fi
chmod 600 "$DGIT_DIR/wrangler.celld.jsonc"
rm -f "$VARS_FILE" "$PATCH_FILE"
trap - EXIT

log "celld deploy"
# Credentials reach celld through the environment of this shell only; they are
# written to disk once, in the 0600 EnvironmentFile the service reads.
{creds_exports}
celld deploy wrangler.celld.jsonc {flags}
log "deploy done"
"#,
        dir = sq(&d.dgit_dir),
        vars = vars,
        patch = PATCH_WRANGLER_JS,
        creds_exports = creds_exports(d),
        flags = d.celld_flags(),
    )
}

fn creds_exports(d: &Deploy) -> String {
    let mut out = String::new();
    if let Some(k) = &d.access_key_id {
        out.push_str(&format!("export AWS_ACCESS_KEY_ID={}\n", sq(k)));
    }
    if let Some(k) = &d.secret_access_key {
        out.push_str(&format!("export AWS_SECRET_ACCESS_KEY={}\n", sq(k)));
    }
    if let Some(k) = &d.session_token {
        out.push_str(&format!("export AWS_SESSION_TOKEN={}\n", sq(k)));
    }
    if !d.region.is_empty() {
        out.push_str(&format!("export AWS_REGION={}\n", sq(&d.region)));
        out.push_str(&format!("export AWS_DEFAULT_REGION={}\n", sq(&d.region)));
    }
    if let Some(ep) = &d.endpoint {
        out.push_str(&format!("export S3_ENDPOINT={}\n", sq(ep)));
    }
    if !d.esbuild_bin.is_empty() {
        out.push_str(&format!("export CELLD_ESBUILD={}\n", sq(&d.esbuild_bin)));
    }
    out
}

/// Write the EnvironmentFile and the unit, start celld, and smoke-test it.
pub fn service_script(d: &Deploy) -> String {
    format!(
        r#"{PRELUDE}
umask 077

log "write /etc/celld/celld.env"
as_root install -d -m 0750 /etc/celld
TMP_ENV="$(mktemp)"
chmod 600 "$TMP_ENV"
trap 'rm -f "$TMP_ENV"' EXIT
cat >"$TMP_ENV" <<'NASHGIT_ENV_EOF'
{env}
NASHGIT_ENV_EOF
as_root install -m 0600 -o root -g root "$TMP_ENV" /etc/celld/celld.env
rm -f "$TMP_ENV"
trap - EXIT

log "write /etc/systemd/system/celld.service"
TMP_UNIT="$(mktemp)"
cat >"$TMP_UNIT" <<'NASHGIT_UNIT_EOF'
{unit}
NASHGIT_UNIT_EOF
as_root install -m 0644 -o root -g root "$TMP_UNIT" /etc/systemd/system/celld.service
rm -f "$TMP_UNIT"

as_root systemctl daemon-reload
as_root systemctl enable celld
as_root systemctl restart celld

log "wait for celld on {listen}"
ok=0
for i in $(seq 1 60); do
  code="$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 5 "http://{listen}/" || true)"
  if [ "$code" = "200" ]; then ok=1; break; fi
  sleep 2
done
if [ "$ok" != 1 ]; then
  log "celld did not answer 200 on http://{listen}/ — last 40 journal lines:"
  as_root journalctl -u celld -n 40 --no-pager || true
  exit 1
fi
log "celld is serving dgit on http://{listen}/"
echo "NASHGIT_SERVICE=active"
"#,
        env = d.env_file().trim_end(),
        unit = d.unit_file().trim_end(),
        listen = d.listen,
    )
}

/// Bring the node onto the tailnet.
///
/// With an auth key the whole thing is one non-interactive call. Without one,
/// `tailscale up` blocks on a browser login, so it is started detached and the
/// login URL is scraped out of its log and handed back to the user.
pub fn tailscale_up_script(auth_key: Option<&str>) -> String {
    let key_block = match auth_key {
        Some(k) => format!(
            r#"KEYFILE="$(mktemp)"
chmod 600 "$KEYFILE"
printf '%s' {key} >"$KEYFILE"
as_root tailscale up --auth-key="file:$KEYFILE" --timeout=120s
rm -f "$KEYFILE"
echo "NASHGIT_AUTH_URL="
"#,
            key = sq(k)
        ),
        None => r#"LOG="$(mktemp /tmp/nashgit-tsup.XXXXXX)"
as_root nohup tailscale up --timeout=600s >"$LOG" 2>&1 </dev/null &
url=""
for i in $(seq 1 60); do
  url="$(grep -om1 'https://login\.tailscale\.com/[A-Za-z0-9/_.-]*' "$LOG" 2>/dev/null || true)"
  [ -n "$url" ] && break
  sleep 1
done
echo "NASHGIT_AUTH_URL=$url"
"#
        .to_string(),
    };
    format!(
        r#"{PRELUDE}
state="$(tailscale status --json 2>/dev/null | node -e {reader} 2>/dev/null || echo Unknown)"
if [ "$state" = "Running" ]; then
  log "skip tailscale up (already Running)"
  echo "NASHGIT_AUTH_URL="
  echo "NASHGIT_TS_STATE=Running"
  exit 0
fi
log "tailscale up"
{key_block}
echo "NASHGIT_TS_STATE=$state"
"#,
        reader = sq(BACKEND_STATE_JS),
        key_block = key_block,
    )
}

/// Read one field out of `tailscale status --json` without needing `jq`.
const BACKEND_STATE_JS: &str = r#"let d="";process.stdin.on("data",c=>d+=c).on("end",()=>{try{console.log(JSON.parse(d).BackendState||"Unknown")}catch(e){console.log("Unknown")}})"#;
const DNS_NAME_JS: &str = r#"let d="";process.stdin.on("data",c=>d+=c).on("end",()=>{try{const j=JSON.parse(d);console.log(String((j.Self&&j.Self.DNSName)||"").replace(/\.$/,""))}catch(e){console.log("")}})"#;

/// Poll the tailnet state and report the node's own DNS name.
pub fn tailscale_status_script() -> String {
    format!(
        r#"{PRELUDE}
raw="$(tailscale status --json 2>/dev/null || echo '{{}}')"
echo "NASHGIT_TS_STATE=$(printf '%s' "$raw" | node -e {state} 2>/dev/null || echo Unknown)"
echo "NASHGIT_DNSNAME=$(printf '%s' "$raw" | node -e {dns} 2>/dev/null || echo '')"
"#,
        state = sq(BACKEND_STATE_JS),
        dns = sq(DNS_NAME_JS),
    )
}

/// Put dgit (and optionally the viewer) behind the tailnet's HTTPS listener.
///
/// `tailscale serve` also injects the `Tailscale-User-Login` identity headers
/// the viewer trusts, which is why the viewer is never exposed directly.
pub fn serve_script(d: &Deploy) -> String {
    let viewer = if d.viewer {
        format!(
            "log \"serve :{vp} -> 127.0.0.1:{lp}\"\nas_root tailscale serve --bg --https={vp} http://127.0.0.1:{lp}\n",
            vp = d.viewer_https_port,
            lp = d.viewer_port
        )
    } else {
        String::new()
    };
    format!(
        r#"{PRELUDE}
log "serve :{hp} -> http://{listen}"
as_root tailscale serve --bg --https={hp} http://{listen}
{viewer}
as_root tailscale serve status || true
"#,
        hp = d.https_port,
        listen = d.listen,
        viewer = viewer,
    )
}

/// End-to-end proof: create a repository by pushing to it, read it back with no
/// credentials at all, then delete it.
///
/// It runs against the loopback listener, so it tests dgit and celld and the
/// bucket without depending on the tailnet being up yet.
///
/// The token is exported once and referenced from a credential helper and a
/// curl config file, never passed as an argument, so it stays out of `ps`.
pub fn verify_script(token: &str, listen: &str) -> String {
    format!(
        r#"{PRELUDE}
export NASHGIT_TOKEN={token}
BASE="http://{listen}"
NAME="nashgit-verify-$$-$(date +%s)"
WORK="$(mktemp -d)"
cleanup() {{
  cat >"$WORK/curlrc" <<'NASHGIT_CURLRC_EOF'
NASHGIT_CURLRC_EOF
  rm -rf "$WORK"
}}
trap cleanup EXIT

umask 077
printf 'user = "x:%s"\n' "$NASHGIT_TOKEN" >"$WORK/curlrc"

log "create a temporary repository by pushing to it"
mkdir -p "$WORK/src"
cd "$WORK/src"
git init --quiet -b main .
git config user.email "nashgit@localhost"
git config user.name "nashgit setup"
printf 'nashgit end-to-end check\n' >README.md
git add README.md
git -c commit.gpgsign=false commit --quiet -m "nashgit verify"
git -c credential.helper='!f() {{ echo username=x; echo "password=$NASHGIT_TOKEN"; }}; f' \
    push --quiet "$BASE/$NAME.git" main

log "clone it back with no credentials"
cd "$WORK"
git -c credential.helper= clone --quiet "$BASE/$NAME.git" "$WORK/back"
test -f "$WORK/back/README.md" || {{ log "anonymous clone did not return the file"; exit 1; }}

log "delete the temporary repository"
curl -fsS -K "$WORK/curlrc" -X DELETE "$BASE/$NAME" >/dev/null

log "verified: authenticated push, anonymous clone, delete"
echo "NASHGIT_VERIFY=ok"
echo "NASHGIT_VERIFY_REPO=$NAME"
"#,
        token = sq(token),
        listen = listen,
    )
}

// --- invites ----------------------------------------------------------------
//
// dgit reads extra push tokens, comma-separated, from the GIT_TOKENS var,
// alongside the main GIT_TOKEN. dgit only sees that flat list, so the CLI owns
// a name → token mapping on the host (`~/git-invites.toml`, 0600) and
// regenerates GIT_TOKENS from it on every change. The redeploy is the same
// mechanism `setup` uses: patch the vars into wrangler.celld.jsonc, `celld
// deploy`, restart the service.

/// `celld deploy` flags rebuilt from the saved profile.
fn profile_celld_flags(p: &Profile) -> String {
    let mut s = String::new();
    if let Some(b) = &p.bucket {
        s.push_str(&format!("--bucket {}", sq(b)));
    }
    if let Some(e) = &p.endpoint {
        s.push_str(&format!(" --endpoint {}", sq(e)));
    }
    if let Some(r) = &p.region {
        s.push_str(&format!(" --region {}", sq(r)));
    }
    s
}

/// Regenerate GIT_TOKENS from the mapping file and push it live.
///
/// Bucket credentials come from the 0600 EnvironmentFile the service already
/// reads; nothing secret travels for the deploy itself.
fn regen_git_tokens(p: &Profile) -> String {
    format!(
        r#"DGIT_DIR="$HOME/dgit"
TOKENS="$(sed -n 's/^[^=]* = "\(.*\)"$/\1/p' "$INVITES" | paste -sd, -)"
cd "$DGIT_DIR"
VARS_FILE="$(mktemp "$DGIT_DIR/.nashgit-vars.XXXXXX")"
printf '{{"GIT_TOKENS": "%s"}}' "$TOKENS" >"$VARS_FILE"
node - "$DGIT_DIR/wrangler.celld.jsonc" "$VARS_FILE" <<'NASHGIT_PATCH_EOF'
{patch}
NASHGIT_PATCH_EOF
chmod 600 "$DGIT_DIR/wrangler.celld.jsonc"
rm -f "$VARS_FILE"
log "celld deploy"
if as_root test -r /etc/celld/celld.env 2>/dev/null; then
  as_root sh -c 'set -a; . /etc/celld/celld.env; set +a; exec "$0" deploy wrangler.celld.jsonc "$@"' \
    "$(command -v celld || echo celld)" {flags}
else
  celld deploy wrangler.celld.jsonc {flags}
fi
as_root systemctl restart celld
log "celld restarted"
"#,
        patch = PATCH_WRANGLER_JS,
        flags = profile_celld_flags(p),
    )
}

/// Probe the loopback listener with `$NASHGIT_PROBE_TOKEN` until it answers.
///
/// The probe is dgit's usual read-only one: a PUT with a body that is not
/// JSON. 400 means the token was accepted (and nothing changed), 401 means it
/// was rejected. The token reaches curl through a 0600 config file, not argv.
fn probe_block(listen: &str) -> String {
    format!(
        r#"CURLRC="$(mktemp)"
chmod 600 "$CURLRC"
printf 'user = "x:%s"\n' "$NASHGIT_PROBE_TOKEN" >"$CURLRC"
code=000
for i in $(seq 1 30); do
  code="$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 5 -K "$CURLRC" -X PUT --data '~' "http://{listen}/nashgit-invite-probe/config" || true)"
  case "$code" in 400|401|403) break ;; esac
  sleep 1
done
rm -f "$CURLRC"
"#
    )
}

/// Add (or rotate) one person's push token and put it live.
pub fn invite_script(name: &str, token: &str, p: &Profile, listen: &str) -> String {
    format!(
        r#"{PRELUDE}
umask 077
INVITES="$HOME/git-invites.toml"
NAME={name}
export NASHGIT_PROBE_TOKEN={token}
touch "$INVITES"
chmod 600 "$INVITES"
TMP="$(mktemp)"
grep -v "^$NAME = " "$INVITES" >"$TMP" || true
printf '%s = "%s"\n' "$NAME" "$NASHGIT_PROBE_TOKEN" >>"$TMP"
install -m 0600 "$TMP" "$INVITES"
rm -f "$TMP"
log "recorded $NAME in $INVITES"
{regen}
{probe}echo "NASHGIT_INVITE_PROBE=$code"
if [ "$code" != 400 ]; then log "the new token was not accepted (HTTP $code)"; exit 1; fi
echo "NASHGIT_INVITE=ok"
"#,
        name = sq(name),
        token = sq(token),
        regen = regen_git_tokens(p),
        probe = probe_block(listen),
    )
}

/// Remove one person's token, put the shrunken set live, and prove the old
/// token no longer authenticates.
pub fn revoke_script(name: &str, p: &Profile, listen: &str) -> String {
    format!(
        r#"{PRELUDE}
umask 077
INVITES="$HOME/git-invites.toml"
NAME={name}
grep -q "^$NAME = " "$INVITES" 2>/dev/null || {{ log "no invite named $NAME"; exit 1; }}
export NASHGIT_PROBE_TOKEN="$(sed -n "s/^$NAME = \"\(.*\)\"$/\1/p" "$INVITES")"
TMP="$(mktemp)"
grep -v "^$NAME = " "$INVITES" >"$TMP" || true
install -m 0600 "$TMP" "$INVITES"
rm -f "$TMP"
log "removed $NAME from $INVITES"
{regen}
{probe}echo "NASHGIT_REVOKE_PROBE=$code"
if [ "$code" = 400 ]; then log "the revoked token is still accepted"; exit 1; fi
echo "NASHGIT_REVOKE=ok"
"#,
        name = sq(name),
        regen = regen_git_tokens(p),
        probe = probe_block(listen),
    )
}

/// The invited names, one `NASHGIT_INVITE_NAME=` line each. Never the tokens.
pub fn invites_list_script() -> String {
    format!(
        r#"{PRELUDE}
INVITES="$HOME/git-invites.toml"
if [ -f "$INVITES" ]; then
  sed -n 's/^\([^ =]*\) = .*/NASHGIT_INVITE_NAME=\1/p' "$INVITES"
fi
echo "NASHGIT_LIST=ok"
"#
    )
}

/// The host-side half of `nashgit doctor`, as one round trip.
pub fn doctor_script(p: &Profile, listen: &str) -> String {
    let diagnose = match (&p.bucket, &p.region) {
        (Some(bucket), region) => {
            let mut flags = format!("--bucket {}", sq(bucket));
            if let Some(ep) = &p.endpoint {
                flags.push_str(&format!(" --endpoint {}", sq(ep)));
            }
            if let Some(r) = region {
                flags.push_str(&format!(" --region {}", sq(r)));
            }
            format!(
                r#"if as_root test -r /etc/celld/celld.env 2>/dev/null; then
  if as_root sh -c 'set -a; . /etc/celld/celld.env; set +a; exec "$0" diagnose "$@"' "$(command -v celld || echo celld)" {flags} >/dev/null 2>&1; then
    echo "NASHGIT_BUCKET=ok"
  else
    echo "NASHGIT_BUCKET=fail"
  fi
else
  echo "NASHGIT_BUCKET=skip"
fi
"#
            )
        }
        _ => "echo \"NASHGIT_BUCKET=skip\"\n".to_string(),
    };
    format!(
        r#"{PRELUDE}
echo "NASHGIT_SERVICE=$(systemctl is-active celld 2>/dev/null || echo inactive)"
echo "NASHGIT_LOOPBACK=$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 5 "http://{listen}/" 2>/dev/null || echo 000)"
echo "NASHGIT_TS_STATE=$(tailscale status --json 2>/dev/null | node -e {state} 2>/dev/null || echo Unknown)"
if tailscale serve status 2>/dev/null | grep -q '127.0.0.1:{port}'; then
  echo "NASHGIT_SERVE=ok"
else
  echo "NASHGIT_SERVE=fail"
fi
{diagnose}"#,
        listen = listen,
        port = listen.rsplit(':').next().unwrap_or("8080"),
        state = sq(BACKEND_STATE_JS),
        diagnose = diagnose,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Deploy {
        Deploy {
            bucket: "s3://example-cells".into(),
            endpoint: Some("https://example.invalid".into()),
            region: "auto".into(),
            access_key_id: Some("AKIAEXAMPLE".into()),
            secret_access_key: Some("s3cr3t".into()),
            session_token: None,
            site_name: "example-git".into(),
            site_desc: "a dgit host".into(),
            site_owner: "example".into(),
            token: "tok3n".into(),
            dgit_dir: "/home/u/dgit".into(),
            service_user: "u".into(),
            listen: "127.0.0.1:8080".into(),
            celld_bin: "/home/u/.local/bin/celld".into(),
            esbuild_bin: "/usr/local/bin/esbuild".into(),
            viewer: true,
            viewer_port: 8090,
            https_port: 443,
            viewer_https_port: 8443,
        }
    }

    #[test]
    fn sq_survives_an_embedded_quote() {
        assert_eq!(sq("it's"), r"'it'\''s'");
    }

    #[test]
    fn every_script_is_strict_mode() {
        for s in [
            preflight_script(),
            install_script(),
            deploy_script(&sample()),
            service_script(&sample()),
            serve_script(&sample()),
            tailscale_up_script(None),
            tailscale_status_script(),
            verify_script("t", "127.0.0.1:8080"),
            invite_script("alice", "tok3n", &Profile::default(), "127.0.0.1:8080"),
            revoke_script("alice", &Profile::default(), "127.0.0.1:8080"),
            invites_list_script(),
        ] {
            assert!(
                s.starts_with("set -euo pipefail\n"),
                "missing strict mode: {}",
                &s[..40]
            );
        }
    }

    #[test]
    fn unit_binds_loopback_and_reads_the_env_file() {
        let unit = sample().unit_file();
        assert!(unit.contains("--listen 127.0.0.1:8080"));
        assert!(unit.contains("EnvironmentFile=/etc/celld/celld.env"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("User=u"));
        // The drain bound must sit under the stop timeout, or systemd SIGKILLs
        // celld mid-handoff.
        assert!(unit.contains("CELLD_SHUTDOWN_DRAIN_MS=25000"));
        assert!(unit.contains("TimeoutStopSec=40"));
    }

    #[test]
    fn no_secret_reaches_a_command_line() {
        // The token and the secret key may appear in the script body (it is
        // piped over stdin) but never on a line that runs a program with them
        // as an argument.
        let s = verify_script("tok3n", "127.0.0.1:8080");
        assert!(!s.contains("-u x:tok3n"));
        assert!(!s.contains("--auth-key=tok3n"));
        let d = deploy_script(&sample());
        assert!(!d.contains("celld deploy wrangler.celld.jsonc --token"));
        assert!(d.contains("export AWS_SECRET_ACCESS_KEY='s3cr3t'"));
    }

    #[test]
    fn a_deploy_rerun_survives_the_dirty_wrangler_file_and_keeps_the_invites() {
        let s = deploy_script(&sample());
        // The previous deploy patched wrangler.celld.jsonc in the tree, so a
        // plain `checkout --detach` refuses and `set -e` kills the re-run.
        assert!(s.contains("checkout --quiet --force --detach FETCH_HEAD"), "{s}");
        // The force-checkout resets the file to upstream, so the invited
        // tokens must come back from the mapping.
        assert!(s.contains(r#"if [ -s "$INVITES" ]"#), "{s}");
        assert!(s.contains(r#"printf '{"GIT_TOKENS": "%s"}' "$TOKENS""#), "{s}");
        // Temp files are cleaned up even when a step dies.
        assert!(s.contains(r#"trap 'rm -f "$VARS_FILE" "$PATCH_FILE"' EXIT"#), "{s}");
    }

    #[test]
    fn invite_scripts_keep_the_token_off_command_lines_and_off_list_output() {
        let p = Profile::default();
        let s = invite_script("alice", "tok3n", &p, "127.0.0.1:8080");
        // The token lands in the script body (stdin) as a variable, and curl
        // reads it from a 0600 config file, never argv.
        assert!(s.contains("export NASHGIT_PROBE_TOKEN='tok3n'"));
        assert!(s.contains("-K \"$CURLRC\""));
        assert!(!s.contains("-u x:"));
        assert!(s.contains("install -m 0600"));

        // Revoke reads the token from the mapping; the CLI never sends it.
        let r = revoke_script("alice", &p, "127.0.0.1:8080");
        assert!(!r.contains("tok3n"));
        assert!(r.contains("NASHGIT_REVOKE_PROBE"));

        // The list script touches only the name column.
        let l = invites_list_script();
        assert!(l.contains("NASHGIT_INVITE_NAME=\\1"));
        assert!(!l.contains("NASHGIT_PROBE_TOKEN"));
    }

    #[test]
    fn env_file_carries_the_bucket_and_credentials() {
        let env = sample().env_file();
        assert!(env.contains("CELLD_BUCKET=s3://example-cells"));
        assert!(env.contains("S3_ENDPOINT=https://example.invalid"));
        assert!(env.contains("AWS_REGION=auto"));
        assert!(env.contains("AWS_SECRET_ACCESS_KEY=s3cr3t"));
        assert!(env.contains("CELLD_ESBUILD=/usr/local/bin/esbuild"));
    }

    #[test]
    fn serve_wires_the_viewer_only_when_asked() {
        let mut d = sample();
        assert!(serve_script(&d).contains("--https=8443 http://127.0.0.1:8090"));
        d.viewer = false;
        assert!(!serve_script(&d).contains("8443"));
        assert!(serve_script(&d).contains("--https=443 http://127.0.0.1:8080"));
    }

    #[test]
    fn wrangler_vars_carry_the_token_and_site_identity() {
        let v = sample().wrangler_vars();
        assert_eq!(v["GIT_TOKEN"], "tok3n");
        assert_eq!(v["SITE_NAME"], "example-git");
        assert_eq!(v["SITE_OWNER"], "example");
    }
}
