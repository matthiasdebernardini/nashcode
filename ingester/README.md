# The public ingester

A celld application that takes Sentry envelopes and NDJSON logs from anywhere on
the internet, buffers them one cell per project, and holds them until nashcode
drains them over iroh. nashcode accepts no inbound connection; the tailnet stays
sealed.

The design is `goals/error-tracking/ingester.md`. The interop rules the public
doors obey — response shapes, the rate-limit header, the CORS set — are
`goals/error-tracking/goal.md`, and the viewer obeys the same ones in
`viewer/src/web/bugs.rs`. Change one door and you change both.

```
src/index.ts         the Worker: two public doors, one bearer-authed control plane
src/protocol.ts      the Sentry contract: CORS, rate limits, key parsing, caps
src/body.ts          capped reads, the envelope header line, base64
src/ingest-cell.ts   IngestCell — one FIFO per project, plus the quota
src/registry-cell.ts RegistryCell — the (project_id, key, active) set
test.sh              the test entry point: MinIO, a real celld node, real HTTP
```

## Run the tests

```sh
ingester/test.sh
```

About twenty seconds. It starts a MinIO container for the fleet bucket, deploys
this Worker to it, starts a celld node on a free loopback port, and drives the
whole surface over HTTP with `node --test`. Every run gets a new bucket, so no
cell and no quota counter survives from one run to the next.

Needs `docker`, `node`, `esbuild`, and celld 0.2.0 or newer. Set `CELLD` to
choose a binary. `KEEP=1` leaves the node and the bucket up and prints the URL
and the drain token so you can poke at it by hand.

## Deploy

```sh
cd ingester
celld deploy . --bucket s3://<ingest bucket>
```

Then restart the nodes: a node loads its deployment at startup and keeps serving
the old version until it does.

Three processes run under systemd on the VPS. None of the names below belongs in
this repository; they are host configuration.

| Process | Command |
|---|---|
| celld | `celld --bucket s3://<ingest bucket> --listen 127.0.0.1:8080` |
| caddy | TLS for the ingest domain, forwarding only `POST`/`OPTIONS` on `/api/*/envelope/` and `POST` on `/api/*/logs`, 404 for everything else |
| iroh-ingress | `iroh-ingress --allow <nashcode EndpointId> --forward 127.0.0.1:8080` |

The Worker reads four variables, all as `CELLD_VAR_*` on the node:

| Variable | Default | What it does |
|---|---|---|
| `DRAIN_TOKEN` | empty | The bearer token for the control plane. Empty means every control route answers 404. |
| `REGISTRY_TTL_MS` | `60000` | How long an isolate caches the project registry. |
| `MAX_BUFFER_ROWS` | `10000` | Buffered envelopes per project before 429. |
| `MAX_BUFFER_BYTES` | `209715200` | Buffered bytes per project before 429. |

The ingest bucket must be a store with real conditional writes. Amazon S3, R2,
Tigris, and GCS qualify; celld's own documentation rules out Backblaze B2,
Hetzner, and DigitalOcean Spaces. Keep it separate from the celld-mvp fleet
bucket and from `s3://<archive bucket>`: this one holds transit buffers only, and
its credential is the fleet's root of authority.

## The public doors

```
POST    /api/<project_id>/envelope/    a Sentry envelope, any SDK
OPTIONS /api/<project_id>/envelope/    the browser preflight
POST    /api/<project_id>/logs         NDJSON, for journald, Vector, curl, cron
```

Authentication is a DSN public key, taken from `X-Sentry-Auth`, `?sentry_key=`,
or the `dsn` field of the envelope header line, and checked against the registry
**before any byte is buffered**. An unknown project is 404, a wrong key is 403,
and neither reaches a cell. A body over 2 MiB compressed is 413, counted as it
streams, so a chunked upload that declares no length is refused at the same
place. A request whose only credential is the in-envelope `dsn` gets 64 KiB
instead of 2 MiB, because it has to be read before it can be judged.

A successful envelope gets `200 {"id":"<event_id>"}` and the `X-Sentry-Rate-Limits`
suppression header. A brotli or zstd body gets `200 {}`: the edge cannot open it,
and the protocol allows the empty answer. The log door answers
`200 {"buffered": <bytes>}` — counting records would mean decompressing and
parsing, which is nashcode's job at digest.

Over quota (1000 per five minutes, 5000 per hour, per project) or over the buffer
cap, both doors answer `429` with `Retry-After` and the SDK backs off on its own.

## The drainer contract

This is the interface nashcode's drainer codes against. It is deliberately small
and deliberately not celld-shaped: if celld disappoints, about five hundred lines
of axum plus SQLite behind the same iroh-ingress implement the same three routes
and the drainer never notices. Do not let the drainer learn anything celld knows.

Every route needs `Authorization: Bearer <drain token>`. Without it, or with the
wrong one, they answer `404 {"detail":"not found"}` — the same thing they say to a
stranger, so a probe cannot learn they exist. **A drainer that suddenly gets 404
on every call has a token problem, not a routing problem.**

### `GET /_nashcode/drain/<project_id>?after=<seq>&max_bytes=<n>`

`200`, `content-type: application/x-ndjson`, one JSON object per line:

```json
{"seq":7,"received_at":1787171010775,"kind":"envelope","content_encoding":"gzip","remote_ip":"203.0.113.9","bytes":1637,"body":"H4sIA..."}
```

- `seq` rises for ever within a project. It is never reused, including after an
  ack empties the buffer, so a cursor can never silently rewind.
- `kind` is `envelope` or `logs`: which door it arrived through, and so which
  door to replay it into.
- `body` is base64 of the bytes exactly as they arrived. Still compressed if
  `content_encoding` says so — decompress at digest, with nashcode's streaming
  caps.
- `max_bytes` caps the sum of `bytes` in the answer. Default 8 MiB, hard limit
  32 MiB. One row always comes back even if it alone busts the budget, or a large
  envelope would stall the drain for ever.
- Two response headers save a parse: `X-Ingest-Last-Seq` is the highest `seq` in
  this answer, `X-Ingest-Remaining` is how many rows are left after it.

Rows stay until they are acked. Draining twice without acking returns the same
rows, which is what makes redelivery safe: digest dedupes by `event_id`, and by
envelope hash for items without one.

### `POST /_nashcode/ack/<project_id>` — `{"up_to": <seq>}`

Deletes every row at or below `up_to`. Answers
`200 {"deleted":n,"remaining":n,"remaining_bytes":n}`. Acking a sequence twice
deletes nothing, so an ack whose answer was lost can simply be sent again.

Ack only what digest has finished with. Everything acked is gone from the edge.

### `GET|PUT /_nashcode/registry`

```json
{"projects":[{"project_id":"1","key":"0123456789abcdef0123456789abcdef","active":true}]}
```

`PUT` **replaces** the whole set; it does not merge. A project nashcode deleted
has to stop authenticating here, and a merge would leave it working for ever.
`project_id` is the numeric id, `key` is the 32-hex DSN public key; anything else
is a 400 and nothing is written. `GET` reads the set back.

An isolate caches the registry for `REGISTRY_TTL_MS`, so a change takes up to
that long to reach every node. Push the set whenever a project is created,
rotated, or revoked.

### `GET /_nashcode/stats/<project_id>`

`{"rows":n,"bytes":n,"first_seq":n,"last_seq":n,"max_rows":n,"max_bytes":n}`. For
operators and tests. A drainer does not need it.

## Security

- Auth before buffer. An unknown key costs one registry lookup and no storage.
- The 2 MiB cap is the Worker's alone: celld has no body cap and a cell row caps
  at about 2.2 MB.
- The DSN keys in the registry are routing identifiers, not secrets — they travel
  in browser query strings by design. The drain token is the one secret, and it
  is never logged and never in this repository.
- Drain, ack, and registry sit behind three layers: caddy's 404, the iroh
  EndpointId allowlist, and the bearer token. Any one of them is enough.
- celld's internal listener is an unauthenticated operator API. It stays on
  loopback. Never expose it.
