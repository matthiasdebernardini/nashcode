# Notes: the public ingester

Where the implementation had to choose, and what the design document did not
know. `viewer/NOTES.md` covers the nashcode side; this file covers the edge,
because none of it is Rust and none of it ships in either binary.

Everything below was checked on 2026-08-19 against celld 0.2.1, MinIO
`RELEASE.2025-08-13`, and Node 22.22.

## What celld turned out to be

**0.2.1 works exactly as `ingester.md` assumed.** The Worker surface, the SQLite
Durable Objects, `ctx.storage.sql.exec`, `DecompressionStream`, `crypto`,
`atob`/`btoa` — all present, all behaving like workerd. The design needed no
concession.

**TypeScript needs no toolchain.** `celld deploy` bundles `main` with esbuild,
which strips types on its own. There is no `package.json`, no `node_modules`, and
no build step in this directory.

**`CELLD_VAR_<NAME>` overrides `vars.<NAME>` from `wrangler.jsonc`**, and the
deploy output lists every binding it found, which is the fastest way to check a
config change landed. A var must exist in `wrangler.jsonc` to be overridable, so
`DRAIN_TOKEN` is declared there with an empty default. Empty is the "not
configured" state and every control route answers 404 in it.

**A first run of an unsigned celld binary on macOS 26 stalls for minutes.** Not
celld: Gatekeeper. `celld --version` hung in `_dyld_start` with no output and no
CPU for about five minutes on first launch, then ran instantly for ever after. If
a downloaded celld looks dead, wait it out once before you go hunting. The same
stall hit `bash` once, mid-session, and made `test.sh` look wedged before it had
printed a line — `sample <pid>` showing nothing but `_dyld_start` is the
signature.

**celld self-fences when it loses the bucket.** Measured while building the
registry-outage test: with the bucket unreachable for longer than the node lease,
celld logs `SELF-FENCE: node lease not renewed within TTL — halting` and exits
with code 3. So a total bucket outage is not a fleet answering wrongly; it is a
fleet that is not there, and caddy answers 502, which SDKs retry. That is a better
failure mode than the one the review assumed, and it is why the outage test raises
`CELLD_TTL_MS` — at the stock ten seconds the node halts before it can be
measured. In production, leave the TTL alone: halting is the correct behaviour.

**Cell residency decides whether a bucket fault is visible at all.** A resident
cell answers a read out of memory, so the bucket can be gone and a `GET /registry`
still succeeds. Only a cell that has to be activated — never touched, or evicted
after `CELLD_IDLE_EVICT_S` — actually reads the bucket. Both matter to the outage
test, which is why it runs a second, deliberately cold node.

**MinIO now implements the conditional writes celld needs.** `ingester.md` and
celld's own fencing document both say MinIO community does not, and that was
true when they were written. `RELEASE.2025-08-13` refuses a second
`If-None-Match: *` PUT with `PreconditionFailed` and refuses a stale `If-Match`
the same way, which is exactly the pair celld asks for. That is what makes
`test.sh` possible: celld has no filesystem or in-memory bucket mode, so a local
store with real conditional writes was the only way to get a real node onto a
laptop.

This does not license MinIO in production. celld's warning is about stores that
accept the headers and do not enforce them, which fails late and silently; the
test fleet is one node, where ownership contention cannot arise at all. The
production bucket stays S3, R2, GCS, or Tigris.

## What the adversarial review changed

A review traced every path to a cell write and probed a live node about forty
times. It found no way past the authentication, and the gzip handling, registry
validation, bearer handling, sequence counter, and interop headers all held. Six
things did change, and the reasoning is worth keeping.

**A registry blip used to be a 404 storm.** `loadRegistry` swallowed its errors
and returned an empty map, which the cache then held for the full TTL. One second
of trouble reading the RegistryCell meant sixty seconds of "unknown project" for
every project on the fleet — and because an SDK reads 4xx as a verdict rather
than as weather, those events were destroyed, not delayed. Directly against
`ingester.md`'s "nothing is silently dropped". Now `loadRegistry` throws, a failed
refresh leaves the last good set in place, and an isolate that has never managed
to read one answers 503 with `Retry-After`. The cost is a revoked key living a few
seconds past its revocation during an outage, which is a routing identifier
working slightly too long — a much smaller thing than what it replaces.

**`X-Forwarded-For` was read from the wrong end.** caddy *appends* the peer it
saw, so the first entry is whatever the client typed and the last is the only one
worth believing. The probe put a `<script>` tag in a stored row, which then
crosses to nashcode. Now the edge takes the last entry and keeps it only if it
parses as an IPv4 or IPv6 address, and the README carries the
`header_up X-Forwarded-For {remote_host}` line that makes caddy the thing setting
it.

**Drain answers were sized for a machine with no other work to do.** At the old
32 MiB hard cap the answer existed roughly three times over — the array of line
strings, the joined string, and the encoded body — against a 128 MB isolate, with
base64 already inflating everything by a third. Worse, a cell serves one request
at a time: an 8 MiB drain was measured stalling a concurrent append by 992 ms, so
a big batch is a project that stops accepting envelopes. Now the default is 2 MiB
and the hard cap 8 MiB, and each line is encoded and released as it is produced
rather than collected and joined. The drainer loops on `X-Ingest-Remaining`
anyway, so nothing is lost by asking for less.

**`PUT {"projects":[]}` used to revoke the fleet in silence.** That is one
serialisation bug away from being sent by accident — an empty `Vec` where the real
set should have been — and by the finding above it destroys events rather than
delaying them. It now needs `?allow_empty=1`.

**Nothing caught an unexpected throw.** A failure anywhere in the control plane or
the doors would have surfaced as whatever celld makes of an exception, with no
CORS headers, which a browser SDK cannot read at all. There is a top-level catch
now, answering 502 with the headers.

**Two parameters were coerced where they should have been checked.** `up_to`
accepted `"5"` and `true`, both of which coerce to something plausible in
JavaScript on an endpoint whose whole job is deleting rows; it now has to be a
JSON number. A malformed `after` silently became 0 and replayed the buffer from
the start; it is a 400. `max_bytes=0` silently became the default; it is a 400.

## Where this differs from `ingester.md`

**The NDJSON log door.** `ingester.md` predates slice 2 and describes only the
envelope route. Amended in its own commit before any of this was written: an edge
that carries only envelopes leaves every journald, Vector, curl, and cron log
producer pointed at the tailnet, which is the thing phase 3 exists to stop. Both
doors buffer into the same cell and drain through the same protocol; the row
records which door it came in by.

**The log door answers `{"buffered": <bytes>}`, not the viewer's
`{"accepted","rejected"}`.** Counting records means decompressing and parsing,
and the edge does neither by design. The authoritative counts appear when
nashcode replays the batch into the viewer's door.

**`X-Sentry-Rate-Limits` goes on log 200s too.** The viewer puts it only on
envelope answers. There is no SDK behind the NDJSON door to obey it, so it costs
nothing, and one rule — every 200 carries it — is easier to keep true than two.

**Control routes live under `/_nashcode/`, not at `/drain` and `/registry`.**
The cells answer at the paths the design names; the Worker has to say *which*
cell, and a project id has to be in the path to do that. Keeping them off `/api/`
also means caddy's allowlist and the public dispatch path cannot collide, which
is tested.

**Rejection is 404, not 401.** A caller without the bearer token learns nothing
about what lives behind it. The cost is that a misconfigured drainer sees 404s
instead of a clear 401; the README says so in bold, twice.

**Three tunables the design fixed as constants** are `vars` with the design's
numbers as defaults: `REGISTRY_TTL_MS` (60 s), `MAX_BUFFER_ROWS` (10k), and
`MAX_BUFFER_BYTES` (200 MB). Testability drove it — a 60-second cache makes a
registry-flip test take a minute, and a 200 MB cap makes a buffer-cap test move
200 MB — but per-fleet tuning is a real operational want too. The quota numbers
stayed hard-coded, because 1000 requests take a second and the test does them for
real.

**Per-IP throttling is not built.** `ingester.md` mentions `X-Forwarded-For` for
logging and per-IP throttling. The address is parsed, stored on the row, and
available to the drainer; the throttle is not there. The per-project quota is the
one that matters, since the key is what selects a cell, and an IP-keyed counter
at the edge would need its own cell to be worth anything. Left for when abuse
justifies it.

**`GET /_nashcode/stats/<project_id>` is new.** The design does not mention it.
It exists because "did that request get buffered?" is the question every test and
every incident asks first, and answering it from the drain endpoint means pulling
the bodies.

## Choices the design left open

**`seq` comes from a counter row, not from `rowid`.** With `INTEGER PRIMARY KEY`
alone, an ack that empties the table sends the next insert back to 1 and every
drain cursor nashcode holds silently rewinds — it would re-deliver rows it had
already acked and, worse, skip rows below its cursor for ever. `meta.next_seq`
makes the invariant explicit rather than depending on `AUTOINCREMENT` semantics
in a Durable Object's SQLite. A test asserts the sequence does not go backwards
after the buffer empties.

**An envelope with no `event_id` gets a minted one.** Relay answers `{}` there.
The viewer mints, so the edge mints, and an SDK that reads the id back has
something to correlate either way. Brotli and zstd still get `{}`, because those
bodies cannot be opened here at all.

**A request authenticated only by the in-envelope `dsn` gets 64 KiB, not 2 MiB.**
It has to be read before it can be judged, so an unauthenticated sender must
never be able to make the edge hold the full cap. Same number as the viewer's
`MAX_UNAUTHED_COMPRESSED`. A real SDK sends `X-Sentry-Auth` or `?sentry_key=` and
never meets this limit.

**The registry fails stale, not closed.** This started out failing closed — an
unreadable registry authenticated nobody — which reads well and is wrong, for the
reason in the review section above. What it fails *open* to is bounded: a set that
was read successfully, once, by this isolate. It never invents an entry and never
keeps one that a successful read has dropped.

**The Worker parses `X-Sentry-Auth` and DSNs by hand.** `sentry-types` is a Rust
crate and the edge has no dependencies at all — no `package.json`, nothing to
audit, nothing to update. Both grammars are a few lines. Keys that are not 32 hex
are rejected before any comparison, which keeps junk out of the constant-time
compare.

**Secret comparison is constant-time; the DSN keys are not secrets.** The drain
token comparison has no early exit. The DSN keys go through the same helper for
uniformity, though they travel in browser query strings by design and protect
nothing.

## The test harness

Real processes, no mocks, per the repo rule: a MinIO container, a real `celld
deploy`, two real celld nodes, and 41 assertions over real HTTP. Nothing about
celld is stubbed. The bucket outage is a real `docker stop`, not a flag.

The second node exists only to be cold. Proving the 503 needs a Worker isolate
that has never once read the registry, and the only honest way to get one is a
node nothing has touched yet — a warm isolate has a cache by definition, and a
resident cell would answer the read out of memory even with the bucket gone. So
`test.sh` starts it, leaves it alone, and the outage test is the first thing ever
to send it a request.

What is not covered:

- **No iroh.** The drainer half of the loop is a later slice, and iroh-ingress is
  a separate binary from celld-mvp. The tests reach the control plane over
  loopback HTTP, which is what iroh-ingress forwards to.
- **The stale-cache path is asserted by its consequence, not by its cause.**
  During the outage a known project with a wrong key answers 403, which can only
  come from a set the isolate still holds; the old empty-map bug answered 404
  there. Whether that particular read failed or came out of a resident cell is
  celld's business and not observable from outside, so the test asserts the thing
  that matters — never 404 for a project that exists — and the cold node's 503
  covers the other half unambiguously.
- **The 200 MB and 10k-row defaults are not exercised at their defaults.** The
  test node runs with `MAX_BUFFER_BYTES=8388608` so the cap trips after five 2 MiB
  bodies instead of a hundred. The code path is identical; only the number
  changes.
- **No kill-mid-burst durability test.** Acceptance fact 2 in `ingester.md` — kill
  celld mid-burst and lose no acknowledged envelope — is celld's output gate, not
  our code, and proving it wants a fault-injection fleet rather than a laptop.
  celld's own release testing covers it.
- **Docker is required.** celld has no filesystem or in-memory bucket mode; the
  bucket *is* the coordinator. `test.sh` says so and stops rather than pretending.
