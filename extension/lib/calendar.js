// Google Calendar via chrome.identity — calendar.readonly only.
// The extension does the event lookup (it needs attendees for name prefill)
// and ships the event with the transcript; the server never talks to Google.

async function authToken(interactive = false) {
  return new Promise((resolve, reject) => {
    chrome.identity.getAuthToken({ interactive }, (token) => {
      if (chrome.runtime.lastError || !token) {
        reject(new Error(chrome.runtime.lastError?.message || 'no auth token'));
      } else {
        resolve(token);
      }
    });
  });
}

async function gcal(path, interactive = false) {
  const token = await authToken(interactive);
  const r = await fetch(`https://www.googleapis.com/calendar/v3${path}`, {
    headers: { authorization: `Bearer ${token}` },
  });
  if (!r.ok) throw new Error(`calendar api ${path}: ${r.status} ${await r.text()}`);
  return r.json();
}

/// All calendars on the signed-in account — the options page lists these.
export async function listCalendars(interactive = true) {
  const body = await gcal('/users/me/calendarList', interactive);
  return (body.items || []).map((c) => ({
    id: c.id,
    summary: c.summary,
    primary: !!c.primary,
  }));
}

/// Find the event overlapping [startedAt, endedAt] in the selected calendars.
/// Picks the one with the largest overlap when several qualify.
export async function findOverlappingEvent(calendarIds, startedAt, endedAt) {
  const s = Date.parse(startedAt);
  const e = Date.parse(endedAt);
  let best = null;
  let bestOverlap = 0;
  for (const calId of calendarIds || []) {
    const qs = new URLSearchParams({
      timeMin: new Date(s - 60 * 60000).toISOString(),
      timeMax: new Date(e + 60 * 60000).toISOString(),
      singleEvents: 'true',
      orderBy: 'startTime',
      maxResults: '20',
    });
    let body;
    try {
      body = await gcal(`/calendars/${encodeURIComponent(calId)}/events?${qs}`);
    } catch (err) {
      console.warn(`calendar ${calId} lookup failed`, err);
      continue;
    }
    for (const ev of body.items || []) {
      const evS = Date.parse(ev.start?.dateTime || ev.start?.date || 0);
      const evE = Date.parse(ev.end?.dateTime || ev.end?.date || 0);
      const overlap = Math.min(e, evE) - Math.max(s, evS);
      if (overlap > 0 && overlap > bestOverlap) {
        bestOverlap = overlap;
        best = ev;
      }
    }
  }
  if (!best) return null;
  return {
    id: best.id,
    title: best.summary || null,
    attendees: (best.attendees || []).map((a) => ({
      name: a.displayName || null,
      email: a.email || null,
    })),
  };
}
