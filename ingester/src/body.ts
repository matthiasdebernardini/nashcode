// Reading a request body without trusting it.
//
// Two jobs: never hold more than the cap in memory, and get at the envelope
// header line — the first line, which carries `dsn` and `event_id` — without
// decompressing the rest of the body.

import { MAX_HEADER_LINE } from "./protocol";

export interface CappedBody {
  /// The raw bytes exactly as they arrived. Compressed stays compressed: the
  /// edge stores what it was sent and nashcode expands it at digest.
  bytes: Uint8Array;
}

export class TooLarge extends Error {}

/// Read the whole body, counting as we go, and abort past `cap`.
///
/// `Content-Length` is checked first so an oversized upload is refused before a
/// byte of it is read, but the streaming count is the authority: a chunked body
/// declares no length and a lying one declares the wrong one.
export async function readCapped(request: Request, cap: number): Promise<CappedBody> {
  const declared = Number(request.headers.get("content-length") ?? "");
  if (Number.isFinite(declared) && declared > cap) {
    throw new TooLarge(`a body of ${declared} bytes is over the ${cap} byte limit`);
  }
  const body = request.body;
  if (!body) return { bytes: new Uint8Array(0) };

  const chunks: Uint8Array[] = [];
  let total = 0;
  const reader = body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > cap) {
      // Stop pulling. The sender is told 413 and the bytes are never buffered.
      await reader.cancel().catch(() => {});
      throw new TooLarge(`a body over the ${cap} byte limit`);
    }
    chunks.push(value);
  }
  return { bytes: concat(chunks, total) };
}

export function concat(chunks: Uint8Array[], total: number): Uint8Array {
  if (chunks.length === 1) return chunks[0];
  const out = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.byteLength;
  }
  return out;
}

/// The content encodings the edge can look inside. Brotli and zstd are stored
/// and forwarded like any other body; they just cannot be read here, which the
/// protocol allows — the answer is `{}` and the SDK carries on.
export function decompressionFormat(encoding: string | null): string | null {
  const name = (encoding ?? "").trim().toLowerCase();
  if (name === "" || name === "identity") return "identity";
  if (name === "gzip" || name === "x-gzip") return "gzip";
  if (name === "deflate") return "deflate";
  return null;
}

/// The first line of the body, decompressing only as far as that line.
///
/// Returns null when the encoding is unreadable here, when the stream is
/// corrupt, or when no newline turns up inside `MAX_HEADER_LINE` bytes.
export async function headerLine(bytes: Uint8Array, encoding: string | null): Promise<Uint8Array | null> {
  const format = decompressionFormat(encoding);
  if (format === null) return null;
  if (format === "identity") return firstLine(bytes);

  let stream: ReadableStream<Uint8Array>;
  try {
    stream = new Blob([bytes as BlobPart]).stream().pipeThrough(new DecompressionStream(format));
  } catch {
    return null;
  }
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(value);
      total += value.byteLength;
      if (indexOfNewline(value) >= 0 || total > MAX_HEADER_LINE) break;
    }
  } catch {
    return null;
  } finally {
    // We asked for one line. Let the rest of the body go.
    await reader.cancel().catch(() => {});
  }
  return firstLine(concat(chunks, total));
}

function indexOfNewline(bytes: Uint8Array): number {
  return bytes.indexOf(0x0a);
}

function firstLine(bytes: Uint8Array): Uint8Array | null {
  const end = indexOfNewline(bytes);
  const line = end < 0 ? bytes : bytes.subarray(0, end);
  if (line.byteLength === 0 || line.byteLength > MAX_HEADER_LINE) return null;
  return line;
}

/// Parse an envelope header line. A body that is not JSON is not an envelope,
/// which is a 403 when it was the only credential and a `{}` otherwise.
export function parseHeaderLine(line: Uint8Array | null): Record<string, unknown> | null {
  if (!line) return null;
  try {
    const value = JSON.parse(new TextDecoder().decode(line));
    return value && typeof value === "object" && !Array.isArray(value) ? value : null;
  } catch {
    return null;
  }
}

const BASE64_CHUNK = 0x8000;

/// Base64 without spreading a 2 MiB array into a call frame.
export function toBase64(bytes: Uint8Array): string {
  let binary = "";
  for (let at = 0; at < bytes.length; at += BASE64_CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(at, at + BASE64_CHUNK));
  }
  return btoa(binary);
}

export function fromBase64(text: string): Uint8Array {
  const binary = atob(text);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}
