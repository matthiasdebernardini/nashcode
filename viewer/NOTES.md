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
