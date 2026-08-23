// Pure mapping logic — no chrome.*, no DOM — so it runs identically in the
// mapping screen and under node:test. This is where the standard speaker-
// naming flow lives: normalize per-channel diarization, merge channels,
// prefill names from the calendar event, precheck mic-channel speakers as the
// You/Rob defaults (the shared-webcam case), pick snippet utterances.

/// `2026-06-12-1500-weekly-sync` — must mirror the server's transcript_id()
/// so the recording upload keys match what filing produced.
export function recIdFromDate(date, title) {
  const p = (n, w = 2) => String(n).padStart(w, '0');
  const stamp = `${date.getUTCFullYear()}-${p(date.getUTCMonth() + 1)}-${p(
    date.getUTCDate(),
  )}-${p(date.getUTCHours())}${p(date.getUTCMinutes())}`;
  return `${stamp}-${slugify(title)}`;
}

export function slugify(title) {
  const slug = (title || '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return slug ? slug.slice(0, 60).replace(/-+$/g, '') : 'meeting';
}

/// Merge per-channel normalized transcripts into one timeline.
/// Each input: { channel: 'tab'|'mic', segments: [{speaker, start_ms, end_ms, text}] }
/// where `speaker` is the provider's per-channel label (0, 1, "spk_0", …).
/// Output speakers get stable global ids: S1, S2, … (tab first, then mic),
/// each tagged with its channel.
export function mergeChannels(channels) {
  const speakers = [];
  const segments = [];
  for (const { channel, segments: segs } of channels) {
    const localIds = new Map();
    for (const seg of segs) {
      const key = String(seg.speaker);
      if (!localIds.has(key)) {
        const id = `S${speakers.length + 1}`;
        localIds.set(key, id);
        speakers.push({ id, channel });
      }
      segments.push({
        speaker: localIds.get(key),
        start_ms: seg.start_ms,
        end_ms: seg.end_ms,
        text: seg.text,
      });
    }
  }
  segments.sort((a, b) => a.start_ms - b.start_ms);
  return { speakers, segments };
}

/// Prefill the mapping screen.
///   speakers   — from mergeChannels
///   attendees  — calendar event attendees [{name, email}] (may be empty)
///   micDefaults— configured names for mic-channel speakers, in order
/// Returns one card per speaker:
///   { id, channel, prefill: string|null, prechecked: bool, candidates: [names] }
/// Mic-channel speakers precheck as You/Rob (your stated default); tab
/// speakers offer the remaining attendees as one-tap candidates and prefill
/// only when exactly one candidate is left (no guessing among several).
export function prefillSpeakers(speakers, attendees, micDefaults) {
  const attendeeNames = (attendees || [])
    .map((a) => a.name || a.email)
    .filter(Boolean);
  // Attendees that plausibly ARE the mic side shouldn't be offered for tab
  // speakers; match by case-insensitive first-name inclusion.
  const isMicDefault = (name) =>
    (micDefaults || []).some(
      (d) =>
        name.toLowerCase().includes(d.toLowerCase()) ||
        d.toLowerCase().includes(name.toLowerCase().split('@')[0]),
    );
  const remoteCandidates = attendeeNames.filter((n) => !isMicDefault(n));

  let micIdx = 0;
  return speakers.map((s) => {
    if (s.channel === 'mic') {
      const name = (micDefaults || [])[micIdx] ?? null;
      micIdx += 1;
      return {
        id: s.id,
        channel: s.channel,
        prefill: name,
        prechecked: name != null,
        candidates: micDefaults || [],
      };
    }
    const tabSpeakerCount = speakers.filter((x) => x.channel === 'tab').length;
    const prefill =
      remoteCandidates.length === 1 && tabSpeakerCount === 1
        ? remoteCandidates[0]
        : null;
    return {
      id: s.id,
      channel: s.channel,
      prefill,
      prechecked: prefill != null,
      candidates: remoteCandidates,
    };
  });
}

/// Parse a free-text "who was on the call?" field into clean names.
/// Splits on commas and newlines (the two ways a human lists people), trims,
/// drops empties. Used when there's no calendar event to seed the candidate
/// pool. Pure.
export function parsePeople(text) {
  return (text || '')
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/// Union several name lists into one, case-insensitively deduped, first
/// spelling wins, order preserved. This is the "known names" pool the mapping
/// screen offers as one-tap chips on every card (mic defaults + calendar
/// attendees + the people field + whatever you've already typed). Pure.
export function mergeNames(...lists) {
  const seen = new Set();
  const out = [];
  for (const list of lists) {
    for (const raw of list || []) {
      const name = (raw || '').trim();
      if (!name) continue;
      const key = name.toLowerCase();
      if (seen.has(key)) continue;
      seen.add(key);
      out.push(name);
    }
  }
  return out;
}

/// The N longest utterances per speaker — the mapping screen shows these as
/// text + playable snippets so "who is Speaker 2" takes seconds.
export function longestUtterances(segments, speakerId, n = 3) {
  return segments
    .filter((s) => s.speaker === speakerId)
    .sort((a, b) => (b.end_ms - b.start_ms) - (a.end_ms - a.start_ms))
    .slice(0, n);
}

/// Collapse speakers the human gave the same name into one logical speaker.
///
/// Long meetings chunk, and each chunk is diarized independently, so the SAME
/// physical person can surface as several global ids (S1 on tab, S5 on a later
/// mic segment, …) — extra cards on the mapping screen. Once the human assigns
/// names, those cards carry the cross-chunk identity: same (trimmed,
/// case-insensitive) name ⇒ same person. We merge them into the first id that
/// bore the name and repoint every segment, so the shipped transcript shows one
/// "Matthias" spanning the meeting instead of fragments. Speakers with no name
/// pass through untouched (the unconfirmed/skip case). Pure; server unchanged.
export function collapseByAssignedName(speakers, segments, assignments) {
  const canonicalByName = new Map(); // lowercased name → canonical speaker id
  const remap = new Map(); // old id → surviving id
  const kept = [];
  for (const s of speakers) {
    const name = assignments[s.id]?.trim();
    if (!name) {
      remap.set(s.id, s.id);
      kept.push(s);
      continue;
    }
    const key = name.toLowerCase();
    if (canonicalByName.has(key)) {
      remap.set(s.id, canonicalByName.get(key));
    } else {
      canonicalByName.set(key, s.id);
      remap.set(s.id, s.id);
      kept.push(s);
    }
  }
  const repointed = segments.map((seg) => ({
    ...seg,
    speaker: remap.get(seg.speaker) ?? seg.speaker,
  }));
  return { speakers: kept, segments: repointed };
}

/// Assemble the final POST payload for the nashmeet service.
/// `manualTitle` is the topic the user typed in the no-calendar context box;
/// it fills the title when there's no calendar event to supply one.
export function buildPayload({ session, merged, assignments, calendarEvent, provider, manualTitle }) {
  // Unify same-named speakers (the cross-chunk identity step) before shipping.
  const collapsed = collapseByAssignedName(merged.speakers, merged.segments, assignments);
  const confirmed =
    collapsed.speakers.length > 0 &&
    collapsed.speakers.every((s) => assignments[s.id]?.trim());
  return {
    title: calendarEvent?.title || manualTitle?.trim() || session.title || '',
    started_at: session.startedAt,
    ended_at: session.endedAt,
    meeting_url: session.meetingUrl || null,
    provider: provider || null,
    speakers: collapsed.speakers.map((s) => ({
      id: s.id,
      name: assignments[s.id]?.trim() || null,
      channel: s.channel,
    })),
    speakers_confirmed: confirmed,
    calendar_event: calendarEvent
      ? {
          id: calendarEvent.id,
          title: calendarEvent.title || null,
          attendees: (calendarEvent.attendees || []).map((a) => ({
            name: a.name || null,
            email: a.email || null,
          })),
        }
      : null,
    segments: collapsed.segments,
  };
}

/// How to call a person on the reason line: the invite's display name, or the
/// address when the invite carried no name. Pure.
export function attendeeLabel(attendee) {
  return (attendee?.name || '').trim() || (attendee?.email || '').trim();
}

/// The people who actually put this project on top. The route answer says which
/// of the contacts we sent scored, lowercased and stripped to addresses; the
/// invite says what those people are called. Contacts matched by phone carry no
/// email and cannot be named from a calendar invite, so they drop. Empty means
/// the viewer did not say (an older one) — the caller then counts instead of
/// naming. Pure.
export function matchedNames(match, attendees) {
  const labelByEmail = new Map();
  for (const a of attendees || []) {
    const email = (a?.email || '').trim().toLowerCase();
    if (email && !labelByEmail.has(email)) labelByEmail.set(email, attendeeLabel(a));
  }
  const out = [];
  for (const c of match?.contacts || []) {
    const label = labelByEmail.get((c?.email || '').trim().toLowerCase());
    if (label && !out.includes(label)) out.push(label);
  }
  return out;
}

/// `a`, `a or b`, `a, b or c` — one list, read aloud. Pure.
function orList(items) {
  if (items.length < 2) return items.join('');
  return `${items.slice(0, -1).join(', ')} or ${items[items.length - 1]}`;
}

/// Which repo this meeting files into, and the one line under the box that says
/// why.
///
///   routing     — `GET /people/route`'s answer, `{matches, tie}`, or its
///                 `unavailable` form, or null when nobody could be asked.
///   attendees   — the contacts that were sent (the invite minus you). The
///                 route answer names matched contacts by address; the names
///                 come from here.
///   defaultRepo — the settings repo, the fallback all the way through.
///   hasEvent    — was there a calendar event at all. An event whose attendees
///                 all filtered out is still an event, and reads as no match.
///
/// Returns `{repo, reason, offered}`. `offered` is the tied repos, to put at the
/// top of the datalist; it is empty in every other case. Pure.
export function chooseRepo({ routing, attendees, defaultRepo, hasEvent }) {
  const fallback = (defaultRepo || '').trim();
  const contacts = attendees || [];
  // Every dead end lands the same way: keep the settings repo, say which dead
  // end it was. Only the wording changes, so only the wording is passed in.
  const fall = (why) => ({
    repo: fallback,
    reason: fallback ? `${why}; using the default repo` : `${why}; pick a repo`,
    offered: [],
  });

  if (!hasEvent) return fall('no calendar event');
  if (!contacts.length) return fall('no match on the invite');
  if (!routing) return fall('routing could not be asked');
  if (routing.unavailable) return fall('no people file pushed yet');

  const matches = routing.matches || [];
  if (!matches.length) return fall('no match on the invite');

  if (routing.tie) {
    // Equal top scores keep file order, so the tied projects are the run of
    // matches sharing the first one's score. A tied project with no nashcode
    // repo stays in the sentence — it is one of the answers — but there is
    // nothing to offer for it.
    const tied = matches.filter((m) => m.score === matches[0].score);
    const labels = tied.map((m) => (m.repo ? m.repo : `${m.project} (no nashcode repo)`));
    return {
      repo: '',
      reason: `tie: ${orList(labels)}`,
      offered: tied.map((m) => m.repo).filter(Boolean),
    };
  }

  const top = matches[0];
  // A project can be GitHub-only. There is then nowhere to file, and saying so
  // beats silently filing the meeting somewhere else.
  if (!top.repo) {
    return { repo: '', reason: `${top.project} has no nashcode repo; pick one`, offered: [] };
  }
  const names = matchedNames(top, contacts);
  return {
    repo: top.repo,
    reason: names.length
      ? `${top.repo} — ${names.join(', ')} ${names.length === 1 ? 'is' : 'are'} on the invite`
      : `${top.repo} — ${top.score} of ${contacts.length} on the invite match`,
    offered: [],
  };
}
