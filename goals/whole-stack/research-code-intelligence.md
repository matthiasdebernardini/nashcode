# Research: cross-repo, cross-language code intelligence

How to click from a call in repo A to its definition in dependency repo B at the
exact pinned version. Conclusion up front: SCIP symbol strings are the cross-repo
primary key; the win for agents is signature-level and one hop deep.

## SCIP / LSIF

1. **Cross-repo resolution is string equality, not a graph join.** SCIP symbol
   grammar: `<scheme> <manager> <package-name> <version> <descriptor>+`.
   Descriptors are sigil-typed (`ns/`, `Type#`, `method().`, …); `SymbolRole` is a
   bitset (Definition, Import, ForwardDefinition — the last is the C/C++ header
   hop). Index dependency B, store its symbol strings, and A's references match by
   exact string including version. The only infrastructure needed is a
   symbol → (repo, commit, path, range) table.

2. **The `.` placeholder trap.** Unpublished/workspace crates emit
   `rust-analyzer cargo mycrate . foo/` — version is a dot, so inbound cross-repo
   navigation dies for ordinary (unpublished) repos. Fix: mint synthetic package
   identity from git remote + commit SHA at index time.

3. **Costs.** Both sides must be indexed at the exact commit. scip-clang needs
   `compile_commands.json` and a working build; LLVM's index is 375 MB raw.
   Sourcegraph.com has precise nav on ~45k of 2.8M repos — precise indexing is the
   exception, so text/tree-sitter fallback must stay first-class.

4. **Self-hosting is fine.** SCIP is vendor-neutral (open governance since March
   2026), indexers are standalone binaries (rust-analyzer `--output-format scip`,
   scip-typescript, scip-python, scip-clang, even debian-lsp). Skip LSIF (4–5×
   larger, superseded). Meta's Glean is an existing open-source SCIP consumer if a
   fact store is ever wanted; nashcode's SQLite tables are enough for now.

## Kythe / stack-graphs

5. **Kythe**: compiler-integrated, exact, not incremental, wants a Bazel-shaped
   monoculture. Skip.
6. **stack-graphs**: tree-sitter-based, no build required, incremental — and
   archived by GitHub 2025-09-09 with no successor; the per-language DSL cost never
   amortised (only Python and TypeScript shipped). Steal the idea (no-build
   incremental extraction — nashcode's tree-sitter pass already is this); skip the
   codebase.

## Package → source resolution

7. **deps.dev v3**: purl → source repo + commit, with an explicit provenance enum
   (`SLSA_ATTESTATION`, `GO_ORIGIN`, …, `UNVERIFIED_METADATA`). Steal the enum
   verbatim — every link carries how it was derived.
8. **crates.io**: index has repository URL but no commit; published tarball ≠ repo
   tree (normalized Cargo.toml). Recovering the commit needs blob-hash matching.
   crates.io itself grew a browsable Code tab (July 2026) — validation of demand.
9. **Software Heritage**: intrinsic SWHIDs; prefer `swh:1:dir` (recomputable from
   the tree). Fallback tier for vanished upstreams only.
10. **Debian/Ubuntu**: binary → source via dpkg `Source:` fields; no clean REST
    API; epochs and binNMUs break name==version assumptions. See the
    whole-system report for the working chain.

## Cross-language / FFI

11. **Searchfox (Mozilla)** is the only production system linking across language
    boundaries well, and its lesson: don't infer across the boundary — link through
    the *generated binding*, which already knows both sides. For Rust:
    bindgen/cbindgen/cxx/PyO3 output is the linkage table; record
    (extern symbol ↔ header decl) at generation time. Research systems (PolyCruise
    USENIX Sec'22, PyXray ICSE'26) confirm general inference is hard; skip it.

## What agents measurably need

12. **SWE-Explore** (848 issues, 203 repos): file-level localization is solved;
    line-level precision and context-efficient ranking are the differentiators.
13. **Graph indexes beat text retrieval** for localization (RepoGraph +32.8%
    relative on SWE-bench; CoSIL, LocAgent) — all intra-repo so far; the cross-repo
    hop is unclaimed territory.
14. **The cross-repo pain is a token cost**: ~15 steps / ~16k tokens for an agent
    to see one third-party signature (ksrc measurement); greppable vendored source
    (node_modules, ~/.cargo/registry) is why TS/Python agents don't feel it.
15. **Signature-level beats body-level.** A precise type signature usually answers
    the agent's question. Context7's traffic (top MCP server by usage) validates
    "docs/signatures for the exact pinned version" as the product.

**Verdict:** index declared deps fully at the pin (they're few), resolve
cross-repo by SCIP string equality, serve signatures first, one hop deep. Skip
transitive body-level indexing; for OS packages, link to source rather than index.

## Sources

- https://github.com/scip-code/scip (scip.proto: symbol grammar, SymbolRole)
- https://sourcegraph.com/blog/cross-repository-code-navigation
- https://sourcegraph.com/blog/announcing-scip
- https://github.com/sourcegraph/scip-clang/blob/main/docs/IndexingProjects.md
- https://github.com/rust-lang/rust-analyzer/pull/13456 (the `.` placeholder)
- https://github.com/github/stack-graphs (archived 2025-09-09)
- https://engineering.fb.com/2024/12/19/developer-tools/glean-open-source-code-indexing/
- https://docs.deps.dev/api/v3alpha/
- https://blog.rust-lang.org/2026/07/13/crates-io-development-update/
- https://codeandbitters.com/published-crate-analysis/
- https://docs.softwareheritage.org/devel/swh-web/uri-scheme-api-swhids.html
- https://github.com/mozsearch/mozsearch/blob/master/docs/analysis.md
- https://arxiv.org/abs/2606.07297 (SWE-Explore)
- https://github.com/respawn-app/ksrc
- https://modem.dev/blog/how-coding-agents-read-your-code
