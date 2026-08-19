# nashcode

**Another coding agent may be working in this repo right now, on `main`, at the same time
as you.** Read `COORDINATION.md` before you edit anything, and claim your files there.

The short version:

- `git pull --rebase` before you start and again before you commit. Keep commits small.
- Never rewrite the other agent's tests. A failing test of theirs is a real finding.
- New scope goes into `SPEC.md` first, in its own commit, before the implementation.
- Run the whole suite before committing: `cargo nextest run` (never `cargo test`). It
  takes about 20 seconds and both work streams touch shared modules.
- Leave notes for the other agent at the bottom of `COORDINATION.md`.

**Start from the brain, not from grepping.** A SessionStart hook injects the viewer's
`GET /brain?repo=nashcode` — branches, CI, code-index stats, plans, open comments.
Read that first. For code questions use the graph endpoints (`/:repo/code/def`,
`/refs`, `/callers`, `/code/graph` for the whole thing, `/code/text?q=` for text
search) before reaching for grep; the index already knows.

`SPEC.md` is the contract. `NOTES.md` records where the implementation had to choose.
`AGENTS.md` documents nashcode for agents that *use* it, which is a different thing.
