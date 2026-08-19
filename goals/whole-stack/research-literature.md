# Research: academic literature on multi-repo systems

What the research record says about systems spanning many repositories and stack
layers. Conclusion up front: cross-repo change coupling is real and measured, ~1 in
5 package versions doesn't map cleanly to source, and graph indexes measurably beat
text retrieval for agent localization.

## Monorepo vs multi-repo

1. **Potvin & Levenberg, CACM 2016** — Google's Piper: ~1B files, one snapshot =
   one consistent world, atomic cross-project change. The cost is tooling, not
   storage. Validates one-snapshot-of-the-whole-world as the primitive.
2. **Brito et al., 2018 (multivocal review)** — monorepo benefits are navigation
   and coordination; costs are governance and mandatory tooling. A read-only union
   view takes the first without paying the second.
3. **Arabat & Sayagh, EMSE 2024** — 49,357 cross-component dependent changes
   across OpenStack's 1,310 projects, declared *manually* via Gerrit `Depends-On`.
   Cross-repo edges are frequent enough that people hand-maintain them; a
   whole-stack index recovers them mechanically.
4. **Blincoe et al., IST 2019** — manifest edges under-count real coupling;
   symbol-level reference edges are needed.

## Upstream → downstream impact

5. **Venturini et al., TOSEM 2023; Ochoa et al., EMSE 2021; Raemaekers (Maven)** —
   breaking changes routinely ship in minor/patch releases; ~a third of Maven
   releases are binary-incompatible; half of client-impacting breaks violate
   semver. Version pins don't predict breakage — the downstream *call sites* do.
   This is the evidence for upstream watch (diff the pin, intersect with my refs).
6. **He et al., TSE 2023; Rebatchi et al., EMSE 2024 (Dependabot)** — automation
   already consumes dependency graphs at scale; vulnerabilities stay hidden ~512
   days. The unanswered question is "why does this update touch *me*" — a
   cross-boundary reference query.

## SBOM / provenance ↔ source

7. **OmniBOR (arXiv:2402.08980)** — artifact IDs are git object IDs; build-step
   manifests yield a binary → object → source-file graph. The ground-truth
   mechanism for "which source line is in this shipped binary".
8. **SWHID (ISO/IEC DIS 18670)** — intrinsic content-addressed IDs; the stable
   cross-layer key that survives upstream force-push or deletion.
9. **SBOM quality is measured-bad** — ~1% of SPDX SBOMs met NTIA minimum fields;
   generators systematically miss dependencies. Treat ingested SBOMs as hints;
   verify against mirrored source.
10. **Phantom artifacts** — DepDive: **20.1% of package updates contain files not
    traceable to the source repo**; Maven source locatable for only 80.4%. The
    viewer needs an explicit unverified/phantom state, not a broken link.
11. **Malka et al., MSR 2025** — 709,816 Nixpkgs rebuilds: bitwise reproducibility
    69%→91% (2017–2023). Input-addressed builds give a whole-system BOM including
    the OS layer; Google OSS Rebuild does it registry-scale without publisher
    action.

## Code search & navigation

12. **stack graphs (arXiv:2211.01224, archived 2025)** — file-incremental name
    resolution worked; per-language grammar maintenance didn't. Expect tree-sitter
    extraction + SCIP-style global symbol names, not full semantic resolution
    everywhere.
13. **Sadowski et al., FSE 2015** — Google devs: ~5.3 code searches/day, >26% of
    queries path-scoped, mostly in familiar code. Scoping UI beats raw recall;
    agents behave the same.
14. **SWE-Explore (arXiv:2606.07297)** — agents hit the right file (0.64–0.68)
    but the right lines rarely (0.14–0.19); CoSIL's iterative graph search is the
    top non-oracle method; context efficiency correlates with resolve rate at
    r=0.95. Strongest evidence here: serve a *graph*, bias toward recall across
    layer boundaries.

## Version selection

15. **Cox, MVS (2018); "Package Managers à la Carte" (2026)** — general version
    resolution is NP-complete; minimum-version selection is linear-time and
    reproducible without a lockfile. A whole-stack pin format with minimum-only
    constraints stays deterministic and diff-friendly; anything richer needs a
    solver. nashcode's stack manifest pins exact revs — even simpler, keep it that
    way.

## Near-exact precedents

16. **Debsources (ESEM 2014)** — the whole Debian archive browsable + symbol
    search; the OS-layer product, already proven.
17. **Elixir (Bootlin)** — indexes every kernel release by indexing *blobs, not
    trees*, so unchanged files across versions are indexed once. Directly reusable
    for mirroring many versions of one dependency (nashcode's index is already
    blob-keyed).
18. **repo / west / gclient** — manifest pins N repos into one tree, but none has
    a cross-repo symbol layer. That gap is the product.

## Market signal

19. **Sourcegraph** pivoted away from self-hosted code search (~$49–99/user/mo,
    focus now on Amp); the OSS field is Zoekt and OpenGrok, both text-level.
    Nobody sells a self-hosted whole-stack (app + deps + image + OS) navigable
    index.
20. **Provenance is going default-on** (npm Sigstore GA, PyPI PEP 740, SLSA v1.2
    Source track) but measured adoption is near-zero; build for its absence.

## Sources

- https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/
- https://arxiv.org/abs/1810.09477
- https://link.springer.com/article/10.1007/s10664-024-10488-y
- https://dl.acm.org/doi/10.1145/3576037
- https://link.springer.com/article/10.1007/s10664-021-10052-y
- https://dl.acm.org/doi/10.1109/TSE.2023.3278129
- https://arxiv.org/pdf/2402.08980
- https://docs.softwareheritage.org/devel/swh-model/persistent-identifiers.html
- https://arxiv.org/pdf/2206.09422 (DepDive)
- https://arxiv.org/html/2501.15919v1
- https://security.googleblog.com/2025/07/introducing-oss-rebuild.html
- https://arxiv.org/pdf/2211.01224
- https://research.google.com/pubs/archive/43835.pdf
- https://arxiv.org/html/2606.07297v1 (SWE-Explore)
- https://research.swtch.com/vgo-mvs
- https://upsilon.cc/~zack/research/publications/debsources-ese-2016.pdf
- https://bootlin.com/blog/elixir/
