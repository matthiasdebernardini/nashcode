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
  `tasks`, `board`, `ci`, `comments`, `raw`, `traces`, `commits`, `assets`, `tree`,
  `blob`. `tasks` and `commits` are reserved beyond the spec's list: a card page renders
  at `/{repo}/tasks/{*path}` and the commit-to-trace link at
  `/{repo}/commits/{sha}/trace`. Reserving costs nothing extra: a `#[page]` with a
  literal second segment out-ranks the catch-all in the router, so `tree` and `blob`
  needed no branch-name filter of their own.
- `GET /{repo}/traces[...]` is both a page and an API: `Accept: application/json`
  selects JSON, anything else gets HTML. One URL per resource instead of a parallel
  `/api` tree.
- `/{repo}/raw/{*rest}`: the branch part of the rest is matched greedily against real
  branch names (longest prefix wins) so branches with `/` still get raw URLs; if nothing
  matches, the first segment is taken as the revision.

## Code tab (`/{repo}`, `/{repo}/tree`, `/{repo}/blob`)

- **Trees are addressed as `<rev>:<dir>`, not through a pathspec.** Directory names
  arrive from the URL, and a pathspec would read `*` or a leading `:` in one of them as
  a pattern. Object addressing has no such syntax, so `../..` and friends simply fail to
  resolve — the 404 is git's answer, not a filter of ours.
- **The blob page reads the parent directory first.** `git show <rev>:<dir>` succeeds on
  a directory and prints its entries, so asking for the bytes first would render a tree
  as if it were a file. One `ls-tree` of the parent answers "does it exist" and "is it a
  blob" together, and hands back the size for free.
- **A wrong-kind URL is a 404, not a redirect.** `/blob/` on a directory and `/tree/` on
  a file both 404. A redirect would have to put a repo path in a `Location` header,
  which means percent-encoding decisions for paths containing spaces; every link the
  viewer generates already points at the right one of the two.
- **Only the default branch.** The spec describes the Code tab as the repo home; a
  `?branch=` selector is not in it. Other branches are read through their PR view.
- **README is `README.md`, any case, at that tree level only.** No `README`, no
  `readme.rst`, no walking up. The renderer is `render::markdown`, the same one the
  plans pages use, so escaping, XSS handling, and plan/branch autolinking are identical.
- **A mirror with no branches renders "Nothing pushed here yet."** Before this, an empty
  mirror took `/{repo}` to a 500 through `default_branch()`. Degrading was the rule
  everywhere else already.

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

## Mirror refresh: stale while revalidate

The fetch used to run on the request path. Against the real remote it costs 4 to 6.5
seconds, so the first navigation after ten idle seconds paid all of it: measured 6.5s to
first byte cold, 0.28s warm. `Mirrors::refresh` now answers from the mirror on disk and
puts the fetch on a background task. Choices worth knowing about:

- **`stale` still means "the last attempt failed", not "a fetch is running".** A fetch in
  flight has learned nothing yet, so flipping the page to the stale banner would be a lie
  that clears itself a second later. A failed background fetch shows up on the next
  request, which for a person navigating is the next click.
- **The in-flight guard is the repo's own lock, not a second flag.** `spawn_fetch` takes
  the lock with `try_lock_owned` and moves the guard into the task, so the guard is held
  for exactly the life of the fetch and a second caller finds the repo busy and leaves.
  Two flags (a lock plus an "is fetching" bool) could disagree; one cannot.
- **Three entry points, one fetch.** `refresh` is the page path (never waits, except on
  a repo with no mirror, which has nothing to render). `refresh_all` warms every mirror at
  startup and waits. `refresh_now` is the write path: it takes the lock, clears the
  debounce *under* it, and fetches inline. Clearing the debounce before taking the lock
  would let a fetch that started before the caller's push satisfy it, and the caller would
  not see its own write.
- **The state mutex is never held across the git call.** `fetch` reads no state and takes
  the state lock only to record the result.
- **A missing mirror still retries on every request,** with no debounce, exactly as
  before. It blocks, but it is the only way that request can have content, and one clone
  ends it.

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

## Code intelligence choices (2026-08-19, from the research pass)

- **Graph: codanna + SCIP, hybrid.** codanna (Apache-2.0, `lib` crate, tree-sitter,
  covers our three languages) builds the graph on every push — fast and incremental,
  but name resolution is heuristic. SCIP is regenerated on merge as the accurate
  overlay (`rust-analyzer scip`, `scip-typescript`, `scip-python`) and read in-process
  with the first-party `scip` crate. Rejected: stack-graphs (archived 2025-09), Kythe
  (frozen since the 2024 layoffs), Glean (consumes SCIP anyway; heavy ops), GitLab gkg
  (maintenance mode, successor is EE-licensed).
- **Known costs:** rust-analyzer's SCIP pass is single-threaded
  (rust-analyzer#18140) — shard by crate and cache by blob SHA. `scip-python` is the
  worst-maintained of the three indexers; where it fails, that repo keeps codanna
  edges only. SCIP occurrences don't name the enclosing function; the range→symbol
  interval map is ours to build (~200 lines) and is what impact queries hang off.
- **Embeddings: `JinaEmbeddingsV2BaseCode`** — the one code-trained model fastembed
  ships (768-dim, Apache-2.0), and it beats the general-text models on code retrieval
  by a wide margin. Upgrade order when quality pinches: add fastembed's `TextRerank`
  over the top ~50 hits first; only then swap the encoder via
  `UserDefinedEmbeddingModel` (nomic-embed-code or CodeSage, local ONNX).
