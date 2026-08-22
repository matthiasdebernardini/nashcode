# nashcode for agents

This file is for coding agents. It documents the loop: push a plan, get it reviewed,
revise, ship.

Two ideas make this work. Plans and cards are **files in the repo**, so you change them the
way you change any file — edit and push. Comments are **not** in the repo, so they have an
HTTP API with a cursor you can poll.

Set two things:

```sh
NASHCODE=http://nashcode.example    # the viewer
REPO=alpha
```

## The `nashcode` CLI

Nobody types it: you drive it. So it answers the way you read — **one JSON envelope on
stdout, always**, and a typed exit code you branch on instead of parsing prose.

```json
{ "ok": true, "command": "nashcode ls", "timestamp": "…", "exit_code": 0,
  "result": { … }, "next_actions": [ … ] }
```

A failure swaps `result` for `error` and adds `fix`:

```json
{ "ok": false, "exit_code": 4,
  "error": { "message": "…returned HTTP 401 — the profile's token was rejected…",
             "code": "AUTH", "retryable": false },
  "fix": "nashcode token   # compare it with GIT_TOKEN in ~/dgit/wrangler.celld.jsonc" }
```

| exit | meaning | what to do |
|---|---|---|
| 0 | it worked | read `result` |
| 1 | something else broke | read the message |
| 2 | bad invocation | run `fix`; it is the corrected line |
| 3 | no such profile, repo, viewer, or token | create it |
| 4 | the token was rejected (401/403) | rotate it |
| 5 | dgit, the viewer, or an ssh script failed | `nashcode doctor` |

`fix` is a command, never advice. Run it.

`next_actions` is the trail: after `plan new` it names the `annotate` and `comments` calls
for that file; after `comments` it names the same call with `--since=` of the newest
comment it just returned, so a polling loop needs no bookkeeping. `--quiet` drops them.

Four flags work on every command without being declared:

- `--select=a,b,c` — project the result to those fields. Lists advertise their own paths
  in `result.fields` (`items.id,items.body`); paste them back unedited.
- `--compact` — drop null and empty fields.
- `--quiet` — empties `next_actions` on a successful envelope and silences the progress
  notes on stderr. It does **not** strip them from an error envelope;
  the trail on a failure is the fix, so that is no great loss, but do not count on it.
- `--json` — accepted and ignored. Output is always JSON; the flag survives so calls you
  already have memorised keep working.

`--yes` and `--dry-run` are reserved too: `nashcode rm <name>` refuses without `--yes`
(there is no prompt to answer), and `nashcode setup --dry-run` returns every remote script
it would have run in `result.scripts` instead of running any.

`--profile <name>` acts on a saved profile other than the active one, on every command.

`nashcode skill` prints this CLI as a `SKILL.md` built from the live command tree, and
`nashcode skill --install=<dir>` writes it to `<dir>/nashcode/SKILL.md`. If you can run
the binary once, you can bootstrap from it.

Progress notes go to stderr; stdout is only ever the envelope.

Two commands are deliberately different:

- **`nashcode grep`** answers in ripgrep's format with ripgrep's exit codes (0 hits,
  1 none), because that is what makes it usable on reflex. It keeps its own `--json`.
- **`nashcode brain`** never fails. See "State" below.

Run `nashcode` with no arguments for the command tree, or `nashcode help <command>`.

## The loop

1. Write a plan to `plans/<name>.md` on a branch. Push it.
2. A human reads it at `$NASHCODE/$REPO/plans/<name>.md` and comments. Tools can post
   comments instead.
3. Poll for comments with a `since` cursor.
4. Revise the plan, push again.
5. When the plan is settled, implement it on the same branch.
6. A human merges.

## 1. Push a plan

```sh
git checkout -b feat/retries
mkdir -p plans
cat > plans/retries.md <<'EOF'
---
branch: feat/retries
---

# Retry policy

Retry idempotent requests three times with exponential backoff.
EOF
git add plans/retries.md
git commit -m "plan: retry policy"
git push origin feat/retries
```

The `branch:` front matter links the plan to the branch. The branch page then shows the
plan, and the plan shows the branch with its CI status.

## 2. Read comments

```sh
curl -s "$NASHCODE/$REPO/comments?file=plans/retries.md"
```

Every comment carries an `id`, an `author`, an `on_behalf_of`, and a `created_at`. Results
come back oldest first, ordered by `created_at` then `id`.

```json
[
  {
    "id": 41,
    "repo": "alpha",
    "branch": "feat/retries",
    "file": "plans/retries.md",
    "line": 7,
    "commit": "9c1f...",
    "author": "ada@example.com",
    "on_behalf_of": null,
    "body": "Three is too many. Two, then fail loudly.",
    "created_at": "2026-08-18T10:04:11.512004Z",
    "orphaned_at": null
  }
]
```

`orphaned_at` is when the comment's branch was deleted, `null` while the branch is alive.

`line` is the one-based line in the file at `commit`. It is `null` for a comment on the
whole file or the whole branch.

`author` is the Tailscale login of whoever posted. `on_behalf_of` is the person an agent
posted for, `null` otherwise; the viewer renders the pair as "on_behalf_of via author".

## 3. Poll with a cursor

Keep the `created_at` of the last comment you handled. Pass it as `since`. You get
everything strictly newer, once.

```sh
LAST=2026-08-18T10:04:11.512004Z
curl -s "$NASHCODE/$REPO/comments?file=plans/retries.md&since=$LAST"
```

`since` is RFC3339. Timestamps are fixed-width UTC, so they sort in the order they happened.
Poll on a timer. There is no long-poll and no websocket.

Filters combine: `?branch=`, `?file=`, `?since=`. Drop `file` to watch a whole branch.

## 4. Post a comment

```sh
curl -X POST "$NASHCODE/$REPO/comments" \
  -H 'content-type: application/json' \
  -d '{
    "branch": "feat/retries",
    "file": "plans/retries.md",
    "line": 7,
    "body": "Dropped to two retries in the next push.",
    "on_behalf_of": "ada@example.com"
  }'
```

`file` and `line` are optional. Omit both for a branch-level comment.

An anchor has to point at something real, or you get a `400`: `file` must be in the branch
at its current tip, and `line` must be a line that file has. A `line` without a `file`, a
`line` below 1, an unknown `branch`, and an empty `body` are `400` too. The check keeps a
comment from being stored where it can never render.

You cannot choose the `author`. It is always your Tailscale identity, or `local` for a
direct loopback hit; an `author` field in the body is ignored. Send `on_behalf_of` when you
post for someone else — it records the person without hiding you, and only the actor can
delete the comment. You get `201` and the stored comment, `id` included.

Reply on the line you are answering. That is how a human sees the thread in place.

A human reviewing locally does not curl. `nashcode annotate plans/retries.md` opens the
plan in plannotator with an Approve button and posts what they decided here for them: their
notes, or `Approved.` when they had none. It arrives as a whole-file comment on the branch
the working copy is on, so a poller watching `?file=plans/retries.md` sees it like any
other. Dismissing posts nothing.

Launching is what the command is for, so it launches by default — you open the tool for the
human. `--no-launch` is the inspect-only form: it reports the file, where plannotator is
(`null` when it is not installed), and the plan's viewer URL, and opens nothing. When the
comment cannot be posted, the feedback comes back inside the error message rather than
being lost, and the `next_action` is the `comments --since=<now>` call to poll with.

## 5. Revise

Edit the plan, commit, push. The comments stay put. Ones anchored to a line whose file has
since changed move to an "outdated" section, so a human can tell what you have already
addressed. So do comments on a file the branch has deleted: they degrade to file-level and
outdated, they are never dropped.

Deleting a branch does not delete its comments either. Each one gets an `orphaned_at`
timestamp and moves to an "orphaned comments" group on the default branch page, named with
the branch it came from.

## Cards

Markdown under `tasks/` is a kanban card. You create and move cards by editing files and
pushing — the same as plans. The board endpoint exists for humans dragging with a mouse.

```markdown
---
status: todo
title: Wire the retry policy
assignee: builder-agent
branch: feat/retries
plan: plans/retries.md
---

Implement the two-retry policy from the plan.
```

- `status` is the column. `todo`, `doing`, `done` are the canonical ones. Anything else
  becomes its own column.
- `title` defaults to the first heading, then to the filename.
- `branch` and `plan` build the links.
- `blocks: [tasks/b.md, ...]` says this card blocks those: `b.md` cannot start until this
  one is `done`. One path or a list.

A `todo` card is **ready** when every card that blocks it is `done`. `GET /brain` lists
the ready paths per repo, `/{repo}/board?ready=1` narrows the todo column to them, and
`nashcode ready [<repo>]` prints them one row apiece.

A `blocks:` cycle is quarantined at ingest: every card on the loop lands in
`needs-attention` with `blocks cycle: tasks/a.md -> tasks/b.md -> tasks/a.md`, because
none of them can ever be ready. A `blocks:` path no file answers to is a dangling ref,
reported with the others.

Take a ready card with `nashcode claim tasks/x.md`: it writes `status: doing` and
`assignee: <your user.name>`, commits that one file, and pushes. Two agents reading the
same ready list then race on the push, not on the file.

Move a card by rewriting its `status` and pushing:

```sh
sed -i 's/^status: todo$/status: doing/' tasks/retries.md
git commit -am "start retries"
git push origin main
```

Change only the `status` line. The rest of the file belongs to whoever wrote it.

When a human merges a branch, any card with that `branch:` flips to `done` automatically.
You do not need to close it yourself.

If front matter will not parse, the card lands in a "needs attention" column instead of
breaking the board. Look there when a card goes missing.

## Transcripts

POST a finished meeting and it lands as one commit on the default branch, at
`transcripts/YYYY/MM/<id>.md`. The id is the UTC start minute plus a slug of the title.
A name already taken gets a `-2` suffix, so nothing overwrites an earlier meeting. The
file holds front matter (`id`, `title`, `started_at`, `attendees`, `digested: false`),
the action items, and the turns. The reply is `201` with `id`, `path`, and `commit`.

```sh
curl -X POST "$NASHCODE/$REPO/transcripts" \
  -H 'content-type: application/json' \
  -d '{"title":"Weekly sync","started_at":"2026-06-12T15:00:00Z",
       "ended_at":"2026-06-12T15:30:00Z","speakers":[{"id":"S1","name":"Rob"}],
       "segments":[{"speaker":"S1","start_ms":5000,"end_ms":9000,"text":"Morning."}]}'
```

Bad payloads get `400` with the reason. A browser extension must POST from its service
worker: a content script sends `Sec-Fetch-Site: cross-site`, which the origin check
refuses.

## State

`GET /brain` returns everything nashcode knows, as one JSON document: every repo's branches
with stack parent and CI status, plans, cards by column, recent merges, restacks, comments,
and CI runs.

```sh
curl -s "$NASHCODE/brain?repo=$REPO"
curl -s "$NASHCODE/brain?since=2026-08-18T00:00:00Z"
```

Read this to answer "what is going on" without a dozen calls.

`nashcode brain [repo]` is the same document, digested — the form to read at the start of
a session. It keeps the facts and drops the aggregate: branches with tip, ahead count and
CI state; what the code index holds and how old it is; the plan files and how many comments
wait on each; the latest architecture submission; and the last five activity entries, one
line apiece.

```sh
nashcode brain            # the repo `origin` points at; every repo outside one
nashcode brain alpha
nashcode brain alpha --select=repos.branches,repos.code
```

It always exits 0, so it is safe in a session-start hook. A viewer that is down or not
configured is still an `ok: true` envelope, with `result.status` set to `unavailable` and
`result.error` saying why; a live one sets `result.status` to `ok`. Nothing about a dead
viewer can take a session down with it. Use `curl /brain` when you want the whole document.

`POST /brain/ask` puts Claude in front of the same document for judgment calls.

```sh
curl -X POST "$NASHCODE/brain/ask" \
  -H 'content-type: application/json' \
  -d '{"question":"what should I pick up next?","repo":"alpha"}'
```

It returns `{"answer": "...", "model": "..."}`. It needs `ANTHROPIC_API_KEY` on the server
and answers 404 without it. Use `/brain` for facts and `/brain/ask` for opinions.

## Raw files

```sh
curl -s "$NASHCODE/$REPO/raw/main/plans/retries.md"
```

Exact bytes, `text/plain`. The branch may contain slashes.

## Traces

If the `nashcode-viewer hook` is wired into your harness (see the README), every commit you make
is linked to your session automatically — no trailer, no convention. Humans get from your
diff to your transcript in one click; you can read sessions back too:

```sh
curl -s -H 'accept: application/json' "$NASHCODE/$REPO/traces"
curl -s -H 'accept: application/json' "$NASHCODE/$REPO/traces/<session>"
curl -s "$NASHCODE/$REPO/commits/<sha>/trace"
```

To record events yourself, POST batches; `(session, seq)` is idempotent, so retries are
safe:

```sh
curl -X POST "$NASHCODE/$REPO/traces/events" \
  -H 'content-type: application/json' \
  -d '{"session":"my-session","agent":"my-agent","events":[
        {"seq":1,"kind":"prompt","payload":{"prompt":"..."},"head":"<git rev-parse HEAD>"}]}'
```

Include `head` whenever you can — it is the whole linking mechanism.

## Prompts

Every prompt recorded through the hook is listed at `/:repo/prompts`, searchable, linked
back to its session. Read them as JSON to see what a human has been asking for:

```sh
curl -s -H 'accept: application/json' "$NASHCODE/$REPO/prompts?q=retry"
curl -s -H 'accept: application/json' "$NASHCODE/$REPO/prompts?session=<session>"
```

Each entry carries `session`, `seq`, `text`, `head`, `agent`, and `created_at`.

## Architecture

Read the graph, draw it, submit the drawing. The whole static analysis comes in one call:

```sh
curl -s "$NASHCODE/$REPO/code/graph"
```

That document holds `files`, `symbols`, and `edges`. Turn it into a mermaid diagram and
post that:

```sh
curl -X POST "$NASHCODE/$REPO/architecture" \
  -H 'content-type: application/json' \
  -d '{"title":"Request path","note":"Where a page load goes.",
       "mermaid":"graph TD;\n  viewer-->mirror;\n  mirror-->dgit;"}'
```

`mermaid` is required and capped at 64 KiB; `title` and `note` are optional. The server
stores the text verbatim and never parses it — a diagram that will not render shows
mermaid's own error box on the page. It responds 201 with the stored row.

Submissions are append-only. A new drawing is a new row; nothing is edited.

```sh
curl -s -H 'accept: application/json' "$NASHCODE/$REPO/architecture"          # the latest
curl -s -H 'accept: application/json' "$NASHCODE/$REPO/architecture?history"  # every one
curl -s -H 'accept: application/json' "$NASHCODE/$REPO/architecture?id=7"     # one by id
```

Each row carries `id`, `mermaid`, `title`, `note`, `author`, and `created_at`; `?history`
leaves out the sources. Without the `accept` header the same URL is the page a human
reads.

With nothing submitted, the page shows the mermaid blocks of the repo's own
`ARCHITECTURE.md`. Committing that file is the other way to answer the question.

## Bugs (errors and logs)

nashcode is also the error tracker. Each bugs project mints a DSN; unmodified Sentry
SDKs post exceptions and logs to it, and `POST /api/<project-id>/logs` takes NDJSON
(one JSON object per line: `ts`, `level`, `message`, free attributes) from anything
that can run curl. Auth for the NDJSON door is the same DSN key (`?sentry_key=<key>`).

Read state as JSON, same accept-header convention as everything else:

```sh
curl -s -H 'accept: application/json' "$NASHCODE/bugs"                       # projects + notification state
curl -s -H 'accept: application/json' "$NASHCODE/bugs/<project>"             # issues (+ ?state=unresolved|resolved|muted)
curl -s -H 'accept: application/json' "$NASHCODE/bugs/<project>/issues/<id>" # one issue, events, stack
curl -s -H 'accept: application/json' "$NASHCODE/bugs/<project>/logs?q=timeout&level=error"
```

`GET /bugs` answers an object, not a bare list:
`{"projects": [...], "pushover": {"on": <bool>, "budget": {...}}}`. The budget carries
what is left of the month's notification allowance, how many messages are waiting, and
`parked_until` when the queue is being held — read it before you conclude that a quiet
phone means a quiet week.

The logs search takes free text (FTS), a `file:` token to narrow by source file, a
`level`, and a zero-based `page` (newest first). Resolve or mute an issue with
`POST /bugs/<project>/issues/<id>/state` and a form body `state=resolved`.

**Notifications go out on state changes only** — a new issue, a regression, an unmute —
plus one extra as an unresolved issue crosses 10, 100 and 1000 events. Never one per
event, and never for a log line. If you are wondering whether your thousand-event
crash loop woke somebody: it sent four messages, and each rung rings once in an issue's
life.

Three habits that make this useful to you:

- Attach `code.file.path` and `code.line.number` attributes to log records when your
  runtime does not already (Rust `tracing` does; Python's logger does not yet). A row
  that carries them links straight to the source line in the code browser when the
  project declares its repo, and carries the three lines either side.
- **Set `release` to the git SHA you are running** (most SDKs do this by default). It is
  what pins a log line or stack frame to the exact commit: with a SHA the mirror knows,
  the source shown is the source that ran. Anything else — `v2.4.1`, a build number —
  falls back to the default-branch tip, and the page says "tip, not release" so you know
  you are reading today's code about yesterday's crash.
- Report the path your process actually sees and do not try to make it repo-relative.
  `/app/src/handler.py` from inside a container resolves by matching the longest suffix
  that names exactly one file in the repo. Two files of the same name in different
  directories are ambiguous and render as plain text rather than a link to the wrong
  one, so a fuller path resolves where a bare filename will not.

The viewer reports its own errors the same way when `NASHCODE_BUGS_SELF_DSN` is set,
through the same public door. Notification links are built from `NASHCODE_URL`, so a
deployment that leaves it unset sends links only that box can open.

## Rules

- Never force-push a branch a human is reviewing without saying so in a comment first.
- One plan per branch. The `branch:` ref assumes it.
- Do not edit another agent's card body. Change `status`, or add a comment.
- CI runs on every new tip, from `.nashcode/ci` in the repo. Check it is green before asking
  for a merge — a red branch will not merge without a human overriding it.
- CI is opt-in, and only the **default branch** can opt in. A repo runs nothing until its
  default branch carries `.nashcode/ci.toml` with `enabled = true`; add `git_token = true`
  there to give jobs `GIT_TOKEN`. Adding either file on your own branch does nothing — the
  run is recorded `skipped` with "ci not enabled on default branch", which does not block
  a merge. To turn CI on, change `ci.toml` on the default branch and have a human merge it.
- You cannot merge. A human does that.
