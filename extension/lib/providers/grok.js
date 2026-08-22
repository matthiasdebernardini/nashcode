// Grok STT (xAI) — the authoritative batch transcriber for the final pass.
//
// xAI is batch-only: there is NO streaming STT endpoint (/v1/stt/stream → 404,
// verified 2026-06-13), so the realtime preview lives elsewhere (Chrome's local
// Web Speech API in offscreen.js). The real /v1/stt response is word-level
// { text, words:[{text,start,end,speaker}] } — normalize() coalesces it into
// per-speaker segments and tolerates the older shapes for forward-compat.

const BATCH_URL = 'https://api.x.ai/v1/stt';

function token(cfg) {
  // nashcode mints nothing: the key is the user's own, kept in
  // chrome.storage.local (never sync). Missing key must say exactly where to
  // put it, or the failure reads like a transcription bug.
  if (cfg.xaiKey) return cfg.xaiKey;
  throw new Error('no xAI API key — add one in nashmeet Settings → "xAI API key"');
}

/// Batch transcription with diarization + word timestamps.
/// Returns normalized segments [{speaker, start_ms, end_ms, text}].
export async function transcribeBatch(cfg, blob) {
  const tok = token(cfg);
  const form = new FormData();
  form.append('file', blob, 'audio.webm');
  form.append('model', 'grok-stt');
  form.append('diarize', 'true');
  form.append('timestamps', 'word');
  const r = await fetch(BATCH_URL, {
    method: 'POST',
    headers: { authorization: `Bearer ${tok}` },
    body: form,
  });
  if (!r.ok) throw new Error(`grok batch failed: ${r.status} ${await r.text()}`);
  const body = await r.json();
  const segs = normalize(body);
  if (!segs.length) {
    // Diagnostic: distinguish an empty/silent recording (tiny blob) from a
    // format/parse miss (real blob but Grok found nothing) — the bare "empty
    // transcript" hid which. Keep it: it's cheap and saves a debug round-trip.
    const sizeKB = blob ? Math.round(blob.size / 1024) : '?';
    const text = (body.text || '').slice(0, 80);
    throw new Error(
      `0 segments [blob ${sizeKB}KB · resp keys: ${Object.keys(body).join(',') || 'none'} · text:"${text}"]`,
    );
  }
  return segs;
}

/// Normalize xAI's STT response to segments [{speaker, start_ms, end_ms, text}].
/// xAI actually returns word-level {text, words:[{text,start,end,speaker}]}; we
/// also keep the utterance/segment shapes (forward-compat + tests). A bare
/// {text} with no timings degrades to a single segment.
export function normalize(body) {
  const items = body.segments || body.utterances;
  if (items?.length) {
    return items.map((u) => ({
      speaker: u.speaker ?? u.speaker_id ?? 0,
      start_ms: toMs(u.start_ms ?? u.start),
      end_ms: toMs(u.end_ms ?? u.end),
      text: u.text ?? u.transcript ?? '',
    }));
  }
  if (body.words?.length) return coalesceWords(body.words);
  if (body.text) return [{ speaker: 0, start_ms: 0, end_ms: 0, text: body.text }];
  return [];
}

/// Group a flat word stream into utterance segments, breaking on speaker change.
function coalesceWords(words) {
  const segs = [];
  for (const w of words) {
    const speaker = w.speaker ?? w.speaker_id ?? 0;
    const text = (w.text ?? w.word ?? '').trim();
    const start_ms = toMs(w.start ?? w.start_ms);
    const end_ms = toMs(w.end ?? w.end_ms);
    const last = segs[segs.length - 1];
    if (last && last.speaker === speaker) {
      last.text += (last.text && text ? ' ' : '') + text;
      last.end_ms = end_ms;
    } else {
      segs.push({ speaker, start_ms, end_ms, text });
    }
  }
  return segs;
}

function toMs(v) {
  if (v == null) return 0;
  // Heuristic: timestamps under 10^7 with fractions are seconds.
  return Number.isInteger(v) && v > 100000 ? v : Math.round(v * 1000);
}

