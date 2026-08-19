// IngestCell — one cell per project, a FIFO of raw request bodies.
//
// It is deliberately stupid. It does not know what a Sentry envelope is, it
// never decompresses anything, and it makes no decision the drainer could not
// make for itself. What it does own is the two things a buffer must own: a
// monotonic sequence number that survives a delete, and the quota that stops one
// project filling the node.
//
// Nothing here may leak into nashcode's drainer. The HTTP shapes below are the
// contract; celld is an implementation of it. See ingester/README.md.

import { concat, toBase64 } from "./body";

/// Per-project quota, from goal.md: 1k per 5 minutes, 5k per hour. Counters
/// reset lazily, on the next write after the window has run out, so an idle
/// project costs nothing.
const WINDOWS = [
  { name: "short", ms: 5 * 60 * 1000, limit: 1000 },
  { name: "long", ms: 60 * 60 * 1000, limit: 5000 },
];

/// The buffer cap. Past it the cell answers 429 and the SDK backs off, which is
/// the correct behaviour when nashcode has stopped draining: refuse loudly
/// rather than grow without bound. Both numbers are the design's defaults and
/// both are per cell, so a noisy project cannot crowd out a quiet one.
const DEFAULT_MAX_ROWS = 10_000;
const DEFAULT_MAX_STORED_BYTES = 200 * 1024 * 1024;

/// How long an SDK should wait when the buffer is full rather than the quota.
/// One drain interval, near enough.
const FULL_RETRY_AFTER = 30;

/// A drain that names no budget gets `DEFAULT_MAX_BYTES`, and no drain gets more
/// than `HARD_MAX_BYTES`.
///
/// Both numbers are small on purpose, and both were larger until a review
/// measured what they cost. A cell is single-threaded: while it is building a
/// drain answer it is not accepting envelopes, and an 8 MiB drain was measured
/// stalling a concurrent append by 992 ms. Base64 also inflates by a third, and
/// the answer exists twice in memory before it leaves, against an isolate heap
/// of 128 MB. 2 MiB per round trip costs a few hundred milliseconds of stall and
/// a couple of extra round trips; the drainer loops until `X-Ingest-Remaining`
/// reaches zero, so batching harder buys it nothing.
const DEFAULT_MAX_BYTES = 2 * 1024 * 1024;
const HARD_MAX_BYTES = 8 * 1024 * 1024;

interface Row {
  seq: number;
  received_at: number;
  kind: string;
  content_encoding: string | null;
  remote_ip: string | null;
  bytes: number;
  body: ArrayBuffer | Uint8Array;
}

export class IngestCell {
  private sql: any;
  private maxRows: number;
  private maxStoredBytes: number;

  constructor(state: any, env: { MAX_BUFFER_ROWS?: string; MAX_BUFFER_BYTES?: string }) {
    this.sql = state.storage.sql;
    this.maxRows = Number(env?.MAX_BUFFER_ROWS ?? "") || DEFAULT_MAX_ROWS;
    this.maxStoredBytes = Number(env?.MAX_BUFFER_BYTES ?? "") || DEFAULT_MAX_STORED_BYTES;
    this.sql.exec(
      `CREATE TABLE IF NOT EXISTS envelopes (
         seq INTEGER PRIMARY KEY,
         received_at INTEGER NOT NULL,
         kind TEXT NOT NULL,
         content_encoding TEXT,
         remote_ip TEXT,
         bytes INTEGER NOT NULL,
         body BLOB NOT NULL
       )`,
    );
    // `seq` is handed out from here rather than from rowid. After an ack empties
    // the table, rowid would start again at 1 and every drain cursor nashcode
    // holds would silently rewind.
    this.sql.exec(`CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v INTEGER NOT NULL)`);
    this.sql.exec(`INSERT OR IGNORE INTO meta (k, v) VALUES ('next_seq', 1)`);
    this.sql.exec(
      `CREATE TABLE IF NOT EXISTS quota (
         window TEXT PRIMARY KEY,
         started_at INTEGER NOT NULL,
         count INTEGER NOT NULL
       )`,
    );
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "POST" && url.pathname === "/append") return this.append(request);
    if (request.method === "GET" && url.pathname === "/drain") return this.drain(url);
    if (request.method === "POST" && url.pathname === "/ack") return this.ack(request);
    if (request.method === "GET" && url.pathname === "/stats") return this.stats();
    return json(404, { detail: "no such cell route" });
  }

  private async append(request: Request): Promise<Response> {
    const body = new Uint8Array(await request.arrayBuffer());
    const now = Date.now();

    const over = this.overQuota(now);
    if (over) return json(429, { detail: over.detail }, { "retry-after": String(over.retryAfter) });

    const { rows, bytes } = this.usage();
    if (rows >= this.maxRows || bytes + body.byteLength > this.maxStoredBytes) {
      return json(
        429,
        { detail: `the project buffer is full (${rows} envelopes, ${bytes} bytes)` },
        { "retry-after": String(FULL_RETRY_AFTER) },
      );
    }

    this.spendQuota(now);
    const seq = this.nextSeq();
    this.sql.exec(
      `INSERT INTO envelopes (seq, received_at, kind, content_encoding, remote_ip, bytes, body)
       VALUES (?, ?, ?, ?, ?, ?, ?)`,
      seq,
      now,
      request.headers.get("x-ingest-kind") ?? "envelope",
      request.headers.get("x-ingest-encoding"),
      request.headers.get("x-ingest-ip"),
      body.byteLength,
      body,
    );
    return json(200, { seq, bytes: body.byteLength });
  }

  /// `GET /drain?after=<seq>&max_bytes=<n>` → NDJSON, one row per line, body
  /// base64. Rows stay until they are acked, so a drain that is never acked
  /// returns the same rows again — that is what makes redelivery safe.
  private drain(url: URL): Response {
    // A cursor that cannot be read is refused rather than rounded down to zero:
    // silently replaying the whole buffer from the beginning is exactly the
    // failure a typo in a drainer should not be able to cause.
    const askedAfter = intParam(url, "after");
    if (askedAfter === null) return json(400, { detail: "after must be a non-negative integer" });
    const askedBytes = intParam(url, "max_bytes");
    if (askedBytes === null || askedBytes === 0) {
      return json(400, { detail: "max_bytes must be a positive integer" });
    }
    const after = askedAfter ?? 0;
    const budget = Math.min(askedBytes ?? DEFAULT_MAX_BYTES, HARD_MAX_BYTES);

    const cursor = this.sql.exec(
      `SELECT seq, received_at, kind, content_encoding, remote_ip, bytes, body
         FROM envelopes WHERE seq > ? ORDER BY seq LIMIT ?`,
      after,
      this.maxRows,
    );

    // Each row is encoded and released one at a time. Collecting the lines as
    // strings and joining them at the end would hold the whole answer three
    // times over — the array, the joined string, and the encoded body — and
    // base64 has already made it a third bigger than the bytes it describes.
    const encoder = new TextEncoder();
    const chunks: Uint8Array[] = [];
    let encoded = 0;
    let spent = 0;
    let last = after;
    for (const row of cursor as Iterable<Row>) {
      // Always take the first row even when it alone busts the budget, or one
      // large envelope would stall the drain for ever.
      if (chunks.length > 0 && spent + row.bytes > budget) break;
      spent += row.bytes;
      last = row.seq;
      const line = encoder.encode(
        JSON.stringify({
          seq: row.seq,
          received_at: row.received_at,
          kind: row.kind,
          content_encoding: row.content_encoding,
          remote_ip: row.remote_ip,
          bytes: row.bytes,
          body: toBase64(asBytes(row.body)),
        }) + "\n",
      );
      encoded += line.byteLength;
      chunks.push(line);
    }

    const remaining = Number(
      first(this.sql.exec(`SELECT COUNT(*) AS n FROM envelopes WHERE seq > ?`, last))?.n ?? 0,
    );
    return new Response(chunks.length === 0 ? "" : concat(chunks, encoded), {
      status: 200,
      headers: {
        "content-type": "application/x-ndjson",
        "x-ingest-last-seq": String(last),
        "x-ingest-remaining": String(remaining),
      },
    });
  }

  /// `POST /ack {"up_to": seq}` — the drained rows go away. Acking a sequence
  /// twice is a no-op, which is what lets the drainer retry an ack it never saw
  /// the answer to.
  private async ack(request: Request): Promise<Response> {
    let payload: any;
    try {
      payload = await request.json();
    } catch {
      return json(400, { detail: "the ack body is not JSON" });
    }
    // `Number("5")` is 5 and `Number(true)` is 1, so coercing here would let a
    // drainer that serialises the wrong shape delete real rows. It has to be a
    // number already.
    const upTo = payload?.up_to;
    if (typeof upTo !== "number" || !Number.isSafeInteger(upTo) || upTo < 0) {
      return json(400, { detail: "up_to must be a non-negative integer" });
    }
    const before = this.usage();
    this.sql.exec(`DELETE FROM envelopes WHERE seq <= ?`, upTo);
    const after = this.usage();
    return json(200, {
      deleted: before.rows - after.rows,
      remaining: after.rows,
      remaining_bytes: after.bytes,
    });
  }

  private stats(): Response {
    const { rows, bytes } = this.usage();
    const lowest = first(this.sql.exec(`SELECT MIN(seq) AS s, MAX(seq) AS m FROM envelopes`));
    return json(200, {
      rows,
      bytes,
      first_seq: lowest?.s === null || lowest?.s === undefined ? null : Number(lowest.s),
      last_seq: lowest?.m === null || lowest?.m === undefined ? null : Number(lowest.m),
      max_rows: this.maxRows,
      max_bytes: this.maxStoredBytes,
    });
  }

  private usage(): { rows: number; bytes: number } {
    const row = first(this.sql.exec(`SELECT COUNT(*) AS n, COALESCE(SUM(bytes), 0) AS b FROM envelopes`));
    return { rows: Number(row?.n ?? 0), bytes: Number(row?.b ?? 0) };
  }

  private nextSeq(): number {
    const row = first(this.sql.exec(`SELECT v FROM meta WHERE k = 'next_seq'`));
    const seq = Number(row?.v ?? 1);
    this.sql.exec(`UPDATE meta SET v = ? WHERE k = 'next_seq'`, seq + 1);
    return seq;
  }

  private overQuota(now: number): { detail: string; retryAfter: number } | null {
    for (const window of WINDOWS) {
      const row = first(this.sql.exec(`SELECT started_at, count FROM quota WHERE window = ?`, window.name));
      if (!row) continue;
      const startedAt = Number(row.started_at);
      if (now - startedAt >= window.ms) continue; // the window has run out; it resets on write
      if (Number(row.count) < window.limit) continue;
      const retryAfter = Math.max(1, Math.ceil((startedAt + window.ms - now) / 1000));
      return {
        detail: `over the ${window.limit} per ${window.ms / 1000}s quota for this project`,
        retryAfter,
      };
    }
    return null;
  }

  private spendQuota(now: number): void {
    for (const window of WINDOWS) {
      const row = first(this.sql.exec(`SELECT started_at, count FROM quota WHERE window = ?`, window.name));
      if (!row || now - Number(row.started_at) >= window.ms) {
        this.sql.exec(
          `INSERT INTO quota (window, started_at, count) VALUES (?, ?, 1)
             ON CONFLICT(window) DO UPDATE SET started_at = excluded.started_at, count = 1`,
          window.name,
          now,
        );
      } else {
        this.sql.exec(`UPDATE quota SET count = count + 1 WHERE window = ?`, window.name);
      }
    }
  }
}

function asBytes(body: ArrayBuffer | Uint8Array): Uint8Array {
  return body instanceof Uint8Array ? body : new Uint8Array(body);
}

function first(cursor: any): any {
  for (const row of cursor) return row;
  return null;
}

/// `undefined` when the parameter is absent, `null` when it is there and
/// unreadable. The caller has to tell those apart: a missing cursor means "from
/// the start", a broken one means "stop and say so".
function intParam(url: URL, name: string): number | null | undefined {
  const raw = url.searchParams.get(name);
  if (raw === null) return undefined;
  if (!/^[0-9]{1,15}$/.test(raw)) return null;
  return Number(raw);
}

function json(status: number, payload: unknown, extra?: Record<string, string>): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "content-type": "application/json", ...(extra ?? {}) },
  });
}
