// The dispatcher.
//
// Two public doors — the Sentry envelope endpoint and the NDJSON log endpoint —
// and a control plane that only nashcode can see. Everything else is 404.
//
// The order below is the security property, so read it as an order rather than a
// list. The control routes are matched first and every one of them needs the
// bearer token; without the token they answer 404, so a probe cannot even learn
// that they exist. Then the public routes, which authenticate the request
// against the registry *before* a byte reaches a cell. Auth before buffer is the
// invariant that makes this box safe to point at the internet: an unknown key
// costs one registry lookup and no storage.

import { headerLine, parseHeaderLine, readCapped, TooLarge, decompressionFormat } from "./body";
import {
  MAX_BODY,
  MAX_UNAUTHED_BODY,
  RATE_LIMITS,
  jsonResponse,
  keyFromAuthHeader,
  keyFromDsn,
  keyFromQuery,
  newEventId,
  preflight,
  secretEquals,
  sentryError,
} from "./protocol";

export { IngestCell } from "./ingest-cell";
export { RegistryCell } from "./registry-cell";

interface Env {
  INGEST: any;
  REGISTRY: any;
  DRAIN_TOKEN?: string;
  REGISTRY_TTL_MS?: string;
}

interface Project {
  key: string;
  active: boolean;
}

/// The registry cache lives on the isolate, not in the cell, so the common case
/// — a known project sending its thousandth envelope — costs no cell hop at all.
/// A cold isolate pays one. TTL is a var so a test can shorten it; 60 s is the
/// configured default and the number the design assumes.
let cache: { at: number; projects: Map<string, Project> } | null = null;

const DEFAULT_REGISTRY_TTL_MS = 60_000;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname.startsWith("/_nashcode/")) return control(request, env, url);

    const api = matchApi(url.pathname);
    if (!api) return sentryError(404, "no such endpoint");

    if (request.method === "OPTIONS") return preflight();
    if (request.method !== "POST") return sentryError(405, "this endpoint takes POST");

    return api.door === "envelope"
      ? acceptEnvelope(request, env, url, api.projectId)
      : acceptLogs(request, env, url, api.projectId);
  },
};

// ---- the public doors --------------------------------------------------------

interface ApiRoute {
  projectId: string;
  door: "envelope" | "logs";
}

/// `/api/<project_id>/envelope/` (the trailing slash every SDK sends, and the
/// spelling without it) and `/api/<project_id>/logs`. Every other path under
/// `/api/` is a Sentry endpoint we do not implement — `/store/`, `/minidump/` —
/// and 404 is what we mean to say about them.
function matchApi(pathname: string): ApiRoute | null {
  const parts = pathname.split("/").filter((part) => part.length > 0);
  if (parts.length !== 3 || parts[0] !== "api") return null;
  if (!/^[0-9]{1,19}$/.test(parts[1])) return null;
  if (parts[2] === "envelope") return { projectId: parts[1], door: "envelope" };
  if (parts[2] === "logs") return { projectId: parts[1], door: "logs" };
  return null;
}

async function acceptEnvelope(request: Request, env: Env, url: URL, projectId: string): Promise<Response> {
  const project = await lookup(env, projectId);
  if (!project) return sentryError(404, "unknown project");

  const declared = keyFromAuthHeader(request.headers.get("x-sentry-auth")) ?? keyFromQuery(url);
  if (declared && !secretEquals(declared, project.key)) {
    return sentryError(403, "wrong key for this project");
  }

  // A request whose only credential is the `dsn` inside the envelope has to be
  // read before it can be judged. It gets a small budget rather than the full
  // one, so an unauthenticated sender can never make us hold 2 MiB.
  const encoding = request.headers.get("content-encoding");
  let raw: Uint8Array;
  try {
    raw = (await readCapped(request, declared ? MAX_BODY : MAX_UNAUTHED_BODY)).bytes;
  } catch (error) {
    if (error instanceof TooLarge) return sentryError(413, error.message);
    return sentryError(400, "the body could not be read");
  }

  const header = parseHeaderLine(await headerLine(raw, encoding));

  if (!declared) {
    const key = keyFromDsn(header?.dsn);
    if (!key) return sentryError(403, "no sentry_key");
    if (!secretEquals(key, project.key)) return sentryError(403, "wrong key for this project");
  }

  const buffered = await buffer(env, projectId, "envelope", raw, request, encoding);
  if (buffered) return buffered;

  // The id an SDK reads back. Brotli and zstd bodies cannot be opened here, so
  // they get `{}`, which the protocol allows. Everything else gets the envelope's
  // own event_id, or a minted one — no SDK has ever been harmed by an id it did
  // not ask for, and a correlatable answer beats a bare `{}`.
  const readable = decompressionFormat(encoding) !== null;
  const eventId = typeof header?.event_id === "string" ? header.event_id : readable ? newEventId() : null;
  return jsonResponse(200, eventId ? { id: eventId } : {}, { "x-sentry-rate-limits": RATE_LIMITS });
}

/// NDJSON in, one JSON object per line, authenticated by the same DSN key.
///
/// There is no envelope header here, so the third auth door — the `dsn` inside
/// the body — does not exist: a key in `X-Sentry-Auth` or `?sentry_key=` is the
/// whole of it. That is also why the body is never read before the key is
/// judged.
async function acceptLogs(request: Request, env: Env, url: URL, projectId: string): Promise<Response> {
  const project = await lookup(env, projectId);
  if (!project) return sentryError(404, "unknown project");

  const declared = keyFromAuthHeader(request.headers.get("x-sentry-auth")) ?? keyFromQuery(url);
  if (!declared) return sentryError(403, "no sentry_key");
  if (!secretEquals(declared, project.key)) return sentryError(403, "wrong key for this project");

  const encoding = request.headers.get("content-encoding");
  let raw: Uint8Array;
  try {
    raw = (await readCapped(request, MAX_BODY)).bytes;
  } catch (error) {
    if (error instanceof TooLarge) return sentryError(413, error.message);
    return sentryError(400, "the body could not be read");
  }

  const buffered = await buffer(env, projectId, "logs", raw, request, encoding);
  if (buffered) return buffered;

  // The edge counts bytes, not records: counting records means decompressing and
  // parsing, which is nashcode's job at digest. `accepted` and `rejected` appear
  // there, on the same batch, when it is replayed into the viewer's log door.
  return jsonResponse(200, { buffered: raw.byteLength }, { "x-sentry-rate-limits": RATE_LIMITS });
}

/// Hand the raw bytes to the project's cell. Returns a response only when
/// something went wrong — a 429 the SDK should back off on, or a storage failure
/// it should retry.
async function buffer(
  env: Env,
  projectId: string,
  kind: string,
  raw: Uint8Array,
  request: Request,
  encoding: string | null,
): Promise<Response | null> {
  const stub = env.INGEST.get(env.INGEST.idFromName(projectId));
  const headers: Record<string, string> = { "x-ingest-kind": kind };
  if (encoding) headers["x-ingest-encoding"] = encoding;
  const ip = clientIp(request);
  if (ip) headers["x-ingest-ip"] = ip;

  let answer: Response;
  try {
    answer = await stub.fetch("https://cell.invalid/append", {
      method: "POST",
      headers,
      body: raw as BodyInit,
    });
  } catch (error) {
    console.error(`ingest: project ${projectId} cell unreachable: ${error}`);
    return sentryError(502, "cannot buffer the request");
  }

  if (answer.status === 200) return null;
  if (answer.status === 429) {
    const detail = await detailOf(answer, "over quota");
    return sentryError(429, detail, { "retry-after": answer.headers.get("retry-after") ?? "30" });
  }
  console.error(`ingest: project ${projectId} cell answered ${answer.status}`);
  return sentryError(502, "cannot buffer the request");
}

/// Caddy sets `X-Forwarded-For`; celld is not told to trust it, because the only
/// uses here are the stored row and the log line.
function clientIp(request: Request): string | null {
  const raw = request.headers.get("x-forwarded-for");
  if (!raw) return null;
  const first = raw.split(",")[0].trim();
  return first.length > 0 && first.length <= 64 ? first : null;
}

async function detailOf(answer: Response, fallback: string): Promise<string> {
  try {
    const payload: any = await answer.json();
    return typeof payload?.detail === "string" ? payload.detail : fallback;
  } catch {
    return fallback;
  }
}

// ---- the registry cache ------------------------------------------------------

async function lookup(env: Env, projectId: string): Promise<Project | null> {
  const ttl = Number(env.REGISTRY_TTL_MS ?? "") || DEFAULT_REGISTRY_TTL_MS;
  if (!cache || Date.now() - cache.at > ttl) cache = { at: Date.now(), projects: await loadRegistry(env) };
  const project = cache.projects.get(projectId);
  return project && project.active ? project : null;
}

async function loadRegistry(env: Env): Promise<Map<string, Project>> {
  const projects = new Map<string, Project>();
  try {
    const stub = env.REGISTRY.get(env.REGISTRY.idFromName("registry"));
    const answer = await stub.fetch("https://cell.invalid/registry", { method: "GET" });
    const payload: any = await answer.json();
    for (const entry of payload?.projects ?? []) {
      projects.set(String(entry.project_id), { key: String(entry.key), active: entry.active !== false });
    }
  } catch (error) {
    // An unreadable registry authenticates nobody. Failing closed here turns a
    // storage outage into 404s rather than into an open door.
    console.error(`ingest: cannot read the registry: ${error}`);
  }
  return projects;
}

// ---- the control plane -------------------------------------------------------

/// Drain, ack, and registry. Three layers guard these in production — caddy
/// forwards only `/api/`, iroh-ingress admits only nashcode's EndpointId, and
/// the bearer token below — and this is the innermost.
///
/// Rejection is 404 rather than 401 on purpose: a caller with no token learns
/// nothing about what lives here. A drainer that suddenly sees 404 on every call
/// has a token problem, and that is written down in the README.
async function control(request: Request, env: Env, url: URL): Promise<Response> {
  const token = env.DRAIN_TOKEN ?? "";
  const presented = bearer(request);
  if (token.length === 0 || !presented || !secretEquals(presented, token)) {
    if (token.length === 0) console.error("ingest: a control request arrived with no DRAIN_TOKEN configured");
    return jsonResponse(404, { detail: "not found" });
  }

  const parts = url.pathname.split("/").filter((part) => part.length > 0);
  // parts[0] is "_nashcode".
  if (parts.length === 2 && parts[1] === "registry") {
    if (request.method !== "GET" && request.method !== "PUT") {
      return jsonResponse(405, { detail: "the registry takes GET and PUT" });
    }
    const stub = env.REGISTRY.get(env.REGISTRY.idFromName("registry"));
    const answer = await stub.fetch("https://cell.invalid/registry", {
      method: request.method,
      body: request.method === "PUT" ? await request.text() : undefined,
    });
    // The next public request must see the new set, not the cached one. Other
    // isolates fall back to the TTL.
    if (request.method === "PUT" && answer.status === 200) cache = null;
    return passthrough(answer);
  }

  if (parts.length === 3 && (parts[1] === "drain" || parts[1] === "ack")) {
    const projectId = parts[2];
    if (!/^[0-9]{1,19}$/.test(projectId)) return jsonResponse(404, { detail: "not found" });
    const wanted = parts[1] === "drain" ? "GET" : "POST";
    if (request.method !== wanted) return jsonResponse(405, { detail: `use ${wanted}` });
    const stub = env.INGEST.get(env.INGEST.idFromName(projectId));
    const path = parts[1] === "drain" ? `/drain${url.search}` : "/ack";
    const answer = await stub.fetch(`https://cell.invalid${path}`, {
      method: request.method,
      body: request.method === "POST" ? await request.text() : undefined,
    });
    return passthrough(answer);
  }

  if (parts.length === 3 && parts[1] === "stats" && request.method === "GET") {
    const projectId = parts[2];
    if (!/^[0-9]{1,19}$/.test(projectId)) return jsonResponse(404, { detail: "not found" });
    const stub = env.INGEST.get(env.INGEST.idFromName(projectId));
    return passthrough(await stub.fetch("https://cell.invalid/stats", { method: "GET" }));
  }

  return jsonResponse(404, { detail: "not found" });
}

function bearer(request: Request): string | null {
  const raw = request.headers.get("authorization");
  if (!raw) return null;
  const match = /^bearer\s+(.+)$/i.exec(raw.trim());
  return match ? match[1].trim() : null;
}

/// The cell's answer, byte for byte, with its own headers. No CORS: nothing here
/// is for a browser.
async function passthrough(answer: Response): Promise<Response> {
  return new Response(answer.body, { status: answer.status, headers: answer.headers });
}
