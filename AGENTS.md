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

Every comment carries an `id`, an `author`, and a `created_at`. Results come back oldest
first, ordered by `created_at` then `id`.

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
    "body": "Three is too many. Two, then fail loudly.",
    "created_at": "2026-08-18T10:04:11.512004Z"
  }
]
```

`line` is the one-based line in the file at `commit`. It is `null` for a comment on the
whole file or the whole branch.

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
    "author": "planner-agent"
  }'
```

`file` and `line` are optional. Omit both for a branch-level comment. Omit `author` and it
falls back to the caller's Tailscale identity, then to `local`. You get `201` and the stored
comment, `id` included.

Reply on the line you are answering. That is how a human sees the thread in place.

## 5. Revise

Edit the plan, commit, push. The comments stay put. Ones anchored to a line whose file has
since changed move to an "outdated" section, so a human can tell what you have already
addressed.

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

## State

`GET /brain` returns everything nashcode knows, as one JSON document: every repo's branches
with stack parent and CI status, plans, cards by column, recent merges, restacks, comments,
and CI runs.

```sh
curl -s "$NASHCODE/brain?repo=$REPO"
curl -s "$NASHCODE/brain?since=2026-08-18T00:00:00Z"
```

Read this to answer "what is going on" without a dozen calls.

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

## Rules

- Never force-push a branch a human is reviewing without saying so in a comment first.
- One plan per branch. The `branch:` ref assumes it.
- Do not edit another agent's card body. Change `status`, or add a comment.
- CI runs on every new tip, from `.nashcode/ci` in the repo. Check it is green before asking
  for a merge — a red branch will not merge without a human overriding it.
- You cannot merge. A human does that.
