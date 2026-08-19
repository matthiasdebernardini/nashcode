# nashcode CLI

A single Rust binary (`nashcode`) that turns the manual setup we did by hand into a
repeatable wizard, and then manages day-to-day use. Built standalone in this directory as
crate `nashcode-cli` (binary name `nashcode`); it will be merged into the nashcode workspace
repo later — no path deps outside this directory.

Audience: a developer with a Unix box they can SSH to, a Tailscale tailnet, and an
S3-compatible bucket. The CLI does everything else.

## Commands

### `nashcode setup`
A wizard with no questions in it: every answer is a flag, and a missing one is a
usage error naming the flag.

1. **Host** — `--host` names an SSH destination (`user@host`); verify reachability, sudo,
   arch. Everything server-side happens over SSH; the CLI never needs to run on the host.
2. **Bucket** — `--provider` (AWS S3 / Cloudflare R2 / Tigris) + bucket + region +
   endpoint + credentials (or `--creds-on-host`). WARN, citing
   https://celld.dev/docs/fencing, that MinIO community, Backblaze B2, Hetzner, and DO
   Spaces do not implement the conditional writes celld requires and are not offered.
3. **Install** — over SSH: tailscale (installer script + `systemctl enable --now
   tailscaled`), celld (celld.dev/install.sh), node+npm+esbuild, git. Idempotent: skip
   what's present.
4. **Deploy dgit** — clone https://github.com/littledivy/dgit on the host, `npm install`,
   generate a random `GIT_TOKEN` (openssl rand -hex 24), patch `wrangler.celld.jsonc`
   (token, site name/owner from the flags), `celld deploy`, write `/etc/systemd/system/
   celld.service` (loopback listen 127.0.0.1:8080, EnvironmentFile with AWS creds +
   region, Restart=always), start it, smoke-test `curl 127.0.0.1:8080` == 200.
5. **Tailnet** — `tailscale up`; relay the auth URL on stderr and wait for the node to
   come up. Then `tailscale serve --bg --https=443 http://127.0.0.1:8080` (and, if the
   viewer is installed, `--https=8443` → 8090). The final HTTPS URLs go in the result.
6. **Verify** — end-to-end: temp repo, push with token, clone anonymously, delete.
7. **Profile** — write everything non-secret to the local profile store (below); the
   GIT_TOKEN goes into the profile file chmod 600.

### `nashcode use <profile>` / `nashcode profiles`
Profile store at `~/.config/nashcode/config.toml`: named servers (`url`, `ssh`, `token`),
one marked active. `use` selects the active one; all other commands honor
`--profile <name>` to override (`doctor` excepted — see "Agent envelope"). This is the "select it" surface — multiple deployments
(personal, team, client) coexist.

### Repo commands (against the active profile, dgit's HTTP API)
- `nashcode init [name]` — jj-first creation: version the current directory.
  Create the repository on the server (PUT `/name/config`), initialise a working
  copy if the folder has none (`jj git init --colocate` when jj is on PATH,
  `git init -b main` otherwise; `--git`/`--jj` override), wire `origin` with the
  token via the credential helper, commit anything uncommitted, push. Default
  name = directory name. Re-running is safe. `--no-push` stops before the push.
- `nashcode new <name> [--private] [--desc ...] [--section ...]` — dgit creates on first
  push, so this pushes an empty commit is WRONG — instead: PUT `/name/config` with the
  token to create/describe, then if run inside a git worktree, add `origin` with the
  token embedded for pushes (`https://x:TOKEN@host/name.git`) stored via git credential
  helper, not in the remote URL — use `git credential approve`.
- `nashcode ls` — scrape the index page (dgit has no JSON list endpoint; parse the HTML
  anchor list, tolerate markup drift with a loose regex).
- `nashcode clone <name> [dir]`, `nashcode rm <name>` (DELETE, and `--yes` is required:
  nothing prompts),
  `nashcode gc <name>` (POST /gc), `nashcode desc <name> ...` (PUT /config).
- `nashcode remote [name]` — wire `origin` in the cwd repo (default name = dir name).
- `nashcode token` — print the push token for the active profile (for CI use).

### `nashcode invite` — per-person push access
dgit reads extra comma-separated push tokens from its GIT_TOKENS var, alongside
GIT_TOKEN. dgit only sees the flat list, so the CLI owns a name → token mapping
on the host (`~/git-invites.toml`, 0600) and regenerates GIT_TOKENS from it on
every change, applied the same way `setup` deploys: patch wrangler.celld.jsonc
vars, `celld deploy`, restart the service.

- `nashcode invite <name>` — generate a fresh token, update the mapping over SSH
  (profile's ssh dest), regenerate + redeploy, verify with an auth probe using
  the new token. Print a ready-to-send snippet: remote URL, the token, and the
  one-liner to store it via `git credential approve` — plus a reminder that
  network access is Tailscale's job (add to tailnet or share the node); the CLI
  does not touch Tailscale ACLs.
- `nashcode invite --list` — names only, never tokens.
- `nashcode invite --revoke <name>` — remove from the mapping, regenerate,
  redeploy, verify the revoked token now fails the auth probe.
- Idempotent: re-inviting an existing name rotates that person's token.
- Secrets discipline unchanged: token to the host via stdin, never argv; only
  `invite <name>` prints a token, once.
- Tests through the fake-ssh shim: invite writes the mapping + regenerates the
  var, revoke removes it, list never prints token material, and an envelope
  shape test for each. No network, no real host.

### jj (Jujutsu) awareness
- Detect the working copy from the directory layout alone: plain git (`.git`),
  colocated jj (`.jj` + `.git`), jj-only (`.jj`). Colocated counts as jj.
- In a jj repository use `jj git remote add` / `set-url`, never `git remote`.
- git-credential storage still applies: jj asks git's credential helpers, so
  `git credential approve` covers both.
- `--jj` on `new` and `clone` colocates jj on top of the git working copy;
  `NASHCODE_JJ=1` makes that the default. An explicit `--jj` with no jj on PATH
  is an error; the env-var default degrades to a warning.
- README carries a "Using with jj" section.
- Tests: detection via directory-layout fixtures (plain git / colocated /
  jj-only); jj is shelled out behind a shim seam (`NASHCODE_JJ_BIN`,
  `NASHCODE_JJ_AVAILABLE`) so no test needs jj installed.

### `nashcode doctor`
Checks, one entry each, pass/fail/skip: profile exists, server reachable, TLS cert valid, token
accepted (auth probe), tailscale identity headers present, celld service active (via
SSH if configured), bucket reachable from host, viewer up (if configured).

### `nashcode brain [repo]`
The viewer's `GET /brain?repo=` for one repo, digested for an agent's first read of a
session. Repo defaults to the name `origin` points at, the way `comments` resolves it.
The point is the transform: the raw stanza buries the useful facts under an activity
log, so the command reshapes it — branches with tip and CI state, the code-index
stanza (files, symbols, age), plan files, open-comment counts, latest architecture
submission, and the last five activity entries. The result is that digest, never the
raw stanza. A viewer that is down answers `status: unavailable` with the reason and
exits 0 — this runs from session-start hooks, and a dead viewer must not break a
session. Raw dump stays `curl /brain`; this command is the usable view.

### `nashcode grep [flags] PATTERN [path...]`
Grep for agents: the surface is ripgrep's, the answers come from the code index. An
LLM must be able to use it on reflex, so the syntax is the contract:

- **Flags mirror rg** where they matter: `-i`, `-n` (on by default), `-l`, `-C`/`-A`/
  `-B`, `-t rust`, `-g <glob>`, and the global `--json`. **Unknown flags are ignored,
  never an error** — whatever an agent types out of rg habit still runs. `-t`/`-g`
  filter paths on both the local and index sides.
- **Output is grep's**: one hit per line, `path:line:content`, so anything that
  parses grep parses this. Context lines use grep's `path-line-` form. Extras ride
  in `#` comment lines and in grouping; text and semantic hit lines stay pure. The
  one exception is the definitions block, whose lines carry a single trailing
  ` # kind, N refs, M callers` annotation — a parser that wants raw content strips
  from the last ` # `; `--json` carries the same facts unambiguously.
- **What the index adds, in fixed order:** a `# definitions:` block first (from
  `GET /:repo/code/find`), then the text hits, then — only when the text pass found
  nothing — a
  `# semantic (no exact match):` block from the embeddings. A `#` header names the
  indexed commit and its age.
- **Freshness is hybrid, not a warning.** Text hits come from a local `rg` run over
  the working tree when the command runs inside a checkout with `rg` on PATH — the
  tree an agent is editing is always fresher than the index. Definitions, counts,
  and semantic hits come from the index. Outside a checkout, or with no `rg`, the
  text pass falls back to the index's chunk search.
- **Exit codes are grep's** (0 hits, 1 none) — agents branch on them. A dead viewer
  degrades to plain local rg with one `#` line saying the index was unreachable;
  only "no checkout AND no index" is an error.
- Backend: `GET /:repo/code/find?q=` (see viewer SPEC "Code intelligence"); the CLI
  owns only flag translation, the local rg pass, and the merge.

## Agent envelope (agcli)

The CLI is agent-only: no human types it, only coding agents do. It is built on
`agcli` (crates.io, same author; source at `/Users/md/Projects/agcli`), not clap.
This section is the output contract; the sections above stay the behavioural
contract per command.

- **One JSON envelope on stdout, always.** agcli's envelope replaces the old
  human/`--json` dual mode. `--json` is no longer a declared flag; agcli parses
  an undeclared bare flag as a boolean and ignores it, so existing invocations
  keep working — verified by test, not assumed.
- **Typed exit codes**, mapped at the handler boundary: 0 success, 2 usage,
  3 not-found (profile missing, repo not on server), 4 auth (token rejected,
  401/403 from dgit or viewer), 5 api (dgit/viewer HTTP failures, ssh
  remote-script failures), 1 everything else. Agents branch on the code, not on
  error prose.
- **Every error carries a `fix`**: a runnable command or check, not prose. The
  existing bail/context wording survives as the message.
- **Reserved agcli flags** (`--select`, `--compact`, `--quiet`, `--yes`,
  `--dry-run`) come free on every command. `setup --dry-run` and `rm --yes`
  route through them. `--profile <name>` stays declared per command.
- **No interactivity.** dialoguer is gone. `setup` with missing answers is a
  usage error whose `fix` lists the missing flags; env fallbacks
  (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `TS_AUTHKEY`) stay. `rm`
  requires `--yes`; without it, usage error with the exact rerun line.
- **`brain` never returns an error.** A down or unconfigured viewer is an
  `ok: true` envelope with a `status` field saying so, exit 0. The SessionStart
  hook depends on this.
- **`annotate` launches plannotator by default** (the agent launches it FOR the
  human now); `--no-launch` restores inspect-only. Envelope reports file,
  plannotator path (null when absent), viewer URL.
- **`ls` and `comments` emit bounded lists** (`{items, count, total, truncated,
  fields}`), which advertises `--select` for free.
- **`doctor`** is agcli's built-in doctor wrapping the existing checks; a
  failing check carries its typed exit code (e.g. auth). Skipped never passes.
- **`grep` bypasses the envelope.** Raw rg-format `path:line:content` stdout
  and rg exit codes ARE its contract (see its section above); it keeps its own
  flag surface, including its own `--json`, unchanged. agcli must support a
  raw-stdout command; if it does not, that feature lands in agcli first.
- **`next_actions`** on success, the useful ones only: `setup` → `init`,
  `doctor`; `init`/`new` → `plan new`, `index`, `brain`; `plan new` →
  `annotate`, `comments`; `annotate` → `comments --since=<now>`; `index` →
  `index --status`, `brain`; `comments` → the same call with `--since=` of the
  newest comment returned. Errors surface their `fix` as a runnable action.
- **Surface-only rewrite.** `cli.rs`, `main.rs`, `output.rs` change; command
  bodies and everything they call stay synchronous (blocking inside a handler
  is fine for a run-once CLI). ureq stays. Every `long_about` paragraph moves
  into the agcli command docs; zero prose is dropped.
- **Audit in tests**: `assert!(cli.audit().is_clean())`, plus one exit-code
  test per class and a stray-`--json`-is-ignored test.

## Implementation constraints

- Rust, agcli (see "Agent envelope" above), edition 2024. No prompt crate, no
  TUI framework — the CLI never prompts.
- SSH = shell out to the system `ssh`/`scp` (respects user's config/agent); never an SSH
  library. All remote scripts are idempotent and `set -e`.
- Secrets never in argv of remote commands where avoidable (pipe via stdin), never
  printed unless explicitly requested (`nashcode token`).
- Output is the agcli envelope (one JSON value on stdout); progress and
  warnings go to stderr, `--quiet` strips the chatter.
- Every command's description is written for a reader who has never seen celld, and the
  root tree opens with one paragraph of what/why explaining the architecture (dgit worker
  on celld, bucket is the store, tailnet is the perimeter).
- Tests (`cargo nextest run`): profile store round-trip, index-page parse against a
  saved dgit HTML fixture, remote-script idempotency (run the generated install script
  twice against a fake `ssh` shim recording invocations), doctor output shape, jj
  detection via directory-layout fixtures, `comments` against a canned JSON fixture on
  a loopback listener, and `annotate` against a fake plannotator on PATH plus a loopback
  listener: the argv it hands the child, the request line and wire payload it posts, that
  `--no-launch` launches nothing, and both exit codes (0 when there is nowhere to post,
  nonzero when a post is refused), and `brain` against a stanza captured from the viewer's
  own `GET /brain`: that the result is the digest rather than the stanza, that only the
  last five activity rows survive, and that every failure path — refused, hung, non-200,
  not JSON, no viewer configured — is one `status` field and exit 0. No test may require
  network or a real host.

## Plans + plannotator

Plans are markdown files under `plans/` in a repo (a nashcode convention; the viewer
renders them). CLI support:

- `nashcode plan new <title>` — create `plans/<slug>.md` from a minimal template in the
  cwd repo.
- `nashcode annotate <plans/file.md>` — shell out to a locally installed `plannotator`
  binary against the file if present (`which plannotator`), else print install pointer.
  When the active profile has a viewer URL configured, print the plan's viewer URL too.
  It launches with `--gate --json --result-file <scratch>/decision.json`, so plannotator
  shows an Approve button and writes one JSON record of what the human decided. That
  record becomes one comment on the viewer:

  | decision | comment body |
  |---|---|
  | `annotated` | the feedback |
  | `approved` with feedback | `Approved.`, a blank line, then the feedback |
  | `approved` alone | `Approved.` |
  | `dismissed` | nothing is posted |

  Approval posts too, because a polling agent has no other way to hear the loop close.
  The request is `POST /:repo/comments` with `{"branch", "file", "body"}`: whole-file,
  since the record carries no per-annotation anchors, and unauthored, since the viewer
  falls back to the caller's Tailscale identity. Any 2xx is success, and the stored `id`
  comes back when the answer carries one. Launching is the default — the agent opens
  plannotator for the human — and `--no-launch` is the inspect-only form.

  A comment is anchored to a branch, so the CLI needs a branch name it believes in. In a
  jj repository that name is the nearest bookmark, asked of jj — git's HEAD is detached
  in a colocated repo, or points at `jj/root`, and neither is a branch the server has
  heard of. The viewer rejects a branch its mirror does not know, so annotating a plan on
  a branch nobody pushed yet answers HTTP 400.

  Feedback is never lost. When the profile names no viewer, when the repository or the
  branch cannot be named, or when the plan sits outside the repository, the feedback
  comes back in the result with the reason it was not posted, and the command exits 0,
  because that is a configuration, not a fault. When the POST itself fails the command
  exits nonzero and the feedback rides in the error message: an envelope has no other
  place to carry it.
- `nashcode comments <file> [--branch ...] [--since RFC3339] [--repo ...]` — GET
  the viewer's `/:repo/comments` JSON endpoint (`viewer_url` from the active
  profile; a clear error when it is unset). `--repo` defaults to the name
  `origin` points at. The rows are the viewer's own objects, passed through key for
  key, inside a bounded list.
  Tested against a canned JSON fixture served by a local test listener.
- Nothing else: annotation feedback still lives in the viewer's comment API. `annotate` is
  only the courier that carries a local review there.

## Non-goals

Windows hosts, non-systemd hosts, bucket creation/IAM provisioning (print the exact
aws/wrangler commands for the user instead — provider docs drift too fast to automate),
managing the viewer's code (only its service + serve wiring).
