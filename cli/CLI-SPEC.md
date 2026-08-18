# nashgit CLI

A single Rust binary (`nashgit`) that turns the manual setup we did by hand into a
repeatable wizard, and then manages day-to-day use. Built standalone in this directory as
crate `nashgit-cli` (binary name `nashgit`); it will be merged into the nashgit workspace
repo later — no path deps outside this directory.

Audience: a developer with a Unix box they can SSH to, a Tailscale tailnet, and an
S3-compatible bucket. The CLI does everything else.

## Commands

### `nashgit setup`
Interactive wizard (also fully scriptable via flags for every prompt):

1. **Host** — prompt for an SSH destination (`user@host`); verify reachability, sudo,
   arch. Everything server-side happens over SSH; the CLI never needs to run on the host.
2. **Bucket** — prompt for provider (AWS S3 / Cloudflare R2 / Tigris) + bucket + region +
   endpoint + credentials (or "already in env on the host"). WARN, citing
   https://celld.dev/docs/fencing, that MinIO community, Backblaze B2, Hetzner, and DO
   Spaces do not implement the conditional writes celld requires and are not offered.
3. **Install** — over SSH: tailscale (installer script + `systemctl enable --now
   tailscaled`), celld (celld.dev/install.sh), node+npm+esbuild, git. Idempotent: skip
   what's present.
4. **Deploy dgit** — clone https://github.com/littledivy/dgit on the host, `npm install`,
   generate a random `GIT_TOKEN` (openssl rand -hex 24), patch `wrangler.celld.jsonc`
   (token, site name/owner from prompts), `celld deploy`, write `/etc/systemd/system/
   celld.service` (loopback listen 127.0.0.1:8080, EnvironmentFile with AWS creds +
   region, Restart=always), start it, smoke-test `curl 127.0.0.1:8080` == 200.
5. **Tailnet** — `tailscale up`; relay the auth URL to the user and wait for the node to
   come up. Then `tailscale serve --bg --https=443 http://127.0.0.1:8080` (and, if the
   viewer is installed, `--https=8443` → 8090). Print the final HTTPS URLs.
6. **Verify** — end-to-end: temp repo, push with token, clone anonymously, delete.
7. **Profile** — write everything non-secret to the local profile store (below); the
   GIT_TOKEN goes into the profile file chmod 600.

### `nashgit use <profile>` / `nashgit profiles`
Profile store at `~/.config/nashgit/config.toml`: named servers (`url`, `ssh`, `token`),
one marked active. `use` selects the active one; all other commands honor
`--profile <name>` to override. This is the "select it" surface — multiple deployments
(personal, team, client) coexist.

### Repo commands (against the active profile, dgit's HTTP API)
- `nashgit init [name]` — jj-first creation: version the current directory.
  Create the repository on the server (PUT `/name/config`), initialise a working
  copy if the folder has none (`jj git init --colocate` when jj is on PATH,
  `git init -b main` otherwise; `--git`/`--jj` override), wire `origin` with the
  token via the credential helper, commit anything uncommitted, push. Default
  name = directory name. Re-running is safe. `--no-push` stops before the push.
- `nashgit new <name> [--private] [--desc ...] [--section ...]` — dgit creates on first
  push, so this pushes an empty commit is WRONG — instead: PUT `/name/config` with the
  token to create/describe, then if run inside a git worktree, add `origin` with the
  token embedded for pushes (`https://x:TOKEN@host/name.git`) stored via git credential
  helper, not in the remote URL — use `git credential approve`.
- `nashgit ls` — scrape the index page (dgit has no JSON list endpoint; parse the HTML
  anchor list, tolerate markup drift with a loose regex).
- `nashgit clone <name> [dir]`, `nashgit rm <name>` (DELETE, with a y/N confirm),
  `nashgit gc <name>` (POST /gc), `nashgit desc <name> ...` (PUT /config).
- `nashgit remote [name]` — wire `origin` in the cwd repo (default name = dir name).
- `nashgit token` — print the push token for the active profile (for CI use).

### jj (Jujutsu) awareness
- Detect the working copy from the directory layout alone: plain git (`.git`),
  colocated jj (`.jj` + `.git`), jj-only (`.jj`). Colocated counts as jj.
- In a jj repository use `jj git remote add` / `set-url`, never `git remote`.
- git-credential storage still applies: jj asks git's credential helpers, so
  `git credential approve` covers both.
- `--jj` on `new` and `clone` colocates jj on top of the git working copy;
  `NASHGIT_JJ=1` makes that the default. An explicit `--jj` with no jj on PATH
  is an error; the env-var default degrades to a warning.
- README carries a "Using with jj" section.
- Tests: detection via directory-layout fixtures (plain git / colocated /
  jj-only); jj is shelled out behind a shim seam (`NASHGIT_JJ_BIN`,
  `NASHGIT_JJ_AVAILABLE`) so no test needs jj installed.

### `nashgit doctor`
Checks, each one line, ✓/✗: profile exists, server reachable, TLS cert valid, token
accepted (auth probe), tailscale identity headers present, celld service active (via
SSH if configured), bucket reachable from host, viewer up (if configured).

## Implementation constraints

- Rust, clap (derive), edition 2024. Prompts via `dialoguer` or equivalent
  well-maintained crate; spinners fine, no TUI framework.
- SSH = shell out to the system `ssh`/`scp` (respects user's config/agent); never an SSH
  library. All remote scripts are idempotent and `set -e`.
- Secrets never in argv of remote commands where avoidable (pipe via stdin), never
  printed unless explicitly requested (`nashgit token`).
- Every command supports `--json` for agent use; human output stays terse.
- `--help` for every command is written for someone who has never seen celld: one
  paragraph of what/why at the top level explaining the architecture (dgit worker on
  celld, bucket is the store, tailnet is the perimeter).
- Tests (`cargo nextest run`): profile store round-trip, index-page parse against a
  saved dgit HTML fixture, remote-script idempotency (run the generated install script
  twice against a fake `ssh` shim recording invocations), doctor output shape, jj
  detection via directory-layout fixtures, `comments` against a canned JSON fixture on
  a loopback listener. No test may require network or a real host.

## Plans + plannotator

Plans are markdown files under `plans/` in a repo (a nashgit convention; the viewer
renders them). CLI support:

- `nashgit plan new <title>` — create `plans/<slug>.md` from a minimal template in the
  cwd repo.
- `nashgit annotate <plans/file.md>` — shell out to a locally installed `plannotator`
  binary against the file if present (`which plannotator`), else print install pointer.
  When the active profile has a viewer URL configured, print the plan's viewer URL too.
- `nashgit comments <file> [--branch ...] [--since RFC3339] [--repo ...]` — GET
  the viewer's `/:repo/comments` JSON endpoint (`viewer_url` from the active
  profile; a clear error when it is unset). `--repo` defaults to the name
  `origin` points at. `--json` passes the viewer's answer through untouched.
  Tested against a canned JSON fixture served by a local test listener.
- Nothing else: annotation feedback flows through the viewer's comment API, not the CLI.

## Non-goals

Windows hosts, non-systemd hosts, bucket creation/IAM provisioning (print the exact
aws/wrangler commands for the user instead — provider docs drift too fast to automate),
managing the viewer's code (only its service + serve wiring).
