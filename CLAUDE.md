# nashgit

**Another coding agent may be working in this repo right now, on `main`, at the same time
as you.** Read `COORDINATION.md` before you edit anything, and claim your files there.

The short version:

- `git pull --rebase` before you start and again before you commit. Keep commits small.
- Never rewrite the other agent's tests. A failing test of theirs is a real finding.
- New scope goes into `SPEC.md` first, in its own commit, before the implementation.
- Run the whole suite before committing: `cargo nextest run` (never `cargo test`). It
  takes about 20 seconds and both work streams touch shared modules.
- Leave notes for the other agent at the bottom of `COORDINATION.md`.

`SPEC.md` is the contract. `NOTES.md` records where the implementation had to choose.
`AGENTS.md` documents nashgit for agents that *use* it, which is a different thing.
