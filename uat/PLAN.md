# nashgit UAT — the plan

The single UAT document. It supersedes `UAT-STORIES.md` and `UAT-TESTS.md`; delete both
when this lands. Companion code: [`uat.py`](uat.py), the automated harness.

## Decisions (locked with Matthias, 2026-08-18)

1. **All local.** No exe.dev deploy. nashgit is not hosted anywhere today; the exe.dev
   box runs only dgit.
2. **One repo.** The live instance serves exactly one repo: nashgit itself. No extra
   fixture repos in the live setup. The harness keeps one throwaway fixture repo (named
   `demo`) for write-path tests, because merges and restacks must not touch the real
   repo.
3. **"How the code evolved" = traces + audit log + self-hosting.** nashgit pointed at its
   own repo, with the Claude session transcripts that built it backfilled, is the
   evolution demo. No per-commit diff view gets built; the ask is flattened into the one
   tool.
4. **Visual pass is recorded locally with cap** against the self-hosted instance.

## Status

- The workspace restructure landed: the repo is now `viewer/` (binary
  `nashgit-viewer`: serve, hook, trace, doctor) plus `cli/` (binary `nashgit`, the
  deploy CLI). Everything below uses `nashgit-viewer`.
- Automated suite: green, including the T17/T18 additions and the beta-fixture
  flattening. Run: `cargo build && python3 uat/uat.py`.

## Part 1 — self-host nashgit on nashgit (the evolution demo)

The repo's `origin` is GitHub now, so the local bare hub gets its own remote name:

```sh
git clone --bare ~/Projects/nashgit ~/git-local/nashgit.git
git -C ~/Projects/nashgit remote add hub ~/git-local/nashgit.git

DGIT_URL=~/git-local NASHGIT_REPOS=nashgit \
NASHGIT_MIRRORS=~/git-local/mirrors NASHGIT_TRACES=~/git-local/traces \
  ~/Projects/nashgit/target/debug/nashgit-viewer serve
```

Backfill the sessions that built nashgit (transcripts live in
`~/.claude/projects/-Users-md-Projects-nashgit/`):

```sh
for t in ~/.claude/projects/-Users-md-Projects-nashgit/*.jsonl; do
  NASHGIT_REPO=nashgit ./target/debug/nashgit-viewer trace push "$t"
done
```

UAT finding, fixed in `viewer/src/cli.rs`: backfilled transcripts stored the user's
words under `message.content`, where the prompts page cannot see them. `trace push`
now lifts them into the event's `prompt` field (skipping harness markup), so
`/nashgit/prompts` lists the real asks that built this tool.

Wire the hook so future sessions attribute their commits automatically
(project `.claude/settings.json`, per the viewer README): `nashgit-viewer hook` on
`UserPromptSubmit`, `PostToolUse`, and `Stop`.

Accept when:

- `http://127.0.0.1:8090/nashgit` shows the real branch list;
  `/nashgit/traces` lists the backfilled sessions; a session page renders the actual
  prompts and tool calls of the conversations that wrote this code.
- `/nashgit/prompts` lists and searches the prompts across sessions.
- The stacks page shows the audit log once the first real merge happens.
- Known caveat, stated not hidden: **backfilled** sessions carry no per-event HEAD, so
  their commit attribution may be empty. Attribution is proven live by the hook
  (T12 below) and applies to every session after the hook is wired.

## Part 2 — automated acceptance suite

`uat.py` boots the binary against a throwaway fixture world and asserts the criteria
below. Fixture shape: one repo with a stacked chain (`feat/retry-core` →
`feat/retry-jitter`), a red-CI branch, a second stack for restacks, plans, cards
(including one malformed and one with a dangling ref), and a `.nashgit/ci` script.

| Group | Covers | Accept when |
|---|---|---|
| T1 pages | every route | all 200; branch list shows parent, ahead count, CI per row |
| T2 stacks | inference | parents and ahead counts match ancestry; orphans fall back to the default branch |
| T3 review | branch page | unique commits only; diffs, parent/child links, card+plan back-links; reserved words never collide; slash branches resolve; unknown repo/branch 4xx |
| T4 CI | runner | red fails, green passes, no-script repos only ever `skipped`; logs show script output; rerun records a new run |
| T5 raw | byte fidelity | raw bytes identical to `git show`, slash branches included |
| T6 comments | API | 201+id; author precedence explicit → Tailscale header → `local`; `since` cursor strictly newer, ordered, no repeats; plan files commentable; delete own only |
| T7 outdated | anchors | comment on a moved file lands in the outdated section; fresh anchors stay inline |
| T8 board | kanban | columns ordered; malformed card → needs-attention; move rewrites only `status:` (body byte-identical), one commit as the Tailscale user; invalid moves 400 with no commit |
| T9 links | refs | back-links both directions; dangling ref renders "missing", page still 200; backticked branch tokens link |
| T10 merge | write path | red CI → 409, nothing moves; green merge lands both commits, no-ff when parent moved; card flips to `done` in a separate commit by the merger; audit + `merged` webhook |
| T11 restack | write path | invoked on the moved branch (`main`); conflict → 409 naming the file, atomic abort, no tip moves; after unblocking, descendants rebase onto the new tip; audit + `restacked` webhook |
| T12 traces | evolution | event batches idempotent on `(session, seq)`; hook attributes commits with zero agent cooperation; commit ↔ session maps both ways; transcript round-trips verbatim; `trace list/show` work; hook exits 0 on garbage and dead servers |
| T13 brain | aggregate | branches/plans/cards/activity present; `?repo=`/`?since=` filter; `/brain/ask` 404 without a key; `plans?branch=` works |
| T14 webhooks | delivery | `push`, `ci_finished`, `merged`, `restacked` all received |
| T15 degradation | resilience | dead git server → every page 200 with the stale banner; unknown repo 404, never 500 |
| T16 CLI | doctor | exit 0 + "reachable" when up; nonzero when down |

Additions (landed with this plan):

- **T17 prompts page.** `/:repo/prompts` renders; `?q=` and `?session=` filter;
  `Accept: application/json` returns the list; a backfilled Claude Code transcript's
  user text surfaces as a searchable prompt and harness markup does not.
- **T18 lease semantics.** Deleting a ref that moved under the viewer is **rejected**,
  not applied (`--force-with-lease --atomic`), and the concurrent push survives; once
  the mirror catches up the same delete succeeds. Git subprocess timeouts (60s local /
  300s remote) stay covered by the crate's own tests.
- **Fixture flattening.** The second repo (`beta`) is gone: the branch-only plan and
  the skipped-CI check moved onto branches of the single fixture repo; multi-repo
  brain aggregation and `?repo=` narrowing stay covered by the crate's own tests.

## Part 3 — visual pass (cap recordings, local, macos-harness driven)

Five short recordings against the self-hosted instance from Part 1. Each has a shot
list; the recording fails if a shot can't be produced.

Delivered (2026-08-18, driven via browser-harness CDP, window-scoped cap capture):

1. Review — https://cap.so/s/f49xcsxk2a3e2cd
2. Evolution — https://cap.so/s/7t88hndt8djawtx
3. Board — https://cap.so/s/0zkjqv6zyrpm065
4. Merge — https://cap.so/s/t1ar5esqadh38gp
5. Degradation — https://cap.so/s/dzze698w9kam82g

One shot fell short: the red-merge `confirm()` dialog is a native OS window, which
window-scoped capture cannot see. Clip 4 shows the red "Merge into main despite CI"
gate and the branch staying put; the dialog itself is not in frame.

1. **Review** — open `/nashgit/<branch>`: Pierre-rendered syntax-highlighted diffs (not
   a plain `<pre>`), IBM Plex Mono in code, click a line, post an inline comment, see it
   in the annotation slot. Toggle dark mode; both legible.
2. **Evolution** — `/nashgit/traces`: open a backfilled session, scroll the real
   transcript, follow a commit link from a hook-attributed session; `/nashgit/prompts`
   search.
3. **Board** — drag a card between columns; show the resulting commit in the log; show
   the malformed card sitting in needs-attention.
4. **Merge/restack** — the confirm step on a red branch; a green merge; the card
   flipping to done; the audit line appearing on the stacks page.
5. **Degradation** — stop the hub, reload: stale banner, everything still readable.

## Part 4 — the re-loop (keeping this exhaustive)

The catalog went stale once already (prompts page, lease semantics). Standing rule:

- After every `git pull`, diff `SPEC.md`/`README.md` features and `COORDINATION.md`
  notes against Part 2's table. New feature → new row + harness checks before it counts
  as covered.
- `python3 uat/uat.py` must be green at the commit under review before any visual pass
  or sign-off.

## Gap register (known, accepted, revisit deliberately)

- **Identity perimeter is simulated.** The suite injects `Tailscale-User-Login` headers
  directly; the real `tailscale serve` path is untested until nashgit deploys next to
  dgit. Revisit at deploy time.
- **`/brain/ask` live path** untested here (no key in the server env); the stubbed
  contract (success/refusal/429/502) is covered by crate tests.
- **Backfilled-session commit attribution** may be empty (Part 1 caveat); live hook
  attribution is the proven path.
- **Job-env allowlist, 30-min CI timeout, webhook retry timing**: crate tests own these;
  UAT does not repeat them.
- **The deploy CLI (`cli/`, binary `nashgit`) is out of UAT scope.** This plan covers
  the viewer. The CLI's setup/invite/doctor surface is covered by its own 84 crate
  tests. The marketing site under `docs/` is content, not product; no row.
