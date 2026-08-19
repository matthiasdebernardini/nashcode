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
///
/// It only ever holds a set that was read successfully. A failed refresh leaves
/// the last good one in place, because the alternative — caching emptiness — is
/// far worse than serving a stale key: an SDK reads 4xx as permanent and throws
/// the event away, so one second of registry trouble would silently destroy a
/// minute of everybody's telemetry.
let cache: { at: number; projects: Map<string, Project> } | null = null;

const DEFAULT_REGISTRY_TTL_MS = 60_000;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    try {
      return await route(request, env);
    } catch (error) {
      // Nothing should reach here. If something does, an SDK must hear
      // "try again", with the CORS headers a browser needs to read it at all.
      console.error(`ingest: unhandled failure: ${error}`);
      return sentryError(502, "the ingester failed to handle the request", { "retry-after": "30" });
    }
  },
};

async function route(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  // `//api/1/envelope/` reaches a Worker as-is. It authenticates like any other
  // request, but it is the shape a caddy path matcher gets wrong, so collapse it
  // here rather than trust two matchers to agree.
  const pathname = url.pathname.replace(/\/{2,}/g, "/");

  if (pathname.startsWith("/_nashcode/")) return control(request, env, url, pathname);

  const api = matchApi(pathname);
  if (!api) return sentryError(404, "no such endpoint");

  if (request.method === "OPTIONS") return preflight();
  if (request.method !== "POST") return sentryError(405, "this endpoint takes POST");

  return api.door === "envelope"
    ? acceptEnvelope(request, env, url, api.projectId)
    : acceptLogs(request, env, url, api.projectId);
}

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
  const found = await lookup(env, projectId);
  if (found.status !== "known") return unresolved(found.status);
  const project = found.project;

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
  const found = await lookup(env, projectId);
  if (found.status !== "known") return unresolved(found.status);
  const project = found.project;

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

/// The client address, from the one entry of `X-Forwarded-For` a proxy wrote.
///
/// Caddy **appends** the peer it actually saw, so the trustworthy entry is the
/// last one, not the first. The first is whatever the client sent, and a client
/// will send anything: an address that is not its own, a hostname, a sentence, a
/// `<script>` tag. Taking the first put attacker-chosen text in a stored row and
/// carried it across to nashcode.
///
/// It still has to parse as an address, because caddy is only trustworthy when
/// caddy is what set the header — see the `header_up` line in the README.
function clientIp(request: Request): string | null {
  const raw = request.headers.get("x-forwarded-for");
  if (!raw) return null;
  const entries = raw.split(",");
  const last = entries[entries.length - 1].trim();
  return isIpAddress(last) ? last : null;
}

/// IPv4 dotted quad, or IPv6 with an optional `::` run and an optional zone or
/// embedded IPv4 tail. Deliberately strict: anything unrecognised is dropped
/// rather than stored.
function isIpAddress(value: string): boolean {
  if (value.length === 0 || value.length > 45) return false;
  if (/^(\d{1,3})(\.\d{1,3}){3}$/.test(value)) {
    return value.split(".").every((part) => part.length <= 3 && Number(part) <= 255);
  }
  const address = value.split("%")[0];
  if (!/^[0-9a-fA-F:.]+$/.test(address) || !address.includes(":")) return false;
  const halves = address.split("::");
  if (halves.length > 2) return false;
  let groups = 0;
  for (const half of halves) {
    if (half.length === 0) continue;
    for (const part of half.split(":")) {
      if (part.length === 0) return false;
      if (part.includes(".")) {
        if (!isIpAddress(part)) return false;
        groups += 2;
      } else if (/^[0-9a-fA-F]{1,4}$/.test(part)) {
        groups += 1;
      } else {
        return false;
      }
    }
  }
  return halves.length === 2 ? groups <= 7 : groups === 8;
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

type Lookup =
  | { status: "known"; project: Project }
  | { status: "unknown" }
  | { status: "unavailable" };

async function lookup(env: Env, projectId: string): Promise<Lookup> {
  const ttl = Number(env.REGISTRY_TTL_MS ?? "") || DEFAULT_REGISTRY_TTL_MS;
  if (!cache || Date.now() - cache.at > ttl) {
    try {
      cache = { at: Date.now(), projects: await loadRegistry(env) };
    } catch (error) {
      console.error(`ingest: cannot read the registry: ${error}`);
      // Serve the last good set rather than an empty one. A stale key outliving
      // a revocation by a few seconds is a routing identifier working slightly
      // too long; an empty set is every project on earth being told, in a status
      // code SDKs never retry, that it does not exist.
      if (!cache) return { status: "unavailable" };
      cache = { at: Date.now(), projects: cache.projects };
    }
  }
  const project = cache.projects.get(projectId);
  if (!project) return { status: "unknown" };
  return project.active ? { status: "known", project } : { status: "unknown" };
}

/// Read the whole set, or throw. Never return a partial or empty answer for a
/// failure — the caller cannot tell those apart from a registry that really is
/// empty, and it has to.
async function loadRegistry(env: Env): Promise<Map<string, Project>> {
  const stub = env.REGISTRY.get(env.REGISTRY.idFromName("registry"));
  const answer = await stub.fetch("https://cell.invalid/registry", { method: "GET" });
  if (answer.status !== 200) throw new Error(`the registry answered ${answer.status}`);
  const payload: any = await answer.json();
  if (!Array.isArray(payload?.projects)) throw new Error("the registry answered without a projects array");

  const projects = new Map<string, Project>();
  for (const entry of payload.projects) {
    projects.set(String(entry.project_id), { key: String(entry.key), active: entry.active !== false });
  }
  return projects;
}

/// A project we cannot serve. The difference matters more than it looks: 404 is
/// permanent and an SDK drops the event, 503 is temporary and it retries.
function unresolved(status: "unknown" | "unavailable"): Response {
  return status === "unknown"
    ? sentryError(404, "unknown project")
    : sentryError(503, "the project registry is unavailable", { "retry-after": "30" });
}

// ---- the control plane -------------------------------------------------------

/// Drain, ack, and registry. Three layers guard these in production — caddy
/// forwards only `/api/`, iroh-ingress admits only nashcode's EndpointId, and
/// the bearer token below — and this is the innermost.
///
/// Rejection is 404 rather than 401 on purpose: a caller with no token learns
/// nothing about what lives here. A drainer that suddenly sees 404 on every call
/// has a token problem, and that is written down in the README.
async function control(request: Request, env: Env, url: URL, pathname: string): Promise<Response> {
  const token = env.DRAIN_TOKEN ?? "";
  const presented = bearer(request);
  if (token.length === 0 || !presented || !secretEquals(presented, token)) {
    if (token.length === 0) console.error("ingest: a control request arrived with no DRAIN_TOKEN configured");
    return jsonResponse(404, { detail: "not found" });
  }

  const parts = pathname.split("/").filter((part) => part.length > 0);
  // parts[0] is "_nashcode".
  if (parts.length === 2 && parts[1] === "registry") {
    if (request.method !== "GET" && request.method !== "PUT") {
      return jsonResponse(405, { detail: "the registry takes GET and PUT" });
    }
    const stub = env.REGISTRY.get(env.REGISTRY.idFromName("registry"));
    const answer = await stub.fetch(`https://cell.invalid/registry${url.search}`, {
      method: request.method,
      body: request.method === "PUT" ? await request.text() : undefined,
    });
    // The next public request must see the new set, not the cached one. Expire
    // it rather than drop it: if the reload fails, the old set is still a far
    // better answer than none. Other isolates fall back to the TTL.
    if (request.method === "PUT" && answer.status === 200 && cache) cache = { at: 0, projects: cache.projects };
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
