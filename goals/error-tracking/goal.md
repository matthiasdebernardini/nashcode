# Goal: error tracking in nashcode ("bugs")

nashcode becomes the error tracker. Each service gets a DSN (Data Source Name, the
Sentry connection string) minted by nashcode. Unmodified official Sentry SDKs post
errors, logs, and cron check-ins to it. Raw payloads live in S3-compatible object
storage, celld-style. Notifications go to Pushover and nowhere else. This replaces
the Bugsink instance at nac-bugs.fly.dev.

Read `goals/error-tracking/research.md` first. It holds the evidence: Bugsink's
source, the Sentry protocol docs, the tracker landscape, Pushover's API,
log-storage prior art, and grouping semantics. The decisions below are settled.
Flag anything the code proves wrong instead of relitigating.

## Hard constraints

- **Object storage holds the payloads.** Raw event JSON, log batches, and later
  dSYMs go to an S3-compatible bucket. SQLite holds only the index: issues,
  grouping keys, event metadata, the hot log window, check-ins, quotas, the push
  queue. Losing the DB loses no payloads. `nashcode bugs reindex` rebuilds the
  index by re-digesting the bucket.
- **Configure the bucket the way celld does.** `NASHCODE_BUGS_BUCKET=s3://name`,
  optional `S3_ENDPOINT` for MinIO/Garage, credentials from AWS env vars only,
  matching celld, whose S3 client ignores `~/.aws/credentials`. Use a dedicated
  bucket, NOT the celld fleet bucket: its credentials are the fleet's root of
  authority, and telemetry must not widen that blast radius. Unset bucket =
  feature off, one startup line says so (same pattern as `ANTHROPIC_API_KEY`).
- **Pushover only.** No email, no chat webhooks. `NASHCODE_PUSHOVER_TOKEN` +
  `NASHCODE_PUSHOVER_USER`, one application token for everything (since May 2026
  the 10k/month pool is per account, so per-app tokens add no quota).
- **Unmodified official SDKs must work.** Python, JS/Bun, Rust, Ruby/Rails,
  Swift. The protocol subset is small; the interop rules below are the ones that
  break real SDKs when violated.
- **License guardrails.** Never copy Bugsink source (PolyForm Shield, noncompete)
  or errex (AGPL). Mine stackpit (MIT) for patterns. Depend on `sentry-types`
  (MIT, getsentry). Treat getsentry/relay as reference reading only (FSL, not on
  crates.io). Reimplement from develop.sentry.dev; the public spec is not
  licensable.

## Architecture (settled)

**Ingest.** One route: `POST /api/<project_id>/envelope/` (trailing slash). Skip
the legacy `/store/`. Auth from any of: `X-Sentry-Auth` header, `?sentry_key=`
query param, `dsn` key in the envelope header. Look up the project by (numeric
id, 32-hex key), 403 on mismatch. Decompress gzip, deflate, br (zstd if cheap)
with streaming size caps: 1 MiB per event/log item, 20 MiB compressed envelope,
100 MiB decompressed. Write raw bytes to the bucket fast, enqueue digest, return
immediately: Bugsink's ingest/digest split, proven at 1.5M events/day on a $5
VPS. Digest is a single writer: parse, group, index, alert in one transaction.

**Interop rules (each has broken a real server).**
- Return `200` with JSON body `{"id":"<event_id>"}`. An empty body sends
  sentry-elixir into a retry loop (Bugsink PR #396).
- Never 400 on unknown item types. Skip and count them. This is the number-one
  rule in the spec.
- Emit `X-Sentry-Rate-Limits: 86400:transaction;span;profile;profile_chunk;replay;trace_metric:project`
  on every 200 so SDKs stop sending what we do not store. Never include `error`,
  `default`, `log_item`, `monitor`, or `session`. Never send an empty category
  list: it would silence errors and logs too.
- Full browser CORS: `Access-Control-Allow-Origin: *`, expose
  `x-sentry-error, x-sentry-rate-limits, retry-after`, plus an OPTIONS handler
  with Relay's 11-header allow list (copied from relay's `cors.rs`, cited in
  research.md).
- Parse leniently: only `event_id`, `timestamp` (string and numeric forms), and
  `platform` are required. Index the minimal fields; the raw JSON in the bucket
  is the source of truth for the detail view.

**Items handled.** `event` → digest. `log` container → log store. `check_in` →
cron monitoring. `client_report` → counters. Everything else (transactions,
sessions, attachments, profiles, replays) → count and drop. Minidumps: never
(Bugsink flags them off as a DoS magnet).

**DSN.** `https://<32-hex-key>@<host>/<numeric-project-id>`. The DSN host comes
from `NASHCODE_BUGS_INGEST_URL`, not the bind address: the public ingest domain
for projects on public infra, the tailnet URL for tailnet ones. Old nac-bugs
DSNs are not preserved; the two affected apps get new DSNs (rework accepted).
Projects live in SQLite, created in the UI. A project page shows its DSN and a
copy-paste SDK snippet, and may declare a nashcode repo for cross-links.

**Grouping (mechanism `nashcode-v1`, store the version tag per issue).** An
explicit SDK `fingerprint` wins, with `{{ default }}` substitution. Otherwise:
last exception in the chain, key = `{type}: {parameterized value}`. Port the
parameterization step (uuid/hex/int/ip/email/url/date/quoted-str to
placeholders, first 2 lines, value ≤1024, type ≤128). `mechanism.synthetic` →
use the crash-location function name. Native (iOS) events: key = (debug_id,
instruction_addr − image_addr) of the top in-app frame, which groups correctly
per build with zero symbolication. Store the grouping key as a readable string,
hashed for lookup.

**Issues.** States: unresolved / resolved / muted. Any event on a resolved issue
reopens it (regression). Mute-for (duration) and mute-until (N events per
period), evaluated on ingest only. Escalation ladder: one extra notification
when an unresolved issue crosses 10, 100, 1000 events.

**Pushover.** Notify on state changes only: new issue, regression, unmute, cron
incident open, recovery. Never per event. Payload: title
`{project}: {issue title}` (≤250 chars), message = exception value plus a few
tags (≤1024 chars, never empty), `url` = the nashcode issue page, `url_title` =
"Open in nashcode". Priority 0; 1 for fatal. Emergency (2) is per-project opt-in
with `tags=<issue-key>` and `cancel_by_tag` on resolve, never the callback param
(it needs a public URL). Outbound queue in SQLite, single sender task: 5xx →
retry after ≥5 s; any 4xx → never retry; 429 → park the queue until
`X-Limit-App-Reset`. Global cap ~20 messages/hour; when it trips, send one
"notifications suppressed, N pending" message. Track `X-Limit-App-Remaining`
and show the monthly budget in the UI.

**Logs.** Two doors, one store. (1) Sentry `log` envelope items on the existing
endpoint, so every DSN is already a log sink for SDKs (JS ≥9.41, Python ≥2.35,
Rust ≥0.42, Ruby ≥5.24, Cocoa ≥9.0). (2) `POST /api/<project_id>/logs` NDJSON
(ts, level, message, free attributes), authed by the same DSN key, for
journald/Vector/curl/cron. The schema uses the OTel severity model (trace…fatal,
severity_number 1–24) so a future OTLP receiver needs no migration. Batches
archive to the bucket as NDJSON objects.

Every log row indexes its code origin when the attributes carry it:
`code.file.path`, `code.line.number`, `code.function.name` (also accept the
pre-2024 OTel names `code.filepath` / `code.lineno` / `code.function` and
normalize). SDK logger integrations attach these by default; the NDJSON door
takes them as plain attributes. The logs page shows `file:line` on each row,
and when the project declares a nashcode repo, links it to the code browser
(`/:repo/blob/:path#L<n>`, path resolved against the repo root; unresolvable
paths render as plain text, never a dead link). Filter by `file:` in the logs
search. The same linking applies to stack frames on the issue detail page:
in-app frames whose filename resolves in the declared repo link to the line. SQLite keeps the hot window with FTS5
(external-content table) for search, pruned nightly by per-project
`retention_days`. Logs never push to Pushover (per-project fatal-log opt-in at
most, later).

**Crons.** Store `check_in` items. Upsert a monitor only when a valid
`monitor_config` accompanies the check-in. Persist `next_checkin_latest` and
`timeout_at`; a 1-minute tokio interval sweeps both (two indexed queries),
healthchecks.io-style. Missed and timeout are server-computed only; coerce
client-sent ones. Use the `croner` crate (5-field Vixie, timezones,
previous-occurrence search) and plain chrono math for interval schedules.
Defaults: checkin_margin 1 min, max_runtime 30 min. Pushover fires at one choke
point: incident open (error / missed / timeout) and recovery.

**Quotas and retention.** Pre-parse per-project quota gate → 429 (defaults
1k/5min, 5k/hour, 1M/month; SDKs back off on their own). Per-project max stored
events (default 10k) with Bugsink-shaped eviction: age- and volume-weighted,
first-seen and regression events never evicted. Eviction deletes bucket objects
and index rows together.

**UI.** New top-level section `/bugs`: project list with open-issue counts;
`/bugs/:project` issues by state; issue detail (events, stack, tags, resolve and
mute buttons); `/bugs/:project/logs` search; `/bugs/:project/crons`. Same
accept-header JSON convention as the rest of nashcode. Add a bugs summary to
`/brain`. Tailscale headers stamp resolve/mute actions, as everywhere.

**Crates (vetted).** `sentry-types` 0.49 for Dsn/Auth parsing; verify its
envelope `from_slice` on unknown item types with captured envelopes before
trusting it, and hand-write the ~100-line splitter if it disappoints. Keep raw
item bytes either way. `croner` 3.x (not `cron`, wrong dialect; not `saffron`,
dormant). `object_store` for S3 (endpoint override, env-chain credentials). MIT
fixtures: github.com/bugsink/event-samples.

## Phasing

1. **Core loop**: project + DSN minting, envelope ingest to bucket, digest,
   grouping, issues UI, Pushover on new issue and regression. Wire nashcode to
   its own DSN (Rust SDK, `sentry-tracing` with the `logs` feature): dogfood
   from day one.
2. **Logs**: both log doors, FTS search, code origin, retention prune, the
   ingest hardening. (Landed with slice 2.)
3. **Public ingester** (pulled forward from last place, 2026-08-19: ingestion
   must scale from day one — all agent output, app logs, and errors flow
   through it): the design in `goals/error-tracking/ingester.md`. A celld app
   on one public VPS accepts envelopes (one buffer cell per project,
   bucket-durable via celld's replication); nashcode pulls batches over iroh
   with mutually pinned EndpointIds and feeds its normal digest. nashcode
   accepts no inbound connection. nashcode side: the iroh drainer task and
   registry push. The edge scales per project; digest stays the single writer
   behind the buffer.
4. **Pushover + context capture + dogfood**: the notification queue, the
   mirror-read source snippets (SPEC "Context capture"), path suffix-matching
   for containerized apps, nashcode wired to its own DSN.
5. **Crons + quotas + retention polish + cutover**: check-ins, sweeper,
   eviction, escalation ladder, mutes evaluation; then mint new DSNs for the
   two nac-bugs apps, verify end to end, decommission the nac-bugs Fly app,
   and update the `nac-bugs-wire` skill.

Out of scope, deliberately: transactions/APM, sessions/release health, replays,
profiles, browser-JS sourcemap symbolication (the one expensive feature; keep a
Bugsink instance if a minified frontend ever needs it), minidumps, the Sentry
Web API surface (404 catch-all), email, accounts. iOS dSYM symbolication is
phase 4 if an iOS app gets wired: GlitchTip proved the shape, days of work with
the `symbolic` crates. Store raw frames now so it can be retroactive.

## Facts (acceptance)

Fixtures: bugsink/event-samples (MIT). Tests follow the repo rule: real `git`,
real HTTP, no mocks.

1. A project created in the UI shows a DSN; `sentry-python` pointed at it
   delivers an exception that appears as an issue with a readable title.
2. The envelope response is `200` with body `{"id":"..."}` and content-type
   application/json.
3. An envelope containing an unknown item type ingests without error; the known
   items in it are processed.
4. gzip, deflate, and br request bodies all ingest; an envelope over the size
   cap is rejected with 413 without buffering it fully.
5. Wrong key for a valid project id → 403. Unknown project → 404. Over quota →
   429 with Retry-After.
6. Every 200 carries the X-Sentry-Rate-Limits header suppressing transactions;
   a Python SDK with `traces_sample_rate=1.0` stops sending transactions after
   the first response.
7. A browser OPTIONS preflight to the envelope route returns the Relay header
   set; a fetch POST with `?sentry_key=` auth succeeds.
8. Raw event JSON exists as a bucket object; the SQLite row holds index fields
   only; the issue detail page renders from the bucket object.
9. `nashcode bugs reindex` on an empty DB rebuilds issues, counts, and log
   indexes from the bucket alone.
10. Two events with the same exception type and messages differing only in a
    UUID group into one issue; an explicit SDK fingerprint overrides grouping;
    `{{ default }}` extends it.
11. Resolving an issue and sending the same error again reopens it and sends a
    regression push; a second identical event sends nothing.
12. 1000 events on one unresolved issue produce at most the ladder pushes
    (10/100/1000), not 1000 pushes; the hourly cap sends one suppression notice.
13. Pushover payloads: title ≤250, message ≤1024 and never empty, url opens the
    issue page. A 429 from Pushover parks the queue until reset with no 4xx
    retries (verify against a stub server).
14. `Sentry.logger.info(...)` from a JS SDK and a curl NDJSON POST both land in
    the log store; the logs page finds them by FTS query; rows older than
    `retention_days` are pruned and the bucket archive object remains.
15. A `check_in` with a crontab `monitor_config` creates a monitor; stopping the
    job produces a missed incident and one Pushover message within
    margin + 1 minute; the next ok check-in sends recovery.
16. An in_progress check-in with no finish flips to timeout after max_runtime;
    a late ok updates duration but not the status.
17. With `NASHCODE_BUGS_BUCKET` unset, nashcode starts, prints one line about it,
    and answers 404 on `/bugs` and the ingest routes.
18. Eviction at the 10k cap removes bucket objects and index rows together and
    never removes first-seen or regression trigger events.
19. nashcode's own tracing errors arrive in its own bugs project (dogfood wiring
    active in production config).
20. SPEC.md, README.md, and AGENTS.md document the feature per repo convention.

## Done

Phases 1–2 merged with all facts passing; phase 3 live with one project per
client class (server, browser, Worker) verified end to end through the public
ingester, the nac-bugs Fly app decommissioned, Bugsink's DB archived, and the
`nac-bugs-wire` skill updated to point at nashcode.
