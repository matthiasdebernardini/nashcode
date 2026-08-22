// nashmeet service worker — session control.
//
// Toolbar/popup "start" on a meeting tab:
//   1. mint a tabCapture stream id for that tab
//   2. spin up the offscreen document (audio capture must outlive the popup)
//   3. inject the always-visible recording pill into the page
//   4. badge the toolbar icon red with elapsed minutes
// "stop" tears that down and opens the mapping screen for the finished session.

import { recIdFromDate } from './lib/mapping-core.js';
import { getConfig } from './lib/config.js';

const OFFSCREEN_URL = 'offscreen.html';

// The control session (which tab is recording, its id + start time) is the one
// piece of state that MUST outlive service-worker eviction: Chrome tears an
// idle MV3 worker down mid-meeting, but the offscreen document keeps recording.
// Held in a module global it would read back null on the next wake, so "stop"
// would no-op while audio kept flowing and the pill froze on "… stopping"
// forever. chrome.storage.session survives worker restarts (cleared only on
// browser close), so a stop click after eviction still finds the session.
const CTL_KEY = 'ctl';
const loadCtl = async () => (await chrome.storage.session.get(CTL_KEY))[CTL_KEY] ?? null;
const saveCtl = (ctl) => chrome.storage.session.set({ [CTL_KEY]: ctl });
const clearCtl = () => chrome.storage.session.remove(CTL_KEY);

// Reject if a promise outruns `ms` — so a wedged or absent offscreen stop can
// never hang the whole teardown (which is what left the badge on REC and the
// pill pinned on "… stopping").
function withTimeout(promise, ms, label) {
  return Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`${label} timed out`)), ms)),
  ]);
}

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  (async () => {
    try {
      switch (msg.type) {
        case 'nashmeet:start':
          sendResponse(await startSession(msg.tabId));
          break;
        case 'nashmeet:stop':
          sendResponse(await stopSession());
          break;
        case 'nashmeet:status':
          sendResponse(await sessionStatus());
          break;
        case 'nashmeet:mapping:transcribed':
          // The mapping screen finished the Grok batch pass — clear the in-page
          // pill on the meeting tab it told us about.
          if (msg.tabId != null) removePill(msg.tabId).catch(() => {});
          sendResponse({ ok: true });
          break;
        default:
          sendResponse(null);
      }
    } catch (e) {
      // Any throw (e.g. tabCapture refused on a chrome:// tab) must still
      // resolve the channel with an error, or the popup hangs on "Starting…".
      sendResponse({ error: e?.message || String(e) });
    }
  })();
  return true; // async response
});

async function sessionStatus() {
  const ctl = await loadCtl();
  if (!ctl) return { recording: false };
  return {
    recording: true,
    startedAt: ctl.startedAt,
    tabId: ctl.tabId,
    title: ctl.title,
  };
}

async function startSession(tabId) {
  if (await loadCtl()) return { error: 'already recording' };
  // chrome.tabs.get REJECTS for a missing id (it never resolves falsy), so the
  // lookup must be guarded or the throw escapes unhandled and the popup wedges.
  let tab;
  try {
    tab = await chrome.tabs.get(tabId);
  } catch {
    return { error: 'no such tab' };
  }

  const streamId = await chrome.tabCapture.getMediaStreamId({
    targetTabId: tabId,
  });

  await ensureOffscreen();
  const startedAt = new Date().toISOString();
  const sessionId = recIdFromDate(new Date(), tab.title || 'meeting');
  // Read config here in the service worker and hand it to the offscreen doc:
  // offscreen documents don't expose chrome.storage, so they can't call
  // getConfig() themselves.
  const config = await getConfig();
  const reply = await chrome.runtime.sendMessage({
    type: 'nashmeet:offscreen:start',
    streamId,
    sessionId,
    startedAt,
    meetingUrl: tab.url,
    title: tab.title,
    config,
  });
  if (reply?.error) return reply;

  await saveCtl({ tabId, startedAt, sessionId, title: tab.title, url: tab.url });
  await injectPill(tabId, config.livePreview !== false);
  startBadge(startedAt);
  return { ok: true, sessionId };
}

async function stopSession() {
  const ctl = await loadCtl();
  // The offscreen document is the source of truth for what's actually
  // recording and it survives worker eviction, so always ask it to stop and
  // trust the session id it hands back, even if our ctl state was lost.
  let stoppedId = ctl?.sessionId ?? null;
  let stopErr = null;
  try {
    const reply = await withTimeout(
      chrome.runtime.sendMessage({ type: 'nashmeet:offscreen:stop' }),
      4000,
      'offscreen stop',
    );
    if (reply?.sessionId) stoppedId = reply.sessionId;
    if (reply?.error && reply.error !== 'not capturing') stopErr = reply.error;
  } catch (e) {
    // A wedged/absent offscreen doc must still let us tear the UI down — the
    // recordings are already persisted in IndexedDB regardless.
    stopErr = e?.message || String(e);
  }

  stopBadge();
  await clearCtl();

  if (!stoppedId) {
    if (ctl?.tabId != null) removePill(ctl.tabId).catch(() => {});
    return stopErr ? { error: stopErr } : { ok: true };
  }

  // Recording stopped. Leave the pill up but flip it to "transcribing" so the
  // long Grok pass is legible, and hand off to the mapping screen that runs it
  // (batch pass + naming + POST).
  if (ctl?.tabId != null) setPillTranscribing(ctl.tabId).catch(() => {});
  await chrome.tabs.create({
    url: chrome.runtime.getURL(
      `mapping.html?session=${encodeURIComponent(stoppedId)}` +
        (ctl?.tabId != null ? `&pilltab=${encodeURIComponent(ctl.tabId)}` : ''),
    ),
  });
  return { ok: true, sessionId: stoppedId };
}

async function ensureOffscreen() {
  const has = await chrome.offscreen.hasDocument();
  if (has) return;
  await chrome.offscreen.createDocument({
    url: OFFSCREEN_URL,
    reasons: ['USER_MEDIA'],
    justification:
      'Capture meeting tab audio and microphone for transcription; the recording must outlive the popup.',
  });
}

// --- Always-visible recording indicator -----------------------------------
// nashmeet never records silently: a red toolbar badge with elapsed minutes AND
// an in-page "● nashmeet recording" pill (the pill is also click-to-stop).

async function injectPill(tabId, livePreview) {
  try {
    await chrome.scripting.executeScript({
      target: { tabId },
      files: ['content.js'],
    });
    await chrome.tabs.sendMessage(tabId, { type: 'nashmeet:pill:show', livePreview });
  } catch (e) {
    // Page may forbid injection (chrome:// etc.) — badge still shows.
    console.warn('pill injection failed', e);
  }
}

async function removePill(tabId) {
  await chrome.tabs.sendMessage(tabId, { type: 'nashmeet:pill:hide' });
}

// Flip the live pill from "recording" to "transcribing" — the meeting is fully
// captured; the Grok batch pass is now running in the mapping tab.
async function setPillTranscribing(tabId) {
  await chrome.tabs.sendMessage(tabId, { type: 'nashmeet:pill:transcribing' });
}

// The elapsed-minutes badge is driven by a chrome.alarm, not setInterval: a SW
// timer dies with the worker (which Chrome evicts mid-meeting), freezing the
// badge. An alarm re-wakes the worker, so the badge stays live and always
// reflects the real recording state read back from storage.
const BADGE_ALARM = 'nashmeet-badge';

function badgeText(startedAt) {
  const mins = Math.floor((Date.now() - Date.parse(startedAt)) / 60000);
  return mins > 0 ? `${mins}m` : 'REC';
}

function startBadge(startedAt) {
  chrome.action.setBadgeBackgroundColor({ color: '#cc0000' });
  chrome.action.setBadgeText({ text: badgeText(startedAt) });
  chrome.alarms.create(BADGE_ALARM, { periodInMinutes: 1 });
}

function stopBadge() {
  chrome.alarms.clear(BADGE_ALARM);
  chrome.action.setBadgeText({ text: '' });
}

chrome.alarms.onAlarm.addListener(async (alarm) => {
  if (alarm.name !== BADGE_ALARM) return;
  const ctl = await loadCtl();
  if (ctl) chrome.action.setBadgeText({ text: badgeText(ctl.startedAt) });
  else stopBadge();
});

// Content-script pill clicked "stop".
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg.type === 'nashmeet:pill:stop-clicked') {
    stopSession().then(sendResponse);
    return true;
  }
  return false;
});

// Order-independent finalize: if the user ends the meeting by simply closing
// its tab (instead of clicking Stop), treat that as a stop. Otherwise the tab
// stream dies under a live recorder — the channel truncates to a header-only
// blob (the "4KB · text:\"\"" empty-transcript case) and the session stays
// pinned on REC until a manual stop. Closing the call now flushes a complete
// recording and opens the mapping pass exactly as Stop would. Guarded to the
// recording tab so unrelated tab closes are ignored. (Only onRemoved, not
// navigation — a mid-call redirect must not be mistaken for "meeting over".)
chrome.tabs.onRemoved.addListener(async (tabId) => {
  const ctl = await loadCtl();
  if (ctl?.tabId === tabId) await stopSession();
});

// First run: open the settings page so a new user lands on the setup checklist
// (tailnet reachable · grant mic · sign in · your name) instead of a bare
// toolbar icon. Only on install — an update must not nag.
chrome.runtime.onInstalled.addListener((details) => {
  if (details.reason === 'install') {
    chrome.tabs.create({ url: chrome.runtime.getURL('options.html?firstrun=1') });
  }
});
