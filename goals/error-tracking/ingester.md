# Design: the public ingester (celld app, drained by nashcode over iroh)

The public half of nashcode error tracking. A celld application on one public VPS
accepts Sentry envelopes from anywhere, buffers them one cell per project, and
holds them until nashcode pulls them over iroh. nashcode never accepts an inbound
connection; the tailnet stays sealed. Bugsink and the nac-bugs Fly app retire
completely. The two apps on nac-bugs get new DSNs, which is accepted.

Facts about celld below were verified 2026-08-18 against celld.dev and the
denoland/celld source (v0.2.1). celld is Deno Land's platform, Apache-2.0,
self-assessed alpha. The risk section deals with that.

## Topology

```
Sentry SDKs (Fly, EC2, Workers, browsers, iOS)
        |  HTTPS POST /api/<id>/envelope/
        v
   caddy (TLS, path allowlist)          public VPS
        |  127.0.0.1:8080
        v
   celld node ── Worker ──> IngestCell per project (SQLite)
        |                        RegistryCell (id -> key)
        |  LTX replication (RPO=0)
        v
   s3://<ingest fleet bucket>

   iroh-ingress (pinned EndpointIds) ──> 127.0.0.1:8080
        ^
        |  iroh QUIC, ALPN celld/http/0
   nashcode drainer (tailnet) ── digest ── s3://nashcode-bugs ── Pushover
```

Three processes under systemd on the VPS: caddy, celld, iroh-ingress. The
iroh-ingress binary is celld-mvp's, reused as-is: it rejects any EndpointId not
on the allowlist before reading a stream byte and forwards to the celld
listener.

## The Worker (dispatcher)

Two routes are public, the same two the viewer answers on the tailnet side:
`POST`/`OPTIONS` on `/api/<project_id>/envelope/` and `POST /api/<project_id>/logs`,
the NDJSON door for journald, Vector, curl, and cron. Caddy forwards those and
404s everything else, so the drain and registry routes never exist on the public
side at all.

(Amendment, 2026-08-19: this document predates slice 2, which grew the NDJSON
log door in the viewer. An edge that carries only envelopes would send every
non-SDK log producer straight at the tailnet, which is the thing phase 3 exists
to stop. The two doors buffer into the same cell and drain through the same
protocol; only the stored `kind` differs, so the drainer knows which viewer door
to replay each row into.)

Per envelope POST:

1. Auth: take the key from `X-Sentry-Auth`, `?sentry_key=`, or the envelope
   `dsn` header. Check (project_id, key) against a module-level registry cache
   (refreshed from RegistryCell, TTL 60 s). Unknown → 403 before any buffering.
2. Size cap: stream-count the body and abort past **2 MiB compressed**. celld
   itself has no body cap and a cell row caps at 2,200,000 bytes, so the
   Worker is the only guard. 2 MiB is generous for errors and logs;
   attachments get dropped at digest anyway.
3. Buffer: forward to `IngestCell.idFromName(project_id)`, which appends
   (seq, received_at, content_encoding, remote_ip, body BLOB) and bumps quota
   counters. Store the body raw and compressed. The edge never decompresses
   for storage; nashcode decompresses at digest with its streaming caps.
4. Respond `200` + `{"id":"<event_id>"}` when the encoding is identity, gzip,
   or deflate (DecompressionStream handles those; read only the first header
   line). Brotli and zstd bodies get `{}`, which the spec allows. Attach the
   CORS headers and the `X-Sentry-Rate-Limits` suppression header
   (transactions, spans, profiles, replays) from goal.md on every 200, so
   unwanted telemetry stops before crossing the internet twice.
5. Reject with 429 + Retry-After when the project's quota window
   (1k/5 min, 5k/hour, lazily reset counters) or the buffer cap
   (default 10k envelopes or 200 MB per cell) is hit. SDKs back off on
   their own.

Client IP comes from caddy's `X-Forwarded-For`. Do not pass
`--trust-forwarded-headers` to celld; the Worker reads the header itself and
only for logging and per-IP throttling.

## The cells

**IngestCell** (one per project) is a plain FIFO in SQLite via
`ctx.storage.sql.exec`:

- `envelopes(seq INTEGER PRIMARY KEY, received_at, kind, content_encoding,
  remote_ip, bytes INTEGER, body BLOB)`, where `kind` is `envelope` or `logs`
- quota counters with window timestamps, reset lazily on write
- `GET /drain?after=<seq>&max_bytes=<n>` → NDJSON of rows, body base64
- `POST /ack {"up_to": seq}` → `DELETE WHERE seq <= ?`

**RegistryCell** holds `(project_id, key, active)`. nashcode owns it: `PUT
/registry` replaces the set, `GET /registry` reads it back. Both routes, plus
drain and ack, require `Authorization: Bearer <drain-token>` (a celld var only
nashcode knows): defense in depth behind the caddy 404 and the iroh allowlist.

Durability is celld's, not ours: every acknowledged write is proven in the
fleet bucket before the 200 leaves (celld's output gate: RPO — Recovery Point
Objective — of zero, no acknowledged write can be lost). Keep the gate on.
If the VPS dies, a replacement node adopts the cells from the bucket in ~20 s;
buffered envelopes survive. That is the "use object storage" requirement,
satisfied by the platform instead of by our code.

## The drain (nashcode side)

A tokio task in nashcode, every 15–30 s:

1. Dial the ingester's iroh EndpointId (pinned in config) on ALPN
   `celld/http/0`, through iroh-ingress.
2. For each active project: drain batches, feed each envelope into the same
   digest pipeline the tailnet-direct endpoint uses (decompress with streaming
   caps, parse, group, archive to s3://nashcode-bugs, index, Pushover), then
   ack.
3. Push registry diffs whenever projects were created, rotated, or revoked
   since the last cycle.

Delivery is at-least-once; digest dedupes by event_id, and by envelope hash
for items without one. nashcode keeps a persistent iroh SecretKey; its
EndpointId goes in the ingester's allow-file. Worst-case Pushover latency is
one drain interval. 30 s is fine for error notification.

Failure modes hold up: nashcode down → cells buffer to their cap, then 429 and
SDK backoff, nothing lost below the cap. Ingester down → SDKs retry per their
own transport rules; tailnet projects are unaffected because they post
directly to nashcode. Bucket down → celld's output gate refuses writes, SDKs
see errors and retry; nothing is silently dropped.

## Configuration

| Where | Setting |
|---|---|
| VPS | `celld --bucket s3://nashcode-ingest --listen 127.0.0.1:8080` |
| VPS | caddy: TLS for the ingest domain, allowlist `/api/*/envelope/` and `/api/*/logs` |
| VPS | `iroh-ingress --allow <nashcode EndpointId> --forward 127.0.0.1:8080` |
| Worker vars | `CELLD_VAR_DRAIN_TOKEN` |
| nashcode | `NASHCODE_BUGS_DRAIN=<ingester EndpointId>`, drain token, iroh key path |
| nashcode | `NASHCODE_BUGS_INGEST_URL=https://<ingest domain>` (DSN host for public projects) |

Buckets stay separate on purpose: `s3://nashcode-ingest` is this fleet's root
of authority and holds only transit buffers; `s3://nashcode-bugs` is the
long-term archive; the celld-mvp fleet bucket is untouched. The ingest bucket
must be on a store with real conditional writes: S3, R2, GCS, Azure, or
Tigris. MinIO community, B2, Hetzner, and DO Spaces silently break celld's
ownership leases; v0.2.1 probes at startup and refuses to serve if the store
fails.

## Security summary

- One public route, everything else 404 at caddy.
- Drain, ack, and registry live behind three layers: caddy 404, iroh
  EndpointId allowlist, bearer token.
- Mutual pinning: nashcode is allowlisted at the ingester; the ingester's
  EndpointId is pinned in nashcode.
- The tailnet accepts zero inbound connections; nashcode only dials out.
- A fully compromised VPS yields buffered envelopes and DSN keys (routing
  identifiers, not secrets). No path to nashcode or the tailnet exists.
- celld's internal listener (unauthenticated operator API) stays on loopback;
  never expose it. The fleet bucket credential lives only on the VPS, scoped
  to the ingest bucket.

## The honest risk: celld is alpha

v0.2.1 shipped 4 days ago; the first public release is 16 days old. Deno Land
says "not safe for hostile multi-tenant use", upgrades are stop-the-world, and
the operator API can change between releases. This box faces the hostile
internet.

Why it is still acceptable: the exposed surface is caddy plus one Worker route
with auth-before-buffer; the box holds nothing sensitive by design; testing
rigor is unusually strong (differential conformance against workerd,
deterministic simulation, live fault-injection fleet); and dgit already runs
on celld, so the operational bet is already placed.

The hedge is the contract, not the implementation: the drain/ack/registry
protocol above is the interface nashcode codes against. If celld disappoints,
a ~500-line Rust binary (axum + SQLite + the same iroh-ingress in front)
implements the identical protocol and nashcode never notices. Do not let
nashcode's drainer know anything celld-specific.

## Build order

1. celld app (Worker + two cell classes, wrangler.jsonc, `celld deploy`) with
   a local celld node; test with sentry-python and captured envelopes.
2. nashcode drainer behind a feature flag, against the local node over
   loopback iroh.
3. VPS: systemd units, caddy, DNS, real bucket; end-to-end with one test
   project per client class (server SDK, browser fetch with `?sentry_key=`,
   Worker SDK).
4. Mint new DSNs for the two nac-bugs apps, deploy them, watch a forced error
   arrive on the phone, then decommission the nac-bugs Fly app.

Acceptance facts 1–7 and 13 from goal.md apply to the ingester path verbatim,
with "the envelope endpoint" read as the public ingest domain. Add three:

1. An envelope POSTed publicly appears in nashcode within one drain interval
   and produces exactly one issue and one Pushover message.
2. Killing celld mid-burst loses no acknowledged envelope (output-gate check:
   every 200-acknowledged event eventually reaches nashcode).
3. Draining twice without ack does not duplicate issues or events in nashcode.
