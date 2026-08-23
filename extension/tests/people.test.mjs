// node --test nashmeet-extension/tests/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  selfFromAttendees,
  selfFromCalendarIds,
  routingContacts,
  routeUrl,
  routeAttendees,
} from '../lib/people.js';

const ME = 'me@example.com';
const ROB = { name: 'Rob Castro', email: 'rob@example.com' };
const JOEY = { name: 'Joey Locker', email: 'joey@example.com' };

// Swap in a fetch for one call and hand back what it was asked for.
function stubFetch(handler) {
  const before = globalThis.fetch;
  const calls = [];
  globalThis.fetch = async (url, opts) => {
    calls.push({ url: String(url), opts });
    return handler(String(url));
  };
  return { calls, restore: () => { globalThis.fetch = before; } };
}

const ok = (body) => ({ ok: true, status: 200, json: async () => body });

test('selfFromAttendees: the invite marks your own row', () => {
  assert.equal(selfFromAttendees([ROB, { ...JOEY, self: true }]), JOEY.email);
  assert.equal(selfFromAttendees([ROB, JOEY]), '');
  assert.equal(selfFromAttendees(null), '');
});

test('selfFromCalendarIds: a calendar you own is named by your address', () => {
  assert.equal(
    selfFromCalendarIds(['en.usa#holiday@group.v.calendar.google.com', ME, 'team@example.com']),
    ME,
  );
  // Shared, resource and holiday calendars belong to no person.
  assert.equal(selfFromCalendarIds(['abc123@group.calendar.google.com']), '');
  assert.equal(selfFromCalendarIds([]), '');
});

test('routingContacts: drops you, blanks, and duplicates', () => {
  const attendees = [
    ROB,
    { name: 'Me', email: 'ME@Example.com' }, // you, differently cased
    { name: 'Room', email: '' },
    { name: 'Rob again', email: 'ROB@example.com' }, // same person twice
    JOEY,
  ];
  assert.deepEqual(routingContacts(attendees, ME).map((c) => c.email), [
    ROB.email,
    JOEY.email,
  ]);
});

test('routingContacts: no signed-in address excludes nobody', () => {
  assert.equal(routingContacts([ROB, JOEY], '').length, 2);
});

test('routeUrl: one repeated email parameter per contact', () => {
  assert.equal(
    routeUrl('https://viewer.example//', [ROB, JOEY]),
    'https://viewer.example/people/route?email=rob%40example.com&email=joey%40example.com',
  );
});

test('routeAttendees: passes the answer through', async () => {
  const answer = {
    matches: [{ project: 'agstaff', repo: 'agstaff', folder: '~/w', people: ['rob'], score: 1 }],
    tie: false,
  };
  const f = stubFetch(() => ok(answer));
  try {
    const out = await routeAttendees('https://viewer.example', [ROB, { ...JOEY, self: true }], '');
    assert.deepEqual(out, answer);
    // Joey marked himself as the signed-in row, but only an explicit selfEmail
    // filters here — the caller decides who "you" is.
    assert.match(f.calls[0].url, /email=rob%40example\.com&email=joey%40example\.com$/);
  } finally {
    f.restore();
  }
});

test('routeAttendees: nobody to ask about → no request, no answer', async () => {
  const f = stubFetch(() => { throw new Error('should not fetch'); });
  try {
    assert.equal(await routeAttendees('https://viewer.example', [], ''), null);
    assert.equal(await routeAttendees('https://viewer.example', [{ name: 'Me', email: ME }], ME), null);
    assert.equal(f.calls.length, 0);
  } finally {
    f.restore();
  }
});

test('routeAttendees: 404 before any people push is a state, not a failure', async () => {
  const f = stubFetch(() => ({ ok: false, status: 404, json: async () => ({ error: 'no people file' }) }));
  try {
    // Told apart from a failure on purpose: the line under the repo box has to
    // say "nobody pushed a people file", not "nobody matched".
    assert.deepEqual(await routeAttendees('https://viewer.example', [ROB], ME), {
      matches: [],
      tie: false,
      unavailable: 'no people file',
    });
  } finally {
    f.restore();
  }
});

test('routeAttendees: a dead viewer, a 400, or a junk body is null and never throws', async () => {
  const dead = stubFetch(() => { throw new TypeError('failed to fetch'); });
  try {
    assert.equal(await routeAttendees('https://viewer.example', [ROB], ME), null);
  } finally {
    dead.restore();
  }
  const bad = stubFetch(() => ({ ok: false, status: 400, json: async () => ({ error: 'no contacts' }) }));
  try {
    assert.equal(await routeAttendees('https://viewer.example', [ROB], ME), null);
  } finally {
    bad.restore();
  }
  // Not JSON at all — a proxy's HTML error page. r.json() rejects; nothing escapes.
  const html = stubFetch(() => ({
    ok: true,
    status: 200,
    json: async () => { throw new SyntaxError('Unexpected token < in JSON at position 0'); },
  }));
  try {
    assert.equal(await routeAttendees('https://viewer.example', [ROB], ME), null);
  } finally {
    html.restore();
  }
  // JSON, but not the answer we asked for.
  const wrong = stubFetch(() => ok({ hello: 'world' }));
  try {
    assert.equal(await routeAttendees('https://viewer.example', [ROB], ME), null);
  } finally {
    wrong.restore();
  }
});
