# Coordination

Two coding agents are working in this repo at the same time, on `main`, with no branch
isolation. This file is how we stay out of each other's way. Read it before you start,
edit it when you claim something.

Not to be confused with `AGENTS.md`, which documents nashgit *for* agents that use it.
This file is for agents that are *building* it.

## Conventions

1. **Claim before you edit.** Add a row to the table below naming the files you are about
   to touch. Remove the row when you land it.
2. **Pull first, commit small.** `git pull --rebase` before you start a change and again
   before you commit. Small commits rebase cleanly; a 20-file commit does not.
3. **Never rewrite the other agent's tests.** If a test of theirs fails because of your
   change, that is a real finding — fix the code, or say so in your commit message and
   leave the test alone.
4. **`SPEC.md` is the contract.** New scope goes into `SPEC.md` first, in its own commit,
   before the implementation. That way the other agent sees the intent, not just a diff.
5. **Run the whole suite before committing**, not just your file's tests. The suite is
   fast (about 20 seconds) and the two work streams touch shared modules.

## Current state

- `cargo nextest run` — 98 tests, all passing.
- Fresh clone plus `cargo build` produces a runnable server: `build.rs` runs `npm ci` and
  esbuild, and the binary embeds both bundles. Verified from a clean clone with no
  `node_modules`.
- Binds loopback only. No hostname, bucket, account, or tailnet name in tracked source.

## Claims

| Area | Agent | Status |
|---|---|---|
| — | — | nothing currently claimed |

## Who has been doing what

Roughly, so far. Not a fence, just context.

- **Implementation against `SPEC.md`**: mirror and stack layers, plans/board/links, brain,
  traces and the prompts page, the git-safety hardening, README and AGENTS.
- **UAT**: `UAT-STORIES.md`, `uat/`, and the gap-closing fixes that came out of running
  those stories, including the stored-XSS fix in markdown rendering.

## Open work, unclaimed

These came from an external review against Cursor's "Git at any scale". None lose data;
they are performance and self-healing. Take one by claiming it above.

1. **Re-clone on mirror corruption.** A corrupt mirror currently stays corrupt and every
   page for that repo degrades forever. Detect the failure and re-clone once.
2. **Cache stack inference.** `StackGraph::infer` is O(branches²) subprocess calls on
   every page load. Cache it per set of tips, invalidated the way the doc index already
   is.
3. **`git cat-file --batch` in the doc scan**, instead of one `git show` per file.
4. **One diff pass instead of per-file.** The branch page runs `git diff` once per changed
   file; a single call split client-side would do.
5. **A background `git maintenance` task**, so repacking never lands on a request.

## Notes for each other

Leave short messages here. Delete them once they are read and acted on.

- Nothing pending.
