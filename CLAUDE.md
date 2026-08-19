# nashcode

**Another coding agent may be working in this repo right now, on `main`, at the same time
as you.** Read `COORDINATION.md` before you edit anything, and claim your files there.

The short version:

- `git pull --rebase` before you start and again before you commit. Keep commits small.
- Never rewrite the other agent's tests. A failing test of theirs is a real finding.
- New scope goes into `SPEC.md` first, in its own commit, before the implementation.
- Run the whole suite before committing: `cargo nextest run` (never `cargo test`). Give
  it an isolated `CARGO_TARGET_DIR`/`CARGO_BUILD_BUILD_DIR` and expect ~20 minutes when
  other agents are building — do NOT kill a slow run; the shared build dir serializes
  everyone. Both work streams touch shared modules.
- Leave notes for the other agent at the bottom of `COORDINATION.md`.

**Start from the brain, not from grepping.** A SessionStart hook injects the viewer's
`GET /brain?repo=nashcode` — branches, CI, code-index stats, plans, open comments.
Read that first.

**Search with `nashcode grep`, not grep/rg.** rg's flags (`-i -l -C -t -g`; anything
else is passed straight through to rg), grep's `path:line:content` output, grep's exit
codes — 0 hits, 1 none, 2 only for a usage mistake or for having neither a checkout nor
an index. Definitions come first, each carrying its kind and reference/caller counts
after a trailing ` # `; text and semantic lines stay pure. Text hits come from a live rg
pass over the working tree, so your uncommitted edits are never invisible, and `-i`,
`-t`, `-g` and a path argument narrow the index side too. Structure questions still go
to `/:repo/code/def`, `/refs`, `/callers`, `/code/graph`.

`SPEC.md` is the contract. `NOTES.md` records where the implementation had to choose.
`AGENTS.md` documents nashcode for agents that *use* it, which is a different thing.
