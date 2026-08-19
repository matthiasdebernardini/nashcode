# Goal: migrate the nashcode CLI from clap to agcli

Repo: `/Users/md/Projects/nashcode` (Rust workspace; `cli/` = client crate
`nashcode-cli`, binary `nashcode`). The CLI is now agent-only: no human ever
types it, only Claude drives it. agcli (crates.io, `agcli = "0.14"`, source at
`/Users/md/Projects/agcli`, same author) is a framework for exactly this:
JSON-only envelopes, typed exit codes, HATEOAS `next_actions`, `--select` /
`--compact` / `--quiet` on every command, built-in `doctor` and a static
command-tree audit.

Read `/Users/md/Projects/agcli/README.md` and `examples/ops.rs` before writing
anything. Read `cli/src/cli.rs` in full — its `long_about` prose is the CLI's
documentation and must survive the migration.

## Why

Every ergonomic argument for clap (human output mode, prose `--help` for a
human reader) died when the CLI went agent-only. What an agent gains from
agcli: branch on typed exit codes instead of parsing error text, structured
errors with a `fix` hint for self-correction, `--select`/`--compact` on the
outputs it polls in loops (`comments`, `brain`), and `next_actions` so a fresh
agent discovers the next command without re-reading help. Dogfooding: nashcode
becomes agcli's flagship consumer; every rough edge found here is an agcli fix.

## Contract

### What is rewritten (the surface layer only)

- `cli/src/cli.rs` — clap derive structs → agcli `Command` declarations.
- `cli/src/main.rs` — clap dispatch → `AgentCli` + `#[tokio::main]`.
- `cli/src/output.rs` — `Out` shrinks: `line`/`emit`/`json` die (the envelope
  replaces them); `step` and `warn` stay as stderr progress (agents read
  stderr; `--quiet` in agcli strips `next_actions`, our `step` suppression can
  ride the same flag via `req`).

### What is NOT rewritten

Command bodies and everything they call: `commands/*.rs` logic, `api.rs`,
`remote.rs`, `ssh.rs`, `vcs.rs`, `profile.rs`, `index_page.rs`, `timefmt.rs`.
Handlers become thin async wrappers that call the existing sync functions and
map their results into `CommandOutput` / `CommandError`. Do NOT async-ify the
ssh/git/ureq internals; blocking inside a handler is fine for a CLI that runs
one command and exits. Keep ureq; do not switch to reqwest.

### Envelope and flags

- Output is agcli's envelope: one JSON value on stdout, always.
- `--json` is removed as a declared flag. agcli's parser treats an undeclared
  bare `--json` as a boolean and ignores it, so existing agent invocations
  keep working; verify this with a test rather than assuming it.
- `--profile <name>` stays, declared on every command that uses a profile
  (read via `req.flag("profile")` into the existing `Ctx`).
- Reserved agcli flags (`--select`, `--compact`, `--quiet`, `--yes`,
  `--dry-run`) come for free. `setup --dry-run` and `rm --yes` must route
  through `req.dry_run()` / `req.assume_yes()` instead of their old clap flags.

### Exit codes

Map existing anyhow errors to agcli's typed codes at the handler boundary:

- profile missing / repo not found on server → `NOT_FOUND` (3)
- token rejected, 401/403 from dgit or viewer → `AUTH` (4)
- dgit/viewer HTTP failures, ssh remote-script failures → `API` (5)
- bad invocation (missing required flag in `--yes` mode) → `USAGE` (2)
- everything else → `ERROR` (1)

Every `CommandError` gets a real `fix` string — the command or check the agent
should run next, not prose. Grep the existing `anyhow::bail!`/`context` sites
for the messages; keep their wording.

### Per-command notes

- **`brain`** keeps its hook contract: it NEVER returns `Err`. A down or
  unconfigured viewer is an `ok: true` envelope with a `status` field saying
  so. The SessionStart hook depends on exit 0.
- **`setup`** loses interactive prompts entirely (delete the dialoguer
  dependency and `Out::interactive`). Missing answers are a `USAGE` error
  whose `fix` lists the missing flags. Env fallbacks (`AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, `TS_AUTHKEY`) stay.
- **`annotate`** launches plannotator by default again (the old `--json`
  suppression made sense when `--json` meant "an agent is asking"; now the
  agent launches it FOR the human). Envelope reports file, plannotator path
  (null when absent), viewer URL. Add `--no-launch` for the old inspect-only
  behavior.
- **`rm`** requires `--yes` (no TTY confirm exists anymore); without it,
  `USAGE` error with the exact rerun line in `fix`.
- **`ls`, `comments`** use `CommandOutput::list(...)` /
  `list_truncated(...)` so agents get `{items, count, total, truncated,
  fields}` and the free `--select` advertisement.
- **`doctor`** becomes agcli's built-in `doctor` with the existing checks
  wrapped as `Check`s; a failing check carries its exit code (e.g. `AUTH`)
  per agcli semantics. Skipped-never-passing stays.
- **`token`** stays the one command that puts a secret in the envelope; say
  so in its description.
- **`grep`** (in flight from another work stream, see Coordination): if it
  has landed by the time you start, migrate it like the rest — but its
  rg-compatible `path:line:content` stdout and rg exit codes are its whole
  contract, so it is the ONE command that must bypass the envelope. agcli
  must allow a raw-stdout command; if it cannot, that is an agcli feature to
  add first (author is the same person — add it, publish, then depend on it).

### `next_actions` (the useful ones only, not decoration)

- `setup` → `init`, `doctor`
- `init` / `new` → `plan new`, `index`, `brain`
- `plan new` → `annotate`, `comments <file>`
- `annotate` → `comments <file> --since=<now>`
- `index` → `index --status`, `brain`
- `comments` → the same call with `--since=` of the newest comment returned
- errors → the `fix` as a runnable next action where one exists

### Help prose

Every `long_about` in the current `cli.rs` moves into the agcli command
description (or a `docs` field on the command if descriptions must stay
short — check what the root tree renders). Zero prose may be dropped; the
seven setup steps, the token semantics, the ls-scrape caveat, and the invite
token-vs-network distinction are load-bearing. `AGENTS.md` gets updated to
the envelope shapes in the same commit as the surface change.

## Coordination (this repo runs concurrent agents)

- `CLAUDE.md` and `COORDINATION.md` rules apply: `git pull --rebase` first,
  claim `cli/src/**` in `COORDINATION.md` before touching it, small commits.
- Another agent is actively modifying `cli/src/` (brain digest, `nashcode
  grep`). Do NOT start the `cli.rs`/`main.rs` rewrite until their work is
  committed and you have rebased on it. If their tree is still dirty, do the
  spec commit and the agcli-side prep first, then wait.
- New scope goes into `SPEC.md` first, in its own commit, before the
  implementation. The envelope contract above IS that scope; condense it into
  SPEC.md, not this file's prose verbatim.

## Tests

`cli/tests/` drives the binary as a subprocess and asserts on the old
`--json` shapes (`brain_cli.rs`, `cli_json.rs`, `annotate_cli.rs`,
`comments_cli.rs`, `invite_cli.rs`, ...). These are the downstream contract:

- Update them to the envelope shape deliberately, one test file per commit,
  keeping the fixtures (`cli/tests/fixtures/`) and the shim pattern
  (`ssh_shim.rs`, `jj_shim.rs`).
- Add: an `audit` test (`assert!(cli.audit().is_clean())`), a test that a
  stray `--json` is ignored (back-compat), a `brain`-exits-0-when-viewer-down
  test if one does not already exist, and one typed-exit-code test per class
  (3/4/5).
- Run with `cargo nextest run --workspace` (NEVER `cargo test`), keep clippy
  clean (`cargo clippy --workspace --all-targets`).
- Build trap: the machine-wide shared cargo build dir can serve stale rlibs
  when two agents build concurrently — impossible-looking failures mean
  rebuild with an isolated `CARGO_TARGET_DIR`.

## Order of work

1. Spec commit: envelope contract into `SPEC.md`.
2. agcli gap check: raw-stdout command support for `grep` (and anything else
   the port surfaces). Fix in `/Users/md/Projects/agcli`, publish a version,
   pin it. Record every gap found — that list is a deliverable too.
3. Claim files; rewrite `main.rs` + `cli.rs` + `output.rs`; wrap handlers.
4. Port tests file by file.
5. Update `AGENTS.md`, the SessionStart hook invocation if its flags changed,
   and `~/.claude/CLAUDE.md`'s nashcode section is Matthias's to update — note
   needed changes in NOTES.md instead of editing his global file.
6. Record decisions in `NOTES.md`, clear claims from `COORDINATION.md`.

## Done means

Old clap surface gone (clap and dialoguer out of `cli/Cargo.toml`; agcli,
tokio in). Every command emits one envelope with typed exit codes and `fix`
hints; `brain` still exits 0 when the viewer is down; `grep` (if present)
still speaks raw rg format; suite green and grown under nextest; clippy
clean; `cli.audit().is_clean()` asserted in tests; AGENTS.md matches reality;
the agcli gap list recorded in NOTES.md.
