# nashcode

Stacked-branch review, CI, and a kanban board for a [dgit](https://github.com/littledivy/dgit)
server on a private tailnet.

dgit already serves git over smart HTTP and gives you a cgit-style browser. nashcode adds
what it lacks: real diffs, stacked-branch review, a CI runner, comments, and a board. It
has no accounts and no login. The tailnet is the perimeter.

It never parses git internals. Every question is answered by running the real `git` binary
against `git clone --mirror` copies on local disk.

## What you get

- `/` — every repo with its stacks
- `/:repo` — branches, their stack parent, commits ahead, CI status
- `/:repo/stacks` — the stack graph and the merge/restack audit log
- `/:repo/:branch` — the review page: commits unique to the branch, then per-file diffs
  rendered by [`@pierre/diffs`](https://diffs.com), with inline comments, a merge button,
  and a restack button
- `/:repo/plans` — markdown under `plans/`, rendered and commentable
- `/:repo/board` — markdown under `tasks/` as a drag-and-drop kanban board
- `/:repo/ci` — recent CI runs and their logs
- `/:repo/agent` — agent sessions, each linked to the commits it produced, over a search
  across every prompt you have written
- `/brain` — the whole tailnet's work state as JSON

## Requirements

- Rust (edition 2024) and `cargo`
- `node` and `npm` — the build bundles the browser assets
- `git` on `PATH` at runtime, not just at build time

## Run it

```sh
git clone <this repo> && cd nashcode
DGIT_URL=https://git.your-tailnet.example \
GIT_TOKEN=your-dgit-token \
cargo run
```

There is no repo list to write. The viewer reads the git server's index page every
minute and mirrors what it finds, so a `git push` to a new name shows up within a minute.

`cargo build` on a fresh clone produces a runnable server. The build script runs `npm ci`
and esbuild, then embeds both bundles in the binary, so there is no separate asset step and
nothing to copy at deploy time.

To try it without a dgit server, point `DGIT_URL` at a directory of bare repos:

```sh
DGIT_URL=/srv/git cargo run
```

That lists the `*.git` directories in `/srv/git` and mirrors each one.

## Configuration

Environment only. Nothing about your deployment lives in the source.

| Variable | Default | Meaning |
|---|---|---|
| `DGIT_URL` | *(none)* | Base URL of the dgit server. Repo `x` is `$DGIT_URL/x.git`. A filesystem path works too. |
| `GIT_TOKEN` | empty | Push token, sent as basic auth `x:$GIT_TOKEN`. Reads are anonymous. Empty means pushes are anonymous. |
| `NASHCODE_REPOS` | empty | Comma-separated repo names to start with. Optional: the viewer discovers the rest from the git server. A name here is never dropped. |
| `NASHCODE_MIRRORS` | `~/mirrors` | Where the mirror clones live. |
| `NASHCODE_BIND` | `127.0.0.1:8090` | Listen address. Keep it on loopback. |
| `NASHCODE_DB` | `$NASHCODE_MIRRORS/nashcode.db` | SQLite file: comments, CI runs, audit trail. |
| `NASHCODE_CI_LOGS` | `$NASHCODE_MIRRORS/ci-logs` | CI log files. |
| `NASHCODE_WEBHOOKS` | *(none)* | Path to a JSON file mapping events to URLs. |
| `NASHCODE_TRACES` | `$NASHCODE_MIRRORS/traces` | Raw agent transcripts. |
| `ANTHROPIC_API_KEY` | *(none)* | Enables `POST /brain/ask`. Without it that route answers 404. |
| `NASHCODE_BRAIN_MODEL` | `claude-opus-5` | Model for `/brain/ask`. |
| `NASHCODE_URL` | `http://$NASHCODE_BIND` | Where the viewer is, from outside. Every absolute link nashcode puts in a notification hangs off it. |
| `NASHCODE_PUSHOVER_TOKEN` | *(none)* | Pushover application token. Set both this and the user key, or neither. |
| `NASHCODE_PUSHOVER_USER` | *(none)* | Pushover user or group key. Without both halves nothing is sent and everything else works. |
| `NASHCODE_PUSHOVER_URL` | `https://api.pushover.net` | API origin. Overridable so tests can point at a local listener. |
| `NASHCODE_BUGS_SELF_DSN` | *(none)* | The DSN nashcode reports its own errors to. Unset means it reports nothing about itself. |

The client commands (`hook`, `trace`, `doctor`) read `NASHCODE_URL` too, plus one more:

| Variable | Default | Meaning |
|---|---|---|
| `NASHCODE_REPO` | *(inferred)* | Repo name, when the git remote's basename is not it. |

At startup the server prints a line for each thing that is unset and what you lose by it.

## Exposing it

nashcode binds loopback and has no auth of its own. Put `tailscale serve` in front:

```sh
tailscale serve --bg --https 8443 http://127.0.0.1:8090
```

Tailscale injects `Tailscale-User-Login` and `Tailscale-User-Name` on every proxied
request. nashcode trusts those headers and stamps them on comments, merges, and reruns. A
request without them shows as `local`.

Do not expose the port any other way. The headers are trusted, so anything that can reach
the port can claim any identity.

## CI

Put an executable `.nashcode/ci` in a repo, and opt the repo in from its **default branch**
with `.nashcode/ci.toml`:

```toml
enabled = true      # without this, nothing runs
git_token = true    # optional: put GIT_TOKEN in the job's environment
```

When nashcode sees a branch tip it has not seen before, it reads that file from the default
branch tip. `enabled = true` there, and only there, lets the job check the commit out into a
scratch directory and run the script. Anything else — no file, `enabled = false`, a file
that will not parse, a copy added on the pushed branch — records the run as `skipped` with
"ci not enabled on default branch". A skipped run never blocks a merge. No script means no
job either.

Jobs run one at a time. The script gets `NASHCODE_REPO`, `NASHCODE_BRANCH`, and
`NASHCODE_COMMIT`, plus `GIT_TOKEN` when the default branch asked for it, and nothing else.
Non-zero exit is red. Timeout is 30 minutes. Combined output goes to a log file and the
status lands next to the branch everywhere it appears.

There is no separate deploy system. If the script wants to deploy, it deploys.

### Security model — read this before granting push access

**Push access to a repo whose default branch enables CI is code execution on the nashcode
host.** `.nashcode/ci` runs as the server's own user and there is no sandbox: no container,
no seccomp, no resource limits beyond the 30-minute timeout. Once `enabled = true` is on the
default branch, anyone who can push *any* branch can run anything the nashcode user can run,
because the script comes from the pushed commit. `git_token = true` widens that to "and can
push anywhere the token can push", which is why it is a second, separate switch.

The opt-in lives on the default branch so that turning CI on is a merge somebody reviewed,
not a push. Until then a pushed branch runs nothing. On a personal tailnet where every
pusher is you or your agents, enabling it is the point — the CI script deploying *is* the
deploy system. Do not enable it on repos that people you would not hand a shell to can push
to.

Two sharp edges of the timeout: the kill reaches the script process itself, not
grandchildren it spawned into their own process groups — a detached child can outlive
the job — and a timed-out job keeps whatever output it printed, marked as partial.

## Plans and boards

Two conventions, both file-native. Nothing about a plan, a card, or a link is stored in the
database. Git is the store, so an agent that edits a file and pushes has changed the board.

Markdown under `plans/` is a plan. Markdown under `tasks/` is a card, with front matter:

```markdown
---
status: doing
title: Ship the diff endpoint
assignee: ada
branch: feat/diffs
plan: plans/api.md
---

Body becomes the card detail.
```

`status` is the column. `todo`, `doing`, and `done` come first, in that order; any other
value becomes a column after them. A card whose front matter will not parse lands in a
"needs attention" column instead of breaking the board.

`branch:` and `plan:` wire the card to a branch and a plan, and nashcode computes the
back-links: the branch page shows its card and plan, the plan shows its cards. A ref
pointing at something that does not exist renders as plain text with a "missing" marker.

Dragging a card rewrites only the `status:` line, commits as the Tailscale user, and pushes.
The rest of the file is untouched, byte for byte.

## Comments

Line-anchored or branch-level, stored in SQLite, rendered inline on diffs through the
`@pierre/diffs` annotation slots. Markdown, no editing, no reactions, delete your own only.

Anchors are not re-anchored when a branch moves. A comment whose file changed since it was
written moves to an "outdated" section.

Comments work on any file at a commit, not only on files in a diff, which is what makes a
plan commentable.

```sh
# post
curl -X POST http://nashcode.example/alpha/comments \
  -H 'content-type: application/json' \
  -d '{"branch":"main","file":"plans/api.md","line":12,"body":"needs a timeout"}'

# read, oldest first
curl 'http://nashcode.example/alpha/comments?file=plans/api.md'
```

`file` and `line` are optional. Omit both for a branch-level comment. `author` is optional
and falls back to the Tailscale header. See [AGENTS.md](AGENTS.md) for the polling loop.

## Merge and restack

**Merge** takes a branch into its stack parent in a scratch worktree, fast-forwarding when
the parent has not moved and making a `--no-ff` merge commit when it has, then pushes the
parent and offers to delete the branch. Red or running CI blocks it behind a confirm step.
If a card declares `branch: <the merged branch>`, the same push flips it to `done`.

**Restack** rebases every descendant onto the new parent tip, in order, and force-pushes
them. All rebases happen in a scratch clone first and every ref goes in one atomic push, so
a conflict aborts the whole thing with nothing pushed and reports the conflicting files.
Finish a conflicted restack in your terminal.

Every merge and restack records who, what, when, old tip, and new tip. That log is on the
repo's stacks page.

## Webhooks

Outgoing only. dgit emits none, so nashcode's poller is the event source. Point
`NASHCODE_WEBHOOKS` at a JSON file:

```json
{
  "push": "https://hooks.example/push",
  "ci_finished": ["https://hooks.example/ci", "https://other.example/ci"],
  "merged": "https://hooks.example/merged",
  "restacked": "https://hooks.example/restacked"
}
```

POST JSON, 10-second timeout, one retry. Failures are logged, not queued.

## Traces

A commit answers "what changed". The trace that produced it answers "why, and what was
tried first". nashcode stores agent sessions — prompts, tool calls, results — and links
them to commits automatically: every recorded event carries the repo's `HEAD` at that
moment, so when `HEAD` moves between two events, the commits in between belong to that
session. No commit trailers, nothing for the agent to remember.

Transcripts live in SQLite and under `NASHCODE_TRACES`, not in git. They are large and
append-heavy; committing them would bloat every clone. The link to git is the commit SHA.

`/:repo/agent` lists sessions, newest first, each titled by its first prompt and counted
by its events and commits. `/:repo/agent/:session` reads one run as the conversation it
was: prompts and replies as markdown, thinking folded away, each tool call with the one
argument worth reading, results folded except the errors, and every file edit rendered as
a diff by the same `@pierre/diffs` pipeline the branch page uses. The diff comes from the
transcript, not from git, so unpushed and since-rewritten changes still display. A
commit's row on the branch page links back to the conversation that wrote it, and
`GET /:repo/commits/:sha/trace` answers the same question as JSON.

The renderer reads two shapes natively: live hook events (`prompt`, `tool_name`,
`tool_input`) and raw Claude Code transcript lines (`type` plus `message.content` block
arrays). Anything else falls back to a one-line summary, never a blank row.

One warning: a transcript contains whatever the agent saw, secrets included. nashcode does
not redact. The tailnet is the perimeter here as everywhere else.

### Searching your prompts

The search box on `/:repo/agent` runs over every prompt written in that repo. `?q=`
filters by substring and `?session=` narrows to one run. The same URL answers JSON, so
your prompt library is greppable:

```sh
curl -s -H 'accept: application/json' "http://nashcode.example/alpha/agent?q=retry"
```

A prompt is any recorded event whose payload carries a `prompt` field, so this works for
any harness that reports one.

`/:repo/traces`, `/:repo/traces/:session`, and `/:repo/prompts` moved here: a browser gets
a 301 to the `/agent` equivalent. Their JSON keeps answering at the old paths, because
agents push to and poll them.

### Wiring an agent

The viewer binary is also the trace client. Put it on the agent's machine and let the harness
hooks feed it. For Claude Code, in `.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "nashcode-viewer hook" }] }],
    "PostToolUse":      [{ "hooks": [{ "type": "command", "command": "nashcode-viewer hook" }] }],
    "Stop":             [{ "hooks": [{ "type": "command", "command": "nashcode-viewer hook" }] }]
  }
}
```

`nashcode-viewer hook` reads one hook payload from stdin, records it, and exits 0 — always. A
dead server, garbage input, or a missing repo never fails the agent's turn. Set
`NASHCODE_DEBUG=1` to see why an event was dropped.

For a run that happened without the hook, backfill from the harness transcript:

```sh
nashcode-viewer trace push ~/.claude/projects/<project>/<session>.jsonl
nashcode-viewer trace list
nashcode-viewer trace show <session>
nashcode-viewer doctor   # what is configured, is the server reachable
```

## Brain

`GET /brain` returns every repo's branches, stacks, plans, cards, CI status, and recent
activity as one JSON document. `?repo=` filters, `?since=` bounds the activity.

`POST /brain/ask` answers questions about that state with Claude. It needs
`ANTHROPIC_API_KEY` and 404s without it.

```sh
curl -X POST http://nashcode.example/brain/ask \
  -H 'content-type: application/json' \
  -d '{"question":"which stack is closest to mergeable?","repo":"alpha"}'
```

## Deploy

Build on a Linux box that has `cargo`, `node`, and `npm`:

```sh
cargo build --release
```

Ship `target/release/nashcode-viewer` on its own. The assets are inside it, and node is not needed
at runtime. Only `git` is.

A systemd unit:

```ini
[Unit]
Description=nashcode
After=network-online.target

[Service]
ExecStart=/usr/local/bin/nashcode-viewer
Environment=DGIT_URL=https://git.your-tailnet.example
Environment=NASHCODE_MIRRORS=/var/lib/nashcode/mirrors
Environment=NASHCODE_BIND=127.0.0.1:8090
EnvironmentFile=/etc/nashcode.env
Restart=on-failure
StateDirectory=nashcode

[Install]
WantedBy=multi-user.target
```

Keep `GIT_TOKEN` and `ANTHROPIC_API_KEY` in `/etc/nashcode.env`, mode 600.

The mirrors directory is a cache. It costs a re-clone to lose, not data. Your repos live on
the dgit server, which is what you back up. SQLite holds the only thing that exists nowhere
else: comments, CI history, traces, and the audit trail. Back up `NASHCODE_DB`, and
`NASHCODE_TRACES` if the raw transcripts matter to you.

## Development

```sh
cargo nextest run     # the whole suite, real git in tempdirs
cargo run             # against a local bare repo dir, per above
```

Tests build fixture repos with real `git` commands. There are no git mocks.

`NASHCODE_SKIP_ASSET_BUILD=1` skips esbuild for a Rust-only compile without node.

[SPEC.md](SPEC.md) is the contract. [NOTES.md](NOTES.md) records where the implementation
had to choose.

## License

MIT
