import { test } from 'node:test';
import assert from 'node:assert/strict';

function installIndexedDbFake() {
  const state = {
    stores: {},
    chunks: [],
    sessions: new Map(),
  };

  const db = {
    objectStoreNames: {
      contains(name) {
        return Object.hasOwn(state.stores, name);
      },
    },
    createObjectStore(name) {
      state.stores[name] = true;
      return {
        createIndex() {},
      };
    },
    transaction(storeName) {
      const tx = {
        oncomplete: null,
        onerror: null,
        error: null,
        objectStore(name) {
          return {
            add(row) {
              assert.equal(name, 'chunks');
              state.chunks.push(row);
              const req = { result: state.chunks.length };
              queueMicrotask(() => tx.oncomplete?.());
              return req;
            },
            put(row) {
              assert.equal(name, 'sessions');
              state.sessions.set(row.sessionId, row);
              const req = { result: row.sessionId };
              queueMicrotask(() => tx.oncomplete?.());
              return req;
            },
          };
        },
      };
      assert.ok(['chunks', 'sessions'].includes(storeName));
      return tx;
    },
    close() {},
  };

  globalThis.indexedDB = {
    open() {
      const req = { result: db, error: null, onupgradeneeded: null, onsuccess: null, onerror: null };
      queueMicrotask(() => {
        req.onupgradeneeded?.();
        req.onsuccess?.();
      });
      return req;
    },
  };

  return state;
}

function makeTrack(label) {
  return {
    label,
    muted: false,
    enabled: true,
    readyState: 'live',
    stopped: false,
    listeners: {},
    stop() {
      this.stopped = true;
      this.readyState = 'ended';
    },
    addEventListener(type, fn) {
      this.listeners[type] ||= [];
      this.listeners[type].push(fn);
    },
  };
}

function makeStream(label) {
  const track = makeTrack(label);
  return {
    label,
    track,
    getTracks() {
      return [track];
    },
    getAudioTracks() {
      return [track];
    },
  };
}

function installOffscreenFakes({ micError = null } = {}) {
  const listeners = [];
  const db = installIndexedDbFake();
  const calls = {
    getUserMedia: [],
    contexts: [],
    mediaSources: [],
    recorders: [],
  };
  const tabStream = makeStream('tab-audio');
  const micStream = makeStream('mic-audio');

  globalThis.chrome = {
    runtime: {
      onMessage: {
        addListener(fn) {
          listeners.push(fn);
        },
      },
    },
  };

  Object.defineProperty(globalThis, 'navigator', {
    value: {
      mediaDevices: {
        async getUserMedia(constraints) {
          calls.getUserMedia.push(constraints);
          if (constraints.audio?.mandatory?.chromeMediaSource === 'tab') return tabStream;
          if (micError) throw micError;
          return micStream;
        },
      },
    },
    configurable: true,
  });

  class FakeAudioContext {
    constructor() {
      this.destination = {};
      this.closed = false;
      this.state = 'suspended';
      calls.contexts.push(this);
    }

    async resume() {
      this.state = 'running';
    }

    createMediaStreamSource(stream) {
      calls.mediaSources.push(stream);
      return { connect() {} };
    }

    createAnalyser() {
      return {
        fftSize: 0,
        getFloatTimeDomainData(data) {
          data.fill(1);
        },
      };
    }

    close() {
      this.closed = true;
      return Promise.resolve();
    }
  }

  class FakeMediaRecorder {
    constructor(stream, opts) {
      this.stream = stream;
      this.opts = opts;
      this.state = 'inactive';
      this.ondataavailable = null;
      this.onstop = null;
      calls.recorders.push(this);
    }

    start(timeslice) {
      this.timeslice = timeslice;
      this.state = 'recording';
    }

    requestData() {
      this.ondataavailable?.({
        data: new Blob([`${this.stream.label}-chunk`], { type: this.opts.mimeType }),
      });
    }

    stop() {
      this.state = 'inactive';
      this.onstop?.();
    }
  }

  globalThis.AudioContext = FakeAudioContext;
  globalThis.MediaRecorder = FakeMediaRecorder;

  async function dispatchRuntimeMessage(msg) {
    for (const listener of listeners) {
      const handled = await new Promise((resolve) => {
        const ret = listener(msg, {}, resolve);
        if (ret !== true) resolve(undefined);
      });
      if (handled !== undefined) return handled;
    }
    return undefined;
  }

  return { calls, db, tabStream, micStream, dispatchRuntimeMessage };
}

async function settle() {
  await new Promise((resolve) => setTimeout(resolve, 20));
}

test('offscreen start records tab and mic streams and persists session/chunks', async () => {
  const fake = installOffscreenFakes();
  await import(`../offscreen.js?test=${Date.now()}`);

  const started = await fake.dispatchRuntimeMessage({
    type: 'nashmeet:offscreen:start',
    streamId: 'stream-42',
    sessionId: 'sess-1',
    startedAt: '2026-06-21T12:00:00.000Z',
    meetingUrl: 'https://meet.google.com/abc-defg-hij',
    title: 'Weekly Sync',
    config: {
      segmentTargetSeconds: 9999,
      segmentMaxSeconds: 9999,
      segmentSilenceRms: 0.01,
      segmentSilenceHoldMs: 800,
    },
  });

  assert.deepEqual(started, { ok: true });
  assert.deepEqual(fake.calls.getUserMedia[0], {
    audio: { mandatory: { chromeMediaSource: 'tab', chromeMediaSourceId: 'stream-42' } },
  });
  assert.deepEqual(fake.calls.getUserMedia[1], {
    audio: { echoCancellation: true, noiseSuppression: true },
  });
  assert.equal(fake.calls.recorders.length, 2);
  assert.equal(fake.calls.recorders.every((rec) => rec.timeslice === 5000), true);
  assert.equal(fake.db.sessions.get('sess-1').endedAt, null);

  const stopped = await fake.dispatchRuntimeMessage({ type: 'nashmeet:offscreen:stop' });
  await settle();

  assert.equal(stopped.ok, true);
  assert.equal(stopped.sessionId, 'sess-1');
  assert.equal(fake.tabStream.track.stopped, true);
  assert.equal(fake.micStream.track.stopped, true);
  assert.equal(fake.calls.contexts[0].closed, true);
  assert.deepEqual(
    fake.db.chunks.map((row) => [row.sessionId, row.channel, row.segmentIndex, row.offsetMs]),
    [
      ['sess-1', 'tab', 0, 0],
      ['sess-1', 'mic', 0, 0],
    ],
  );
  assert.match(fake.db.sessions.get('sess-1').endedAt, /^\d{4}-\d{2}-\d{2}T/);
});

test('offscreen mic permission failure stops tab capture and returns actionable error', async () => {
  const err = new Error('denied');
  err.name = 'NotAllowedError';
  const fake = installOffscreenFakes({ micError: err });
  await import(`../offscreen.js?test=${Date.now()}-denied`);

  const started = await fake.dispatchRuntimeMessage({
    type: 'nashmeet:offscreen:start',
    streamId: 'stream-99',
    sessionId: 'sess-denied',
    startedAt: '2026-06-21T12:00:00.000Z',
    meetingUrl: 'https://meet.google.com/abc-defg-hij',
    title: 'Weekly Sync',
    config: {},
  });

  assert.match(started.error, /Grant microphone access/);
  assert.equal(fake.tabStream.track.stopped, true);
  assert.equal(fake.calls.contexts[0].closed, true);
  assert.equal(fake.calls.recorders.length, 0);
  assert.equal(fake.db.sessions.has('sess-denied'), false);
});
