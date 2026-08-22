// node --test nashmeet-extension/tests/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  recIdFromDate,
  slugify,
  mergeChannels,
  prefillSpeakers,
  longestUtterances,
  collapseByAssignedName,
  buildPayload,
  parsePeople,
  mergeNames,
} from '../lib/mapping-core.js';

const MIC_DEFAULTS = ['Matthias', 'Rob'];

// The flagged worry: calendar says 2 attendees, audio contains 3 voices
// (remote person on tab, you + Rob sharing the webcam on mic).
function sharedCamFixture() {
  return mergeChannels([
    {
      channel: 'tab',
      segments: [
        { speaker: 0, start_ms: 1000, end_ms: 6000, text: 'Thanks for joining, both of you.' },
      ],
    },
    {
      channel: 'mic',
      segments: [
        { speaker: 0, start_ms: 6500, end_ms: 11000, text: 'Glad to be here, Jane.' },
        { speaker: 1, start_ms: 11500, end_ms: 14000, text: 'Likewise!' },
        { speaker: 0, start_ms: 14500, end_ms: 15000, text: 'So.' },
      ],
    },
  ]);
}

test('mergeChannels: three voices across two channels get stable global ids', () => {
  const merged = sharedCamFixture();
  assert.equal(merged.speakers.length, 3);
  assert.deepEqual(
    merged.speakers.map((s) => s.channel),
    ['tab', 'mic', 'mic'],
  );
  // Timeline is sorted across channels.
  const starts = merged.segments.map((s) => s.start_ms);
  assert.deepEqual(starts, [...starts].sort((a, b) => a - b));
  // Same local speaker maps to the same global id both times.
  const micFirst = merged.segments.filter((s) => s.text.startsWith('Glad'))[0];
  const micAgain = merged.segments.filter((s) => s.text === 'So.')[0];
  assert.equal(micFirst.speaker, micAgain.speaker);
});

test('prefill: shared-cam case — remote prefills from calendar, mic prechecks You/Rob', () => {
  const merged = sharedCamFixture();
  const attendees = [
    { name: 'Jane Doe', email: 'jane@acme.com' },
    { name: 'Matthias', email: 'matthias@nashvilleautomation.io' },
  ];
  const cards = prefillSpeakers(merged.speakers, attendees, MIC_DEFAULTS);

  const tab = cards.find((c) => c.channel === 'tab');
  // Only one remote candidate remains (Matthias is a mic default) and only
  // one tab speaker — safe to prefill.
  assert.equal(tab.prefill, 'Jane Doe');
  assert.equal(tab.prechecked, true);

  const mics = cards.filter((c) => c.channel === 'mic');
  assert.deepEqual(mics.map((c) => c.prefill), ['Matthias', 'Rob']);
  assert.ok(mics.every((c) => c.prechecked));
});

test('prefill: several remote candidates → no guessing, offered as candidates', () => {
  const merged = mergeChannels([
    {
      channel: 'tab',
      segments: [{ speaker: 0, start_ms: 0, end_ms: 1, text: 'hi' }],
    },
  ]);
  const attendees = [
    { name: 'Jane Doe', email: 'jane@acme.com' },
    { name: 'John Roe', email: 'john@acme.com' },
  ];
  const cards = prefillSpeakers(merged.speakers, attendees, MIC_DEFAULTS);
  assert.equal(cards[0].prefill, null);
  assert.deepEqual(cards[0].candidates, ['Jane Doe', 'John Roe']);
});

test('prefill: no calendar event → free-text with mic defaults still prechecked', () => {
  const merged = sharedCamFixture();
  const cards = prefillSpeakers(merged.speakers, [], MIC_DEFAULTS);
  assert.equal(cards.find((c) => c.channel === 'tab').prefill, null);
  assert.deepEqual(
    cards.filter((c) => c.channel === 'mic').map((c) => c.prefill),
    ['Matthias', 'Rob'],
  );
});

test('prefill: third mic voice beyond defaults gets no precheck', () => {
  const merged = mergeChannels([
    {
      channel: 'mic',
      segments: [
        { speaker: 0, start_ms: 0, end_ms: 1, text: 'a' },
        { speaker: 1, start_ms: 2, end_ms: 3, text: 'b' },
        { speaker: 2, start_ms: 4, end_ms: 5, text: 'c' },
      ],
    },
  ]);
  const cards = prefillSpeakers(merged.speakers, [], MIC_DEFAULTS);
  assert.deepEqual(cards.map((c) => c.prefill), ['Matthias', 'Rob', null]);
  assert.equal(cards[2].prechecked, false);
});

test('longestUtterances picks the longest per speaker', () => {
  const merged = sharedCamFixture();
  const micId = merged.speakers.find((s) => s.channel === 'mic').id;
  const utts = longestUtterances(merged.segments, micId, 2);
  assert.equal(utts[0].text, 'Glad to be here, Jane.');
  assert.equal(utts.length, 2);
});

test('buildPayload: full assignment → speakers_confirmed', () => {
  const merged = sharedCamFixture();
  const session = {
    startedAt: '2026-06-12T15:00:00Z',
    endedAt: '2026-06-12T15:30:00Z',
    title: 'tab title',
    meetingUrl: 'https://meet.google.com/x',
  };
  const assignments = Object.fromEntries(merged.speakers.map((s, i) => [s.id, `P${i}`]));
  const p = buildPayload({
    session,
    merged,
    assignments,
    calendarEvent: { id: 'e1', title: 'Weekly', attendees: [] },
    provider: 'grok-batch',
  });
  assert.equal(p.speakers_confirmed, true);
  assert.equal(p.title, 'Weekly'); // calendar title wins over tab title
  assert.equal(p.segments.length, 4);
});

test('buildPayload: skipped mapping → unconfirmed, null names', () => {
  const merged = sharedCamFixture();
  const p = buildPayload({
    session: { startedAt: 's', endedAt: 'e', title: 't' },
    merged,
    assignments: {},
    calendarEvent: null,
    provider: null,
  });
  assert.equal(p.speakers_confirmed, false);
  assert.ok(p.speakers.every((s) => s.name === null));
});

// ---- collapse same-named speakers across chunks ----

test('collapseByAssignedName: same name in two chunks merges into one speaker', () => {
  const speakers = [
    { id: 'S1', channel: 'tab' },
    { id: 'S2', channel: 'mic' },
    { id: 'S3', channel: 'mic' },
  ];
  const segments = [
    { speaker: 'S1', start_ms: 0, end_ms: 1, text: 'a' },
    { speaker: 'S2', start_ms: 2, end_ms: 3, text: 'b' },
    { speaker: 'S3', start_ms: 4, end_ms: 5, text: 'c' },
  ];
  // S1 and S3 are the same person diarized in two chunks → both "Matthias".
  const assignments = { S1: 'Matthias', S2: 'Jane', S3: ' matthias ' };
  const out = collapseByAssignedName(speakers, segments, assignments);
  assert.deepEqual(out.speakers.map((s) => s.id), ['S1', 'S2']); // S3 folded into S1
  assert.deepEqual(out.segments.map((s) => s.speaker), ['S1', 'S2', 'S1']);
});

test('collapseByAssignedName: unnamed speakers pass through untouched', () => {
  const speakers = [
    { id: 'S1', channel: 'tab' },
    { id: 'S2', channel: 'mic' },
  ];
  const segments = [
    { speaker: 'S1', start_ms: 0, end_ms: 1, text: 'a' },
    { speaker: 'S2', start_ms: 2, end_ms: 3, text: 'b' },
  ];
  const out = collapseByAssignedName(speakers, segments, {}); // skipped mapping
  assert.deepEqual(out.speakers.map((s) => s.id), ['S1', 'S2']);
  assert.deepEqual(out.segments.map((s) => s.speaker), ['S1', 'S2']);
});

test('buildPayload: collapses same-named chunk speakers into one shipped speaker', () => {
  const merged = {
    speakers: [
      { id: 'S1', channel: 'tab' },
      { id: 'S2', channel: 'mic' },
      { id: 'S3', channel: 'mic' },
    ],
    segments: [
      { speaker: 'S1', start_ms: 0, end_ms: 1, text: 'a' },
      { speaker: 'S2', start_ms: 2, end_ms: 3, text: 'b' },
      { speaker: 'S3', start_ms: 4, end_ms: 5, text: 'c' },
    ],
  };
  const p = buildPayload({
    session: { startedAt: 's', endedAt: 'e', title: 't' },
    merged,
    assignments: { S1: 'Matthias', S2: 'Jane', S3: 'Matthias' },
    calendarEvent: null,
    provider: 'grok-batch',
  });
  // One Matthias, one Jane — the cross-chunk fragments are unified on the wire.
  assert.equal(p.speakers.length, 2);
  assert.deepEqual(p.speakers.map((s) => s.name).sort(), ['Jane', 'Matthias']);
  assert.equal(p.speakers_confirmed, true);
  assert.equal(p.segments.length, 3);
  const matthias = p.speakers.find((s) => s.name === 'Matthias').id;
  assert.equal(p.segments.filter((s) => s.speaker === matthias).length, 2);
});

// ---- no-calendar context: people field + known-name pool ----

test('parsePeople splits on commas and newlines, trims, drops empties', () => {
  assert.deepEqual(parsePeople('Bob, Alice\nCarol'), ['Bob', 'Alice', 'Carol']);
  assert.deepEqual(parsePeople('  Bob ,, \n  Dave  '), ['Bob', 'Dave']);
  assert.deepEqual(parsePeople(''), []);
  assert.deepEqual(parsePeople(null), []);
});

test('mergeNames unions case-insensitively, first spelling wins, order kept', () => {
  const pool = mergeNames(['Matthias', 'Rob'], ['Bob'], [' matthias '], ['', 'Alice', 'bob']);
  assert.deepEqual(pool, ['Matthias', 'Rob', 'Bob', 'Alice']);
});

test('buildPayload: manualTitle fills the title when there is no calendar event', () => {
  const merged = sharedCamFixture();
  const p = buildPayload({
    session: { startedAt: 's', endedAt: 'e', title: 'tab title' },
    merged,
    assignments: {},
    calendarEvent: null,
    provider: null,
    manualTitle: '  Bob — paddleboarding 1:1  ',
  });
  assert.equal(p.title, 'Bob — paddleboarding 1:1'); // trimmed, beats tab title
});

test('buildPayload: calendar title still wins over a manualTitle', () => {
  const merged = sharedCamFixture();
  const p = buildPayload({
    session: { startedAt: 's', endedAt: 'e', title: 'tab title' },
    merged,
    assignments: {},
    calendarEvent: { id: 'e1', title: 'Weekly', attendees: [] },
    provider: null,
    manualTitle: 'whatever',
  });
  assert.equal(p.title, 'Weekly');
});

test('recIdFromDate mirrors the server transcript_id format', () => {
  const d = new Date('2026-06-12T15:00:00Z');
  assert.equal(recIdFromDate(d, 'Weekly Sync: Rob & Matthias'), '2026-06-12-1500-weekly-sync-rob-matthias');
  assert.equal(slugify('!!!'), 'meeting');
});
