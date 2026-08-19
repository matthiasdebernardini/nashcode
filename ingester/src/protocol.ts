// The parts of the Sentry ingest contract that are the same at the edge as they
// are inside nashcode. Every constant here is copied from goal.md's interop
// rules, which are copied in turn from develop.sentry.dev and relay's cors.rs.
// Change one of them here and you have to change it in viewer/src/web/bugs.rs
// too: an SDK talks to whichever door it was given, and the two must answer the
// same way.

/// Relay's CORS allow list, verbatim: eleven headers.
///
/// The browser SDK sends no custom headers by default, but `fetchOptions` and a
/// couple of Chrome quirks still produce preflights, and a preflight that fails
/// silently loses every browser event.
export const ALLOW_HEADERS =
  "x-sentry-auth, x-requested-with, x-forwarded-for, origin, referer, " +
  "accept, content-type, authentication, authorization, content-encoding, transfer-encoding";

/// The three response headers an SDK reads to back off. Unexposed, browser
/// backoff breaks without a single visible symptom.
export const EXPOSE_HEADERS = "x-sentry-error, x-sentry-rate-limits, retry-after";

/// Tell every SDK to stop sending what nashcode does not store, for a day, on
/// every answer.
///
/// `error`, `default`, `log_item`, `monitor` and `session` are deliberately
/// absent — those are the categories we want — and the list is never empty,
/// because an empty list means "everything" and would silence errors too.
export const RATE_LIMITS =
  "86400:transaction;span;profile;profile_chunk;replay;trace_metric:project";

/// The Worker is the only body cap in the system: celld has none, and a cell row
/// caps at about 2.2 MB. 2 MiB compressed is generous for errors and logs, and
/// attachments are dropped at digest anyway.
export const MAX_BODY = 2 * 1024 * 1024;

/// A request whose only credential is the `dsn` inside the envelope has to be
/// read before it can be judged, so it gets a small budget instead of the full
/// one. Same number as the viewer's `MAX_UNAUTHED_COMPRESSED`.
export const MAX_UNAUTHED_BODY = 64 * 1024;

/// How far into a decompressed body we will look for the envelope header line.
/// The header is the first line and is tens of bytes; anything past this is not
/// an envelope we can read.
export const MAX_HEADER_LINE = 64 * 1024;

export function corsHeaders(headers: Headers): void {
  headers.set("access-control-allow-origin", "*");
  headers.set("access-control-expose-headers", EXPOSE_HEADERS);
}

export function jsonResponse(status: number, payload: unknown, extra?: Record<string, string>): Response {
  const headers = new Headers({ "content-type": "application/json" });
  if (extra) {
    for (const [name, value] of Object.entries(extra)) headers.set(name, value);
  }
  corsHeaders(headers);
  return new Response(JSON.stringify(payload), { status, headers });
}

/// An error an SDK can read: the documented `{"detail": "..."}` body, the
/// `X-Sentry-Error` header relay sets, and the CORS headers without which a
/// browser sees none of it.
export function sentryError(status: number, detail: string, extra?: Record<string, string>): Response {
  const headers: Record<string, string> = { ...(extra ?? {}) };
  // Header values are latin-1; a detail we cannot encode still goes in the body.
  if (/^[\x20-\x7e]*$/.test(detail)) headers["x-sentry-error"] = detail;
  return jsonResponse(status, { detail }, headers);
}

/// The browser preflight, with relay's eleven-header allow list.
export function preflight(): Response {
  const headers = new Headers({
    "access-control-allow-methods": "POST",
    "access-control-allow-headers": ALLOW_HEADERS,
    "access-control-max-age": "3600",
  });
  corsHeaders(headers);
  return new Response(null, { status: 200, headers });
}

/// `X-Sentry-Auth: Sentry sentry_key=..., sentry_version=7, ...`
///
/// Hand-parsed rather than pulled from `sentry-types`: the edge has no crate
/// dependencies and the grammar is a comma-separated key=value list.
export function keyFromAuthHeader(raw: string | null): string | null {
  if (!raw) return null;
  const trimmed = raw.trim();
  const body = /^sentry\s+/i.test(trimmed) ? trimmed.slice(trimmed.indexOf(" ") + 1) : trimmed;
  for (const part of body.split(",")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    if (part.slice(0, eq).trim().toLowerCase() !== "sentry_key") continue;
    return normaliseKey(part.slice(eq + 1).trim());
  }
  return null;
}

/// `?sentry_key=...` — how the browser SDK authenticates, precisely so its POST
/// stays a CORS simple request.
export function keyFromQuery(url: URL): string | null {
  return normaliseKey(url.searchParams.get("sentry_key"));
}

/// The public key of the `dsn` in one envelope header line.
///
/// A DSN is `https://<key>@<host>/<project>`; the key is the URL's username.
export function keyFromDsn(dsn: unknown): string | null {
  if (typeof dsn !== "string") return null;
  try {
    return normaliseKey(new URL(dsn).username);
  } catch {
    return null;
  }
}

/// A DSN key is 32 lowercase hex characters. Anything else never matches a
/// project, so refusing it here keeps junk out of the comparison.
export function normaliseKey(raw: string | null | undefined): string | null {
  if (!raw) return null;
  const key = raw.trim().toLowerCase();
  return /^[0-9a-f]{32}$/.test(key) ? key : null;
}

/// Sentry event ids are 32 hex characters with no dashes.
export function newEventId(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  let out = "";
  for (const byte of bytes) out += byte.toString(16).padStart(2, "0");
  return out;
}

/// Compare two secrets without a length- or content-dependent early exit.
export function secretEquals(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}
