---
title: Invariants — what nashcode holds, what it lacks, what other forges do
status: draft
---

# Invariants

Research date: 2026-08-22. Four passes: a code audit of this repo, and web surveys (Exa + Firecrawl) of forges, trackers, and review/CI systems. Source URLs are in the appendix.

## 1. Where an invariant lives decides how it fails

Every system surveyed enforces rules in one of five layers. The layer predicts the failure mode.

| Layer | Example | Failure when violated |
|---|---|---|
| Ref-update hook (pre-receive) | GitLab push rules, Gerrit `Change-Id`, Gitea protected branches | Hard reject at push |
| App state machine | GitHub merge button, Jira transitions, Bugzilla `_check_resolution` | Wedged item: a required signal never arrives, nothing rejects |
| Scheduler / queue | GitHub merge queue, GitLab merge trains | Ejection cascades, queue starvation |
| Cryptographic quorum | Radicle delegate threshold | Below threshold, the state is not canonical — no error |
| Client-side metadata | Graphite parent pointers, Beads ready-set | Silent divergence between tool model and git DAG |

nashcode today lives almost entirely in layer 2 (the viewer) and layer 5 (front matter in git). It has **no layer 1**: dgit accepts any push. This is the root of most gaps below.

## 2. What nashcode already enforces

The audit found ~70 enforced invariants. The strong ones:

- **Path safety**: plain repo names at one insertion point (`viewer/src/config.rs:94-100`), write paths resolved and symlink-refused (`viewer/src/ops.rs:193-247`), git flags neutralised with `--`.
- **Merge gate**: target is the inferred stack parent; CI `queued/running/failed/timeout/error` all block (`viewer/src/db.rs:100-104`); `allow_red` is an explicit override.
- **Atomic writes**: every push is `--atomic` and `--force-with-lease`; branch delete is leased on the merged tip (`viewer/src/ops.rs:148-171,334-341`).
- **Card flip on merge**: every non-`done` card with `branch: B` flips in the same atomic push; quarantined cards never flip (`viewer/src/ops.rs:389-448`).
- **Bad card never breaks the board**: unparseable front matter lands in `needs-attention` (`viewer/src/docs.rs:26-27`).
- **Status rewrite touches one line** by offset (`viewer/src/docs.rs:112-144`).
- **Comment outdated** when anchor commit ≠ tip and the file changed (`viewer/src/web/pages.rs:1285-1294`) — same model as GitHub.
- **CI env is cleared and rebuilt**; 30-min timeout; every failure is a recorded status.
- **Traces**: `(repo, session, seq)` unique; commits attributed only when HEAD moves between two events.
- **Upstream mirrors** never receive `GIT_TOKEN`; `https` only.

## 3. Gaps, ranked by damage

| # | Gap | Where | What the field does |
|---|---|---|---|
| 1 | **`StackGraph::infer` reads tips one at a time** across a possible fetch, yielding wrong parents. The fix, `Repo::tips()`, exists with zero callers. | `viewer/src/stack.rs:44-47`, `viewer/src/git.rs:191-210` | Graphite: one recorded parent per branch, restack is a tree op. |
| 2 | **A crashed CI run stays `running` and blocks merge forever.** No startup reconciliation. Only escape is `allow_red`, indistinguishable from a real red. | `viewer/src/db.rs:269-278`, `viewer/src/ci.rs:211` | GitHub's #1 real-world failure: a required check that can never be satisfied. Buildkite reports `error` for skipped builds, never leaves pending. |
| 3 | **Status validated on one door of two.** HTTP move enforces `[a-z0-9-_]`, ≤40, not `needs-attention`. A git push accepts any string, including `needs-attention` — colliding with the quarantine column. No state machine: `done → todo → done` is fine. | `viewer/src/web/api.rs:281-288` vs `viewer/src/docs.rs:182-190` | Linear: status *type* is a closed enum, statuses free. Bugzilla: N×N transition matrix, write-time. Maniphest: no transitions by design, but exactly-one `default/closed/duplicate` special, validated at config load. |
| 4 | **"One plan per branch" is documented, not enforced.** `card_for_branch` takes `.find()` — first wins, silently. All matches flip on merge. | `viewer/src/docs.rs:349-364`, `viewer/src/ops.rs:405-424` | Gerrit: one `Change-Id` ↔ one change per (project, branch), rejected at push. Copilot agent: exactly one PR per task. |
| 5 | **Anyone who can push runs code on the box with `GIT_TOKEN`.** `.nashcode/ci` runs from the pushed commit, no per-repo opt-in. An invite is remote code execution. | `viewer/src/ci.rs:258-280` | GitHub treats agents as outside contributors: workflows need a human "Approve and run". |
| 6 | **Comment `author` is client-supplied.** Impersonation also hands deletion rights to the impersonated person. | `viewer/src/web/api.rs:166-170`, `viewer/src/db.rs:249-254` | Every system binds assertions to an authenticated identity. Reviewable types agent authorship so gates can reason about it. |
| 7 | **Comments have no referential integrity.** No FK (the repo set is in memory). Branch delete strands comments; line anchors never bounds-checked — line 9999 of a 12-line file renders as "current". | `viewer/src/db.rs:1302-1312`, `viewer/src/web/api.rs:150-164` | Gerrit: ported comments degrade inline → file-level → change-log, never vanish. Hypothesis: explicit `orphan` state. Notion silently drops — the counterexample. |
| 8 | **Dangling `branch:`/`plan:`/`tasks:` refs** render "missing" on one page and are reported nowhere. | `viewer/src/web/pages.rs:1396-1411` | Trac: states derived from transitions, so a typo invents a state — they document `reset_workflow` as the rescue. Beads: `bd doctor`. |
| 9 | **No blocking graph at all.** Cards have `tasks:` (containment) but no `blocks:`. An agent can claim work whose prerequisite is open. | `viewer/src/docs.rs:95-110` | Beads: typed edges, cycle check before commit, `bd ready` = all blockers closed, `--claim` atomic. Bugzilla: transitive-closure acyclicity on every write. |
| 10 | **Trace `seq` race**: `SELECT MAX+1` then `INSERT OR IGNORE` outside a transaction; loser reported as duplicate. Session ids collide on disk (`a.b` = `a_b`, 128→120 truncation) and overwrite. | `viewer/src/db.rs:355-382`, `viewer/src/traces.rs:129-136` | git-bug: IDs from exact stored bytes; FF-only pushes so merged state is never overwritten. |

Smaller: `walk`/`descendants` have no cycle guard (fine until #1 tears the graph); `/plans/{*rest}` has no `..` check; comment body has no cap or rate limit; core tables have no FKs while `bugs_*` tables do; audit write failure after a successful push is a log line.

## 4. Five patterns worth stealing

1. **Version binding.** Every assertion (comment, approval, CI status) names the artifact version it is about. nashcode does this for comments (anchor commit) and CI (`seen_tips`). It does not do it for card state: a card's `status: done` has no link to *which* merge made it so.
2. **Monotonic degradation, never silent relocation.** Gerrit: inline → file-level → log. Hypothesis: loud `orphan`. nashcode's "outdated" is a good first rung; it needs the "file gone" and "branch gone" rungs.
3. **Separate gate axes.** Critique: LGTM (correctness) ≠ Approval (ownership) ≠ CI (mechanical). Collapsing them loses the ability to say what is missing. nashcode has one axis (CI) plus `allow_red`. Stuck-CI and red-CI should not share an override.
4. **Named next actor.** Gerrit attention set, Reviewable "waiting on". Without it a stale signal has no owner. nashcode's `/brain` is the natural home.
5. **Config is itself reviewed.** Gerrit's `refs/meta/config`; Radicle's signed identity doc. nashcode already stores plans and cards in git. Its *rules* (status alphabet, reserved statuses, CI opt-in) could live in `.nashcode/` too — then a rule change is a diff with comments.

The dominant failure across every system: **a gate that can never be satisfied**, and the universal escape is admin bypass, which voids the audit trail. Gaps #2 and #3 are nashcode's versions of that.

## 5. Recommended order

Ranked by damage ÷ effort. Each is one small diff.

1. Call `Repo::tips()` from `StackGraph::infer` (#1). One-liner; the only gap that corrupts merges.
2. On startup, mark every `queued`/`running` run from a prior process as `error` (#2).
3. Apply the API status rule in `parse_document`; reserve `needs-attention` for the parser (#3).
4. Record a conflict when `by_branch[b].len() > 1`; surface in `/brain`; refuse the merge flip until resolved (#4).
5. Per-repo CI opt-in file on the default branch; pass `GIT_TOKEN` only when declared (#5).
6. Bind comment `author` to the Tailscale login, store any claimed name separately (#6).
7. Bounds-check comment `line` and existence of `file` at the anchor commit; tombstone comments on branch delete (#7).
8. `DocIndex` emits dangling refs to `/brain` (#8).
9. `BEGIN IMMEDIATE` around seq allocation; hash session ids for the on-disk name (#10).
10. `blocks:` front-matter key with a cycle check at ingest and a `ready` filter on the board (#9). Largest, and the one that most changes what agents can do.

## Appendix — sources

Forges: GitHub rulesets and merge queue (docs.github.com), GitLab push rules and merge trains (docs.gitlab.com), Gerrit submit requirements and `Change-Id` (gerrit-review.googlesource.com), Phabricator Herald and Phorge T15410, Graphite restack docs, Sapling/ghstack, Radicle protocol guide, Gitea/Forgejo protected branches, SourceHut man pages.

Trackers: Jira workflow docs, Linear workflow/cycle docs, GitHub sub-issues and closing keywords, Bugzilla `Bug.pm` (`_check_resolution`, `ValidateDependencies`, `_resolve_ultimate_dup_id`), Maniphest `ManiphestTaskStatus.php`, Shortcut API, Beads docs (beads.gascity.com), git-bug `dag-entity.md` spec, Fossil `bugtheory.wiki`, Trac `TracWorkflow`.

Review/CI: GitHub pull-comment API and stale-approval changelog (2023-06-06), Gerrit porting-comments and `copyCondition`, Reviewable docs, SWE at Google ch.19 (Critique), Hypothesis fuzzy anchoring, Google Drive comment anchors, Plannotator, Copilot cloud agent docs, Devin bot-comment settings, Beads hash IDs.
