# nashgit — personal git hosting on the tailnet

Two pieces:

1. **Server (no code, ops only):** [dgit](https://github.com/littledivy/dgit) on a small
   cloud box, replicating to an S3 bucket. Bound tailnet-only; `tailscale serve` provides
   HTTPS. dgit itself gives smart-HTTP git plus a cgit-style web UI (log, tree, blame,
   diffs, snapshots). Which box and which bucket is deployment configuration, never
   source: this repo is public and names no host, bucket, account, or tailnet.

2. **Viewer (this repo):** a [Topcoat](https://github.com/tokio-rs/topcoat) (Rust) web app
   that adds what dgit lacks: Pierre-quality diffs and stacked-branch ("stacked PR") review.

## Viewer requirements

- **Mirrors, not packfile parsing.** The viewer keeps `git clone --mirror` copies of each
  repo under `$NASHGIT_MIRRORS` (default `~/mirrors`), fetched on page load with a short
  (~10s) debounce. All git questions are answered by shelling out to `git` against the
  mirror. Auth to dgit: basic auth `x:$GIT_TOKEN`.
- **Repo discovery:** `$NASHGIT_REPOS` env var, comma-separated repo names, matching dgit
  repo names at `$DGIT_URL/<name>.git`. (dgit has no list API we rely on.)
- **Diff rendering: `@pierre/diffs`** (npm, vanilla-JS build, Shiki-based). The Rust app
  serves unified-diff text (`git diff parent...branch` per file); the browser renders it
  with `FileDiff` from `@pierre/diffs`. Bundle the JS once at build time with esbuild;
  commit the bundle or build it in `build.rs` — implementer's choice, but `cargo build`
  on a fresh clone must produce a runnable server given node+npm present.
- **Degrade, never 500.** This is a daily driver. If dgit is unreachable or a mirror fetch
  fails, pages serve the last-known mirror state behind a stale banner. A repo with no
  mirror yet shows an error card, not a 500. Only a genuinely broken request (unknown repo,
  unknown branch) is a 4xx.
- **No hardcoded infrastructure.** The repo is published publicly: no hostnames, bucket
  names, account IDs, or tailnet names anywhere in the source. Everything is env-driven and
  the README is written for a stranger.
- **Stack inference (no PR model exists):** a "stack" is a chain of branches. For each
  branch B (excluding the default branch), its parent is the branch P whose tip is an
  ancestor of B and whose merge-base with B is closest to B (most commits shared);
  fallback parent is the default branch. Render each chain as a Graphite-style column:
  main → part-1 → part-2 → part-3.
- **Pages:**
  - `/` — repos, each with its stacks summarized (branch names, commits-ahead counts).
  - `/:repo` — Code tab: branch list, Forgejo-style (branch, stack parent, ahead count,
    last commit, CI dot).
  - `/:repo/stacks` — full stack graph for the repo, plus the merge/restack audit log.
  - `/:repo/plans` — the Plans tab (see below).
  - `/:repo/ci` — recent CI runs for the repo.
  - `/:repo/:branch` — the "PR view": commits unique to B (`parent..B`), then per-file
    Pierre diffs of `merge-base(parent,B)..B` (three-dot semantics). Banner links to
    parent and children in the stack. Branch names containing `/` are matched as a
    catch-all, so the tab names above are reserved words for branch names.
- **UI: GitHub's design language, via Primer.** The viewer is a daily driver and must read
  like GitHub. Use [Primer](https://primer.style) — `@primer/primitives` design tokens and
  `@primer/css` — bundled from npm at build time alongside the diff JS. Do not hand-invent
  a theme: colors, spacing, and type come from Primer CSS variables. Mirror GitHub's
  information architecture: a repo header with tab nav (Code / Stacks / Plans / CI),
  `Box`-style bordered lists, counter pills, branch labels, and GitHub-style file headers
  wrapping the `@pierre/diffs` components. Light and dark both work, switched by
  `prefers-color-scheme` through Primer's color modes. Components are project-owned
  `#[component]` modules under `src/components/` (the topcoat-ui copy-in pattern), so they
  stay ours to restyle. No auth of its own (the tailnet is the perimeter). Keep it small —
  this is a reader, not a forge.
- **Bind** `127.0.0.1:8090` (tailscale serve fronts it on :8443). Config via env only:
  `DGIT_URL`, `GIT_TOKEN`, `NASHGIT_REPOS`, `NASHGIT_MIRRORS`, `NASHGIT_BIND`,
  `NASHGIT_DB`, `NASHGIT_WEBHOOKS`, `NASHGIT_CI_LOGS`.

## Acceptance criteria

- `cargo nextest run` green; tests cover stack inference (fixture repo built in a tempdir
  with real `git` commands: main + two stacked branches + one independent branch) and the
  diff endpoint (returns per-file unified diffs for a known fixture).
- `cargo run` with env pointed at a local bare repo dir serves all three pages.
- Diff pages render with @pierre/diffs (real component, not a re-implementation).
- No public listener: binds loopback only.
- Merge/restack covered by tests against fixture repos (real `git`, tempdir): merge into
  parent, blocked-on-red-CI, restack of a two-child stack, and restack-conflict abort
  leaving every branch untouched.
- Comment round-trip test (post, render inline, outdated after branch moves), including a
  post through the public JSON API anchored to a file that is not in any diff, and a
  `GET ?since=` cursor test proving ordering by `created_at` and no repeats.
- Webhook test against a local listener (push + ci_finished payloads).
- Plans: a fixture repo with `plans/*.md` lists and renders them, and the raw URL returns
  the file bytes unchanged.
- Board: a fixture repo with `tasks/` cards in three statuses renders three columns; the
  move endpoint rewrites the front-matter status only (body byte-identical) in exactly one
  commit; a card with malformed front matter lands in "needs attention" instead of
  crashing the board.
- Links: a back-link scan on a fixture repo wires plan↔card↔branch in both directions, a
  dangling ref renders as missing without an error, and the merge tests cover a merge
  flipping its card to `done`.
- Brain: `/brain` aggregates a two-repo fixture into the documented shape; `/brain/ask` is
  tested against a stub HTTP server standing in for the Anthropic API (success, refusal,
  429 passthrough) with no real API calls, and the route 404s without `ANTHROPIC_API_KEY`.
- Degradation: with `DGIT_URL` pointed at a dead host, every page still renders (200) from
  the existing mirror and shows the stale banner.
- No hostname, bucket, account id, or tailnet name appears in the source tree.
- Rust edition 2024, plain axum-under-topcoat idioms as topcoat dictates; follow the
  framework, don't fight it.

## CI/CD

Polling CI, built into the viewer (dgit emits no webhooks):

- On mirror fetch, any branch tip not seen before is enqueued. One global worker runs
  jobs serially (`// ponytail: serial queue; parallelize per-repo if it ever backs up`).
- A job: fresh `git worktree`/clone of that commit into a scratch dir, run `.nashgit/ci`
  from the repo root if present and executable (else no job), 30-minute timeout, capture
  combined output to a log file, record `(repo, branch, commit, status, duration)` in
  SQLite. Nonzero exit = red.
- CD is not a separate system: the script deploys if it wants to. Jobs get `GIT_TOKEN`,
  branch/commit env vars (`NASHGIT_REPO`, `NASHGIT_BRANCH`, `NASHGIT_COMMIT`), nothing else.
- UI: status dot per branch in stack views, `/:repo/:branch/ci` shows the log (plain
  `<pre>`, ANSI stripped). A `/:repo/:branch/ci/rerun` POST re-enqueues the tip.

## Users (via Tailscale)

No accounts. `tailscale serve` injects `Tailscale-User-Login` / `Tailscale-User-Name`
headers on every proxied request; the viewer trusts them (the tailnet is the perimeter)
and stamps them on comments, merges, and reruns. Requests without the headers (direct
loopback hits) show as `local`. No ACLs beyond tailnet membership.

## Plans

Plans are first-class. By convention, every markdown file under `plans/` in a repo is a
plan document.

- `/:repo/plans` lists the plans on the default branch (`?branch=` to pick another).
- `/:repo/plans/{*path}` renders one, with Primer `markdown-body` styling, the
  file-anchored comment thread for that file, and a link to its raw URL.
- `/:repo/raw/{branch}/{*path}` serves any file from the mirror verbatim
  (`text/plain; charset=utf-8`), so external tools can fetch a plan by a stable URL.

## Board

A kanban board, GitHub Projects-style. **The cards live in the repo; the board is only a
view.** No board state goes in SQLite — git is the store, so an agent moves a card the same
way it moves anything else: edit the file, push.

- Convention: every markdown file under `tasks/` is a card, with YAML front matter:
  - `status` (required, string). `todo`, `doing`, `done` are the canonical columns in that
    order; any other status becomes an extra column after `done`, alphabetically.
  - `title` (optional; defaults to the first heading, then to the filename).
  - `assignee` (optional).
  - Body is the card detail.
- `/:repo/board` renders one column per status, cards ordered by mtime in the mirror,
  newest at the top. A file whose front matter will not parse lands in a **needs
  attention** column — a bad card never breaks the board.
- Clicking a card opens it rendered like a plan, with its file-anchored comment thread.
- Drag and drop moves a card between columns with native HTML drag-and-drop and no
  library. `POST /:repo/board/move {file, status}` rewrites **only** the status line of the
  front matter, commits to the default branch as the Tailscale user
  (`Name <login>`), pushes to dgit, then refetches the mirror. The push must succeed
  before the endpoint reports success, so the mirror and dgit never diverge; on failure the
  UI shows a toast and the card snaps back.
- `board` joins the reserved branch-name words.

## Links

Everything links to everything, GitHub-style. The mechanism is file-native: links are
declared in front matter or inferred from paths. **Nothing about a link is stored in
SQLite.**

- **Declared refs**, in the front matter of any plan or card: `branch: <name>`,
  `plan: plans/x.md`, `tasks: [tasks/a.md, ...]`. A ref whose target does not exist renders
  as plain text with a subtle "missing" marker and never breaks the page.
- **Path autolinking**: in rendered markdown, a token that matches a file that exists in
  the repo (under `plans/` or `tasks/`) becomes a link to that file's rendered page, and a
  backticked token matching an existing branch name becomes a link to that branch's PR
  view.
- **Computed back-links**, derived at render time by scanning front matter across the
  mirror tip. One scan per tip commit, cached, invalidated when the tip moves.
  - The branch page is the hub: it shows the card and the plan that declare
    `branch: <this>`, alongside the CI status it already carries.
  - A card shows its branch's CI dot and stack position, and its plan link.
  - A plan shows the cards and branches that reference it, each with status and CI.
  - Board cards carry their branch's CI dot inline.
- **One piece of automation**: when the merge button merges branch B, any card declaring
  `branch: B` whose status is not `done` is rewritten to `done` in the same push, as a
  separate commit authored by the merging user. The merge audit line says so.

## Comments

- PR-level comments and line-anchored comments on diff pages, stored in the viewer's
  SQLite: `(repo, branch, file, line_anchor, commit, author, body, created_at)`. Line
  anchors reference the new-side line of the diff at the commit it was made against; when
  the branch moves, show stale-anchored comments in a "outdated" section rather than
  re-anchoring. Markdown rendered with a plain renderer; no reactions, no edits, delete
  own comments only.
- `@pierre/diffs` supports annotation slots — use them for inline display.
- **Comments are line-anchored to any file at a commit, not only to files in a diff.** A
  plan under `plans/` takes comments the same way a changed source file does, and they
  render inline when that file is viewed.
- **Public JSON API, for external tools.** Comments must be postable by something other
  than the UI (the author's plannotator fork pushes plan-review feedback through it):

  ```
  POST /:repo/comments
  {"branch": "main", "file": "plans/foo.md", "line": 12, "body": "...", "author": "..."}
  ```

  `file` and `line` are optional (omit both for a PR-level comment). `author` is optional
  and falls back to the `Tailscale-User-Login` header, then to `local`. Responds
  `201` with the stored comment as JSON.

  The read side closes the loop for coding agents:

  ```
  GET /:repo/comments?branch=&file=&since=
  ```

  `since` is RFC3339. Every row carries a stable integer `id`, and results come back
  ordered by `created_at` (then `id`), so an agent can poll with a `since` cursor and never
  miss or repeat a comment. Response shape is the POST body plus `id`, `author`,
  `created_at`, and `commit`. Document both in the README.

## Brain

The API exposes the whole tailnet's work state as one queryable surface, plus an optional
subjective layer on top of it.

- `GET /brain` — a deterministic JSON aggregate across every configured repo, built from
  the mirrors and SQLite with no model in the loop. Per repo: branches (stack parent, ahead
  count, latest CI status), plans (path, title, front-matter refs, first paragraph), cards
  grouped by status with their declared refs, recent activity (merges, restacks, comments,
  CI runs — each with an author and an RFC3339 timestamp), and open comment counts per
  file. `?repo=` filters to one repo; `?since=` bounds the activity arrays. This is what a
  chief-of-staff agent slurps to know the state of everything. Cached per set of tips, with
  the same invalidation as the back-link scan.
- `POST /brain/ask {question, repo?}` — the subjective layer, for "what should I pick up
  next", "which stack is closest to mergeable", "summarize the week". Mounted only when
  `ANTHROPIC_API_KEY` is set; without it the route answers 404 and a doctor-style line at
  startup says why. It builds the `/brain` JSON, adds the full text of any plan or card
  under the repo filter, sends exactly one request to the Claude API, and returns
  `{answer, model}`.

  The Claude API contract (Rust has no official Anthropic SDK, so this is reqwest against
  the documented HTTP shape):

  - `POST https://api.anthropic.com/v1/messages`, headers `x-api-key`,
    `anthropic-version: 2023-06-01`, `content-type: application/json`.
  - Body carries `model`, `max_tokens: 16000`, a terse `system` role, and one user message
    holding the state JSON and the question. Model comes from `NASHGIT_BRAIN_MODEL`,
    defaulting to `claude-opus-5`. No `thinking`, no `temperature`, no `budget_tokens`.
  - `content` comes back as an array of typed blocks: concatenate the `text` ones and
    ignore the rest. `stop_reason: "refusal"` becomes a 502 carrying the refusal
    explanation; `stop_reason: "max_tokens"` appends a truncation note.
  - Five-minute timeout. An upstream failure surfaces as a 502 with the API's own error
    message — never a 500.
  - The key is read from `ANTHROPIC_API_KEY` only. It is never logged and never appears in
    any config or profile surface. `NASHGIT_ANTHROPIC_URL` overrides the base URL so tests
    can point at a stub.

## Agents

Coding agents use nashgit as their planning system, so the loop must be documented for a
machine reader. An `AGENTS.md` at the repo root spells it out end to end: push a markdown
plan to `plans/` on a branch, humans annotate it in the viewer (or plannotator posts to the
comment API), the agent polls `GET /:repo/comments?file=plans/x.md&since=<last-check>`,
revises, force-pushes, and a human merges. Exact `curl` examples with placeholder host and
token, terse and precise, no marketing.

It also documents the `tasks/` card convention — agents create and move cards by editing
files and pushing, exactly like plans; the board's move endpoint exists for humans
dragging — and a Brain section: `GET /brain` for state, `POST /brain/ask` for judgment
calls, with curl examples.

## Merge and restack

The write path to git (the viewer pushes to dgit with `GIT_TOKEN`):

- **Merge button** on `/:repo/:branch`: merges the branch into its stack parent in a
  scratch worktree (`--no-ff` merge commit; fast-forward when the parent hasn't moved),
  pushes the parent, then offers branch deletion. Merge is blocked while CI for the tip
  is red or running — override requires a confirm step.
- **Restack button**: after a parent moves (merge or new commits), rebase each descendant
  branch in the stack onto the new parent tip, in order, force-pushing each. Any rebase
  conflict aborts the whole restack cleanly (no partial force-pushes) and reports the
  conflicting file list; conflicted restacks are finished in the terminal.
- Every merge/restack records `(who, what, when, old tip, new tip)` in SQLite — that log
  is the audit trail, shown on the repo page.

## Webhooks

Outgoing only (dgit emits none; the viewer's poller is the event source). `$NASHGIT_WEBHOOKS`
maps events to URLs (JSON file path). Events: `push` (new tip seen), `ci_finished`
(status, log tail), `merged`, `restacked`. Delivery: POST JSON, 10s timeout, one retry,
failures logged not queued. `// ponytail: fire-and-forget; add a delivery table if a
consumer ever needs replay`.

## Inspiration

Forgejo is the reference for what a small forge should surface — branch/commit lists that
answer "what changed and is it green", a PR page that reads top-to-bottom, CI status
visible where you decide to merge. Steal its information hierarchy, not its scope: we
deliberately have no issues, users, orgs, or merge button (see Non-goals). Where a page
here has a Forgejo equivalent, match the fields Forgejo shows before inventing new ones.

## Non-goals

Packfile parsing (shell out to real `git`; never reimplement git internals), user
accounts/passwords (Tailscale headers are identity), issues/orgs, incoming webhooks,
comment editing/reactions, conflict resolution in the browser (conflicted restacks are
finished in the terminal).
