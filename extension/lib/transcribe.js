// The provider-fallback state machine: try each provider in configured order
// against the SAME local recording (ground truth), first success wins. The
// provider layer is a map of name → transcribeBatch(cfg, blob); adding another
// provider is registering it here, not a rewrite. Grok is the only one wired
// today.

import * as grok from './providers/grok.js';

// The provider registry. Internal — callers inject their own map in tests; the
// runtime path always uses this default, so it isn't part of the public API.
const PROVIDERS = {
  grok: grok.transcribeBatch,
};

/// Run one channel blob through the fallback chain.
/// Returns { provider, segments } or throws with every error aggregated.
/// `providers` is injectable for tests.
export async function transcribeWithFallback(cfg, blob, providers = PROVIDERS, onAttempt = () => {}) {
  const order = cfg.providerOrder?.length ? cfg.providerOrder : ['grok'];
  const errors = [];
  for (const name of order) {
    const fn = providers[name];
    if (!fn) {
      errors.push(`${name}: unknown provider`);
      continue;
    }
    onAttempt(name);
    try {
      const segments = await fn(cfg, blob);
      if (!segments.length) throw new Error('empty transcript');
      return { provider: providerLabel(name), segments };
    } catch (e) {
      errors.push(`${name}: ${e.message || e}`);
    }
  }
  throw new Error(
    `all providers failed — the recording is safe locally, retry from the mapping screen.\n${errors.join('\n')}`,
  );
}

function providerLabel(name) {
  return { grok: 'grok-batch' }[name] || name;
}

/// Stitch independently-transcribed segments back onto one meeting timeline.
/// Input: ordered [{ offsetMs, segments:[{speaker,start_ms,end_ms,text}] }].
/// Each segment is diarized on its own, so "speaker 0" in one is NOT the same
/// person as "speaker 0" in the next — we namespace the label by segment
/// position so mergeChannels assigns them distinct global ids (the
/// collapse-by-name step later re-unifies the same physical person). Every
/// timestamp gets the segment's offsetMs added, and segments are concatenated
/// in order, yielding a monotonic whole-meeting timeline. A single segment at
/// offset 0 is byte-identical to the unchunked path once mergeChannels
/// normalizes the labels.
export function stitchSegmentResults(results) {
  const out = [];
  results.forEach(({ offsetMs, segments }, i) => {
    const off = offsetMs || 0;
    for (const s of segments) {
      out.push({
        speaker: `${i}:${s.speaker}`,
        start_ms: s.start_ms + off,
        end_ms: s.end_ms + off,
        text: s.text,
      });
    }
  });
  return out;
}

/// Transcribe a channel's recorded segments (from channelSegments) and stitch
/// them into one timeline. Each segment runs through transcribeWithFallback
/// (unchanged). A per-segment empty/failure is tolerated — a channel can be
/// silent during one window (nobody on the tab, or you not speaking on the
/// mic) — and only an all-empty channel throws. Returns the same
/// { provider, segments } shape as transcribeWithFallback, so the caller is
/// unchanged. `recordedSegments` is [{ segmentIndex, offsetMs, blob }, …].
export async function transcribeSegmented(
  cfg,
  recordedSegments,
  providers = PROVIDERS,
  onAttempt = () => {},
) {
  const ordered = [...recordedSegments].sort((a, b) => a.segmentIndex - b.segmentIndex);
  const stitchInput = [];
  const errors = [];
  let provider = null;
  for (const seg of ordered) {
    try {
      const res = await transcribeWithFallback(cfg, seg.blob, providers, onAttempt);
      provider = res.provider;
      stitchInput.push({ offsetMs: seg.offsetMs, segments: res.segments });
    } catch (e) {
      // A silent segment is normal in a multi-segment recording — note and skip.
      errors.push(`segment ${seg.segmentIndex}: ${e.message || e}`);
    }
  }
  if (!stitchInput.length) {
    throw new Error(
      `all ${ordered.length} segment(s) empty/failed — the recording is safe locally, ` +
        `retry from the mapping screen.\n${errors.join('\n')}`,
    );
  }
  return { provider, segments: stitchSegmentResults(stitchInput) };
}
