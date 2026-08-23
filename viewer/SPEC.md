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
- **Repo discovery:** the viewer finds repos itself. On every mirror poll cycle it
  fetches dgit's index page (`GET $DGIT_URL/`), parses the repo names with the same
  parser `nashcode ls` uses, and unions every plain name into its repo set; a name seen
  for the first time gets its mirror cloned like any other repo. `$NASHCODE_REPOS`
  (comma-separated) seeds the set and is optional; a name listed there is never dropped,
  even when dgit stops listing it. No name is ever removed from the set. A failed index
  fetch logs one warning and changes nothing. When `$DGIT_URL` is a filesystem path (the
  test setup), the `*.git` directories in it are the index. Known gap: dgit hides
  `private: true` repos from its index with or without credentials, so a private repo
  appears only through `$NASHCODE_REPOS`.
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

## Context

What the work is about, filed where the work is. A meeting, an email, a pasted chat, or
a note becomes one committed markdown file in the repo it concerns; a digest on the
operator's machine turns those files into the memory a session reads before it
searches. The server accepts files and indexes them. It never fetches email, chat, or
audio.

Two layers, both plain files on the default branch:

- `context/<kind>/YYYY/MM/<id>.md` is provenance: raw, filtered at the source, edited
  only by the digest. It exists so a claim in `brain/` can be traced.
- `brain/entities/<slug>.md` is memory. The digest writes it. Every fact line carries a
  date and a source path. Nothing here is the server's to write.

### Writing

- `POST /:repo/context/:kind` files one item. Kinds: `meeting`, `email`, `chat`,
  `note`. Any other kind is `400`.
- Body for `meeting` is the browser extension's transcript (title, RFC3339
  `started_at`/`ended_at`, speakers, segments, optional calendar event and action
  items); `at` is `started_at` and `source` is `meeting_url`. Body for the other kinds
  is `{title, at, text, source?}`; `at` is RFC3339, `text` is the body verbatim.
  `source` is the provider's stable id: a Gmail message id, a chat thread plus day, a
  URL.
- A payload that cannot be filed (no segments, no speakers, a segment naming an
  undeclared speaker, times that run backwards, an empty `text`, an `at` that does not
  parse) is refused with `400` and the reason. Nothing is committed.
- The id is the UTC `at` minute plus a slug of the title. With `source`, the id ends in
  the first 8 hex of `sha256(source)`, and a put whose file already exists at the
  default-branch tip commits nothing and answers `200 {ok, existing: true, id, path}`.
  Without `source`, a name already taken gets a `-2`, `-3`, … suffix, so a same-minute
  same-title item never overwrites an earlier one.
- A new file answers `201 {ok, id, path, commit}`. The commit lands on the default
  branch through the same write path as a board move: committed as the Tailscale user,
  pushed to dgit before the response says so, then the mirror refetched.
- Front matter is `kind`, `id`, `title`, `at`, `ingested_at` (the server clock, RFC3339
  with milliseconds), `source` (when given), `entities: []`, `digested: false`. A
  meeting keeps its existing keys too (`ended_at`, `speakers_confirmed`,
  `calendar_event_id`, `attendees`, `provider`) and its body: the action items, then
  one line per turn with consecutive segments by one speaker merged. For the other
  kinds the body is `text`.
- Nothing about a context item lives in SQLite; the file is the record.

### Reading

- `GET /:repo/context/:kind/:id` answers the front-matter fields plus `body`, read at
  the default-branch tip. Unknown id: `404`.
- `GET /:repo/context?kind=&since=` lists items at the default-branch tip, ordered by
  `(ingested_at, kind, id)`, as `{items: [{kind, id, path, title, at, ingested_at,
  source, digested, entities}], next_since}`. `since` is the opaque `next_since`
  string from a previous answer (`ingested_at|kind/id`) and is strictly exclusive: an
  item equal to the cursor is not repeated. `at` is never the cursor; a backfilled
  item is older than the items around it, and two items can share a minute.

### Memory

- `GET /brain?repo=` gains `memory` per repo: the newest twenty `brain/entities/*.md`
  by last commit, each `{slug, path, updated_at, facts}` where `facts` is the file's
  last three fact lines, plus `undigested` (context files still `digested: false`) and
  `conflicts` (entity files with a `## Conflicts` section). `nashcode brain` prints
  the same. The SessionStart hook already injects brain, so a session sees memory
  before it searches.

### The digest

Not the server's. A runner on the operator's machine checks the repo out at tip,
runs Claude Code headless with the `context-digest` skill and no shell, and checks the
diff before it commits: a changed path outside `context/`, `brain/`, `tasks/` aborts the
run and leaves the checkout dirty for a person. The runner makes one commit per digested
file and pushes only to a remote on the nashcode host; a GitHub remote is refused by
name. Claude never runs git. The file's text is data, never instructions: an email that
asks the reader to run a command is recorded as a fact about the email, nothing else.

### Your to-do list

Cards are the record; one Google Tasks list is the view, and it is short: only what the
operator said is theirs and is top of mind. `bin/context-tasks`, run by the digest
runner inside the same lock and before the same push, mirrors cards to that list and
back. The list has other sources too; the sync touches only tasks it created.

- The digest sets `assignee: <me>` and `top: true` on a card only when the operator
  claimed the item in the first person ("action items from me are…", "I will…"). A
  mention is not a claim. Everything else stays a board card.
- A card with `top: true`, status `todo` or `doing`, and no `gtask` key becomes one
  task titled `[<repo>] <title>`, with the card path in the notes and the card's `due`
  when it has one. The id is written back as `gtask`, one commit per card.
- The list stays short: `max_open` (default 7) counts every open task on the list,
  hand-made ones included. At the cap the sync adds nothing and names what waits.
- Completed in Google moves the card to `status: done`. `done` on the board completes
  the task. Deleted in Google demotes the card: `top: false`, `gtask` removed, status
  kept, so a deletion means "not top of mind", not "do it again".
- A mirrored task still open after `stale_days` (default 14, `0` disables) is removed
  from the list and its card demoted the same way, and named in the run's output.
- The sync rewrites only the `status`, `top`, and `gtask` lines of a card's front
  matter, the board's rule. Tasks the operator adds by hand in Google are never pulled
  into the repo.

### Reserved words

- `context` joins `RESERVED_ROUTES` next to `brain`, so discovery refuses a repo with
  either name, and the reserved branch-name words.
- `/:repo/transcripts` is removed, not aliased. A repo with a `transcripts/` directory
  is migrated by one commit that moves it to `context/meeting/` and adds `ingested_at`
  and `source` to each file's front matter.

## People

Who belongs to which project, so an inbox routes by who wrote. A client texts, mails,
and joins a Meet; the calendar, Gmail, and Messages each know *who*. One file on the
operator's machine joins who to which project, and every consumer reads that file.
The viewer holds a pushed copy and answers one question: which project. It never sees
the Mac's file and never hands the copy back out.

### The file

- `~/.nashcode/people.json` (`NASHCODE_PEOPLE` overrides the path), hand-editable, the
  only source of truth. It is not a git repo: the router and the desktop app read it
  without a checkout, and the numbers in it are for no mirror.
- `me` is the operator's own emails and phones. `people` is `[{id, name, phones,
  emails}]`. `projects` is `[{id, name, folder, repo?, people, chat_ids, imsg: {prompt,
  enrich, media_only}, email: {account, query?}}]`. `people` on a project lists person
  ids; `chat_ids` are iMessage group ids. `repo` is the nashcode repo name and may be
  absent for a GitHub-only client; meetings and email then have nowhere to file, and
  the consumer says so.
- Phones are E.164. Emails compare case-insensitively. `id` is the join key: a project
  naming an id that no person has is refused, by the CLI and by `PUT /people` alike.

### Routing

- One rule, in one workspace crate (`people-core`) that the viewer, the CLI and the
  desktop app all depend on, so the file's types, its validation, its routing, and the
  push client exist once: `route(contacts) -> matches`. A project scores one point per
  distinct person matched by any email or phone in the contacts. Projects with a score
  come back highest first; equal scores keep file order and the answer says `tie:
  true` when the top score is shared. The operator's own addresses (`me`, plus each
  project's `email.account`) never score.
- Chat ids are iMessage-only and are matched in Swift, before participants.

### Viewer

- `PUT /people` stores the body at `NASHCODE_PEOPLE` (default `<mirrors>/people.json`)
  with the time of the push; the same validation as the CLI, `400` and the reason on
  failure. Answers `{ok, people, projects, pushed_at}` with the two counts.
- `GET /people/route?email=&phone=` (both repeatable) answers `{matches: [{project,
  repo, folder, people: [ids], contacts: [the ones that matched], score}], tie}`. No
  contacts is `400`. Before any push it answers `404 no people file`.
- There is no `GET /people`. Client phones and emails stay on the Mac; the viewer only
  answers "which project". Reads are anonymous on the tailnet today, so the answer is
  already the least it can say: project ids, repo names, and who matched by id.
- `GET /brain` gains `people: {projects, people, pushed_at, pushed_by}` once per
  viewer, not per repo, and `null` before a push. `pushed_by` is the Tailscale login
  the push arrived from.
- `people` joins `RESERVED_ROUTES` next to `brain` and `context`.

### CLI

- `nashcode people ls` prints projects with their people. `nashcode people route
  --email … --phone …` prints the ranking the viewer would. `nashcode people push`
  puts the file. `nashcode people check` names every dangling id, duplicate id,
  project with no people, and phone that is not E.164, and exits non-zero when it
  found one. `--json` on each, in the agcli envelope.

### Consumers

- **Meet.** After the extension finds the overlapping calendar event, it asks
  `GET /people/route` with the attendees minus the signed-in user. One match: the repo
  box holds that repo and one line says why (`agstaff — Rob Castro is on the
  invite`). No event, or no match: the box holds the settings default and the line
  says so. A tie: the box is empty and each tied repo is offered.
- **Email.** `bin/context-email` reads the file. For each project with a `repo` and an
  `email.account`, the Gmail query is `(from:(a OR b) OR to:(a OR b))` over the
  project's people's emails, plus the age bound, unless the project's `email.query`
  replaces it. The per-client `[[source]]` tables are gone from
  `~/.nashcode/context.toml`; the file keeps only the runner's own settings (`host`,
  `me`, `tasklist`, `max_open`, `stale_days`), and the digest runner takes its repo
  list from the projects that have a `repo`.
- **iMessage.** imsg-router reads the file and nothing else for routing. A project's
  participants are the union of its people's phones and emails (an Apple ID handle is
  an email), compared case-insensitively, minus anything in `me`. A chat id match
  (the chat's row id in Messages) wins over a participant match; among participant
  matches the first project in file order wins. A message that arrives before the file
  exists waits; it is not marked handled. The enrichment config lives in
  `~/.imsg-router/config.json`.
- **Desktop.** `nashcode-people` (workspace member `people/`) is one canvas: three
  lanes, contacts, people, projects, with a drawn link from each phone or email to its
  person and from each person to each project they are on. Clicking a card lights up
  everything it routes through and dims the rest; an inspector beside the lanes edits
  the selected person or project, and toggles a person's projects. A contact or a
  person on no project sits in a "routes nowhere" band. Save writes the file; Push
  sends it; the status line shows the last push from brain.

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
- **Click a line to comment.** On any rendered diff, clicking a line (its number or its
  text) opens an inline composer anchored to that file and new-side line, right under
  the clicked line, with `file` and `line` carried as hidden fields; submitting posts
  through the same `POST /:repo/comments` and the comment renders in place. Clicking
  another line moves the composer; Escape or Cancel closes it. The visible numeric
  "line #" input leaves the per-file composer — the file-level composer (no line) stays
  for whole-file remarks, and the JSON API is unchanged. If `@pierre/diffs` exposes no
  line-click event, a delegated click handler over its rendered rows reading the line
  number from the DOM is acceptable; when the number cannot be read confidently, fall
  back to the file-level composer rather than mis-anchor.
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
- **`GET /:repo/code/find?q=` — the fused query, one call instead of four.** The
  caller sends what it has — an identifier, a regex, a phrase — and the server routes:
  an exact symbol match returns the definitions first (name, kind, path, line,
  snippet) with reference and caller counts attached; text hits over the chunks come
  next; when text comes back thin, an embeddings pass adds semantic hits, labeled as
  such. Every hit says which layer produced it (`definition`, `reference`, `text`,
  `semantic`) and the response header says the indexed commit and its age, because
  the index lags the working tree and the caller must know what it is looking at.
  Ranking is fixed: definitions, then references, then text, then semantic. Degrades
  like the sibling endpoints: unindexed repo answers empty with the index hint, never
  an error.
- **Brain is the front door.** `GET /brain` grows a per-repo `code` stanza (index age,
  chunk and symbol counts). `POST /brain/ask` gains tool access to the three query
  endpoints so "where is retry handled and who calls it" is answerable in one question.
  The JSON endpoints stay public individually — an agent that knows what it wants
  should not pay for a model round-trip.

## Stack (upstream dependencies)

A repo can declare the code it is built on — its upstream column — and the viewer
mirrors that column next to the repo's own code. One naming caution: the "Stacks" tab
is branch stacks; "the stack" here is the dependency column (`plans/whole-stack.md`).
The two share a word and nothing else.

- **The manifest is a commit.** `.nashcode/stack.toml` at the default-branch tip, read
  on refresh. Each `[[dep]]` carries: `name` (a plain name, unique in the file), `url`
  (an `https` git clone URL, or `http` to loopback — any other scheme, plain `http` to
  anywhere else, and any URL carrying credentials is reported in brain and never
  fetched), exactly one of `pin` (a commit id, 7 to 40 hex digits) or `track` (a
  branch), and an optional free-text `layer`. A malformed manifest degrades: brain
  carries the parse error and every other page is unaffected. No manifest, no stack,
  no cost.
- **One mirror per URL, global.** Upstream mirrors are `git clone --mirror` copies
  under `$NASHCODE_MIRRORS/up/<host>/<path>.git`, keyed by normalized clone URL, so
  two repos declaring the same dependency share one mirror. `http` and `https` of one
  host are that same mirror, which is why plain `http` off the box is refused rather
  than left to downgrade it. A URL that cannot be spelled as a directory — an empty or
  traversing path segment, a segment ending in `.git` — is refused for the same
  reason: no two URLs may land on one directory. Mirrors are read-only everywhere: no
  push, no CI, no plans, no board, no comments, no traces.
- **`pin` fetches until the commit is on disk, then never again.** A pin the upstream
  publishes on no branch or tag says so in brain rather than reading as merely behind,
  and a pin already on disk is not marked stale by a sibling dep's fetch failing.
  `track` deps refresh on a 30-minute schedule, plus `POST /:repo/stack/sync` for "I
  need it now" — itself limited to one fetch a minute per mirror, since a route anyone
  can call in a loop is otherwise an amplifier aimed at somebody else's server.
  Upstream fetches follow the mirror rules: a failure degrades to stale, and never
  blocks or fails a page.
- **Brain tells the whole story.** The per-repo `/brain` JSON grows a `stack` stanza:
  per dep its name, url, layer, mode, the declared rev, the commit actually resolved
  on disk, freshness, and any error — parse errors and fetch errors both land here.
- Mirrors are whole, not partial: dgit and celld are small. Blobless clones for a
  kernel-sized dep are a known ceiling, taken when one hurts.

Browsing the column (phase 2):

- **`GET /:repo/stack` renders the column**: the repo itself, then each dep at its
  resolved commit — name, layer, url, mode, freshness, and the commit it points at —
  each entry opening that dep's tree. One page, N trees; never a merged fake tree.
  A dep whose mirror is absent or refused renders as a card that says why, exactly
  like an unavailable repo. The tab label is "Stack", singular, next to "Stacks";
  the two pages link to each other's concept in one line so nobody guesses wrong.
- **`GET /:repo/stack/:dep/tree/{*path}` and `/:repo/stack/:dep/blob/{*path}`**
  browse the dep's mirror at the resolved commit. `?rev=` narrows to any commit
  already present in the mirror — a gitlink target, for instance — and a rev the
  mirror does not have is a 404, never a fetch. Read-only surfaces: no edit, no
  new-file, no actions, no comments. `:dep` is the manifest name, valid only under
  the repo that declared it; a declaration the manifest refused has no mirror and
  so has no page (404), while a dep that is accepted but not yet fetched has one
  that says so.
- **Submodule gitlinks link through.** A submodule entry in any tree whose
  `.gitmodules` URL normalizes onto a declared dep of the same repo becomes a link
  to that dep at the gitlink's commit; every other gitlink stays the inert label it
  is today. The link follows the declaration, not the state of the mirror — a
  gitlink pinning a commit nobody has fetched is still a link, and answers the
  ordinary 404 — so the affordance does not flicker as mirrors move.

## Architecture

A repo tab that answers "what is the shape of this system" — both the shape somebody
*intends* and the shape the analysis actually *sees*. The loop is agent-driven: an agent
downloads the full static analysis in one call, draws a mermaid diagram from it, and
submits the diagram back; the tab renders the latest submission. Humans can submit too;
the endpoint does not care who is drawing.

- **`GET /:repo/code/graph` — the whole analysis, one call.** Everything the code
  intelligence indexes know, dumped as one JSON document: the file inventory (path,
  language, blob SHA), every symbol (name, kind, file, line), and every edge
  (defines/references/calls). This is the bulk companion to the per-symbol query
  endpoints — an agent drawing a diagram must not page through `?symbol=` calls to see
  the graph. When the graph index is absent or a language is unindexed, the dump
  degrades to what exists (worst case: files only, `symbols: []`), never to an error.
  The response says what it is: `{"generated_at", "commit", "files", "symbols",
  "edges"}`.
- **`POST /:repo/architecture` — submit a diagram.** Body:
  `{"mermaid": "...", "title": "...", "note": "..."}`; `title` and `note` optional.
  Author resolution is the comments rule: `Tailscale-User-Login` header, else `local`.
  The server validates size (64 KiB cap) and stores the text verbatim in SQLite with
  author and timestamp — it does not parse mermaid; a diagram that will not render is
  the author's problem, shown as mermaid's own error box. Responds `201` with the
  stored row. Submissions are append-only history, never edits: `GET
  /:repo/architecture` with `Accept: application/json` returns the latest, `?history`
  lists all of them (id, author, created_at, title), `?id=` fetches one.
- **The tab: `/:repo/architecture`.** Renders the latest submitted diagram, its title,
  note, author, and age, with a history list to view any earlier submission. When
  nothing has been submitted, the page falls back to the mermaid blocks of the repo's
  `ARCHITECTURE.md` if it has one, else shows the `POST` recipe so the empty state
  teaches the loop.
- **Mermaid renders client-side, lazy, strict.** The mermaid library loads only on this
  page (it is an order of magnitude bigger than the whole current bundle — it must not
  ride along on every page). Diagram text is untrusted user content: mermaid runs with
  `securityLevel: "strict"`, and the source is delivered to the page as text for the
  client to render, never spliced into server HTML. Same reasoning as the markdown
  stored-XSS fix.
- **Nodes link back to the code.** A rendered diagram is a claim about the codebase;
  clicking a node shows where the codebase makes it true. Mermaid stays at
  `securityLevel: "strict"` — `click` directives inside submitted text remain dead —
  so the wiring happens after render, in the viewer's own JS, from data the server
  resolves. `GET /:repo/code/where?names=a,b,c` batch-resolves diagram labels against
  the code graph: for each name, exact symbol matches (name, kind, path, line) and
  file-stem matches (`mirror` → `viewer/src/mirror.rs`) carrying that file's defined
  symbols. Rust only for now — matches are filtered to `.rs` paths; a label from any
  other language resolves to nothing, never to an error. At most 100 names per call;
  more is a `400`. On the page, a node with at least one match gets a pointer
  affordance; clicking opens a popover naming the functions and types the node is
  made of, each a link to `/:repo/blob/<path>#L<line>`. A node with no match stays
  inert.
- **Brain sees it.** The per-repo stanza in `GET /brain` grows
  `architecture: {submissions, latest_at, latest_author}` so "which repos have a drawn
  design and how stale is it" is one question.

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

## Invariants

Rules the viewer holds, each at the lowest layer that can hold it: parser before HTTP
handler, database before app code, startup reconciliation before runtime polling. Two
doors share one rule. Every gate can be satisfied or cleared. Audit and rationale:
`plans/invariants.md`.

| Rule | Enforced at |
|---|---|
| Stack parents come from one ref snapshot; `descendants`/`walk` terminate on a cyclic graph. | `stack.rs` (`StackGraph::infer` → `Repo::tips()`; visited set) |
| No CI run blocks merge forever: rows in flight at open become `error` ("orphaned by restart"); a running job heartbeats every 60 s; a `running` row quiet for 5 min reads as `stuck`, which does not block and can be requeued. | `db.rs` (`Db::open`, `CiRun::effective_status`), `ci.rs`, `POST /:repo/:branch/ci/requeue` |
| Card status alphabet (`[a-z0-9-_]`, ≤ 40 chars, never `needs-attention`) is one function used by the parser and the move endpoint; a pushed violation is quarantined with `front_matter_error = "invalid status"`. | `docs.rs` (`valid_status`, `parse_document`), `api.rs` (`board_move`) |
| A branch is claimed by at most one card and one plan; more is a `conflict` on the branch page and in `/brain`, and merge refuses until one remains. | `docs.rs` (`DocIndex::conflicts`), `ops.rs` (merge precondition) |
| CI runs only when the default branch carries `.nashcode/ci.toml` with `enabled = true`; `GIT_TOKEN` enters the job env only with `git_token = true` there. Anything else is `skipped`. | `ci.rs` (`policy`, read from the default-branch tip before the clone) |
| A comment's `author` is the Tailscale actor, never a client field; `on_behalf_of` is stored apart and rendered "X via Y"; deletion is scoped to the actor. | `api.rs` (comment handler), `db.rs` (`delete_comment`) |
| Comments degrade, never vanish: `file` must exist at the anchor commit and `line` be within it (else 400); branch delete sets `orphaned_at` and the default-branch page lists them; a comment on a file gone at tip renders as outdated, file-level. | `api.rs`, `ops.rs` (`orphan_comments`), `pages.rs` |
| Every `branch:`/`plan:`/`tasks:` target that resolves to nothing is listed in `/brain` under `dangling` and badged on the board. | `docs.rs` (`DocIndex::dangling`), `brain.rs`, `pages.rs` |
| `blocks:` edges form a DAG: a cycle quarantines every card on it (`front_matter_error = "blocks cycle: …"`); a `todo` card is *ready* when every blocker is `done` (`/brain` `ready`, board `?ready=1`, `nashcode ready`, `nashcode claim`). | `docs.rs` (`blocks_cycles`, `DocIndex::ready`), `brain.rs`, `cli/src/commands/card.rs` |
| Trace `seq` allocation is one `BEGIN IMMEDIATE` transaction (retried on busy); transcripts are keyed by `sha256(session_id)` and never overwritten without `?replace=1` (409 otherwise). | `db.rs` (`insert_trace_event`), `traces.rs`, `web/traces.rs` |

## Webhooks

Outgoing only (dgit emits none; the viewer's poller is the event source). `$NASHCODE_WEBHOOKS`
maps events to URLs (JSON file path). Events: `push` (new tip seen), `ci_finished`
(status, log tail), `merged`, `restacked`. Delivery: POST JSON, 10s timeout, one retry,
failures logged not queued. `// ponytail: fire-and-forget; add a delivery table if a
consumer ever needs replay`.

## Bugs (error tracking)

nashcode is the error tracker. Full contract: `goals/error-tracking/goal.md` (settled
decisions — flag what the code disproves, do not relitigate) and
`goals/error-tracking/ingester.md` (phase 3, the public ingester). This section binds the
viewer-side surface; the goal doc binds protocol, grouping, and notification semantics.

- **Projects and DSNs.** Projects live in SQLite, created in the UI. DSN =
  `https://<32-hex-key>@<host>/<numeric-id>`; host from `NASHCODE_BUGS_INGEST_URL`. A
  project page shows the DSN and an SDK snippet, and may declare a nashcode repo for
  cross-links.
- **Revocation.** A project carries `active`. Revoking one keeps every issue and log row
  it already filed — history does not stop being true when a DSN is retired — and closes
  both doors: the tailnet ingest routes answer 404 at once, and the public ingester
  learns it on the next registry push, where `active:false` means the same as absent. A
  revoked key is *absent*, not wrong, so a sender cannot tell a retired project from one
  that never existed. There is no UI for it yet and no CLI verb: the column, the setter,
  and the registry push are the whole of it, so revoking today means writing the column.
- **Ingest.** One route, `POST /api/<project_id>/envelope/`. Auth from `X-Sentry-Auth`,
  `?sentry_key=`, or the envelope `dsn` header; 403 on key mismatch, 404 on unknown
  project, 429 over quota. Decompress gzip/deflate/br with streaming caps (1 MiB per
  item, 20 MiB compressed, 100 MiB decompressed → 413). Raw bytes to the bucket first,
  digest queued, `200 {"id":"..."}` immediately. Unknown item types are counted and
  skipped, never 400. Every 200 carries the `X-Sentry-Rate-Limits` suppression header
  and full browser CORS per the goal doc.
- **Storage split.** `NASHCODE_BUGS_BUCKET=s3://name` (+ optional `S3_ENDPOINT`, AWS env
  credentials only) holds raw payloads; SQLite holds only the index. Unset bucket = the
  feature is off: one startup line, 404 on `/bugs` and ingest routes.
  `nashcode bugs reindex` rebuilds the index from the bucket.
- **Digest.** Single writer: parse, group (`nashcode-v1`: explicit fingerprint wins,
  else last exception type + parameterized value; synthetic → crash function; native →
  debug_id + relative addr), index, alert, one transaction. Issues: unresolved /
  resolved / muted; any event on a resolved issue reopens it as a regression. The queue
  is bounded: a full queue answers `429` with `Retry-After` and the SDK backs off.
  Every accepted envelope row carries `digested_at`; a startup sweep re-digests the
  rows that have none, so a crash between the bucket write and the index write costs
  nothing.
- **Logs.** Two doors, one store. (1) `log` envelope items on the ingest route.
  (2) `POST /api/<project_id>/logs`, NDJSON, one JSON object per line (`ts`, `level`,
  `message`, free attributes), authed by the same DSN key and held to the same size
  caps. The store is a SQLite hot window on the OTel severity model (`severity_text`
  trace…fatal, `severity_number` 1–24) with an FTS5 external-content index over the
  message. Every batch archives to the bucket as one NDJSON object. A nightly prune
  drops hot rows past the project's `retention_days`; the archive object stays.
  `/bugs/:project/logs` searches it — FTS query, level filter, `file:` token, newest
  first, paged. Logs never push to Pushover.
- **Drain.** The viewer pulls from the public ingester; it never accepts an inbound
  connection. Protocol: `ingester/README.md`, "The drainer contract" — three bearer-authed
  routes, and nothing in the drainer may know that celld serves them.
  `NASHCODE_BUGS_DRAIN` is an iroh EndpointId or an `http://host:port` base URL, and both
  work identically; `NASHCODE_BUGS_DRAIN_TOKEN` is the bearer token;
  `NASHCODE_BUGS_DRAIN_KEY` is the file holding the persistent iroh secret key, whose
  EndpointId goes in the ingester's allow-file; `NASHCODE_BUGS_DRAIN_INTERVAL` is seconds
  between cycles, default 30. The iroh half is the `drain-iroh` cargo feature, off until
  it has dialled a live ingress once; a default build handed an EndpointId refuses to
  start and names the flag. Unset `NASHCODE_BUGS_DRAIN` = the drainer is off, one doctor
  line. Drain set with `NASHCODE_BUGS_BUCKET` unset is a refusal to start, not a warning:
  a drain with nowhere durable to put a payload would ack rows into nothing.
  Each cycle drains every active project after its stored cursor and replays each row into
  the door its `kind` names — `envelope` into the envelope pipeline, `logs` into the NDJSON
  one — with the same streaming caps the direct doors use. **Ack only what digest took.** A
  429 off the byte-budget queue ends the project's cycle with no ack, so the rows come back
  next time; delivery is at-least-once and the dedupe on `event_id` and `dedupe_key` is what
  makes a redelivery cost nothing. A row that can never parse is acked and counted, because
  one poison row must not wedge a project's queue for ever. The cursor per project lives in
  SQLite, so a restart replays only the unacked tail. The registry is `PUT` whole whenever
  the project set changed since the last push, never merged; an empty set is refused and
  logged rather than pushed, since emptying it takes every project on the fleet offline.
- **Pushover.** `NASHCODE_PUSHOVER_TOKEN` + `NASHCODE_PUSHOVER_USER`, both or neither;
  either one alone is a configuration error, logged, and the feature stays off. Unset =
  off, one doctor line, everything else works. `NASHCODE_PUSHOVER_URL` overrides the API
  base so a test can point the sender at a listener it started.
  State changes only — new issue, regression, unmute, cron incident, recovery — never
  per event. The escalation ladder adds one push when an unresolved issue crosses 10,
  100 or 1000 events. **Each rung rings once in an issue's life**, because the event
  counter only ever goes up: resolving an issue does not reset it, so a rung that has
  been crossed cannot be crossed again. An issue that is fixed and breaks again says so
  through its regression push, which is the state change; the ladder is about volume,
  and the volume is cumulative. Logs never push.
  Payload: title `{project}: {issue title}` truncated to 250 characters, message =
  exception value plus a few tags, truncated to 1024 and never empty, `url` = the issue
  page under `NASHCODE_URL`, `url_title` = "Open in nashcode", priority 0 and 1 for
  fatal. Emergency priority and `cancel_by_tag` are not built: they are per-project
  opt-in and wait for the crons slice.
  The queue is a SQLite table and the sender is one task. A 5xx retries after at least
  5 seconds with backoff; **any** 4xx is final and never retried, because Pushover
  answers 4xx to a message it has judged, not to one it failed to read. A 429 parks the
  whole queue until `X-Limit-App-Reset`. A local cap of 20 sent messages per rolling
  hour parks it too, and the trip sends exactly one "notifications suppressed, N
  pending" message rather than the N it is holding. `X-Limit-App-Remaining` off the last
  answer is stored and shown on `/bugs`, in that page's JSON, and in the brain stanza —
  the monthly budget is the number that decides whether a real incident will reach a
  phone.
- **Path suffix-matching.** An SDK inside a container reports the path it ran from
  (`/app/src/foo.py`) and the repo knows `src/foo.py`, so an exact match resolves
  nothing. When the reported path does not resolve, take the longest suffix of it, on
  path-segment boundaries, that names exactly one file in the repo tree at the relevant
  rev. One match links and gets a snippet; no match or an ambiguous one renders as
  plain text. Applies to log rows and to stack frames alike. The candidate set is the
  tree listing at that rev, read once per (repo, rev) and reused for the whole page.
- **Dogfood.** `NASHCODE_BUGS_SELF_DSN` points the viewer's own `tracing` errors at a
  DSN — normally one of its own projects, through the normal door. Unset = off, one
  doctor line. Two guards against a feedback loop, because an error raised inside
  digest that is reported into the same digest is a loop that ends in a full disk: the
  reporter is skipped for anything logged from inside the bugs pipeline (a re-entrancy
  flag on the reporting task), and every self-report carries a `nashcode.self` tag so a
  human can see where it came from. Grouping does the rest: a recurring internal error
  is one issue with a count, not a queue.
- **UI.** `/bugs` project list; `/bugs/:project` issues by state; issue detail with
  resolve/mute (Tailscale headers stamp the actor); later `/logs` and `/crons`. Same
  accept-header JSON convention as the rest of the viewer. Bugs summary joins `/brain`.
- **Code origin.** Log rows index `code.file.path` / `code.line.number` /
  `code.function.name` (old OTel names accepted and normalized). The logs page shows
  `file:line` per row; when the project declares a nashcode repo, it links into the code
  browser (`/:repo/blob/:path#L<n>`), and issue-detail in-app stack frames get the same
  links. Unresolvable paths render as text, never a dead link. `file:` filters log
  search.
- **Context capture.** When a log row or stack frame resolves to `file:line` in the
  declared repo, the server reads the surrounding source (±3 lines) from the mirror —
  at the commit named by `sentry.release` when that is a SHA the mirror knows,
  otherwise at the default-branch tip with a "tip, not release" marker — and shows the
  snippet inline on the log row (expandable) and the issue frame. Read on render, not
  at ingest: the mirror is local and the index stores only file/line/release. A path
  or SHA the mirror cannot answer degrades to the plain link, never an error.
- **Crons.** `check_in` envelope items store; a monitor is upserted only when a valid
  `monitor_config` accompanies the check-in. `next_checkin_latest` and `timeout_at`
  persist; a 1-minute sweep computes missed and timeout server-side (client-sent ones
  coerced). `croner` for 5-field Vixie schedules, chrono math for intervals; defaults
  checkin_margin 1 min, max_runtime 30 min. Pushover at one choke point: incident open
  (error/missed/timeout) and recovery. `/bugs/:project/crons` lists monitors and
  incidents, same JSON convention.
- **Quotas and eviction.** Pre-parse per-project quota gate → 429 + Retry-After
  (defaults 1k/5min, 5k/hour, 1M/month). Per-project max stored events (default 10k),
  Bugsink-shaped eviction: age- and volume-weighted; first-seen and regression trigger
  events never evicted; eviction deletes bucket objects and index rows together. Mutes
  evaluated on ingest: mute-for (duration) and mute-until (N events per period); unmute
  notifies through the existing `Notifier::unmuted` hook. Reindex takes
  `Notifier::off()` so history never re-rings.
- **Phasing.** 1: core loop (landed). 2: logs + hardening (landed). 3: public ingester
  per `ingester.md`, pulled forward — ingestion scales from day one; the celld edge
  buffers per project, the viewer's digest stays the single writer behind it. 4:
  Pushover, context capture, dogfood. 5: crons, quotas, retention polish, nac-bugs
  cutover. Acceptance = the 20 facts in the goal doc.

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
