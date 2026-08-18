# nashgit — UAT test design and acceptance criteria

Companion to [`UAT-STORIES.md`](../UAT-STORIES.md). Every test names the stories it
covers. Two execution modes:

- **auto** — implemented in [`uat.py`](uat.py). Run `python3 uat/uat.py` from the repo
  root after `cargo build`. It builds its own fixture world in a temp dir, boots the
  compiled binary on its own port, runs every check, and exits nonzero on any failure.
- **visual** — needs eyes: rendering, fonts, drag-and-drop, confirm dialogs. Executed as
  a macos-harness + cap recording pass, one video per group, after the auto pass is
  green.

The fixture world `uat.py` builds:

```
demo.git                          beta.git
  main ── plans/retries.md          main ── plans/api.md, tasks/api.md
  │       tasks/{retry,docs,setup,  └── feat/endpoint
  │              broken,ghost}.md
  │       .nashgit/ci  src/retry.py
  ├── feat/retry-core   (+2, green CI)
  │     └── feat/retry-jitter (+1)
  ├── chore/bad-lint    (+1, red CI: syntax error)
  └── stackb/base ── stackb/child
              └───── stackb/conflict (edits the same line the test later
                                      moves under it, to force a rebase conflict)
```

`broken.md` has unparseable front matter (board resilience). `ghost.md` declares
`plan: plans/nope.md` (dangling ref). All writes go to throwaway bare repos in the
temp dir; nothing touches a real remote.

## T1 — Pages render (auto) — stories 22, 23, 35, 39, 43

Steps: GET `/`, `/:repo`, `/:repo/stacks`, `/:repo/plans`, `/:repo/board`, `/:repo/ci`,
`/:repo/:branch`, `/:repo/traces`.

Accept when: every route is 200; `/` names both repos; `/demo` lists all branches with
stack parent, ahead counter, and a CI status icon per row.

## T2 — Stack inference (auto) — stories 24, 25

Steps: read `/brain` and extract each branch's inferred parent and ahead count.

Accept when: `feat/retry-core` → parent `main` (+2); `feat/retry-jitter` →
`feat/retry-core` (+1); `chore/bad-lint` → `main` (+1); `stackb/child` →
`stackb/base`; a branch with no better ancestor falls back to `main`.

## T3 — Review page content (auto + visual) — stories 1, 2, 3, 4, 5, 11

Auto steps: GET `/demo/feat/retry-core`.
Accept when: the page shows exactly the branch's unique commit subjects ("Retry core…",
"Gate retries…") and not the parent's; it references `src/retry.py` diff content; it
links the parent (`main`) and the child (`feat/retry-jitter`); it shows the card and
plan that declare the branch; reserved words (`stacks`, `board`, …) never resolve as
branches; a `/`-named branch resolves.

Visual: diffs render through `@pierre/diffs` with syntax highlighting (not plain
`<pre>`), IBM Plex Mono in code, Departure Mono chrome, light/dark both legible.

## T4 — CI lifecycle (auto) — stories 33, 34, 35, 36, 38

Steps: wait for the boot-time runs; read `/brain` and the CI pages; POST
`/demo/chore/bad-lint/ci/rerun`.

Accept when: `chore/bad-lint` records `failed` and green branches record `passed`
without any per-repo config; the green log page contains the script's output
("all green") and the red log the compile error; rerun answers ok and a new run for
the same tip appears; `beta` (no `.nashgit/ci`) records no runs; job env is only the
four documented vars (asserted by the CI script itself, which fails if `HOME` leaks —
covered by the script printing its env size).

## T5 — Raw files (auto) — stories 42

Steps: GET `/demo/raw/main/plans/retries.md` and
`/demo/raw/feat/retry-jitter/src/jitter.py`.

Accept when: bytes are identical to `git show` at that ref (slash-branch included) and
content type is `text/plain`.

## T6 — Comments API (auto) — stories 6, 7, 9, 10, 51, 52, 53, 54

Steps: POST three comments on `plans/retries.md` (one with a `Tailscale-User-Login`
header, one with explicit `author`, one bare), one branch-level comment, one
line-anchored code comment; GET with `?file=`, then with `?since=` set to the second
comment's `created_at`; delete one comment as its author and try to delete it as
someone else first.

Accept when: POST answers 201 with a stable integer `id`; author precedence is
explicit `author` → Tailscale header → `local`; results come back oldest-first ordered
by `created_at` then `id`; `since` returns strictly newer rows, no misses, no repeats;
a comment anchored to a plan file (not in any diff) is accepted and rendered on the
plan page; deleting someone else's comment fails and leaves it; deleting your own
removes it.

## T7 — Outdated comments (auto + visual) — story 8

Steps: line-anchor a comment on `src/jitter.py` at the `feat/retry-jitter` tip and one
on `README.md`; push a new commit to the branch that rewrites `src/jitter.py`; reload
the branch page.

Accept when: the jitter comment moves to the outdated section (its body still
readable); the README comment stays inline, because its file did not change.

Visual: outdated section is visually distinct; inline comments sit in the
`@pierre/diffs` annotation slots on the right lines.

## T8 — Board (auto + visual) — stories 43, 44, 45, 46, 47

Auto steps: GET `/demo/board`; POST `/demo/board/move {"file":"tasks/docs.md",
"status":"doing"}` with Tailscale headers; fetch the file and history afterward.

Accept when: columns are `todo`, `doing`, `done` in that order plus `needs-attention`
holding `broken.md`; after the move only the `status:` line differs (body
byte-identical), exactly one new commit exists on `main`, authored as the Tailscale
user, and the endpoint reported the new commit; a move naming a file outside `tasks/`
or an invalid status is a 4xx with no commit.

Visual: drag-and-drop performs the same move; on a forced push failure the card snaps
back with a toast.

## T9 — Links (auto + visual) — stories 48, 49, 50

Auto steps: GET the plan page, card page, and branch page.

Accept when: `plans/retries.md` shows its cards (`tasks/retry.md`, `tasks/docs.md`)
and its branch with CI status; the branch page shows the plan and card back;
`tasks/ghost.md` renders its dangling `plan: plans/nope.md` as plain text with a
"missing" label and the page is 200; a backticked branch token in markdown links to
the branch page.

## T10 — Merge (auto + visual) — stories 26, 27, 28, 32

Auto steps: POST `/demo/chore/bad-lint/merge` (red CI) expecting refusal; then move a
card first so `main` has moved, wait for green CI on `feat/retry-core`, POST its
merge with Tailscale identity headers.

Accept when: the red branch answers 409 "blocked" and nothing moves; the green merge
answers ok; `main` gains a `--no-ff` merge commit (parent had moved) containing both
branch commits; `tasks/retry.md` flips to `status: done` in a **separate** commit
authored by the merging user; the audit log on `/demo/stacks` records the merge with
that identity; a `merged` webhook fires.

Visual: the confirm step gates a red/running merge; the button offers branch deletion.

## T11 — Restack (auto + visual) — stories 29, 30, 31, 32

Auto steps: push a new commit to `stackb/base` that edits the line
`stackb/conflict` also edits; POST `/demo/stackb/base/restack` (expect conflict);
POST `/demo/stackb/conflict/delete`; restack again.

Accept when: the conflicted restack answers 409 naming `base.txt`, and **neither**
child's tip moved (atomic abort, nothing pushed); after deleting the conflicting
branch the restack succeeds, `stackb/child` is rebased onto the new base tip (old tip
no longer an ancestor path, new base tip is), the audit log records the restack, and
a `restacked` webhook fires.

## T12 — Traces (auto + visual) — stories 12–21

Auto steps: POST an event batch to `/demo/traces/events` twice; drive the real
`nashgit hook` binary around two commits in a clone (UserPromptSubmit → commit →
PostToolUse → commit → Stop), push; POST and GET a transcript; run `nashgit trace
list` / `show`; feed the hook garbage and a dead server.

Accept when: the second identical batch stores 0 and reports the duplicates
(idempotent on `(session, seq)`); the session lists both commits made between its
events, with zero cooperation from the "agent"; `GET /demo/commits/:sha/trace` maps a
commit back to the session; the session page renders the prompt text; the transcript
round-trips verbatim; `trace list`/`show` print the session and its events; the hook
exits 0 on garbage stdin and on an unreachable server, silently.

Visual: the session page reads top-to-bottom with commits linked inline; the branch
page links commits to their trace.

## T13 — Brain (auto) — stories 55, 56, 57

Steps: GET `/brain`, `/brain?repo=beta`, `/brain?since=<future>`; POST `/brain/ask`
with no `ANTHROPIC_API_KEY` in the server env.

Accept when: the aggregate carries both repos with branches, plans, cards, and
activity; `?repo=` filters to one; a future `since` empties the activity arrays
without erroring; `/brain/ask` is 404 when the key is absent. (The live-key ask path
and the stubbed 502/429 contract are covered by the crate's integration tests, not
UAT.)

## T14 — Webhooks (auto) — stories 58

Steps: run the whole suite with `NASHGIT_WEBHOOKS` pointed at an in-process listener.

Accept when: by the end of the run the listener has received `push` (with repo,
branch, commit), `ci_finished` (with status), `merged`, and `restacked` events.
(Timeout/retry behavior is covered by the crate's tests.)

## T15 — Degradation (auto + visual) — stories 62, 63

Auto steps: stop the server; restart it with `DGIT_URL` pointing at a path that does
not exist, same mirrors and DB; GET the pages again.

Accept when: every repo page still answers 200 from the last-known mirror with the
banner "Showing the last mirrored state"; an unknown repo and an unknown branch are
404, never 500.

## T16 — CLI doctor + identity (auto) — stories 60, 10

Steps: run `nashgit doctor` against the live server and against a dead URL.

Accept when: live → exit 0 and "reachable"; dead → nonzero. Identity: every write in
the suite made with `Tailscale-User-Login` headers shows that identity in comments,
commits, and the audit log; writes without headers show `local`.

## Not automated (deliberate)

- **Story 59/61/64/65** (single-binary build, loopback bind, backup story, Primer/
  Phosphor/typography): build is proven by the suite using the compiled binary;
  loopback default and the rest are covered by the crate's own tests or are visual.
- **30-minute CI timeout, webhook 10s/retry, brain 502/429**: already covered by
  `cargo nextest run` integration tests; UAT does not repeat them.
- **Merge confirm step, drag-and-drop, toast, Pierre rendering, dark mode**: the
  visual cap pass (T3, T7, T8, T10, T12 visual halves).
