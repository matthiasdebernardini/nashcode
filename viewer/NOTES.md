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

## Code browser parity and the wiki (2026-08-19)

Two SPEC sections landed together: "Code browser parity" and "Docs (wiki)". Where the
implementation had to choose:

- **The gutter is server-rendered; shiki only swaps line contents.** The blob page ships
  one block per line — an `<a class="nashcode-lineno">` and a `<span
  class="nashcode-line-code">` inside a `<span class="nashcode-line" id="L{n}">`. `app.js`
  writes into the code span and never touches the gutter, so numbering and `#L10` work
  with JS off, with an unknown extension, and after a failed highlight. The three failure
  paths all land on the same working page.
- **The line number is a `::before`, not text.** `content: attr(data-line)` keeps the
  numbers out of a copied selection while still rendering them without JS. It also gives
  every blank line a non-empty line box, so an empty line keeps full height without a
  `min-height` guess.
- **A dual-theme highlight with `defaultColor: false`.** shiki writes `--shiki-light` and
  `--shiki-dark` on the `<pre>` and on every token; `app.css` picks one by Primer's color
  mode (`[data-color-mode="dark"]`, and `auto` under `prefers-color-scheme`). No second
  render, no flash, and the gutter keeps Primer's color through both.
- **`shiki` is now a direct dependency.** `js/app.js` imports it by name, so the phantom
  hoisted copy it was resolving through @pierre/diffs is declared. The range matches
  @pierre/diffs' own (`^3 || ^4`), so npm still resolves one copy and esbuild still emits
  one chunk graph. The import is dynamic, which is what keeps the four hundred grammar
  chunks out of the entry: the entry is 248 KB and one grammar arrives per file read.
- **The 5000-line cutoff drops the language tag, never the numbering.** Above it the page
  is still fully linkable; it is only unpainted.
- **shiki has no `ignore` grammar,** so `.gitignore` and friends stay a plain `<pre>`
  rather than naming a grammar the bundle cannot load.
- **`/{repo}/edit` is one endpoint for both new files and edits.** The path travels in the
  form body, not the URL, so "New file" and the pencil post to the same place; only the GET
  differs (`/edit` empty, `/edit/{*path}` prefilled). That reserves one word instead of two
  and keeps the POST out of the branch catch-all, which a literal route already outranks.
- **A rejected commit re-renders the form, not an error page.** The person's text and
  message come back with the reason above them, because the alternative is losing an edit
  to a push race. Only the *push* decides success: `ops::commit_file` is the board's own
  write path, so the mirror and dgit cannot diverge here either.
- **`safe_repo_path` refuses rather than repairs.** No empty, `.`, `..`, or `.git`
  segments, no backslashes, no control characters. A path that had to be repaired is a
  path the person did not mean.
- **A textarea posts CRLF whatever the file held.** The write path normalizes to `\n` and
  restores the trailing newline, so a round-trip through the browser does not rewrite every
  line ending in the repo.
- **The wiki reads `DocIndex.all_paths`, which the plans index already builds.** No second
  scan and no new cache: the markdown list is that set filtered by extension, so the wiki
  is exactly as fresh as the tip.
- **Relative-link rewriting extends `render::markdown` rather than forking it.**
  `markdown_in_docs` is the same function with the document's directory supplied; every
  other caller keeps the old signature and the old behaviour. Only wiki pages rewrite
  relative links, because only they have a directory to resolve against.
- **The sidebar is a recursive `String`, not a component.** A `#[component]` cannot call
  itself without boxing its own future, and the markup is a nested list. Everything in it
  goes through `render::escape_text`/`escape_attr` — the labels are filenames, and git will
  carry any byte a filename can.
- **`docs` and `edit` join the reserved first segments** (`stacks`, `plans`, `tasks`,
  `board`, `ci`, `comments`, `raw`, `traces`, `commits`, `assets`, `tree`, `blob`). As
  before, reserving costs nothing: a literal second segment out-ranks the branch catch-all
  in the router, so a branch named `docs` keeps every URL except that one.
- **A wiki URL only reaches markdown that exists.** `/docs/src/lib.rs` is a 404, not a
  redirect to `/blob/`; the wiki's own links already point at the right one of the two.

### After peer review

- **`docs/...` is a namespace the wiki shares with branches.** Reserving `docs` was
  meant to cost one branch name; a catch-all under it would have cost every name
  beginning `docs/`. So a wiki URL that names no markdown page in the index but does
  name a real branch falls through to that branch's PR view, and a name that is
  neither is a 404 rather than a guess. A wiki page always wins, because it is the
  thing the URL says it is.
- **A path's text is not enough to make a write safe.** A repo can commit
  `link -> /etc` as an ordinary blob, and the scratch clone checks it out;
  `root.join("link/passwd")` then names a file outside the clone, which
  `create_dir_all` and `write` reach happily and git objects to only afterwards.
  `ops::resolve_inside` walks the path component by component, refuses a component
  that is a symlink, and requires the result to stay under the canonicalized root.
  Both writers use it — the card flip on merge writes a repo-supplied path too.
- **Cross-site POSTs were already handled, by Topcoat.** `OriginPolicy` is on by
  default and `web::router` never disables it: `sec-fetch-site` present and not
  `same-origin`/`none` is a 403 before any handler runs, an absent header is allowed
  (the CLI, curl, and the trace hook carry no ambient credentials), GETs are untouched.
  That is load-bearing for a viewer whose actor comes from a header, so `tests/csrf.rs`
  pins it instead of trusting it.
- **The edit form carries the blob it was opened against.** A submit whose base no
  longer matches is refused with the person's text intact, because overwriting a push
  that landed in between is the one outcome nobody can undo from this page. A client
  that never loaded a form sends no base and gets no check — it never had a version to
  be stale against.
- **The form also reports whether the file ended with a newline.** Restoring one
  unconditionally rewrote the last line of every file that did without, which is a diff
  nobody asked for.
- **Two line caps, not one.** 5000 lines drops the language tag (shiki would out-think
  the reader); 50 000 drops the gutter as well, because ~145 bytes a line makes a
  500k-line generated file a 70 MB page. Past that cap the raw link is the honest
  answer, and it is already on the header.
- **URL paths are percent-encoded through `render::encode_path`.** Ordinary paths come
  out byte-identical, so no existing URL moved; `a#b.md` and `notes v2.md` stop
  truncating. Topcoat decodes catch-all segments one at a time, so the round trip is
  exact. In markdown source a `#` is still a fragment — that is the author's to escape.

## Click a line to comment (2026-08-19)

- **`@pierre/diffs` gives us the click, so nothing is scraped.** `FileDiff` takes
  `onLineClick` and `onLineNumberClick`; both hand back the row's own `lineNumber`,
  `annotationSide`, and `lineType`. The number is the renderer's, not something read out
  of the DOM, so the anchor cannot drift from what the reader clicked. The two
  callbacks are mutually exclusive inside the interaction manager (number column wins),
  so both point at one handler and a click anywhere on the row opens the composer.
- **The composer goes back in through the annotation slots.** The same mechanism that
  renders stored comments renders the composer: one extra `{side, lineNumber, metadata}`
  appended to the server's annotations, then `instance.render({ lineAnnotations })`.
  Nothing is positioned by hand and the composer lands under the clicked row wherever
  the renderer puts that row — after any comments already anchored there.
- **One metadata object, compared by identity.** `areDiffLineAnnotationsEqual` compares
  `metadata` with `===`, so a single stable object keeps the renderer's annotation cache
  from rebuilding the composer on every render — the element survives, and with it the
  half-typed comment.
- **A deletion-side click has no new-side line to anchor to, so it does not guess.**
  Comments anchor to the new side. A click in the deletion column focuses the
  file-level composer under the diff instead, which is the honest answer: a whole-file
  remark rather than a line the reader never picked.
- **The composer is a clone of a server-rendered `<template>`.** Same action, same
  hidden `branch`/`file`, plus the hidden `line` the click fills in. The server sees an
  ordinary form post on `POST /:repo/comments` and needs no endpoint, and the markup is
  testable from Rust without a browser. A submit is a plain navigation: the redirect
  brings the page back with the comment already in the annotation slot.
- **The typed "line #" input is gone from both composers.** Typing a number to anchor a
  comment was a guess with a keyboard; clicking the line is the same intent with none of
  the arithmetic. Plan pages keep the file-level composer for whole-file remarks, and
  the JSON API still takes `line` for tools that compute one.
