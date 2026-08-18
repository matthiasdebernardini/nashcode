# Implementation notes

Working notes for whoever picks this up next. SPEC.md is the contract; this file records
where the implementation had to choose.

## Topcoat deviations

- **Assets bypass `asset!`/`topcoat asset bundle`.** The spec requires that plain
  `cargo build` on a fresh clone produce a runnable server. Topcoat's asset bundler is a
  separate CLI step (`topcoat asset bundle`) that must run against the exact binary, so
  instead `build.rs` runs esbuild and the two bundles are embedded with `include_bytes!`
  and served from `/assets/...` routes with a content-hash query for cache busting.
- **A `shell` component instead of `#[layout]`.** Pages pass the title, the active tab,
  and the mirror status into the chrome. A path-prefix layout cannot take arguments from
  the page it wraps, so the chrome is an ordinary `#[component]` every page calls.
  Routing itself is idiomatic: `#[page]`/`#[route]` + `Router::builder().discover()`.
- **`topcoat::serve` with our own listener,** because the bind address comes from
  `NASHCODE_BIND`, not Topcoat's `HOST`/`PORT`.

## Routing decisions

- Branch names may contain `/`, so `/{repo}/{*rest}` is a catch-all and action suffixes
  are parsed off the tail: `.../ci`, `.../ci/rerun`, `.../merge`, `.../restack`,
  `.../delete`. A branch name therefore may not *end* in one of those suffixes.
- Reserved first segments under a repo (never valid branch names): `stacks`, `plans`,
  `tasks`, `board`, `ci`, `comments`, `raw`, `traces`, `commits`, `assets`. `tasks` and
  `commits` are reserved beyond the spec's list: a card page renders at
  `/{repo}/tasks/{*path}` and the commit-to-trace link at `/{repo}/commits/{sha}/trace`.
- `GET /{repo}/traces[...]` is both a page and an API: `Accept: application/json`
  selects JSON, anything else gets HTML. One URL per resource instead of a parallel
  `/api` tree.
- `/{repo}/raw/{*rest}`: the branch part of the rest is matched greedily against real
  branch names (longest prefix wins) so branches with `/` still get raw URLs; if nothing
  matches, the first segment is taken as the revision.

## Spec interpretation

- **Merge fast-forward:** when the parent's tip is an ancestor of the branch tip the
  merge fast-forwards; otherwise `git merge --no-ff` creates a merge commit.
- **Outdated comments:** a line-anchored comment is *outdated* when the branch tip moved
  past the comment's commit **and** the anchored file changed between the two commits
  (`git diff --quiet`). Comments on untouched files stay inline.
- **/brain/ask upstream errors:** a 429 from the Claude API passes through as 429 (it is
  actionable rate-limit information); every other upstream failure is a 502 carrying the
  API's own error message. The spec says "502, never 500"; the acceptance list says "429
  passthrough" — this satisfies both.
- **Restack pushes are atomic:** all rebases happen in a scratch clone first, then one
  `git push --force --atomic` pushes every new tip; a conflict aborts before any push.

## The Agent tab

Traces and Prompts merged into one tab. The spec left these open; here is what was
chosen and why.

- **`/:repo/agent` returns the prompt search as JSON, not the session list.** The spec
  puts the search on this page and says "the same URL returns JSON". The session list is
  still `GET /:repo/traces` with `Accept: application/json`, unchanged, so nothing an
  agent already polls had to move. `/agent?q=` and `/prompts?q=` return byte-identical
  bodies from one shared query.
- **Searching filters the session list too.** With `?q=` or `?session=` set, the page
  shows the sessions that hold a match and lists the matching prompts under them.
  Without either, it is the session list alone.
- **A session's title is its first prompt.** That comes from the same `prompts` query
  the search uses, so a session whose payloads carry no `prompt` field falls back to its
  id. Backfills through `nashcode-viewer trace push` lift the prompt out of
  `message.content`, so they get titles; a transcript posted straight to the API does
  not.
- **User text starting with `<` is harness markup, not a prompt.** Command output and
  system reminders arrive as ordinary user lines. They render as a one-line note instead
  of as something a person wrote, which is the same judgment `trace push` already makes
  when it decides what counts as a prompt.
- **Synthesized diffs carry positions, not locations.** A file edit renders from
  `toolUseResult.structuredPatch` when the harness computed one — that is the real diff
  against the file on disk. Without it, the diff is rebuilt from the call's own
  `old_string`/`new_string` (or `content` for `Write`), and the hunk headers count from
  line 1 because the arguments do not say where in the file the match landed. The change
  is exact; the line numbers are not.
- **Tool results are clipped at 4000 characters** on the page. A `Read` of a large file
  is a whole file in one `<details>`, and the raw transcript is one link away.
- **`escape_json_for_script` is duplicated** from `web/pages.rs`. It is one `replace`
  call, and the alternative was reaching into a module another work stream owns.
- **The branch page's "trace" link still points at `/:repo/traces/:session`**, so it
  costs one redirect. `web/pages.rs` belongs to another work stream right now; point it
  at `/:repo/agent/:session` when that lands.

## Known caveats

- **The JS bundle is ~10 MB unminified-by-content.** `@pierre/diffs` pulls in Shiki, and
  Shiki's default entry point carries every bundled grammar and theme. It gzips to a
  fraction of that and is cached after the first load, but it is the one number in this
  project that would embarrass you on a cold load over a slow link. The fix, if it ever
  matters: register only the languages the repos actually contain via
  `registerCustomLanguage` / `preloadHighlighter` and import Shiki's core entry instead of
  the full bundle. Not done here because it trades a real correctness property (any file
  in any language highlights) for a load-time number nobody on a tailnet will notice.
- **CI runs jobs serially** in one global worker, per the spec. The `// ponytail` marker
  at the queue notes where to parallelize per repo if it ever backs up.
- **`git` is assumed on `PATH`** at runtime, not just at build time. There is no
  vendored git and there never should be.

## Workspace overlap (post-publish work)

The viewer's agent-side subcommands (`nashcode-viewer hook`, `trace push/list/show`,
`doctor`) overlap conceptually with the `cli/` crate: both are client tools an agent
machine installs, both infer the repo from the working directory, both talk HTTP to a
server. They are deliberately NOT unified yet — the CLI talks to dgit, the viewer client
talks to the viewer, and merging the two surfaces is post-publish work, not a publish
blocker.
