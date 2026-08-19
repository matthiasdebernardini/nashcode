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

About half a minute. It starts a MinIO container for the fleet bucket, deploys
this Worker to it, starts two celld nodes on free loopback ports, and drives the
whole surface over HTTP with `node --test`. Every run gets a new bucket, so no
cell and no quota counter survives from one run to the next.

The last test stops the MinIO container on purpose, to watch what a bucket outage
does to authentication, and starts it again afterwards. The second node is there
for that test alone: it is deliberately never touched until then, so its Worker
isolate has no cached registry to fall back on.

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
| caddy | TLS for the ingest domain, forwarding only `POST`/`OPTIONS` on `/api/*/envelope/` and `POST` on `/api/*/logs`, 404 for everything else. Two directives are load-bearing, not decoration — see below. |
| iroh-ingress | `iroh-ingress --allow <nashcode EndpointId> --forward 127.0.0.1:8080` |

**caddy is the only thing separating the two audiences.** Both the public
internet and nashcode's drainer arrive at the same `127.0.0.1:8080`; the public
side is public because caddy forwards `/api/` and nothing else, and the control
plane is private because caddy never forwards `/_nashcode/`. Get that path
matcher wrong and the bearer token becomes the only guard left. Write it against
the collapsed path — the Worker collapses `//api/1/envelope/` to one slash
before it matches, and a caddy matcher that does not will disagree with it.

```
# The header caddy must set, not merely pass through: it replaces whatever the
# client claimed with the peer caddy actually saw. The Worker reads the last
# entry, so this is the entry it reads.
header_up X-Forwarded-For {remote_host}

# A rate limit belongs here too. Project ids are small integers and the edge
# tells them apart out loud — 404 for a project that does not exist, 403 for one
# that does — so the whole project space is enumerable at HTTP speed. Browser
# DSN keys are public by design, so the quota is per project, not per sender.
# caddy's rate_limit, keyed on {remote_host}, is what makes that expensive.
rate_limit {
  zone ingest { key {remote_host}  events 600  window 1m }
}
```

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
place.

When the registry cannot be read at all, the answer is `503` with `Retry-After`,
never 404. The difference is the whole point: an SDK treats 4xx as a verdict and
destroys the event, and 5xx as weather and keeps it.

A successful envelope gets `200 {"id":"<event_id>"}` and the `X-Sentry-Rate-Limits`
suppression header. A brotli or zstd body gets `200 {}`: the edge cannot open it,
and the protocol allows the empty answer. The log door answers
`200 {"buffered": <bytes>}` — counting records would mean decompressing and
parsing, which is nashcode's job at digest.

### Two caps that differ from the tailnet door, on purpose

The viewer takes 20 MiB compressed; this edge takes 2 MiB, because celld has no
body cap of its own and a cell row stops at about 2.2 MB. **Moving a project from
the tailnet DSN to the public one therefore lowers its ceiling by a factor of
ten.** Anything that ships 2 MiB-plus envelopes today — a big batch, an event
with attachments — starts getting 413 the moment its DSN changes. Check the
largest envelope a project actually sends before you cut it over in phase 5.

The second difference is narrower. A request whose only credential is the `dsn`
inside the envelope has to be read before it can be judged, so it gets 64 KiB
rather than 2 MiB. The viewer peeks at the header line first and raises its cap
once that line authenticates; the edge does not, so **a correctly authenticated
envelope over 64 KiB is 413 here and 200 there** when the in-envelope `dsn` is
the only key. Every SDK we support sends `X-Sentry-Auth` or `?sentry_key=` and
never meets this. If one ever does, peek-then-raise is the fix.

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

The drainer is `viewer/src/bugs/drain.rs`. It reaches these routes two ways, and the
loop above the transport cannot tell which: an `http://host:port` target dials TCP, and
an iroh EndpointId dials `iroh-ingress` on ALPN `celld/http/0`. Provisioning the second
one takes one step that is easy to forget. nashcode keeps a persistent iroh secret key
at `NASHCODE_BUGS_DRAIN_KEY` and prints the EndpointId derived from it at startup;
**that EndpointId has to be in `iroh-ingress --allow` before the first dial**, or the
ingress closes the stream before celld ever sees a byte and the drainer reports the box
as unreachable. The pinning is mutual: `NASHCODE_BUGS_DRAIN` holds the ingester's
EndpointId on the other side.

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
- `max_bytes` caps the sum of `bytes` in the answer. Default 2 MiB, hard limit
  8 MiB; a larger request is clamped, not refused. One row always comes back even
  if it alone busts the budget, or a large envelope would stall the drain for
  ever. **Take the default and loop.** A cell serves one request at a time, so
  every byte in a drain answer is a byte the project spends not accepting
  envelopes: an 8 MiB drain was measured stalling a concurrent append by 992 ms.
- `X-Ingest-Last-Seq` is **the cursor for your next call**, which is the highest
  `seq` in this answer, or the `after` you sent when the answer is empty. Pass it
  straight back as `after` either way.
- `X-Ingest-Remaining` is how many rows are left after that cursor. Loop until it
  is `0`.
- `after` and `max_bytes` are validated, not coerced. A malformed `after` is a
  `400`, never a silent replay from the start of the buffer, and `max_bytes=0` is
  a `400` rather than a silent fall back to the default.

Rows stay until they are acked. Draining twice without acking returns the same
bytes, which is what makes redelivery safe: digest dedupes by `event_id`, and by
envelope hash for items without one.

### `POST /_nashcode/ack/<project_id>` — `{"up_to": <seq>}`

Deletes every row at or below `up_to`. Answers
`200 {"deleted":n,"remaining":n,"remaining_bytes":n}`. Acking a sequence twice
deletes nothing, so an ack whose answer was lost can simply be sent again.

`up_to` must already be a JSON number. A string or a boolean is a `400` and
deletes nothing — `"5"` and `true` both coerce to something plausible in
JavaScript, and this endpoint deletes rows for a living.

Ack only what digest has finished with. Everything acked is gone from the edge.

### `GET|PUT /_nashcode/registry`

```json
{"projects":[{"project_id":"1","key":"0123456789abcdef0123456789abcdef","active":true}]}
```

`PUT` **replaces** the whole set; it does not merge. A project nashcode deleted
has to stop authenticating here, and a merge would leave it working for ever.
`project_id` is the numeric id, `key` is the 32-hex DSN public key, and
`active:false` means the same as absent. Anything else is a 400 and nothing is
written; validation runs over the whole set before a row is touched, so a bad PUT
never half-lands. `GET` reads the set back.

**An empty `projects` array is refused** with a 400 unless the URL says
`?allow_empty=1`. Emptying the set takes every project on the fleet offline, and
since the resulting 404 is permanent as far as an SDK is concerned, it destroys
events rather than delaying them. It is also exactly what one serialisation bug
looks like — an empty `Vec` where the real set should have been. Nashcode really
can have no projects, so the door exists; it just has to be opened deliberately.

An isolate caches the registry for `REGISTRY_TTL_MS`, so a change takes up to
that long to reach every node. Push the set whenever a project is created,
rotated, or revoked.

A refresh that fails does **not** empty the cache: the isolate keeps serving the
last set it read successfully, and answers 503 only if it has never read one. A
revoked key therefore outlives its revocation by the length of the outage. That
is the trade made on purpose — a DSN key is a routing identifier, and a few extra
seconds of one is worth less than every project's telemetry being told, in a
status code nothing retries, that it does not exist.

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
- `remote_ip` on a stored row is the **last** `X-Forwarded-For` entry and only
  ever an address that parses. Everything before it came from the client, and a
  client will send a `<script>` tag if it thinks something downstream will render
  one. That row crosses to nashcode; treat it as data, not as markup, on the
  other side too.
- celld's internal listener is an unauthenticated operator API. It stays on
  loopback. Never expose it.
- No unhandled failure escapes the Worker. Anything unexpected becomes a `502`
  with the CORS headers, so a browser SDK sees a retryable error rather than an
  opaque network failure it cannot back off from.
