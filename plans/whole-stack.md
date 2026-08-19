# The stack: one view from app code to host OS

nashcode is four codebases pretending to be one system: this repo, dgit
(littledivy/dgit — the git server), celld (denoland/celld — the runtime that hosts
it), and the Ubuntu box underneath. A bug in celld is a nashcode bug, but the viewer
sees one repo at a time, so neither Matthias nor an agent can trace a symptom down
the column. The goal: declare the stack once, and nashcode mirrors, browses, and
indexes the whole column — own code and upstream code — as one navigable thing.
Monorepo ergonomics without monorepo governance.

Four research agents surveyed the tooling, the academic literature, cross-repo code
intelligence, and whole-system provenance. Full reports:
`goals/whole-stack/research-*.md`. Six conclusions bind this design.

## What the research settled

1. **Compose the view, not the history.** Everyone who stitched git histories
   (subtree, git-meta, submodule veneers) regretted it. Everyone at scale
   (Sourcegraph, Microsoft Scalar, Gitea mirrors) keeps repos separate and unifies
   the index. Submodules' *data model* — path → URL → pinned commit — is right; the
   client workflow is the part that hurts. So: each dependency is its own read-only
   mirror repo, and the "monorepo" is a view over them.

2. **The manifest is a commit, not server config.** `west.yml`, `workspace.josh`,
   Android's `repo` XML all agree: the mapping lives versioned in the user's repo.
   Pin by exact rev for reproducibility, or track a branch for freshness — the user
   chooses per dependency.

3. **Cross-repo navigation is string equality on SCIP symbols.** A SCIP symbol
   carries `scheme manager package version descriptor`. If both sides are indexed at
   the pinned commit, "def in the other repo" is a table lookup, no resolver. One
   known trap: workspace/unpublished crates emit a `.` version placeholder —
   nashcode mints a synthetic version from remote URL + commit SHA instead.

4. **Signature-first, one hop deep.** The measured agent win (SWE-Explore, RepoGraph,
   ksrc) is reaching a precise signature in a direct dependency cheaply. Indexing
   cost scales with dependency depth; agent benefit does not. So: full index for
   declared deps, nothing transitive by default.

5. **The OS layer is a lookup, not a mirror.** `dpkg -l` + a snapshot.ubuntu.com
   timestamp pins the entire archive state — Ubuntu's equivalent of a nixpkgs
   commit. Source comes lazily per package from `apt source`, the snapshot `/mr/`
   API, or git.launchpad.net's `import/<version>` tags. Debsources proves the
   browse-the-distro product works. Never mirror the archive, never rebuild, never
   require Nix.

6. **Every link states its provenance.** About 1 in 5 package versions does not map
   cleanly to its claimed source (DepDive). Steal deps.dev's `relationProvenance`:
   each cross-layer link is labeled verified / metadata-only / unverified, shown, not
   hidden.

## What exists today

- Repo discovery is only the `NASHCODE_REPOS` env var (`viewer/src/config.rs:73`);
  mirrors are `git clone --mirror` per repo (`viewer/src/mirror.rs`), refreshed with
  a 10s debounce.
- The code index is SQLite keyed `(repo, blob, …)` (`viewer/src/db.rs:1362`) — a
  cross-repo query is schema-compatible; no code does it yet. tree-sitter for
  Rust/Python/TS, SCIP overlay via marker files at the repo root
  (`viewer/src/code/scip.rs:64`).
- Submodule tree entries render as inert labels (`viewer/src/web/pages.rs:364`); the
  indexer skips them (`viewer/src/code/mod.rs:367`).
- `/brain/ask` already scopes tools to a repo list (`viewer/src/brain.rs:316`) — the
  closest existing multi-repo surface.
- `/code/find` and `nashcode grep` are specced and claimed by the other agent but
  unimplemented. The stack work layers a scope on top of them; it must not collide.
- `SPEC.md:461` says "these are personal repos, not monorepos". That ceiling gets
  revised: brute force stays per-repo; stack queries fan out per repo and merge.

## Design

### The manifest: `.nashcode/stack.toml`

```toml
[[dep]]
name  = "dgit"
url   = "https://github.com/littledivy/dgit"
pin   = "1a2b3c4"        # exact rev; or track = "main"
layer = "server"

[[dep]]
name  = "celld"
url   = "https://github.com/denoland/celld"
track = "main"
layer = "runtime"

[system]                  # phase 4
image    = "ubuntu:24.04"
snapshot = "20260819T000000Z"   # snapshot.ubuntu.com timestamp
```

On refresh of a repo, the viewer reads `stack.toml` at the default-branch tip and
registers the mirrors it names. No new env vars.

### Mirrors of upstreams

Reuse `mirror.rs`. Upstream mirrors live in the same `$NASHCODE_MIRRORS`, namespaced
`up/<name>.git` so they never collide with own repos. They are read-only everywhere: no push, no CI, no plans, no board. `pin` deps
fetch once and never re-resolve; `track` deps refresh on a per-repo interval
(default 30 min) plus sync-now, with a rate-limit budget. Big trees (a kernel,
later) use `--filter=blob:none` partial clone.

### Composite browse

`GET /:repo/stack` renders the column: own repo, each dep at its pin, the system
layer — each entry opening the existing code browser at that exact commit. One page,
N trees; no merged fake tree. Submodule gitlinks in any tree become links when the
`.gitmodules` URL matches a mirror.

### Stack-scoped intelligence

Index each mirror at its pinned commit with the existing indexer — the schema is
ready. Then one new query dimension: `scope=stack` on `/code/def`, `/refs`,
`/callers`, `/text`, `/similar`, and `/code/find` when it lands; `nashcode grep
--stack` in the CLI. Cross-repo def/refs resolve by SCIP symbol equality.
`/brain/ask` tool scope widens to the stack.

### Upstream watch

The payoff feature. For each `track` dep: diff `pin..upstream-head`, extract changed
symbols, intersect with this repo's references into that dep. The brain then says
"celld moved 47 commits; 3 touch symbols you call: `spawn_worker`, …" — upstream
bug impact, mechanically, before it bites.

### The system layer

`nashcode snapshot` (CLI, run on the box or against an OCI image via syft) records
the dpkg inventory + snapshot timestamp as a pinned `system` entry. The viewer
resolves any package to browsable source lazily — snapshot `/mr/` API, Launchpad
`import/<version>` tag — cached content-addressed so `ubuntu:24.04` is stored once
across every stack. Kernel source comes from the Launchpad kernel git, not source
packages. Anything installed by `curl | sh` is honestly labeled unpinned.

## Phases

Each phase lands SPEC first, in its own commit, then the implementation.

**1 — Declare and mirror.** Parse `stack.toml`; create/refresh `up/` mirrors; brain
grows a `stack` stanza (deps, pins, staleness, provenance labels). Accept: nashcode
declares dgit + celld; `GET /brain?repo=nashcode` shows both, fresh.

**2 — Browse the column.** The `/stack` page; mirrors browsable at pin; submodule
gitlinks link through. Accept: from nashcode's stack page, open a celld file at the
pinned commit in two clicks.

**3 — Index the column.** Mirrors indexed at pin; `scope=stack` on the code
endpoints; `nashcode grep --stack`. Accept: from the nashcode working tree,
`nashcode grep --stack <celld symbol>` returns its definition with reference
counts; `/code/def?scope=stack` resolves a cross-repo symbol.

**4 — Upstream watch + system layer.** Drift report with impacted call sites;
`nashcode snapshot`; lazy package-source browse. Accept: brain reports celld drift
naming at least one impacted nashcode call site; one dpkg package opens as source
in the viewer.

## Non-goals

- No merged git history, no FUSE/virtual filesystem, no submodule client workflow.
- No write-back to upstreams — mirrors are read-only; upstream is authoritative.
- No transitive dependency indexing by default; no Software Heritage mirror; no
  rebuilds; no Nix/Bazel requirement.

## Open questions — annotate

1. Namespace: `up/<name>` (short, per-stack) vs full host path
   (`github.com/littledivy/dgit`, global dedup across repos' stacks). Sourcegraph's
   one-host-per-instance regret says decide early.
2. Does `stack.toml` also drive local checkout (`nashcode stack pull` materializing
   deps as sibling worktrees), or is this viewer-only for now?
3. Cargo.lock auto-discovery: offer `[[dep]]` suggestions from lockfiles, or stay
   fully declarative?
4. Is the system layer phase 4 here, or its own plan once 1–3 are dogfooded?
