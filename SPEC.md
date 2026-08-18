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
  post through the public JSON API anchored to a file that is not in any diff.
- Webhook test against a local listener (push + ci_finished payloads).
- Plans: a fixture repo with `plans/*.md` lists and renders them, and the raw URL returns
  the file bytes unchanged.
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
  `201` with the stored comment as JSON. `GET /:repo/comments?branch=&file=` reads them
  back. Document both in the README.

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
