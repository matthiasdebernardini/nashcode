# nashgit — UAT user-story catalog

Every story a user can act out against the current tree. Grouped by persona. Review and
code evolution come first, per Matthias. Each story is one testable behavior; the UAT pass
records each group as a cap video.

Personas:

- **Reviewer** — Matthias in a browser on the tailnet.
- **Agent** — a coding agent (Claude Code) that pushes branches, plans, and cards, and
  talks to the HTTP API.
- **Chief-of-staff** — an agent that reads state and makes recommendations.
- **Operator** — whoever deploys and configures the server.
- **Tool** — an external program: plannotator, a webhook consumer, curl.

## A. Code review (first-class)

1. As a reviewer, I open `/:repo/:branch` and see the commits unique to that branch
   (`parent..branch`), newest context first, so I know what this branch adds.
2. As a reviewer, I see per-file diffs rendered by `@pierre/diffs` with syntax
   highlighting (three-dot `merge-base..branch` semantics), so I review only what the
   branch changed, not what the parent moved under it.
3. As a reviewer, I see a banner linking the branch's stack parent and children, so I can
   walk the stack up and down.
4. As a reviewer on a branch page, I see the CI status of the tip and a link to its log,
   so I know whether it is green before I judge the code.
5. As a reviewer, I see the card and plan that declare `branch: <this>` on the branch
   page, so the code, the plan, and the task are one view.
6. As a reviewer, I click a line in a diff and post an inline comment, and it renders in
   the diff's annotation slot, so my feedback sits on the code it is about.
7. As a reviewer, I post a branch-level comment (no file, no line), so I can comment on
   the whole change.
8. As a reviewer, I return after the author force-pushed and see comments whose file has
   since changed in an "outdated" section, still readable, so I can tell what was
   addressed.
9. As a reviewer, I delete my own comment but cannot delete anyone else's.
10. As a reviewer, my comments are stamped with my Tailscale identity without any login.
11. As a reviewer, a branch whose name contains `/` (e.g. `feat/diffs`) gets a working
    review page, and reserved words (`stacks`, `plans`, `tasks`, `board`, `ci`,
    `comments`, `raw`) never collide with it.

## B. Code evolution — how the code came to be

12. As a reviewer, I open `/:repo/traces` and see agent sessions newest first — agent,
    when, event count, commits produced — so I know which conversations produced code.
13. As a reviewer, I open a session at `/:repo/traces/:session` and read the transcript
    top to bottom (prompts, tool calls, results) with the commits it produced linked
    inline, so I see not just what changed but what was tried first and why.
14. As a reviewer, from a commit in a branch page's commit list I follow its trace link
    to the session that produced it — diff to conversation in one click.
15. As a reviewer, I query `GET /:repo/commits/:sha/trace` and get the session(s) that
    produced that commit as JSON.
16. As a reviewer, I open `/:repo/stacks` and read the merge/restack audit log — who,
    what, when, old tip, new tip — so the write history of the repo is inspectable.
17. As an agent, my harness's hook (`nashgit-viewer hook` on PreToolUse/PostToolUse/Stop) records
    my events with zero cooperation from me; commits made between two of my events are
    attributed to my session automatically.
18. As an agent, `nashgit-viewer hook` NEVER fails my turn: server down, garbage on stdin, no
    configured repo — all exit 0 silently.
19. As an agent (or Matthias backfilling), `nashgit-viewer trace push <file>` uploads a full
    transcript for a run that happened without the hook installed.
20. As an agent, posting the same event batch twice stores one copy (idempotent on
    `(session, seq)`), so retries never double-write.
21. As Matthias in a terminal, `nashgit-viewer trace list` and `nashgit-viewer trace show <session>`
    read traces without a browser.

## C. Stacks

22. As a reviewer, I open `/` and see every repo with its stacks summarized (branch
    names, commits-ahead counts), so one page answers "what is in flight".
23. As a reviewer, I open `/:repo` and see a Forgejo-style branch list: branch, stack
    parent, ahead count, last commit, CI dot.
24. As a reviewer, I open `/:repo/stacks` and see each chain rendered as a column
    (main → part-1 → part-2), with parents inferred from ancestry — no PR model needed.
25. As an author, a branch with no meaningful parent falls back to the default branch as
    its stack parent, so nothing is orphaned.

## D. Merge and restack

26. As a reviewer, I press Merge on a green branch: fast-forward when the parent has not
    moved, `--no-ff` merge commit when it has; the parent is pushed and I am offered
    branch deletion.
27. As a reviewer, Merge is blocked behind a confirm step while the tip's CI is red or
    still running, so I cannot ship a red branch by accident.
28. As a reviewer, merging branch B flips any card declaring `branch: B` to `done` in the
    same push, as a separate commit authored by me, and the audit line says so.
29. As a reviewer, I press Restack after a parent moved: every descendant rebases onto
    the new tip in order and all refs go in one atomic force-push.
30. As a reviewer, a restack conflict aborts the whole thing — nothing pushed, every
    branch untouched — and reports the conflicting files so I can finish in a terminal.
31. As a reviewer, I delete a merged branch from the UI.
32. As an auditor, every merge and restack lands in the audit log with my identity.

## E. CI

33. As an author, pushing a new tip to a repo containing an executable `.nashgit/ci`
    enqueues a job automatically — no config, no YAML.
34. As an author, a repo without `.nashgit/ci` gets no job and no error.
35. As a reviewer, I open `/:repo/ci` and see recent runs (branch, commit, status,
    duration); `/:repo/:branch/ci` shows the log as plain text, ANSI stripped.
36. As a reviewer, I POST `/:repo/:branch/ci/rerun` (or press the button) to re-run the
    tip, stamped with my identity.
37. As an operator, jobs run serially, time out at 30 minutes, and get only `GIT_TOKEN`,
    `NASHGIT_REPO`, `NASHGIT_BRANCH`, `NASHGIT_COMMIT` in the environment.
38. As an author, a nonzero exit shows red next to my branch everywhere the branch
    appears; the CI script deploying on green IS the deploy system.

## F. Plans

39. As a reviewer, `/:repo/plans` lists every markdown file under `plans/` on the default
    branch; `?branch=` picks another branch.
40. As a reviewer, a plan renders with Primer markdown styling and shows the file's
    comment thread inline, so a plan is reviewable like code.
41. As an agent, I push a plan on a branch, a human comments in the viewer, I poll
    `GET /:repo/comments?file=plans/x.md&since=<cursor>`, revise, and force-push — the
    documented agent loop end to end.
42. As a tool, `/:repo/raw/:branch/{path}` returns the file's exact bytes as
    `text/plain`, at a stable URL, including for branch names with slashes.

## G. Board

43. As a reviewer, `/:repo/board` renders one column per `status:` — `todo`, `doing`,
    `done` first in that order, other statuses after — cards newest first.
44. As a reviewer, I drag a card to another column: only the `status:` line changes (body
    byte-identical), one commit authored as me, pushed to dgit before the UI reports
    success; on push failure the card snaps back with a toast.
45. As a reviewer, a card with malformed front matter lands in "needs attention" instead
    of breaking the board.
46. As a reviewer, I click a card and it opens rendered like a plan, with its comment
    thread, its branch's CI dot, and its stack position.
47. As an agent, I create and move cards by editing files and pushing — the board is only
    a view; git is the store.

## H. Links

48. As a reviewer, front-matter refs (`branch:`, `plan:`, `tasks:`) become links in both
    directions: branch page shows its card and plan; a plan shows its cards and branches
    with status and CI.
49. As a reviewer, a token in rendered markdown matching an existing `plans/` or `tasks/`
    file autolinks to its page; a backticked token matching a branch links to its review
    page.
50. As a reviewer, a ref to something that does not exist renders as plain text with a
    "missing" marker — never an error.

## I. Comments API (for tools)

51. As a tool (plannotator), I POST `/:repo/comments` with branch/file/line/body and get
    `201` with the stored comment including its `id`.
52. As a tool, `author` falls back to the Tailscale header, then `local`, when omitted.
53. As an agent, `GET /:repo/comments?since=<RFC3339>` returns strictly newer comments,
    ordered by `created_at` then `id` — a cursor that never misses or repeats.
54. As a reviewer, comments anchor to ANY file at a commit, not only files in a diff —
    that is what makes a plan commentable.

## J. Brain

55. As a chief-of-staff, `GET /brain` returns the whole tailnet's work state as one JSON
    document — branches, stacks, plans, cards, CI, recent activity, open comment counts;
    `?repo=` and `?since=` filter it.
56. As a chief-of-staff, `POST /brain/ask {question, repo?}` gets a Claude answer over
    that state ("which stack is closest to mergeable?") and returns `{answer, model}`.
57. As an operator, without `ANTHROPIC_API_KEY` the route answers 404 and a doctor line
    at startup says why; upstream failures surface as 502 with the API's message, 429
    passes through.

## K. Webhooks

58. As a tool, I receive POSTs for `push`, `ci_finished`, `merged`, `restacked` at the
    URLs in my `NASHGIT_WEBHOOKS` file — 10s timeout, one retry, failures logged not
    queued.

## L. Operations and resilience

59. As an operator, `cargo build` on a fresh clone produces one self-contained binary —
    assets embedded, only `git` needed at runtime.
60. As an operator, startup prints one doctor line per unset thing and what I lose by it;
    `nashgit-viewer doctor` does the same from the client side and exits nonzero when the
    server is unreachable.
61. As an operator, the server binds loopback only; `tailscale serve` is the front door;
    anything that can reach the port can claim any identity — documented, deliberate.
62. As a reviewer, when dgit is unreachable every page still renders (200) from the
    last-known mirror behind a stale banner — degrade, never 500.
63. As a reviewer, a repo with no mirror yet shows an error card, not a 500; only unknown
    repo/branch is a 4xx.
64. As an operator, losing the mirrors directory costs a re-clone, not data; SQLite holds
    the only unique state (comments, CI history, audit, traces) and is the one backup.
65. As a reviewer, the UI reads like GitHub — Primer tokens, Phosphor icons, IBM Plex
    Mono for code, Departure Mono for chrome — and light/dark both work via
    `prefers-color-scheme`.

## Known gaps this catalog surfaces

- **No per-commit diff page.** "How did this file evolve commit by commit" is delegated
  to dgit's cgit UI (log/blame/diff). nashgit shows branch-level diffs, the audit log,
  and traces. If commit-by-commit archaeology should live in nashgit, that is a new
  feature, not a test.
- **Traces and the CLI are implemented and committed** (`src/traces.rs`, `src/cli.rs`,
  `src/web/traces.rs`), with integration tests covering stories 12–21.
- **nashgit is not deployed.** The dgit box runs dgit only. UAT needs a local run or a
  deploy first.
