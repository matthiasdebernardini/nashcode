# cli/NOTES.md

Where the clap → agcli migration had to choose, and what agcli could not do.
`CLI-SPEC.md` is the contract; this is the record of the gaps in it.

## agcli gaps

agcli 0.15.0 is pinned. Two gaps were found before this port started and fixed
in that release; three more turned up during it and are still open.

**Fixed in 0.15.0, and used here:**

1. **Raw passthrough commands.** `grep` owes callers ripgrep's
   `path:line:content` and ripgrep's exit codes. An envelope cannot express
   that. `Command::raw_handler` is the opt-out: the handler gets the argv tail
   verbatim, writes its own stdout, and returns its own exit code.
2. **A third check outcome.** `doctor` has always refused to report a check it
   could not run as a pass. `CheckResult::skip` keeps that: a skip leaves
   `healthy` true and never drives the exit code.

**Still open, worked around here:**

3. **`AgentCli::doctor` hardcodes the command's usage string** (`<name> doctor`)
   and its description (`Run environment health checks`). Two consequences:
   - `nashcode --profile x doctor` is rejected with `UNKNOWN_FLAG` (exit 2),
     because unknown-flag validation reads the usage string as the flag schema
     and the built-in usage declares none. `doctor` therefore always checks the
     **active** profile. This is the one place `--profile` does not work, and it
     is a real regression against the clap surface.
   - the `doctor` prose had nowhere to live, so it rides in the root tree as a
     `doctor` field via `root_field`. Zero prose dropped, wrong home.

   Fix in agcli: a `doctor_with(Command, Vec<Check>)`, or make `doctor` return
   the `Command` for the caller to finish building.

4. **`CommandError` carries no structured data.** `annotate` has one promise it
   must not break: feedback a human spent ten minutes writing is never lost. On
   a refused POST there is now nowhere structured to put it, so it is appended
   to the error message under an `unposted feedback:` separator. A
   `CommandError::detail(Value)` — a payload beside `message`/`code`/`fix` —
   would let the failure carry the thing that failed to send as data.

5. **Reserved-flag values are not readable.** `req.dry_run()` and
   `req.assume_yes()` are booleans, which is all this CLI needs, but there is no
   accessor for a reserved flag's raw value. Not blocking; noted because the
   next consumer will want one.

Two non-gaps worth recording, because both were assumed to be problems:

- **`--json` back-compat works.** It is reserved as a *boolean*, so
  `nashcode brain --json somerepo` keeps `somerepo` as the positional. Proven by
  `cli/tests/envelope.rs::a_stray_json_flag_is_ignored_and_never_eats_the_positional`,
  from both sides of the command name.
- **A raw command's argv is genuinely verbatim.** `nashcode grep -- -Zthreads`
  arrives with its `--` intact. `grep::raw_args()` used to walk the process's
  own `std::env::args()` because clap ate the first `--`; that walk is gone.

## Decisions the spec left open

**`grep` still reads two flags off the process argv.** A raw handler is given
everything *after* `grep` and nothing before it — correct for the pattern, wrong
for `--profile` and the habitual `--json`, both of which agents type before the
subcommand. `grep::preceding_flags()` scans `std::env::args()` as far as the
first non-flag token (the subcommand) for exactly those two. This is the same
region the old `raw_args()` skipped, so behaviour is unchanged.

**`brain` reports `status`, not `ok`.** The envelope around it is always
`ok: true` — a viewer that is down is a fact about the deployment, not a failure
of the command — so an inner `ok: false` would have contradicted it. The digest
now carries `status: "ok"` or `status: "unavailable"` with `error`.

**Human renderers are gone, and so are the tests that only exercised layout.**
`brain::render`, `doctor::render`/`report`, and `plan::render_comments` had no
caller once stdout became the envelope. The invariants they carried were
re-pointed at the data instead:

- terminal-escape sanitising moved from `brain::render` into `brain::repo_digest`
  (repository and branch names), so the guarantee survives without a renderer.
  JSON encoding escapes control bytes anyway; this keeps the promise for a
  consumer that does not decode.
- `plan::parse_comments` (a normalising reader) became `plan::rows_of` (a
  passthrough) plus `plan::newest_timestamp`, which is what the polling
  `next_action` needs.
- pure-layout tests were deleted rather than rewritten. Their subject no longer
  exists.

**Exit codes are classified from the message text**, in `cli::classify`. The
command bodies keep their `bail!`/`context` wording exactly as it was — that
was a hard requirement — so the class has to be read off it. The alternative,
a typed error enum threaded through nine modules, is the rewrite this migration
was explicitly not supposed to be. If the wording drifts, the class drifts with
it; `cli/tests/envelope.rs` pins one case per class against the real binary.

**`nashcode ls` rows carry their own URL.** The old shape was
`{url, count, repos}`; a bounded list has no room for a top-level `url`, so each
row got `url` and `web`. `--select=items.name,items.url` then still answers
"where is this".

**`nashcode index` keeps the viewer's answer verbatim** and adds `repo`,
`queued`, and `summary` (the one-line reading that used to be the human output).
`code::render_status` survives as the producer of `summary`, tests and all.

**`doctor` runs its checks once, not nine times.** agcli asks for one closure
per check; four of nashcode's share a single SSH round trip and three share one
HTTP request. `doctor::checks()` puts the whole sweep behind a `OnceLock` and
each closure reads its own row out of it. A side effect: when the server is
unreachable, the duplicate `tailscale-headers` row the old report emitted (once
from the server checks, once from the host checks) is now reported once.

**`setup --dry-run` returns its scripts** in `result.scripts` as
`[{step, script}]`. It used to print them, and stdout is not ours to print to
any more.

**`rm` and `setup` no longer own their gates.** `--yes` and `--dry-run` are
reserved, so `RmArgs::yes` and `SetupArgs::yes` are gone from the argument
records and the surface enforces them. `rm` without `--yes` is a `USAGE` error
whose `fix` is the exact rerun line.

## The SessionStart hook

`.claude/settings.json` runs `.claude/brain-hook.sh`, which calls
`nashcode brain --json`. **No change was needed.** `--json` is reserved and
ignored, output was already JSON in that mode, and the exit code is still 0 on
every path. The hook's `jq` fallback reads the viewer's raw `/brain` and is
untouched by any of this.

## For the global `~/.claude/CLAUDE.md` (Matthias's file — not edited here)

Its nashcode section says `--json` on every command. That is now redundant
rather than wrong: output is always JSON and the flag is accepted and ignored.
Two edits worth making when you next touch that file:

- drop "`--json` on every command"; say instead that every command answers with
  one JSON envelope and a typed exit code (0/2/3/4/5), and that errors carry a
  runnable `fix`.
- add `--select=<fields>` / `--compact` / `--quiet` as the token-economy flags
  for the commands driven in a loop (`comments`, `brain`, `ls`).

`nashcode grep` and `nashcode brain` behave exactly as that section describes,
so nothing there needs correcting.

## Commit shape

The suggested sequence — surface first, then one test file per commit — means
the commit that lands `cli.rs`/`main.rs` has integration tests still asserting
the clap shapes. The whole port was verified green before any of it was
committed; a bisect that stops between the surface commit and the last test
commit will show `cli/tests/*` failing.
