import { test } from 'node:test';
import assert from 'node:assert/strict';

function installChromeFake(opts = {}) {
  const runtimeListeners = [];
  const alarmListeners = [];
  const tabRemovedListeners = [];
  const sessionStore = { ...(opts.sessionStore ?? {}) };
  const calls = {
    offscreenCreates: [],
    offscreenMessages: [],
    scriptInjections: [],
    tabMessages: [],
    tabsCreated: [],
    badgeBackgrounds: [],
    badgeTexts: [],
    alarmsCreated: [],
    alarmsCleared: [],
  };

  let offscreenExists = opts.offscreenExists ?? false;
  let lastSessionId = opts.offscreenSessionId ?? null;

  globalThis.chrome = {
    runtime: {
      onMessage: {
        addListener(fn) {
          runtimeListeners.push(fn);
        },
      },
      async sendMessage(msg) {
        calls.offscreenMessages.push(msg);
        if (msg.type === 'nashmeet:offscreen:start') {
          lastSessionId = msg.sessionId;
          return { ok: true };
        }
        if (msg.type === 'nashmeet:offscreen:stop') {
          return { ok: true, sessionId: lastSessionId };
        }
        return { ok: true };
      },
      getURL(path) {
        return `chrome-extension://nashmeet/${path}`;
      },
      onInstalled: {
        addListener() {},
      },
    },
    storage: {
      session: {
        async get(key) {
          return { [key]: sessionStore[key] };
        },
        async set(patch) {
          Object.assign(sessionStore, patch);
        },
        async remove(key) {
          delete sessionStore[key];
        },
      },
      sync: {
        async get() {
          return {
            viewerBase: 'https://nashcode.test:8443',
            livePreview: true,
            micDefaults: ['Matthias', 'Rob'],
            calendarIds: ['primary'],
          };
        },
      },
      local: {
        async get() {
          return {};
        },
      },
    },
    tabs: {
      async get(tabId) {
        return opts.tab ?? {
          id: tabId,
          title: 'Weekly Sync: Rob & Matthias',
          url: 'https://meet.google.com/abc-defg-hij',
        };
      },
      async create(opts) {
        calls.tabsCreated.push(opts);
      },
      async sendMessage(tabId, msg) {
        calls.tabMessages.push({ tabId, msg });
      },
      onRemoved: {
        addListener(fn) {
          tabRemovedListeners.push(fn);
        },
      },
    },
    tabCapture: {
      async getMediaStreamId({ targetTabId }) {
        return `stream-${targetTabId}`;
      },
    },
    offscreen: {
      async hasDocument() {
        return offscreenExists;
      },
      async createDocument(opts) {
        offscreenExists = true;
        calls.offscreenCreates.push(opts);
      },
    },
    scripting: {
      async executeScript(opts) {
        calls.scriptInjections.push(opts);
      },
    },
    action: {
      setBadgeBackgroundColor(opts) {
        calls.badgeBackgrounds.push(opts);
      },
      setBadgeText(opts) {
        calls.badgeTexts.push(opts);
      },
    },
    alarms: {
      create(name, opts) {
        calls.alarmsCreated.push({ name, opts });
      },
      clear(name) {
        calls.alarmsCleared.push(name);
      },
      onAlarm: {
        addListener(fn) {
          alarmListeners.push(fn);
        },
      },
    },
  };

  async function dispatchRuntimeMessage(msg) {
    for (const listener of runtimeListeners) {
      const handled = await new Promise((resolve) => {
        const ret = listener(msg, {}, resolve);
        if (ret !== true) resolve(undefined);
      });
      if (handled !== undefined) return handled;
    }
    return undefined;
  }

  async function dispatchAlarm(name) {
    for (const listener of alarmListeners) await listener({ name });
  }

  async function dispatchTabRemoved(tabId) {
    for (const listener of tabRemovedListeners) await listener(tabId);
  }

  return {
    calls,
    sessionStore,
    dispatchRuntimeMessage,
    dispatchAlarm,
    dispatchTabRemoved,
  };
}

test('service worker start/status/stop controls recording handoff', async () => {
  const fake = installChromeFake();
  await import(`../background.js?test=${Date.now()}-${Math.random()}`);

  const started = await fake.dispatchRuntimeMessage({ type: 'nashmeet:start', tabId: 42 });
  assert.equal(started.ok, true);
  assert.match(started.sessionId, /weekly-sync-rob-matthias$/);
  assert.deepEqual(fake.calls.offscreenCreates[0], {
    url: 'offscreen.html',
    reasons: ['USER_MEDIA'],
    justification:
      'Capture meeting tab audio and microphone for transcription; the recording must outlive the popup.',
  });
  assert.equal(fake.calls.offscreenMessages[0].type, 'nashmeet:offscreen:start');
  assert.equal(fake.calls.offscreenMessages[0].streamId, 'stream-42');
  assert.equal(fake.calls.offscreenMessages[0].meetingUrl, 'https://meet.google.com/abc-defg-hij');
  assert.equal(fake.calls.offscreenMessages[0].config.viewerBase, 'https://nashcode.test:8443');
  assert.deepEqual(fake.calls.scriptInjections[0], {
    target: { tabId: 42 },
    files: ['content.js'],
  });
  assert.deepEqual(fake.calls.tabMessages[0], {
    tabId: 42,
    msg: { type: 'nashmeet:pill:show', livePreview: true },
  });
  assert.deepEqual(fake.calls.badgeBackgrounds[0], { color: '#cc0000' });
  assert.equal(fake.calls.badgeTexts.at(-1).text.length > 0, true);
  assert.deepEqual(fake.calls.alarmsCreated[0], {
    name: 'nashmeet-badge',
    opts: { periodInMinutes: 1 },
  });

  const status = await fake.dispatchRuntimeMessage({ type: 'nashmeet:status' });
  assert.equal(status.recording, true);
  assert.equal(status.tabId, 42);
  assert.equal(status.title, 'Weekly Sync: Rob & Matthias');

  const duplicate = await fake.dispatchRuntimeMessage({ type: 'nashmeet:start', tabId: 42 });
  assert.deepEqual(duplicate, { error: 'already recording' });

  const stopped = await fake.dispatchRuntimeMessage({ type: 'nashmeet:stop' });
  assert.deepEqual(stopped, { ok: true, sessionId: started.sessionId });
  assert.deepEqual(fake.calls.tabMessages.at(-1), {
    tabId: 42,
    msg: { type: 'nashmeet:pill:transcribing' },
  });
  assert.equal(fake.calls.alarmsCleared.includes('nashmeet-badge'), true);
  assert.equal(fake.calls.badgeTexts.at(-1).text, '');
  assert.equal(fake.calls.tabsCreated.length, 1);
  assert.match(
    fake.calls.tabsCreated[0].url,
    new RegExp(`^chrome-extension://nashmeet/mapping\\.html\\?session=${started.sessionId}&pilltab=42$`),
  );

  const afterStop = await fake.dispatchRuntimeMessage({ type: 'nashmeet:status' });
  assert.deepEqual(afterStop, { recording: false });
});

test('service worker can stop a persisted recording after restart', async () => {
  const ctl = {
    tabId: 77,
    startedAt: new Date(Date.now() - 61_000).toISOString(),
    sessionId: '2026-06-21-persisted-meeting',
    title: 'Persisted Meeting',
    url: 'https://meet.google.com/persisted',
  };
  const fake = installChromeFake({
    sessionStore: { ctl },
    offscreenExists: true,
    offscreenSessionId: ctl.sessionId,
  });
  await import(`../background.js?restart=${Date.now()}-${Math.random()}`);

  const status = await fake.dispatchRuntimeMessage({ type: 'nashmeet:status' });
  assert.deepEqual(status, {
    recording: true,
    startedAt: ctl.startedAt,
    tabId: 77,
    title: 'Persisted Meeting',
  });

  await fake.dispatchAlarm('nashmeet-badge');
  assert.equal(fake.calls.badgeTexts.at(-1).text, '1m');

  const stopped = await fake.dispatchRuntimeMessage({ type: 'nashmeet:stop' });
  assert.deepEqual(stopped, { ok: true, sessionId: ctl.sessionId });
  assert.equal(fake.sessionStore.ctl, undefined);
  assert.deepEqual(fake.calls.offscreenMessages.at(-1), { type: 'nashmeet:offscreen:stop' });
  assert.deepEqual(fake.calls.tabMessages.at(-1), {
    tabId: 77,
    msg: { type: 'nashmeet:pill:transcribing' },
  });
  assert.match(
    fake.calls.tabsCreated[0].url,
    /^chrome-extension:\/\/nashmeet\/mapping\.html\?session=2026-06-21-persisted-meeting&pilltab=77$/,
  );
});

test('closing the recorded tab finalizes the persisted recording', async () => {
  const ctl = {
    tabId: 88,
    startedAt: new Date().toISOString(),
    sessionId: '2026-06-21-closed-tab',
    title: 'Closed Tab Meeting',
    url: 'https://meet.google.com/closed',
  };
  const fake = installChromeFake({
    sessionStore: { ctl },
    offscreenExists: true,
    offscreenSessionId: ctl.sessionId,
  });
  await import(`../background.js?tabclose=${Date.now()}-${Math.random()}`);

  await fake.dispatchTabRemoved(99);
  assert.equal(fake.calls.offscreenMessages.length, 0);
  assert.equal(fake.sessionStore.ctl, ctl);

  await fake.dispatchTabRemoved(88);
  assert.deepEqual(fake.calls.offscreenMessages.at(-1), { type: 'nashmeet:offscreen:stop' });
  assert.equal(fake.sessionStore.ctl, undefined);
  assert.match(
    fake.calls.tabsCreated[0].url,
    /^chrome-extension:\/\/nashmeet\/mapping\.html\?session=2026-06-21-closed-tab&pilltab=88$/,
  );
});
