import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  transcribeWithFallback,
  stitchSegmentResults,
  transcribeSegmented,
} from '../lib/transcribe.js';
import { normalize as grokNormalize } from '../lib/providers/grok.js';

const SEGS = [{ speaker: 0, start_ms: 0, end_ms: 1000, text: 'hi' }];
// Grok is the only registered provider, but the fallback machine is generic, so
// these exercise it with grok + a synthetic second provider injected by name.
const cfg = { providerOrder: ['grok', 'backup'] };

test('fallback: grok success short-circuits', async () => {
  const attempts = [];
  const res = await transcribeWithFallback(
    cfg,
    'blob',
    { grok: async () => SEGS, backup: async () => { throw new Error('unreachable'); } },
    (n) => attempts.push(n),
  );
  assert.equal(res.provider, 'grok-batch');
  assert.deepEqual(attempts, ['grok']);
});

test('fallback: grok failure falls through to the next provider', async () => {
  const attempts = [];
  const res = await transcribeWithFallback(
    cfg,
    'blob',
    {
      grok: async () => { throw new Error('xai 503'); },
      backup: async () => SEGS,
    },
    (n) => attempts.push(n),
  );
  assert.equal(res.provider, 'backup'); // unknown names label as themselves
  assert.deepEqual(attempts, ['grok', 'backup']);
});

test('fallback: empty transcript counts as failure', async () => {
  const res = await transcribeWithFallback(cfg, 'blob', {
    grok: async () => [],
    backup: async () => SEGS,
  });
  assert.equal(res.provider, 'backup');
});

test('fallback: all providers failing aggregates errors and reassures', async () => {
  await assert.rejects(
    transcribeWithFallback(cfg, 'blob', {
      grok: async () => { throw new Error('xai down'); },
      backup: async () => { throw new Error('backup down'); },
    }),
    (e) => {
      assert.match(e.message, /grok: xai down/);
      assert.match(e.message, /backup: backup down/);
      assert.match(e.message, /recording is safe locally/);
      return true;
    },
  );
});

test('grok normalize accepts segments or utterances, seconds or ms', () => {
  assert.deepEqual(
    grokNormalize({ segments: [{ speaker: 1, start: 1.5, end: 2.5, text: 'a' }] }),
    [{ speaker: 1, start_ms: 1500, end_ms: 2500, text: 'a' }],
  );
  assert.deepEqual(
    grokNormalize({ utterances: [{ speaker_id: 'spk0', start_ms: 500000, end_ms: 600000, transcript: 'b' }] }),
    [{ speaker: 'spk0', start_ms: 500000, end_ms: 600000, text: 'b' }],
  );
});

test('grok normalize coalesces xAI word-level {text,words} into speaker segments', () => {
  // The real xAI /v1/stt shape (verified against the live API 2026-06-13).
  const body = {
    text: 'Hello there general',
    language: 'en',
    duration: 2.0,
    words: [
      { text: 'Hello', start: 0.1, end: 0.3, speaker: 0 },
      { text: 'there', start: 0.35, end: 0.6, speaker: 0 },
      { text: 'general', start: 0.7, end: 1.1, speaker: 1 },
    ],
  };
  assert.deepEqual(grokNormalize(body), [
    { speaker: 0, start_ms: 100, end_ms: 600, text: 'Hello there' },
    { speaker: 1, start_ms: 700, end_ms: 1100, text: 'general' },
  ]);
});

test('grok normalize degrades a bare {text} to one segment', () => {
  assert.deepEqual(grokNormalize({ text: 'just words' }), [
    { speaker: 0, start_ms: 0, end_ms: 0, text: 'just words' },
  ]);
});

// ---- long-meeting chunking: stitch + segmented transcribe ----

test('stitchSegmentResults: offsets each segment, namespaces speakers, concatenates in order', () => {
  const out = stitchSegmentResults([
    { offsetMs: 0, segments: [{ speaker: 0, start_ms: 100, end_ms: 600, text: 'first' }] },
    {
      offsetMs: 600_000,
      segments: [
        { speaker: 0, start_ms: 0, end_ms: 400, text: 'second' },
        { speaker: 1, start_ms: 500, end_ms: 900, text: 'third' },
      ],
    },
  ]);
  assert.deepEqual(out, [
    { speaker: '0:0', start_ms: 100, end_ms: 600, text: 'first' },
    { speaker: '1:0', start_ms: 600_000, end_ms: 600_400, text: 'second' },
    { speaker: '1:1', start_ms: 600_500, end_ms: 600_900, text: 'third' },
  ]);
  // Speaker 0 in different segments is namespaced apart — they're not the same
  // person across independently-diarized chunks.
  assert.notEqual(out[0].speaker, out[1].speaker);
  // Timeline is monotonic.
  const starts = out.map((s) => s.start_ms);
  assert.deepEqual(starts, [...starts].sort((a, b) => a - b));
});

test('stitchSegmentResults: single segment at offset 0 is a plain pass-through (common case)', () => {
  const segs = [{ speaker: 0, start_ms: 0, end_ms: 1000, text: 'hi' }];
  const out = stitchSegmentResults([{ offsetMs: 0, segments: segs }]);
  assert.deepEqual(out, [{ speaker: '0:0', start_ms: 0, end_ms: 1000, text: 'hi' }]);
});

const recSeg = (segmentIndex, offsetMs) => ({ segmentIndex, offsetMs, blob: `blob-${segmentIndex}` });

test('transcribeSegmented: stitches every segment onto one timeline', async () => {
  const res = await transcribeSegmented(
    { providerOrder: ['grok'] },
    [recSeg(1, 600_000), recSeg(0, 0)], // intentionally out of order
    {
      grok: async (cfg, blob) =>
        blob === 'blob-0'
          ? [{ speaker: 0, start_ms: 0, end_ms: 500, text: 'a' }]
          : [{ speaker: 0, start_ms: 0, end_ms: 500, text: 'b' }],
    },
  );
  assert.equal(res.provider, 'grok-batch');
  assert.deepEqual(res.segments, [
    { speaker: '0:0', start_ms: 0, end_ms: 500, text: 'a' },
    { speaker: '1:0', start_ms: 600_000, end_ms: 600_500, text: 'b' },
  ]);
});

test('transcribeSegmented: tolerates a per-segment empty, keeps the rest', async () => {
  const res = await transcribeSegmented({ providerOrder: ['grok'] }, [recSeg(0, 0), recSeg(1, 600_000)], {
    grok: async (cfg, blob) =>
      blob === 'blob-1' ? [] : [{ speaker: 0, start_ms: 0, end_ms: 500, text: 'kept' }],
  });
  assert.equal(res.segments.length, 1);
  assert.equal(res.segments[0].text, 'kept');
});

test('transcribeSegmented: fails only if every segment is empty', async () => {
  await assert.rejects(
    transcribeSegmented({ providerOrder: ['grok'] }, [recSeg(0, 0), recSeg(1, 1000)], {
      grok: async () => [],
    }),
    (e) => {
      assert.match(e.message, /all 2 segment\(s\) empty\/failed/);
      assert.match(e.message, /recording is safe locally/);
      return true;
    },
  );
});

test('transcribeSegmented: propagates the provider label of a successful segment', async () => {
  const res = await transcribeSegmented({ providerOrder: ['grok', 'backup'] }, [recSeg(0, 0)], {
    grok: async () => { throw new Error('xai 503'); },
    backup: async () => [{ speaker: 0, start_ms: 0, end_ms: 1, text: 'x' }],
  });
  assert.equal(res.provider, 'backup');
});
