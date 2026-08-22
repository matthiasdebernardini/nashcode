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

## Code intelligence as built (2026-08-19)

The endpoint contract in SPEC.md is exactly what shipped. Two of the tools behind it
are not, and both changes were forced by measurement.

### codanna is out: it cannot coexist with fastembed

codanna 0.13 pins `fastembed = "=5.6.0"`, which pins `ort = "=2.0.0-rc.10"`. fastembed
6 pins `ort = "=2.0.0-rc.13"`. `ort` links a native library, so cargo cannot carry two
of it, and the resolver refuses outright:

    error: failed to select a version for `ort`.
        ... required by package `fastembed v5.6.0`
        ... which satisfies dependency `fastembed = "=5.6.0"` of package `codanna v0.13.0`
      previously selected package `ort v2.0.0-rc.13`

Downgrading to fastembed 5.6.0 to match does not rescue it: that version has no
`ort-download-binaries-rustls-tls` feature, so it can only reach ONNX Runtime through
native-tls, which means OpenSSL, which does not cross-compile. And codanna is a whole
application — tantivy, rmcp, axum, notify, sysinfo, clap — behind a `parse` API whose
output would need adapting into our tables anyway.

So the graph is built by our own tree-sitter pass, in `code/lang.rs`. The escape hatch
in the brief covers this, and the cost is small: SPEC already requires tree-sitter for
function-level chunking, so the grammars were going to be dependencies regardless, and
codanna's `find_calls` is itself a tree-sitter query. What is lost is codanna's
per-language import resolution; a call is attributed by its last path segment, so
`self.retry()` and `crate::net::retry()` both answer `retry`. The SCIP overlay is what
fixes that, and it is the layer the accuracy was always supposed to come from.

### ort is loaded, not linked, and that is what makes the release build cross-compile

`cargo zigbuild --release -p nashcode --target x86_64-unknown-linux-gnu` **links, with
embeddings on by default.** It did not at first. fastembed's default
`ort-download-binaries` pulls a prebuilt ONNX Runtime static archive, and zig cannot
link it, because the archive wants GNU libstdc++ and zig ships libc++:

    ld.lld: error: undefined symbol: std::__cxx11::basic_string<...>::find(char, unsigned long) const
    >>> referenced by manifest_parser.cc in archive libort_sys.rlib

Those are GNU-ABI symbols (`std::__cxx11::`); libc++ spells them `std::__1::`. There
is no flag that bridges that. The fix is `ort`'s `load-dynamic` feature: no native
archive enters the link at all, and the runtime is opened with `dlopen` on first use.
The linux binary's only dynamic dependencies are `libc`, `libm`, `libdl`, and
`libpthread` — no `libonnxruntime`, no `libstdc++`.

This is better than the feature-gate the brief offered as a fallback, because there is
one binary and one code path. The `embeddings` cargo feature still exists and is still
on by default, but it is now a build-size knob rather than a portability one:
`--no-default-features` compiles fastembed out and `/code/similar` answers
`"this build has no embedding support"`.

What the box needs, then:

- **`libonnxruntime.so` somewhere the loader looks**, or `ORT_DYLIB_PATH` pointing at
  it. Without it, `/:repo/code/similar` reports itself unavailable and everything else
  is untouched. `nashcode-viewer serve` prints this as a doctor line at startup.
- **Network on the first index run**, which downloads the model (a few hundred
  megabytes) into the hf-hub cache. Later runs read the cache.
- **Nothing else.** SCIP indexers on `PATH` are optional; see below.

`ort` *panics* when the dylib is missing rather than returning an error, which is
exactly the case a fresh box hits. `Embeddings::load` catches that panic and turns it
into a message, and remembers the failure for ten minutes so a missing dependency
costs one warning line rather than one per merge.

### The trigger is the mirror's tip observer, not a call inside `merge`

SPEC says "every merge to the default branch". The implementation hangs off
`Mirrors::with_observer` — the callback that already queues CI for every newly seen
tip. Two reasons. `Ops::merge` ends in `refresh_now`, which observes the new tip
anyway, so a call inside `merge` would be the second of two triggers for one event.
And a merge pushed straight to dgit, bypassing the viewer, gets indexed by this and
would not by the other. One rule covers both.

**An index job carries no branch and no commit.** It says only "this repo moved"; the
run resolves the default branch tip when it starts. That is what makes coalescing
safe, and coalescing is what the observer needs — a push of five branches fires it
five times, and a merge fires it again. `CiQueue` keeps one pending job per repo and
releases the slot as the run *starts*, not as it finishes, so a push landing mid-run
can still queue the run that will see it. A job dropped as a duplicate cannot have
seen anything the survivor will miss, because the survivor reads the tip later.

The first shape of this filtered by branch in the worker and kept the branch on the
job. That combination is unsafe once you coalesce: a queued job for `feature` (which
the worker would skip) would swallow a job for `main`, and nothing would index.
Removing the filter costs one tree read on a feature-branch push and removes the
failure mode.

`POST /:repo/code/index` is the manual path and queues the same job. `nashcode index
[repo]` is a thin client over that endpoint; `--status` reads `GET /:repo/code`
without queueing anything. Indexing never runs on a request path.

### A run over an unmoved tree stops before it starts

An index run whose commit is already the last recorded run's, and where every blob is
known, returns immediately. The parse it skips is cheap; the SCIP overlay it skips is
not — that clones the repo into a scratch checkout and runs a language indexer over
it. Without the early return, a merge that only moved a card paid for a full
rust-analyzer pass.

`code_seen_blobs` is what makes "every blob is known" true for blobs that stored
nothing. An empty file, a PNG, a two-line stub, and a minified bundle all produce zero
chunks, so asking `code_chunks` whether they were indexed answers "no" forever and
re-reads them on every merge. On a repo that is mostly assets that was most of the run.

### Everything is keyed by blob SHA, and the path table is separate

`code_chunks`, `code_symbols`, and `code_refs` are keyed by `(repo, blob, ordinal)`.
`code_files` maps `(repo, path)` to a blob. Queries join the two. The consequences are
the point:

- An index run parses and embeds only blobs the repo has never held. A second run over
  an unchanged tree does no work at all.
- A rename is free — the content did not change, so the chunks, the vectors, and the
  symbols stay and only the path moves.
- A delete is a path-table rewrite, and the sweep that follows it drops content no path
  references any more, so a deleted file stops answering immediately.
- Two files with identical content share one set of vectors.

Line numbers live with the content, not the path, which is what makes that safe: a
chunk's `start_line` is a fact about the blob and is true wherever the blob sits.

### Chunking, in one paragraph

Rust, Python, and TypeScript get a real parse. One chunk per top-level function, class,
struct, trait, or interface; nested definitions are symbols but not their own chunks,
because the enclosing one already carries their text. Runs of ten or more lines that no
definition claimed — module preamble, top-level statements — get fifty-line window
chunks, so a question about a file's imports still has something to hit. Everything
else, and anything that fails to parse, is fifty-line windows over the whole file.
Files over a megabyte are skipped on the size `ls-tree` already reported, without ever
being read; files with a NUL in the first eight kilobytes are binary and skipped too.
The TSX grammar covers `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, and `.cjs`: it is a
superset of the TypeScript one and parses plain JavaScript.

### SCIP: the overlay, and the interval map SCIP does not give you

`code/scip.rs` runs `rust-analyzer scip`, `scip-typescript`, and `scip-python` as
subprocesses in a scratch checkout, but only where the binary is on `PATH` *and* the
repo has the marker file for it (`Cargo.toml`, `package.json`, `pyproject.toml`). A
missing binary is silent — most repos have none, and the tree-sitter graph is the
designed answer for them. A binary that fails, times out, or emits an unreadable index
is one note on the run record; the tree-sitter graph it would have replaced stays
exactly as it was. The degradation ladder is SCIP, then tree-sitter, then `git grep`,
and no rung is an error.

**An indexer is repo-controlled code and is treated as such.** `rust-analyzer scip`
runs `cargo check`, which executes the indexed repository's `build.rs` and expands its
proc macros. So indexers run under the same `env_clear()` the CI worker uses — `PATH`,
`HOME`, `GIT_TERMINAL_PROMPT=0`, nothing else — and rust-analyzer is additionally
passed `cargo.buildScripts.enable=false` and `procMacro.enable=false`. Before that,
indexing a repository handed that repository `ANTHROPIC_API_KEY` and `GIT_TOKEN`. What
the overlay needs is name resolution, not a build.

The research pass flagged that SCIP occurrences do not name their enclosing function.
The map is built in `read_document`: every occurrence carrying the Definition role
contributes its `enclosing_range` (the body, not the identifier) to an interval list,
and each reference is attributed to the innermost interval containing its line. Two
details worth keeping:

- **A symbol string is not a name.** `rust-analyzer cargo demo 0.1.0 net/retry().` is
  precise and unreadable. The last descriptor's `name` is stored, so `/code/def?symbol=`
  takes the same input whichever layer answered. Locals (`local 12`), parameters, and
  type parameters are dropped: nobody looks those up by name.
- **SCIP does not separate "called" from "mentioned".** Every non-definition occurrence
  is stored as a call edge. Over-reporting a caller beats silently dropping one, and the
  alternative would be inferring call-ness from a range, which is guesswork.

`rust-analyzer`'s SCIP pass is single-threaded (rust-analyzer#18140). Nothing here works
around that; it runs on the CI queue behind a twenty-minute deadline and nothing waits
on it.

### `/brain/ask` gained a tool loop

`brain::ask` sent one request and returned. It now loops up to six times, offering five
tools — `code_text`, `code_similar`, `code_def`, `code_refs`, `code_callers`. The
seventh request goes without tools, so a model that keeps asking has to answer with what
it already found. The opening user message is still a plain string, so the request an
operator sees on the wire has not changed shape; only the tool-result turns use content
blocks. `Answer` gained `tools_used`, which names what the reply was actually grounded
in.

A tool call may only name a repo the question was scoped to, so `?repo=x` is a boundary
rather than a hint. Without `ANTHROPIC_API_KEY` the route still answers 404, unchanged.

### Smaller decisions

- **`/code/similar` never loads the model.** Loading downloads hundreds of megabytes.
  A request that triggered it would hang for minutes, so an unloaded model is a 503
  naming what makes it loaded (an index run) and pointing at `/code/text`. Because
  indexing is in-process, the model is resident from the first index run onward.
- **`code` joins the reserved first segments** under a repo, alongside `stacks`,
  `plans`, `tasks`, and the rest. A branch may not be named `code`.
- **The graph endpoints answer 200 with an empty list and a hint,** never 404 or 500,
  for a symbol that is not there. A language the graph cannot parse is *expected* to
  fall through to text search, and an error would teach a caller to stop asking.
- **Brute-force cosine, as specified,** but in two passes. The scan reads ids, paths,
  and vectors — never snippets, which run to eight kilobytes each and would move
  megabytes of text through the connection mutex to rank ten rows; the snippets are
  fetched afterwards for the rows that placed. The whole thing runs on a blocking
  thread, because it holds that mutex and does real arithmetic. No ANN index. A tie
  breaks on path then id, so the order is stable across runs rather than whatever the
  scan visited first, and a vector of a different length scores zero rather than
  panicking — which is what makes changing `NASHCODE_EMBED_MODEL` safe: old vectors
  simply stop matching.
- **`repo` from a JSON body is validated like a path parameter.** `/brain/ask` takes
  its repo from the body, and that value reaches `Config::mirror_path` and then
  `git --git-dir`, which reads whatever it is handed. It is checked against
  `knows_repo` before the model sees the question, and `mirror_path` itself now refuses
  a name carrying a separator, traversal, NUL, or leading dash — so the same mistake
  made in a future handler yields a path inside the mirror directory that cannot exist
  rather than an escape.
- **Every unbounded read has a ceiling now.** `git grep`'s `--max-count` is per file,
  so a common word across thousands of files was unbounded: the captured output is
  capped at four megabytes (the child is killed at it), the parse stops at a thousand
  matches, and a matched line is clipped at 500 bytes so one minified bundle line is
  not the response. `/code/graph` is capped at 100k edges. All of them report
  `truncated: true` rather than looking complete.
- **One deadline for `/brain/ask`, not one per hop.** SPEC gives it five minutes. That
  used to be the same number as the per-request timeout; with a tool loop, seven
  requests could sit behind it, so the deadline moved around the loop and answers 504.
- **`git cat-file blob`, not `git show`.** The index has the object id and does not
  care which path it came from. `Repo::read_blob` was added for it. (`git show ":<sha>"`
  is not valid syntax; that bug silently indexed nothing until an integration test
  caught it.)
- **`git grep --null`** so a path containing a colon cannot be misread as a field
  boundary. Exit status 1 means "no matches", which is an answer.
- **One transaction per blob.** An index run that dies halfway leaves every blob it
  finished intact, and the next run resumes exactly where it stopped.

### `GET /:repo/code/graph`, the bulk dump

From the Architecture section of SPEC, which is otherwise a later stream's. It selects
the three tables whole and answers
`{repo, generated_at, commit, files, symbols, edges}`. `commit` is the commit the last
index run read, so a caller can tell whether the dump describes the tree it is looking
at; it is `null` before the first run.

Edges are synthesised rather than stored, because storing them would be a second copy
of the two tables that can disagree with the first. One `defines` edge per symbol
(file to symbol), and one `calls` or `references` edge per reference. A reference edge
starts at the function that encloses it, falling back to the file when the call sits at
module level — a diagram needs an arrow from *something*, and the file is the honest
answer when there is no function.

The degradation the spec asks for falls out of the join: a repo with no index answers
with empty lists, and a repo of files no grammar reads answers with the inventory and
`symbols: []`. Neither is an error.

## Architecture tab

- **`architecture` joins the reserved first segments** under a repo, alongside `stacks`,
  `plans`, `tasks`, `code`, and the rest. A branch may not be named `architecture`.
- **The diagram never becomes markup on the server.** It goes out escaped inside the
  fallback `<pre>` and the client reads it back with `textContent` before handing it to
  mermaid. Nothing has to un-escape anything by hand, which is the property the markdown
  stored-XSS fix bought and this page must not spend. `securityLevel: "strict"` is the
  second layer, not the first.
- **Mermaid loads behind a dynamic `import()`,** the same trick that keeps shiki's
  grammars off every page. It is bigger than the rest of the bundle put together; adding
  it grew the entry chunk by 701 bytes and nothing else.
- **`GET /{repo}/architecture` with `?id=` that does not exist is a 404**, but the bare
  URL with nothing submitted is a 200 page — an empty state is the answer to "what is the
  shape of this system", not an error.
- **The empty state reads `ARCHITECTURE.md` at the default tip**, extracting only its
  ` ```mermaid ` fences. The prose around them belongs to the wiki tab, which already
  renders that file whole.

## `nashcode annotate` posts what plannotator decided (2026-08-19)

Lives in `cli/`, noted here because this is the file that records choices SPEC left open.

- **The decision arrives in a result file, not on stdout.** plannotator's
  `--gate --json --result-file <path>` writes one JSON record and refuses to overwrite an
  existing file, so `annotate` makes a fresh directory per run under the system temp dir
  (`nashcode-annotate-<pid>-<n>`, with `create_dir` as the collision check) and lets
  plannotator create the file inside it. `tempfile` stayed a dev-dependency: ten lines of
  stdlib do this, and the shipped binary keeps its dependency list.
- **`annotated` with no feedback is an error, not an empty comment.** The four records are
  specified; a malformed one is not. An empty comment would tell the polling agent the
  review is over and give it nothing to act on, so the command bails. Blank `feedback` on
  an `approved` record is different — it degrades to the bare `Approved.`, because there
  the decision is the whole message.
- **The branch comes from jj in a jj repo, and it is not optional.** The first version of
  this asked git's `symbolic-ref` everywhere, on the belief that a colocated jj repo keeps
  git's HEAD on the checked-out bookmark. That is false, and peer review caught it against
  jj 0.44.0: in the ordinary state after `jj commit` or `jj new`, git's HEAD is detached
  and `symbolic-ref` exits 1; right after `jj edit <bookmark>` it prints `jj/root`. The
  first failure sends no branch and the viewer answers 400. The second sends a branch name
  no server has heard of, and when the mirror happens to be unavailable the viewer takes
  the comment with an empty anchor commit — a comment nobody will ever query, which is the
  one way this design could lose feedback silently. jj is now asked in jj's terms:
  `jj log -r 'heads(::@ & bookmarks())' -T bookmarks`, the nearest bookmark at or behind
  the working copy, which is right after `jj commit`, `jj new`, and `jj edit` alike.
  Several bookmarks on that commit come back alphabetically and the first wins. `jj/`
  names and jj's trailing `*` out-of-sync marker are filtered out of both paths.
- **No branch is a reason, not an omitted field.** `comment_payload` takes `&str`, not
  `Option<&str>`. The viewer requires a branch, so a payload without one cannot succeed,
  and there is no value in constructing it. A workspace that cannot name a branch takes
  the print-the-feedback path with "cannot tell which branch this plan is on".
- **A plan outside the repository is the same kind of refusal.** `relative_path` returns
  `Option`. An absolute path posted as a file name is accepted by the viewer and then
  rendered by no page and matched by no `?file=` query, so it is worse than not posting.
- **"Nowhere to post" exits 0; a refused post does not.** No viewer URL, no resolvable
  repository, no branch, a file outside the root: configuration, not failure. Print the
  feedback, say why it went nowhere, exit 0. A POST that was attempted and rejected is a
  real failure, so the feedback prints and then the command bails.
- **The decision is read before the exit code is judged.** plannotator publishes the
  result file atomically, so a file that exists is a whole decision. Bailing on a nonzero
  exit without looking would throw away a review the human had already finished. A nonzero
  exit with a published decision warns and posts; a nonzero exit with nothing published
  fails.
- **plannotator's stdout is nulled.** `--json` makes it print the decision record on
  stdout as well as publishing it, and that copy would land between the feedback and the
  posted id in the user's terminal. Its human-facing lines go to stderr, so nothing is
  hidden by this. The record comes from the file.
- **The scratch directory is 0700 from birth,** created with `DirBuilder::mode` rather
  than chmodded afterwards. The feedback sits in a shared `/tmp` for as long as the human
  is writing it.

## Architecture: node links (`/code/where`)

- Node links point at the line numbers of the commit the code index was last built
  from, rendered against the tip blob. Where the two have drifted, the link lands
  near the symbol rather than on it; re-indexing is what closes the gap.

## Bugs, slice 1: the DSN consumer (2026-08-19)

`goals/error-tracking/goal.md` is the contract and `SPEC.md` binds the surface. This
is where the code had to choose, and the two places it disagreed with the plan.

### `sentry-types` cannot split an envelope, so the splitter is hand-written

The goal doc says to try `sentry-types` 0.49's `Envelope::from_slice` on real
fixtures first. It fails the test. Its `EnvelopeItemType` is a closed serde enum with
no unknown variant, so one `{"type":"profile_chunk"}` item makes the *whole* envelope
fail to parse — which is the exact behaviour the protocol names as the number-one
thing a server must not do (acceptance fact 3). It also deserializes `event` items
into typed structs, which throws away the raw bytes the bucket is supposed to hold
and makes parsing stricter than "only `event_id`, `timestamp` and `platform` are
required".

So `bugs/envelope.rs` is the ~120-line splitter the doc allowed for, working in raw
bytes throughout, and `sentry-types` keeps the two jobs it is good at: `Auth` for the
`X-Sentry-Auth` header and the `?sentry_key=` query string, and `Dsn` for the
envelope's own `dsn` header.

### `file:///path` is a bucket

`NASHCODE_BUGS_BUCKET` takes `s3://name` as specified, and also `file:///path`, which
`object_store`'s `LocalFileSystem` serves. That is not a production fallback: it is
what lets the whole ingest path — put, get, key layout, the digest reading back — run
against a real object store in a tempdir with no S3, no MinIO container, and no mock.
The tests use it. Nothing else about the code path differs.

### The ingest route is a catch-all under `/api/{id}/`

The protocol's URL is `POST /api/<project_id>/envelope/`, with the trailing slash,
and every current SDK sends it. Topcoat refuses to register a route path ending in a
slash ("invalid path: empty segment"), and matchit's catch-all needs a non-empty
remainder, so `/api/1/envelope/` matches neither `/api/{id}/envelope` nor
`/api/{id}/envelope/{*rest}` — it falls through to the viewer's own
`POST /{repo}/{*rest}` and 404s. The route is therefore `/api/{project_id}/{*rest}`,
which catches both spellings, with the handler checking the tail is exactly
`envelope`. Everything else under `/api/` gets a 404, which is what the goal doc
wants said about `/store/`, `/minidump/` and the Sentry Web API anyway.

The cost: a repo named literally `api` would have its `POST /api/<branch>/merge`
swallowed by this route. `NASHCODE_REPOS` would have to name one for that to bite.

### The ingest route is exempt from origin verification

Topcoat rejects a state-changing cross-origin browser request by default, which is
right for every other route here and wrong for this one: a browser SDK's whole job is
to POST cross-origin from any page in the world. It carries no ambient credential —
the `sentry_key` in the request is the entire auth — so there is nothing for origin
verification to protect. `web::router` exempts the two ingest paths and nothing else.

### The bugs tables are owned by `bugs/`, not by `db.rs`

`bugs/index.rs` carries its own `SCHEMA` and applies it with `execute_batch` on
`Bugs::new`, the same migration pattern `db.rs` uses, through the same connection.
The feature ships its schema with itself, and `db.rs` — which two work streams touch
— did not have to move.

### The digest task starts on the first envelope

`CiQueue` hands its receiver to `main.rs`, which spawns the worker. Bugs cannot do
that: `Bugs::new` runs outside the async runtime in both `main` and the tests, and a
test that never spawned the worker would see ingest succeed and no issue appear. So
the receiver sits in the `Bugs` handle and the worker spawns on the first `enqueue`,
which is always inside an async handler. Single-writer is unchanged. `Bugs::digested(n)`
is how a caller waits for the queue to drain without sleeping.

### Parameterization: what was implemented and what was left out

The goal doc lists uuid, hex, int, ip, email, url, date and quoted string. All eight
are there, as one ordered regex alternation, plus two judgement calls:

- **Only double-quoted strings.** A single-quote rule would eat from the apostrophe
  in `Can't connect` to the next apostrophe anywhere in the message. `KeyError:
  'user_id'` therefore keeps its quotes; the type and the parameterized rest still
  group it correctly.
- **The `hex` arm has to classify, not just accept or reject.** Its bare form,
  `\b[0-9a-fA-F]{7,}\b`, is tried before `int` and also matches a run of seven or more
  *decimal* digits, so `1755561600` never reaches the `int` arm — the `hex` branch is
  the only place it can be dealt with. The first version there returned an
  unrecognised match verbatim, which meant every epoch second and byte count opened
  its own issue. Peer review caught it. Now: all digits → `<int>`; `0x…` or a mix of
  digits and letters → `<hex>`; letters only → left alone, which is what keeps words
  that happen to be spelled in hex letters (`defaced`, `acceded`, `deedface`) as
  words.

Sentry's float and duration classes are not implemented. `1.5` becomes
`<int>.<int>`, which groups exactly as stably.

### `{{ default }}` is matched by shape, not by spelling

Upstream's fingerprint substitution is `\{\{\s*(\S+)\s*\}\}`, so `{{default}}` and
`{{ default }}` are the same thing and SDK users write both. Comparing against one
exact string made the other a *literal* fingerprint part, which merges every event
carrying that fingerprint into a single issue — a failure that looks like success,
since the issue exists and the events pile up in it. `is_default_sentinel` strips the
braces and trims instead. Peer review caught this one too.

### In-app frames link into the code browser, conservatively

SPEC's "Code origin" amendment says an unresolvable path renders as text and never a
dead link, so `Frame::blob_url` returns `None` unless all three hold: the frame is
`in_app`, the project declared a repo, and the `filename` is plainly relative (no
leading `/`, no `.`, no `..`, no empty segment). A frame out of site-packages or with
an absolute `abs_path` therefore stays plain text. The viewer does not check the file
exists at the tip — that would be a mirror read per frame on every page load, and the
blob route already renders its own "not found" for the case that matters.

The log half of that amendment is slice 2.

### Smaller choices

- **The grouping key is stored twice**: readable, so a person can see why two events
  landed together, and SHA-256 hashed, which is what the unique index is on.
- **An event id is sanitized before it becomes an object key.** It is client-supplied
  and goes straight into a path. Anything but alphanumerics and `-` is dropped, and
  an id with nothing left gets a fresh uuid.
- **The same `event_id` twice is one occurrence, not two.** A client retry after a
  timeout is the common case and double-counting it would inflate every issue.
- **The raw envelope and the event payload are both objects.** The envelope is the
  reindex source; the event object is what the detail page reads with one `get`.
  Slice 2's `reindex` can drop the second if the duplication ever costs anything.
- **A bucket that will not open turns the feature off** rather than failing the
  process. The viewer's other twenty jobs are not worth refusing to start over.
- **`deflate` falls back to raw deflate only on a format error.** Falling back on any
  error turned an over-cap zlib body into a 400 instead of the 413 the protocol
  requires, and expanded the bomb a second time on the way there.
- **gzip is read with `MultiGzDecoder`.** Several concatenated gzip members are one
  legal gzip stream; `GzDecoder` reads the first and stops, which truncates the
  envelope without erroring, and a truncated envelope is a lost event.
- **An envelope with no event in it still gets an id back.** Relay answers `{}` there.
  We mint one instead: it costs nothing, and it means the response body has the same
  shape every time.
- **Resolving clears the regression flag.** The flag means "this came back after we
  said it was fixed", so it belongs to the reopening, not to the issue forever.
  Muting leaves it alone — muting is not a fix.
- **`X-Sentry-Rate-Limits` is asserted category by category** in the tests, including
  the negative half: `error`, `default`, `log_item`, `monitor` and `session` must
  never appear, and the list must never be empty. An empty category list means
  "everything", which would silence the errors too.

## Error tracking, slice 2: the log store

### The Sentry log item was captured, not guessed

`viewer/tests/fixtures/bugs/sentry-logs.envelope` is a real envelope, taken off
`sentry-python` 2.68.0 with `enable_logs=True` pointed at a throwaway local listener.
Two things in it differ from the shape the develop docs describe, and both would have
been wrong if the fixture had been written by hand:

- The payload is `{"version": 2, "items": [...]}`, not a bare `{"items": [...]}`.
- **`severity_number` is not a field on the record.** The SDK puts it in the
  attributes, as `sentry.severity_number`, beside `sentry.severity_text`. The parser
  reads both places; the documented one is still first.

The fixture is genuine, not edited: the capture ran with `server_name`, `release` and
`environment` pinned to fixture values, because the default `server_name` is the
machine's hostname and this repo carries no hostnames.

One thing the goal doc says that the capture disproves: "SDK logger integrations
attach these by default" is not true of `sentry.logger` in python 2.68 — no `code.*`
attribute appears. The store reads them when they are there and shows nothing when
they are not, which is the behaviour either way; the NDJSON door and an OTel-based SDK
are where they actually come from today.

### Both generations of the OTel code attributes

`code.file.path` / `code.line.number` / `code.function.name` and the pre-2024
`code.filepath` / `code.lineno` / `code.function` are both read and normalized to one
column each. This is not politeness: the rename is a year old and SDKs pinned before
it are still in production, so reading only the new names would show the file for some
services and nothing for others — a difference nobody would think to look for.

A line number of `0` is stored as NULL. Some loggers use it for "unknown", and
`#L0` is not a line.

### Retention: 30 days, and only the hot rows

`retention_days` defaults to 30. The number decides what search is fast over, not what
is kept: the NDJSON batch is already in the bucket and the prune never touches it. A
month is long enough to answer "what happened last time this broke" and short enough
that the FTS index stays small on a box with one disk.

The prune runs on a plain 24-hour `tokio::interval`, so it prunes at whatever time the
viewer last restarted. Aligning it to midnight would be one more thing to be wrong
about a timezone with.

### The search box is not an FTS5 program

FTS5's MATCH takes a small query language, and a person typing `can't` or `AND` into a
search box gets a syntax error rather than an answer. Every term is quoted and joined
with `AND` before it reaches SQLite, so nothing typed can fail the query. The cost is
that `OR` and `NEAR` are not reachable from the box; the trade is that the box always
works. `file:` is pulled out first and becomes a `LIKE` on `code_file`, never a search
term.

### The logs page resolves a path before it links it

SPEC's "Code origin" bullet says an unresolvable path renders as plain text and never
a dead link. The issue-detail page took that conservatively — relative path, in-app
frame, declared repo — and did not check the file exists, because that would be a
mirror read per frame.

The logs page does check, because it can afford to: it lists each *distinct parent
directory* once at the default tip, exactly the way the blob page decides whether a
path exists (`ls_tree`, then look for the name, and reject a directory). A hundred rows
out of three files cost three `git ls-tree` calls. So `src/app.py:41` links when the
repo really has that file, and stays grey text when it does not — including for a
`/usr/lib/...` path out of site-packages, a path that has been deleted since, and a
path that names a directory.

### Both doors, one store

The envelope door goes through the digest (it is inside an envelope that is already in
the bucket); the NDJSON door writes inline, because there is nothing to group and
nothing to be single-writer about. Both call `logs::store_batch`, which archives the
batch to the bucket *before* it inserts the rows — the same ordering the events use,
for the same reason.

The NDJSON door has two auth sources, not three: there is no envelope header to carry
a `dsn`, so a key in `X-Sentry-Auth` or `?sentry_key=` is the whole of it. One
unreadable line is counted into `rejected` and the readable lines beside it still
land; a shipper tailing a file will send a truncated line eventually, and losing the
batch over it would be the wrong trade.

### Slice-1 review follow-ups

**Authenticate before decompressing.** The envelope `dsn` header is the first line of
an uncompressed body, so `ingest::Reader` hands that line back before the rest is
pulled: a request with no key costs a few hundred bytes instead of 100 MiB. A
compressed body has no readable first line, so the no-declared-key path gets a small
budget instead — 64 KiB compressed, 4 MiB expanded. Every SDK that compresses also
sends a header or query key, both of which are judged before a byte is read and lift
the caps back to 20 MiB / 100 MiB.

The reader buffers whole transport chunks, so the honest guarantee is "one chunk plus
the header line", not "exactly the header line". Reading a byte at a time to tighten
that would cost every honest request to slow one dishonest one.

**A bounded queue.** 1024 envelopes. The slot is reserved *before* the bucket write,
so a full queue answers 429 with `Retry-After: 5` and stores nothing — a refusal that
wrote the payload anyway would grow the backlog every time the client retried. Every
SDK already backs off on a 429.

**`digested_at` and the sweep.** Every `bugs_envelopes` row is stamped when the digest
finishes with it, whatever the outcome: a body that fails to split will fail the same
way forever, and a sweep that re-queued it would never reach the rest. `Bugs::sweep`
re-reads the unstamped rows from the bucket at startup, capped at 10 000. `sweep(true)`
takes every envelope instead — that is the primitive `nashcode bugs reindex` needs.

**`nashcode bugs reindex` is not built.** The viewer half is (`sweep(true)`), but the
command lives in `cli/`, which two other sessions hold. Left for a later slice.

**One `exception` helper.** `group::last_exception` accepts `{"values": [...]}` and
the bare array, and the detail page now calls it instead of reading `values` itself.
An event in the bare form used to group correctly and then render with no exception.

**One bad item no longer drops the rest.** A failed bucket write is logged, counted
into `Outcome.failed`, and the loop goes on.

### A test seam that was not added

`Bugs::store` — the durable half of `accept`, with nothing queued — is public because
the sweep test needs to reproduce a crash between the two writes, and because a
backlog importer would want exactly it. Filling the digest queue *is* test-only, so it
is a unit test inside `bugs/mod.rs` (holding reserved permits, which needs private
access) rather than a public `fill_queue_for_test`. The 429 the route builds from that
decision is asserted separately, in `web/bugs.rs`'s own test.

### Slice 2, peer-review fixes

**A line-length cap bounds nothing.** The NDJSON door held each line to 1 MiB and had
no opinion about how many lines there were. 100 MiB of `0\n` is fifty million of them,
every one under the cap: a `Vec<usize>` of rejected line numbers big enough to end the
process, and a response echoing all fifty million back. `MAX_BATCH` is 10 000 lines,
past which the batch is refused whole with 413 and nothing is parsed, stored, or
echoed. `rejected` became `{count, lines}` — the count exact, the list capped at 20 —
because the size of an answer must not be something the caller chooses. The envelope
door is capped the same way, but truncates rather than refuses: an envelope must never
fail over one item.

**Slots are a poor proxy for memory.** The queue bounded envelope *count* at 1024, and
each job can carry a 100 MiB decompressed body, so the real bound was about 100 GiB. A
`Semaphore` with one permit per byte now sits beside the slot; the permit rides in the
job and is released when the job drops, which is after the digest is done with it.
`QUEUE_BYTES` is 256 MiB and has to stay above `MAX_DECOMPRESSED`, or a single legal
envelope could never be admitted at all — there is a test that says so.

**Log rows were not idempotent, which made the sweep a duplicator.** Events have always
deduped on `(project_id, event_id)`; log rows inserted unconditionally, so `sweep(true)`
— the reindex primitive — doubled the store on every pass. Rows now carry a
`dedupe_key` under a unique index, written `INSERT OR IGNORE`. The key is
`{envelope object key}#{item index}#{record index}` for the envelope door, which is
stable across re-digests because the object key is; the NDJSON door has no stable
identity to reach for — each POST is its own event — so its fresh archive key stands in.

The review also asked for `digested_at` to be stamped inside the digest transaction.
It cannot be: one digest spans a bucket write and N independent item transactions, so
there is no single transaction to be inside of. Dedupe is what actually buys
idempotency, and it buys it whatever order the stamping happens in. Stamping stays
where it was.

**A fatal must not hide under `info`.** `{"level":"info","severity_number":21}` stored
`("info", 21)`, so the row wore the info badge and answered the info filter while
carrying a fatal number. `resolve_severity` now lets the number decide the band. The two
agree in every real payload, so this only fires on a sender that is already confused —
and it fails towards being seen.

**The reader's budget was the cap plus one chunk.** `read_to_end` appended each chunk
and then tested the total, so the unauthenticated path's 64 KiB was really 64 KiB plus
whatever the transport handed over. The test moved ahead of the append.

**Stack frames now resolve like log rows.** SPEC says frames resolve too, and the
slice-1 implementation only checked that a path was syntactically relative — so a file
renamed since the release that threw still rendered as a link, straight to a 404. Both
pages now share `resolve_in_repo`. This changed a slice-1 test that asserted a link to
`probe_capture_exception.py`, a file the fixture repo does not contain: it was asserting
a dead link. It now asserts both directions against the fixture's real contents.

Nits from the same review: `page * LOGS_PER_PAGE` is a `saturating_mul`; the FTS chain
takes at most 32 terms, because SQLite refuses an expression tree past
`SQLITE_MAX_EXPR_DEPTH` and a pasted paragraph would reach it; the NDJSON door reads
`severity_number` through `scalar_number` like the envelope door does. The retention
default lives in two places — a Rust constant and a DDL string — pinned by a test rather
than by pulling in a const-formatting crate for one line.

## The drain (phase 3, the nashcode half)

`viewer/src/bugs/drain.rs` pulls buffered rows off the public ingester and replays them
into the doors they arrived at. The protocol is `ingester/README.md`; SPEC's Bugs
section binds the configuration surface and the ack rule. What follows is what the
implementation had to decide.

**The transport is a trait, and the TCP one is not a test fixture.** `NASHCODE_BUGS_DRAIN`
takes an iroh EndpointId or an `http://host:port` URL, and both are first-class. That is
not generosity: the design doc's hedge is that five hundred lines of axum could replace
celld without the drainer noticing, and a drainer that can only speak iroh cannot be
pointed at the replacement. It is also what makes the contract testable, because the
tests reach a real celld node over loopback.

**Ack after digest, and the one exception.** `Bugs::accept` returns once the bytes are in
the bucket and a queue slot is held, which is the durable point — the digest itself runs
later and can be redone from the object. So the ack follows `accept`, not the digest
task. A 429 off the byte-budget queue ends the project's cycle with no ack; the rows come
back next cycle and dedupe eats the duplicates. The exception is a row nothing can ever
take: a body that will not decompress, an envelope that will not split, a `kind` no door
serves. Those are acked and counted, because the alternative is a project wedged for ever
behind one bad row, and the buffer filling until the edge starts answering 429 to
everything the project sends. `Drainer::poison_count` is the number to watch; it should
be zero, and it is in the log at `warn` with the seq and the reason.

**The cursor is belt and braces.** Acked rows are deleted at the edge, so `after=0` would
also fetch exactly the unacked tail. The cursor exists because the loop needs one inside
a cycle, because a restart should not re-ask for rows it has finished with, and because
`seq` is documented never to rewind — so a cursor is the cheapest way to notice if it
ever does. It only ever moves forwards: the upsert has a `WHERE acked_seq < ?` on it.

**Projects grew an `active` column, and the tailnet door reads it too.** The registry the
edge wants is `(project_id, key, active)`, and nothing here could produce the third
field — there was no way to revoke a project at all. `active` defaults to 1, so every
project that existed before the column keeps working. Revoking one leaves its issues and
logs readable and closes both doors: the edge stops authenticating it on the next
registry push, and `viewer/src/web/bugs.rs` answers 404 immediately. A revoked key is
absent, not wrong — the same thing the edge says.

**An empty registry is never pushed automatically.** The edge refuses one without
`?allow_empty=1`, and the drainer does not reach for that parameter. Emptying the set
takes every project on the fleet offline, and an SDK reads the resulting 404 as a verdict
rather than as weather, so it destroys events instead of delaying them. An empty set here
is far more likely to be a serialisation bug than an intention. It logs a warning naming
the manual `PUT` instead.

**The drain refuses to start without a bucket.** `NASHCODE_BUGS_DRAIN` set and
`NASHCODE_BUGS_BUCKET` unset exits 1 rather than warning. Everything acked is gone from
the edge; a drainer with nowhere durable to put a payload would delete real events off a
box we do not control.

**iroh is written, unverified, and behind a feature.** `viewer/src/bugs/iroh.rs` is the
connector for an EndpointId target: dial the EndpointId on ALPN `celld/http/0`, open one
bidirectional stream, drive HTTP/1 over it with hyper. It has never talked to a real
`iroh-ingress`, because there is not one on this machine, and nothing here fakes one — a
test against a stub would prove only that the stub agreed with itself. So it sits behind
the non-default `drain-iroh` feature, and a default build that is handed an EndpointId
refuses to start and says which flag it wants. The reason the feature exists rather than
the dependency simply being taken: iroh is the whole QUIC stack, this is a workspace
several agents build at once, and an unverified transport is not worth putting that on
everyone's critical path. **Turn it on for the VPS build, watch one drain land, then make
it default and delete this paragraph.**

What the VPS needs is in `ingester/README.md`: nashcode's EndpointId — printed at startup,
derived from the persistent secret key at `NASHCODE_BUGS_DRAIN_KEY` — has to be in
`iroh-ingress --allow` before the first dial. Until that has been done once and watched,
the TCP path is the proven one.

### What the peer review found

Three of the findings were the kind that only a second reader catches.

**A drained log batch had no stable name, so every redelivery duplicated every line.**
A log row's `dedupe_key` is built from the batch's *origin*, and `accept_logs` passed
`None`, which falls back to the archive key — freshly minted on every call. Envelopes
were fine because they dedupe on `event_id`, which travels in the payload; logs have no
such thing. `accept_logs_with_origin` now takes it, the direct NDJSON door still passes
`None` (each POST really is its own event), and the drainer passes
`drain/<project>/<seq>`, the one name the edge promises never repeats and never rewinds.
Acceptance fact 3 was false for half the traffic until this.

**The test that should have caught it was asserting nothing.** It posted the log batch,
drained it, acked it, and only then opened the no-ack redelivery window — so the rows it
checked for duplication had left the edge before the window began. Both kinds now go in
before the window opens. The lesson generalises: a redelivery test has to prove the row
was *in* the window, not merely that a count did not change.

**An unreadable line could wedge a project for ever.** If the lowest unacked line will
not parse, nothing parses, nothing is acked, the cursor never moves, and the next cycle
asks for the same line again. `Answer::last_seq` was parsed and never read; it is the way
out, because it names the highest seq the edge just served whether or not we could read
it. A 200 that yields no usable row now acks to it and counts the lines as poison.

**`Bugs::store` swallowed an index-write failure.** It logged and returned `Ok` on the
reasoning that the object was safe in the bucket. It is not safe: an object with no
`bugs_envelopes` row is invisible to the sweep, which is the only thing that would ever
look at it again. Under the drainer that is data loss with extra steps — the caller acks,
the edge deletes its copy, and a crash before the in-memory digest finishes takes the
only remaining one. It returns an error now, so the drainer defers.

**Cursor starvation has a belt.** A replacement edge whose cell was rebuilt from an older
bucket snapshot hands out sequence numbers the cursor has already passed, and a strictly
monotone cursor then starves in silence for ever. An answer with no rows and a
`X-Ingest-Remaining` above zero is the unambiguous shape of it: logged on the first
occurrence, and on the second in a row the cursor is rewound to what the edge says it
last served. Rewinding is safe by construction — at-least-once is the whole contract —
and starving is not.

Smaller ones, same batch: the drain token is redacted from every `Debug` (the trait is
required and all three holders carried it in a `String`); `Reach::Down` compares on kind
rather than on message text, because two unreachable ticks whose error strings differ by
a port number are one state and comparing the text put a Down/Up pair in the log every
thirty seconds; `note_up` waits for the registry push as well as the drains; the iroh
transport keeps one QUIC session instead of handshaking up to sixty-four times a cycle,
and its connection driver is aborted by a `Drop` guard rather than on the happy path
only; and `key_at` mints a key only when the file is *absent* — any other read error
used to mint a new identity over the old one, which is permanent exile from the ingress
allow-file.

**Revocation is database-only.** `active` has no UI and no CLI verb; the column, the
setter, and the registry push are the whole of it, so revoking a project today means
writing the column by hand. SPEC says so too.

**The tests are split, and the split is deliberate.** `viewer/tests/bugs_drain.rs` drives
the real edge: MinIO, `celld deploy`, a real node, real envelopes over real HTTP. It is
one test function because nextest gives every function its own process, and a node per
fact would be eight containers. It skips, loudly, when docker or celld 0.2+ or esbuild is
missing; `NASHCODE_REQUIRE_CELLD=1` turns the skip into a failure for a machine that is
supposed to have them. Two facts are unit tests in `drain.rs` instead, because a real
edge cannot be asked to hold still in either state: a digest queue that is exactly full,
and an ack the edge refuses.
## The upstream column, phase 1: declare and mirror (2026-08-19)

`plans/whole-stack.md` phase 1 and SPEC's "Stack (upstream dependencies)" are the
contract. `viewer/src/upstream.rs` holds all of it; `web/stack.rs` is the one route.

### Mirrors are keyed by the whole URL, and the manifest `name` is decoration

The plan left the namespace open: `up/<name>` per stack, or the full host path with
global dedup. Full host path wins — `<mirrors>/up/<host>/<path>.git`. Sourcegraph
built one-host-per-instance and spent years undoing it, and the same shape here would
mean two repos that both depend on celld each keeping a copy, and no way to ask "who
in the tailnet depends on this". The URL is the identity; the short `name` is display
only, which is why it is validated as a plain name and then never used for a path.

Normalization is small and written down: the parser lowers scheme and host, one
trailing `/` and then a trailing `.git` come off, query and fragment are dropped, and
the port is part of the key because two servers on one host are two upstreams.

Every path component is *checked*, not cleaned: a segment outside
`[A-Za-z0-9._~-]`, or one that is `.`, `..` or leading-dash, refuses the whole URL
with an error in the stanza. Cleaning would be the more forgiving choice and the
wrong one — two different URLs could clean down to one directory, and a shared mirror
is a shared answer.

Review found three ways the first version of that rule still let two URLs land on one
directory. Each is now a refusal with a test:

- **`.../a/.git` became `.../a`**, which belongs to a different URL. The `.git` suffix
  now comes off before any trailing-slash trimming, so the last segment is left empty
  and refused rather than quietly disappearing.
- **`.../a/b.git/c` put a plain directory at `up/<host>/a/b.git`** — exactly where
  `.../a/b`'s mirror goes. Every clone there afterwards fails with "not a git
  repository", forever, with no self-heal. Any path segment ending in `.git` is
  refused.
- **A URL carrying credentials** keyed a mirror that repos share and that is fetched
  anonymously. Refused, and git is handed the parser's normalized URL rather than the
  manifest's own string.

The third defence is `is_repo`, which decides clone-versus-fetch by looking for a
`HEAD` file rather than for a directory. A stray directory at a mirror's address now
fails one clone and says so, instead of being fetched into forever.

### Plain `http` is for this box only

`http://` and `https://` of one host share one mirror directory by design — the scheme
is not in the key. That means an `http` spelling in one repo's manifest would silently
downgrade the transport of a mirror another repo declared over `https`. Adding the
scheme to the key would fix the downgrade by doubling the mirrors, which is the wrong
trade. Instead plain `http` is allowed only for loopback (127.0.0.0/8, `::1`,
`localhost`) — what the tests and a local dgit use — and everything else must be
`https`, refused in the same words a bad scheme gets.

It also closes most of a port-scan oracle: a manifest can no longer aim the fetcher at
arbitrary hosts on the private network over plain http and read the outcome out of the
stanza.

This is the one place the implementation is *stricter* than SPEC was, so SPEC moved
with it: the manifest bullet now says `https`, or `http` to loopback, and names the
credential and directory-collision refusals alongside it. Tightening a contract still
changes it, and a contract nobody updated is a contract nobody can trust.

### The stack stanza sits outside brain's tip cache

`Brain` caches the git-derived half of each repo against its branch tips. The stack
stanza cannot live there: a `track` dep moves when a branch in somebody else's repo
moves, and a mirror whose fetch failed has to be able to say so today, with none of
our own tips having moved. Folding upstream state into the cache key would mean
computing that state to decide whether to use the cache, which is the whole cost
anyway. So the stanza is built fresh on every `/brain`, beside `code` and `activity`.

The cost is two local git calls per repo per request (default branch, then `git show`
of the manifest), plus one `rev-parse` per dep and a second `cat-file` for a pinned
one. A repo with no `.nashcode/stack.toml` pays the first two and grows no `stack` key
at all — absent, not null, the way `architecture` is absent for a repo nobody has
drawn. A repo whose *git* failed is a different answer again: the stanza appears with
a manifest-level error, because a mirror having a bad minute must not look like a repo
that never declared anything.

### Upstream fetches are anonymous

`GIT_TOKEN` is the dgit push token. An upstream is somebody else's server, so
`upstream_repo` builds its handles with `Auth::default()` and the token is never
offered to github.com. Our own mirrors keep the token; these do not.

### What a `pin` can and cannot reach

A pin is fetched until `git cat-file -e <pin>^{commit}` says the commit is on disk,
and then never again — upstream cannot change what a commit says. The fetch itself
asks for `refs/heads/*` and `refs/tags/*`, which is every commit a normal upstream
publishes. A pin upstream does not publish on any of them is therefore never found,
and the stanza says exactly that: *pin `<rev>` is not in the upstream's branches or
tags*. The first version of this left `error: null` there, which read as "still
loading" forever; being wrong and being behind are different states and now say so.
Retries stay on the 30-minute interval a `track` dep uses, for the same reason:
hammering a server that does not have the commit will not conjure it.

"Then never again" also means a satisfied pin does not inherit its neighbours'
trouble. Two deps can name one URL — one pinned, one tracking a branch — and share one
mirror and one fetch state. If the branch goes dark, the tracked dep is stale and the
pinned one is not: it already has its commit, and there is nothing left it could want.

A `pin` must look like a commit id (7 to 40 hex digits) and a `track` must look like a
branch name. That is stricter than git — a tag is a legal pin to a person, and git
takes a 4-digit abbreviation — and it is what lets both go straight into a git
argument without an escape hatch. Seven is the floor because four is ambiguous in any
repo worth pinning, and a pin that quietly resolves to the wrong commit is worse than
one that will not parse.

### Sync has a budget

`POST /{repo}/stack/sync` exists because half an hour is sometimes too long. Without a
limit it is also an amplifier: one endpoint anyone on the tailnet can call in a loop,
aimed at a third party's server. So a mirror will not go back to the wire twice inside
`SYNC_DEBOUNCE` (60s); inside the window sync answers from disk. The pin rule is
checked first, so a satisfied pin still short-circuits for its own reason rather than
looking rate-limited.

The window is an atomic on the shared handle rather than a constant, so a test can
stand on both sides of it without sleeping for a minute. That is the only reason it is
a knob.

### `Repo::rev_parse` was echoing its own flag

`git rev-parse` echoes back every argument it does not recognise as a revision, and
`--end-of-options` is one of them — so `Repo::rev_parse` was returning two lines with
the commit on the second. The one caller in tree (`pages.rs` `current_blob`) was
unharmed only because both sides of its comparison carried the same pollution. Fixed
in `git.rs` by adding `--verify`, which promises exactly one object id and nothing
else. A revision that does not exist was always an error — git exits 128 with or
without the flag; the echo was the whole bug. `upstream.rs` uses the fixed helper;
`git.rs` is on the claim row and the other agents have a note.

### The clock, and why the stanza also pokes it

A `track` dep goes stale with no push, no webhook and no page load of ours to notice
— so `Upstreams::watch` ticks every 30 minutes over every configured repo. The first
tick is immediate, which warms the pins on a cold box; a repo whose own mirror has
not finished cloning yet has no manifest to read, and the second trigger covers it:
building the brain stanza starts anything overdue in the background, exactly the way
`mirror.rs` refreshes behind a page load. `POST /{repo}/stack/sync` is the third
door, and the only one that waits for the wire.

### Smaller choices

- **One bad `[[dep]]` is one error, not a dead manifest.** Every field deserializes
  as optional, so a dep missing `name` or declaring both `pin` and `track` shows up
  in the stanza with its own error while its neighbours are fetched normally. Only
  TOML that will not parse at all becomes the manifest-level `error`, and then `deps`
  is empty.
- **A refused dep is stripped of its mode and its location**, not merely flagged, so
  there is nothing for a later code path to act on by accident.
- **The first `[[dep]]` of a repeated name is the one that works.** The duplicate is
  the one refused, which reads the way a person writing the file would expect.
- **`POST /{repo}/stack/sync` takes the ordinary origin check.** No exemption: it is
  a state-changing route like every other, and the CLI and curl send no fetch
  metadata, so they are unaffected. A repo with no manifest answers
  `{"repo": ..., "stack": null}` rather than an empty stack, because "declares
  nothing" and "declares nothing that works" are different answers.
- **The tests serve real bare repos over git's dumb HTTP protocol** — a bare repo
  plus `git update-server-info` plus a 40-line static file server on a loopback port.
  The fetches are real fetches with no network, and the server's request counter is
  what proves a satisfied pin stops asking.

### Known, deliberately not fixed in phase 1

Each of these came out of the phase-1 review, was weighed, and was left. None of them
is a correctness bug; all three are shapes that only start to hurt at a size this does
not have yet.

- **The stanza is uncached.** Every `/brain` costs two local git calls per repo plus
  one or two per dep. That is the same cost class as unclaimed open-work item 2 in
  `COORDINATION.md` (cache `StackGraph::infer` per set of tips), and it wants the same
  answer: a manifest cache keyed by the default-branch tip, and dep state keyed by the
  mirror's own tips. Worth doing when the repo count or the dep count grows, not
  before — the cache would have to be invalidated by the very fetches it is trying to
  avoid observing.
- **`watch` is fully serial.** One task walks every repo and every dep in order, so one
  blackholed host stretches the whole 30-minute cadence by its timeout. Git's
  low-speed timeout bounds it at 30s per fetch, so the cadence degrades rather than
  stalls, and with two upstreams it is invisible. Bounded fan-out — a `JoinSet` with a
  small limit — is the fix when a real stack has ten deps and one of them is slow.
- **Two repos sharing one mirror is tested for the outcome, not for the race.** The
  dedup test syncs them one after the other, so the per-mirror lock is exercised but
  the contention on it is not. Proving that a concurrent pair produces one clone and
  one fetch needs a barrier the test bed does not have.

## The upstream column, phase 2: browsing it (2026-08-19)

Phase 1 mirrored the column and reported it in the brain. Phase 2 opens it: one page
that is the column, and a read-only code browser over each dep's mirror.

### One page, N trees — and never a merged one

`/{repo}/stack` is the repo followed by each dep at the commit its mirror answers with.
Every entry opens that dep's own tree at `/{repo}/stack/{dep}`, and nothing is ever
spliced into a single tree that pretends the column is one repository. A merged tree
would have to invent an answer for two deps that both carry `src/lib.rs`, and the
answer it invented would be wrong at exactly the moment somebody relied on it.

The page is built from `Upstreams::stack`, which is the same call the brain stanza
makes: it reports what is on disk and starts whatever is overdue behind the caller's
back. A dep whose mirror is absent, or whose declaration was refused, renders as a
danger-bordered card carrying the reason — the shape `unavailable_card` uses for a repo
whose first clone has not landed. It is a state, not an error, and the rest of the page
is unaffected.

### `{dep}` is a name in one manifest, not a name on the box

Mirrors are shared: two repos declaring the same upstream share one directory. Names are
not. `{dep}` is looked up in the *declaring* repo's manifest, so `/other/stack/dgit` is a
404 even when `demo` declares `dgit` and its mirror is right there on disk. Nothing about
a URL a repo never named is reachable through that repo's routes.

A dep the manifest refused — bad URL, both `pin` and `track`, a name that is not a name —
has no mode and no mirror path after validation, so it has nothing to open: 404 as well.
A dep that is fine but whose commit has not been fetched yet is neither; it gets a card
that says so, because that is a slow upstream, not a reader's mistake.

### `?rev=` is the pin grammar first, then a question for the mirror

The rev in a query string reaches a git argv, so it is held to `upstream::is_commit_id` —
the same 7-to-40 hex-digit rule a `pin` in the manifest is held to, exported rather than
written twice. Then one question for the mirror: `rev-parse --verify <rev>^{commit}` both
asks whether the commit is on disk and answers with its full id, since peeling to
`^{commit}` has to read the object to do it. One call, not a `cat-file -e` followed by a
resolve. A commit the mirror does not have fails it, and that is the 404.

Never a fetch. Browsing is a read of what has been mirrored; if a page load could pull a
new commit, any link on any page would be a request aimed at somebody else's server, and
`SYNC_DEBOUNCE` would be a budget with a hole in it. The dep tree and blob routes hold to
that absolutely; the column page is the one surface that starts anything, and only in the
background, exactly as the brain stanza does.

Counting requests is how the tests hold it, and a counted claim is only worth its
arming: a dep is not due for half an hour after its last attempt, so a test that syncs
and then asserts "no more requests" would pass no matter what the pages did.
`Upstreams::set_track_interval` drops the interval to zero and every counting test first
proves the column page *does* reach the upstream under exactly those conditions. A
regression that gave a dep's tree page the column page's refresh would then show up as a
number that moved.

The full commit id is what travels on the links out of a page, not the abbreviation the
reader typed: one canonical URL per tree, whatever spelling got them there. Browsing at
the declared commit keeps the clean URL — the `?rev=` only sticks when it was asked for.

### Read-only, visibly

No pencil, no "New file", no comment composer, no raw download, and no POST route under
any dep path. The markup is the code browser's minus every affordance that writes.
Markdown is the one non-obvious cut: `render::markdown` autolinks against the *viewing*
repo's document index and branch list, so a dep's README would grow links to plans and
files that live somewhere else entirely. Upstream source reads as source.

### Gitlinks: `.gitmodules` at the tree's own commit, through the one normalizer

A gitlink records a commit and nothing else. The URL lives in `.gitmodules`, which is
read at the same commit as the tree being rendered — a submodule's URL can move between
commits like any other file, and reading it at the tip would attribute today's URL to a
year-old tree.

The URL is then put through `upstream::locate`, the same function that keys the mirrors,
and matched on the mirror *directory* rather than on the string. That is what makes
`https://GitHub.com/a/b.git/` in `.gitmodules` find the dep the manifest declared as
`https://github.com/a/b`. A gitlink that lands on a mirrored dep of the same repo becomes
a link to that dep at the gitlink's own commit, via `?rev=`; everything else keeps the
inert label it has always had, including relative URLs (`../sibling.git`), which have no
host to key a mirror by.

This works in any tree the viewer renders — the repo's own code tab and a dep's tree
alike — and the column consulted is always the declaring repo's. A tree with no submodule
entries costs nothing: the function returns before it reads anything.

**The link is drawn from the tree, not from a fetch, and is not gated on the commit being
present.** A gitlink pinning a commit the mirror has not fetched still renders as a link,
which then 404s. The alternative — one `cat-file -e` per gitlink before deciding whether
to draw a link — makes the affordance flicker with the mirror's state, and a link that is
sometimes there is worse than a link that says "not here" when followed.

### Smaller choices

- **The code browser's helpers are exported, not copied.** `numbered_code`, `entry_icon`,
  `human_size` and `shiki_lang` became `pub(super)` in `pages.rs` — four one-word
  changes, versus 150 lines of duplicated highlighting that would drift. The page bodies
  and the breadcrumb component *are* duplicated in `stack.rs`, deliberately: the dep
  pages differ in every URL they build and in everything they refuse to show, and
  parameterizing `pages.rs` for a second caller would have touched history other agents
  are working in.
- **Two tabs, one word.** "Stacks" (branch stacks) and "Stack" (the dependency column)
  sit side by side in the nav, and each page carries a one-line pointer at the other. A
  reader who lands on the wrong one finds out in a sentence.
- **The upstream test fixtures moved to `tests/common`.** `Origin` — bare repos published
  over git's dumb HTTP protocol on a loopback port, with a request counter — and
  `bed_declaring` are now shared by `stack_deps.rs` and `stack_browse.rs` instead of
  copied. Additive to the common harness; no `Config` fields changed.
- **`sync` is not a dep name.** `POST /{repo}/stack/sync` is a static route, so
  `/{repo}/stack/sync` can never be a dep's page. A manifest using the name would get a
  link on the column page that answers "not with this method". Validation refuses the
  name where the manifest is read, with an error that says why, rather than letting the
  collision surface as a dead link.
- **Refused is not stale.** A dep whose declaration was refused has no mode after
  validation, which is exactly how the column page tells the two apart: an upstream
  nobody could reach is behind and reads "stale"; a URL that was never going to be
  fetched reads "refused", in danger colours, with the reason underneath. Calling both
  of them stale would suggest the second one is one good minute away from working.
- **The gitlink fixtures are written straight into the index** with `update-index
  --cacheinfo 160000,<sha>,<path>`. A real `git submodule add` would clone the upstream
  into the fixture and prove nothing extra: the tree entry plus `.gitmodules` is the
  whole of what the viewer reads, and this way a gitlink can point at a commit that is
  deliberately absent.

### Known, deliberately not fixed in phase 2

Each came out of the phase-2 review, was weighed, and was left. None is a correctness
bug in what phase 2 promises; each is worth doing when the shape it assumes stops
holding.

- **A manifest read error wears the parse error's clothes.** `Upstreams::manifest`
  reports a git failure as a manifest whose `error` is `cannot read ...: <git stderr>`,
  which the column page renders under "this manifest will not parse". Two problems in
  one: a reader cannot tell a broken TOML file from a mirror having a bad minute, and
  git's stderr can carry a server-side path onto a page that needs no authentication.
  The fix is a second error kind on `Manifest`, rendered as a different card, with the
  git text logged rather than shown. It is a small change with a brain-shape decision
  attached, which is why it is not folded into a browse commit.
- **Two deps declaring one URL: the first wins a gitlink.** `submodule_links` matches on
  the mirror directory and takes the first dep whose path matches, so if a manifest
  names one upstream twice the gitlink links to whichever was declared first. Manifest
  order is at least stable and explains itself; naming one upstream twice is already
  odd. A rule that prefers the dep whose commit is on disk would be the better answer if
  it ever comes up.
- **The `.gitmodules` read is uncapped.** `show_file` reads the whole blob, so a repo
  carrying a hostile `.gitmodules` makes the viewer hold it in memory for one render.
  Same class as the README the code tab already reads at full size, and it wants the
  same answer: a byte cap on the page reads, in one place, rather than a special case
  here.
- **A gitlink-bearing tree re-reads the manifest.** Rendering a tree with submodules in
  it costs `column()`: the default branch, the manifest blob, and one `rev-parse` per
  dep, plus one more per matched gitlink. Trees with submodules are rare and columns are
  short, so it is invisible today; a wide column under a repo that vendors everything is
  where it starts to hurt, and the answer is the manifest cache the phase-1 notes
  already want for the brain stanza.
- **The read-only POST assertion covers one path of four.** The test posts to a dep blob
  URL and takes a 404 or 405. `/{repo}/stack`, `/{repo}/stack/{dep}` and the tree route
  are not posted to. There is no handler that could accept them — the router only knows
  the pages — so this is coverage, not a hole.

## Phase 4 of error tracking: Pushover, context capture, the self-DSN

### The digest is the only place that knows something is news

`index::record` already returned a `Landing` — new, regression, repeat, duplicate — and
the digest already threw it away. Every notification decision hangs off that value now,
and nothing outside the digest queues an issue notification at all. The alternative was
a rule at each mutation site, which is how a tracker ends up pushing twice for one event
and not at all for another.

Two dedupe keys carry the whole "exactly once" property, because the queue refuses a
repeat key:

- `issue/<id>/new` — once ever. An issue is only new once.
- `issue/<id>/regression/<events>` — the event count at the moment the issue reopened.
  Unique per regression, so an issue fixed and broken three times rings three times, and
  the second event on a reopened issue rings not at all. A plain `issue/<id>/regression`
  would have silenced every regression after the first, which is the notification that
  matters most.
- `issue/<id>/ladder/<rung>` — once ever, because the counter only goes up.

That last one is why there is no cycle counter on `bugs_issues`. A rung is crossed once
in an issue's life whatever happens to its state in between.

**That is a decision, not an accident, and the SPEC said the opposite for a day.** The
first draft claimed a rung could ring again per resolve cycle. It cannot: the event
counter never resets, so a rung that has been crossed can never be crossed again, and a
key with a cycle in it would have been a key that could never collide. The alternative —
resetting the count on resolve — was considered and refused, because the ladder is about
volume and volume is cumulative: an issue at 900 events that gets resolved and comes back
is not an issue at zero. The regression push is the state change; the ladder is the
weight. The SPEC sentence was amended to match the code rather than the other way round.

### The message budget belongs to the tags, not the exception

The first build spent the whole 1024 characters on the exception value and clipped the
tags off the end. A four-kilobyte Python repr is not worth `environment=prod`, so the
fixed parts — the lead line and up to four tags, each tag value clipped to 64 — are laid
out first and the exception value gets what is left. The test that found this posts a
4000-character value.

### 429 is not a 4xx here

The goal doc says "any 4xx → never retry" and "429 → park until reset" in the same
breath, and 429 is a 4xx. The specific rule wins: a 429 means the message was fine and
the account is out of budget, so the message stays pending and the whole queue parks. A
400 means Pushover read the message and judged it, and every retry gets the same answer,
so it is marked failed and never asked about again — otherwise one malformed message
wedges every notification behind it for ever.

A 5xx defers with doubling backoff off a 5-second floor, capped at 15 minutes, and gives
up after 12 attempts. The goal doc set the floor and said nothing about a ceiling or a
limit; an unbounded retry is a row that is pending for ever and a `pending` count that
means nothing.

### One suppression notice, not one per message

The hourly cap parks the queue until the oldest message in the window leaves it. That
means the cap trips again the moment the queue moves, and a notice each time is exactly
the flood the cap exists to prevent. `bugs_push_state.suppressed_at` makes the second
trip inside the hour silent. The notice itself is sent *over* the cap on purpose: it is
the message that tells a person the others exist.

### Context capture reads at a revision, and says which one

Source moves. A line number from last week's release read against today's tip points at
whatever is there now, which is worse than showing nothing because it looks right. So
`sentry.release` decides the revision when it is something the mirror can resolve, and
when it is not the page prints "tip, not release" beside the commit it did use.

Only something shaped like an object id is tried — hex, 7 to 40 characters. A release
named `v2.4.1` or `main` would resolve through `rev_parse` to a tag or a branch, which
is not what the sender meant and moves under us. `^{commit}` on the end refuses anything
that is not a commit.

### Suffix matching replaced the per-directory listing

The old `resolve_in_repo` listed each parent directory with `ls-tree` and looked for the
name. That cannot answer `/app/src/foo.py`, which is what every containerised SDK
reports, because `/app/src` is not a directory in the repo. One `ls-tree -r` per (repo,
revision) is now read instead and every question is answered in memory.

The rule is the longest suffix, on segment boundaries, that names exactly one file. Two
properties fall out of it and both are deliberate:

- Matching stops at the *first* suffix that matches anything. Every shorter suffix
  matches at least as many files, so there is nothing to gain by continuing.
- That suffix matching several files is `Ambiguous`, and ambiguous renders as plain
  text. Two files called `utils.py` in different packages are precisely the case where a
  guess sends a person to the wrong file and they believe it.

Segment boundaries matter: `src/notfoo.py` ends with the string `foo.py` and is a
different file.

### The source cache is keyed on the commit, never on the release string

The first build keyed it on the raw `sentry.release`. That is a sender-controlled
free-text attribute on every log row, and the consequence was quadratic in the wrong
variable: a hundred rows carrying `v1.0.1` … `v1.0.100` would open a hundred sources —
`default_branch`, `tip` and a whole `ls-tree -r` apiece — and every one of them would be
*the same tree*, because not one of those strings resolves to a commit.

Resolving first and keying on the answer collapses them onto one. Three properties fall
out and each one matters on its own:

- A release that cannot be an object id costs no subprocess at all, because the syntax
  check runs before the `rev_parse`.
- A release that can is remembered, so fifty rows naming one release ask once.
- Every unresolvable release lands on the same tip source, which is already open.

`MAX_SOURCES = 4` sits on top, because collapsing is not a bound: four real commits on
one page is a deploy in progress, forty is a sender being strange. Past the cap the page
falls back to the tip, and the snippet then says "tip, not release", which is what it is.

The tree listing is byte-capped at 8 MiB too — it used to go through `list_files`, which
is uncapped, while the *snippet* read beside it was capped. A truncated listing turns
suffix matching off rather than trusting it: membership still proves a path exists, but
uniqueness over a partial list is not uniqueness, and a wrong link is worse than plain
text. The last entry is dropped as well, since a cap that lands mid-name would invent a
path that is not in the repository.

### What a page is allowed to spend

Reading source is one `git show` per distinct `file:line`. A page of a hundred log rows
out of one hot loop is three distinct sites, and the per-page cache is what usually
decides this. `SNIPPETS_PER_PAGE = 24` is the cap for the page that really does name a
hundred different files — every link is still there, the first two dozen carry their
source. No measurement said 24; it is the number of rows that fit on a screen, and it is
the constant to raise first if anybody complains.

### `/bugs` JSON is an object now

It was a bare array of projects. Whether a notification can still get out this month is
part of the state of the feature, and a reader that has to make a second request for it
will not make it. `{"projects": [...], "pushover": {"on": bool, "budget": {...}}}`.

### Three things the review found that a test would not have

- **The panic hook bypassed both non-recursion guards.** The tracing layer's filter is
  the obvious place for them, and it is the wrong place for two kinds of event: a panic,
  which the panic hook captures directly, and a log record from a foreign crate, which
  `enable_logs` captures directly. A panic inside the ingest door while it handles a
  self-report envelope is precisely the loop the guards exist for, arriving by the one
  path the layer cannot watch. Both guards now also sit in `before_send` and
  `before_send_log`, which every event passes through whatever captured it.
- **`NASHCODE_RELEASE` was `option_env!`.** Compile-time, and nothing anywhere set it —
  so the one project this phase dogfoods was the one project whose snippets always read
  "tip, not release". It is `std::env::var` now, with a doctor line when the self-DSN is
  set and the release is not.
- **The 60/minute self-report cap discards; it does not defer.** Worth saying plainly
  because "cap" reads like a queue. Over the limit, `before_send` returns `None` and the
  event is gone. That is the right trade for a last-resort guard against a hot path that
  fails on every request — the alternative is a buffer that grows exactly when the
  process is least able to afford one — but it means the cap is a data-loss mechanism
  and should never be the *first* line of defence. The name-based and task-based guards
  are.

### Two smaller things, written down because they will look arbitrary later

- **A message is claimed before it is sent, not marked after.** `next_pending` is one
  `UPDATE … RETURNING` that writes a 60-second lease into `not_before`. Nothing today
  runs two senders — one task per process — but "one process" is a property of the
  deployment, not of the code, and a restart that overlaps its predecessor would
  otherwise have both send every pending notification. The lease rides the field the
  retry path already respects, so a sender that dies holding one releases it by doing
  nothing.
- **A future `nashcode bugs reindex` must build its digest with `Notifier::off()`.**
  Re-digesting the bucket replays every event through `index::record`, which will report
  every issue as new again. The dedupe keys stop the *queue* from doubling, but only
  because the rows are still there; a reindex into a fresh database has no rows and would
  ring for the entire history at once. The sweep is safe today because it re-digests
  envelopes whose issues already exist.

### `Config` grew three fields, and `db.rs` grew two functions

`pushover`, `public_url` (`NASHCODE_URL`, which until now only the CLI half read) and
`bugs_self_dsn`. Every exhaustive `Config { .. }` literal needed three more lines —
nine files. `db::now_offset` and `db::from_unix` are new: every deadline stored here is a
timestamp string compared lexicographically, so it has to come out of the same formatter
as `db::now` or the comparison quietly means nothing.

## Phase 5: crons, quotas, eviction, mutes

### Three dependencies, not one

`croner` was the one the SPEC named. Its whole API speaks `chrono::DateTime`, so `chrono`
is not a choice — it is croner's alphabet. `chrono-tz` is the third, and it is the one
worth arguing about: a `monitor_config` carries an IANA timezone, and `0 9 * * *` in
`America/Chicago` is 14:00 UTC in summer and 15:00 in winter. Evaluating that in UTC
would file a missed-check-in alert every morning for a job that ran on time. Storing the
zone and ignoring it is worse than not storing it, so the zone is applied — and a zone
this box cannot resolve is dropped at parse time rather than kept and quietly disregarded.

Storage stays on `time`. chrono is confined to `bugs::crons`, between reading a Unix
second and producing the next one.

### The schema lives with the module, not in `index.rs`

`bugs_monitors`, `bugs_checkins`, `bugs_incidents` and `bugs_quota` are created by
`crons::migrate` and `quota::migrate`, the way `logs` and `pushover` already own theirs.
`Bugs::new` calls each. Only the columns on tables `index.rs` owns went through its
`ADDED_COLUMNS` list: `bugs_events.irrelevance` and the six `bugs_issues.mute_*`.

`irrelevance` defaults to 0, which makes every event predating the column the *most*
relevant of all. That is deliberate: those are the events somebody has been reading, and
age retires them soon enough on its own.

### Only the server says missed or timeout

A `check_in` may say `in_progress`, `ok` or `error`. Anything else — including the
`missed` and `timeout` a client has no way to know, and any word this build does not
recognise — is stored as `error` with a `coerced` flag. A process healthy enough to
report that it was missed was not missed.

The two states the server does own are computed by a one-minute sweep off two stored
deadlines, `next_checkin_latest` and `timeout_at`. Timeouts are decided first: a run that
overran is a run that started, so filing it as missing as well would file one silence
twice. Each transition pushes the deadline past `now`, which is what stops the same
lateness being filed every minute.

### A monitor needs a schedule or it is not a monitor

Upserted only when a `monitor_config` carries a schedule this box can evaluate. A
check-in with no config, or with a config whose schedule will not parse, is stored and
declares nothing. The reasoning is that a monitor with no schedule can never be late, so
inventing one puts a row on the page that says "ok" for ever — worse than an empty page,
because it looks like coverage.

`failure_issue_threshold` and `recovery_threshold` are parsed past and not implemented.
One failure is an incident here. Thresholds are a real feature and they are not in the
SPEC bullet.

### The all-zero `check_in_id` is a sentinel and never an identity

The protocol lets an SDK send `"00000000000000000000000000000000"` to mean "update
whichever run is still in progress". Storing that as an id would have been quietly
catastrophic: check-in uniqueness is `(project, check_in_id, status)`, so every `ok` a
sender ever sent with the sentinel would collide with the first one and every run after
the first would move nothing at all. It gets a minted id instead. The cost is that the
two halves of that run are not paired, which costs a duration on the page.

### One open incident per monitor

A job that errors, then starts missing, then times out is one thing being broken. A
second failure of a different kind opens nothing and rings nothing; an `ok` closes the
one incident and rings the recovery. Both go through `Notifier::cron_incident` and
`Notifier::cron_recovered`, which are two more rows on the existing push queue and not a
second send path. Dedupe keys name the incident, so each opens once and closes once
however often the sweep runs.

### Quotas are checked before the body and counted after it

Two calls, and the split is the point. `quota::check` runs the moment the project is
known and before a byte is read or decompressed — a gate that fires after 20 MiB is in
memory has spent exactly what it exists to save. `quota::record` runs once the request is
stored. Nothing increments a counter for a request that turned out to have the wrong key,
so knowing a numeric project id does not let anybody spend that project's month.

The gap between the two is a race, and the overshoot it allows is a handful of requests
under concurrency. That is the right trade for a budget: a lock here would put SQLite
contention on the hot path to make a bucket bill exact to three decimal places.

Windows are fixed, not sliding: one row per project per window, rolled on read and on
write. A sliding window needs a row per request. The boundary effect — up to two windows'
worth inside one window's span, either side of a roll — is the standard cost and is fine
for a number that bounds storage rather than a login form.

**The gate fails open.** A database that will not answer a question about a budget is not
a reason to drop somebody's telemetry, and the bounded digest queue is still there to
stop the box from drowning.

**The refusal names the earliest reset, not the latest.** With two windows full, being
asked back too soon costs one wasted request that gets the same answer; being asked back
too late costs an SDK sitting on events it could have delivered.

**A 429 carries `X-Sentry-Rate-Limits: <seconds>::project`** as well as `Retry-After`.
Empty categories means every category and the scope is this DSN, so an SDK stops sending
altogether for the window instead of retrying into a refusal. `ingester/` needs no change:
the edge does not gate quotas, it buffers, and the viewer is where the budget lives.

**Drained rows bypass the quota**, and this is the one hole worth writing down. The gate
is on the tailnet HTTP doors, where a live SDK can hear a 429 and back off. A drained row
has already left the edge and there is nowhere for it to go back to — gating it would end
the cycle with no ack and wedge that project's buffer for as long as the quota lasts,
which for the monthly window is a month. Eviction is the backstop that bounds storage
whichever door an event came in by.

### Eviction: how close to Bugsink

Close on the arithmetic, deliberately apart on one rule.

Kept verbatim: the two-part score, `nonzero_leading_bits(round(r · count · 2))` fixed at
ingest from the issue's *stored* count, plus `log₄(hours + 1)` computed at eviction time,
added together. Base four is Bugsink's empirical calibration and the reason it works is
worth keeping — it makes a week aging into a month cost about what a doubling of an
issue's volume costs, so age and volume trade against each other instead of one always
winning. The 500-row batch ceiling is theirs too, and so is the 5%-of-cap floor.

Three deviations:

- **The multiplier is derived from the event id, not from a random source.** Bugsink
  randomises so that a count hovering around one value does not hand out the same score
  every time — which matters, because a project that fills and is evicted and fills again
  hovers by construction. An FNV-1a hash of the event id breaks the same ties for the same
  reason and is reproducible, which is worth a lot in a function that decides what gets
  deleted: the same event scores the same in a test, on a rerun, and after a reindex.
- **The candidates are sorted, not walked down a falling threshold.** Same events, stricter
  order, and a project's candidate set is at most a cap's worth of small rows.
- **`keep` is absolute.** Bugsink recurses with `include_never_evict=True` rather than fail
  to reach its target — "never say never". Here the first-seen event and every regression
  trigger stay, and a project that reaches its cap with nothing left to take gets a warning
  line. An issue whose first event is gone cannot answer "when did this start", which is
  the question the issue page exists for. A cap that can only be met by deleting the answer
  is a cap that is too small, and that is a thing to say rather than to solve silently.

`bugs_issues.events` is **not** decremented. It is a lifetime counter and the escalation
ladder depends on it only ever going up; eviction removes stored payloads, not history.

Rows go before objects. An object with no row is invisible and costs storage; a row with
no object is a dead link where a person expected an answer. So a pass interrupted halfway
leaks bytes rather than breaking a page.

Eviction rides the cron sweep's one-minute tick rather than the nightly prune. A project
under its cap costs one indexed `COUNT(*)`; a project over it must not stay over for an
hour.

### Mute-until counts over a window, not over a life

Sentry's "ignore until it happens this many times in this window", read the way the words
say: an issue at three events an hour stays muted for ever under a rule of ten-in-an-hour,
and that is the point of the rule rather than a bug in it. The window is fixed and rolls
whole; a sliding one needs a row per event to be exact, which is a table the size of the
log store to answer "is this loud now".

Rules are evaluated on ingest, in the digest, and nowhere else. There is no timer, because
an issue nothing is arriving at never needs to come off mute — coming off mute is only
interesting when there is something to be told about.

**A duplicate landing does not advance a rule.** Found while writing the reindex test: a
re-digest replays every stored envelope, every event id is already indexed, and without
this the whole bucket would walk an issue's mute-until counter up and quietly unmute
everything somebody had silenced. A duplicate event id means nothing happened — the
issue's own counter does not move either.

Mutes are judged *before* the notifier sees the landing. An event that lifts a mute has to
reach the notifier as an event on an open issue, or the escalation ladder — which does not
ring for a muted issue, rightly — steps straight over the rung that very event crossed.

The rule lives in six columns on `bugs_issues` and not on the `Issue` struct. Adding fields
to `Issue` would have rippled into every exhaustive literal in the tests, for data only two
pages read; `mute::progress` is its own query and its own `mute` key in the issue JSON,
absent rather than null when nothing is armed.

`set_state` clears all six on every move, including a move *to* muted — the caller arms the
new rule immediately after. That is what stops a re-mute inheriting a half-counted window,
and what makes an unmute final.

A rule that will not parse is a 400, not a downgrade to "forever". Somebody who asked for
an hour and silently got for ever does not find out until the outage they miss.

### `sweep(true)` digests in silence

`digest::Job` grew a `silent` flag and `Bugs::sweep(all: true)` sets it; the worker then
runs on `Notifier::off()`. The crash-recovery pass (`all: false`) keeps its voice, because
those envelopes were genuinely never digested and their news is genuinely news.

Worth being straight about what the test proves today: against the same database every
replayed event is a duplicate, so the dedupe keys would cover it anyway. The flag is what
makes the *fresh-database* reindex safe — the case `nashcode bugs reindex` will actually
be, where no rows exist and every issue is new again. The test asserts the reachable half
(a replay moves nothing and rings nothing) and the flag carries the rest.

### Phase 5, after peer review

Six things changed shape. The three that were wrong are worth reading; the rest are
where the code and its own comments had drifted apart.

**Eviction now leaves tombstones, because deleting a row does not delete an event.**
The original pass deleted the `bugs_events` row and the per-event object and called the
event gone. It was not: `sweep(true)` — the reindex primitive — re-reads
`bugs_envelopes`, and the evicted payload is still sitting in an envelope object. On the
next reindex it found no `(project_id, event_id)` row, landed as an ordinary repeat,
wrote the row back, and **incremented the issue's lifetime counter a second time**. That
counter only ever goes up and the escalation ladder reads it, so every reindex after an
eviction inflated it permanently, by exactly the eviction volume. The old comment
claiming "a reindex never resurrects what was evicted, because the row is what it would
have read" was simply false.

`bugs_evicted_events (project_id, event_id, issue_id, evicted_at)` fixes it, written in
the same transaction as the delete — a row deleted without its tombstone is a row the
next reindex puts straight back, so a crash between two transactions would reopen the
hole. `index::record` consults it and answers `Duplicate`; the digest consults it once
more before the bucket write, which turns an object-store PUT into one indexed read on
the one path where that matters. The table lives in `index.rs` rather than `evict.rs`
because `record` reads it on the hot path and the events it tombstones are that module's.

The cost: tombstones are never pruned. They are about a third the size of the event row
they replace and they have to outlive the envelope that carries the payload, so pruning
them would restore the bug. Bounding them properly means envelope retention, which is
the next gap.

**`crons::record` is one transaction, and the gap it closed was reachable twice.** It
used to be three writes — upsert the monitor, insert the check-in, apply the state — and
the one-minute sweep could land between the second and the third: it found a monitor
whose deadline had passed because the `ok` had not been applied yet, opened a missed
incident and pushed it, and then `apply` landed, closed that incident and pushed a
recovery. Two phone notifications for a job that ran exactly on time. A crash in the same
gap was worse and permanent: the check-in row is durable, so redelivery hit
`INSERT OR IGNORE`, found the row, and returned before applying anything — leaving the
monitor at `unknown` with a NULL deadline, which both sweep predicates skip. It could
never go missing and never recover, for ever, silently.

One transaction closes the race outright. There is no test for it, and there cannot
usefully be one — the structure is the proof. The wedged monitor *is* tested, through
`stale_since`: an already-stored check-in is normally a replay and must move nothing, but
if the monitor's own `last_checkin_at` is behind that row's timestamp then the monitor
never absorbed it, and it is applied — from the row's timestamp, not from now, because
that is when the job actually ran. `transition` got the same treatment.

Notifications are collected inside the transaction as a `News` value and delivered after
it commits. `Db::with` holds one mutex over one connection, so queueing a push from
inside a transaction would take that lock twice and wedge the digest task against itself.

**A schedule that parses is not a schedule that fires.** `0 0 30 2 *` — the thirtieth of
February — is well-formed and croner accepts it, and `find_next_occurrence` then never
resolves. The monitor got a NULL deadline and became invisible to the missed sweep: the
page said "ok" for ever while covering nothing, which is worse than an empty page because
it looks like coverage. `Schedule::parse` now asks for one occurrence before believing a
pattern. Anywhere a deadline still computes to `None` after that — a daylight-saving gap
an interval step landed inside, a calendar step off the end of the range — now warns
instead of writing NULL in silence.

**Check-ins are pruned; envelopes are not, and that is the honest hole in this phase.**
`bugs_checkins` had no cap: a monitor checking in every minute files half a million rows
a year and nothing would ever remove one. It now rides the nightly log prune under the
same `retention_days`. Incidents and monitors stay — there are few of them and they are
the history a person actually reads.

But **eviction does not bound bucket storage.** It caps the index rows and the per-event
objects; the raw envelope objects hold the same payloads and are never pruned, so a
project at its cap has shed roughly half the bytes it appears to have shed. This is not
built and was not attempted this pass: envelope retention interacts with the reindex path
(dropping an envelope makes its events unrecoverable) and with the tombstones above
(dropping an envelope is the only thing that would ever make a tombstone safe to drop),
and that is a design question, not a patch. Phase 5 bounds the index and the event
objects. Envelope retention is open.

**Two smaller reversals.**

- A corrupt `mute_from` used to make a `mute-until` rule permanently unfireable: the
  unparseable stamp read as zero, the window looked like 1970, so it rolled on every
  event and the count reset to one for ever. A corrupt row silencing an issue permanently
  is the wrong direction, and the `mute-for` arm two lines above already argues the
  opposite explicitly — an unreadable deadline is treated as passed. The window now
  re-opens at now and keeps its count, so the rule stays fireable.
- `quota::WindowState`'s doc claimed it was shown on the project page. It was not — the
  function had no web caller at all. Rather than correct the comment down, the page and
  its JSON now carry it, because "why is my SDK getting 429s" should be answerable
  without reading a log. It renders only once something has been sent; three zeroes on a
  fresh project would be noise on a page that is about issues.

Errors in the digest's mute path are logged rather than swallowed, `mute::Rule::parse`'s
doc no longer contradicts the handler that refuses an unparseable rule with a 400, and
`crons::seconds_of` says so when it substitutes now for a stamp that will not parse.

## Repo discovery

**The parser lives in its own crate, `dgit-index/`.** `cli` already builds as a lib, so
the viewer could have depended on it, but that would invert the layering and pull agcli,
ureq, and a second tokio configuration into the server for one regex pass. The new crate
carries regex and serde and nothing else. No re-export shim was left behind in `cli`: the
four call sites and one fixture test name `dgit_index` directly.

**`Config.repos` is `Arc<RwLock<BTreeSet<String>>>`,** `std::sync` and not `tokio::sync`.
Every method on `Repos` returns owned data, so a guard cannot survive into an `.await` —
the compiler enforces that for us, which a tokio lock would not. A poisoned lock is
recovered rather than propagated: a panic somewhere else must not turn every repo into a
404. `Config::knows_repo` is unchanged in meaning and is still the only gate.

Two consequences of a set rather than a `Vec`:

- **The index page is alphabetical now**, not in `NASHCODE_REPOS` order. Nothing asserted
  the old order, and a discovered repo has no declared position to keep.
- **`Config::clone()` shares the repo set**, because the `Arc` is what is cloned. In
  production `Config` is built once; in a test that derives one config from another with
  `..(*bed.config).clone()`, override `repos` unless sharing is what you meant.

**The index fetch is authed** with the same basic auth `x:$GIT_TOKEN` the clones use.
Anonymous would list exactly the same repos — dgit hides a `private: true` repo from its
index with or without credentials, which is the known gap SPEC records — so this buys
nothing today. It is sent anyway because the clone that follows sends it, and a git
server that starts caring who is asking should not be a two-line surprise.

**A repo named after a route is refused**, with a warning. `/{repo}` is matched after
the literal top-level paths, so a repo called `brain`, `bugs`, `api`, `assets` or
`favicon.svg` would be shadowed on every page it has. `RESERVED_ROUTES` in `mirror.rs`
is the list, and it sits next to `discover` so the two move together. `NASHCODE_REPOS`
does not consult it: an operator naming one of those has asked for it on purpose.

**The doctor line is about the seed, not about discovery.** It runs before the first
cycle, so it can only report what `NASHCODE_REPOS` gave it; saying "discovery found
nothing" there would fire on every healthy discovery-only start.

**A filesystem `DGIT_URL` lists `*.git` directories** instead. That is what the tests
point at, and what a local setup with no dgit in front of it uses; `remote_url` already
treats it as a directory of bare repos, so discovery has to agree.

**A failed index read changes nothing** — one `warn!` and the set stands. Nothing removes
a name, ever: a repo that drops off dgit's index still has a mirror on disk and pages that
render from it, and a server that answers a truncated list must not be able to 404 a repo
that was working a minute ago.

The cycle is `Mirrors::watch`: one pass immediately, then one a minute. It replaces the
single warming `refresh_all` that `main.rs` used to spawn, so the poll interval is also
the longest a pushed repo can stay invisible.

**Out of scope, and wanted: `PUT /:repo/track`.** Discovery sees what dgit lists, which
is its public repos. A private repo needs the operator to say so by name, and there is no
door for that yet — `NASHCODE_REPOS` is the only way in. Not built here.

## Invariants (plans/invariants.md)

**A stuck CI run does not block merge.** The goal said "treat as `error`", but `error`
blocks, and the point was to stop wedging. A `running` row with no heartbeat for five
minutes carries the same information as a run that never happened, and `blocks_merge(None)`
is already false. Orphans found at open still become `error`: that is terminal and
clearable, not pending forever. One line in `status::blocks_merge` flips it if requeue
should be the only way out.

**Transcripts are keyed by `sha256`, not blake3.** `sha2` is already a direct dependency;
blake3 is only transitive. No new crate for a filename.

**The CI policy is read from the default branch before the scratch clone.** A branch that
is not allowed to run never gets written to disk. Every read failure — missing file, bad
UTF-8, bad TOML — is "off", so the gate fails closed.

**Two cards on one branch block the merge before the push**, next to the CI gate, so
nothing flips halfway. Cards are counted by `tasks/` directory, not `is_card()`: a plan
with `status:` would otherwise count as a card, and the directory is what the flip rewrites.

**Orphaned comments show on the default branch page.** Nothing links a comment row to the
branch it merged into; tracking that needs a second column. The default branch is where
everything merges and `branch_page` already knows `is_default`.

**Dangling refs over-report, never under-report.** `DocIndexCache` is keyed on
`repo@commit` and branch existence is not a function of the commit, so a branch created
without moving the tip stays "dangling" until the tip moves. The stale direction is the
safe one. `// ponytail: put the branch set in the cache key if it bites`.

**`add_columns` lives in `db.rs`.** Calling it from `bugs::index` made `db → bugs`, a
cycle the pre-commit hook caught. It is generic SQLite plumbing; `bugs` now imports it.
