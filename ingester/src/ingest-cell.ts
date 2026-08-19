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

import { toBase64 } from "./body";

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

/// A drain that names no budget gets this one. Big enough to be worth a round
/// trip, small enough that a stall costs one batch.
const DEFAULT_MAX_BYTES = 8 * 1024 * 1024;
const HARD_MAX_BYTES = 32 * 1024 * 1024;

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
    const after = intParam(url, "after", 0);
    const budget = Math.min(intParam(url, "max_bytes", DEFAULT_MAX_BYTES) || DEFAULT_MAX_BYTES, HARD_MAX_BYTES);

    const cursor = this.sql.exec(
      `SELECT seq, received_at, kind, content_encoding, remote_ip, bytes, body
         FROM envelopes WHERE seq > ? ORDER BY seq LIMIT ?`,
      after,
      this.maxRows,
    );

    const lines: string[] = [];
    let spent = 0;
    let last = after;
    for (const row of cursor as Iterable<Row>) {
      // Always take the first row even when it alone busts the budget, or one
      // large envelope would stall the drain for ever.
      if (lines.length > 0 && spent + row.bytes > budget) break;
      spent += row.bytes;
      last = row.seq;
      lines.push(
        JSON.stringify({
          seq: row.seq,
          received_at: row.received_at,
          kind: row.kind,
          content_encoding: row.content_encoding,
          remote_ip: row.remote_ip,
          bytes: row.bytes,
          body: toBase64(asBytes(row.body)),
        }),
      );
    }

    const remaining = Number(
      first(this.sql.exec(`SELECT COUNT(*) AS n FROM envelopes WHERE seq > ?`, last))?.n ?? 0,
    );
    const text = lines.length === 0 ? "" : lines.join("\n") + "\n";
    return new Response(text, {
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
    const upTo = Number(payload?.up_to);
    if (!Number.isInteger(upTo) || upTo < 0) {
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

function intParam(url: URL, name: string, fallback: number): number {
  const raw = url.searchParams.get(name);
  if (raw === null) return fallback;
  const value = Number(raw);
  return Number.isInteger(value) && value >= 0 ? value : fallback;
}

function json(status: number, payload: unknown, extra?: Record<string, string>): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "content-type": "application/json", ...(extra ?? {}) },
  });
}
