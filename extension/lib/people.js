// Which project does this meeting belong to? The operator's people file joins
// people to projects; the viewer holds a pushed copy and answers that one
// question. It never hands the file back out, so we send the invite's emails
// and get project ids, repo names, and which of our contacts scored — never
// anyone else's contact details.
//
// Every failure here is silent by design. Routing only improves on the repo the
// settings already chose, so a people file nobody pushed yet and a viewer that
// is down both leave the default standing. They are told apart, though: the
// line under the repo box has to say which happened.

import { viewerRoot } from './config.js';

/// The attendee whose address is the signed-in account's. Google marks it on
/// events read from the user's own calendars, which is the only kind nashmeet
/// looks at, so this is usually the exact answer. Pure.
export function selfFromAttendees(attendees) {
  const me = (attendees || []).find((a) => a?.self && a?.email);
  return me ? me.email : '';
}

/// Fallback for an event that carries no `self` row: the id of a Google
/// calendar you own IS your address. Generated calendars — shared, resources,
/// holidays, birthdays — live under `*.calendar.google.com` and belong to no
/// person, so they are skipped. An empty answer excludes nobody, which costs at
/// most one wrong point in the ranking. Pure.
export function selfFromCalendarIds(calendarIds) {
  const own = (calendarIds || []).find(
    (id) => typeof id === 'string' && id.includes('@') && !/\.calendar\.google\.com$/i.test(id),
  );
  return own || '';
}

/// The attendees worth asking about: everyone on the invite except you.
/// Emails compare case-insensitively — the people file's rule — and an
/// attendee with no address cannot be matched, so it goes too. Pure.
export function routingContacts(attendees, selfEmail) {
  const self = (selfEmail || '').trim().toLowerCase();
  const seen = new Set();
  const out = [];
  for (const a of attendees || []) {
    const key = (a?.email || '').trim().toLowerCase();
    if (!key || key === self || seen.has(key)) continue;
    seen.add(key);
    out.push(a);
  }
  return out;
}

/// `GET /people/route?email=a&email=b` — `email` repeats, one per contact.
/// Pure.
export function routeUrl(viewerBase, contacts) {
  const qs = new URLSearchParams();
  for (const c of contacts) qs.append('email', c.email);
  return `${viewerRoot(viewerBase)}/people/route?${qs}`;
}

/// Ask the viewer which project these people belong to.
///
/// Answers `{matches, tie}`; each match carries `contacts`, the addresses we
/// sent that scored for it, so the caller can name them. Two other answers:
///   - `{matches: [], tie: false, unavailable: 'no people file'}` on 404 — the
///     operator has not pushed a file yet, which is a state, not a failure.
///   - null when there was nothing to ask (no contacts) or the question could
///     not be put at all (network, 400, a body that is not JSON).
/// Never throws.
export async function routeAttendees(viewerBase, attendees, selfEmail) {
  const contacts = routingContacts(attendees, selfEmail);
  if (!contacts.length) return null;
  try {
    const r = await fetch(routeUrl(viewerBase, contacts), { cache: 'no-store' });
    if (r.status === 404) return { matches: [], tie: false, unavailable: 'no people file' };
    if (!r.ok) return null;
    const body = await r.json();
    if (!body || !Array.isArray(body.matches)) return null;
    return { matches: body.matches, tie: !!body.tie };
  } catch (e) {
    console.warn('people route failed', e);
    return null;
  }
}
