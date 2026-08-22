// IndexedDB persistence for recordings — disk-backed, so a crashed tab, a
// dropped stream, or a provider outage can never lose a meeting. Chunks are
// appended as MediaRecorder emits them; the mapping screen reassembles blobs.

const DB_NAME = 'nashmeet';
// v2 adds per-chunk `segmentIndex` + `offsetMs` for long-meeting chunking.
// They're denormalized onto each chunk row, so no new store or index is needed
// and the upgrade is a no-op; pre-v2 rows that lack the fields read as
// segment 0 / offset 0 (channelSegments coalesces them), so old recordings
// captured before this change still reassemble correctly.
const DB_VERSION = 2;

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains('chunks')) {
        const chunks = db.createObjectStore('chunks', { autoIncrement: true });
        chunks.createIndex('bySession', ['sessionId', 'channel'], { unique: false });
      }
      if (!db.objectStoreNames.contains('sessions')) {
        db.createObjectStore('sessions', { keyPath: 'sessionId' });
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function tx(db, store, mode, fn) {
  return new Promise((resolve, reject) => {
    const t = db.transaction(store, mode);
    const result = fn(t.objectStore(store));
    t.oncomplete = () => resolve(result?.result ?? result);
    t.onerror = () => reject(t.error);
  });
}

// Each export wraps its work in try/finally so db.close() runs even when the
// transaction rejects (a failed write, an aborted tx, QuotaExceededError on a
// full disk) — a leaked open connection would otherwise block a later
// onupgradeneeded when DB_VERSION bumps.
export async function appendChunk(sessionId, channel, blob, segmentIndex = 0, offsetMs = 0) {
  const db = await openDb();
  try {
    await tx(db, 'chunks', 'readwrite', (s) =>
      s.add({ sessionId, channel, blob, segmentIndex, offsetMs }),
    );
  } finally {
    db.close();
  }
}

export async function saveSession(meta) {
  const db = await openDb();
  try {
    await tx(db, 'sessions', 'readwrite', (s) => s.put(meta));
  } finally {
    db.close();
  }
}

export async function getSession(sessionId) {
  const db = await openDb();
  try {
    return await new Promise((resolve, reject) => {
      const req = db.transaction('sessions').objectStore('sessions').get(sessionId);
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  } finally {
    db.close();
  }
}

/// Reassemble one channel as ordered, independently-playable segments:
///   [{ segmentIndex, offsetMs, blob }, …]
/// Each segment is a complete webm (its own MediaRecorder lifetime); offsetMs
/// is its start relative to session start, used to stitch transcripts back onto
/// one timeline. getAll on the [sessionId, channel] index returns rows in
/// primary-key (insertion) order, so both within-segment chunk order and
/// across-segment order are preserved. Pre-v2 rows lacking the fields coalesce
/// into a single segment 0 at offset 0 — i.e. exactly the old behavior.
export async function channelSegments(sessionId, channel) {
  const db = await openDb();
  let rows;
  try {
    rows = await new Promise((resolve, reject) => {
      const idx = db.transaction('chunks').objectStore('chunks').index('bySession');
      const req = idx.getAll(IDBKeyRange.only([sessionId, channel]));
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  } finally {
    db.close();
  }
  if (!rows.length) return [];
  const groups = new Map();
  for (const r of rows) {
    const si = r.segmentIndex ?? 0;
    if (!groups.has(si)) {
      groups.set(si, { segmentIndex: si, offsetMs: r.offsetMs ?? 0, parts: [] });
    }
    groups.get(si).parts.push(r.blob);
  }
  return [...groups.values()]
    .sort((a, b) => a.segmentIndex - b.segmentIndex)
    .map((g) => ({
      segmentIndex: g.segmentIndex,
      offsetMs: g.offsetMs,
      blob: new Blob(g.parts, { type: 'audio/webm;codecs=opus' }),
    }));
}

/// Delete a session's chunks + meta once it's safely filed on the box.
export async function deleteSession(sessionId) {
  const db = await openDb();
  try {
    await new Promise((resolve, reject) => {
      const t = db.transaction(['chunks', 'sessions'], 'readwrite');
      const idx = t.objectStore('chunks').index('bySession');
      for (const channel of ['tab', 'mic']) {
        const req = idx.openCursor(IDBKeyRange.only([sessionId, channel]));
        req.onsuccess = () => {
          const cur = req.result;
          if (cur) {
            cur.delete();
            cur.continue();
          }
        };
      }
      t.objectStore('sessions').delete(sessionId);
      t.oncomplete = resolve;
      t.onerror = () => reject(t.error);
    });
  } finally {
    db.close();
  }
}
