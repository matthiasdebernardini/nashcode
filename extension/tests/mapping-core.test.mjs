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
  chooseRepo,
  matchedNames,
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

// --- chooseRepo: the repo box, decided by who is on the invite --------------

const ROB = { name: 'Rob Castro', email: 'rob@example.com' };
const JOEY = { name: 'Joey Locker', email: 'joey@example.com' };
const DANA = { name: 'Dana Poole', email: 'dana@example.com' };

/// The four arguments, with the invite-shaped defaults filled in.
const choose = (over) =>
  chooseRepo({ routing: null, attendees: [], defaultRepo: '', hasEvent: true, ...over });

const match = (over) => ({ project: 'agstaff', repo: 'agstaff', score: 1, ...over });

test('matchedNames: only the contacts the viewer scored, named from the invite', () => {
  const m = match({ contacts: [{ email: 'ROB@example.com' }, { phone: '+15550000000' }] });
  // The invite spells him "Rob Castro"; the answer only knows the address, and
  // a contact matched by phone has no name to find here.
  assert.deepEqual(matchedNames(m, [ROB, JOEY]), ['Rob Castro']);
  // An older viewer sends no `contacts` at all.
  assert.deepEqual(matchedNames(match({}), [ROB]), []);
});

test('chooseRepo: one match names only the people who matched', () => {
  const routing = {
    matches: [match({ folder: '~/w/agstaff', people: ['rob'], contacts: [{ email: 'rob@example.com' }] })],
    tie: false,
  };
  // Dana was on the invite and scored for nobody, so Dana is not named.
  assert.deepEqual(choose({ routing, attendees: [ROB, DANA], defaultRepo: 'fallback' }), {
    repo: 'agstaff',
    reason: 'agstaff — Rob Castro is on the invite',
    offered: [],
  });
});

test('chooseRepo: two matched people share one line, with plural grammar', () => {
  const routing = {
    matches: [
      match({ score: 2, people: ['rob', 'joey'], contacts: [{ email: 'rob@example.com' }, { email: 'joey@example.com' }] }),
    ],
    tie: false,
  };
  assert.equal(
    choose({ routing, attendees: [ROB, JOEY, DANA] }).reason,
    'agstaff — Rob Castro, Joey Locker are on the invite',
  );
});

test('chooseRepo: an attendee with no display name is named by address', () => {
  const routing = { matches: [match({ contacts: [{ email: 'rob@example.com' }] })], tie: false };
  const { reason } = choose({ routing, attendees: [{ name: null, email: 'rob@example.com' }] });
  assert.equal(reason, 'agstaff — rob@example.com is on the invite');
});

test('chooseRepo: an older viewer sends no contacts, so the line counts', () => {
  const routing = { matches: [match({ score: 1 })], tie: false };
  assert.equal(
    choose({ routing, attendees: [ROB, JOEY, DANA] }).reason,
    'agstaff — 1 of 3 on the invite match',
  );
});

test('chooseRepo: no match on the invite falls back to the settings repo', () => {
  assert.deepEqual(choose({ routing: { matches: [], tie: false }, attendees: [ROB], defaultRepo: 'nashcode' }), {
    repo: 'nashcode',
    reason: 'no match on the invite; using the default repo',
    offered: [],
  });
  assert.equal(
    choose({ routing: { matches: [], tie: false }, attendees: [ROB] }).reason,
    'no match on the invite; pick a repo',
  );
});

test('chooseRepo: no calendar event reads differently from no match', () => {
  assert.deepEqual(choose({ hasEvent: false, defaultRepo: 'nashcode' }), {
    repo: 'nashcode',
    reason: 'no calendar event; using the default repo',
    offered: [],
  });
  assert.equal(choose({ hasEvent: false }).reason, 'no calendar event; pick a repo');
  // An event whose attendees all filtered out (only you on it) is still an
  // event — nobody matched, which is not the same as nothing to match against.
  assert.equal(choose({ hasEvent: true, attendees: [] }).reason, 'no match on the invite; pick a repo');
});

test('chooseRepo: an unpushed people file and an unreachable viewer say so', () => {
  assert.equal(
    choose({
      routing: { matches: [], tie: false, unavailable: 'no people file' },
      attendees: [ROB],
      defaultRepo: 'nashcode',
    }).reason,
    'no people file pushed yet; using the default repo',
  );
  // routeAttendees answers null for a network drop, a 400, or a body that is
  // not JSON. None of those mean "nobody matched".
  assert.equal(
    choose({ routing: null, attendees: [ROB], defaultRepo: 'nashcode' }).reason,
    'routing could not be asked; using the default repo',
  );
  assert.equal(choose({ routing: null, attendees: [ROB] }).reason, 'routing could not be asked; pick a repo');
});

test('chooseRepo: a tie empties the box and offers both repos', () => {
  const routing = {
    matches: [
      match({ project: 'agstaff', repo: 'agstaff', contacts: [{ email: 'rob@example.com' }] }),
      match({ project: 'pristine', repo: 'pristine', contacts: [{ email: 'joey@example.com' }] }),
    ],
    tie: true,
  };
  assert.deepEqual(choose({ routing, attendees: [ROB, JOEY], defaultRepo: 'nashcode' }), {
    repo: '',
    reason: 'tie: agstaff or pristine',
    offered: ['agstaff', 'pristine'],
  });
});

test('chooseRepo: a three-way tie reads as a list', () => {
  const routing = {
    matches: [
      match({ project: 'a', repo: 'a' }),
      match({ project: 'b', repo: 'b' }),
      match({ project: 'c', repo: 'c' }),
    ],
    tie: true,
  };
  const out = choose({ routing, attendees: [ROB] });
  assert.equal(out.reason, 'tie: a, b or c');
  assert.deepEqual(out.offered, ['a', 'b', 'c']);
});

test('chooseRepo: a mixed tie keeps the repoless project in the sentence', () => {
  const routing = {
    matches: [
      match({ project: 'bee', repo: 'bee' }),
      match({ project: 'alpha', repo: null }),
    ],
    tie: true,
  };
  // Both are answers, so both are named — but only one can be filed into, so
  // only one is offered.
  assert.deepEqual(choose({ routing, attendees: [ROB], defaultRepo: 'nashcode' }), {
    repo: '',
    reason: 'tie: bee or alpha (no nashcode repo)',
    offered: ['bee'],
  });
});

test('chooseRepo: a matched project with no nashcode repo says so', () => {
  const routing = {
    matches: [match({ project: 'pristine', repo: null, folder: '~/w/pristine', people: ['brad'] })],
    tie: false,
  };
  // The settings default must NOT win here: the invite named a project, and
  // filing this meeting into some other repo would be the wrong answer.
  assert.deepEqual(choose({ routing, attendees: [ROB], defaultRepo: 'nashcode' }), {
    repo: '',
    reason: 'pristine has no nashcode repo; pick one',
    offered: [],
  });
});

test('chooseRepo: only the top score is part of the tie', () => {
  const routing = {
    matches: [
      match({ project: 'agstaff', repo: 'agstaff', score: 2 }),
      match({ project: 'pristine', repo: 'pristine', score: 2 }),
      match({ project: 'other', repo: 'other', score: 1 }),
    ],
    tie: true,
  };
  assert.deepEqual(choose({ routing, attendees: [ROB, JOEY] }).offered, ['agstaff', 'pristine']);
});
