# Error tracking in nashgit — research digest

Compiled 2026-08-18 by a 10-agent research workflow (Exa + Firecrawl against primary sources).
Six topics, then three gap follow-ups. Facts are dated where they are version-sensitive.

## Bugsink deep dive

### Key facts
- Bugsink is a Python/Django monolith by Klaas van Schelven (Bugsink B.V., NL), actively maintained as of Aug 2026: 48 releases, latest 2.5.0 (21 July 2026), last commit 11 Aug 2026, ~1.9k stars, 40 contributors.
- License is PolyForm Shield 1.0.0 (source-available, NOT open source): free for any purpose except providing a product that competes with Bugsink; competition counts 'even when provided free of charge'; the license restricts use of Bugsink's code/software, not independent reimplementation of the Sentry protocol from public docs.
- Storage: SQLite by default (custom 'timed_sqlite_backend'), MySQL and PostgreSQL also supported; a second SQLite database is the task queue (snappea); raw event bytes land on the filesystem (INGEST_STORE_BASE_DIR) before digest; all DB writes are serialized through a single-writer 'immediate_atomic' transaction model built around SQLite's BEGIN IMMEDIATE.
- Ingestion is split ingest/digest: the HTTP view streams the (decompressed, size-capped) event to disk and enqueues a 'digest' task (snappea worker, or eager mode); stated capacity is 18 events/s of 50KB ≈ 1.5M events/day on a $5 VPS with 2 vCPU / 4GB RAM.
- Sentry protocol coverage: /api/<project_id>/envelope/, /api/<project_id>/store/ (legacy), /api/<project_id>/security/ (CSP reports), plus sentry-cli sourcemap/debug-file upload endpoints (chunk-upload, artifactbundle/assemble, difs/assemble); the envelope parser keeps ONLY items of type 'event' and 'attachment' with attachment_type 'event.minidump' (minidumps feature-flagged off by default) — transactions/spans, sessions/release health, Sentry Logs ('log' items), client reports, check-ins, and profiles are deliberately discarded; all other Sentry Web API routes 404 via a catch-all.
- DSN handling: each Project gets sentry_key = uuid4 (stored as UUIDField, rendered as 32-hex) at creation; DSN = {scheme}://{key-hex}@{host}{path}/{integer-project-id}; ingest auth accepts the key from ?sentry_key= query param, the X-Sentry-Auth header, or the 'dsn' envelope header, then looks up Project by (pk, sentry_key) and returns 403 on mismatch; the deprecated secret key is ignored; protocol version pinned at sentry_version=7.
- Compression: request bodies may be gzip, deflate, or brotli (no zstd as of Aug 2026 — a gap, since newer SDKs can send zstd); size caps mirror Sentry Relay: 1MiB event, 100MiB envelope uncompressed, 20MiB envelope compressed, 100MiB attachment, all enforced with streaming MaxDataReader/Writer to resist decompression bombs.
- Quota and retention: installation-wide and per-project rate limits (defaults 1,000/5min, 5,000/hour, 1M/month) return HTTP 429 which official SDKs treat as ~60s backoff, over-quota events dropped uncounted; retention is per-project max stored events (default 10,000) with an 'irrelevance' eviction score = log4(age+1) + leading-bits-of-event-count + randomization, so old and over-represented events go first; events tied to issue TurningPoints are never evicted.
- Issue grouping: an explicit SDK 'fingerprint' wins (mechanism-independent); otherwise v2 (default since 2.5.0) groups by exception type + value normalized with Sentry's own normalize_message_for_grouping (strips IDs, IPs, numbers); v1 grouped by 'title ⋄ transaction'; grouping keys are sha256-hashed rows in a Grouping table pointing at Issues; grouping mechanisms are versioned per-project with a 30-day transition window.
- Regression handling: a new event on a resolved issue runs release-aware issue_is_regression (supports 'resolved in next release' and per-release fixed_at markers); on regression the issue is reopened (IssueStateManager.reopen), a TurningPoint kind=REGRESSED is recorded, and a regression alert fires.
- Notifications: email (stepped user/team/project opt-in) plus webhook chat backends — Slack-compatible, Discord, Mattermost, MS Teams (merged 11 Aug 2026), Telegram, and a generic 'custom' JSON webhook; triggers are new issue, regression, and unmute; there is NO Pushover support anywhere (zero hits in code, issues, or PRs) — the only bolt-on path is the custom webhook pointed at a relay that converts JSON to Pushover's form-encoded POST.
- The /envelope/ endpoint must return JSON {"id": "<event_id>"} on 200 — Bugsink's empty-body response broke sentry-elixir v11 into a retry loop and was fixed in PR #396 (merged June 2026); nashgit should return that body from day one.
- Pricing: self-hosted is free with unlimited users/events; hosted tiers are Free (15K events/mo), $16/75K, $50/600K, $158/3M, $568/15M, $1,288/50M per month.
- The author's blog documents exactly what a minimal Sentry-compatible server needs: 'Understanding Sentry DSNs', 'Single-writer Database Architecture with SQLite', 'Snappea: A Simple Task Queue', 'Moving Event Data Out of the Database', 'Multi-process Docker Images' (monofy), 'Does it scale (down)?', 'Track Errors First' / 'You don't need APM' (the errors-only design thesis), and 'Handled Errors'.
- The separate bugsink/event-samples GitHub repo contains MIT-licensed sample Sentry event JSON (plus minidump samples) explicitly intended for testing — safe fixtures for nashgit's ingest tests.

### Recommendations
- Implement /api/{project_id}/envelope/ as the primary ingest endpoint (plus optionally the legacy /store/), accept auth via ?sentry_key= query param, X-Sentry-Auth header, AND the dsn envelope header, and return JSON {"id": "<event_id>"} on 200 — sentry-elixir and other strict SDKs break on an empty body (Bugsink PR #396).
- Mint DSNs Bugsink-style: a random 32-hex key per project plus an integer project id, format {scheme}://{key}@{host}/{project_id}; validate by (project_id, key) pair and return 403 on mismatch, 429 on quota.
- Process only envelope items of type 'event' (drop transaction/session/client_report/check_in/profile with a log line), but consider ALSO accepting 'log' items (Sentry Structured Logs) since nashgit wants server logs — that is a real gap in Bugsink.
- Support gzip, deflate, and brotli request-body decompression with streaming size caps (1MiB event, 20MiB compressed envelope, mirroring Sentry Relay); add zstd to beat Bugsink and cover newer SDKs.
- Copy the architecture, not the code: ingest writes raw bytes fast and enqueues; a single-writer digest task does parse→group→store→alert inside one transaction — this maps 1:1 onto nashgit's SQLite + tokio design and is proven at 1.5M events/day on a $5 VPS.
- Group by sha256 of (exception type + value normalized to strip IDs/IPs/numbers), let an explicit SDK 'fingerprint' override, and store the grouping-key→issue mapping in its own table; on a new event for a resolved issue, reopen it and fire a regression notification.
- Adopt simple quotas (per-project events per 5min/hour/month → 429, SDKs back off automatically) and per-project max-stored-events retention with age-weighted eviction; Bugsink's default of 10,000 stored events per project is a sane starting point.
- Build Pushover as a first-class alert backend on the three Bugsink triggers (new issue, regression, unmute) with an hourly send cap — Bugsink has zero Pushover support and no shim exists, so this is nashgit's clearest win over the current nac-bugs setup.
- Do NOT copy any Bugsink source into nashgit: it is PolyForm Shield 1.0.0 (noncompete, source-available), and the vendored 'sentry/' dirs are BSD-3-Clause; reimplement from Sentry's public develop docs (develop.sentry.dev/sdk/foundations/) instead, which the license cannot reach.
- Use the MIT-licensed bugsink/event-samples repo as ingest test fixtures (real event JSON from sentry and glitchtip codebases, plus minidump samples).
- Skip minidumps entirely (Bugsink feature-flags them off as a DoS magnet) and skip the Sentry Web API surface — a 404 catch-all for unimplemented /api/ routes is what Bugsink ships and SDKs tolerate it fine.

### Detail

# Bugsink deep dive (researched 2026-08-18)

Sources: bugsink.com docs and blog, the GitHub repo (shallow clone inspected at `/tmp/bugsink-research`), GitHub issues/PRs. Line-level claims below come from reading the source at the current `main` (last commit 2026-08-11).

## 1. Architecture

- **Language/framework**: Python + Django. Django templates + Tailwind for the UI, gunicorn as the WSGI server. One codebase, one process model: the Docker image runs `monofy` (author's own tiny process supervisor) to run gunicorn and the `snappea` background worker in a single container (`Dockerfile` CMD).
- **Storage backends**: SQLite is the default and the flagship story (`bugsink/settings/default.py` uses a custom `bugsink.timed_sqlite_backend` engine with per-query timeouts). MySQL and PostgreSQL are both supported and documented (docs/mysql, docs/postgresql; Postgres docs say "Bugsink works well with SQLite by default", Postgres is for when it fits your stack). A **second SQLite database** (`snappea.sqlite3`) is used as the message queue and the settings explicitly warn against moving it to MySQL/Postgres ("database as message queue... you probably shouldn't").
- **Single-writer model**: the blog post "Single-writer Database Architecture with SQLite" (Mar 2025) explains it: writes go through `immediate_atomic` (SQLite `BEGIN IMMEDIATE`, one global write lock), reads get snapshot isolation. Event digestion is sequential by design; this is presented as a feature (predictable, consistent state), not a limitation.
- **Event payload storage**: originally events-as-blobs in the DB; since ~1.2 ("Moving Event Data Out of the Database", Feb 2025) event JSON can live on the filesystem or object storage via `EVENT_STORAGES` (`events/storage.py`, `storage_registry.py`). Ingested-but-not-yet-digested bytes always land as files under `INGEST_STORE_BASE_DIR` (default `/tmp/bugsink/ingestion`).
- **Resource footprint / scale claims** (bugsink.com/scalable-and-reliable/): a $5/month VPS, 2 vCPU, 4GB RAM sustains **18 events/s of 50KB each = 1.5M events/day = 46M/month**. Single-server production docs repeat the 1.5M/day figure. Stress-test scripts ship with the product (`performance/` app + docs/stress-testing).

## 2. Sentry protocol coverage

Endpoints (`ingest/urls.py`, `bugsink/urls.py`):
- `POST /api/<project_id>/envelope/` — the main one (modern SDKs).
- `POST /api/<project_id>/store/` — legacy, still implemented (settings call its compressed-size cap "the deprecated store endpoint").
- `POST /api/<project_id>/security/` — browser CSP violation reports (`report-uri`), translated into events; auth via `?sentry_key=` because browsers can't set headers there.
- Minidumps: a dedicated path exists (`sentry/minidump.py` merges minidump data into an event) but is behind `FEATURE_MINIDUMPS = False` by default ("likely a DOS-magnet").
- sentry-cli support: `chunk-upload`, `artifactbundle/assemble`, `difs/assemble` endpoints exist so **sourcemap upload works** (Bugsink 1.5 "Introducing Sourcemaps").
- Everything else under `/api/` hits `api_catch_all` → 404 (optionally logged via `API_LOG_UNIMPLEMENTED_CALLS`).
- Bugsink's own management REST API ("canonical API", since 2.0) lives at `/api/canonical/0/` with auth tokens and a Swagger schema — it is Bugsink-specific, not Sentry-Web-API-compatible.

Envelope item handling (`ingest/views.py`, `_post2` factory, ~line 900): items are streamed and **only two types are kept**: `event`, and `attachment` with `attachment_type == "event.minidump"` (when the feature flag is on). Everything else — `transaction`, `session`/`sessions` (release health), `log` (Sentry Structured Logs), `client_report`, `check_in` (crons), `profile`, other attachments — is read to a NullWriter and skipped with a log line. Only **one event per envelope** is accepted; multi-event envelopes are ignored wholesale. Envelope headers validated: `dsn`, `sdk`, `sent_at`, `event_id` (`ingest/header_validators.py`).

Response shape: `/envelope/` returns `JsonResponse({"id": event_id})`. This was an empty body until PR #396 (merged 2026-06-04) — sentry-elixir v11 treated the empty body as transport failure and entered a retry/client-report loop. A wire-compat lesson for nashgit.

What works from the SDK's perspective: error events (exceptions and log-message events, incl. `capture_message` and logging integrations — grouped as "Log Message"), breadcrumbs, contexts/tags/user data, releases (auto-created on digest, `create_release_if_needed`), environments, explicit fingerprints, handled/unhandled mechanism, sourcemapped JS stacktraces, local variables. Deliberately NOT implemented: tracing/performance/APM, session/release-health stats, metrics, profiling, structured logs, cron monitoring, user feedback. The docs are explicit: "Bugsink intentionally focuses only on error events. It does not handle metrics, traces, or other event types" (docs/sdk-recommendations, linking the "Track Errors First" post). The UI's SDK snippets set `traces_sample_rate=0` and recommend `send_default_pii=True` (self-hosted, so keep the data).

## 3. DSN handling

- **Minting** (`projects/models.py`): `sentry_key = models.UUIDField(editable=False, default=uuid.uuid4)` — created automatically with the project. Project id is the integer PK ("we would prefer a uuid but the sentry clients have int baked into the DSN" — comment in the model; note Bugsink's own parser does NOT require int).
- **Format** (`compat/dsn.py: build_dsn`): `{scheme}://{sentry_key.hex}@{host}{:port}{path}/{project_id}` — no secret key, matching modern Sentry. The SDK-side URL derivation is the standard one: `{BASE_URI}/api/{PROJECT_ID}/{ENDPOINT}/`.
- **Auth on ingest** (`ingest/views.py: get_sentry_key_for_request`, ~line 242): the key is taken from, in order: `?sentry_key=` query param, `X-Sentry-Auth: Sentry sentry_key=...` header (parsed by `compat/auth.py`), or the `dsn` envelope header. Lookup: `Project.objects.get(pk=project_pk, sentry_key=sentry_key, is_deleted=False)`; failure → `PermissionDenied` (403) with a deliberately debuggable message echoing the attempted DSN. They do NOT cross-check that the DSN-in-envelope's project id matches the URL path ("reasons unconvincing"). `sentry_secret` is deprecated/ignored. `sentry_version` is 7.

## 4. Ingestion pipeline

- **Flow**: HTTP view → decompress stream (`Content-Encoding: gzip | deflate | br`; **no zstd** as of Aug 2026) wrapped in `MaxDataReader` caps → `StreamingEnvelopeParser` streams each kept item to a file on disk → `digest.delay(event_id, metadata)` enqueues a snappea task → snappea worker (4 workers default; but writes serialize anyway) runs `digest_event` inside `immediate_atomic`: parse JSON, validate (`VALIDATE_ON_DIGEST` off by default), compute grouping, create/lookup Issue, record release/tags/turning points, evict for retention, count quotas, fire alerts via `delay_on_commit`. `TASK_ALWAYS_EAGER` mode digests inline (dev/small installs).
- **Size limits** (defaults in `bugsink/app_settings.py`, "mirror the (current) values for the Sentry Relay"): `MAX_EVENT_SIZE` 1MiB, `MAX_ENVELOPE_SIZE` 100MiB, `MAX_ENVELOPE_COMPRESSED_SIZE` 20MiB, `MAX_ATTACHMENT_SIZE` 100MiB, `MAX_EVENT_COMPRESSED_SIZE` 200KiB (store endpoint only), `MAX_HEADER_SIZE` 8KiB, `MAX_CSP_REPORT_SIZE` 64KiB, `MAX_EVENT_TAGS` 100.
- **Quota** (`ingest/event_counter.py` + `QUOTA_THRESHOLDS` in views): sliding windows at Installation and Project level — defaults 1,000 per 5 min, 5,000 per hour, 1,000,000 per month (each level). Exceeded → **HTTP 429**, which official SDKs implement as ~60s backoff. Over-quota events are dropped without being counted or stored. A `next_quota_check` optimization avoids recounting on every request.
- **Retention/eviction** (`events/retention.py`, per-project `retention_max_event_count` default **10,000**, editable per project; plus optional global `MAX_RETENTION_*`, `MAX_EVENT_AGE_DAYS` settings, all off by default): each event gets an "irrelevance" score at digest = `nonzero_leading_bits(stored_event_count)` + randomization; eviction adds age-based irrelevance `log4(age_hours+1)` and deletes highest-irrelevance events first, so per-issue sampling density decays with volume and age ("smart retention" marketing page). Events referenced by TurningPoints (first seen, regressed, etc.) are marked `never_evict`. Since 2.5.0 there's also cleanup of issues whose events were all evicted.

## 5. Issue grouping and regressions

- `issues/utils.py: get_key_with_mechanism_for_data`: if the event has an explicit `fingerprint` (SDK-side), that is the grouping key ("mechanism-independent"); `{{ default }}` interpolation is supported so fingerprints can extend the default grouper.
- Default grouper, **v2** (default for new projects since 2.5.0, `issues/grouping_mechanisms/v2.py`): title from exception type + value where the value is passed through Sentry's own vendored `normalize_message_for_grouping` (strips IDs, IPs, numbers, uuids) — so `TimeoutError: request 8f3a... timed out` and its siblings group together. **v1** (`v1.py`): `title ⋄ transaction` (exception type+value, verbatim, plus the transaction name). The type/value extraction is vendored Sentry code (`sentry/at_...` directories, BSD-3-Clause, kept verbatim for fidelity). Synthetic exceptions group by crash-location function. Log-message events group as `"Log Message" + first line of message` (1024-char cap).
- Grouping keys are sha256-hashed and stored in a `Grouping` table (project_id, key, hash, mechanism) → Issue. Mechanisms are **versioned per-project** with a 30-day `GROUPING_TRANSITION_PERIOD` so upgrades don't split existing issues; project admins opt in to v2 for old projects.
- **Regressions** (`issues/regressions.py`, wired in `digest_event` ~line 586): a new event on a resolved issue triggers `issue_is_regression`, which is release-order aware: walks ordered releases tracking `fixed_at` markers vs `events_at`; supports "resolve unconditionally", "resolve by next release", and re-breaks only when the event's release is at/after a fix point. On regression: `TurningPoint(kind=REGRESSED)` recorded, `IssueStateManager.reopen(issue)` unresolves it, and `send_regression_alert` fires (if `alert_on_regression`, default true).

## 6. Notifications

- **Native channels**: (1) Email — per-user opt-in resolved through a stepped user → team → project preference chain (`alerts/tasks.py`), throttled by `MAX_EMAILS_PER_HOUR` default 60; (2) webhook "messaging backends" (`alerts/service_backends/`): **Slack-compatible incoming webhooks, Discord, Mattermost, MS Teams (merged 11 Aug 2026), Telegram, and a generic `custom` JSON webhook** that POSTs a serialized issue payload. Outbound webhooks pass SSRF protection (URL validation, DNS-rebinding pinning fixed in 2.4.0 GHSA-w589-2ffr-2prv, allow/deny lists, deny-non-global-IPs default).
- **Triggers**: new issue, regression, unmute — per-project toggles (`alert_on_new_issue`, `alert_on_regression`, `alert_on_unmute`, all default true). No per-issue thresholds beyond mute-until conditions; no digests/summaries.
- **Pushover**: **zero support**. `rg -i pushover` over the whole repo: no hits. GitHub issue/PR search for pushover, ntfy, gotify, apprise: no hits. Issue #118 ("Add Multiple Messaging Backends", open) is the umbrella for new backends and invites suggestions — Pushover has not even been requested. People who want push notifications today must point the `custom` webhook backend at a self-hosted relay that reshapes the JSON into Pushover's form-encoded `POST https://api.pushover.net/1/messages.json` (token/user/message) — Pushover does not accept arbitrary JSON webhooks, so a shim is mandatory. This is exactly the gap Matthias's nac-bugs setup papers over, and a native-Pushover nashgit tracker eliminates the shim.

## 7. License, pricing, maintenance

- **License**: **PolyForm Shield License 1.0.0** since Jan 2025 (blog "New License & Pricing"). Free for every purpose EXCEPT "providing any product that competes with the software or any product the licensor... provides using the software"; competition counts across interface kinds and "even when provided free of charge". Plus: sentry-vendored dirs are BSD-3-Clause (Sentry's), `ee/` reserved for a future Enterprise Edition license, Heroicons MIT. Implications for nashgit: (a) running Bugsink internally is a permitted purpose; (b) **copying Bugsink source into nashgit would drag PolyForm Shield terms in — don't**; (c) a from-scratch Rust implementation of the *Sentry protocol* (from Sentry's public develop docs and observed wire behavior) is outside the license's reach entirely — the license governs use of the software, not the ideas; (d) the separate `bugsink/event-samples` repo is **MIT** and explicitly meant for testing.
- **Pricing**: self-hosted **free, unlimited users and events**. Hosted (bugsink.com homepage, Aug 2026): Free 15K events/mo → $16/mo 75K → $50/mo 600K → $158/mo 3M → $568/mo 15M → $1,288/mo 50M. There's also paid self-hosted **support** for Sentry/Bugsink installs.
- **Maintenance**: very active solo-lead project. 48 releases; 2.5.0 on 2026-07-21 (versioned grouping, global issue list, admin QoL); 2.4.0 on 2026-07-10 (webhook SSRF fix, sparklines); last commit on main 2026-08-11 (MS Teams backend). ~1,928 stars, 40 contributors, 129 open issues.

## 8. Blog posts relevant to building a minimal Sentry-compatible server

- **Understanding Sentry DSNs** (bugsink.com/sentry-data-source-name/): DSN = URL endpoint + key; auto-assigned per project; SDKs read `SENTRY_DSN` env var; no DSN → SDK sends nothing.
- **Single-writer Database Architecture with SQLite** (blog/database-transactions/, Mar 2025): the transaction model — global write lock, snapshot reads, event processing as a conveyor belt; argues error-tracker ingest is the perfect single-writer workload.
- **Snappea: A Simple Task Queue for Python** (blog/snappea-design/): SQLite-as-queue + filesystem wakeup calls instead of Redis/Celery; the whole reason one container suffices.
- **Moving Event Data Out of the Database** (blog/moving-event-data-out-of-the-database/, Feb 2025): events verbatim as files, DB keeps metadata — for millions of events.
- **Django Deployment, Simplified** (blog/installation-simplification-journey/) and **Multi-process Docker Images** (blog/multi-process-docker-images/): the `monofy` single-container story.
- **Does it scale (down)?** (blog/does-it-scale-down/): the design goal of running tiny.
- **Track Errors First** (blog/track-errors-first/) and **You don't need APM** (blog/you-dont-need-application-performance-monitoring/): the thesis for errors-only scope — the justification for dropping every non-event envelope item.
- **Why I gave up on self-hosted Sentry** (blog/why-i-gave-up-on-self-hosted-sentry/): the origin story; self-hosted Sentry's ~dozen-service footprint is the anti-goal.
- **Handled Errors** (blog/handled-errors) and **Grouping Connection-Errors** (bugsink.com/sentry-fingerprint/): practical notes on the `mechanism.handled` flag and SDK-side `fingerprint` usage.
- There is no single "anatomy of the envelope" post; for the wire format the author leans on Sentry's own develop docs (develop.sentry.dev/sdk/foundations/envelopes/, .../transport/authentication/), which the code comments cite line-by-line.

## 9. What nashgit can learn (delta vs Bugsink)

Bugsink proves that an unmodified official Sentry SDK is satisfied by: one `/api/{id}/envelope/` endpoint, sentry_key auth from three places, gzip/deflate/br decompression, keeping only `type: "event"` items, and a `{"id": ...}` JSON response. Genuine gaps nashgit can beat: **native Pushover** (Bugsink has none), **zstd Content-Encoding** (Bugsink lacks it), and **accepting Sentry Structured Logs (`log` envelope items)** for the server-logs use case (Bugsink drops them; its only log story is error-level log records arriving as regular events via SDK logging integrations).

### Sources
- https://github.com/bugsink/bugsink
- https://raw.githubusercontent.com/bugsink/bugsink/main/LICENSE
- https://raw.githubusercontent.com/bugsink/bugsink/main/README.md
- Local source inspection of github.com/bugsink/bugsink @ main 2026-08-11 (ingest/views.py, ingest/header_validators.py, ingest/tasks.py, compat/dsn.py, compat/auth.py, bugsink/app_settings.py, bugsink/streams.py, bugsink/settings/default.py, issues/grouping_mechanisms/, issues/regressions.py, issues/utils.py, alerts/, events/retention.py, events/storage.py, snappea/settings.py, projects/models.py, Dockerfile, CHANGELOG.md)
- https://www.bugsink.com/
- https://www.bugsink.com/sentry-sdk-compatible/
- https://www.bugsink.com/scalable-and-reliable/
- https://www.bugsink.com/sentry-data-source-name/
- https://www.bugsink.com/docs/alerts/
- https://www.bugsink.com/docs/sdk-recommendations/
- https://www.bugsink.com/docs/ingestion-rate-limits-and-retention/
- https://www.bugsink.com/docs/postgresql/
- https://www.bugsink.com/docs/single-server-production/
- https://www.bugsink.com/blog/
- https://www.bugsink.com/blog/new-license-new-pricing/
- https://www.bugsink.com/blog/database-transactions/
- https://www.bugsink.com/blog/moving-event-data-out-of-the-database/
- https://www.bugsink.com/blog/you-dont-need-application-performance-monitoring/
- https://www.bugsink.com/blog/why-i-gave-up-on-self-hosted-sentry/
- https://www.bugsink.com/blog/installation-simplification-journey/
- https://github.com/bugsink/bugsink/issues/118
- https://github.com/bugsink/bugsink/pull/396
- https://github.com/bugsink/event-samples
- https://develop.sentry.dev/sdk/foundations/transport/authentication/
- https://develop.sentry.dev/sdk/foundations/envelopes/

## The alternatives landscape

### Key facts
- GlitchTip (MIT, Django + PostgreSQL 14+, optional Valkey) is the mainstream Bugsink alternative: Sentry-DSN compatible errors, basic transactions ('life support' per its maintainer), and uptime checks in 256-512 MB RAM; alerts are email + generic webhooks only (no Pushover); active in 2026 with v6.2.x shipping and a 7.0 branch in progress.
- Official self-hosted Sentry requires 4 CPU cores, 16 GB RAM + 16 GB swap (32 GB recommended) and ~40+ containers (Kafka, ClickHouse, Snuba, Relay, Symbolicator, Postgres, Redis), under the FSL-1.1-Apache-2.0 license (each version converts to Apache-2.0 two years after release) — disqualifying as a tailnet side-feature.
- Bugsink itself (Python/Django, single-writer SQLite, Polyform Shield license, 2.0.7 released Jan 2026) is errors-only by explicit design (ignores transactions/metrics) and alerts via email plus Slack/Mattermost/Discord webhooks — still no native Pushover.
- A 2026 wave of solo-built micro Sentry-clones proves the ingest subset is one-developer territory: errex (Rust + SQLite, AGPL-3.0, ~7 MB RAM, alpha), stackpit (Rust + SQLite, MIT core), TrapFall (Rust, Apache-2.0, 6 MB Docker image), tindra (Go + Postgres, ELv2, errors + performance + uptime + cron), urgentry (Go, FSL, Tiny mode ~52 MB), kestrel (Go, MIT, abandoned WIP) — all under ~65 stars, single-maintainer, months old: excellent references, unacceptable dependencies.
- stackpit (franzos/stackpit) is the closest existing artifact to the nashgit plan — Rust single binary, SQLite, envelope + legacy store endpoints with all auth methods, grouping, releases, logs, source maps, cron monitors, MCP endpoint — and its core is MIT, so nashgit can legally mine its implementation.
- Among observability platforms only Uptrace (AGPL-3.0, Go + ClickHouse + Postgres) officially ingests unmodified Sentry SDK traffic (beta-quality, needs a hand-tweaked DSN); SigNoz closed its Sentry-ingest issue as wontfix (OTel-only), HyperDX only bridges Sentry SDKs through its own npm OTel packages, OpenObserve has no Sentry ingest, and Highlight.io requires its own SDK and was absorbed into LaunchDarkly (April 2025).
- The sentry-types crate (MIT, published by getsentry, v0.49.1, Aug 2026, 50M+ downloads) already models the whole ingest surface nashgit needs: Dsn parsing plus Envelope::from_slice/to_writer and an EnvelopeItem enum covering Event, Transaction, SessionUpdate/Aggregates, Attachment, MonitorCheckIn, ClientReport, and LogsContainer (structured logs).
- getsentry/relay's crates (relay-event-schema, relay-protocol with its lenient Annotated<T> model) are NOT on crates.io (git-dependency only) and are FSL-1.1-Apache-2.0; relay releases older than two years have already converted to Apache-2.0 — treat relay as a reference for normalization/grouping, not a dependency.
- Sentry structured logs are first-class in the Rust SDK (logs feature + enable_logs, tracing/log integrations) and travel through the same envelope endpoint, so nashgit's 'also store server logs' requirement rides the identical ingest path with zero custom client code.
- No tool in the entire landscape ships native Pushover notifications — everything is email plus Slack-shaped JSON webhooks (Pushover needs a form-encoded POST with token/user, which generic JSON webhooks cannot produce without a bridge) — so Pushover-only alerting is a genuine nashgit differentiator that cannot be bought off the shelf.
- The Sentry ingest protocol subset that matters is small and stable: POST /api/<project_id>/envelope/ (plus legacy /store/), X-Sentry-Auth header or sentry_key query param, gzip/deflate bodies, numeric project IDs in the DSN path — fully documented at develop.sentry.dev (envelopes + event payloads), with relay's mini-sentry test server as a minimal reference implementation.
- The feature cliff every micro-tracker hits is browser JavaScript sourcemap symbolication plus the sentry-cli upload API — Bugsink and stackpit have it, errex does not; server-side Rust/Python/Ruby projects never need it.
- Non-Sentry-protocol trackers are irrelevant to a DSN-swap strategy: Errbit (Ruby + MongoDB, MIT, Airbrake protocol, 4.3K stars but slow maintenance) and errorpush (Rollbar protocol); Telebugs (Ruby, $299 one-time, errors-only Sentry ingest, email/push/webhooks) is the only other commercial small player.
- The landscape verdict: embedding an errors+logs-only Sentry-compatible ingest into nashgit is exactly the scope multiple solo developers shipped in 2026, and sentry-types removes the riskiest part (protocol modeling); keep Bugsink running only as a transition crutch and for any future minified-JS sourcemap need.

### Recommendations
- Embed the tracker in nashgit: build an errors+logs-only Sentry-compatible ingest on the sentry-types crate (MIT, getsentry) — use Dsn for minted per-project DSNs (keep project IDs numeric) and Envelope::from_slice for ingest; store raw item JSON in SQLite and deserialize into typed structs opportunistically so exotic-SDK payloads never bounce.
- Implement the small protocol surface only: POST /api/<project_id>/envelope/ plus legacy /store/, X-Sentry-Auth header and sentry_key query auth, gzip/deflate body decoding; accept-and-discard (but count) Transaction/Session/ClientReport items so default SDK configs (tracesSampleRate etc.) work without errors.
- Ship server-log storage via the same pipe: enable the Sentry Rust SDK logs feature (enable_logs + tracing/log integrations) in client projects so logs arrive as LogsContainer envelope items — no custom log-shipping protocol needed.
- Write the Pushover notifier natively (reqwest form-POST to api.pushover.net/1/messages.json) and copy Bugsink's alert-state model — notify on new issue, regression, and unmute only, never per-event — with per-project cooldowns.
- Mine, don't depend: read stackpit (MIT, Rust+SQLite, envelope+store+grouping+logs — the closest existing artifact to this plan) for implementation patterns, relay's relay-event-schema/relay-event-normalization for grouping/fingerprinting logic (FSL; tags older than two years are already Apache-2.0), and relay's mini-sentry test server plus develop.sentry.dev's Envelopes and Event Payloads pages as the spec.
- Punt browser-JS sourcemap symbolication; it is the one genuinely expensive feature (sentry-cli chunk-upload API + resolution) and irrelevant for server-side Rust/Python projects — if a minified-frontend project needs it later, that is the sole reason to keep a Bugsink instance around.
- Migrate incrementally: keep nac-bugs.fly.dev running and swap one project's DSN at a time into nashgit (update the nac-bugs-wire skill afterward); the DSN swap is reversible per project, so the decision carries almost no lock-in risk.
- Do not adopt any 2026 micro-tracker (errex/TrapFall/tindra/urgentry) as infrastructure — all are single-maintainer alphas — and do not stand up GlitchTip (Postgres + Django + email ops for features nashgit does not need); also note errex is AGPL-3.0, so avoid copying its code verbatim unless comfortable with AGPL terms.
- Guard nashgit's core: bound the ingest path (size caps per envelope, bounded ingest queue, single-writer SQLite discipline) so a misbehaving SDK cannot degrade the git server sharing the process.

### Detail

# Alternatives to Bugsink: the self-hosted error-tracker landscape (as of 2026-08-18)

## 1. The heavyweights

### Official self-hosted Sentry — disqualifying
- **Language/storage:** Python/Rust monorepo; Postgres + Redis + Kafka + ClickHouse (Snuba) + Symbolicator + Relay; ~40+ containers via Docker Compose.
- **Requirements (official, develop.sentry.dev):** minimum **4 CPU cores, 16 GB RAM + 16 GB swap** (32 GB recommended), Docker 19.03.6+ / Compose 2.32.2+. Community reports of 100% CPU/RAM at 8 GB are routine; one operator ran a t3.xlarge + 700 GB EBS and called it "my least favorite" thing to run.
- **License:** **FSL-1.1-Apache-2.0** (Functional Source License) for downloads after Nov 17, 2023 (BSL before that). Fair Source, not open source: free to self-host for internal use, forbidden to offer as a competing product; **each version converts to Apache-2.0 two years after its release**.
- **Protocol:** the reference — everything (errors, tracing, profiling, replay, cron, logs, metrics).
- **Notifications:** email, Slack, PagerDuty, 50+ integrations. No Pushover.
- **Verdict:** two orders of magnitude too heavy for a tailnet-resident feature inside a SQLite web server.

### GlitchTip — the mainstream lightweight choice
- **Language/storage:** Python (Django) + Angular frontend; **PostgreSQL 14+** required, Valkey/Redis optional. MIT licensed.
- **Footprint:** 512 MB RAM recommended, 256 MB minimum (128 MB + swap with care); ~30 GB disk per 1M events/month; 1–3 containers.
- **Protocol subset:** Sentry-DSN drop-in for **error events**, plus **basic performance transactions** — which the founder describes in issue #70 as kept "on life support" (no detail view, weak filtering). Built-in uptime monitoring. No session replay, no profiling.
- **Notifications:** email + webhooks (Slack/Discord/Rocket.Chat-style and generic JSON, with recent MRs adding customizable metadata fields). **No Pushover.**
- **Maintenance (2026):** healthy — v6.2.6 tagged, Docker image (5M+ pulls) updated within the last two weeks, active 7.0 branch (squashed migrations, single-process mode, dropping legacy event shapes). Paid hosted option funds it.
- **For nashgit:** proof that Postgres + Django + email is the "standard" shape — i.e., everything nashgit is trying not to run. Nothing to reuse (Python), but its scoping (errors + uptime, transactions reluctantly) is instructive.

### Bugsink — the incumbent (what nac-bugs.fly.dev runs)
- **Language/storage:** Python/Django; **SQLite by default** with a deliberate single-writer architecture (their blog documents it well — directly relevant prior art for a rusqlite design); MySQL/Postgres optional. Single container.
- **License:** **Polyform Shield** (source-available; free to self-host for any non-competing use) — changed from earlier terms, announced on their blog with a hosted option.
- **Protocol subset:** **errors only, by explicit philosophy** ("You don't need APM", "Track errors first") — transactions/metrics/traces are intentionally out of scope. Sourcemaps supported; minidumps experimental behind a feature flag in 2.0.7 (flagged as DoS-risky pre-security-review).
- **Notifications:** email (user/team/project-stepped preferences), webhooks for **Slack (1.6, Jun 2025), Mattermost + Discord (2.0.7, Jan 2026)**, with an SSRF-aware outbound policy. **No Pushover** — your Pushover flow is external to Bugsink.
- **Maintenance (2026):** active, ~2K stars, 2.0.7 released January 2026, single maintainer (Klaas van Schelven) with a commercial model behind it.

## 2. The 2026 micro-tracker wave (Sentry-DSN-compatible minimal servers)

A GitHub `topic:sentry-alternative` sweep plus targeted searches shows a genuine explosion of tiny Sentry-protocol servers, almost all created in 2026, almost all single-maintainer:

| Project | Lang/storage | License | Protocol subset | Alerts | Footprint | Status |
|---|---|---|---|---|---|---|
| **errex** (TheHoltz) | Rust + SQLite, SvelteKit SPA embedded, one binary | **AGPL-3.0** | envelope ingest, grouping, regressions; **no sourcemaps, no multi-tenant yet** | Slack/Discord/Teams | **7 MB idle / 10.5 MB at 7,500 events/s**, 5 MB binary; survived a 96 MB cgroup at 4k RPS | alpha, 8-9 stars, created Apr 2026, 432 tests |
| **stackpit** (franzos) | Rust + SQLite (Postgres optional), server-rendered UI, one binary | **MIT core** (only a Prometheus /metrics endpoint is commercially gated) | **envelope + legacy store, all auth methods**, grouping/regressions, releases + crash-free rates, transactions/spans/Web Vitals, **logs**, cron monitors, **sourcemaps via sentry-cli**, replay storage, JSON API, OIDC, **MCP endpoint**, migrate-in from Sentry | email (SMTP/Postmark/SendGrid), Slack, webhooks, digests, thresholds | single binary, single SQLite file | brand new, ~0 stars, 1 contributor |
| **TrapFall** (codecoradev) | Rust (axum) + SvelteKit 5; SQLite or Postgres | Apache-2.0 | envelope ingest, multi-project DSNs, issue search/filters, DSN rotation | webhooks | **6 MB Docker image** | created Jun 2026, 4 stars, pushed Aug 2026 |
| **tindra** (blendbyte) | **Go + PostgreSQL**, one binary | **ELv2** (Elastic License) | "every Sentry SDK": errors, transactions with span waterfalls + p50-p99, cron monitors (Sentry/Oh Dear/Spatie check-ins), uptime probing, releases, **server-side sourcemaps** | **email, Slack, Discord, Teams, webhooks** with filters/thresholds/cooldowns | one binary + one Postgres | pushed 2026-08-17, CI + codecov, 4 stars |
| **urgentry** (Wraxle LLC) | Go; Tiny mode = one binary + SQLite; scaled mode = Postgres + MinIO + Valkey + NATS | **FSL-1.1-ALv2** (fair source) | DSN drop-in, errors-first; benchmarks itself vs Sentry 26.3.1 (Tiny: 400 eps at 52 MB peak vs Sentry's 8.2 GB) | (product-managed) | 52 MB peak Tiny mode | 63 stars, v0.2.12 May 2026, commercial ambitions |
| **kestrel** (wearzdk) | Go + SQLite, MCP-native | MIT | errors for AI agents, <20 MB | — | tiny | WIP, created and last pushed the same day (May 2026) — dead |
| **proof** (scr34m) | Go | none stated | minimal Sentry drop-in "for development/local use" | — | tiny | 8 stars, touched Feb 2026, dev-tool only |
| **sveltry**, error-mom, sentro, crashlens, airbag, etc. | Bun/TS/Go/Python | misc | zero-to-one-star 2026 vibecoded experiments | — | — | noise, listed for completeness |
| **temps** (gotempsh) | Rust, Apache-2.0, 648 stars, very active | Apache-2.0 | an entire self-hosted PaaS ("Vercel + Sentry + PostHog + Pingdom + Resend + E2B" in one binary) with error tracking as one module; Sentry-SDK compatibility **not verified** | — | one large binary | active but wrong shape (platform, not component) |

**What this wave proves:** (a) an errors-focused Sentry-compatible ingest + grouping + dashboard is demonstrably a **one-developer, few-months project in Rust or Go on SQLite** — at least five people shipped one in 2026 alone; (b) **none of them is a safe dependency** — the mature one (Bugsink) is the thing being replaced, and everything else is alpha with a bus factor of one. The correct way to consume this wave is as **reference implementations**, and stackpit's MIT core makes it legally minable Rust.

### Telebugs (commercial small player)
Ruby (by Kyrylo Silin, ex-Airbrake maintainer). One-time **$299** license, self-hosted only, single Docker command. Accepts **Sentry SDK error events** (errors only — keep SDK, swap DSN). Notifications: email, push, and webhooks with Slack/Discord/custom JSON templates, alert rules/cooldowns/spike detection. Actively developed (1.16.0 added a REST API). A JSON-template webhook still cannot hit Pushover's form-encoded API directly.

## 3. Observability platforms: do they accept Sentry-protocol ingest?

- **Uptrace — yes, uniquely.** Official docs page "Sentry SDK Configuration for Uptrace": point unmodified Sentry SDKs at a hand-modified Uptrace DSN (append a fake numeric project id, drop the `?grpc=4317` param). Explicitly labeled "a new feature that will be improved based on demand." Self-hosted Uptrace is Go + **ClickHouse + PostgreSQL**, **AGPL-3.0** (community edition), 4.3K stars, active. Sentry ingest is a compatibility shim on an OTel-first store, and the infra weight (ClickHouse) rules it out for the tailnet.
- **SigNoz — no.** Issue #8243 "Sentry Issues" closed **wontfix** (Jul 2025). OTel-only by principle; exceptions come via OTel SDKs.
- **HyperDX — no direct ingest.** OTel-native (ClickHouse). It bridges Sentry SDKs *client-side* via its own npm packages (`@hyperdx/instrumentation-sentry-node`, `@hyperdx/instrumentation-exception`, plus a 2024 patch to tolerate the `X-Sentry-Auth` header) — you install HyperDX's SDK, not point a Sentry DSN at it.
- **OpenObserve — no.** OTLP + its own RUM SDK; no Sentry-protocol endpoint found.
- **Highlight.io — no.** Requires its own (OTel-based) SDKs; heavy self-host stack; acquired by **LaunchDarkly (announced April 2025)**, now "LaunchDarkly Observability" — self-hosted future uncertain.

## 4. Non-Sentry-protocol error trackers (brief)

- **Errbit** — Ruby + MongoDB, MIT, **Airbrake-protocol**, 4.3K stars, alive since 2010 but slow-moving. Wrong protocol, wrong stack.
- **errorpush** — Python, MIT, minimalist **Rollbar-protocol** collector (391 stars). Wrong protocol.
- Everything else notable (AppEnlight, Opbeat) is dead.

## 5. What a Rust implementation can reuse — the load-bearing section

### sentry-types (crates.io, MIT, published by getsentry) — the answer
Version **0.49.1 (Aug 3, 2026)**, 50M+ total downloads, actively maintained as part of `getsentry/sentry-rust`. Its `protocol::v7` module IS the wire format:
- **`Dsn`** — parse/validate/format DSNs (mint-a-DSN-per-project comes free).
- **`Envelope`** — verified in source (`sentry-types/src/protocol/envelope.rs`): `Envelope::from_slice(&[u8])` (ingest parsing), `from_path`, `to_writer` (round-trip), plus a raw-items iterator.
- **`EnvelopeItem`** enum — `Event`, `Transaction`, `SessionUpdate`, `SessionAggregates`, `Attachment`, `MonitorCheckIn`, `ClientReport`, and **`LogsContainer`/`MetricsContainer`** (Sentry structured logs and trace metrics — the new item types).
- Full typed `Event` (exceptions, stacktraces, breadcrumbs, contexts, user, tags) with an `other` catch-all map for unknown fields.

**Caveat:** sentry-types is the *SDK-side* strict serde model; a malformed field from some exotic SDK can fail deserialization where Sentry's own pipeline would tolerate it. The robust pattern: split the envelope into raw items with sentry-types, **store the raw JSON payload in SQLite**, and deserialize into typed structs opportunistically (falling back to `serde_json::Value`) for grouping/display.

### getsentry/relay — reference, not dependency
- Rust, the production ingest edge of Sentry. Crates: `relay-event-schema` (full event schema with the lenient `Annotated<T>` model that never rejects malformed data), `relay-event-normalization` (grouping-relevant normalization), `relay-protocol`.
- **Not on crates.io** — `relay-event-schema` "does not exist" per the crates.io API (a stale docs.rs 0.0.1 entry points at an unrelated repo — squat/deleted). Reuse means a git dependency on a large workspace.
- **License: FSL-1.1-Apache-2.0** — fine for internal self-hosted use, and **any relay release older than two years has already converted to Apache-2.0** (so ~mid-2024 tags are Apache-2.0 today). Best used to crib normalization and fingerprinting logic, and its `mini-sentry` Python test server is a minimal reference for the ingest endpoints.

### Protocol surface to implement (small and documented)
- `POST /api/<project_id>/envelope/` (modern; all current SDKs) and optionally legacy `POST /api/<project_id>/store/`.
- Auth: `X-Sentry-Auth` header (`sentry_key=...`) or `?sentry_key=` query param. Bodies arrive gzip/deflate-compressed. Keep project IDs numeric in minted DSNs (some SDKs validate).
- Specs: develop.sentry.dev — Envelopes, Event Payloads, and the self-hosted docs.
- **Logs:** Sentry Rust SDK ships structured logs (`logs` feature, `enable_logs`, `log`/`tracing` integrations) through the **same envelope endpoint** as `LogsContainer` items — nashgit's "store server logs" requirement needs zero custom client protocol.
- **The feature cliff:** browser-JS sourcemap symbolication + the sentry-cli chunk-upload API. Bugsink, stackpit, and tindra do it; errex doesn't. Server-side Rust/Python/Ruby stack traces don't need it.

### Pushover
No surveyed tool has native Pushover. All webhook systems emit Slack-shaped JSON; Pushover's `POST https://api.pushover.net/1/messages.json` wants form-encoded `token`/`user`/`message`, so even "custom webhook" features need a bridge. In nashgit it is ~20 lines of reqwest, and Bugsink's alert-state model (notify on new issue, regression, unmute — never per-event) is the correct throttling design to copy.

## 6. Honest assessment: embed in nashgit vs keep Bugsink

**The landscape argues for embedding, with eyes open.**

For embedding:
1. **The scope is proven solo-sized.** Five separate people shipped Rust/Go + SQLite Sentry-compatible trackers in 2026; errex holds 7,500 events/s in 10 MB RAM. Errors + logs + grouping + Pushover is smaller than what any of them built.
2. **The riskiest part is already a maintained MIT crate.** sentry-types gives DSN + envelope parsing + the full event model, maintained by Sentry itself and guaranteed round-trip-compatible with the SDKs. No other ecosystem (Go included) gets the vendor's own protocol types for free.
3. **Nothing off the shelf fits nashgit's actual constraints.** Tailnet identity headers, no auth, loopback bind, one SQLite file, Pushover-only: GlitchTip drags in Postgres + Django + email; Bugsink stays a separate Django deployment outside the tailnet on fly.dev; the micro-trackers are alphas with their own auth/UI opinions; Uptrace drags in ClickHouse. Pushover-only alerting exists nowhere.
4. **Bugsink's own philosophy endorses the plan.** Its errors-only scoping and single-writer SQLite architecture are exactly nashgit's shape — the difference is Python vs Rust and a separate box vs the server you already run.

Against (what keeping Bugsink buys):
1. **Maturity:** grouping quality, retention/eviction, SDK-quirk tolerance, and sourcemap support represent years of accumulated fixes; v1 nashgit grouping will be cruder.
2. **Sourcemaps:** if a minified-browser-JS project ever matters, that is the one feature genuinely expensive to rebuild.
3. **Blast radius:** the error tracker moves inside the same process/DB as nashgit itself — an ingest bug can now take down the git server (mitigate with bounded ingest buffers à la errex, and remember a tailnet-only ingest surface mostly neutralizes the abuse concern that public trackers must engineer for).

**Net:** absorb it. Build errors+logs-only ingest on sentry-types, native Pushover, accept-and-count-but-drop transactions/sessions so default SDK configs don't break, and keep nac-bugs.fly.dev alive only until each project's DSN is swapped — migration is literally one DSN change per project, in both directions, which also makes the decision cheaply reversible.

### Sources
- https://develop.sentry.dev/self-hosted/
- https://github.com/getsentry/sentry/blob/master/LICENSE.md
- https://fsl.software/
- https://www.sentry.help/en/articles/13964953-what-terms-govern-my-use-of-sentry
- https://glitchtip.com/documentation/install
- https://glitchtip.com/documentation/performance
- https://glitchtip.com/blog/2021-07-09-glitchtip-1-7
- https://gitlab.com/glitchtip/glitchtip/-/issues/70
- https://hub.docker.com/r/glitchtip/glitchtip
- https://www.bugsink.com/blog/new-license-new-pricing/
- https://www.bugsink.com/docs/alerts/
- https://www.bugsink.com/docs/sdk-recommendations/
- https://www.bugsink.com/docs/webhook-outbound-policy/
- https://github.com/bugsink/bugsink/releases/tag/2.0.7
- https://www.bugsink.com/blog/database-transactions/
- https://www.bugsink.com/blog/glitchtip-vs-sentry-vs-bugsink/
- https://github.com/TheHoltz/errex
- https://github.com/franzos/stackpit
- https://github.com/codecoradev/trapfall
- https://github.com/blendbyte/tindra
- https://github.com/urgentry/urgentry
- https://urgentry.com/docs/getting-started/
- https://github.com/wearzdk/kestrel
- https://github.com/scr34m/proof
- https://github.com/hauxir/errorpush
- https://github.com/gotempsh/temps
- https://telebugs.com/
- https://telebugs.com/sentry-sdk-compatible
- https://docs.telebugs.com/notifications-00.html
- https://errbit.com/
- https://github.com/errbit/errbit
- https://uptrace.dev/ingest/sentry
- https://github.com/uptrace/uptrace
- https://github.com/SigNoz/signoz/issues/8243
- https://www.hyperdx.io/docs/install/browser
- https://github.com/hyperdxio/hyperdx/pull/473
- https://registry.npmjs.org/%40hyperdx%2Finstrumentation-sentry-node
- https://launchdarkly.com/blog/welcome-highlight-to-launchdarkly/
- https://crates.io/crates/sentry-types
- https://docs.rs/sentry-types/latest/sentry_types/protocol/v7/index.html
- https://github.com/getsentry/sentry-rust/blob/master/sentry-types/src/protocol/envelope.rs
- https://github.com/getsentry/relay
- https://github.com/getsentry/relay/blob/master/LICENSE.md
- https://docs.sentry.io/platforms/rust/logs/
- https://selfhosting.sh/apps/glitchtip/
- https://selfhosting.sh/compare/glitchtip-vs-sentry/

## The Sentry ingestion protocol

### Key facts
- A DSN parses as '{PROTOCOL}://{PUBLIC_KEY}:{SECRET_KEY}@{HOST}{PATH}/{PROJECT_ID}'; the secret key is optional and effectively deprecated, and the ingest URL is '{PROTOCOL}://{HOST}{PATH}/api/{PROJECT_ID}/{ENDPOINT}/' (verified develop.sentry.dev, 2026-08).
- The only endpoint nashgit must implement is POST /api/<project_id>/envelope/ — all five target SDKs (current Python, JS/Bun, Rust, Ruby, Swift) send everything (events, transactions, sessions, logs, check-ins, client reports) through it; /store/ is deprecated and only pre-2020 SDKs use it.
- Auth arrives three ways and the server must accept any one: X-Sentry-Auth header (server SDKs), ?sentry_key=...&sentry_version=7 query string (browser JS, to avoid CORS preflight), or a 'dsn' key in the envelope's first JSON header line; sentry_version is 7.
- An envelope is: one JSON-object header line, then repeated (item-header JSON line + payload) pairs separated by \n; the item header's 'length' gives payload bytes (if absent, payload runs to the next newline), and servers MUST skip-and-retain items of unknown type, never reject them.
- Relay accepts content-encodings gzip, deflate, br, and zstd; sentry-python defaults to gzip -9 but silently switches its default to brotli when the 'brotli' module is importable, so the server needs at least gzip+deflate+br (zstd for completeness).
- On success Relay returns 200 with JSON {"id": "<event_id>"} (id omitted when the envelope had none); an empty 200 body breaks stricter SDKs — sentry-elixir v11+ treats it as a transport failure and retry-loops (Bugsink shipped exactly this fix, PR #396, June 2026).
- Rate limiting: 429 + Retry-After, plus X-Sentry-Rate-Limits: '<seconds>:<cat1;cat2>:<scope>:...' which may be sent on ANY response including 200 — so nashgit can proactively return e.g. '86400:transaction;span;profile;replay:project' on every 200 and compliant SDKs stop sending those categories for that window (they re-send after expiry, so keep emitting the header).
- Rate-limit categories are NOT item types: error events = 'error'/'default', transactions = 'transaction', logs = 'log_item' (+ 'log_byte'), check-ins = 'monitor', sessions = 'session'; an empty category list means all categories, and client_report is 'internal' (never rate-limited explicitly).
- Event payload: only event_id (32-char lowercase hex uuid4, no dashes), timestamp (RFC 3339 string or Unix seconds number), and platform are required; level/logger/exception/message/tags/extra/contexts/sdk/release/environment/user/breadcrumbs are optional, and the server is expected to tolerate non-canonical historical formats — store raw JSON, parse only what you index.
- Sentry Logs (stable spec v2.2.0, 2026-06-22) flow through the SAME DSN and envelope endpoint as a single 'log' item per envelope with headers {type:"log", item_count:N, content_type:"application/vnd.sentry.items.log+json"} and payload {"items":[...]}; each log entry requires timestamp (Unix seconds), trace_id, level (trace|debug|info|warn|error|fatal), body, with optional severity_number and typed attributes ({value, type}).
- Logs SDK support (verified docs.sentry.io, Aug 2026): JS >= 9.41.0 (enableLogs: true, Sentry.logger.*), Python >= 2.35.0 (sentry_sdk.logger.*), Ruby >= 5.24.0 (enable_logs, Sentry.logger), Rust >= 0.42.0 (logger_info! macros, on by default via 'logs' feature, plus tracing/log forwarding), Cocoa >= 8.55.0 (stable in 9.0.0); spec v2.0.0 (2026-04-09) flipped enableLogs default to true; SDKs batch <= 100 logs per envelope, flushing at 100 items or 5 s.
- Check-ins are 'check_in' items (max one per envelope, 100 KiB cap): {check_in_id, monitor_slug, status: in_progress|ok|error, duration?, monitor_config?{schedule,...}} — trivially storable and genuinely useful for cron monitoring with Pushover alerts.
- Client reports are 'client_report' items ({timestamp?, discarded_events:[{reason, category, quantity}]}, 4 KiB cap) that SDKs send piggybacked on other envelopes; accept and either discard or store as counters — note that once nashgit 429s transactions, SDKs will report ratelimit_backoff outcomes here.
- The sentry-types crate (crates.io, v0.49.1, same version train as the Rust SDK) gives a reusable server-side parser: sentry_types::protocol::v7::Envelope::from_slice(&[u8]) plus EnvelopeItem {Event, Transaction, SessionUpdate, SessionAggregates, Attachment, MonitorCheckIn, ClientReport, ItemContainer (logs/metrics), Raw} and Dsn/Auth types; the relay-* crates (relay-event-schema etc.) are NOT published on crates.io — usable only as git dependencies from getsentry/relay.
- Relay's reference implementation lives in getsentry/relay: relay-server/src/endpoints/envelope.rs (auth extraction incl. the fallback that reads the DSN from the envelope's first line), relay-server/src/envelope.rs (item parsing), relay-event-schema/src/protocol (full event schema, rustdoc at getsentry.github.io/relay); envelope size limits: 200 MiB decompressed total, 1 MiB per event/log item, 100 KiB per check-in, 4 KiB per client report.

### Recommendations
- Implement exactly one ingest route: POST /api/<project_id>/envelope/ (trailing slash), plus the pipeline decompress (gzip, deflate, br, zstd) -> split envelope -> auth -> store; skip /store/ entirely — none of the five target SDKs use it.
- Accept auth from all three sources in priority order — X-Sentry-Auth header, ?sentry_key= query param (browser/Bun JS uses this), envelope 'dsn' header — and map the public key to a nashgit project, 403 when absent everywhere; use numeric project ids in minted DSNs to stay compatible with strict SDK parsers.
- Return 200 with content-type application/json and body {"id":"<event_id_hex32>"} (or {}), never an empty body — the empty-body bug bit Bugsink (PR #396) via sentry-elixir's retry loop.
- Write the envelope splitter by hand (~100 lines: JSON header line, then per-item header + length-or-newline-delimited payload) and keep every item's raw bytes; use the sentry-types crate (0.49.x) for Dsn/Auth parsing and optionally for typed Event/Log deserialization, but verify its from_slice behavior on unknown item types with real captured envelopes before relying on it; treat getsentry/relay as reference code only (its crates are not on crates.io).
- Never 400 on unknown item types — skip them; this is the number-one interop rule in the spec and the historical source of broken servers.
- Suppress telemetry you do not store by emitting X-Sentry-Rate-Limits on every 200 (e.g. '86400:transaction;span;profile;profile_chunk;replay;trace_metric:project:unwanted') instead of silently discarding — SDKs then stop sending client-side; keep emitting it since limits expire; never include error, default, log_item, monitor, or session in that list, and never use an empty category list (it would also kill logs and errors).
- Parse-and-index only the minimal event fields (event_id, timestamp both formats, level, release, environment, platform, exception type/value + top in-app frames for the fingerprint, message/logentry fallback) and store the full raw JSON for the detail view — the spec explicitly blesses lenient servers.
- Store 'log' items as first-class rows (timestamp, trace_id, level, severity_number, body, attributes JSON) keyed to the project — logs arrive on the same DSN/endpoint, so nashgit's own server logs can use the same store either via its own DSN or by direct insert; support means JS >=9.41, Python >=2.35, Ruby >=5.24, Rust >=0.42, Cocoa >=9.0, so all five stacks can send logs today.
- Store check_in items (tiny table: check_in_id, monitor_slug, status, duration, monitor_config) — they enable cron monitoring with Pushover 'missed run / failed run' alerts, which fits nashgit's notification model perfectly; accept client_report items and either aggregate to counters or drop them silently.
- Wire Pushover on: first event of a new fingerprint (new issue), regression (event on resolved issue), check_in status=error, and missed check-in sweep; do not notify per-event.
- Acceptance test with real SDKs against a dev instance: sentry-python with and without the brotli package installed (its default content-encoding flips to br when brotli is importable), @sentry/bun, sentry (Rust crate), sentry-ruby in a Rails app, sentry-cocoa — plus 'sentry-cli send-envelope' and hand-built adversarial envelopes (implicit length, no trailing newline, unknown item types, empty envelope, mismatched event_id).

### Detail

# Sentry ingestion wire protocol — what a minimal server must implement

All facts verified against develop.sentry.dev (the SDK development docs, reorganized in 2025/26 under `/sdk/foundations/` and `/sdk/telemetry/`), the getsentry/relay source, docs.rs, and current SDK sources. Dates noted where version-sensitive. Research date: 2026-08-18.

## 1. DSN anatomy

DSN (Data Source Name — the one config string an SDK needs):

```
{PROTOCOL}://{PUBLIC_KEY}:{SECRET_KEY}@{HOST}{PATH}/{PROJECT_ID}
```

- `PROTOCOL`: http or https.
- `PUBLIC_KEY`: opaque string, acts as the credential. nashgit mints one per project.
- `SECRET_KEY`: optional, "effectively deprecated"; DSN parsing must not require it; future Sentry versions ignore it entirely. Do not mint one.
- `HOST[:PORT]`: the ingest host. For nashgit: the tailnet hostname.
- `PATH`: optional URL prefix before the project id (`https://key@host/prefix/42` → base URI `https://host/prefix`). SDKs support it, so nashgit could mount ingest under a subpath; empty path is simplest.
- `PROJECT_ID`: the last path segment. Type String per the docs (Sentry uses integers; nashgit can use any slug-safe string, but numeric is the conservative choice — some SDK DSN parsers have historically validated it as an integer).

URL construction (what SDKs do, verified in sentry-javascript `packages/core/src/api.ts`):

```
{BASE_URI} = {PROTOCOL}://{HOST}{PATH}
POST {BASE_URI}/api/{PROJECT_ID}/{ENDPOINT}/
```

Endpoints that exist: `/envelope/` (everything), `/minidump/`, `/unreal/`, `/playstation/`, `/security/` (browser CSP reports). **Only `/envelope/` matters for the five target SDKs.**

Source: https://develop.sentry.dev/sdk/foundations/transport/authentication/

## 2. Endpoints

### POST /api/<project_id>/envelope/ — the only one you need

Trailing slash included; route it exactly (tolerating the slash-less variant costs nothing). `content-type: application/x-sentry-envelope` is the canonical type but it is **implied if missing**, and `text/plain`, `multipart/form-data`, `application/x-www-form-urlencoded` must be treated identically (CORS-preflight avoidance).

### Legacy POST /api/<project_id>/store/

Officially deprecated ("Sending event payloads to the /store/ API endpoint is deprecated"). Current Python (2.x), JS (9/10.x), Rust (0.4x), Ruby (5.x), and Cocoa (8/9.x) SDKs never call it — sentry-python 2.x's transport.py contains no store path at all; JS core only builds `/envelope/`. Only raven-era / pre-2020 SDKs use `/store/` (JSON event body, possibly zlib+base64). Bugsink and GlitchTip implement it for old-client compatibility; nashgit does not need it.

### Authentication — three interchangeable mechanisms

1. **`X-Sentry-Auth` header** (server SDKs: Python, Ruby, Rust, Cocoa):
   ```
   X-Sentry-Auth: Sentry sentry_version=7, sentry_client=sentry.python/2.35.0, sentry_key=<public_key>[, sentry_secret=<secret>]
   ```
   Required fields: `sentry_key`, `sentry_version` (=7). `sentry_client` recommended. `sentry_timestamp` and `sentry_secret` deprecated/ignored. Note the header value format is `Sentry key=value, key=value` (comma-space separated, may fold whitespace).
2. **Query string** (browser JS, to avoid CORS preflight — verified in JS SDK source): `?sentry_version=7&sentry_key=<public_key>&sentry_client=sentry.javascript.browser/9.41.0`.
3. **Envelope header `dsn` key**: the full DSN in the envelope's first JSON line self-authenticates the request (requires Relay >= 21.6.0 server-side). Relay's endpoint code shows the fallback: if header/query auth is missing, it reads the first line of the body and takes the DSN from there. Cocoa also sends the `dsn` envelope header *in addition* to header auth.

Relay validates that multiple auth sources match, and rejects with **403 Forbidden** when all are missing. For nashgit on a no-auth tailnet the pragmatic rule: extract `sentry_key` from whichever source is present (header → query → envelope `dsn` header), map it to a project, verify it matches the `project_id` in the URL, 403/404 on mismatch or unknown key.

## 3. Envelope serialization format

Grammar (exact, from the spec):

```
Envelope = Headers { "\n" Item } [ "\n" ] ;
Item     = Headers "\n" Payload ;
```

- Newlines are `\n` (ASCII 10) only. `\r` before `\n` belongs to the previous payload.
- Headers are one line of UTF-8 JSON (an object). Unknown attributes must be retained. `{}` is valid. Envelope header line is required but may be empty.
- **Envelope headers**: `event_id` (required when an event/transaction item is present; if it mismatches the payload's event_id, the envelope header wins), `dsn` (recommended), `sdk` (recommended, same shape as the event `sdk` interface), `sent_at` (recommended, RFC 3339 UTC — used upstream for clock-drift correction; store raw, ignore for correctness).
- **Item headers**: `type` (required, string), `length` (recommended, payload bytes). If `length` is absent, the payload runs to the next `\n` (sessions commonly omit it for compression). Length-prefixed payloads must terminate with `\n` or EOF; EOF before `length` bytes = malformed. Attachments add `content_type`, `filename`; log items add `item_count` and `content_type`.
- Multiple items per envelope; empty envelopes (headers only) are valid and discardable.
- **Servers MUST gracefully skip and retain items of unknown type.** This is the single most important robustness rule — old Relay versions that dropped envelopes with unknown item types are documented as a bug (the client_report spec calls it out). Never 400 on an unknown `type`.

Worked example from the spec:

```
{"event_id":"9ec79c33ec9942ab8353589fcb2e04dc","dsn":"https://e12d836b15bb49d7bbf99e64295d995b:@sentry.io/42"}\n
{"type":"attachment","length":10,"content_type":"text/plain","filename":"hello.txt"}\n
\xef\xbb\xbfHello\r\n\n
{"type":"event","length":41,"content_type":"application/json"}\n
{"message":"hello world","level":"error"}\n
```

### Item types the target SDKs actually send

| type | payload | notes |
|---|---|---|
| `event` | JSON error/default event | max 1 per envelope; mutually exclusive with `transaction` and `feedback`; 1 MiB cap |
| `transaction` | JSON transaction | max 1; only when tracing enabled; 1 MiB cap |
| `attachment` | arbitrary bytes | multiple allowed; sent with their event |
| `session` / `sessions` | JSON session update / pre-aggregated buckets | sent by default when auto session tracking is on (Python/Ruby server-mode sends `sessions` aggregates); ≤100 sessions per envelope |
| `client_report` | JSON `{timestamp?, discarded_events:[{reason,category,quantity}]}` | on by default in all SDKs; 4 KiB cap |
| `log` | JSON `{"items":[...]}` container | the Logs product; see §6 |
| `check_in` | JSON check-in | max 1 per envelope; 100 KiB cap |
| `feedback`, `user_report` (deprecated), `replay_event`+`replay_recording`, `profile`, `profile_chunk`, `otel_log`, `span` (v2 container), `trace_metric`, `statsd`/`metric_buckets` | | tolerate + discard (or store raw) |
| reserved, never emitted by SDKs: `security`, `unreal_report`, `form_data` | | |

Constraint worth knowing: SDKs MUST NOT mix telemetry types in one envelope (exceptions: attachments/sessions/client_reports/check_ins may ride along with an event). So in practice an envelope is "one event + its attachments + maybe a session + maybe a client_report", or "one log container", etc.

### Size limits (Relay's, a sane menu for nashgit)

200 MiB envelope after decompression; 1 MiB per event/transaction/log item; 100 KiB per check-in; 4 KiB per client report; 100 sessions per envelope.

Source: https://develop.sentry.dev/sdk/foundations/envelopes/ and .../envelope-items/

## 4. Compression

Envelopes themselves have no compression mechanism; the HTTP body does, via `content-encoding`. Relay/Sentry accept: **gzip, deflate (zlib), br (Brotli), zstd**.

What the target SDKs send (verified in source where noted):
- **Python**: gzip level 9 by default, **but the default algorithm flips to Brotli whenever the `brotli` module is importable** (transport.py: `"gzip" ... if compression_level is not None or brotli is None else "br"`). Many deployments have brotli installed transitively — the server must decode `br`.
- **JS**: browser sends uncompressed (fetch, no CompressionStream use for envelopes); Node/Bun gzips.
- **Ruby, Cocoa**: gzip.
- **Rust**: reqwest-based transport, gzip.

Recommendation: decode gzip + deflate + br at minimum; add zstd for completeness (Rust: flate2 + brotli + zstd crates, or ruzstd). Also accept `transfer-encoding: chunked` (reqwest/hyper handle this for free).

Source: https://develop.sentry.dev/sdk/foundations/transport/compression/

## 5. Required responses

Verified from Relay source (`relay-server/src/endpoints/envelope.rs`):

```rust
#[derive(Serialize)]
struct StoreResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<EventId>,
}
// ...
Ok(Json(StoreResponse { id }))
```

- **200 OK** with `content-type: application/json` and body `{"id":"<event_id>"}` (the envelope's event_id) or `{}` when the envelope carried no event. **Do not return an empty body**: sentry-elixir v11+ treats empty-200 as a transport failure and enters a retry/client-report loop — Bugsink shipped exactly this bug and fixed it in PR bugsink/bugsink#396 (merged 2026-06-04). Cheap insurance even though Elixir is not in your five.
- **400** with `{"detail": "..."}` for malformed envelopes (SDKs discard + record `send_error` client report; no retry).
- **403** when auth is entirely missing/invalid.
- **413** for oversized payloads (SDKs must not retry).
- **429** for rate limiting (below).
- SDKs treat any 2xx as success and **do not parse the response body for correctness** (client-reports spec: "MUST consider it a successful send"); 4xx/5xx → discard, no retry (network errors only may be retried). So a server that "just 200s everything" works for all five target SDKs — but return the JSON body anyway.

### Rate limiting — and the "stop sending me transactions" trick

Two headers:

- `Retry-After: <seconds>` on 429 (429 with neither header ⇒ SDKs assume 60 s for all categories).
- `X-Sentry-Rate-Limits: <retry_after>:<cat1;cat2;...>:<scope>:<reason>:<namespaces>, ...` — **may appear on ANY response, including 200**, precisely "to proactively inform SDKs that certain payload types are disabled before SDKs even try to send them." Empty category list = all categories. Scope/reason can be anything (SDKs ignore them).

**Yes, nashgit can suppress categories it does not store.** Emit on every 200:

```
X-Sentry-Rate-Limits: 86400:transaction;span;profile;profile_chunk;replay;trace_metric:project:unwanted
```

Compliant SDKs (all five implement this; it is a mandatory SDK feature) then drop those categories client-side for 24 h and re-apply on every response, so keep sending the header. Consequences to accept: SDKs record `ratelimit_backoff` client-report outcomes for the suppressed categories, and the SDK still *instruments* (spans are created, just not sent). Do NOT rate-limit categories you want: `error`, `default`, `log_item`, `monitor`, `session`.

Category names (from `relay-base-schema/src/data_category.rs`, distinct from item types): `default`, `error`, `transaction`, `span`, `security`, `attachment`, `session`, `monitor` (check-ins), `log_item`, `log_byte`, `replay`, `feedback`, `profile`, `profile_chunk`, `profile_chunk_ui`, `trace_metric`, `trace_metric_byte`, `internal` (client reports; promised never rate-limited explicitly, though the empty-category "all" limit does cover it — so avoid empty-category limits).

Source: https://develop.sentry.dev/sdk/foundations/transport/rate-limiting/

## 6. Event payload schema — parse little, store everything

Only three attributes are required: `event_id` (32-char lowercase hex, uuid4, no dashes), `timestamp` (RFC 3339 string OR Unix-seconds number — handle both), `platform`. Everything else is optional, and the docs explicitly say the server tolerates non-canonical historical formats — so **store the raw item JSON and parse only what you index**:

- For listing/grouping: `level` (fatal|error|warning|info|debug; default error), `logger`, `transaction`, `server_name`, `release`, `environment`, `timestamp`.
- For the issue title + fingerprint: `exception.values[]` (each: `type`, `value`, `module`, `mechanism`, `stacktrace.frames[]` with `filename`/`function`/`lineno`/`context_line`/`pre_context`/`post_context`/`in_app`), or `message` / `logentry` (`formatted`, `message`, `params`) when there is no exception. Sentry's own default grouping ≈ hash of (exception type + normalized stacktrace) with `fingerprint` array as override — for nashgit, hashing `exception type + value-normalized + top in-app frames` is the Bugsink-class approach.
- Keep as raw JSON blobs for detail view: `tags` (string→string, ≤200 chars each), `extra`, `contexts` (trace/os/runtime/device/browser/app...), `user` ({id, username, email, ip_address}), `breadcrumbs.values[]`, `sdk` ({name, version}), `request`, `modules`, `fingerprint`, `dist`, `threads`, `debug_meta`.
- The canonical server-side schema is documented as Rust: https://getsentry.github.io/relay/relay_event_schema/protocol/ and https://github.com/getsentry/relay/tree/master/relay-event-schema/src/protocol.

Sentry's own field caps (useful validation menu, not requirements): tags 200 chars, messages 8192, context objects 8 KiB, extra values 16 KiB (256 KiB total), 50 stack frames.

Source: https://develop.sentry.dev/sdk/foundations/envelopes/event-payloads/

## 7. Sentry Logs — same DSN, same endpoint, one item type

Spec: https://develop.sentry.dev/sdk/telemetry/logs/ — **status: stable, version 2.2.0, last updated 2026-06-22.** Two wire protocols exist; SDKs MUST use the `log` envelope item (the `otel_log` OTLP-shaped item is legacy/appendix — tolerate it, but none of your five SDKs emit it by default).

**Yes: logs flow through the same DSN and the same `/api/<pid>/envelope/` endpoint.** One `log` item max per envelope; logs from different traces may share it (then no DSC envelope header).

Item headers (all three REQUIRED):

```json
{"type":"log","item_count":5,"content_type":"application/vnd.sentry.items.log+json"}
```

Payload: `{"items":[ ...log objects... ]}` (optionally with top-level `"version":2` and `"ingest_settings":{"infer_ip":"auto","infer_user_agent":"auto"}` — server MAY honor or ignore).

Each log object:

| field | req | type | notes |
|---|---|---|---|
| `timestamp` | REQUIRED | Number | Unix seconds (float) |
| `trace_id` | REQUIRED | String | 32-hex |
| `level` | REQUIRED | String | trace\|debug\|info\|warn\|error\|fatal |
| `body` | REQUIRED | String | the message |
| `span_id` | optional | String | 16-hex |
| `severity_number` | optional | Int | OTel mapping: trace=1, debug=5, info=9, warn=13, error=17, fatal=21 |
| `attributes` | optional | Object | typed values: `{"value": X, "type": "string"\|"integer"\|"double"\|"boolean"}` |

Well-known attributes: `sentry.environment`, `sentry.release`, `sentry.sdk.name`, `sentry.sdk.version`, `sentry.message.template` + `sentry.message.parameter.N` (structured logging), `server.address` (backend SDKs), `sentry.origin` (integration-sourced logs, e.g. `auto.log.serilog`), `sentry.timestamp.sequence` (ordering tiebreaker), `user.id`/`user.name`/`user.email`.

Batching: SDKs flush at ≤100 logs per envelope or 5 s, hard cap 1000 queued. Rate-limit categories: `log_item` (count) and `log_byte` (bytes).

**SDK support as of 2026-08 (verified per-platform docs.sentry.io pages):**

| SDK | min version | API | default |
|---|---|---|---|
| JavaScript (incl. Bun/Node) | 9.41.0 | `enableLogs: true`, `Sentry.logger.trace/…/fatal`, `Sentry.logger.fmt` | spec 2.0.0 (2026-04-09) flipped enableLogs default to **true**; older releases need the option |
| Python | 2.35.0 (earlier experimental via `_experiments`) | `sentry_sdk.logger.*`, `enable_logs`, `LoggingIntegration(capture_sentry_logs=True)` forwards stdlib logging | on per current docs |
| Ruby / Rails | 5.24.0 | `config.enable_logs = true`, `Sentry.logger.*` | opt-in at 5.24 |
| Rust | 0.42.0 | `logger_info!` etc. macros; `logs` cargo feature (in default features); `tracing`/`log` integrations forward to Sentry logs | enabled by default |
| Swift / Cocoa | 8.55.0 (experimental) → stable in 9.0.0 | `SentrySDK.logger` | stable at 9.0.0 |

This means nashgit's own server logs can go into the same store by emitting `log` envelope items to its own DSN (or by writing rows directly and skipping HTTP for itself).

## 8. Client reports and check-ins

**Client reports** (`client_report` item, spec stable v1.23.0, 2026-06-22): SDK self-reporting of dropped telemetry — `{timestamp?, discarded_events:[{reason, category, quantity}]}` with reasons like `sample_rate`, `before_send`, `ratelimit_backoff`, `queue_overflow`, `send_error`. They arrive piggybacked on normal envelopes; SDKs assume they are never rate-limited. **Store as counters or discard — never reject.** Mildly interesting for nashgit as a "how much am I dropping" dashboard, especially once you 429 transactions.

**Check-ins** (`check_in` item, spec stable v1.6.0): cron monitoring. Two check-ins per job run sharing a `check_in_id` (uuid): `in_progress` then `ok`/`error` with optional `duration` (seconds) and optional `monitor_config` upsert (`schedule: {type: crontab|interval, value}`, `checkin_margin`, `max_runtime`, `timezone`, thresholds). All-zero `check_in_id` means "update the most recent in_progress for this monitor_slug". **Worth storing**: it is a tiny table and gives nashgit cron-health + missed-run Pushover alerts (a missed-run detector needs a server-side sweep comparing schedules to last check-in). If you don't want them yet, accept-and-store-raw; do not 429 the `monitor` category if you ever might.

## 9. Reusable Rust parsing code

- **`sentry-types` (crates.io, v0.49.1 — versioned with the Rust SDK, actively maintained)** — the practical choice:
  - `sentry_types::Dsn` (parse/validate DSNs), `sentry_types::Auth` (parse X-Sentry-Auth / query params — `Auth::from_querystring` exists).
  - `sentry_types::protocol::v7::Envelope::from_slice(&[u8])` parses a full envelope; `Envelope::from_bytes_raw` keeps it unparsed; `serialize`/`to_writer` for re-emission.
  - `EnvelopeItem` (non-exhaustive): `Event`, `Transaction`, `SessionUpdate`, `SessionAggregates`, `Attachment`, `MonitorCheckIn`, `ClientReport`, `ItemContainer` (which covers `Vec<Log>` and `Vec<Metric>` — i.e. the `log` item), `Raw`. Full typed `Event`, `Exception`, `Stacktrace`, `Breadcrumb`, `Log`, `MonitorCheckIn` structs included.
  - Caveats to test before committing: it is written for the SDK side, so check (a) how `from_slice` treats item types it doesn't model (whether they land in `Raw`/get skipped or error) with a captured real envelope from each SDK, and (b) that its `Event` deserialization is lenient enough for cross-SDK payloads. Fallback: write the ~100-line envelope splitter yourself (the grammar is trivial) and use `sentry-types` only for `Dsn`/`Auth`/typed payloads, keeping unknown items as raw bytes. Given nashgit already stores raw JSON, a hand-rolled splitter + serde_json::Value is arguably the most robust path, with `sentry-types` for DSN/auth parsing.
- **Relay's crates are NOT on crates.io** (`relay-event-schema`, `relay-protocol`, `relay-base-schema` — cargo search confirms no getsentry-published versions). They are usable only as git dependencies on https://github.com/getsentry/relay (heavy: relay-event-schema pulls the whole processor/annotation machinery). Best used as **reference**, not dependency: envelope/item parsing in `relay-server/src/envelope.rs`, endpoint behavior in `relay-server/src/endpoints/envelope.rs` and `common.rs`, event schema in `relay-event-schema/src/protocol/` (rustdoc published at https://getsentry.github.io/relay/relay_event_schema/protocol/), data categories in `relay-base-schema/src/data_category.rs`, size limits in `relay-config/src/config.rs`.

## 10. Prior art on custom Sentry-compatible ingest

- No single official "how to build a Sentry-compatible server" doc exists; develop.sentry.dev's SDK Foundations section IS the wire spec (it now even serves clean `.md` at every URL by appending `.md` — ideal for offline reference).
- **Bugsink** (Python/Django, the thing being replaced) is the best-documented independent implementation: `bugsink/ingest` views, `KEEP_ENVELOPES` debug setting, `VALIDATE_ON_DIGEST` lenient mode; instructive bugs: PR #396 (must return `{"id":...}` JSON, 2026-06), PR #435 (tolerate missing event timestamp, non-UTF-8 envelope headers, 2026-07). GlitchTip (Django) is the other mature one.
- urgentry.com's envelope guide (https://urgentry.com/guides/fundamentals/what-is-a-sentry-envelope/) is an accurate third-party summary: "the practical compatibility claim is that the envelope endpoint parses every standard item type, returns the right status codes, and emits the right rate-limit header."

## 11. Minimal conformance checklist for nashgit

1. `POST /api/<pid>/envelope/` → decompress (gzip/deflate/br/zstd by `content-encoding`) → split envelope → auth from header|query|`dsn` envelope header → map `sentry_key`→project, check `pid` → store items → `200 {"id":"<event_id>"}`.
2. Never reject unknown item types; never require `length` (implicit-length items exist); tolerate both timestamp formats; tolerate missing envelope `event_id` by falling back to the payload's.
3. Emit `X-Sentry-Rate-Limits` on every 200 to suppress unwanted categories (transactions/spans/profiles/replays).
4. Store raw item JSON; index only event_id, timestamp, level, release, environment, fingerprint-hash, and log fields (timestamp, level, trace_id, body).
5. Test matrix: python (gzip AND brotli-installed), sentry (Rust), @sentry/bun, sentry-ruby/Rails, sentry-cocoa — one crash, one captured message, one `Sentry.logger` batch, one check-in each; plus `sentry-cli send-envelope` for adversarial envelopes.

### Sources
- https://develop.sentry.dev/sdk/foundations/envelopes/
- https://develop.sentry.dev/sdk/foundations/envelopes/envelope-items/
- https://develop.sentry.dev/sdk/foundations/envelopes/event-payloads/
- https://develop.sentry.dev/sdk/foundations/transport/authentication/
- https://develop.sentry.dev/sdk/foundations/transport/rate-limiting/
- https://develop.sentry.dev/sdk/foundations/transport/compression/
- https://develop.sentry.dev/sdk/telemetry/logs/
- https://develop.sentry.dev/sdk/telemetry/client-reports/
- https://develop.sentry.dev/sdk/telemetry/check-ins/
- https://develop.sentry.dev/sdk/telemetry/sessions/
- https://github.com/getsentry/relay/blob/master/relay-server/src/endpoints/envelope.rs
- https://getsentry.github.io/relay/relay_event_schema/protocol/index.html
- https://docs.rs/sentry-types/latest/sentry_types/protocol/v7/struct.Envelope.html
- https://docs.rs/sentry-types/latest/sentry_types/protocol/v7/enum.EnvelopeItem.html
- https://docs.sentry.io/platforms/javascript/logs/
- https://docs.sentry.io/platforms/python/logs/
- https://docs.sentry.io/platforms/ruby/logs/
- https://docs.sentry.io/platforms/rust/logs/
- https://docs.sentry.io/platforms/apple/logs/
- https://github.com/bugsink/bugsink/pull/396
- https://github.com/bugsink/bugsink/pull/435
- https://github.com/getsentry/sentry-javascript/blob/develop/packages/core/src/api.ts
- https://github.com/getsentry/sentry-python/blob/master/sentry_sdk/transport.py
- https://urgentry.com/guides/fundamentals/what-is-a-sentry-envelope/

## Pushover

### Key facts
- Send endpoint: POST https://api.pushover.net/1/messages.json over HTTPS; required params token, user, message; optional title, url, url_title, priority, sound, timestamp, device, html/monospace, ttl, tags, attachment; form-encoded, multipart, or JSON (Content-Type: application/json) all accepted (docs read 2026-08-18).
- Hard size limits: message 1024 UTF-8 characters (each up to 4 bytes), title 250 chars, url 512 chars, url_title 100 chars, attachment 5 MB; Pushover rejects empty messages.
- Priorities: -2 silent (badge only), -1 no sound/vibe, 0 default (quiet hours downgrade it to -1), 1 bypasses quiet hours and shows red, 2 emergency repeats every `retry` seconds (min 30) until acknowledged or `expire` seconds (max 10800, hard cap 50 retries) and returns a `receipt`.
- Emergency (2) extras: optional `callback` URL must be reachable from the public Internet — useless on a tailnet-only nashgit, so poll GET /1/receipts/{receipt}.json instead; cancel early via POST /1/receipts/{receipt}/cancel.json or POST /1/receipts/cancel_by_tag/{tag}.json (send `tags` at create time).
- Since May 1, 2026 quotas are per ACCOUNT, not per application: 10,000 free messages/month pooled across unlimited registered apps (Teams: 25,000), reset 00:00 Central on the 1st; when exhausted ALL the account's apps get HTTP 429 until reset or upgrade — so per-project app tokens no longer buy extra quota.
- Extra capacity is a one-time purchase drawn down after the free pool: 10k/$25, 25k/$57.50, 50k/$100, 100k/$150, 500k/$500 USD; auto-upgrade-when-low is available; one 'message' = one successful API call to one user (a group of N users costs N).
- Every messages call returns X-Limit-App-Limit / X-Limit-App-Remaining / X-Limit-App-Reset headers (now reflecting the account pool), and GET /1/apps/limits.json?token=... returns the same as JSON.
- Retry etiquette from the official docs: 200 + status:1 = delivered to queue, done; any 4xx / status!=1 = your input is wrong or quota is gone — repeating the identical request will NEVER work, do not retry; 5xx or no response = retry the same request but no sooner than 5 seconds; keep at most 2 concurrent connections and reuse the TCP connection, or Pushover rate-limits your IP; sustained 4xx floods earn a temporary, escalating IP block.
- There is NO public API to register a Pushover application — app tokens are minted manually at https://pushover.net/apps/build — so nashgit's automated per-project DSN minting cannot mint per-project Pushover tokens; use one app token for the whole tracker.
- Sentry's own legacy plugin (getsentry/sentry 26.6.0, src/sentry_plugins/pushover/plugin.py) sets title = "{project.name}: {group.title}"[:250], message = event title + tags[:1024], url = deep link to the issue, url_title = "Issue Details", and uses a static per-project priority setting (no per-event level mapping), retry default 30, expire default 90.
- Healthchecks.io's transport uses ONE site-wide app token, per-channel user key + separate down/up priorities, html=1, tags = check unique key so a recovery cancels emergency retries via cancel_by_tag without storing receipts, an internal token bucket of 6 messages per user key per minute, and treats HTTP 400 with user:invalid as a permanent failure (channel disabled).
- Grafana's notifier truncates title/message/url in runes at 250/1024/512, substitutes "(no details)" for empty messages, uses separate priority AND sound for alerting vs resolved, and only sends retry/expire when priority==2; Uptime Kuma always sends retry=30 expire=3600, html=1, and a monitor deep link with url_title "Link to Monitor".
- A Pushover support thread (i338) documents a user whose buggy app burned the entire 10,000-message quota in minutes — Pushover offers no per-app frequency cap, so the tracker must do its own dedup/throttling.
- Pushover client apps cost $4.99 one-time per platform after a 30-day trial; receiving is unlimited and free of recurring fees.
- ntfy alternative in one line: self-hosted ntfy has no quota, but instant iOS delivery requires relaying through ntfy.sh (upstream-base-url) and the iOS app has documented reliability issues — Pushover's mature iOS client is the better fit and the user already runs it.

### Recommendations
- Use ONE Pushover application token for all of nashgit (register once at https://pushover.net/apps/build or reuse the existing nac-bugs app token); there is no API to create app tokens, and since May 2026 extra tokens add zero quota. Store token + user key as two config values.
- Copy Sentry's own payload convention: title = "{project}: {issue title}" (truncate 250 chars), message = exception value/culprit + a few tags (truncate 1024 chars, counted in characters not bytes, never empty — fall back to "(no details)"), url = deep link to the nashgit issue page (tailnet URL is fine — his devices are on the tailnet), url_title = "Open in nashgit", timestamp = event occurred-at.
- Notify on issue STATE CHANGES only (new issue, regression, unmute — Bugsink semantics), never per event, and add a local token-bucket throttle (Healthchecks uses 6/user-key/minute) plus a per-issue cooldown; Pushover has no server-side frequency cap and one runaway loop can burn the whole 10k monthly pool in minutes.
- Priority mapping: default 0 for a new error-level issue; 1 for fatal or a per-project "critical" flag; -1 (optionally with a ttl so it self-deletes) for info/log-derived notices; make priority 2 (emergency) a per-project opt-in with retry=60 expire=1800, tags=<issue-key>, and cancel_by_tag on issue resolve — and do NOT use the callback param (Pushover can't reach a tailnet URL); poll the receipt if ack state matters.
- Retry policy in the sender: queue outbound pushes in SQLite; on 5xx/timeout retry with backoff starting at >=5 s; on any 4xx (including 429) never retry the same payload — on 429 park the queue until the X-Limit-App-Reset timestamp and show "quota exhausted" in the nashgit UI; on 400 user-invalid mark the channel broken. Single sender task with reqwest keep-alive satisfies the 2-connection rule for free.
- Track X-Limit-App-Remaining from every response (or poll GET /1/apps/limits.json) and surface the monthly budget in the nashgit UI; consider warning yourself via a -1 priority push at, say, 20% remaining.
- Do not push for server-log ingestion at all — logs are stored and browsable; only error-tracker state changes deserve a phone buzz.
- Use a single user key now; if Rob should get alerts later, swap in a delivery group key (Groups API is programmatic, the swap is transparent to the sending code) and accept that each member doubles quota burn.

### Detail

## Pushover as the sole notification channel for nashgit's error tracker

All facts below verified against pushover.net official docs and support KB (Knowledge Base) on 2026-08-18, plus primary source code of four integrations.

### 1. API mechanics

- **Endpoint**: `POST https://api.pushover.net/1/messages.json` (HTTPS required, POST required; `.xml` suffix for XML responses). Input encodings accepted: `application/x-www-form-urlencoded`, `multipart/form-data`, and JSON with `Content-Type: application/json`. (Sentry's old plugin comment about JSON being disabled is stale — current docs explicitly accept JSON.)
- **Required**: `token` (app token, 30 chars `[A-Za-z0-9]`), `user` (user or group key, 30 chars — interchangeable from the sender's view), `message`.
- **Optional**: `title` (defaults to the registered app's name), `url` + `url_title` (supplementary link shown under the expanded notification), `priority` (-2..2), `sound` (built-in list or account custom sounds; `GET /1/sounds.json?token=...` enumerates), `timestamp` (Unix time to display instead of API-received time — set this to the event's occurred-at), `device` (comma-separated names; omit to hit all devices), `html=1` OR `monospace=1` (mutually exclusive; formatting is stripped in the banner, rendered only inside the app), `ttl` (seconds until auto-delete on device; ignored for priority 2), `tags` (comma-separated, emergency-only, enables cancel_by_tag), `attachment`/`attachment_base64`+`attachment_type` (one image, max 5,242,880 bytes).
- Up to 50 user keys comma-separated in `user` per request. No OAuth, no signing — the token is the whole auth.
- **Response**: HTTP 200 + `{"status":1,"request":"<uuid>"}` on success. Failure: 4xx + `status != 1` + `errors` array (e.g. `{"user":"invalid","errors":["user identifier is invalid"],...}`). `priority=2` responses add a `receipt`. Keep the `request` UUID in logs for support.

### 2. Priorities (-2..2) and emergency mechanics

| Priority | Behavior |
|---|---|
| -2 | No notification at all; iOS badge increments. |
| -1 | Notification, no sound/vibration. Quiet-hours delivery of priority 0 behaves like this. |
| 0 | Default: sound, vibration, banner. |
| 1 | Bypasses quiet hours, always sounds, highlighted red. |
| 2 | Like 1, plus repeats until acknowledged. |

**Emergency (2)**: must supply `retry` (seconds between re-alerts, min 30) and `expire` (max 10,800 s = 3 h; total retries hard-capped at 50). Response includes a `receipt`. Status: `GET /1/receipts/{receipt}.json?token=...` (acknowledged, acknowledged_by, expired, etc.). Optional `callback` URL gets a POST when acknowledged — **but Pushover's servers must reach it from the public Internet, so a tailnet-only nashgit cannot use callbacks; poll the receipt instead**. Cancel early: `POST /1/receipts/{receipt}/cancel.json`, or send `tags=<issue-key>` at create time and `POST /1/receipts/cancel_by_tag/{tag}.json` — Healthchecks uses exactly this to stop the siren when a check recovers, with zero receipt storage. First group member to acknowledge cancels retries for everyone.

**When would an error tracker use 2?** Only as an explicit per-project "page me" escalation (e.g. fatal-level events in a production project), Healthchecks/Grafana style — never as the default for errors. If used: `retry=60, expire=1800` is a sane profile; `tags` = issue key; fire `cancel_by_tag` when the issue is resolved. `ttl` is ignored at priority 2.

### 3. Limits, quota, and cost

- **Per-message**: message 1024 UTF-8 **characters** (each up to 4 bytes — count characters/runes, not bytes), title 250, url 512, url_title 100.
- **Quota — changed May 1, 2026** (blog post 2026-04-08): limits are now **per account**, not per application. Unlimited app registrations; all apps share the account's **10,000 free messages/month** (Teams: 25,000). Resets 00:00:00 US Central on the 1st. One message = one successful API call to one user regardless of device count; a group of N users burns N.
- **When exceeded**: HTTP **429** for every app on the account until reset or upgrade.
- **Monitoring**: every messages call returns `X-Limit-App-Limit`, `X-Limit-App-Remaining`, `X-Limit-App-Reset` (names say "App" for historical reasons; values are the account pool). Same data via `GET /1/apps/limits.json?token=...`.
- **Extra capacity** (one-time purchase, drawn after the free pool, persists until used, non-refundable): 10k → $25; 25k → $57.50; 50k → $100; 100k → $150; 500k → $500 USD. Auto-upgrade-when-low can be enabled at https://pushover.net/settings/upgrade.
- Delivered messages are deleted from Pushover's servers on device sync; undelivered ones after 21 days.
- **Client apps**: $4.99 one-time per platform (iOS/Android/Desktop) after a 30-day trial; receiving is otherwise free and unlimited.
- **Cautionary tale**: support thread i338 — a user's buggy app burned all 10,000 messages in minutes; Pushover has no server-side frequency cap per app, so the tracker must throttle itself.

### 4. App registration: one token vs per-project tokens

- Registration is free at https://pushover.net/apps/build: you set a name (the default message title) and an icon, and get a 30-char token. **There is no public API to register applications** — it is a manual web flow. nashgit mints DSNs (Data Source Names — the Sentry SDK's project ingest URL+key) programmatically, so it cannot mint matching Pushover tokens.
- Since May 2026, per-project tokens no longer add quota (all pooled). What they still buy: per-project icon/name on the phone and per-app usage graphs on the dashboard. What they cost: a manual registration step per project and N secrets instead of 1.
- **Verdict**: one application token named "nashgit" (or reuse the existing nac-bugs one), configured once; put the project name in the `title`. This is exactly Healthchecks' model (single `PUSHOVER_API_TOKEN` site-wide, per-channel user keys).
- **User key vs delivery group**: a single user key suffices for a solo operator. A delivery group key (manageable via the Groups API, which unlike app registration IS programmatic) is drop-in compatible in the `user` param and lets you add a second person (Rob) later without config changes — but doubles quota burn per alert, and for emergency priority the first acknowledger silences everyone.

### 5. Error handling and retry etiquette (official "Being Friendly to our API")

- **200 + status:1** → queued. Done.
- **4xx / status != 1** (including 429 quota-exhausted): the input is invalid or quota is gone. *"Repeating your same request will not work, no matter how many times you retry it."* Parse `errors`, fix or drop. On 429, stop sending until `X-Limit-App-Reset`. Healthchecks additionally treats HTTP 400 with `"user":"invalid"` as a **permanent** failure and disables the channel.
- **5xx / no response** → temporary; retry the same request **no sooner than 5 seconds** later.
- **Concurrency**: max 2 concurrent TCP connections or Pushover rate-limits your IP; send sequentially over one keep-alive connection (reqwest's default pool does this). Sustained 4xx floods trigger a temporary, auto-extending IP block.

### 6. Prior art — payload conventions worth copying

**Sentry's own legacy plugin** (still shipping in Sentry 26.6.0, `src/sentry_plugins/pushover/plugin.py`) — the most direct precedent for an error tracker:
- `title` = `"{project.name}: {group.title}"` truncated to 250
- `message` = event title (256) + `"\n\nTags: k=v, k=v"`, truncated to 1024
- `url` = absolute deep link to the issue (with a `?referrer=pushover_plugin` param), `url_title` = "Issue Details"
- `priority` = static per-project config choice (-2..2) — Sentry does NOT map event level to priority; `retry`/`expire` only meaningful at 2 (defaults 30/90).

**Healthchecks.io** (`hc/integrations/po/transport.py`): single site-wide token; channel value packs `userkey|down_prio|up_prio` (sentinel -3 = suppress that direction); `html=1`; `tags` = check unique key; on recovery from an emergency-priority "down", POSTs `cancel_by_tag` before sending the "up"; `url` = deep link, `url_title` = "View on {SITE_NAME}"; internal token bucket: **6 messages per user key per minute**.

**Uptime Kuma** (`server/notification-providers/pushover.js`): always sends `retry=30, expire=3600` (harmless when priority != 2), `html=1`, `url` = monitor deep link with `url_title` "Link to Monitor", optional `ttl`/`device`, different `sound` for UP vs DOWN.

**Grafana** (`grafana/alerting/receivers/pushover/v1/pushover.go`): truncates title/message/url **in runes** at 250/1024/512 and logs when it truncates; replaces an empty message with `"(no details)"` because Pushover rejects empty messages; separate priority AND sound for alerting vs resolved states; writes `retry`/`expire` only when priority == 2; multipart form; optional image attach self-capped at 2 MB.

**Synthesized mapping for nashgit** (drawing on all four): notify on issue *state change* (new issue, regression, unmute — Bugsink's exact alert semantics), never per event. Map: new error-level issue → 0; fatal / prod-critical → 1 (2 only as per-project opt-in paging); regression → 0 or 1; info/log-derived → -1 with a `ttl` so noise self-deletes. Distinct `sound` per severity is a cheap UX win (`siren` for fatal). Set `timestamp` to the event time so delayed ingestion still shows honest times. `monospace=1` suits a one-line stack frame; you cannot combine it with `html=1`.

### 7. ntfy in one line

Self-hosted ntfy would kill the quota and the third-party dependency, but instant iOS delivery from a self-hosted server must relay through ntfy.sh (`upstream-base-url`) and the iOS app has documented reliability problems (binwiederhier/ntfy-ios TECHNICAL_LIMITATIONS, issue #1377) — Pushover's mature iOS client is the right call for an iPhone-first solo operator, and the user already owns it.

### Sources
- https://pushover.net/api
- https://pushover.net/api/receipts
- https://blog.pushover.net/posts/2026/4/app-limits
- https://support.pushover.net/i13-purchasing-additional-capacity-to-send-more-messages-per-month
- https://support.pushover.net/i12-message-size-and-frequency-limitations
- https://support.pushover.net/i8-how-much-does-pushover-cost-is-there-a-subscription
- https://support.pushover.net/i338-app-notification-frequency-limits
- https://raw.githubusercontent.com/getsentry/sentry/26.6.0/src/sentry_plugins/pushover/plugin.py
- https://raw.githubusercontent.com/getsentry/sentry/26.6.0/src/sentry_plugins/pushover/client.py
- https://raw.githubusercontent.com/healthchecks/healthchecks/master/hc/integrations/po/transport.py
- https://raw.githubusercontent.com/healthchecks/healthchecks/master/hc/api/models.py
- https://raw.githubusercontent.com/louislam/uptime-kuma/master/server/notification-providers/pushover.js
- https://raw.githubusercontent.com/grafana/alerting/main/receivers/pushover/v1/pushover.go
- https://docs.ntfy.sh/known-issues/
- https://github.com/binwiederhier/ntfy/issues/1377
- https://github.com/binwiederhier/ntfy-ios/blob/main/docs/TECHNICAL_LIMITATIONS.md

## Log ingestion and storage

### Key facts
- The Sentry Logs wire protocol is stable (spec v2.2.0 at develop.sentry.dev): a `log` envelope item on the SAME /api/<id>/envelope/ endpoint nashgit already ingests, content-type application/vnd.sentry.items.log+json, with an `items` array of up to 100 logs per envelope; each log = {timestamp, trace_id, span_id, level, body, severity_number 1-24, attributes:{k:{value,type}}}.
- Severity levels are OTel-aligned: six levels trace/debug/info/warn/error/fatal mapping to OpenTelemetry SeverityNumber ranges 1-24, so a Sentry-logs table and an OTLP-logs table can share one schema.
- SDK support is broad in 2026: sentry-python >=2.35.0, sentry-javascript >=9.41.0 (enableLogs), sentry-rust >=0.42.0 (logs on by default, current 0.49.1), sentry-ruby >=5.24.0 / sentry-rails >=5.27.0 (5.28.0 auto-enables Rails structured logging).
- Breaking change 5 days ago: sentry-python 2.68.0 (2026-08-13) made enable_logs a no-op; sentry_sdk.logger.* now always sends, and stdlib-logging/Loguru forwarding requires explicit LoggingIntegration(capture_sentry_logs=True) / LoguruIntegration(capture_sentry_logs=True) (default False).
- For Rust projects, sentry-tracing (feature "logs") captures tokio-rs/tracing events as Sentry structured logs with tracing fields as queryable attributes — so nashgit's own tracing output can flow to nashgit over its own DSN.
- Sentry Logs alone does NOT capture system-level server logs (journald, nginx, cron): there is no official journald->Sentry shipper, and Sentry's own open "Log Drain Support" issue (getsentry/sentry#91726) confirms non-SDK ingestion is unsolved on their side except via OTLP.
- Sentry itself now ingests OTLP logs (open beta) at /api/<project_id>/integration/otlp/v1/logs authenticated by `x-sentry-auth: sentry sentry_key=<dsn-public-key>` (Relay PR #5130, merged 2025-09-15) — copying this exact path+auth shape makes nashgit both Sentry-compatible and OTLP-compatible with the DSN key as the token.
- A minimal OTLP/HTTP logs receiver in Rust is small: POST /v1/logs, body ExportLogsServiceRequest, decoded with the opentelemetry-proto crate (features gen-tonic-messages+logs gives prost protobuf decode; with-serde gives spec-compliant JSON decode); Vector's own OTLP HTTP source is Rust prior art.
- Journald shipping on one VPS is a solved agent problem: Vector's journald source -> stable `http` sink posting NDJSON to any endpoint (the documented Honeybadger recipe), or fluent-bit systemd input -> opentelemetry output; Vector's native `opentelemetry` sink is still beta (OTLP decoding support announced 2025-09-23).
- Every small log vendor converges on bespoke HTTP JSON: Better Stack = POST one JSON object/array with Bearer token, Axiom = POST /v1/datasets/{name}/ingest with JSON/NDJSON, Seq = POST /ingest/clef with CLEF NDJSON (@t, @mt, @l reified fields, X-Seq-ApiKey header) — a ~50-line endpoint pattern proven by all three.
- Serious single-binary log storage on SQLite exists: ChrisLog (Go, syslog+HTTP->SQLite, one container/one file), cortex (Rust, syslog/OTLP/Docker->SQLite+FTS), SolidLog (Rails), loglens (multi-GB Laravel logs on FTS5), timeless_logs (Elixir, compressed blocks + SQLite index); Seq notably outgrew generic engines and built a custom Rust storage engine, but at volumes far beyond one person's servers.
- Proven SQLite retention mechanics: WAL mode + batched transactional inserts; FTS5 as an external-content table over the logs table (no double text storage, delete-safe); prune with daily DELETE WHERE ts<cutoff plus auto_vacuum=INCREMENTAL/incremental_vacuum, or drop whole per-period DB files for instant retention; contentless_delete=1 exists since SQLite 3.43 but external-content is the standard choice.
- Recommended combination: (1) accept the `log` envelope item on the existing DSN endpoint — near-free since envelope parsing exists, covers everything built with a Sentry SDK; (2) add ONE bespoke NDJSON POST /api/<project_id>/logs authenticated by the same DSN public key for journald/curl/cron/Vector — everything else (full OTLP receiver) can wait until a standard agent is genuinely needed.

### Recommendations
- Implement Sentry `log` envelope item ingestion on the existing /api/<id>/envelope/ endpoint first: parse item type "log" (content-type application/vnd.sentry.items.log+json), insert the items array as rows in one transaction — this makes every existing nashgit DSN a log sink for official SDKs with no new auth or endpoint.
- Add a single bespoke NDJSON endpoint POST /api/<project_id>/logs authenticated by the project's DSN public key, with CLEF-style fields (ts, level, message, arbitrary attributes), for journald/curl/cron/Vector traffic; document a copy-paste Vector journald->http-sink config per project the way DSNs are handed out today.
- Store logs in a SEPARATE SQLite file from the error-events DB: table (project_id, ts, severity_number, level, body, attributes JSON, trace_id, span_id, source), WAL mode, batched inserts, FTS5 external-content table over body with triggers, nightly DELETE by per-project retention_days plus incremental_vacuum.
- Reuse the OTel severity model (levels trace..fatal, severity_number 1-24) in the schema so a future OTLP receiver needs no migration.
- Skip a full OTLP/HTTP receiver for now; if standard-agent compatibility is later needed, implement it at Sentry's own beta path shape /api/<project_id>/integration/otlp/v1/logs with x-sentry-auth using the opentelemetry-proto crate (features gen-tonic-messages, logs, with-serde).
- In wiring docs/skills, use the post-2.68.0 Python API: sentry_sdk.logger.* works unconditionally, and stdlib-logging forwarding needs LoggingIntegration(capture_sentry_logs=True) — do NOT document enable_logs, it is a no-op as of 2026-08-13.
- For nashgit's own logs and any Rust project, add the sentry-tracing subscriber layer with the `logs` feature so tracing events land as structured logs on the project DSN.
- Keep Pushover alerts bound to error events only; at most offer per-project opt-in alerting on fatal-severity logs, never per-log notification.

### Detail

# Log ingestion for nashgit — research findings (2026-08-18)

Abbreviations used: DSN (Data Source Name — the per-project Sentry ingest URL+key), OTLP (OpenTelemetry Protocol — the vendor-neutral telemetry wire format), OTel (OpenTelemetry), NDJSON (Newline-Delimited JSON — one JSON object per line), CLEF (Compact Log Event Format — Seq's NDJSON log schema), FTS5 (Full-Text Search 5 — SQLite's search extension), WAL (Write-Ahead Logging — SQLite's concurrent-write journal mode).

## 1. Sentry Logs over the existing DSN/envelope endpoint

### Wire format (spec v2.2.0, status "stable", develop.sentry.dev)
Logs ride the SAME envelope endpoint nashgit already implements for errors. One new item type:

- Item header: `{"type":"log","item_count":N,"content_type":"application/vnd.sentry.items.log+json"}`
- Payload: `{"items":[...]}` (optionally `version` + `ingest_settings` since spec 1.17.0). At most one `log` item per envelope; all logs for a flush are batched into it.
- Each log entry:
```json
{
  "timestamp": 1544719860.0,
  "trace_id": "5b8efff798038103d269b633813fc60c",
  "span_id": "b0e6f15b45c36b12",
  "level": "info",
  "body": "User John has logged in!",
  "severity_number": 9,
  "attributes": {
    "sentry.message.template": {"value": "User %s has logged in!", "type": "string"},
    "sentry.message.parameter.0": {"value": "John", "type": "string"}
  }
}
```
- Severity: six levels `trace|debug|info|warn|error|fatal`, mapped to OTel SeverityNumber ranges 1–24 (trace 1–4 … fatal 21–24). SDKs set the lowest number of the range. This deliberate OTel alignment means one storage schema serves both Sentry Logs and OTLP logs.
- Attribute model: flat map of `{value, type}` where type is string/integer/double/boolean. Well-known keys: `sentry.environment`, `sentry.release`, `sentry.sdk.name/version`, `sentry.message.template` / `sentry.message.parameter.X`, `server.address` (backend SDKs), `sentry.origin` (`auto.log.<lib>` for integration-forwarded logs), `user.id/name/email` (PII-gated), `sentry.timestamp.sequence` (ordering on frozen-clock runtimes like Cloudflare Workers).
- Batching (spec-mandated): SDKs MUST buffer; typical flush at 100 items or 5 s; MUST NOT exceed 100 logs/envelope; hard cap 1000 queued. So nashgit's receiver gets nice batches, not one POST per line.
- Rate-limit category `log_item` (and `log_byte` for client reports) — nashgit can ignore or implement later.
- There is also a legacy `otel_log` envelope item (OTLP LogRecord as JSON inside an envelope), documented as appendix-only; SDKs are required to use `log`.

### SDK support matrix (verified against docs/releases, 2026-08)
- **Python**: logs since sentry-sdk **2.35.0**. BREAKING as of **2.68.0 (2026-08-13)**: `enable_logs` and `enable_metrics` are now **no-ops** (dropped next major). `sentry_sdk.logger.info(...)` etc. now just works, always. Forwarding stdlib `logging` or Loguru requires explicit opt-in: `LoggingIntegration(capture_sentry_logs=True)` / `LoguruIntegration(capture_sentry_logs=True)` — **default False**. So "add the Sentry SDK and all Python logs flow" needs that one flag, and nashgit's wiring docs (nac-bugs-wire successor) must use the new option, not `enable_logs`.
- **JavaScript**: current docs say logs supported in **9.41.0+** (that release, 2025-07-24, promoted `enableLogs`/`beforeSendLog` out of `_experiments`). `Sentry.logger.*` API, `consoleLoggingIntegration` to forward `console.*`, Pino/Winston transports, CDN bundle variants with logs included.
- **Rust**: logs since sentry **0.42.0**; current **0.49.1** (2026-08-03). The `logs` cargo feature is in default features ("logs are enabled by default"). Macros `logger_trace!` … `logger_fatal!` with `key = value` attribute syntax. Crucially, **sentry-tracing** (feature `logs`) captures `tracing` events as Sentry structured logs with tracing fields as searchable attributes — so any tokio/tracing app (nashgit itself included) gets its logs shipped by adding one subscriber layer.
- **Ruby/Rails**: logs since sentry-ruby **5.24.0**, Rails **5.27.0**; `config.enable_logs = true` plus `Sentry.logger`; `config.enabled_patches << :logger` forwards all stdlib `Logger` instances; **5.28.0** (2025-09-26) auto-enables Rails structured logging (`Rails.logger` → Sentry) when `enable_logs` is true.

### Is Sentry Logs enough for "all my server logs"?
For **application** logs: yes — every language Matthias uses has a forwarding integration (Python logging/Loguru, Rails logger, tracing subscriber, console/Pino/Winston). For **system** logs (journald, nginx access, cron, postgres): **no**. No official journald→Sentry shipper exists; Sentry's own open issue "Log Drain Support for Sentry Logging" (getsentry/sentry#91726, open since 2025-05) shows non-SDK sources are an acknowledged gap, and their answer to it is the OTLP endpoint (below).

## 2. OTLP/HTTP logs

### Payload shape
- POST to default path **`/v1/logs`**; body is `ExportLogsServiceRequest` → `resource_logs[] → scope_logs[] → log_records[]`; each LogRecord: `time_unix_nano`, `severity_number` (1–24, same scale as Sentry), `severity_text`, `body` (AnyValue), `attributes` (KeyValue list), `trace_id`/`span_id` (bytes; hex in JSON). Content types: `application/x-protobuf` (default) or `application/json` (proto3 JSON mapping: camelCase, hex IDs); gzip supported; response is `ExportLogsServiceResponse` with `partial_success`. OTLP spec 1.11.0 is stable for logs.

### Minimal receiver in Rust
- The **opentelemetry-proto** crate (0.32.0) with `gen-tonic-messages` + `logs` gives the prost types; `prost::Message::decode` handles protobuf; feature `with-serde` adds spec-compliant JSON serde (verified by the crate's own `json_serde.rs` tests against ExportLogsServiceRequest). A topcoat handler that checks content-type, decodes, and flattens resource+scope attributes into rows is on the order of 100–150 lines. Vector's `src/sources/opentelemetry/http.rs` is working Rust prior art.
- **Key convergence fact**: Sentry now ingests OTLP logs itself (open beta) at `https://oX.ingest.sentry.io/api/<project_id>/integration/otlp/v1/logs` with header `x-sentry-auth: sentry sentry_key=<dsn-public-key>` (Relay PR #5130, merged 2025-09-15). If nashgit ever implements OTLP, copying this exact path + auth shape means the existing per-project DSN key doubles as the OTLP token, and Sentry's own docs/tooling conventions apply unchanged.

### Who can ship logs via OTLP
- OTel Collector (heavy for one VPS), **Vector** (journald source → `opentelemetry` sink, but that sink is **beta**; OTLP decoding highlight dated 2025-09-23), **fluent-bit** (systemd input + opentelemetry output; OTLP logs since PR #5747, 2022), **Grafana Alloy** (loki.source.journal + otelcol.exporter.otlphttp), and every OTel language SDK (`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` env var).
- **journald → OTLP on a single VPS is practical but adds an agent per box.** The lighter documented pattern is Vector's journald source → **stable `http` sink** posting NDJSON to any endpoint (Honeybadger publishes exactly this recipe: journald source, small remap, http sink with `encoding.codec: json`, `framing: newline_delimited`, an auth header). That pattern targets a bespoke endpoint just as easily as an OTLP one — and the http sink is stable while the OTLP sink is not.

## 3. Dead-simple custom HTTP JSON
All three small vendors do essentially the same thing:
- **Better Stack (Logtail)**: `POST https://<ingesting-host>` with `Authorization: Bearer <source-token>`, body = one JSON object or an array; optional `dt` timestamp; arbitrary nested fields.
- **Axiom**: `POST /v1/datasets/{dataset}/ingest`, Bearer token, JSON / NDJSON / CSV.
- **Seq**: `POST /ingest/clef`, NDJSON in CLEF — reified fields `@t` (timestamp), `@mt` (message template), `@m` (message), `@l` (level), `@x` (exception), everything else = properties; API key via `X-Seq-ApiKey` header. CLEF is a tiny public spec (clef-json.org) worth borrowing field names from.

Verdict: yes — for one person's servers a bespoke `POST /logs` with a token is the pragmatic industry-standard answer, ~50 lines of handler. Its only real cost is that nothing speaks it out of the box, so every producer needs a curl/Vector/shipper snippet — which the nashgit UAT/docs pages can hand out per project, exactly like DSNs today.

## Storage: logs in SQLite
- **Prior art says it works at this scale.** ChrisLog (Go: syslog/GELF/HTTP → SQLite, "one container, one binary, one file"), cortex (Rust: syslog + OTLP + Docker logs → SQLite with FTS), SolidLog (Rails HTTP ingestion + FTS, SQLite/PG/MySQL), loglens (multi-GB Laravel logs on a persistent FTS5 index), timeless_logs (Elixir: compressed raw blocks + SQLite index, ~12.8x compression), repartee (SQLite WAL + FTS + `retention_days` pruning). Counterpoint: **Seq** built a custom Rust storage engine because generic embedded engines didn't fit their write/query pattern — but that's at commercial multi-tenant volumes, not one tailnet.
- **Practical schema**: one `logs` table `(id INTEGER PK, project_id, ts REAL, severity_number INT, level TEXT, body TEXT, attributes JSON, trace_id, span_id, source TEXT)` — `source` distinguishes `sentry-sdk` vs `http` vs future `otlp`. Insert each envelope/batch in one transaction under WAL; that comfortably sustains tens of thousands of rows/sec, far beyond need.
- **FTS5**: use an **external-content** table (`CREATE VIRTUAL TABLE logs_fts USING fts5(body, content='logs', content_rowid='id')`) with insert/delete triggers — the text is stored once, and deletes work correctly. (Contentless tables only gained deletes via `contentless_delete=1` in SQLite 3.43; external-content remains the standard.) Run `INSERT INTO logs_fts(logs_fts) VALUES('optimize')` periodically.
- **Retention**, three proven strategies in ascending machinery:
  1. **DELETE + incremental vacuum**: nightly `DELETE FROM logs WHERE ts < cutoff` (per-project retention_days), with `PRAGMA auto_vacuum=INCREMENTAL` set at DB creation and `PRAGMA incremental_vacuum(N)` afterwards — or skip vacuum entirely and let SQLite reuse free pages (file size plateaus). Simplest; fine at this volume.
  2. **One DB file (or table) per period**: drop the whole file/table to expire a day/week — instant, no vacuum, no fragmentation (timeless_logs' block model is this idea). Only worth it if volume surprises.
  3. Keep logs in a **separate DB file from the error tracker** regardless, so log churn never bloats or write-locks the events DB — Bugsink's own "vacuum files: DB timeouts" issue (#372) is a cautionary tale about mixing heavy churn with app data. (No sign in Bugsink's docs, site, or issue tracker that Bugsink ingests Sentry Logs at all, as of 2026-08 — absorbing it into nashgit loses nothing on the logs front.)

## Recommendation
**Minimal combination, two pieces:**
1. **Accept the Sentry `log` envelope item on the existing DSN/envelope endpoint** (primary path). It is one new `match` arm in envelope parsing plus one table. Every project already holding a nashgit DSN then gets logs "for free" from unmodified official SDKs — Python (`sentry_sdk.logger` + `LoggingIntegration(capture_sentry_logs=True)`), JS (`enableLogs`), Rails (`enable_logs` + logger patch), and Rust via the sentry-tracing layer. This is the highest leverage per line of code, and it inherits trace_id correlation with the error events nashgit already stores.
2. **Add one bespoke NDJSON endpoint** — `POST /api/<project_id>/logs`, auth = the same DSN public key (header or query), body = NDJSON with CLEF-ish reified fields (`@t`/`ts`, `level`, `message`, rest = attributes), mapped into the same table with `source='http'`. This is the catch-all for journald (Vector journald source → stable http sink, per the Honeybadger recipe), nginx, cron jobs, and shell one-liners (`curl -d`). ~50 lines.

**Defer OTLP.** A full OTLP/HTTP receiver is only worth it when a standard agent (fluent-bit/Alloy/OTel SDK) must point at nashgit unmodified. When that day comes, implement it at Sentry's own path shape `/api/<project_id>/integration/otlp/v1/logs` with `x-sentry-auth: sentry sentry_key=...`, using opentelemetry-proto (`gen-tonic-messages`,`logs`,`with-serde`) — severity numbers and attributes will drop into the same table because the Sentry log model was copied from OTel's.

**Notifications**: keep Pushover wired to error EVENTS only by default; optionally allow per-project opt-in alerts on `fatal`-level logs — log volume makes per-log notification untenable.

### Sources
- https://develop.sentry.dev/sdk/telemetry/logs/
- https://docs.sentry.io/platforms/python/logs/
- https://github.com/getsentry/sentry-python/releases/tag/2.68.0
- https://docs.sentry.io/platforms/python/integrations/logging/
- https://docs.sentry.io/platforms/javascript/logs/
- https://github.com/getsentry/sentry-javascript/releases/tag/9.41.0
- https://docs.sentry.io/platforms/rust/logs/
- https://docs.rs/sentry/latest/sentry/integrations/tracing/
- https://crates.io/crates/sentry-tracing
- https://docs.sentry.io/platforms/ruby/logs/
- https://docs.sentry.io/platforms/ruby/guides/rails/logs/
- https://github.com/getsentry/sentry-ruby/releases/tag/5.28.0
- https://github.com/getsentry/sentry/issues/91726
- https://docs.sentry.io/concepts/otlp/direct/logs/
- https://github.com/getsentry/relay/pull/5130
- https://opentelemetry.io/docs/specs/otlp/
- https://docs.rs/crate/opentelemetry-proto/latest/features
- https://github.com/open-telemetry/opentelemetry-rust/blob/main/opentelemetry-proto/tests/json_serde.rs
- https://vector.dev/docs/reference/configuration/sources/journald/
- https://vector.dev/docs/reference/configuration/sinks/opentelemetry/
- https://vector.dev/highlights/2025-09-23-otlp-support/
- https://docs.honeybadger.io/guides/insights/integrations/systemd/
- https://docs.fluentbit.io/manual/data-pipeline/inputs/systemd
- https://github.com/fluent/fluent-bit/pull/5747
- https://datalust.co/docs/posting-raw-events
- https://clef-json.org/
- https://datalust.co/blog/a-tour-of-seqs-storage-engine
- https://betterstack.com/docs/logs/ingesting-data/http/logs/
- https://axiom.co/docs/restapi/ingest
- https://github.com/lanbugs/chrislog
- https://github.com/dinglebear-ai/cortex
- https://github.com/namolnad/solid_log
- https://timeless-logs.hexdocs.pm/readme.html
- https://www.sqlite.org/fts5.html
- https://www.bugsink.com/sentry-sdk-compatible/

## Grouping, alerting, retention

### Key facts
- Sentry's default grouping priority is: explicit `fingerprint` field, then stack trace (only in-app frames; per frame: module, normalized filename, normalized context line — never line numbers), then exception type+value, then message.
- The Sentry SDK event payload's `fingerprint` field is a list of strings with a `{{ default }}` sentinel meaning 'extend the server's default grouping'; Bugsink, GlitchTip, and Sentry all implement the same substitution, so nashgit must honor it for SDK compatibility.
- Bugsink's entire default grouper (v2, current) is: exception type + message value normalized by Sentry's vendored parameterizer (which replaces email/url/hostname/ip/uuid/sha1/md5/date/duration/hex/float/int/quoted_str/bool with placeholders, trimmed to 2 lines) — no stack-trace hashing at all; v1 additionally appended the transaction name.
- GlitchTip's fingerprint is `md5(title + culprit + event_type)` where title = 'Type: first line of value' (last exception in chain) and culprit = culprit || transaction || last in-app frame as module.function — also no multi-frame stack hashing.
- Both tools group on the LAST exception in the chain, trim value to 1024 / type to 128 chars, default type to 'Error', and substitute the crash-location function name for the type when `mechanism.synthetic` is set.
- Bugsink versions its grouping mechanisms (bugsink-v1/bugsink-v2) with per-project opt-in and a 30-day transition, because changing the algorithm in place splits every open issue — nashgit should store a grouping-key string plus a mechanism-version tag per issue from day one.
- Regression handling is universal: an event for a resolved issue reopens it and (in Bugsink, Errbit, Honeybadger, Sentry) always notifies; release-aware resolution ('resolved in release X') only counts events from later releases as regressions.
- Bugsink's alert policy is exactly three per-project booleans, all default true — alert_on_new_issue, alert_on_regression, alert_on_unmute — and muting supports 'for 1 day/week/month/3 months' or 'until N events per period' (presets: 5/hour, 5/24h, 100/24h); volume-based unmute doubles as the spike alert.
- Bugsink checks unmute-after only on ingest, so muted issues that stop erroring stay muted forever — mute-until means 'notify me only if this keeps happening'.
- Occurrence ladders are the standard flood control: Errbit defaults to email at the 1st/10th/100th occurrence (ERRBIT_EMAIL_AT_NOTICES='[1,10,100]'), Honeybadger notifies only on first occurrence and regression with opt-in re-notification at 10/100/1000 recurrences, and Sentry throttles per issue with an action interval (min 5 minutes, model default 30 minutes, max 30 days).
- GlitchTip has no first-seen alert: its only alerts are user-defined 'N events in M minutes' thresholds evaluated every 60 s, bundling multiple issues into one notification, and an issue ever notified by a given alert is permanently excluded from that alert's future notifications.
- Bugsink's retention default is 10,000 stored events per project with count-based (not time-based) eviction; GlitchTip instead keeps events 90 days (GLITCHTIP_RETENTION_DAYS).
- Bugsink's eviction score = nonzero_leading_bits(rand*2*issue_event_count) + log4(age_hours+1), so high-volume issues lose events first and ~oldest go next; each run evicts min(max(5% of quota, overage), 500) events, and first-seen/regression trigger events are flagged never_evict.
- Bugsink's ingest quota defaults (checked before parsing, returns HTTP 429): 1,000 events/5 min, 5,000/hour, 1,000,000/month per project and installation-wide, plus a global MAX_EMAILS_PER_HOUR of 60 as notification flood control.
- The simplest credible fingerprint for nashgit, matching both small-tracker implementations: hash of (exception type from last chain entry, parameterized message value, transaction-or-crash-frame culprit), with explicit-fingerprint override — top-N in-app frame hashing is the optional precision upgrade Sentry/Honeybadger/Errbit use, at the cost of minified-JS and wrapper-frame failure modes.

### Recommendations
- Implement grouping as a readable key string, not just a hash: `{type}: {parameterized_value}` (Bugsink v2 style), optionally `⋄ {transaction}`; store it with a mechanism-version tag (e.g. 'nashgit-v1') on each issue so the algorithm can evolve without splitting open issues.
- Port the message parameterization step (replace uuid/hex/int/float/ip/email/url/date/duration/quoted-str/bool with placeholders, keep first 2 non-empty lines) — it is the single change that makes message-based grouping hold up, and Bugsink ships Sentry's exact regex set to crib from.
- Honor the SDK `fingerprint` array with `{{ default }}` substitution (join parts with a separator, substitute the default key) — unmodified official SDKs send it and users expect it.
- Group on the LAST exception in the chain; use crash-location function name when mechanism.synthetic is set; treat non-exception events as a 'Log Message' type keyed by first message line; trim value/type to 1024/128 chars.
- Adopt Bugsink's exact alert trigger set for Pushover: new issue, regression (reopen resolved issue on any event, release-aware later), and unmute-by-volume — never notify per-event for an existing unresolved issue; this alone keeps a 1000×/min storm to ~1 message.
- Add an Errbit/Honeybadger-style escalation ladder as the 'still broken' signal: one extra Pushover message when an unresolved issue's event count crosses 10, 100, 1000 (configurable array, default [10, 100, 1000]).
- Implement mute with Bugsink's two forms — mute-for (time) and mute-until (N events per period) — and evaluate unmute only on ingest so dead issues stay muted; send the unmute notification with the 'more than N events per period occurred' reason.
- Add a global Pushover budget guard modeled on Bugsink's MAX_EMAILS_PER_HOUR=60: cap messages per hour (e.g. 10-20), send one final 'notifications suppressed, N pending — see dashboard' message when the cap trips; 10k/month ≈ 333/day so the trigger policy plus this cap leaves huge headroom.
- For retention, copy Bugsink's shape: per-project max stored events (10k default), a cheap pre-parse quota gate returning 429 (per-5-min/hour/month counters), never-evict flags on first-seen/regression trigger events, and eviction that preferentially deletes events of high-volume issues and old epochs — the full irrelevance algorithm is ~400 lines in /tmp/bugsink-src/events/retention.py and ports cleanly to rusqlite.
- Skip stack-trace-based hashing in v1; if issues over-split later, add an optional per-project 'hash top-5 in-app frames (module+function, no line numbers)' mechanism as nashgit-v2 — the versioned-mechanism field makes that a safe, deliberate migration.
- Keep the cloned reference sources (/tmp/bugsink-src, /tmp/glitchtip-src) around while implementing; the load-bearing files are issues/grouping_mechanisms/, issues/utils.py, issues/regressions.py, ingest/views.py (digest + VBC), events/retention.py in Bugsink and apps/event_ingest/utils.py + apps/alerts/ in GlitchTip.

### Detail

# Issue grouping, fingerprinting, and alert policy — prior art for nashgit's error tracker

Research date: 2026-08-18. Sources: live code from the Bugsink repo (cloned at `/tmp/bugsink-src`, GitHub main) and GlitchTip backend (cloned at `/tmp/glitchtip-src`, GitLab master), Errbit main branch on GitHub, Sentry docs + Sentry source (master and the 23.12.1 tag), Honeybadger docs.

Acronyms used: DSN (Data Source Name — the per-project ingest URL+key a Sentry SDK is pointed at), VBC (Volume-Based Condition — Bugsink's term for "N events per period" thresholds), SDK (Software Development Kit), M2M (many-to-many), PII (Personally Identifiable Information).

## 1. Sentry's grouping algorithm (the reference behavior)

From https://docs.sentry.io/concepts/data-management/event-grouping/:

- Every event gets a **fingerprint**; events with the same fingerprint form one issue. Priority order, all algorithm versions: **explicit `fingerprint` field → stack trace → exception (type+value) → message**.
- **Stack trace grouping**: when a stack trace exists, grouping is "effectively based entirely on the stack trace", and **only frames the SDK marks as in-app** are used (when that info exists). Per frame Sentry uses: **module name, normalized filename** (revision hashes etc. removed), and **normalized context line** (cleaned-up source of the offending line). Notably NOT the line number — line numbers churn on every deploy. Two documented failure modes: minified JavaScript (needs source maps) and decorator/wrapper frames (SDKs can hide frames, e.g. Python's `__traceback_hide__`).
- **Exception grouping** (no stack trace): `type` + `value`. Docs call this "a lot less reliable because of changing error messages".
- **Fallback**: message without parameters, else the full message.
- **Built-in fingerprinting rules** (server-side) special-case notorious noise like chunk-load and hydration errors. **AI-enhanced grouping** (SaaS) embeds message + in-app frames and merges semantically similar new hashes; it never applies to fully custom fingerprints.
- **Customization layers**: merge issues in UI, fingerprint rules (`error.type:ConnectionError -> connection-error`, `message:"fatal: *" -> fatal-log, {{ transaction }}`), stack trace rules (mark frames in-app/out).

**The SDK `fingerprint` event field** (https://develop.sentry.dev/sdk/data-model/event-payloads/): "A list of strings used to dictate the deduplication of this event", e.g. `["myrpc", "POST", "/foo.bar"]` or `["{{ default }}", "http://example.com/my.url"]`. The `{{ default }}` sentinel means "the server's default grouping, extended by these extra parts". Any Sentry-compatible server MUST honor this field — official SDKs expose `scope.fingerprint` and users use it.

## 2. Bugsink's grouper (small, quotable, SQLite-class prior art)

Bugsink does **no stack-trace hashing at all**. The grouping key is a human-readable string, not a hash, stored per issue via a `Grouping` lookup table.

`/tmp/bugsink-src/issues/grouping_mechanisms/v1.py` (default until v2.4.0, July 2026):

```python
def default_issue_grouper(data, calculated_type, calculated_value):
    title = get_title_for_exception_type_and_value(calculated_type, calculated_value)
    transaction = force_str(data.get("transaction") or "<no transaction>")
    return title + " ⋄ " + transaction
```

`v2.py` ("Value-normalized", current default; drops transaction, normalizes the message):

```python
def default_issue_grouper(data, calculated_type, calculated_value):
    calculated_value = normalize_message_for_grouping(force_str(calculated_value))
    return get_title_for_exception_type_and_value(calculated_type, calculated_value)
```

Where (`issues/utils.py`):
- `calculated_type/value` come from the **last exception in the chain** (deliberate choice, documented in `get_main_exception`); value trimmed to 1024 chars, type to 128, default type `"Error"`. If `mechanism.synthetic` is set, the crash-location **function name** replaces the type (so `SIGSEGV`-style synthetic errors group by function).
- Non-exception events group as type `"Log Message"` + first line of the message (≤1024 chars).
- Title = `"{type}: {first line of value}"`.
- `normalize_message_for_grouping` is **vendored verbatim from Sentry** (`/tmp/bugsink-src/sentry/at_597d25951d00/grouping/strategies/message.py`): trim to the first 2 non-empty lines, then replace `email, url, hostname, ip, uuid, sha1, md5, date, duration, hex, float, int, quoted_str, bool` with placeholders via Sentry's `Parameterizer`. This is the whole trick that makes message-based grouping credible — IDs, timestamps, and addresses no longer split issues.
- **Explicit fingerprint support** (`get_key_with_mechanism_for_data`): if `data["fingerprint"]` exists, join the parts with `" ⋄ "`, substituting the default grouper string for `"{{ default }}"`.
- **Versioned grouping mechanisms** (added July 2026, issues #440/#255): the grouping key per issue is computed by a named mechanism (`bugsink-v1`, `bugsink-v2`); changing the algorithm in place would split every open issue into "frozen before / fresh after", so projects opt in per-project with a 30-day transition period (`GROUPING_TRANSITION_PERIOD = timedelta(days=30)`). Lesson for nashgit: **store the grouping-mechanism version alongside the key from day one**.

Bugsink docs (https://www.bugsink.com/docs/grouping/) confirm: factors are exception type+value (last in chain) and transaction; log messages are a separate class; a "Grouping" tab shows the computed grouper string per issue.

## 3. GlitchTip's grouper

`/tmp/glitchtip-src/apps/event_ingest/utils.py`:

```python
def default_hash_input(title: str, culprit: str, type: "IssueEventType") -> str:
    return title + culprit + str(type)

def generate_hash(title, culprit, type, extra=None) -> str:
    """Generate insecure hash used for grouping issues"""
    if extra:
        hash_input = "".join(
            [default_hash_input(title, culprit, type)
             if part == "{{ default }}" else (part or "")
             for part in extra])
    else:
        hash_input = default_hash_input(title, culprit, type)
    return hashlib.md5(hash_input.encode()).hexdigest()
```

Called as `issue_hash = generate_hash(title, culprit, event.type, event.fingerprint)` in `process_event.py` (~line 963), where (vendored `sentry/eventtypes/error.py`):
- `title` = `"{type}: {first line of value}"` (last exception in chain; synthetic → function name; truncated).
- `culprit` = event `culprit` || `transaction` || `generate_culprit(data)` (Sentry's classic culprit: the most relevant in-app frame rendered as `module.function`), capped at MAX_CULPRIT_LENGTH.
- `type` = ERROR / DEFAULT / CSP enum.
- Fingerprint array handled with the same `{{ default }}` substitution.

So GlitchTip = **md5(type-and-message title + one location string + event class)**. Again: no multi-frame stack hashing, no line numbers, but — unlike Bugsink v2 — **no message parameterization**, so `"Timeout after 1523ms"` vs `"Timeout after 1611ms"` make two GlitchTip issues where Bugsink v2 makes one.

**Convergent conclusion:** both open-source SQLite/Postgres-class trackers independently landed on *exception type + message + (maybe) one location string*, plus the explicit-fingerprint override. The simplest credible approximation for nashgit is exactly that, with Bugsink's parameterization step, which is the single highest-value addition. Hashing top-N in-app frames (module+function, never line numbers) is the optional precision upgrade — it is what Sentry/Honeybadger/Errbit do — but its failure modes (minified JS, wrapper frames, missing in_app flags across SDKs) are why the small trackers skipped it.

## 4. Honeybadger and Errbit (fingerprint + escalation ladders)

**Honeybadger** (https://docs.honeybadger.io/guides/errors/): fingerprint = (1) file name, method name, and **line number** of the error's location, (2) error class, (3) component/controller. Custom fingerprint supported. Notification policy (FAQ): "**By default we only send notifications the first time an exception happens, and when it re-occurs after being marked resolved.**" Plus **Rate Escalation** (https://www.honeybadger.io/explain/rate-based-escalation/): opt-in extra notices "whenever an error reoccurs 10, 100, or 1000 times" (number configurable). "Resolve errors on deploy" auto-resolves everything on deploy so recurrences re-alert.

**Errbit** (`app/models/notice_fingerprinter.rb`, main branch):

```ruby
field :error_class, default: true
field :message, default: true
field :backtrace_lines, default: -1   # -1 = all lines
field :component, default: true
field :action, default: true
field :environment_name, default: true

def generate(api_key, notice, backtrace)
  material = [api_key]
  material << notice.error_class if error_class
  material << notice.filtered_message if message
  material << notice.component if component
  material << notice.action if action
  material << notice.environment_name if environment_name
  material << (backtrace_lines < 0 ? backtrace.lines : backtrace.lines.slice(0, backtrace_lines)) if backtrace
  Digest::MD5.hexdigest(material.join)
end
```

Each factor is toggleable per app; `backtrace_lines` can be capped to top-N. `filtered_message` strips Ruby object addresses: `message.gsub(/(#<.+?):[0-9a-f]x[0-9a-f]+(>)/, '\1\2')` — a one-regex ancestor of Sentry's parameterizer. Notification policy (`error_report.rb`): `should_email? = problem_was_resolved || email_at_notices.include?(0) || email_at_notices.include?(problem.notices_count)` — i.e. **always notify on regression**, else notify only when the occurrence counter hits a configured value. Defaults (`.env.default`): `ERRBIT_EMAIL_AT_NOTICES='[1,10,100]'` (email at 1st, 10th, 100th occurrence — the classic escalation ladder), `ERRBIT_NOTIFY_AT_NOTICES='[0]'` for chat/webhook services (0 = every notice).

## 5. Issue lifecycle and muting

**States across tools:** unresolved (open) / resolved / muted-or-archived, plus derived "regressed" and (Sentry) "escalating".

**Regression = event arrives for a resolved issue → reopen + notify.** All four tools implement this:
- **Bugsink** (`ingest/views.py` digest path): new issue → `TurningPoint(FIRST_SEEN)` + `send_new_issue_alert` if `project.alert_on_new_issue`. Existing resolved issue → `issue_is_regression()` → `TurningPoint(REGRESSED)` + `send_regression_alert` + `IssueStateManager.reopen(issue)`. With releases (`issues/regressions.py`): "resolved unconditionally" → any event is a regression; "resolved in release X" → only events from releases at/after X count (a linear walk over ordered releases tracking marked_as_resolved flips); "resolved by next release" → never regresses until that release exists.
- **GlitchTip** (`process_event.py` ~1056): if the hash maps to a RESOLVED issue, reopen (status → UNRESOLVED) unless the event's release equals `resolved_in_release_id`. No dedicated regression notification — alerts are only volume thresholds.
- **Errbit/Honeybadger**: regression always notifies (see above).
- **Sentry** (states-triage doc): plain Resolve treats any later event as a regression; resolve-in-release compares versions.

**Muting:**
- **Bugsink** is the richest small-tracker model. Mute is exclusive with resolved. UI presets (`issues/views.py GLOBAL_MUTE_OPTIONS`): mute **for** 1 day / 1 week / 1 month / 3 months, or mute **until** "5 events per 1 hour" / "5 events per 24 hours" / "100 events per 24 hours" (VBCs, stored as JSON on the issue). Semantics worth copying: `unmute_after` is only checked **on ingest** — an issue that stops erroring stays muted forever ("things that no longer happen should not draw your attention"; the button means "I suppose this will self-resolve in X time; notify me if not"). VBC unmute fires `send_unmute_alert` with reason text like "More than N events per M period occurred" — so mute-until-volume doubles as a **volume-spike alert**. The VBC check is amortized: `next_unmute_check` skips counting until the issue's event counter could possibly cross the threshold.
- **Sentry**: Archive pauses alerts. Default = "archived until escalating"; options: forever / a set time / until it occurs N times / until N users are affected. The **escalating algorithm** (docs) builds per-issue hourly thresholds from the previous week's history; exceeding them flips the issue to Escalating and surfaces it again. Archived issues never trigger alerts.

## 6. Alert policy prior art (what to send, when)

- **Sentry default alert rule** (verified in source, tag 23.12.1 `src/sentry/receivers/rules.py`): label "Send a notification for new issues", condition `FirstSeenEventCondition`, action = email issue owners. Since late 2023, new python/js projects instead get "Send a notification for high priority issues" (new OR existing high-priority; the current workflow-engine version has `config={"frequency": 0}`). Legacy issue alerts have a per-issue **action interval** ("perform the actions at most once every X per issue") — API bounds 5 minutes to 30 days (`project_rules.py`: "The valid range is 5 to 43200" minutes); the Rule model default is 30 minutes. This per-issue throttle is Sentry's core flood control.
- **Bugsink**: three booleans per project, all default True: `alert_on_new_issue`, `alert_on_regression`, `alert_on_unmute`. That is the entire trigger set — no per-event alerts, no thresshold alerts except via mute-until-VBC. Backends: email plus per-project webhook services (Slack-compatible, Discord, Mattermost, MS Teams, Telegram, custom webhook — `alerts/service_backends/`). Global email flood-guard: `MAX_EMAILS_PER_HOUR: 60` ("Sending more than 1 email per minute sustained is self-spam territory").
- **GlitchTip**: alerts are user-created `ProjectAlert` rows: "Send notification when project has {quantity} events in {timespan_minutes} minutes" (docstring example: 15 events in 5 minutes), plus optional uptime alerts. A scheduled task runs every `ALERT_NOTIFICATION_INTERVAL` (default 60 s), finds issues whose event count in the window ≥ quantity, and — the key dedup — `.exclude(notification__project_alert=alert)`: **an issue that was ever included in a notification for that alert is never re-notified by it**. One `Notification` can bundle many issues (M2M) → natural rollup/digest. Per-user per-project on/off switch (`UserProjectAlert`).
- **Honeybadger**: first occurrence + regression only, plus opt-in rate escalation at 10/100/1000 recurrences.
- **Errbit**: regression always; occurrence-ladder `[1,10,100]` for email; `[0]` (= every notice) default for chat services.

**Synthesis — the consensus policy for a Pushover budget of 10k msgs/month (~333/day):**
1. Notify on **first-seen** (new issue). New-issue creation is inherently deduplicated by grouping, so this is cheap: even a bad day produces tens, not thousands.
2. Notify on **regression** (resolved issue re-occurs). Universal across all five tools.
3. Notify on **unmute/escalation** (muted issue crosses "N events per period", Bugsink-style; or Errbit/Honeybadger-style ladder at the 10th/100th/1000th event of an unresolved issue — "notify once, then escalate count" is exactly this ladder).
4. Never notify per-event for an existing unresolved issue. A 1000×/min error storm then costs 1 message (first seen) + at most the ladder steps (~3 more), no matter the volume.
5. Belt-and-suspenders: a per-issue action interval (Sentry: ≥5 min, default 30 min) and a global cap (Bugsink: 60 emails/hour) — for Pushover, something like max ~10 msgs/hour with a "notifications suppressed, see dashboard" final message.

## 7. Retention / quota in SQLite-class storage

**Bugsink** (the directly applicable prior art — Bugsink's recommended deployment is SQLite):
- Per-project `retention_max_event_count`, **default 10,000 events**; optional global clamps `MAX_RETENTION_PER_PROJECT_EVENT_COUNT` / `MAX_RETENTION_EVENT_COUNT` (default None = no limit).
- **Ingest quotas** (drop with HTTP 429 before parsing, checked at installation and project level; `bugsink/app_settings.py`): `MAX_EVENTS_PER_PROJECT_PER_5_MINUTES: 1_000`, `MAX_EVENTS_PER_PROJECT_PER_HOUR: 5_000`, `MAX_EVENTS_PER_PROJECT_PER_MONTH: 1_000_000` (same numbers installation-wide). Comment: sized at ~6% / ~2.7% / 0.8% of a measured 50 events/s capacity on low-grade hardware. Quota checks are amortized via `next_quota_check` counters, not per-event GROUP BYs.
- **Eviction** (`events/retention.py`, well worth reading whole): runs when `stored_event_count > max`. Each event gets a fixed-at-digest `irrelevance_for_retention = nonzero_leading_bits(random()*2*issue_stored_event_count)` — roughly log2 of the issue's event count with jitter, so **events of high-volume issues are the first to go** (per-issue fairness without a hard per-issue cap). Age adds `log4(age_in_hours + 1)` (an event 1 year old adds ~6.5). Eviction lowers a total-irrelevance cutoff until it has deleted `min(max(5% of max, overage), 500)` events. Events that triggered a TurningPoint (first-seen, regression, unmute) get `never_evict=True` and survive until nothing else is left. There is **no per-issue max**; the irrelevance distribution does that job statistically.
- Time-based retention: none by default — count-based only.

**GlitchTip**: time-based, `GLITCHTIP_RETENTION_DAYS` default **90 days** (legacy `GLITCHTIP_MAX_EVENT_LIFE_DAYS`), separate knobs for events/transactions/spans; Postgres partition drops do the deletion. Install docs note ~30 GB disk for a 1M events/month instance.

**Honeybadger** (SaaS): retention is a per-project setting on the plan.

For nashgit-on-SQLite the Bugsink model maps 1:1: per-project max event count (10k default), pinned first-seen/regression events, a coarse quota gate returning 429 in front of the digest path, and irrelevance-based eviction (or the simpler v0: delete oldest events of the biggest issues first, which is the same idea minus the elegance).

## Notes on verification

Every code claim above was read from the cloned repos (`/tmp/bugsink-src`, `/tmp/glitchtip-src`) or raw GitHub files on the named branch/tag, not from blog posts. Bugsink code is current as of its grouping-mechanisms rework (v2.5.0, 21 July 2026). Sentry's default-rule constants were verified in both the 23.12.1 tag (legacy `FirstSeenEventCondition`) and current master (workflow-engine "high priority issues"). Honeybadger notification defaults come from its own docs FAQ; Errbit defaults from `.env.default` in its main branch.

### Sources
- https://github.com/bugsink/bugsink (cloned to /tmp/bugsink-src: issues/grouping_mechanisms/{v1,v2,__init__}.py, issues/utils.py, issues/regressions.py, issues/models.py, issues/views.py, ingest/views.py, alerts/tasks.py, alerts/service_backends/, events/retention.py, projects/models.py, bugsink/app_settings.py, sentry/at_597d25951d00/grouping/strategies/message.py)
- https://gitlab.com/glitchtip/glitchtip-backend (cloned to /tmp/glitchtip-src: apps/event_ingest/utils.py, apps/event_ingest/process_event.py, apps/alerts/{models,tasks,schema}.py, sentry/eventtypes/error.py, glitchtip/settings.py)
- https://docs.sentry.io/concepts/data-management/event-grouping/
- https://docs.sentry.io/concepts/data-management/event-grouping/fingerprint-rules/
- https://docs.sentry.io/product/issues/states-triage/
- https://docs.sentry.io/product/issues/states-triage/escalating-issues/
- https://develop.sentry.dev/sdk/data-model/event-payloads/
- https://raw.githubusercontent.com/getsentry/sentry/23.12.1/src/sentry/receivers/rules.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/workflow_engine/defaults/workflows.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/api/endpoints/project_rules.py
- https://raw.githubusercontent.com/errbit/errbit/main/app/models/notice_fingerprinter.rb
- https://raw.githubusercontent.com/errbit/errbit/main/app/models/error_report.rb
- https://raw.githubusercontent.com/errbit/errbit/main/app/models/notice.rb
- https://raw.githubusercontent.com/errbit/errbit/main/.env.default
- https://docs.honeybadger.io/guides/errors/
- https://docs.honeybadger.io/lib/python/support/faq/
- https://www.honeybadger.io/explain/rate-based-escalation/
- https://www.bugsink.com/docs/grouping/
- https://github.com/bugsink/bugsink/issues/440
- https://glitchtip.com/documentation/error-tracking

## Gap follow-up: How do Sentry SDK clients that are NOT on the tailnet reach nashgit's ingest endpoint — and what does the cuto

### Key facts
- A public HTTPS ingest edge is unavoidable: iOS apps on end users' phones and Cloudflare Workers cannot join a tailnet, so at least those two client classes must POST to a public endpoint no matter what nashgit does.
- Tailscale Funnel can only use <node>.<tailnet>.ts.net hostnames, only ports 443/8443/10000, and traffic is subject to non-configurable bandwidth limits; it is still marked beta (docs validated Jan 20, 2026) — so Funnel can never preserve the nac-bugs.fly.dev hostname, forcing DSN rotation across the whole fleet including shipped iOS binaries.
- Funnel and Serve cannot share a port: whichever command ran last wins and a funneled port is completely public — so the safe layout is UI on `tailscale serve :443` and a separate ingest-only loopback listener funneled on :8443 (path-mounting works, but `--set-path` strips the mount prefix before proxying, per merged PR tailscale/tailscale#7334).
- Sentry DSN semantics make hostname preservation the whole ballgame: the SDK builds `{PROTOCOL}://{HOST}{PATH}/api/{PROJECT_ID}/envelope/` from the DSN, auth is just the public `sentry_key` (query param or X-Sentry-Auth header), and the secret key is deprecated — so if nashgit imports Bugsink's existing project IDs + keys and something keeps answering at nac-bugs.fly.dev, zero clients need redeploying.
- The cheapest hostname-preserving edge is to keep the existing `nac-bugs` Fly app but replace Bugsink with a tiny forwarder that joins the tailnet: Tailscale officially documents tailscaled-in-a-Fly-container (userspace networking + SOCKS5), and tsnet lets a single Go binary join the tailnet in-process with no daemon — the forwarder then reverse-proxies envelope POSTs to nashgit over the tailnet while nashgit stays loopback-bound behind tailscale serve.
- Fly's edge injects `Fly-Client-IP` with the real client IP, enabling per-IP rate limiting at the forwarder; behind Funnel, client-IP visibility is murkier (maintainer says tailscaled injects X-Forwarded-For for HTTP proxying, but FR #12972 asking to expose the funnel requestor's IP is still open) — Fly gives strictly better DoS tooling.
- Browser SDK CORS contract (what nashgit's ingest must return): `Access-Control-Allow-Origin: *` on POST responses, `Access-Control-Expose-Headers: x-sentry-error, x-sentry-rate-limits, retry-after` (the SDK reads the last two), and an OPTIONS handler allowing method POST plus headers x-sentry-auth, x-requested-with, x-forwarded-for, origin, referer, accept, content-type, authentication, authorization, content-encoding, transfer-encoding with max-age 3600 — this is verbatim what official Relay's cors.rs does.
- The browser SDK deliberately avoids preflight in the common path: fetch POST with no custom headers, auth moved into the query string (`?sentry_key=...&sentry_version=7`) precisely because X-Sentry-Auth would trigger OPTIONS (getsentry/sentry-javascript#1992); but preflights still occur in edge cases (Chrome CORS quirks, relay#1809), so the OPTIONS route is required, not optional.
- The JS SDK `tunnel` option routes browser envelopes through the app's own same-origin backend, which converts every browser-side Next.js client into a server-side client — an alternative to CORS-on-the-public-edge for the web apps Matthias controls.
- Official Sentry Relay in proxy mode forwards envelopes with minimal processing and no project-config fetch (no upstream registration), so it could sit in front of nashgit — but it must run somewhere public anyway (i.e., on Fly), it is memory-hungry, it historically dropped non-error item types in proxy mode, and neither Bugsink nor GlitchTip document Relay support; it solves nothing a 100-line forwarder doesn't.
- Cloudflare Workers reaching a tailnet is not production-viable: the only tailnet-native path is @ts-edge/cloudflare (experimental WASM tailscale node in a Worker, v0.3.0, ~22 weekly downloads); Cloudflare's supported answer is Workers VPC (beta) which requires running cloudflared — a second overlay network — next to nashgit.
- Tailnet-joining the server-side fleet (Fly apps via tailscaled sidecar, EC2 natively) works and is officially documented, but it only covers servers — it leaves browsers, Workers, and iOS stranded, so it cannot be the primary answer, only an optional optimization.
- Bugsink upstream acknowledges DSN-preserving migration as a real pattern (open issue bugsink/bugsink#218: update a project's DSN so a replacement server at the old domain keeps old DSNs working) — the same trick in reverse is exactly what the nashgit cutover needs.
- Because the DSN public key ships inside browsers and iOS binaries, it is a routing identifier, not a secret; public-edge defense must be quotas per key, request-size caps, unknown-key rejection at the forwarder (before traffic enters the tailnet), and per-IP throttling at the Fly edge.

### Recommendations
- Choose the Fly forwarder topology: keep the existing nac-bugs Fly app, replace Bugsink's image with a small tsnet-based Go forwarder that joins the tailnet and reverse-proxies /api/{id}/envelope/ to nashgit over tailscale serve — this preserves every deployed DSN (hostname + key + project id) and requires zero client redeploys, which the iOS fleet makes near-mandatory.
- Have nashgit's DSN minting accept explicit (project_id, sentry_key) pairs so Bugsink's existing projects can be imported verbatim before cutover; new projects get freshly minted keys.
- Implement the browser CORS contract natively in nashgit's ingest routes, copied from Relay's cors.rs: ACAO *, allow-method POST, the 11-header allow list, expose x-sentry-error/x-sentry-rate-limits/retry-after, max-age 3600, plus an OPTIONS handler; accept sentry_key via both X-Sentry-Auth header and query string, and handle gzip/deflate bodies.
- Put all public-surface defense in the forwarder, outside the tailnet: reject unknown sentry_keys against an allowlist, cap request body size, rate-limit per Fly-Client-IP and per key, and return 429 + X-Sentry-Rate-Limits so official SDKs back off on their own.
- Tag the forwarder's tailnet node and ACL it to reach exactly one host:port (nashgit's serve endpoint); use a reusable pre-authorized ephemeral auth key stored as a Fly secret.
- Reject the Funnel-as-primary-edge option (ts.net-only hostnames force fleet-wide DSN rotation, beta status, non-configurable bandwidth caps, unclear client-IP visibility) and the Relay-proxy-mode option (heavyweight, must run on public infra anyway, no documented non-Sentry-upstream support) — but keep Relay's cors.rs and rate-limit headers as the reference implementation for nashgit's own ingest.
- Do not tailnet-join the app fleet as the primary answer; optionally move individual high-volume Fly/EC2 apps onto the tailnet later. Cloudflare Workers and browsers stay on the public edge; for owned Next.js apps, the SDK `tunnel` option is an optional way to route browser envelopes through their own backends.
- Sequence the cutover for reversibility: build ingest + import keys → deploy forwarder as a NEW Fly app and verify one test project end-to-end per client class (browser CORS, server, Worker, iOS, Pushover) → fly deploy the forwarder into the nac-bugs app (instant, reversible by redeploying Bugsink) → archive the Bugsink DB.
- Before cutover, confirm nashgit runs on an always-on tailnet node; SDKs drop events when the ingest edge can't reach it.
- During implementation, empirically verify two details flagged as ambiguous upstream: whether X-Forwarded-For carries the real public client IP behind Funnel (only matters if Funnel is ever used as a fallback), and exact Bugsink schema/table names for the project-key export.

### Detail

# How off-tailnet Sentry clients reach nashgit, and what the nac-bugs cutover looks like

## The fleet, decomposed by physical reachability

Per the nac-bugs-wire skill, the client fleet is: apps on Fly.io and public EC2, Cloudflare Workers, browser-side Next.js, and iOS apps on end users' phones.

| Client class | Can join tailnet? | Can use a public edge? | Notes |
|---|---|---|---|
| Fly.io apps | Yes (tailscaled sidecar, officially documented) | Yes | Either path works |
| Public EC2 apps | Yes (native tailscaled) | Yes | Either path works |
| Cloudflare Workers | No (only experimental WASM hack or Workers VPC beta + cloudflared) | Yes | Public edge required in practice |
| Browser Next.js | No | Yes (needs CORS) | OR `tunnel` option through the app's own backend |
| iOS end-user apps | No | Yes | DSN baked into shipped binary; hostname stability is critical |

**Conclusion that frames everything: a public HTTPS ingest edge is unavoidable.** iOS alone forces it. The real question is *which* public edge, and whether it preserves the `nac-bugs.fly.dev` hostname so existing DSNs survive.

## Option 1 — Tailscale Funnel exposing only the ingest path

What the docs (validated Jan 20, 2026; Funnel still **beta**) actually say:

- Funnel can only use DNS names in the tailnet's domain (`<node>.<tailnet>.ts.net`). **No custom hostnames — `nac-bugs.fly.dev` can never live on Funnel.**
- Funnel can only listen on ports **443, 8443, 10000**, TLS only.
- Traffic is subject to **non-configurable bandwidth limits**.
- **The same port cannot be Serve and Funnel at once.** "If the most recent command to configure the port was `funnel`, then the port will be completely public." (Confirmed by tailscale/tailscale#11009: last command wins per port.)
- Requires the `funnel` node attribute in the tailnet policy file and HTTPS certificates enabled (Let's Encrypt per-node cert; rate-limit footgun if reissued frequently).
- macOS host caveat: port sharing via Funnel needs the App Store or standalone variant of the client.

Path scoping *does* work in the sense that only mounted handlers exist on the funneled port — but with two catches:

1. `--set-path=/api` **strips the mount prefix** before proxying (merged PR tailscale/tailscale#7334: "proxied services receive requests as if they were running at the root path"), so nashgit would see `/{project_id}/envelope/` instead of `/api/{project_id}/envelope/`. Cleanest fix: nashgit binds a **second loopback listener that serves only the ingest routes**, mounted at `/` on the funnel port. Then the funneled surface is exactly the ingest router and nothing else.
2. Because serve and funnel can't share a port, the layout is: full UI on `tailscale serve :443` (tailnet-only, unchanged), ingest-only listener on `tailscale funnel --https=8443` → DSNs look like `https://KEY@nashgit-host.tailnet.ts.net:8443/PROJECT_ID` (DSN host may include a port; SDKs handle it).

Client IP for rate limiting: contradictory signals. bradfitz commented (June 2025, #13809) that "tailscaled already injects an `X-Forwarded-For: $IPAddress` HTTP request header into the backend when it's proxying an HTTP connection," but FR #12972 ("expose Funnel requestor's IP address to backend", filed explicitly for DDoS/rate-limiting) is **still open**. Read: XFF is probably present for HTTP-proxy mode, absent for raw TCP mode — verify empirically before relying on per-IP limits behind Funnel.

**Verdict:** works, zero new infra, but it forces a **DSN hostname rotation across the entire fleet** — including iOS App Store releases and every deployed Fly/EC2/Workers secret — puts a beta feature with unpublished bandwidth caps in the hot path, and gives the weakest DoS tooling. It also permanently welds ingest availability to the nashgit host being up and on the tailnet.

## Option 2 — Tiny public forwarder on Fly, keeping the nac-bugs.fly.dev hostname (recommended)

DSN mechanics make this the killer option. Per develop.sentry.dev, the SDK builds its endpoint as `{PROTOCOL}://{HOST}{PATH}/api/{PROJECT_ID}/envelope/` from the DSN, and auth is the public `sentry_key` sent either in the `X-Sentry-Auth` header or the query string; the secret key is deprecated. So a DSN survives migration iff (a) the hostname keeps answering and (b) the same `project_id` + `sentry_key` pairs remain valid. Bugsink upstream itself recognizes this pattern — open issue bugsink/bugsink#218 is exactly "update a project's DSN … migrating while preserving the dsn — this assumes [the old server] is replaced at the old domain."

Shape:

1. **nashgit imports Bugsink's project IDs and keys.** Bugsink is Django on SQLite/Postgres that Matthias self-hosts; dump the projects table, and have nashgit's DSN-minting store an arbitrary (project_id, sentry_key) pair rather than always generating fresh ones.
2. **Replace the Bugsink image in the existing `nac-bugs` Fly app with a forwarder.** Same app name → same `nac-bugs.fly.dev` hostname and automatic TLS. Nothing anywhere in the fleet changes.
3. **The forwarder joins the tailnet and reverse-proxies to nashgit.** Two documented implementation paths:
   - **tsnet (cleanest):** a single Go binary embeds a Tailscale node in-process (userspace gVisor stack, no daemon, no root — per the tsnet README). It listens on Fly's public port and `httputil.ReverseProxy`-es `POST /api/{id}/envelope/` to `https://nashgit-host.tailnet.ts.net/...` (nashgit stays loopback-bound; tailnet exposure stays via `tailscale serve`, exactly today's model).
   - **tailscaled sidecar (official Tailscale-on-Fly doc, validated Dec 4, 2025):** copy tailscaled/tailscale binaries into the container, run with `--tun=userspace-networking --socks5-server=localhost:1055`, app dials tailnet through the SOCKS5 proxy. Use a reusable pre-authorized **ephemeral** auth key; tag the node so ACLs can restrict it to only reach nashgit's serve port.
4. **DoS defense lives at the forwarder, outside the tailnet:** Fly injects `Fly-Client-IP` (documented request header) → real per-IP throttling; reject unknown `sentry_key`s against a small allowlist synced from nashgit; cap body size; drop non-ingest paths. Hostile traffic never touches the tailnet.
5. Tailnet ACL: grant the forwarder's tag access to exactly one host:port. If the forwarder is compromised, the blast radius is "can POST envelopes to nashgit" — the same thing the public already can do.

**Verdict: best option.** Zero client redeploys, zero DSN rotation (decisive for iOS), real IP-based rate limiting, keeps nashgit loopback-bound with Tailscale-headers identity untouched, and reuses the Fly app already being paid for. Cost: one ~100-line Go binary (tsnet) and a tailnet ephemeral-key secret on Fly. It is a long-term architecture, not a stopgap: "public ingest edge at the stable hostname, storage/UI on the tailnet" is exactly how Sentry itself separates `oXXX.ingest.sentry.io` from `sentry.io`.

## Option 3 — Official Sentry Relay in proxy mode as the public edge

Facts (docs.sentry.io/product/relay/modes, relay_config rustdoc):

- `mode: proxy` "forwards all events with minimal processing and does not receive any project settings from Sentry"; no upstream registration/auth (that's managed mode only); upstream rate limits (429/X-Sentry-Rate-Limits) are enforced; `processing.enabled` must be false (hard error otherwise, relay#3580). Unknown envelope item types are forwarded by default outside processing mode (`Routing.accept_unknown_items` defaults true).
- Relay's stated use case includes "act as an opaque proxy for organizations that restrict all HTTP communication to a custom domain name" — pointing `upstream:` at any envelope-accepting server is mechanically fine, and community guides do it.
- But: proxy mode historically dropped non-error item types (metrics relay#3042, replays relay#3175/#3800 — since fixed for replays/profiles); memory footprint drew complaints (#3012) and a "light proxy mode" FR (#3021); and **neither Bugsink nor GlitchTip documents Relay support** (bugsink.com has no relay page at all) — non-Sentry upstreams are untested territory owned by nobody.

**Verdict: rejected.** Relay must run on public infra anyway (i.e., on Fly), so it competes directly with the tiny forwarder — and loses: heavier, less controllable, adds Sentry-internal semantics (its own buffering, its own rate-limit interpretation) between SDKs and nashgit, and its CORS/auth behavior is the part nashgit must implement natively regardless. Its one genuine value here is as a **reference implementation** (see CORS contract below).

## Option 4 — Tailnet-join the server-side clients

Officially supported for both Fly (sidecar doc above) and EC2 (normal install). But it covers only Fly/EC2 apps; browsers, Workers, and iOS remain stranded, and it adds a node key + tailscaled lifecycle to every app container. Use it selectively later if Matthias wants some high-volume internal app off the public edge; it cannot be the primary topology.

## Cloudflare Workers specifically

- **No production-grade tailnet path exists.** The only tailnet-native attempt is `@ts-edge/cloudflare` (npm, v0.3.0, July 2026): a WASM ephemeral Tailscale node inside a Worker/Durable Object — ~22 weekly downloads, no README, experimental.
- Cloudflare's supported private-access mechanism is **Workers VPC (beta, 2026)**: Worker `fetch()` through a **Cloudflare Tunnel** — which means running `cloudflared` on/near the nashgit host, i.e., a second overlay network alongside Tailscale, for one client class.
- Practical answer: Workers keep POSTing to the public edge. With Option 2 they need no change at all.

## Browser CORS: the exact server contract nashgit must implement

How the browser SDK actually sends (verified in `packages/browser/src/transports/fetch.ts` and sentry-javascript#1992):

- `fetch(url, { method: 'POST', body, referrerPolicy: 'strict-origin', keepalive: ... })` — **no custom headers by default**; auth goes in the **query string** (`?sentry_key=...&sentry_version=7&sentry_client=...`) precisely so the request stays a CORS "simple request" and avoids preflight (body is a string → `text/plain;charset=UTF-8`, or a Uint8Array).
- The SDK **reads `X-Sentry-Rate-Limits` and `Retry-After` from the response** to do native backoff — those must be CORS-exposed or backoff silently breaks.
- Preflights still happen in edge cases (Chrome version quirks — relay#1809 — or user-supplied `fetchOptions`/`headers`), so OPTIONS handling is required.

What official Relay returns (verbatim from `relay-server/src/middlewares/cors.rs`, master) — copy this into topcoat middleware on the ingest routes:

```
Access-Control-Allow-Origin: *            (Any)
Access-Control-Allow-Methods: POST
Access-Control-Allow-Headers: x-sentry-auth, x-requested-with, x-forwarded-for,
  origin, referer, accept, content-type, authentication, authorization,
  content-encoding, transfer-encoding
Access-Control-Expose-Headers: x-sentry-error, x-sentry-rate-limits, retry-after
Access-Control-Max-Age: 3600
```

(The same allow-headers list appears in develop.sentry.dev's transport docs as "permitted as per CORS policy" — the two sources agree.) Wildcard origin is correct: auth on this surface is the sentry_key, never the Origin. Note whichever tier terminates the browser request must emit these — with Option 2 either the forwarder adds them or (simpler) nashgit emits them and the forwarder passes them through.

**Escape hatch for owned web apps:** the JS SDK `tunnel` option posts envelopes to a same-origin path on the app's own backend, which forwards server-side. That converts the browser class into the server class (and defeats ad-blockers). Optional under Option 2, since CORS + preserved DSNs already work.

## Ingest auth details nashgit must accept (all clients)

- `X-Sentry-Auth: Sentry sentry_key=..., sentry_version=7, sentry_client=...` header (server SDKs) **and** the same fields as query params (browser SDKs). `sentry_secret` is deprecated — accept and ignore.
- Endpoint path: `/api/{project_id}/envelope/` (project_id is a string per spec). Older SDKs may hit `/api/{id}/store/` — Bugsink supports both; cheap to add.
- Bodies are often gzip/deflate (`Content-Encoding`) — reqwest-side no issue, but the ingest route must decompress.
- Respond 429 + `X-Sentry-Rate-Limits` to shed load; every official SDK backs off natively.

## Cutover plan (Option 2)

1. **nashgit:** implement ingest (envelope parse, per-project sentry_key auth via header *and* query, CORS middleware above, gzip, 429 rate-limit responses); add a second ingest-only loopback listener if Funnel fallback is ever wanted; add "import project with explicit id+key".
2. **Export from Bugsink** (`nac-bugs.fly.dev`): dump projects table (id, name, sentry_key) from its DB; import into nashgit so every existing DSN validates.
3. **Build the forwarder** (tsnet Go binary): public listener → allowlist sentry_key → size cap → per-IP limit via `Fly-Client-IP` → proxy to `https://<nashgit-host>.<tailnet>.ts.net/api/...`; tag its tailnet node, ACL it to that one destination; use a reusable ephemeral auth key stored as a Fly secret.
4. **Dry-run:** deploy forwarder as a *new* Fly app first; point one test project's DSN at it; verify a browser client (CORS), a Rust/server client, a Worker, and an iOS build end-to-end, including Pushover firing from nashgit.
5. **Cutover:** `fly deploy` the forwarder image into the existing `nac-bugs` app (hostname and cert carry over). Instant, reversible by redeploying the Bugsink image.
6. **Verify the fleet** with the nac-bugs-wire skill's per-platform checks; keep the Bugsink DB file archived for history (nashgit doesn't need to import old events unless wanted).
7. **Decommission** Bugsink compute; the Fly app lives on as the ~free-tier forwarder.

## Risks / open items

- **Forwarder availability:** ingest now depends on Fly + tailnet connectivity between forwarder and the nashgit host. SDKs buffer briefly and drop on failure — acceptable for error telemetry, but note the nashgit host (if it's a laptop) sleeping means dropped events; consider running nashgit on an always-on node before cutover.
- **Funnel numbers unpublished:** if Funnel is chosen anyway, its bandwidth caps are non-configurable and undocumented; treat as unsuitable for bursty ingest.
- **Client IP behind Funnel:** unresolved upstream (#12972 open); don't design per-IP limits around Funnel.
- **`fly.dev` hostname coupling:** long-term, a custom domain on the forwarder (Fly supports certs for custom domains) would decouple DSNs from Fly — worth doing for *new* DSNs nashgit mints, while old fly.dev DSNs keep working indefinitely.

### Sources
- https://tailscale.com/docs/features/tailscale-funnel
- https://tailscale.com/docs/reference/tailscale-cli/funnel
- https://tailscale.com/blog/reintroducing-serve-funnel
- https://github.com/tailscale/tailscale/issues/11009
- https://github.com/tailscale/tailscale/pull/7334
- https://github.com/tailscale/tailscale/issues/12972
- https://github.com/tailscale/tailscale/issues/13809
- https://github.com/tailscale/tailscale/issues/12413
- https://tailscale.com/docs/install/cloud/flydotio
- https://tailscale.com/docs/concepts/userspace-networking
- https://github.com/tailscale/tailscale/tree/main/tsnet
- https://community.fly.io/t/connecting-your-fly-apps-to-your-tailscale-tailnet/17828
- https://fly.io/docs/networking/request-headers/
- https://docs.sentry.io/product/relay/
- https://docs.sentry.io/product/relay/modes/
- https://getsentry.github.io/relay/relay_config/enum.RelayMode.html
- https://getsentry.github.io/relay/src/relay_server/middlewares/cors.rs.html
- https://raw.githubusercontent.com/getsentry/relay/master/relay-server/src/middlewares/cors.rs
- https://github.com/getsentry/relay/issues/3580
- https://github.com/getsentry/relay/issues/1809
- https://github.com/getsentry/relay/issues/3042
- https://github.com/getsentry/relay/issues/3021
- https://develop.sentry.dev/sdk/foundations/transport/authentication/
- https://github.com/getsentry/sentry-javascript/issues/1992
- https://raw.githubusercontent.com/getsentry/sentry-javascript/develop/packages/browser/src/transports/fetch.ts
- https://github.com/getsentry/sentry/issues/24637
- https://github.com/bugsink/bugsink/issues/218
- https://www.bugsink.com/docs/
- https://developers.cloudflare.com/workers-vpc/get-started/
- https://developers.cloudflare.com/workers-vpc/configuration/tunnel/
- https://www.npmjs.com/package/@ts-edge/cloudflare

## Gap follow-up: What happens to iOS (sentry-cocoa) crash reports without server-side symbolication — do frames arrive as raw a

### Key facts
- sentry-cocoa v9 (current major, released 2025-12-01) performs no local symbolication: crash frames carry only instruction_addr, image_addr, package, and in_app (verified in SentryCrashReportConverter.m), plus debug_meta.images entries (type "macho", debug_id = LC_UUID, image_addr, image_size) for server-side lookup.
- On-device symbolication is a dead end by explicit SDK decision: v9 removed it for crashes because dladdr is not async-signal-safe and deadlocked (DECISIONS.md #27, issue #6560), and since 8.9.0 (2023) capture-time stacktraces symbolicate locally only when options.debug is enabled.
- Bugsink does not symbolicate Apple events: dSYM support is open tracker issue #20 (Jan 2025), with the maintainer still requesting sample data as of 2025-09-10.
- Bugsink's difs/assemble endpoint is gated behind FEATURE_MINIDUMPS (default False, commented "experimental... likely a DOS-magnet"), and even when enabled the stored DIFs are used only by the Breakpad /minidump/ endpoint — never for sentry-cocoa JSON events.
- nac-bugs runs stock bugsink/bugsink:latest with no FEATURE_MINIDUMPS in fly.toml, so its difs/assemble endpoint returns 404 today; keeping iOS on Bugsink buys zero symbolication.
- None of the three iOS apps (tracehealth-app, nouriche-ios, PristineAcres) links sentry-cocoa at all today — rg finds no SentrySDK/getsentry reference — so no live iOS error stream constrains the Bugsink absorption.
- GlitchTip proves the lightweight path: it symbolicates native frames at ingest in ~500 lines of Python around the symbolic library (Archive → SymCache → lookup(instruction_addr − image_addr) → demangle), with no Symbolicator service.
- getsentry/symbolic is a Rust-native MIT library (stable 13.9.0; 14.0.0-alpha.3, 2026-07-23): symbolic-debuginfo parses dSYMs and extracts debug_id, symbolic-symcache gives fast address→function/file/line lookup including inlinees, symbolic-demangle handles Swift/ObjC/C++ — nashgit can use the crates directly, no service, SQLite-friendly.
- Without Apple system symbols (which Sentry harvests from iOS firmware), UIKit/libsystem frames stay raw even with your dSYMs — but app frames symbolicate fully, which is what root-causing needs.
- Bugsink groups by exception type + normalized value with hex/int/uuid scrubbed to placeholders, so memory addresses do not fragment groups — the actual failure mode is over-grouping (every EXC_BAD_ACCESS in one issue).
- A symbolication-free stable fingerprint exists: (debug_id, instruction_addr − image_addr) of the top in-app frame is ASLR-independent and stable per build, so nashgit can group iOS crashes correctly before any dSYM is uploaded.
- Bitcode is dead (App Store stopped accepting it in 2023), so the dSYMs in the local .xcarchive from the existing asc xcode deploy skills are authoritative — a plain multipart zip upload to nashgit suffices; sentry-cli's chunked difs/assemble protocol is not needed.
- Symbolicator's extra scope (Apple system symbol servers, minidump processing, symbol-server proxying, caching layers) is exactly the part nashgit does not need for readable app frames.

### Recommendations
- Absorb Bugsink fully, iOS included — 'absorb except iOS' is unfounded because Bugsink offers iOS nothing today (difs 404s at nac-bugs, no cocoa-event symbolication even with the flag on) and no iOS app is wired yet.
- In nashgit v1, store debug_meta and raw frames verbatim and group native events by (debug_id, instruction_addr − image_addr) of the top in-app frame, falling back to exception type — correct per-build grouping with zero symbolication work.
- When the first iOS app actually gets wired, add the GlitchTip-style path in Rust with the symbolic crates (13.x, features debuginfo/symcache/demangle): one authenticated multipart endpoint accepting a zip of dSYMs, extract debug_id server-side, cache symcaches keyed by debug_id in SQLite, symbolicate app frames at ingest — budget days, not a Symbolicator-scale project.
- Make symbolication retroactive: keep raw frames stored so a late dSYM upload can re-symbolicate existing events (and optionally re-render titles) instead of only affecting future events.
- Apply the return-address adjustment Symbolicator does and GlitchTip skips: subtract 1 from instruction_addr for every frame except the topmost before symcache lookup, to avoid off-by-one line attribution.
- Skip permanently: sentry-cli difs/assemble protocol compatibility, Apple system symbols, and minidump ingestion — none are needed for readable app frames from sentry-cocoa; system frames showing package+address is acceptable.
- Wire dSYM upload into the existing deploy skills (deploy-tracehealth / testflight / testflight-pristine): after asc xcode archive, zip <archive>.xcarchive/dSYMs and POST it to nashgit in the same just recipe.
- Do not plan around on-device symbolication or unstripped release binaries — sentry-cocoa v9's crash path never resolves symbols regardless of build settings.

### Detail

# iOS crash reports without server-side symbolication — findings

Acronyms used: dSYM (debug symbol bundle — Apple's per-build file that maps addresses to function/file/line), DIF (Debug Information File — Sentry's umbrella term for dSYMs, ELF debug files, PDBs, Proguard maps), SDK (Software Development Kit), DSN (Data Source Name — the Sentry ingest URL+key), ASLR (Address Space Layout Randomization — the OS loads each binary at a random base address per launch), PoC (Proof of Concept), UUID (Universally Unique Identifier).

## 1. What sentry-cocoa actually sends (verified against SDK source, v9 main branch)

**Crash frames arrive as raw addresses. Full stop.** `SentryCrashReportConverter.stackFrameAtIndex:` (Sources/Sentry/SentryCrashReportConverter.m, main @ 2026) builds each frame from exactly four things: `instruction_addr` (hex string), `image_addr`, `package` (binary path), and `in_app`. No `function`, no `filename`, no `lineno`. The event also carries `debug_meta.images` — one entry per binary image referenced by a frame — with `type: "macho"`, `debug_id` (the Mach-O `LC_UUID`, which equals the dSYM's UUID), `image_addr`, `image_size`, `code_file`. Registers and the mach-exception mechanism (type like `EXC_BAD_ACCESS`, value with exception codes) come along too. This is exactly the input a server needs to symbolicate: for each frame, find the image containing `instruction_addr`, compute `instruction_addr − image_addr`, look that relative address up in the dSYM matching `debug_id`.

**On-device symbolication is a closed door, by explicit SDK decision:**
- CHANGELOG 8.9.0 (2023): "Symbolicate locally only when debug is enabled (#3079)" — this covered capture-time stacktraces (`captureError`, `captureMessage`). In release builds, those frames also arrive raw.
- DECISIONS.md #27 "No local symbolication of crashes" (Nov 7, 2025, shipped in v9.0.0 released 2025-12-01): local crash symbolication was removed entirely because `dladdr` in a crash handler is not async-signal-safe and caused deadlocks (issue #6560). The decision note adds that even a safe reimplementation would be debug-only "since in production apps should have their symbols stripped and only available in the dSYM."
- The DeepWiki summary of the repo states it plainly: "As of v9, the SDK no longer performs local symbolication... All symbolication happens server-side using uploaded dSYM files."

So the "on-device symbolication settings" escape hatch does not exist for crashes on any current SDK, and keeping symbols unstripped in the release binary would not help either — the v9 crash path never calls a symbol resolver.

Sentry's own docs confirm the premise the earlier research skipped: "Sentry requires dSYMs (debug information files) to symbolicate your stack traces" (docs.sentry.io, Apple/iOS dSYM page).

## 2. Bugsink: the difs/assemble endpoint exists but does NOT symbolicate iOS events

Verified against the bugsink/bugsink source (main, Aug 2026):

- **dSYM support for Apple events is open tracker issue #20** (opened Jan 7, 2025). As of the last activity (Sep 10, 2025) the maintainer was still asking users for sample dSYMs, event JSON, and API logs — i.e., pre-implementation.
- **`difs_assemble` (files/views.py) is real but feature-flagged off.** First line: `if not get_settings().FEATURE_MINIDUMPS: return 404 "minidumps not enabled"`. The default in bugsink/app_settings.py is `FEATURE_MINIDUMPS: False` with the comment "minidumps are experimental/early-stage and likely a DOS-magnet; disabled by default." The 2.0.7 changelog repeats: not security-reviewed, development-only.
- **Even with the flag on, uploaded DIFs are only used by the `/api/<project>/minidump/` endpoint** (files/minidump.py `event_threads_for_process_state`: symbolic → symcache → lookup, self-described as "good enough for a PoC"). That path processes Breakpad minidumps from sentry-native (C/C++). sentry-cocoa never sends minidumps — it sends JSON events — and no code path in Bugsink symbolicates JSON-event `instruction_addr` frames against stored DIFs. The only debug-file use in the normal event path is JavaScript sourcemaps (events/utils.py, `ecma426`).
- **At nac-bugs specifically:** /Users/md/Projects/nac-bugs/fly.toml runs stock `bugsink/bugsink:latest` with env `PORT/BASE_URL/BEHIND_HTTPS_PROXY/DATABASE_PATH/SINGLE_TEAM/USER_REGISTRATION` and secrets `CREATE_SUPERUSER/SECRET_KEY` — no `FEATURE_MINIDUMPS`. So **`.../files/difs/assemble/` returns 404 at nac-bugs today**; a `sentry-cli debug-files upload` against it fails.

**Bugsink grouping** (issues/grouping_mechanisms/v2.py): issues group by exception type + value, where the value first passes through Sentry's vendored `normalize_message_for_grouping`, which replaces `hex`, `int`, `uuid`, `sha1`, dates, etc. with placeholders. Consequence for unsymbolicated iOS crashes: memory addresses in the value do **not** fragment groups (the feared failure mode) — instead everything collapses the other way: every `EXC_BAD_ACCESS` from every crash site lands in one issue. Unreadable AND over-grouped. Bugsink's UI template (issues/templates/issues/_stacktrace_frames.html) renders `instruction_addr` for such frames — raw hex on screen.

Net: **staying on Bugsink for iOS buys nothing.** Bugsink's iOS story today is identical to a nashgit that stores frames verbatim — raw addresses, coarse grouping — except nashgit can group smarter (below).

## 3. GlitchTip: existence proof of the lightweight path

GlitchTip (glitchtip-backend on GitLab, master 2026) **does** symbolicate native frames server-side, with no Symbolicator service:

- `apps/difs/` implements the sentry-cli-compatible chunk-upload + `difs_assemble` API and stores DIFs per project.
- At ingest (`apps/event_ingest/process_event.py`), if the project has DIFs, `event_difs_resolve_stacktrace` runs `StacktraceProcessor.resolve_native_stacktrace`: open the DIF archive, pick the arch object, build a `SymCache`, then for each frame `lookup(instruction_addr − image_addr)`, `demangle` the result, fill `function`/`filename`/`lineno`, optionally pull source context from an uploaded source bundle. The image base comes from `debug_meta.images` when present.
- The whole native pipeline is roughly 500 lines of Python around bindings to getsentry's `symbolic` Rust library (GlitchTip recently moved to its own `gt_rust.symbolic` bindings).
- Known shortcuts: only the first exception's stacktrace, first source-location per address (inline chains collapsed), no Apple system symbols, no return-address adjustment.

GlitchTip's CLI docs ("upload source maps and debug symbols") confirm this is a shipped, supported feature — so a small self-hosted tracker doing real dSYM symbolication is a solved problem, not a research project.

## 4. The symbolic crate: exactly the machinery nashgit needs, Rust-native

`getsentry/symbolic` (MIT, 539 stars, actively released — stable 13.9.0, 14.0.0-alpha.3 published 2026-07-23) is the library Sentry, Bugsink's minidump PoC, and GlitchTip all sit on. For nashgit (already Rust) there is no bindings layer at all:

- `symbolic-debuginfo`: parse a dSYM (Mach-O/DWARF) from bytes, iterate objects per architecture, read `debug_id` — this is how an upload endpoint indexes files.
- `symbolic-symcache`: convert an object to a compact cache once, then do fast `lookup(relative_addr)` returning function name, file, line, including inlined frames. Cache blobs can live in SQLite keyed by `debug_id`.
- `symbolic-demangle` (facade feature `demangle`): Swift (docs say "up to Swift 5.3" mangling — fine in practice; Sentry uses it in production), Objective-C symbol detection, C++, Rust.

Symbolication algorithm for a cocoa event (mirrors GlitchTip/Symbolicator): for each frame with `instruction_addr`, find the `debug_meta.images` entry (type `macho`) whose `[image_addr, image_addr+image_size)` contains it; `rel = instruction_addr − image_addr`; look up in the symcache for that image's `debug_id`; demangle; write `function/filename/lineno` back; mark frames whose image has no uploaded dSYM. One refinement Symbolicator does and GlitchTip skips: for every frame except the topmost, the address is a *return* address, so subtract 1 before lookup to avoid off-by-one line attribution.

**What you give up versus running Symbolicator:** Apple *system* symbols (UIKit, Foundation, libsystem — Sentry harvests these from iOS firmware images), minidump processing, symbol-server proxying, and industrial caching. None of that blocks readable **app** frames, which are what root-causing needs; system frames simply keep showing `package + address`.

## 5. dSYM acquisition is easy in this shop

Bitcode is dead (Xcode 14 dropped it; the App Store stopped accepting it in 2023), so the App Store never recompiles the binary — **the dSYMs in the local `.xcarchive` produced by the existing `asc xcode` archive step (deploy-tracehealth, testflight, testflight-pristine skills) are authoritative** for the uploaded build. Because nashgit controls both ends, it does not need sentry-cli's chunked `chunk-upload`/`difs/assemble` protocol: a plain authenticated multipart endpoint that accepts a zip of `*.dSYM` bundles (a `just` step or Xcode post-action posts it after archive) is sufficient; nashgit extracts `debug_id`s server-side via `symbolic-debuginfo`. sentry-cli protocol compat can come later if ever wanted.

## 6. Reality check: no iOS app is wired today

`rg` over /Users/md/Projects/tracehealth-app, /Users/md/Projects/nouriche/nouriche-ios, and /Users/md/NashvilleAutomation/PristineAcres finds **zero** references to `SentrySDK`, `sentry-cocoa`, or `getsentry`. The nac-bugs-wire skill documents iOS wiring as a recipe but flags it: "Bugsink's native-crash symbolication is limited — treat iOS wiring as best-effort, don't chase perfect stack traces." So there is no live iOS error stream that the Bugsink→nashgit migration must preserve. The scope question is about the *future* wiring of TraceHealth/Nouriche/PristineConsult, not about parity with something running now.

## 7. Grouping without symbolication — nashgit can beat both incumbents

- Type+value grouping (Bugsink's approach, with hex/int scrubbing) over-groups native crashes: one issue per exception type.
- A symbolication-free fingerprint that works: **(debug_id, instruction_addr − image_addr) of the topmost in-app frame**. Subtracting `image_addr` removes ASLR, and `debug_id` pins the build, so the same crash site groups together across devices and launches of one release, and splits across releases. Symbolication later only improves display and cross-release grouping; it is not required for correct within-release grouping.

## 8. Decision summary

- **"Scope cliff comparable to running Symbolicator" — false.** The GlitchTip-style path is a few hundred lines on crates nashgit can depend on directly: one upload endpoint, one debug_id index, one symcache cache, one address-lookup pass at ingest. Days of work, in-process, SQLite-friendly. Symbolicator only becomes relevant if you want Apple system symbols or minidumps — you don't.
- **"iOS stays on Bugsink" — unfounded.** Bugsink gives iOS nothing today (open issue #20; difs endpoint 404s at nac-bugs; even enabled, DIFs never touch cocoa JSON events). Absorb everything.
- **"Unsymbolicated-but-grouped acceptable?" — as a v1, yes,** if nashgit stores `debug_meta` + raw frames verbatim and fingerprints on (debug_id, relative addr). Traces stay unreadable until a dSYM shows up, but nothing is lost: symbolication can run lazily/retroactively over stored events once the dSYM is uploaded.

### Sources
- https://github.com/bugsink/bugsink/issues/20
- https://github.com/bugsink/bugsink (bugsink/urls.py, files/views.py, files/minidump.py, files/tasks.py, bugsink/app_settings.py, issues/grouping_mechanisms/v2.py, sentry/at_597d25951d00/grouping/strategies/message.py, CHANGELOG.md, main branch Aug 2026)
- https://github.com/getsentry/sentry-cocoa (develop-docs/DECISIONS.md #27, CHANGELOG.md 8.9.0, Sources/Sentry/SentryCrashReportConverter.m, Sources/Sentry/SentryDefaultThreadInspector.m, releases/tag/9.0.0)
- https://github.com/getsentry/sentry-cocoa/issues/3409
- https://develop.sentry.dev/sdk/foundations/envelopes/event-payloads/debugmeta/
- https://docs.sentry.io/platforms/apple/guides/ios/dsym/
- https://deepwiki.com/getsentry/sentry-cocoa/5.2-debug-symbols-and-symbolication
- https://gitlab.com/glitchtip/glitchtip-backend (apps/difs/tasks.py, apps/difs/stacktrace_processor.py, apps/event_ingest/process_event.py, master 2026)
- https://glitchtip.com/documentation/cli
- https://github.com/getsentry/symbolic
- https://docs.rs/symbolic/latest/symbolic/
- https://docs.rs/symbolic-demangle/latest/symbolic_demangle/
- https://crates.io/crates/symbolic-symcache
- https://getsentry.github.io/symbolicator/
- local: /Users/md/Projects/nac-bugs/fly.toml
- local: /Users/md/.claude/skills/nac-bugs-wire/SKILL.md
- local: rg over /Users/md/Projects/tracehealth-app, /Users/md/Projects/nouriche/nouriche-ios, /Users/md/NashvilleAutomation/PristineAcres (no sentry-cocoa references)

## Gap follow-up: What are the exact server-side semantics for detecting a MISSED or TIMED-OUT cron check-in — the monitor_confi

### Key facts
- Sentry's 'missed' and 'timed out' states are computed ONLY server-side by a once-per-minute sweep; Relay even coerces a client-sent status=missed to 'unknown', so nashgit must never accept those statuses from SDKs (getsentry/relay relay-monitors/src/lib.rs, process_check_in).
- monitor_config wire schema (develop.sentry.dev check-ins spec v1.6.0, 2025-09-18): required `schedule` ({type:'crontab', value:'0 * * * *'} or {type:'interval', value:N, unit:year|month|week|day|hour|minute}), optional `checkin_margin` (minutes), `max_runtime` (minutes), `timezone` (tz database string), `failure_issue_threshold`, `recovery_threshold`, `owner`.
- Server defaults and limits (sentry/monitors/constants.py + utils.py): checkin_margin defaults to 1 minute (0 is coerced to 1), max_runtime defaults to 30 minutes, both capped at 40,320 minutes (28 days); thresholds default to 1, capped at 720; timezone defaults to UTC.
- Crontab handling: 5-field Vixie expressions only (6/7-field rejected), whitespace normalized, @yearly/@annually/@monthly/@weekly/@daily/@hourly translated to 5-field equivalents (@reboot unsupported), validated by computing next+prev occurrences; all schedule math runs in the monitor's timezone and results are clamped to minute granularity.
- Missed detection state: each monitor environment persists `next_checkin` and `next_checkin_latest` (= next_checkin + checkin_margin); every accepted check-in (including in_progress) advances both from the check-in's receive time; the sweep flags rows WHERE next_checkin_latest <= tick_ts.
- A monitor that has NEVER checked in is never marked missed — next_checkin is NULL until the first check-in, so upsert alone does not arm missed detection (asserted explicitly in check_missed.py).
- On a miss, Sentry creates a synthetic MISSED check-in backdated to the expected time, recomputes the most recent expected run via get_prev_schedule(expected, now) to survive clock skips, advances next_checkin/next_checkin_latest from that, does NOT touch last_checkin, then runs the same mark_failed/incident path as an error.
- Timeout detection: an in_progress check-in gets `timeout_at` = date_added (clamped to minute) + max_runtime; the sweep flags in_progress check-ins WHERE timeout_at <= tick_ts, flips them to TIMEOUT (terminal — a late ok can only update duration, never the status), and marks the monitor failed unless a newer ok/error check-in already exists.
- Upsert semantics (monitor_consumer.py): monitor looked up by (project, slug≤50 chars); valid monitor_config creates the monitor (name = slug) or updates only the provided keys; invalid config on an existing monitor is ignored (check-in still accepted); unknown slug with no/invalid config → check-in dropped with MONITOR_NOT_FOUND.
- Sentry's Kafka partition-clock machinery (clock_dispatch.py, clock_pulse.py) exists solely so a check-in ingestion backlog slows the clock instead of causing false missed alerts; a single-process SQLite server has no such decoupling, so a plain 1-minute tokio interval task is the correct equivalent.
- Healthchecks.io proves the minimal design: persist one `alert_after` datetime per check (next expected ping + grace, recomputed on every ping via cronsim in the check's timezone then converted back to UTC), poll `WHERE alert_after < now() AND status != 'down'` once per loop, flip with an optimistic-lock UPDATE, and notify on the flip.
- Alerting semantics: mark_failed → failure_issue_threshold (default 1) consecutive non-ok check-ins opens an incident (the single choke point where nashgit fires Pushover); recovery_threshold consecutive ok check-ins resolves it; missed, timeout, and error all funnel through this one path.
- Rust crate verdict (crates.io, checked 2026-08-18): `croner` 3.0.1 (2025-10-27, 7.2M downloads) is the best fit — POSIX 5-field, @aliases, chrono/chrono-tz timezone support, and both find_next_occurrence and find_previous_occurrence; the more popular `cron` 0.17.0 requires seconds-first 6/7-field syntax (wrong dialect), `saffron` is dormant since 2021 with no tz support, `cron-parser` is next-only.
- Interval schedules need no cron parser: Sentry computes them with calendar-aware arithmetic (dateutil rrule) from the last check-in time — in Rust that is plain chrono math plus month/year add-with-clamp (e.g. chronoutil).
- The whole sweeper is two indexed queries per minute (missed: next_checkin_latest <= now on monitors; timeout: timeout_at <= now on in_progress check-ins) — small enough that cron monitoring can ship in nashgit v1 without a scheduler framework.

### Recommendations
- Ship cron monitoring in v1 with the sweeper: it is two indexed SQLite queries driven by one tokio 1-minute interval task, not a scheduler framework — cutting it to status=error alerts would discard most of the feature's value for very little saved complexity.
- Copy the healthchecks.io storage model (persisted next_checkin_latest/timeout_at columns, minute poll, optimistic flip, notify on the flip) while speaking Sentry's wire protocol (check_in envelope item + monitor_config upsert) — do not replicate Sentry's Kafka clock, pulses, seats, or backlog machinery.
- Use the `croner` crate (3.0.1) with chrono-tz for crontab parsing: 5-field Vixie dialect matching Sentry's validator, @alias support, timezone-aware, and it provides both next- and previous-occurrence search (needed for Sentry-style re-anchoring after a miss); avoid the `cron` crate (seconds-first dialect) and `saffron` (dormant, no tz). Implement interval schedules with plain chrono arithmetic plus month/year add-with-clamp.
- Implement the exact Sentry semantics so unmodified SDKs behave correctly: checkin_margin default 1 min (coerce 0 to 1), max_runtime default 30 min, both capped at 28 days; clamp all schedule computations and timeout_at to minute granularity; compute schedules in monitor_config.timezone and convert back to UTC before adding margin/runtime (DST safety).
- Arm missed detection only after the first check-in (next_checkin starts NULL); on every accepted newest check-in — including in_progress — advance next_checkin/next_checkin_latest from the receive time; write exactly one synthetic MISSED check-in per gap, backdated to expected_time, then re-anchor from the most recent schedule occurrence at-or-before now (croner find_previous_occurrence).
- Set timeout_at = minute-clamped start + max_runtime on in_progress check-ins; on sweep, flip to TIMEOUT (terminal — a late ok may update duration but never the status) and skip monitor-failure marking if a newer ok/error check-in exists; let repeated in_progress check-ins act as heartbeats that push timeout_at forward.
- Enforce server-authoritative statuses: accept only in_progress/ok/error from SDKs, coerce or reject client-sent missed/timeout (Relay rewrites missed to unknown), support the all-zeros check_in_id meaning 'update latest in_progress', and treat user-terminal statuses as final.
- Mirror the upsert contract: auto-create a monitor only when a valid monitor_config accompanies the check-in (unknown slug without config → drop the check-in), ignore invalid configs on existing monitors, update only the keys present in the payload, and normalize crontabs (whitespace collapse, @aliases, reject >5 fields, parse-validate with croner before persisting).
- Fire Pushover from one choke point — the mark_failed/incident-open transition shared by error, missed, and timeout — and keep failure_issue_threshold/recovery_threshold support minimal (default 1 = alert on first failure, resolve on first ok); store the thresholds now if desired, but do not build Sentry's issue-occurrence machinery.
- Handle process downtime in the sweeper by iterating skipped minutes in order on wake (compare last-processed tick to now), which is the single-process analog of Sentry's clock backfill and healthchecks' ordered alert_after polling.

### Detail

# Sentry cron monitoring: server-side missed/timeout semantics

All findings verified against primary sources on 2026-08-18: the develop.sentry.dev check-ins spec (v1.6.0, 2025-09-18), getsentry/sentry master (`src/sentry/monitors/`), getsentry/relay master (`relay-monitors`), and healthchecks/healthchecks master.

## 1. monitor_config — wire schema (upsert payload)

Sent inside the `check_in` envelope item, ideally only on the `in_progress` check-in (spec says SHOULD, not MUST):

```json
{
  "monitor_config": {
    "schedule": { "type": "crontab", "value": "0 * * * *" },
    "checkin_margin": 5,
    "max_runtime": 30,
    "failure_issue_threshold": 2,
    "recovery_threshold": 2,
    "timezone": "America/Los_Angeles",
    "owner": "user:john@example.com"
  }
}
```

| Field | Type | Required | Server default | Hard limit |
|---|---|---|---|---|
| `schedule` | object | REQUIRED | — | — |
| `checkin_margin` | minutes (int) | optional | 1 (`DEFAULT_CHECKIN_MARGIN`); 0 coerced to 1 | 40,320 (28 days, `MAX_MARGIN`) |
| `max_runtime` | minutes (int) | optional | 30 (`TIMEOUT`) | 40,320 (`MAX_TIMEOUT`) |
| `timezone` | tz database string | optional | UTC | must be a valid zoneinfo name |
| `failure_issue_threshold` | int | optional (spec 1.4.0+) | treated as 1 | 720 (`MAX_THRESHOLD`), min 1 |
| `recovery_threshold` | int | optional (spec 1.4.0+) | treated as 1 | 720, min 1 |
| `owner` | `user:x` / `team:x` string | optional (spec 1.5.0+) | none | popped out of config, stored on the monitor row |

**Schedule variants** (`schedule.type` is REQUIRED):
- Crontab: `{ "type": "crontab", "value": "0 * * * *" }` — string, 5-field only.
- Interval: `{ "type": "interval", "value": 2, "unit": "hour" }` — unit one of `year|month|week|day|hour|minute`, value must be an integer > 0.

**Crontab validation/normalization** (`validators.py` ConfigValidator):
1. Collapse all whitespace runs to single spaces, trim.
2. `@yearly`/`@annually` → `0 0 1 1 *`, `@monthly` → `0 0 1 * *`, `@weekly` → `0 0 * * 0`, `@daily` → `0 0 * * *`, `@hourly` → `0 * * * *`. `@reboot` is rejected.
3. Reject expressions with more than 5 fields ("Only 5 field crontab syntax is supported") — so the seconds-first Quartz dialect is out.
4. Smoke-test the expression by computing one next and one prev occurrence with **cronsim** (the Python library written by the healthchecks.io author — both products use the same cron engine).

Internally Sentry re-shapes this into `Monitor.config = { schedule_type: 1|2, schedule: "crontab-string" | [n, "unit"], checkin_margin, max_runtime, timezone, failure_issue_threshold, recovery_threshold }`. That internal shape leaks into the denormalized `monitor_config` snapshot stored on every check-in row. nashgit can store the wire shape directly; nothing depends on Sentry's internal encoding.

## 2. Upsert behavior (`monitor_consumer.py::_ensure_monitor_with_config`)

- Lookup key is `(project_id, monitor_slug)`. Relay has already trimmed the slug to 50 chars and rejected empty slugs (`relay-monitors/src/lib.rs`); environment names are capped at 64 chars.
- **No `monitor_config` in payload**: return the existing monitor. If none exists, the check-in is **dropped** with a `MONITOR_NOT_FOUND` processing error. Auto-creation strictly requires a config.
- **Config present and valid**: create the monitor if missing (`name = slug`, active). If it exists and the validated config differs, update — but only the keys the payload actually sent (plus `schedule`/`schedule_type`, which are always updated). Owner is updated when changed.
- **Config present but invalid**: if the monitor already exists, log and keep the old config — the check-in is still accepted. If the monitor does not exist, drop with `MONITOR_INVALID_CONFIG`.
- Sentry.io additionally does quota "seat" assignment on upsert (`ACCEPTED_FOR_UPSERT` → assign seat or disable the monitor); irrelevant for nashgit.
- Disabled monitors: check-ins are filtered out.

## 3. Per-check-in state that arms the detector

Sentry tracks state per **monitor environment** (`(monitor, environment)`, env defaults to `"production"`; `MonitorEnvironment` rows hold `last_checkin`, `next_checkin`, `next_checkin_latest`, `status`). Key semantics:

- `next_checkin` / `next_checkin_latest` start **NULL**. Nothing arms missed detection until the first check-in arrives. `check_missed.py` even asserts this: "next_checkin must be set, since detecting this monitor as missed means there must have been an initial user check-in."
- On every accepted, newest check-in — **including `in_progress`** (`mark_ok` runs for anything not ERROR; ERROR runs the same update via `update_monitor_environment`):
  - `next_checkin = get_next_schedule(received_ts.astimezone(monitor_tz), schedule)` — for crontab, `next(CronSim(expr, ts))`; for interval, calendar-aware `ts + n*unit` via dateutil rrule. Result clamped to `second=0, microsecond=0` (**everything is minute-granular**).
  - `next_checkin_latest = next_checkin + checkin_margin` (timedelta minutes; default 1).
  - `last_checkin = checkin.date_added` (guarded: only if newer than the current value).
- New check-in rows record `expected_time` (the env's `next_checkin` at arrival — powers the "late" UI) and, for `in_progress` only, `timeout_at = date_added.replace(second=0, microsecond=0) + max_runtime`. Terminal statuses get `timeout_at = None`.
- Closing check-in matching (`update_existing_check_in`): same `check_in_id` updates the row. Guards: a user-terminal status (`ok`/`error`) is final — a later terminal check-in raises `CHECKIN_FINISHED`; a later out-of-order `in_progress` only records `date_in_progress`. A `TIMEOUT` row can receive a duration update but its status never leaves TIMEOUT. Repeated `in_progress` check-ins act as heartbeats and push `timeout_at` forward. Implicit duration = |terminal ts − date_added| when the SDK sent none. The all-zeros UUID `check_in_id` means "update the most recent in_progress check-in for this monitor" (spec 1.2.0).

## 4. The clock — how "a run never happened" is decided

Sentry never sets per-monitor timers. A **logical clock ticks once per minute** and each tick sweeps two indexed queries.

### Sentry's clock machinery (and why nashgit doesn't need it)

`clock_dispatch.py`: consumers record the max Kafka message timestamp per partition in Redis; the clock is the **minimum across partitions**; when it rolls over a minute boundary, one consumer (via atomic GETSET) dispatches a tick, backfilling any skipped minutes one at a time. `tasks/clock_pulse.py` (celery, every minute) produces a pulse message into **every** partition so the clock advances even with zero check-in traffic. The entire point (per evanpurkhiser's issues #53661/#79328): if ingestion backlogs, the clock **slows down with the backlog** instead of falsely marking monitors missed, and ticks stay strictly ordered per monitor. There is also a `mark_unknown` task used during detected ingestion incidents to write `unknown` instead of `missed`.

nashgit ingests over HTTP straight into SQLite in one process — there is no queue whose lag can outrun the detector. The correct equivalent is a single tokio 1-minute interval task (clamp `now` to the minute, iterate any missed minutes if the process was suspended, run both sweeps per minute processed).

### Missed sweep (`clock_tasks/check_missed.py`)

Per tick `ts`:
1. `SELECT ... FROM monitor_environment WHERE next_checkin_latest <= ts` excluding disabled/pending-deletion monitors (Postgres index on `(status, next_checkin_latest)`; capped at 10k).
2. Per row (re-checking `next_checkin_latest <= ts` to guard double-processing):
   - Create a **synthetic MISSED check-in** with `date_added = date_updated = expected_time = next_checkin` (backdated to when the run should have happened) and a snapshot of the monitor config.
   - Compute `most_recent_expected_ts = get_prev_schedule(expected_time, ts, schedule)` **in the monitor's timezone** — i.e. the latest schedule occurrence at or before now. This, not `expected_time + 1 tick`, is the new reference; the comments explain it guards against the sweeper having skipped minutes and against `checkin_margin` larger than the schedule gap (only ONE missed row is written per gap, then the schedule re-arms from the true latest occurrence).
   - Advance `next_checkin`/`next_checkin_latest` from that reference, **without changing `last_checkin`** (synthetic check-ins must not pose as real ones in the UI).
   - Call `mark_failed` (alerting path, below).

### Timeout sweep (`clock_tasks/check_timeout.py`)

Per tick `ts`:
1. `SELECT ... FROM monitor_checkin WHERE status = IN_PROGRESS AND timeout_at <= ts` (capped at 10k).
2. Per row: flip the check-in to `TIMEOUT`. If the environment has any **newer** ok/error check-in (dated after this one), stop — the timeout cannot affect monitor status. Otherwise compute `most_recent_expected_ts = get_prev_schedule(checkin.date_added, ts, schedule)` and `mark_failed`. (The env's `next_checkin` was already advanced when the `in_progress` arrived, so timeouts don't touch it — with a known caveat, documented in the source, that interval schedules may compute a next time the user's scheduler doesn't match when jobs overrun.)

### Alerting path (`logic/mark_failed.py` → `logic/incidents.py`)

`mark_failed` = `try_incident_threshold`: read `failure_issue_threshold` (default/falsy → 1). If the env is currently OK/ACTIVE and the last N check-ins (N = threshold) contain no `ok`, open a `MonitorIncident`, set env status to ERROR, and dispatch an issue occurrence — **this is the single choke point where nashgit fires Pushover**, and it is shared by error, missed, and timeout check-ins. `mark_ok` → `try_incident_resolution`: `recovery_threshold` (default 1) consecutive `ok` check-ins resolves the incident (optional second Pushover). Muted monitors skip occurrence creation.

### Statuses are server-authoritative

`CheckInStatus` on the wire is only `in_progress | ok | error`. Relay rewrites an incoming `missed` to `unknown` ("Missed status cannot be ingested, this is computed on the server"), and unknown statuses deserialize to `Unknown` rather than erroring. nashgit should reject or coerce client-sent `missed`/`timeout` the same way.

## 5. Timezone rules

- All schedule math is done in the monitor's timezone: `reference.astimezone(tz)` → cron/interval step → clamp to minute. Crontab fields therefore mean local wall-clock time in `monitor_config.timezone` (default UTC).
- `checkin_margin` and `max_runtime` are plain duration adds, applied **after** the schedule computation.
- Healthchecks documents the DST trap precisely (`hc/api/models.py::get_grace_start`): keep datetimes timezone-aware through the cron step, then **convert the result back to UTC before adding the grace timedelta** — adding a timedelta to a local-zone datetime across a DST transition yields wrong instants. Copy this pattern in Rust: `chrono_tz` aware datetime → croner next → `.with_timezone(&Utc)` → `+ Duration::minutes(margin)`.

## 6. Healthchecks.io — the minimal proven design for a small self-hosted server

Healthchecks.io (same cron engine, cronsim) shows how little state a correct detector needs:

- Each check persists `status` (`new|up|grace|down|paused`), `last_ping`, `last_start`, `grace`, and a single derived column **`alert_after`**, recomputed on every ping: `alert_after = grace_start + grace`, where `grace_start` = next cron occurrence after `last_ping` (computed in the check's tz, converted back to UTC), or `last_ping + timeout` for simple checks. A partial DB index on `alert_after` excludes rows already down.
- The detector (`sendalerts` management command) is a poll loop: `Check.objects.filter(alert_after__lt=now()).exclude(status="down").order_by("alert_after").first()`. Recompute the real status; if genuinely down, flip via an optimistic-lock `UPDATE ... WHERE status = old_status` (so a racing ping wins), record a `Flip` row, and send notifications from the flip. If not actually down (a ping raced in), just refresh `alert_after`. On any computation exception it pushes `alert_after` an hour forward before re-raising — no poison-pill hot loops.
- `/start` pings set `last_start`; `now >= last_start + grace` → down. Grace doubles as max_runtime — Sentry's separate `max_runtime` is strictly more expressive.
- Sentry-vs-healthchecks difference worth keeping from Sentry: the backdated synthetic missed check-in (gives an honest timeline) and per-run `check_in_id` pairing with durations.

## 7. Rust cron crates (crates.io metadata fetched 2026-08-18)

| Crate | Version / updated | Downloads | Verdict for Sentry-dialect crontabs |
|---|---|---|---|
| **croner** | 3.0.1 / 2025-10-27 | 7.2M | **Recommended.** POSIX/Vixie 5-field (optional seconds/year), `@hourly`-style aliases, chrono + optional chrono-tz timezones, and — uniquely — both `find_next_occurrence` and `find_previous_occurrence` plus `iter_after`/`iter_before` (verified on docs.rs 3.0.1). Directly implements Sentry's `get_next_schedule` AND `get_prev_schedule`. |
| cron | 0.17.0 / 2026-06-18 | 20.4M | Wrong dialect: seconds-first 6/7-field Quartz-style expressions (`sec min hour dom month dow year`). Sentry rejects >5 fields, so every incoming expression would need prepending `0 ` plus dialect fixups. Forward iteration only via `upcoming(tz)`. |
| saffron | 0.1.0 / 2021-02-01 | 1.8M | Cloudflare's Cron Triggers parser; dormant since 2021, no timezone support. Skip. |
| cron-parser | 0.11.2 / 2025-12-17 | 1.4M | Timezone support via chrono, but next-occurrence only. Workable if prev is emulated. |
| cronexpr | 1.6.0 / 2026-08-02 | 78k | jiff-based, timezone-in-expression extension; small ecosystem. |

Note on `get_prev_schedule`: Sentry uses it (a) to validate expressions and (b) to re-anchor after a miss/timeout. With croner you get it for free; without it you could iterate forward from the stored `expected_time` instead — but croner removes the need to choose.

**Interval schedules need no cron crate.** Sentry computes them with dateutil rrule from the last check-in: minute/hour/day/week are exact `chrono::Duration` adds; month/year need calendar-aware add-with-clamp (e.g. the `chronoutil` crate's `RelativeDuration`/`shift_months`, or ~15 lines by hand).

## 8. What this means for the nashgit v1 decision

The sweeper is **not** a heavyweight scheduler component. Concretely it is:

1. Two SQLite columns (`next_checkin_at`, `next_checkin_latest_at`) on the monitor(-environment) table plus `timeout_at` on in-progress check-in rows, each indexed.
2. One tokio `interval(60s)` task: clamp to the minute, process any skipped minutes in order, run the two SELECTs, write synthetic missed rows / flip timeouts, re-anchor schedules, and call the shared `mark_failed` → Pushover path.
3. One dependency for crontab parsing (`croner` + `chrono-tz`), plain arithmetic for intervals.

Sentry's genuinely complex parts — Kafka partition clocks, clock pulses, backfill ordering, seats/quotas, ingestion-incident `unknown` marking — exist to solve distributed-ingestion problems nashgit does not have. Healthchecks.io ships the same user-visible guarantee with an indexed datetime column and a poll loop, and has for a decade. Missed-run detection is therefore shippable in v1 without over-promising, provided the semantics table above (margin default 1 min, runtime default 30 min, no missed before first check-in, minute clamping, server-authoritative missed/timeout, one missed row per gap) is followed.

### Sources
- https://develop.sentry.dev/sdk/telemetry/check-ins/
- https://docs.sentry.io/platforms/python/crons/
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/schedule.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/constants.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/clock_tasks/check_missed.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/clock_tasks/check_timeout.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/clock_dispatch.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/tasks/clock_pulse.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/models.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/utils.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/validators.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/consumers/monitor_consumer.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/logic/mark_ok.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/logic/mark_failed.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/logic/incidents.py
- https://raw.githubusercontent.com/getsentry/sentry/master/src/sentry/monitors/logic/monitor_environment.py
- https://raw.githubusercontent.com/getsentry/relay/master/relay-monitors/src/lib.rs
- https://raw.githubusercontent.com/healthchecks/healthchecks/master/hc/api/models.py
- https://raw.githubusercontent.com/healthchecks/healthchecks/master/hc/api/management/commands/sendalerts.py
- https://github.com/getsentry/sentry/issues/53661
- https://github.com/getsentry/sentry/issues/79328
- https://crates.io/api/v1/crates/croner
- https://crates.io/api/v1/crates/cron
- https://crates.io/api/v1/crates/saffron
- https://crates.io/api/v1/crates/cron-parser
- https://crates.io/api/v1/crates/cronexpr
- https://docs.rs/croner/3.0.1/croner/struct.Cron.html
- https://raw.githubusercontent.com/Hexagon/croner-rust/master/README.md
