// The ingester, driven over real HTTP against a real celld node.
//
// test.sh brings up MinIO and the node and hands this file three environment
// variables. Nothing here is stubbed: every assertion below is about bytes that
// crossed a socket into V8 and, in most cases, into SQLite and a bucket.
//
// Project ids are handed out one per concern. Cells are per project, so a test
// that fills a quota or a buffer cannot disturb its neighbours, and the bucket
// is new for every run.

import assert from "node:assert/strict";
import { before, describe, it } from "node:test";
import { readFileSync } from "node:fs";
import { gzipSync, brotliCompressSync } from "node:zlib";
import { join } from "node:path";

const BASE = process.env.INGESTER_URL;
const TOKEN = process.env.INGESTER_DRAIN_TOKEN;
const FIXTURES = process.env.INGESTER_FIXTURES;
assert.ok(BASE && TOKEN && FIXTURES, "run this through ingester/test.sh");

const KEY = "0123456789abcdef0123456789abcdef";
const OTHER_KEY = "fedcba9876543210fedcba9876543210";

const P = {
  envelope: "1",
  logs: "2",
  quota: "3",
  drain: "4",
  flip: "5",
  caps: "6",
  dsn: "7",
  revoked: "8",
};
const UNKNOWN = "999";

/// Relay's list, verbatim. The count matters as much as the contents: an SDK
/// that sends a header outside this set has its preflight refused and its event
/// dropped with no visible symptom.
const ALLOW_HEADERS = [
  "x-sentry-auth",
  "x-requested-with",
  "x-forwarded-for",
  "origin",
  "referer",
  "accept",
  "content-type",
  "authentication",
  "authorization",
  "content-encoding",
  "transfer-encoding",
];

const RATE_LIMITS =
  "86400:transaction;span;profile;profile_chunk;replay;trace_metric:project";

const fixture = (name) => readFileSync(join(FIXTURES, name));

const control = (path, init = {}) =>
  fetch(`${BASE}/_nashcode${path}`, {
    ...init,
    headers: { authorization: `Bearer ${TOKEN}`, ...(init.headers ?? {}) },
  });

const stats = async (project) => (await control(`/stats/${project}`)).json();

const post = (project, body, { key, headers, door = "envelope" } = {}) => {
  const path = door === "logs" ? `/api/${project}/logs` : `/api/${project}/envelope/`;
  const query = key ? `?sentry_key=${key}` : "";
  return fetch(`${BASE}${path}${query}`, { method: "POST", body, headers });
};

/// An envelope with a `dsn` in its header line: how relay lets a client
/// authenticate with no header and no query string.
function envelopeWithDsn(dsn, eventId = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa") {
  const header = JSON.stringify(dsn === null ? { event_id: eventId } : { event_id: eventId, dsn });
  return Buffer.from(`${header}\n{"type":"event","length":2}\n{}\n`);
}

before(async () => {
  const answer = await control("/registry", {
    method: "PUT",
    body: JSON.stringify({
      projects: Object.values(P).map((project_id) => ({ project_id, key: KEY, active: true })),
    }),
  });
  assert.equal(answer.status, 200);
  // The Worker caches the registry for REGISTRY_TTL_MS, which test.sh sets to
  // 200 ms. Outlast it once here rather than in every test.
  await new Promise((done) => setTimeout(done, 400));
});

describe("auth happens before anything is buffered", () => {
  it("answers 404 for a project the registry has never heard of, and stores nothing", async () => {
    const answer = await post(UNKNOWN, fixture("python-exception.envelope"), { key: KEY });
    assert.equal(answer.status, 404);
    assert.equal((await answer.json()).detail, "unknown project");
    assert.equal((await stats(UNKNOWN)).rows, 0);
  });

  it("answers 403 for the wrong key, and stores nothing", async () => {
    const before = await stats(P.envelope);
    const answer = await post(P.envelope, fixture("python-exception.envelope"), { key: OTHER_KEY });
    assert.equal(answer.status, 403);
    assert.equal((await answer.json()).detail, "wrong key for this project");
    assert.equal((await stats(P.envelope)).rows, before.rows);
  });

  it("answers 403 when no key is offered at all", async () => {
    const answer = await post(P.envelope, envelopeWithDsn(null));
    assert.equal(answer.status, 403);
    assert.equal((await answer.json()).detail, "no sentry_key");
  });

  it("puts the refusal where a browser can read it", async () => {
    const answer = await post(P.envelope, fixture("python-exception.envelope"), { key: OTHER_KEY });
    assert.equal(answer.headers.get("access-control-allow-origin"), "*");
    assert.equal(answer.headers.get("x-sentry-error"), "wrong key for this project");
  });
});

describe("the envelope door", () => {
  it("answers 200 with the envelope's own event id", async () => {
    const answer = await post(P.envelope, fixture("python-exception.envelope"), {
      headers: { "x-sentry-auth": `Sentry sentry_key=${KEY}, sentry_version=7` },
    });
    assert.equal(answer.status, 200);
    assert.equal(answer.headers.get("content-type"), "application/json");
    assert.deepEqual(await answer.json(), { id: "4df262ddba6f4cd6a1104f818353c7b6" });
  });

  it("carries the rate-limit suppression header and the CORS headers on every 200", async () => {
    const answer = await post(P.envelope, fixture("python-exception.envelope"), { key: KEY });
    assert.equal(answer.status, 200);
    assert.equal(answer.headers.get("x-sentry-rate-limits"), RATE_LIMITS);
    assert.equal(answer.headers.get("access-control-allow-origin"), "*");
    assert.equal(
      answer.headers.get("access-control-expose-headers"),
      "x-sentry-error, x-sentry-rate-limits, retry-after",
    );
  });

  it("reads the event id out of a gzip body without storing it expanded", async () => {
    const raw = fixture("python-exception.envelope");
    const answer = await post(P.envelope, gzipSync(raw), {
      key: KEY,
      headers: { "content-encoding": "gzip" },
    });
    assert.equal(answer.status, 200);
    assert.deepEqual(await answer.json(), { id: "4df262ddba6f4cd6a1104f818353c7b6" });

    const rows = await drainRows(P.envelope);
    const stored = rows.at(-1);
    assert.equal(stored.content_encoding, "gzip");
    assert.equal(stored.bytes, gzipSync(raw).byteLength);
    assert.ok(stored.bytes < raw.byteLength, "the edge stores the compressed bytes");
  });

  it("mints an id when the envelope carries none", async () => {
    const answer = await post(P.envelope, fixture("sentry-logs.envelope"), { key: KEY });
    assert.equal(answer.status, 200);
    assert.match((await answer.json()).id, /^[0-9a-f]{32}$/);
  });

  it("answers {} for an encoding it cannot open, which the protocol allows", async () => {
    const answer = await post(P.envelope, brotliCompressSync(fixture("python-exception.envelope")), {
      key: KEY,
      headers: { "content-encoding": "br" },
    });
    assert.equal(answer.status, 200);
    assert.deepEqual(await answer.json(), {});
    assert.equal(answer.headers.get("x-sentry-rate-limits"), RATE_LIMITS);
  });

  it("never fails an envelope over an item type it does not know", async () => {
    const answer = await post(P.envelope, fixture("unknown-item.envelope"), { key: KEY });
    assert.equal(answer.status, 200);
  });

  it("takes the key from the dsn inside the envelope", async () => {
    const good = await post(P.dsn, envelopeWithDsn(`https://${KEY}@ingest.invalid/${P.dsn}`));
    assert.equal(good.status, 200);

    const bad = await post(P.dsn, envelopeWithDsn(`https://${OTHER_KEY}@ingest.invalid/${P.dsn}`));
    assert.equal(bad.status, 403);
    assert.equal((await bad.json()).detail, "wrong key for this project");
    assert.equal((await stats(P.dsn)).rows, 1);
  });

  it("answers the browser preflight with relay's eleven headers", async () => {
    const answer = await fetch(`${BASE}/api/${P.envelope}/envelope/`, { method: "OPTIONS" });
    assert.equal(answer.status, 200);
    const allowed = answer.headers.get("access-control-allow-headers").split(",").map((h) => h.trim());
    assert.deepEqual(allowed, ALLOW_HEADERS);
    assert.equal(allowed.length, 11);
    assert.equal(answer.headers.get("access-control-allow-methods"), "POST");
    assert.equal(answer.headers.get("access-control-allow-origin"), "*");
  });

  it("answers 404 for every other Sentry endpoint", async () => {
    for (const path of [`/api/${P.envelope}/store/`, `/api/${P.envelope}/minidump/`, "/api/x/envelope/"]) {
      const answer = await fetch(`${BASE}${path}`, { method: "POST", body: "{}" });
      assert.equal(answer.status, 404, path);
    }
  });
});

describe("the NDJSON log door", () => {
  const batch = '{"level":"info","message":"one"}\n{"level":"error","message":"two"}\n';

  it("buffers a batch and says how many bytes it took", async () => {
    const answer = await post(P.logs, batch, { key: KEY, door: "logs" });
    assert.equal(answer.status, 200);
    assert.deepEqual(await answer.json(), { buffered: Buffer.byteLength(batch) });
    assert.equal((await stats(P.logs)).rows, 1);
  });

  it("refuses a batch with no key before reading the body", async () => {
    const answer = await post(P.logs, batch, { door: "logs" });
    assert.equal(answer.status, 403);
    assert.equal((await stats(P.logs)).rows, 1);
  });

  it("marks the row so the drainer knows which door to replay it into", async () => {
    const rows = await drainRows(P.logs);
    assert.deepEqual(
      rows.map((row) => row.kind),
      ["logs"],
    );
    assert.equal(Buffer.from(rows[0].body, "base64").toString(), batch);
  });
});

describe("the size cap the Worker is the only guard for", () => {
  const big = Buffer.alloc(3 * 1024 * 1024, 0x61);

  it("refuses a declared body over 2 MiB", async () => {
    const answer = await post(P.caps, big, { key: KEY });
    assert.equal(answer.status, 413);
    assert.match((await answer.json()).detail, /2097152 byte limit/);
    assert.equal((await stats(P.caps)).rows, 0);
  });

  it("refuses a chunked body over 2 MiB, which declares no length at all", async () => {
    const stream = new ReadableStream({
      start(controller) {
        for (let sent = 0; sent < big.byteLength; sent += 65536) {
          controller.enqueue(new Uint8Array(big.subarray(sent, sent + 65536)));
        }
        controller.close();
      },
    });
    const answer = await fetch(`${BASE}/api/${P.caps}/envelope/?sentry_key=${KEY}`, {
      method: "POST",
      body: stream,
      duplex: "half",
    });
    assert.equal(answer.status, 413);
    assert.equal((await stats(P.caps)).rows, 0);
  });

  it("gives a body with no key at all a much smaller budget", async () => {
    const answer = await post(P.caps, Buffer.alloc(128 * 1024, 0x61));
    assert.equal(answer.status, 413);
    assert.equal((await stats(P.caps)).rows, 0);
  });

  it("answers 429 with Retry-After when the project buffer is full", async () => {
    // test.sh runs the node with MAX_BUFFER_BYTES=8 MiB, so five 2 MiB bodies
    // reach the cap without waiting for the 200 MB default.
    const chunk = Buffer.concat([
      Buffer.from('{"event_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}\n{"type":"event","length":2000000}\n'),
      Buffer.alloc(2_000_000, 0x20),
      Buffer.from("\n"),
    ]);
    let full = null;
    for (let attempt = 0; attempt < 8 && !full; attempt++) {
      const answer = await post(P.caps, chunk, { key: KEY });
      if (answer.status === 429) full = answer;
      else assert.equal(answer.status, 200);
    }
    assert.ok(full, "the buffer cap never tripped");
    assert.match((await full.json()).detail, /buffer is full/);
    assert.ok(Number(full.headers.get("retry-after")) > 0);
  });
});

describe("the per-project quota", () => {
  it("answers 429 with Retry-After past 1000 in five minutes", async () => {
    const body = envelopeWithDsn(null, "cccccccccccccccccccccccccccccccc");
    let refused = null;
    let accepted = 0;
    for (let sent = 0; sent < 1200 && !refused; sent += 20) {
      const batch = await Promise.all(
        Array.from({ length: 20 }, () => post(P.quota, body, { key: KEY })),
      );
      for (const answer of batch) {
        if (answer.status === 200) accepted++;
        else if (answer.status === 429) refused ??= answer;
        else assert.fail(`unexpected ${answer.status}`);
      }
    }
    assert.ok(refused, "the quota never tripped");
    assert.equal(accepted, 1000);
    assert.match((await refused.json()).detail, /quota for this project/);
    const retryAfter = Number(refused.headers.get("retry-after"));
    assert.ok(retryAfter > 0 && retryAfter <= 300, `retry-after was ${retryAfter}`);
  });
});

describe("drain, ack, and redelivery", () => {
  it("hands the same rows back until they are acked, then forgets them", async () => {
    for (const name of ["python-exception.envelope", "custom-fingerprint.envelope", "log-message.envelope"]) {
      assert.equal((await post(P.drain, fixture(name), { key: KEY })).status, 200);
    }

    const first = await drainRows(P.drain);
    assert.equal(first.length, 3);
    assert.deepEqual(
      first.map((row) => row.seq),
      [1, 2, 3],
    );
    assert.equal(
      Buffer.from(first[0].body, "base64").toString(),
      fixture("python-exception.envelope").toString(),
    );

    // Draining twice without an ack creates nothing and loses nothing.
    const again = await drainRows(P.drain);
    assert.deepEqual(again, first);

    const acked = await (
      await control(`/ack/${P.drain}`, { method: "POST", body: JSON.stringify({ up_to: 2 }) })
    ).json();
    assert.equal(acked.deleted, 2);
    assert.equal(acked.remaining, 1);

    const left = await drainRows(P.drain);
    assert.deepEqual(
      left.map((row) => row.seq),
      [3],
    );

    // Acking the same sequence again is a no-op, so a drainer can retry an ack
    // whose answer it never saw.
    const twice = await (
      await control(`/ack/${P.drain}`, { method: "POST", body: JSON.stringify({ up_to: 2 }) })
    ).json();
    assert.equal(twice.deleted, 0);
  });

  it("never rewinds the sequence after the buffer empties", async () => {
    await control(`/ack/${P.drain}`, { method: "POST", body: JSON.stringify({ up_to: 1_000_000 }) });
    assert.equal((await stats(P.drain)).rows, 0);

    assert.equal((await post(P.drain, fixture("log-message.envelope"), { key: KEY })).status, 200);
    const rows = await drainRows(P.drain);
    assert.equal(rows.length, 1);
    assert.ok(rows[0].seq > 3, `seq went backwards to ${rows[0].seq}`);
  });

  it("honours max_bytes but always returns at least one row", async () => {
    const answer = await control(`/drain/${P.drain}?after=0&max_bytes=1`);
    const rows = ndjson(await answer.text());
    assert.equal(rows.length, 1);
    assert.equal(answer.headers.get("x-ingest-last-seq"), String(rows[0].seq));
  });
});

describe("the registry", () => {
  it("flips a project's authentication as soon as it is replaced", async () => {
    assert.equal((await post(P.flip, envelopeWithDsn(null), { key: KEY })).status, 200);

    const shrunk = Object.values(P)
      .filter((id) => id !== P.revoked)
      .map((project_id) => ({ project_id, key: project_id === P.flip ? OTHER_KEY : KEY, active: true }));
    assert.equal((await control("/registry", { method: "PUT", body: JSON.stringify({ projects: shrunk }) })).status, 200);
    await new Promise((done) => setTimeout(done, 400));

    // The rotated project refuses the old key and takes the new one.
    assert.equal((await post(P.flip, envelopeWithDsn(null), { key: KEY })).status, 403);
    assert.equal((await post(P.flip, envelopeWithDsn(null), { key: OTHER_KEY })).status, 200);

    // A project dropped from the set stops existing, rather than lingering.
    const gone = await post(P.revoked, envelopeWithDsn(null), { key: KEY });
    assert.equal(gone.status, 404);
  });

  it("reads back exactly what was written", async () => {
    const answer = await control("/registry");
    const { projects } = await answer.json();
    const flipped = projects.find((entry) => entry.project_id === P.flip);
    assert.equal(flipped.key, OTHER_KEY);
    assert.ok(!projects.some((entry) => entry.project_id === P.revoked));
  });

  it("refuses a set with a key that is not 32 hex", async () => {
    const answer = await control("/registry", {
      method: "PUT",
      body: JSON.stringify({ projects: [{ project_id: "1", key: "nope" }] }),
    });
    assert.equal(answer.status, 400);
    // The bad PUT replaced nothing.
    assert.ok((await (await control("/registry")).json()).projects.length > 1);
  });
});

describe("the control plane is invisible without the bearer token", () => {
  const paths = [`/registry`, `/drain/${P.drain}`, `/ack/${P.drain}`, `/stats/${P.drain}`];

  it("answers 404 with no Authorization header at all", async () => {
    for (const path of paths) {
      const answer = await fetch(`${BASE}/_nashcode${path}`, { method: "GET" });
      assert.equal(answer.status, 404, path);
      assert.deepEqual(await answer.json(), { detail: "not found" });
    }
  });

  it("answers 404 for a wrong token", async () => {
    for (const path of paths) {
      const answer = await fetch(`${BASE}/_nashcode${path}`, {
        headers: { authorization: `Bearer ${"0".repeat(TOKEN.length)}` },
      });
      assert.equal(answer.status, 404, path);
    }
  });

  it("does not reach the control plane through the public /api door", async () => {
    for (const path of [`/api/${P.drain}/drain`, `/api/${P.drain}/ack`, "/api/1/registry"]) {
      const answer = await fetch(`${BASE}${path}`, {
        method: "POST",
        body: "{}",
        headers: { authorization: `Bearer ${TOKEN}` },
      });
      assert.equal(answer.status, 404, path);
    }
  });
});

// ---- helpers -----------------------------------------------------------------

function ndjson(text) {
  return text
    .split("\n")
    .filter((line) => line.length > 0)
    .map((line) => JSON.parse(line));
}

async function drainRows(project, after = 0) {
  const answer = await control(`/drain/${project}?after=${after}&max_bytes=${32 * 1024 * 1024}`);
  assert.equal(answer.status, 200);
  assert.equal(answer.headers.get("content-type"), "application/x-ndjson");
  return ndjson(await answer.text());
}
