# nashcode — personal git hosting on the tailnet

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
  repo under `$NASHCODE_MIRRORS` (default `~/mirrors`). A page renders immediately from
  the mirror on disk. The fetch that brings that mirror up to date runs in the background,
  still behind a short (~10s) debounce, and the next page load sees its result. Only one
  fetch per repo runs at a time. A repo with no mirror on disk yet is the one exception:
  its first request blocks on the clone, because there is nothing to render. All git
  questions are answered by shelling out to `git` against the mirror. Auth to dgit: basic
  auth `x:$GIT_TOKEN`.
- **Repo discovery:** `$NASHCODE_REPOS` env var, comma-separated repo names, matching dgit
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
  - `/:repo` — Code tab, laid out like a GitHub repo home: the root directory listing
    of the default branch (directories first, then files), and the repo's README
    rendered below it. Directory rows link to `/:repo/tree/:path` (same listing,
    deeper; a README in that directory renders below it too); file rows link to
    `/:repo/blob/:path` (markdown rendered, everything else shown as code, binaries
    offered as a download link). `tree` and `blob` join the reserved words the branch
    catch-all must not swallow. The Forgejo-style branch list moves to the Stacks tab.
  - `/:repo/stacks` — full stack graph for the repo, the branch list (branch, stack
    parent, ahead count, last commit, CI dot), plus the merge/restack audit log.
  - `/:repo/docs` — the wiki: every markdown file in the repo, sidebar-navigable (see
    "Docs (wiki)").
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
  `prefers-color-scheme` through Primer's color modes.
  - **Icons: [Phosphor](https://phosphoricons.com)** everywhere an icon is needed —
    branch, commit, comment, CI dots, board columns, tabs. Bundled at build time from
    npm (`@phosphor-icons/web`), never a CDN: the app must work tailnet-only offline.
    One weight (regular), used consistently.
  - **Type: a three-tier hierarchy, all self-hosted in the asset bundle with license
    files, no CDN.**
    1. *Code surfaces* — diffs (`@pierre/diffs` content), file/blob views, CI logs,
       commit hashes, anything read as code — **IBM Plex Mono** (OFL; npm
       `@ibm/plex-mono`), wired into the `@pierre/diffs`/Shiki rendering as the code
       font-family.
    2. *Personality surfaces* — headings, tab nav, buttons, labels, counters, branch
       pills — **[Departure Mono](https://departuremono.com)** (OFL; vendored woff2
       with its license, since npm has no official package).
    3. *Long-form markdown body* (plans, cards, comments) — Primer's system font
       stack, where a pixel font would tire the eyes at paragraph length.
  - This refines, not replaces, the Primer direction: Primer's tokens still own color,
    spacing, and layout. Where Primer ships octicon-specific or font-stack rules that
    fight this, override them in the small project-owned CSS layer — don't fork Primer.
  Components are project-owned
  `#[component]` modules under `src/components/` (the topcoat-ui copy-in pattern), so they
  stay ours to restyle. No auth of its own (the tailnet is the perimeter). Keep it small —
  this is a reader, not a forge.
- **Bind** `127.0.0.1:8090` (tailscale serve fronts it on :8443). Config via env only:
  `DGIT_URL`, `GIT_TOKEN`, `NASHCODE_REPOS`, `NASHCODE_MIRRORS`, `NASHCODE_BIND`,
  `NASHCODE_DB`, `NASHCODE_WEBHOOKS`, `NASHCODE_CI_LOGS`, `NASHCODE_TRACES`, `NASHCODE_URL`.

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
- Prompts: a prompt recorded in a session shows up on the prompts page with its session
  and the commit that followed it, and `?q=` finds it by substring.
- Traces: recording a session's events attributes the commits made between them to that
  session; the same batch posted twice stores one copy; `nashcode hook` exits 0 with the
  server down and with garbage on stdin; a session page renders its events and its commits.
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
- A job: fresh `git worktree`/clone of that commit into a scratch dir, run `.nashcode/ci`
  from the repo root if present and executable (else no job), 30-minute timeout, capture
  combined output to a log file, record `(repo, branch, commit, status, duration)` in
  SQLite. Nonzero exit = red.
- CD is not a separate system: the script deploys if it wants to. Jobs get `GIT_TOKEN`,
  branch/commit env vars (`NASHCODE_REPO`, `NASHCODE_BRANCH`, `NASHCODE_COMMIT`), nothing else.
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

## Code browser parity

The tree/blob pages grow toward GitHub's file browser. Order of value: read well first,
then navigate, then write.

- **Syntax highlighting, client-side, with the shiki already shipped.** The diff
  renderer's shiki is in the bundle and code-split; the blob page tags its `<pre>` with
  the language (by extension) and `app.js` highlights it with that same shiki, loading
  only that grammar's chunk. Theme follows Primer's color mode. No language match, or
  JS off — the plain `<pre>` stands.
- **Line numbers and anchors.** Every line gets a numbered gutter cell and an `L{n}`
  id. Clicking a number sets `#L10`; shift-click extends to `#L10-L20`; loading a URL
  with a hash highlights the range and scrolls to it. Files above ~5000 lines skip
  highlighting, never numbering.
- **Edit in the browser, one commit.** A pencil on the blob header opens
  `/:repo/edit/:path` — a textarea with the file, a commit-message field. Submitting
  commits to the default branch as the Tailscale user and pushes through the same
  write path as the board (push succeeds before the page says so; on failure an error
  card, mirror and dgit never diverge). "New file" on the tree page is the same form,
  empty. No deletes, no renames — git is there for those.
- **Symbol jump arrives with code intelligence**, not before: once `/code/def` and
  `/code/refs` answer, blob identifiers become clickable (forward to the definition,
  a references panel backward). No interim heuristic — a jump that lands wrong
  teaches distrust.
- Raw stays a link on every blob header (already true).

## Docs (wiki)

Every repo's markdown is its wiki. There is no separate wiki store: the pages are the
files already in git — READMEs, `docs/`, `lat.md`, design notes — so agents edit the wiki
with ordinary commits and the wiki is always at the version you are looking at.

- `/:repo/docs` — the wiki home: `docs/index.md` if present, else the root README. A
  persistent sidebar lists every markdown file in the repo as a tree (directories
  collapsible, current page highlighted), so any page is reachable in one click.
- `/:repo/docs/*path` — any markdown file rendered in the same frame. Relative links
  between markdown files rewrite to their `/docs/` equivalents, so a repo whose docs
  cross-reference each other on GitHub navigates the same way here.
- `lat.md`, when present, is pinned to the top of the sidebar: it is the contract agents
  load, so it is the page a person most often needs to re-read.
- Rendering reuses the plans renderer (escaping and all). Non-markdown files are not the
  wiki's business; they belong to `/blob/`.
- `docs` joins the reserved branch-name words. No web editing — git is the editor, by
  design.

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

## Traces

The transcript and the code are the same artifact seen from two sides. A commit answers
"what changed"; the trace that produced it answers "why, and what was tried first". nashcode
stores both and lets you cross-reference them.

- **A trace is a session.** One agent run: its prompts, its tool calls, and the commits it
  produced. Sessions are identified by the agent harness's own session id.
- **Storage is nashcode's, not git's.** Traces live in SQLite plus raw transcript files under
  `$NASHCODE_TRACES` (default `$NASHCODE_MIRRORS/traces`). They are append-heavy and large;
  committing them would bloat every clone and fight the plans/cards model, where git is the
  store precisely because those files are small and human-edited. The *link* is git-native:
  a commit SHA.
- **Linking is automatic and needs nothing from the agent.** Every recorded event carries
  the repo's `HEAD` at the moment it happened. When `HEAD` moves between two events of a
  session, that commit is attributed to the session. No agent cooperation, no convention to
  remember, no commit-message trailer.
- **Pages: one Agent tab.** The Traces and Prompts tabs merge into a single tab named
  Agent. Reading a session should feel like reading the conversation, not a log table.
  - `/:repo/agent` — sessions, newest first: agent, when, first prompt as the title,
    event count, commits produced. Above the list, a prompt search across all sessions
    (the old `/prompts` behavior): `?q=` filters by substring, `?session=` narrows to
    one run, and the same URL returns JSON for `Accept: application/json`.
  - `/:repo/agent/:session` — the full conversation, top to bottom:
    - **Everything the person wrote** and **everything the agent wrote**, rendered as
      markdown. Thinking blocks collapsed behind a disclosure.
    - **Every tool call**: tool name plus its telling argument (the Bash command, the
      file path) always visible; the full input JSON behind a disclosure.
    - **Every tool result**, collapsed by default — except errors, which are open and
      styled as errors. An agent run that failed should be findable by scrolling.
    - **File changes as Pierre diffs.** A tool call that edited a file (Edit, Write,
      and friends) renders the change with the same `@pierre/diffs` pipeline as the
      branch page. The diff comes from the transcript itself (Claude Code's
      `toolUseResult.structuredPatch`, or synthesized from the tool input when the
      harness gives enough to reconstruct one) — no git required, so unpushed and
      since-rewritten changes still display.
    - Commits attributed to the session linked inline where `HEAD` moved.
  - The renderer understands two payload shapes natively: live hook events
    (`prompt`/`tool_name` fields) and raw Claude Code transcript lines
    (`type: user|assistant|system`, `message.content` block arrays). Unknown shapes
    with recognizable content degrade to the one-line summary.
  - **Bookkeeping rows are dropped, not summarized.** A raw transcript carries
    harness state lines (`last-prompt`, `mode`, `permission-mode`, `bridge-session`,
    `attachment`, `file-history-snapshot`, …) that hold no conversational content.
    Rendering them repeats the type name twice and says nothing; a backfilled session
    opened with dozens of such rows before the first prompt. An event whose payload
    yields no readable piece — and whose `HEAD` move produced no commit — renders
    nothing. Events with attributed commits always render, whatever their shape.
    The JSON APIs still return every stored event; only the HTML page filters.
  - `/:repo/traces...` and `/:repo/prompts` redirect (301) to their `/agent`
    equivalents. The JSON APIs under `/traces` keep their paths; agents already push
    to them.
  - The branch page and every commit list gain a trace link where a session is known, so
    you get from a diff to the conversation that wrote it in one click.
- **API:**
  - `POST /:repo/traces/events` — a batch of events. Idempotent on `(session, seq)`, so a
    retry never double-writes.
  - `POST /:repo/traces/:session/transcript` — the raw transcript, stored verbatim.
  - `GET /:repo/traces`, `GET /:repo/traces/:session` — read back as JSON.
  - `GET /:repo/commits/:sha/trace` — the session(s) that produced a commit.
- **Prompts are first-class.** What you asked for is the most re-readable part of a
  trace, and it should not be buried in a session's event list.
  - `/:repo/prompts` — every prompt you have written in that repo, newest first, each
    with the session it belongs to, the commits that followed it, and a link into the
    trace at that point. `?q=` filters by substring. `?session=` narrows to one run.
  - The same URL returns JSON for `Accept: application/json`, so a prompt library is
    greppable from a script.
  - A prompt is any recorded event whose payload carries a `prompt` field, so this works
    for any harness that reports one without extra configuration.
- **Privacy:** a transcript can contain anything the agent saw, secrets included. nashcode
  does not redact. The tailnet is the perimeter here as everywhere else, and that is a
  deliberate, documented choice rather than an oversight.

## CLI

The same binary is the server and the agent-side client, so there is one thing to install.

- `nashcode serve` — run the viewer. The default with no arguments, so existing service
  files keep working.
- `nashcode hook` — read one agent-harness hook payload as JSON on stdin, record it, exit 0.
  **It must never fail an agent's turn**: unreachable server, malformed payload, or no
  configured repo all exit 0 quietly. Errors go to stderr only when `NASHCODE_DEBUG` is set.
  This is what a Claude Code `PreToolUse`/`PostToolUse`/`UserPromptSubmit`/`Stop` hook runs.
- `nashcode trace push` — upload a full transcript file for a session, for backfilling a run
  that happened without the hook installed.
- `nashcode trace list` / `nashcode trace show <session>` — read traces from the terminal.
- `nashcode doctor` — print what is configured and what is missing, and exit non-zero when
  the server is unreachable.

The client half reads `NASHCODE_URL` (default `http://127.0.0.1:8090`) and infers the repo
from the git remote of the working directory, falling back to `NASHCODE_REPO`.

Both README and AGENTS.md document the hook wiring with a copy-pasteable settings snippet.

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
    holding the state JSON and the question. Model comes from `NASHCODE_BRAIN_MODEL`,
    defaulting to `claude-opus-5`. No `thinking`, no `temperature`, no `budget_tokens`.
  - `content` comes back as an array of typed blocks: concatenate the `text` ones and
    ignore the rest. `stop_reason: "refusal"` becomes a 502 carrying the refusal
    explanation; `stop_reason: "max_tokens"` appends a truncation note.
  - Five-minute timeout. An upstream failure surfaces as a 502 with the API's own error
    message — never a 500.
  - The key is read from `ANTHROPIC_API_KEY` only. It is never logged and never appears in
    any config or profile surface. `NASHCODE_ANTHROPIC_URL` overrides the base URL so tests
    can point at a stub.

## Code intelligence

Three complementary indexes make a repo queryable by an agent, all refreshed by the same
trigger and all fronted by brain: full text answers "where does this string appear",
embeddings answer "what is *about* X", and the code graph answers "who calls this".

Rust-first: everything that can run in-process does (fastembed, the codanna library,
the `scip` crate for reading indexes). Non-Rust tools appear only as CI subprocesses
where accuracy demands them (`scip-typescript`, `scip-python`), and losing one degrades
that language to the in-process graph, never breaks the pipeline.

- **Trigger: every merge to the default branch** (the same post-merge point that flips
  cards), plus a `nashcode index [repo]` CLI command for manual runs and backfills.
  Indexing runs on the CI queue, never on a request path.
- **Incremental by content.** Chunks and graph entries are keyed by blob SHA, so an
  index run touches only files whose blobs changed. A full rebuild is just the
  degenerate case of everything having changed.
- **Full text** needs no stored index: `git grep` against the mirror at query time,
  exposed as `GET /:repo/code/text?q=`.
- **Embeddings: fastembed**, in-process (ONNX), no sidecar service. Code is chunked
  (per function where tree-sitter can parse it, per ~50-line window where it cannot),
  embedded with a code-retrieval model (`NASHCODE_EMBED_MODEL` selects it; the default
  is pinned in NOTES.md once benchmarked), and stored in SQLite as vector blobs.
  Query: `GET /:repo/code/similar?q=` — the query is embedded, brute-force cosine over
  the repo's chunks, top-k with file, line range, and snippet. Brute force is a
  deliberate ceiling: these are personal repos, not monorepos; an ANN index earns its
  place only when a scan measurably hurts.
- **Code graph: the indexer is chosen by the research pass** (SCIP-family is the
  working assumption; the decision and its date go in NOTES.md). Whatever the tool, the
  contract is: it runs headless on merge, emits definitions/references/call edges for
  Rust, Python, and TypeScript, and the server loads the result into SQLite tables it
  owns. Queries: `GET /:repo/code/def?symbol=`, `GET /:repo/code/refs?symbol=`,
  `GET /:repo/code/callers?symbol=`. A language the indexer cannot parse degrades to
  text search, never to an error.
- **Brain is the front door.** `GET /brain` grows a per-repo `code` stanza (index age,
  chunk and symbol counts). `POST /brain/ask` gains tool access to the three query
  endpoints so "where is retry handled and who calls it" is answerable in one question.
  The JSON endpoints stay public individually — an agent that knows what it wants
  should not pay for a model round-trip.

## Advisor

`lat.md` states the project's rules; the advisor reads a merged diff against them and
says what it thinks. It is a reviewer, never a gate.

- **Advisory only, by design.** A language model's judgment is probabilistic; a merge
  gate must not be. Anything worth blocking on becomes a deterministic check in
  `.nashcode/ci`. The advisor's job is the gray zone: "this new module duplicates what
  `ops.rs` already does", "rule 4 says errors are typed, this returns String".
- **Trigger: on merge to the default branch**, on the CI queue, for repos that have a
  `lat.md`. Input: the merged diff plus `lat.md`, nothing else. Output: zero or more
  findings.
- **Findings are comments.** Each finding lands in the existing comments system,
  anchored to the file (and line where the model gives one), authored as `advisor` so
  it is filterable and cannot impersonate a person. No new UI: the branch page and
  comment feeds already render them.
- **The model is local and env-configured.** `NASHCODE_ADVISOR_URL` points at any
  OpenAI-compatible completions endpoint on the tailnet (ollama, llama.cpp,
  whatever); `NASHCODE_ADVISOR_MODEL` names the model. Unset means the advisor is off —
  same pattern as `/brain/ask` and `ANTHROPIC_API_KEY`.
- **Degrades to silence.** An unreachable endpoint, a timeout, or an unparseable
  response records one line in the audit log and posts nothing. A flaky advisor that
  spams half-findings would teach its reader to ignore it, which is worse than absent.
- Advisor comments carry a dismiss affordance like any comment thread; dismissing is a
  normal comment resolution, no special machinery.

## Agents

Coding agents use nashcode as their planning system, so the loop must be documented for a
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

Outgoing only (dgit emits none; the viewer's poller is the event source). `$NASHCODE_WEBHOOKS`
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
