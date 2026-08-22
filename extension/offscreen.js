// nashmeet offscreen document — the capture engine.
//
// Two channels, always recorded locally first (reliability pillar #1):
//   tab — chrome.tabCapture stream (everyone remote)
//   mic — getUserMedia (you, and Rob when you share the webcam)
// Each runs through its own MediaRecorder (Opus/webm, 32 kbps) with chunks
// appended to IndexedDB every 5 s, so nothing in this document's lifetime is
// load-bearing for the transcript. The realtime preview is cosmetic: it uses
// Chrome's local Web Speech API (xAI has no streaming STT). The authoritative
// transcript is the post-meeting batch pass against Grok /v1/stt.

import { appendChunk, saveSession } from './lib/recorder-db.js';
import { DEFAULTS } from './lib/config.js';

let active = null; // { controllers: [], streams: [], ctx, sessionId, startedAt, meta }

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg.type === 'nashmeet:offscreen:start') {
    start(msg).then(sendResponse, (e) => sendResponse({ error: e?.message || String(e) }));
    return true;
  }
  if (msg.type === 'nashmeet:offscreen:stop') {
    stop().then(sendResponse, (e) => sendResponse({ error: e?.message || String(e) }));
    return true;
  }
  return false;
});

async function start({ streamId, sessionId, startedAt, meetingUrl, title, config }) {
  if (active) return { error: 'capture already running' };

  const tabStream = await navigator.mediaDevices.getUserMedia({
    audio: {
      mandatory: { chromeMediaSource: 'tab', chromeMediaSourceId: streamId },
    },
  });
  // Re-play tab audio locally — capturing mutes the tab otherwise. The context
  // can be created suspended (no user gesture in an offscreen doc); resume it
  // or the replay graph stays silent and the user can't hear the meeting.
  const ctx = new AudioContext();
  ctx.createMediaStreamSource(tabStream).connect(ctx.destination);
  ctx.resume().catch(() => {});

  let micStream;
  try {
    micStream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: true, noiseSuppression: true },
    });
  } catch (e) {
    // Offscreen documents can't prompt for the mic — so a missing grant lands
    // here. Don't leak the tab stream we already opened, and surface a message
    // that points at the fix instead of a bare "NotAllowedError".
    tabStream.getTracks().forEach((t) => t.stop());
    ctx.close().catch(() => {});
    if (e && (e.name === 'NotAllowedError' || e.name === 'NotFoundError')) {
      throw new Error(
        'Microphone access not granted. Open nashmeet Settings → "Grant microphone access", then start recording again.',
      );
    }
    throw e;
  }

  // Diagnostic: confirm each channel actually has a live, unmuted audio track.
  // An empty transcript with a silent track points here, not at the provider.
  const trackInfo = (s) =>
    s.getAudioTracks().map((t) => ({ label: t.label, muted: t.muted, enabled: t.enabled, state: t.readyState }));
  console.log('nashmeet capture tracks', { tab: trackInfo(tabStream), mic: trackInfo(micStream) });

  // Chunking knobs (defaults unless the popup/options override them). Only
  // meetings past the target rotate at all; shorter ones stay one segment and
  // behave exactly as before.
  const chunk = { ...DEFAULTS, ...(config || {}) };
  const startedAtMs = Date.now();

  const controllers = [
    recordChannel(sessionId, 'tab', tabStream, ctx, chunk, startedAtMs),
    recordChannel(sessionId, 'mic', micStream, ctx, chunk, startedAtMs),
  ];

  // If a capture track ends on its own — the meeting tab is closed/navigated,
  // or the mic is unplugged — finalize THAT channel's recorder right away so
  // its segment flushes a valid webm instead of being truncated when the dead
  // stream is finally torn down. Session-level finalize (mapping handoff) is
  // background.js's job via tabs.onRemoved; here we only protect the audio.
  for (const [name, stream] of [['tab', tabStream], ['mic', micStream]]) {
    stream.getAudioTracks().forEach((t) => {
      t.addEventListener('ended', () => {
        const c = active?.controllers.find((c) => c.channel === name);
        if (c && !c.stopped) stopController(c).catch(() => {});
      });
    });
  }

  await saveSession({
    sessionId,
    startedAt,
    meetingUrl,
    title,
    endedAt: null,
  });

  // The realtime preview lives in the content script (content.js) now — Web
  // Speech runs in the visible meeting page and renders in the pill. The
  // offscreen doc is pure capture.

  active = {
    controllers,
    streams: [tabStream, micStream],
    ctx,
    sessionId,
    startedAt,
    meta: { meetingUrl, title },
  };
  return { ok: true };
}

/// Per-channel capture controller with pause-aligned recorder rotation.
///
/// One MediaRecorder records the current segment; an AnalyserNode polled every
/// 250 ms watches the stream's RMS. Once a segment has run past the target AND
/// silence has held long enough (or the hard max is hit), we rotate: stop the
/// recorder, and on its `onstop` start a fresh one on the SAME stream. The
/// stop→start gap therefore falls inside a pause, so no speech is split and
/// nothing needs de-duplication. Each segment's chunks are tagged with its
/// segmentIndex and offsetMs (start relative to session start) for later
/// stitching. Rotation is best-effort: a poll error or a failed start() is
/// logged and never tears down what's already captured (chunks are already on
/// disk); worst case the channel simply stops rotating and behaves as before.
function recordChannel(sessionId, channel, stream, ctx, chunk, startedAtMs) {
  const analyser = ctx.createAnalyser();
  analyser.fftSize = 2048;
  ctx.createMediaStreamSource(stream).connect(analyser);

  const c = {
    sessionId,
    channel,
    stream,
    chunk,
    startedAtMs,
    analyser,
    data: new Float32Array(analyser.fftSize),
    rec: null,
    segmentIndex: 0,
    segmentOffsetMs: 0,
    silenceSince: null,
    stopped: false,
    poll: null,
  };
  startSegment(c);
  c.poll = setInterval(() => {
    try {
      tick(c);
    } catch (e) {
      // A measurement glitch must never stop the meeting.
      console.error(`nashmeet rotation poll error (${channel})`, e);
    }
  }, 250);
  return c;
}

/// Start a fresh MediaRecorder for the current segment. Its chunks close over
/// this segment's index + offset (fixed for the recorder's lifetime), so the
/// final pre-rotation chunk is still tagged with the old segment.
function startSegment(c) {
  const rec = new MediaRecorder(c.stream, {
    mimeType: 'audio/webm;codecs=opus',
    audioBitsPerSecond: 32_000,
  });
  const segmentIndex = c.segmentIndex;
  const offsetMs = c.segmentOffsetMs;
  rec.ondataavailable = (ev) => {
    if (ev.data && ev.data.size > 0) {
      appendChunk(c.sessionId, c.channel, ev.data, segmentIndex, offsetMs).catch((e) =>
        console.error(`chunk persist failed (${c.channel})`, e),
      );
    }
  };
  rec.start(5_000);
  c.rec = rec;
  c.segmentStartMs = Date.now();
}

/// Root-mean-square amplitude of the analyser's current time-domain frame —
/// our silence proxy. 0 ≈ pure silence; speech sits well above the threshold.
function measureRms(analyser, data) {
  analyser.getFloatTimeDomainData(data);
  let sum = 0;
  for (let i = 0; i < data.length; i++) sum += data[i] * data[i];
  return Math.sqrt(sum / data.length);
}

/// One 250 ms poll: measure RMS, track how long silence has held, rotate when
/// the segment is both long enough and currently in a pause (or hits the cap).
function tick(c) {
  if (c.stopped) return;
  const now = Date.now();
  const elapsed = now - c.segmentStartMs;
  const { segmentSilenceRms, segmentSilenceHoldMs, segmentTargetSeconds, segmentMaxSeconds } =
    c.chunk;

  const silent = measureRms(c.analyser, c.data) < segmentSilenceRms;
  c.silenceSince = silent ? (c.silenceSince ?? now) : null;
  const silenceHeld = c.silenceSince != null && now - c.silenceSince >= segmentSilenceHoldMs;

  const hitMax = elapsed >= segmentMaxSeconds * 1000;
  const pausedPastTarget = elapsed >= segmentTargetSeconds * 1000 && silenceHeld;
  if (hitMax || pausedPastTarget) rotate(c);
}

/// Rotate the recorder: stop the current segment and, once it has flushed,
/// start the next on the same stream. Best-effort — never leaves the channel
/// without a live recorder unless start() itself fails (logged), in which case
/// captured audio is already safe and only future audio on this channel stops.
function rotate(c) {
  const old = c.rec;
  if (!old || old.state === 'inactive') return;
  c.silenceSince = null;
  old.onstop = () => {
    if (c.stopped) return;
    c.segmentIndex += 1;
    c.segmentOffsetMs = Date.now() - c.startedAtMs;
    try {
      startSegment(c);
    } catch (e) {
      console.error(`nashmeet rotation start failed (${c.channel}) — channel stops rotating`, e);
    }
  };
  try {
    old.requestData();
    old.stop();
  } catch (e) {
    console.error(`nashmeet rotation stop failed (${c.channel})`, e);
    old.onstop = null;
  }
}

/// Tear down one channel's recorder. Resolves on the recorder's `onstop` OR a
/// short watchdog timeout — `onstop` is not reliable across MediaRecorder edge
/// states, and a missed event must never wedge the stop response (which would
/// freeze the in-page pill on "stopping" and the badge on "REC"). The tail is
/// flushed with requestData() before stopping; chunks are persisted
/// independently of this document's teardown, so resolving early is safe.
function stopController(c) {
  return new Promise((resolve) => {
    c.stopped = true;
    if (c.poll) clearInterval(c.poll);
    const rec = c.rec;
    if (!rec || rec.state === 'inactive') return resolve();
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      resolve();
    };
    rec.onstop = finish;
    setTimeout(finish, 1500);
    try {
      rec.requestData();
      rec.stop();
    } catch {
      finish();
    }
  });
}

async function stop() {
  if (!active) return { error: 'not capturing' };
  const { controllers, streams, ctx, sessionId, startedAt, meta } = active;
  active = null;

  await Promise.all(controllers.map((c) => stopController(c)));
  streams.forEach((s) => s.getTracks().forEach((t) => t.stop()));
  ctx?.close().catch(() => {});

  await saveSession({
    sessionId,
    startedAt,
    endedAt: new Date().toISOString(),
    ...meta,
  });
  return { ok: true, sessionId };
}
