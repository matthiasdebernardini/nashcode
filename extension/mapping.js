// The mapping screen — runs after every meeting.
//
// 1. Pull the session's two channel recordings out of IndexedDB (ground truth).
// 2. Batch-transcribe each channel through the provider fallback chain.
// 3. Look up the overlapping calendar event → prefill candidate names;
//    mic-channel speakers precheck as the configured You/Rob defaults.
// 4. User confirms (ten seconds) or skips → POST the transcript to the
//    nashcode viewer, which commits it as markdown; then clean local storage.

import { getConfig } from './lib/config.js';
import { getSession, channelSegments, deleteSession } from './lib/recorder-db.js';
import { transcribeSegmented } from './lib/transcribe.js';
import { findOverlappingEvent } from './lib/calendar.js';
import {
  mergeChannels,
  prefillSpeakers,
  longestUtterances,
  buildPayload,
  parsePeople,
  mergeNames,
} from './lib/mapping-core.js';

const phase = (t, isErr = false) => {
  const el = document.getElementById('phase');
  el.textContent = t;
  el.className = isErr ? 'error' : '';
};

const state = {};

// Tell the service worker the Grok batch pass is done so it can clear the
// "transcribing…" pill still up on the meeting tab. Best-effort: that tab may
// already be closed, and this runs whether transcription succeeded or came up
// empty — either way the pass is over.
function pillTranscribed(tabId) {
  if (tabId == null || Number.isNaN(tabId)) return;
  chrome.runtime.sendMessage({ type: 'nashmeet:mapping:transcribed', tabId }).catch(() => {});
}

async function main() {
  const params = new URLSearchParams(location.search);
  const sessionId = params.get('session');
  const pillTabParam = params.get('pilltab');
  const pillTabId = pillTabParam != null ? Number(pillTabParam) : null;
  const cfg = await getConfig();
  const session = await getSession(sessionId);
  if (!session) return phase(`session ${sessionId} not found in local storage`, true);
  state.cfg = cfg;
  state.session = session;
  state.segments = {};

  // Transcribe both channels from the local recordings. Long meetings rotate
  // into several segments per channel; channelSegments returns them ordered
  // with offsets, and transcribeSegmented runs each through the fallback chain
  // and stitches them onto one timeline (a short meeting is just one segment).
  // A single empty/failed channel must NOT kill the session: a solo recording
  // (nobody else on the call) legitimately yields an empty *tab* channel while
  // your *mic* channel still holds everything you said — and vice-versa. So we
  // collect whatever transcribes and only fail if EVERY channel came up empty.
  const channels = [];
  const channelErrors = [];
  for (const channel of ['tab', 'mic']) {
    const segs = await channelSegments(sessionId, channel);
    state.segments[channel] = segs;
    if (!segs.length) continue;
    phase(`transcribing ${channel} channel…`);
    try {
      const { provider, segments } = await transcribeSegmented(
        cfg,
        segs,
        undefined,
        (name) => phase(`transcribing ${channel} channel via ${name}…`),
      );
      state.provider = provider;
      channels.push({ channel, segments });
    } catch (e) {
      // Note it and move on — the all-empty case is handled below.
      console.warn(`${channel} channel transcription failed`, e);
      channelErrors.push(`${channel}: ${e.message || e}`);
    }
  }
  if (!channels.length) {
    pillTranscribed(pillTabId);
    return phase(
      `no usable audio in any channel — ${channelErrors.join('; ') || 'nothing recorded'}`,
      true,
    );
  }
  // Transcription is complete — release the meeting-tab pill before the
  // (best-effort) calendar lookup and the human naming step.
  pillTranscribed(pillTabId);

  state.merged = mergeChannels(channels);

  // Calendar lookup — best-effort; no event just means free-text entry.
  phase('looking up calendar event…');
  state.calendarEvent = null;
  try {
    state.calendarEvent = await findOverlappingEvent(
      cfg.calendarIds,
      session.startedAt,
      session.endedAt,
    );
  } catch (e) {
    console.warn('calendar lookup failed', e);
  }

  render();
}

function render() {
  const { merged, calendarEvent, cfg } = state;
  renderContext();
  const cards = prefillSpeakers(merged.speakers, calendarEvent?.attendees || [], cfg.micDefaults);
  const cardsEl = document.getElementById('cards');
  cardsEl.innerHTML = '';

  for (const card of cards) {
    const div = document.createElement('div');
    div.className = 'card';
    const h = document.createElement('h3');
    h.innerHTML = `${card.id} <span class="chip ${card.channel}">${card.channel === 'mic' ? 'your mic' : 'remote'}</span>`;
    div.appendChild(h);

    for (const utt of longestUtterances(merged.segments, card.id)) {
      const u = document.createElement('div');
      u.className = 'utt';
      const play = document.createElement('button');
      play.textContent = '▶';
      play.addEventListener('click', () => playSnippet(card.channel, utt));
      u.appendChild(play);
      const span = document.createElement('span');
      span.textContent = `“${utt.text.slice(0, 160)}”`;
      u.appendChild(span);
      div.appendChild(u);
    }

    const input = document.createElement('input');
    input.type = 'text';
    input.placeholder = 'name…';
    input.dataset.speaker = card.id;
    if (card.prefill) input.value = card.prefill;
    // Re-typing a name anywhere updates the shared chip pool, so naming the
    // first fragment of a reconnect split surfaces it as a one-tap chip on the
    // rest (collapseByAssignedName then folds them into one person on file).
    input.addEventListener('input', refreshChips);
    div.appendChild(input);

    // One-tap chips of every known name (mic defaults + calendar attendees +
    // the context "who was on it" field + whatever's been typed). Redrawn live.
    const chips = document.createElement('div');
    chips.className = 'chips';
    chips._input = input;
    div.appendChild(chips);

    cardsEl.appendChild(div);
  }

  refreshChips();
  phase(
    calendarEvent
      ? calendarEvent.title
        ? `matched calendar event “${calendarEvent.title}” — confirm the mapping below`
        : 'matched a calendar event (no title) — add a title and confirm below'
      : 'no calendar event found — add a title and the names below (or skip)',
  );
  document.getElementById('actions').style.display = 'flex';
}

// The "what was this meeting?" box — only when no calendar event supplied a
// title and attendees. Title becomes the filed transcript's title; the people
// field seeds the shared name pool.
function renderContext() {
  const el = document.getElementById('context');
  el.innerHTML = '';
  // Show the title/people box when there's no event OR the matched event has no
  // title — otherwise a titleless event leaves nowhere to name the transcript.
  if (state.calendarEvent?.title) return;
  const card = document.createElement('div');
  card.className = 'card context';
  card.innerHTML =
    '<h3>What was this meeting?</h3>' +
    '<label for="title-input">Title / topic</label>' +
    '<input type="text" id="title-input" placeholder="e.g. Bob — paddleboarding 1:1" />' +
    '<p class="hint">Names the filed transcript (no calendar event was found).</p>' +
    '<label for="people-input">Who was on the call?</label>' +
    '<input type="text" id="people-input" placeholder="comma-separated, e.g. Bob, Alice" />' +
    '<p class="hint">Becomes one-tap name chips on every speaker below.</p>';
  el.appendChild(card);
  document.getElementById('people-input').addEventListener('input', refreshChips);
}

// The live pool of candidate names, recomputed from current DOM each time so a
// freshly-typed name immediately becomes a chip everywhere else.
function knownNames() {
  const { calendarEvent, cfg } = state;
  const attendees = (calendarEvent?.attendees || []).map((a) => a.name || a.email).filter(Boolean);
  const people = parsePeople(document.getElementById('people-input')?.value || '');
  const typed = [...document.querySelectorAll('input[data-speaker]')].map((i) => i.value);
  return mergeNames(cfg.micDefaults || [], attendees, people, typed);
}

// Redraw every card's chip row from the current pool, hiding the chip that
// equals the card's own current value (nothing to re-pick).
function refreshChips() {
  const names = knownNames();
  for (const container of document.querySelectorAll('.chips')) {
    const input = container._input;
    const current = input.value.trim().toLowerCase();
    container.innerHTML = '';
    for (const name of names) {
      if (name.toLowerCase() === current) continue;
      const a = document.createElement('span');
      a.className = 'cand';
      a.textContent = name;
      a.addEventListener('click', () => {
        input.value = name;
        refreshChips();
      });
      container.appendChild(a);
    }
  }
}

// Play the utterance's time range. Utterance times are meeting-absolute, but
// each segment is its own webm decoded independently (the concatenation isn't a
// valid single webm), so we find the segment that owns the time and play at the
// local offset. Each segment's decoded buffer is cached on first use.
const audioCtx = new AudioContext();
const decoded = new Map(); // `${channel}:${segmentIndex}` → AudioBuffer
async function playSnippet(channel, utt) {
  // The context was created at load (no gesture) so it may be suspended;
  // resume on this click or src.start() produces no sound.
  if (audioCtx.state === 'suspended') await audioCtx.resume().catch(() => {});
  const segs = state.segments[channel];
  if (!segs || !segs.length) return;
  // The owning segment is the last one whose offset is at or before the time.
  let seg = segs[0];
  for (const s of segs) {
    if (s.offsetMs <= utt.start_ms) seg = s;
    else break;
  }
  const key = `${channel}:${seg.segmentIndex}`;
  if (!decoded.has(key)) {
    decoded.set(key, await audioCtx.decodeAudioData(await seg.blob.arrayBuffer()));
  }
  const src = audioCtx.createBufferSource();
  src.buffer = decoded.get(key);
  src.connect(audioCtx.destination);
  const start = Math.max(0, (utt.start_ms - seg.offsetMs) / 1000);
  const dur = Math.min((utt.end_ms - utt.start_ms) / 1000, 8);
  src.start(0, start, dur);
}

function collectAssignments() {
  const out = {};
  for (const input of document.querySelectorAll('input[data-speaker]')) {
    out[input.dataset.speaker] = input.value;
  }
  return out;
}

async function file(assignments) {
  const { cfg, session, merged, calendarEvent, provider } = state;
  const manualTitle = document.getElementById('title-input')?.value || '';
  const payload = buildPayload({
    session,
    merged,
    assignments,
    calendarEvent,
    provider,
    manualTitle,
  });

  if (!cfg.repo) {
    phase('no nashcode repo set — open nashmeet Settings and pick one, then reload', true);
    return;
  }

  try {
    phase('filing transcript…');
    const r = await fetch(`${cfg.viewerBase}/${encodeURIComponent(cfg.repo)}/transcripts`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(payload),
    });
    if (!r.ok) {
      phase(`filing failed: ${r.status} ${await r.text()} — recording is safe locally, reload to retry`, true);
      return;
    }
    const filed = await r.json();

    // The transcript is committed; nashcode stores no audio, so the local
    // recording has done its job. Delete only after this 2xx — a failed filing
    // must never destroy the only copy of a meeting.
    await deleteSession(state.session.sessionId);

    phase('done');
    document.getElementById('actions').style.display = 'none';
    const result = document.getElementById('result');
    result.textContent = `Filed: ${filed.path} (commit ${String(filed.commit || '').slice(0, 7)})`;
    const link = document.createElement('a');
    link.href = `${cfg.viewerBase}/${encodeURIComponent(cfg.repo)}`;
    link.target = '_blank';
    link.textContent = `open ${cfg.repo} in nashcode`;
    result.appendChild(document.createElement('br'));
    result.appendChild(link);
  } catch (e) {
    // A network-level rejection (TLS drop, tailnet hiccup, box down) must show
    // the same non-destructive recovery message, not a hung screen.
    phase(`upload interrupted: ${e?.message || e} — recording is safe locally, reload to retry`, true);
  }
}

document.getElementById('confirm').addEventListener('click', () => file(collectAssignments()));
document.getElementById('skip').addEventListener('click', () => file({}));

main();
