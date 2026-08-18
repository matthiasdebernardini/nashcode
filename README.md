# nashgit

Stacked-branch review, CI, and a kanban board for a [dgit](https://github.com/littledivy/dgit)
server on a private tailnet.

dgit already serves git over smart HTTP and gives you a cgit-style browser. nashgit adds
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
- `/:repo/traces` — agent sessions: the transcript that produced each commit
- `/brain` — the whole tailnet's work state as JSON

## Requirements

- Rust (edition 2024) and `cargo`
- `node` and `npm` — the build bundles the browser assets
- `git` on `PATH` at runtime, not just at build time

## Run it

```sh
git clone <this repo> && cd nashgit
DGIT_URL=https://git.your-tailnet.example \
GIT_TOKEN=your-dgit-token \
NASHGIT_REPOS=alpha,beta \
cargo run
```

`cargo build` on a fresh clone produces a runnable server. The build script runs `npm ci`
and esbuild, then embeds both bundles in the binary, so there is no separate asset step and
nothing to copy at deploy time.

To try it without a dgit server, point `DGIT_URL` at a directory of bare repos:

```sh
DGIT_URL=/srv/git NASHGIT_REPOS=demo cargo run
```

That reads `/srv/git/demo.git`.

## Configuration

Environment only. Nothing about your deployment lives in the source.

| Variable | Default | Meaning |
|---|---|---|
| `DGIT_URL` | *(none)* | Base URL of the dgit server. Repo `x` is `$DGIT_URL/x.git`. A filesystem path works too. |
| `GIT_TOKEN` | empty | Push token, sent as basic auth `x:$GIT_TOKEN`. Reads are anonymous. Empty means pushes are anonymous. |
| `NASHGIT_REPOS` | empty | Comma-separated repo names. dgit has no list API, so you name them. |
| `NASHGIT_MIRRORS` | `~/mirrors` | Where the mirror clones live. |
| `NASHGIT_BIND` | `127.0.0.1:8090` | Listen address. Keep it on loopback. |
| `NASHGIT_DB` | `$NASHGIT_MIRRORS/nashgit.db` | SQLite file: comments, CI runs, audit trail. |
| `NASHGIT_CI_LOGS` | `$NASHGIT_MIRRORS/ci-logs` | CI log files. |
| `NASHGIT_WEBHOOKS` | *(none)* | Path to a JSON file mapping events to URLs. |
| `NASHGIT_TRACES` | `$NASHGIT_MIRRORS/traces` | Raw agent transcripts. |
| `ANTHROPIC_API_KEY` | *(none)* | Enables `POST /brain/ask`. Without it that route answers 404. |
| `NASHGIT_BRAIN_MODEL` | `claude-opus-5` | Model for `/brain/ask`. |

The client commands (`hook`, `trace`, `doctor`) read two more:

| Variable | Default | Meaning |
|---|---|---|
| `NASHGIT_URL` | `http://127.0.0.1:8090` | Where the viewer is, from the agent's machine. |
| `NASHGIT_REPO` | *(inferred)* | Repo name, when the git remote's basename is not it. |

At startup the server prints a line for each thing that is unset and what you lose by it.

## Exposing it

nashgit binds loopback and has no auth of its own. Put `tailscale serve` in front:

```sh
tailscale serve --bg --https 8443 http://127.0.0.1:8090
```

Tailscale injects `Tailscale-User-Login` and `Tailscale-User-Name` on every proxied
request. nashgit trusts those headers and stamps them on comments, merges, and reruns. A
request without them shows as `local`.

Do not expose the port any other way. The headers are trusted, so anything that can reach
the port can claim any identity.

## CI

Put an executable `.nashgit/ci` in a repo. When nashgit sees a branch tip it has not seen
before, it checks that commit out into a scratch directory and runs the script. No script
means no job.

Jobs run one at a time. The script gets `GIT_TOKEN`, `NASHGIT_REPO`, `NASHGIT_BRANCH`, and
`NASHGIT_COMMIT`, and nothing else. Non-zero exit is red. Timeout is 30 minutes. Combined
output goes to a log file and the status lands next to the branch everywhere it appears.

There is no separate deploy system. If the script wants to deploy, it deploys.

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

`branch:` and `plan:` wire the card to a branch and a plan, and nashgit computes the
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
curl -X POST http://nashgit.example/alpha/comments \
  -H 'content-type: application/json' \
  -d '{"branch":"main","file":"plans/api.md","line":12,"body":"needs a timeout"}'

# read, oldest first
curl 'http://nashgit.example/alpha/comments?file=plans/api.md'
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

Outgoing only. dgit emits none, so nashgit's poller is the event source. Point
`NASHGIT_WEBHOOKS` at a JSON file:

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
tried first". nashgit stores agent sessions — prompts, tool calls, results — and links
them to commits automatically: every recorded event carries the repo's `HEAD` at that
moment, so when `HEAD` moves between two events, the commits in between belong to that
session. No commit trailers, nothing for the agent to remember.

Transcripts live in SQLite and under `NASHGIT_TRACES`, not in git. They are large and
append-heavy; committing them would bloat every clone. The link to git is the commit SHA.

`/:repo/traces` lists sessions. A session page renders the transcript with its commits
inline, and a commit's row on the branch page links back to the conversation that wrote
it. `GET /:repo/commits/:sha/trace` answers the same question as JSON.

One warning: a transcript contains whatever the agent saw, secrets included. nashgit does
not redact. The tailnet is the perimeter here as everywhere else.

### Wiring an agent

The nashgit binary is also the client. Put it on the agent's machine and let the harness
hooks feed it. For Claude Code, in `.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "nashgit hook" }] }],
    "PostToolUse":      [{ "hooks": [{ "type": "command", "command": "nashgit hook" }] }],
    "Stop":             [{ "hooks": [{ "type": "command", "command": "nashgit hook" }] }]
  }
}
```

`nashgit hook` reads one hook payload from stdin, records it, and exits 0 — always. A
dead server, garbage input, or a missing repo never fails the agent's turn. Set
`NASHGIT_DEBUG=1` to see why an event was dropped.

For a run that happened without the hook, backfill from the harness transcript:

```sh
nashgit trace push ~/.claude/projects/<project>/<session>.jsonl
nashgit trace list
nashgit trace show <session>
nashgit doctor        # what is configured, is the server reachable
```

## Brain

`GET /brain` returns every repo's branches, stacks, plans, cards, CI status, and recent
activity as one JSON document. `?repo=` filters, `?since=` bounds the activity.

`POST /brain/ask` answers questions about that state with Claude. It needs
`ANTHROPIC_API_KEY` and 404s without it.

```sh
curl -X POST http://nashgit.example/brain/ask \
  -H 'content-type: application/json' \
  -d '{"question":"which stack is closest to mergeable?","repo":"alpha"}'
```

## Deploy

Build on a Linux box that has `cargo`, `node`, and `npm`:

```sh
cargo build --release
```

Ship `target/release/nashgit` on its own. The assets are inside it, and node is not needed
at runtime. Only `git` is.

A systemd unit:

```ini
[Unit]
Description=nashgit
After=network-online.target

[Service]
ExecStart=/usr/local/bin/nashgit
Environment=DGIT_URL=https://git.your-tailnet.example
Environment=NASHGIT_REPOS=alpha,beta
Environment=NASHGIT_MIRRORS=/var/lib/nashgit/mirrors
Environment=NASHGIT_BIND=127.0.0.1:8090
EnvironmentFile=/etc/nashgit.env
Restart=on-failure
StateDirectory=nashgit

[Install]
WantedBy=multi-user.target
```

Keep `GIT_TOKEN` and `ANTHROPIC_API_KEY` in `/etc/nashgit.env`, mode 600.

The mirrors directory is a cache. It costs a re-clone to lose, not data. Your repos live on
the dgit server, which is what you back up. SQLite holds the only thing that exists nowhere
else: comments, CI history, and the audit trail. Back up `NASHGIT_DB`.

## Development

```sh
cargo nextest run     # the whole suite, real git in tempdirs
cargo run             # against a local bare repo dir, per above
```

Tests build fixture repos with real `git` commands. There are no git mocks.

`NASHGIT_SKIP_ASSET_BUILD=1` skips esbuild for a Rust-only compile without node.

[SPEC.md](SPEC.md) is the contract. [NOTES.md](NOTES.md) records where the implementation
had to choose.

## License

MIT
