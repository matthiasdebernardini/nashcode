# Research: monorepo and multi-repo tooling prior art

How the industry composes many git repos into one navigable whole. Conclusion up
front: the 2024–2026 convergence is partial clone + sparse checkout (not filesystem
virtualization), and everyone doing cross-repo *browsing* keeps repos separate and
unifies the *index*, not the tree.

## History composition

1. **Git submodules.** Gitlink (commit SHA) in the parent tree + `.gitmodules` URL
   map. Failure catalogue: empty dirs after plain clone, branch-switch corruption,
   per-submodule push/permission duplication, hardcoded URLs breaking for forks.
   Counter-signal: practitioners note the alternatives are historically worse for
   strict pinning of third-party code. Steal the data model (path → URL → pinned
   commit); never expose the client workflow.

2. **git subtree.** Copies the subproject in. Merge nightmares on divergence, repo
   bloat, filter-branch-class slowness. Skip.

3. **git-subrepo.** Vendored copy + a provenance metadata file. Steal the metadata
   idea; skip the tool (niche, slow maintenance).

4. **josh (Just One Single History).** Git-aware HTTP proxy applying reversible,
   incrementally cached filters to history: `:/sub`, `:prefix=`, `:exclude`,
   composition `:[:f1,:f2]` (first filter to claim a path wins), and
   `:workspace=dir` — the composition declared in a *versioned file in the repo*.
   Failures: filters are order-sensitive (silent empty trees), `:hook` documented
   as possibly incorrect, and josh composes subdirs of one repo — it does not
   stitch N unrelated remotes. Steal hard: versioned workspace file as the mapping.

5. **Git X-Modules (commercial).** Server-side sync of a plain directory with an
   external repo; users see an ordinary directory, zero client tooling; conflicting
   concurrent updates become a pull request. Steal the contract: "it's just a
   directory, the server does the work."

6. **git-meta (Two Sigma).** Client-side monorepo veneer over submodules. Stalled
   ~2021, invisible conflicts, perf bottlenecks. Cautionary tale: a client veneer
   over submodules does not survive.

7. **Google `repo` / Zephyr `west` / ROS `vcstool` / git-ws.** All the same shape:
   a manifest (XML/YAML/TOML) of url + path + pinned rev; `sync` materializes.
   Failures are all client-side sync semantics (`repo sync --force-sync` destroying
   work, unpredictable disk usage, non-atomic syncs). Steal the manifest format;
   let the server do the fetching.

8. **Copybara.** Scripted one-way transformation with one designated source of
   truth. Steal the invariant (one repo authoritative); skip the tool.

## Virtual monorepos at scale

9. **Microsoft VFS for Git → Scalar.** Microsoft built filesystem virtualization,
   then explicitly retired it for partial clone + cone-mode sparse checkout. GitLab
   marks partial clone "done", VFS "rejected". Do not build a FUSE layer.

10. **Partial clone / promisor remotes (mainline git).** `--filter=blob:none`
    omits blobs; missing objects fault in from promisor remotes, multiple promisors
    now supported in config order. `git backfill` (2.49) batches missing-blob
    fetches; path-walk packing (2.51, filter-compatible in 2.55). Steal all of it:
    mirror big dependencies blobless, backfill lazily on browse.

11. **Meta Sapling/EdenFS/Mononoke.** "Cost proportional to files touched" is the
    right invariant, but EdenFS/Mononoke are explicitly unsupported outside Meta.
    Skip as a dependency.

## Read-only mirroring (closest match)

12. **Sourcegraph architecture.** Repos stay separate; the code host is the source
    of truth and the mirror is an explicitly eventually-consistent cache.
    Fingerprint-gated re-index, default-branch-only trigram indexing, unindexed
    fallback searcher. Steal the freshness contract.

13. **Sourcegraph package repositories — the most on-point prior art.** Read-only
    repos synthesized from package ecosystems (Go proxies, npm, Maven, crates),
    auto-created when referenced by SCIP uploads; cross-repo go-to-def works
    because SCIP symbols carry package + version. Documented regret: only one
    npm/JVM host per instance — namespace properly from day one. Steal the whole
    design: dependency mirrored as its own read-only repo, linked at the symbol
    layer, not the git layer.

14. **Sourcebot (YC F2025).** Self-hosted Zoekt stack indexing many hosts into one
    corpus, MCP server as the agent surface. Closest commercial neighbor; its
    FSL/ee licensing split shows where the money is.

15. **Gitea/Forgejo pull mirrors.** Scheduled background sync, default 8h,
    per-repo interval + manual sync-now, rate limit as a first-class budget. Steal
    the scheduling shape.

## Adjacent

16. **Bazel vendor mode.** `pin()` / `ignore()` per external dep; registry is a
    cheap static index separate from content. Steal pin/ignore and the
    index-vs-content split.

17. **Go module proxy / Athens.** Immutable versioned snapshots, local cache with
    upstream fallback; never re-resolve a pinned ref. Same problem shape, same fix:
    the mirror is the durable copy when upstream vanishes.

## Agent context (2024–2026)

18. **Externalized dependency graph pattern.** Convergent architectures: read-only
    explorer agents roam all repos, writers confined to one; a checked-in
    coordination graph of cross-repo edges queried as a tool. The durable
    diagnosis: the graph that decides whether a change is safe lives outside every
    single repo's boundary, so it must be served, not rediscovered. Maps directly
    onto extending `/brain` and `/code/graph` across mirrors.

19. **Counter-signal on writes.** Post-AI-adoption incident rates argue for
    read-only mirrors as the default: agents get full-stack visibility, zero
    ability to mutate upstream.

## Sources

- https://blog.timhutt.co.uk/against-submodules/
- https://lobste.rs/s/neab1g/never_use_git_submodules
- https://josh-project.dev/docs/reference/filters.html
- https://new.gitmodules.com/submodules.html
- https://github.com/twosigma/git-meta
- https://docs.zephyrproject.org/latest/develop/west/index.html
- https://source.android.com/docs/setup/reference/repo
- https://devblogs.microsoft.com/devops/introducing-scalar/
- https://git-scm.com/docs/partial-clone
- https://github.blog/open-source/git/highlights-from-git-2-51/
- https://sapling-scm.com/docs/scale/overview/
- https://sourcegraph.com/docs/admin/architecture
- https://docs.sourcegraph.com/admin/external_service/package-repos
- https://github.com/sourcebot-dev/sourcebot
- https://forgejo.org/docs/latest/user/repo-mirror/
- https://bazel.build/versions/8.2.0/external/vendor
- https://riftmap.dev/blog/ai-coding-agents-need-cross-repo-context/ (vendor blog; numbers directional only)
