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

6. **A flag has nowhere to document itself.** A `Command` has a description and
   a usage string; a *flag* has neither. clap gave every flag a doc comment, and
   about twenty facts lived in those — the viewer's `:8443`, `$NASHCODE_JJ`, the
   name charsets, the `s3://bucket/prefix` form, what `--private` actually does,
   every default, and the warning that `--token` lands in your shell history.

   Nothing was dropped: they are recovered into the command descriptions, as a
   flag list at the end of the longer ones. It reads worse than clap's `--help`
   did and it cannot be projected or queried — an agent asking "what does
   `--region` default to" gets the whole description or nothing.

   Fix in agcli: `Command::flag(name, description)`, rendered beside `usage` in
   the tree and in `help <command>`.

7. **`--quiet` does not reach an error envelope.** It empties `next_actions` on
   success and leaves them on failure. Defensible — on a failure the trail *is*
   the fix — but the reserved-flag documentation says "omit `next_actions`"
   without qualification, so either the flag or the sentence is wrong. AGENTS.md
   describes the real behaviour, and
   `cli/tests/envelope.rs::quiet_does_not_reach_an_error_envelope` fails when
   agcli changes its mind, which is the point of it.

8. **Description is the only prose field, so the root tree is 32 KB.** Every
   command's full documentation goes in `description`, because there is nowhere
   else to put it, and the root command tree renders every description. A bare
   `nashcode` costs about 32 KB of context; a typo answers with roughly 14 KB.
   That is a token bill on the one call an agent makes to orient itself.

   The goal file asked whether a `docs` field exists to put the long prose in.
   It does not. Fix in agcli: `Command::docs(...)`, shown by `help <command>`
   and omitted from the root tree, which would leave the tree a one-line
   summary per command — what it is for.

Two non-gaps worth recording, because both were assumed to be problems:

- **`--json` back-compat works.** It is reserved as a *boolean*, so
  `nashcode brain --json somerepo` keeps `somerepo` as the positional. Proven by
  `cli/tests/envelope.rs::a_stray_json_flag_is_ignored_and_never_eats_the_positional`,
  from both sides of the command name.
- **A raw command's argv is genuinely verbatim.** `nashcode grep -- -Zthreads`
  arrives with its `--` intact. `grep::raw_args()` used to walk the process's
  own `std::env::args()` because clap ate the first `--`; that walk is gone.

## Decisions the spec left open

**`grep` reads nashcode's own flags on either side of the command name.**
`--profile`, `--json` and `--quiet` belong to nashcode, not to rg, and an agent
types them wherever it likes. A raw handler is given everything *after* `grep`
and nothing before it, so both sides need reading: `grep::parse` takes them
after the command name, `grep::preceding_flags` scans the process argv as far as
the subcommand for the ones before it — the same region the old `raw_args()`
skipped, so nothing that worked before stopped working. `--profile` in particular used to be
forwarded to rg, which has no such flag, and its value became the pattern —
`nashcode grep --profile work retry` searched for `work`.

`--quiet` silences grep's `#` degradation notes on stderr. That is what the flag
means, and stdout is unaffected, but it is worth knowing that the "every
degradation says so in one line" promise is the one thing `--quiet` can take
away. Under `--json` the same facts are still in the result.

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

**Exit codes are decided where the error is raised**, in `crate::exit`. This
started as substring matching over the finished message, which peer review took
apart: the string being matched contains upstream response bodies and, in
`annotate`, a human's review notes. A reviewer writing "does not exist" turned a
viewer 500 into exit 3. Four more families were misclassified outright —
transport failures on mutating dgit calls, git's own stderr on a push with a
stale token, a remote script's stderr, and a revoke that did not take.

`exit::Classed` is a message that carries its own class and keeps what it
wrapped as its source. It prints exactly as the `.context(...)` it replaces, so
every command's wording survives, and `class_of` reads the class back by type.
Two consequences worth stating plainly: appending anything to a message cannot
change the exit code, and a site that forgets to classify gets the generic
`ERROR`, which is visible in a test rather than silently plausible.

The review recommended matching on an outermost context prefix the CLI owns
instead. That is weaker, and I did not take it: it still matches strings, so it
still fails wherever the outermost context is *built from* foreign text — which
is exactly `ssh::require`, where the remote stderr is interpolated into the
message the reviewer proposed to match. The typed marker has no such edge.

Where a class is read from text at all, it is read from git's own stderr in
`vcs::transport_class`, at the site that ran git and knows the output is git's.
That function returns a type; nothing downstream re-reads prose.

**A `--since` cursor can still skip a comment written in the same microsecond.**
The viewer's cursor is exclusive (`created_at > ?`) over a fixed-width
microsecond timestamp, and its own ordering is `(created_at, id)` — the `id` is
there precisely because ties happen. A comment inserted in the same microsecond
as the newest one an agent has seen will never come back.

Closing that needs a compound `(at, id)` cursor on the viewer, which is not this
side's to add. What this side fixed is the part that was ours: `newest_timestamp`
compared timestamps as *strings*, and the rows are passed through from whatever
answered, so `...:11Z` sorted after `...:11.5Z` while happening before it — a
cursor that stepped backwards and re-delivered. It compares instants now
(`timefmt::instant_key`), and an undateable row sorts below every real one
instead of winning the maximum and freezing the loop.

**No list is ever truncated, and the tests say so rather than faking one.**
Review asked for a `truncated: true` case. There is not one to write: `ls` and
`comments` both use `CommandOutput::list`, which reports everything it was
given, because neither command pages or caps. A test that produced `true` would
have to construct a result no invocation can. So the test pins the invariant
instead — always complete, always `truncated: false`, never any truncation
`guidance` — which is the assertion that fails on the day somebody adds a limit
without telling the caller.

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

Those scripts carry the generated GIT_TOKEN and any bucket credentials, because
they are the scripts that would have run. `TOKEN_DOC` used to claim `token` was
"the one command that writes a secret", which was not true before this migration
either — the old dry run printed the same scripts to stdout. Both `TOKEN_DOC`
and `CLI-SPEC.md` now name both commands. Redacting the preview was the other
option and it is the wrong one: a preview you cannot diff against what will run
is not a preview.

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
