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
