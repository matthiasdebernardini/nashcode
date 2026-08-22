import { test } from 'node:test';
import assert from 'node:assert/strict';

class FakeClassList {
  constructor(el) {
    this.el = el;
    this.names = new Set();
  }

  add(...names) {
    names.forEach((name) => this.names.add(name));
    this.el.className = [...this.names].join(' ');
  }

  remove(...names) {
    names.forEach((name) => this.names.delete(name));
    this.el.className = [...this.names].join(' ');
  }

  contains(name) {
    return this.names.has(name);
  }
}

class FakeElement {
  constructor(tagName = 'div') {
    this.tagName = tagName.toUpperCase();
    this.children = [];
    this.parentNode = null;
    this.listeners = {};
    this.style = {};
    this.value = '';
    this.checked = false;
    this.type = '';
    this.title = '';
    this._innerHTML = '';
    this._textContent = '';
    this.classList = new FakeClassList(this);
    this.className = '';
  }

  set innerHTML(value) {
    this._innerHTML = value;
    this.children = [];
  }

  get innerHTML() {
    return this._innerHTML;
  }

  set textContent(value) {
    this._textContent = String(value);
  }

  get textContent() {
    return this._textContent;
  }

  appendChild(child) {
    child.parentNode = this;
    this.children.push(child);
    return child;
  }

  insertBefore(child, before) {
    child.parentNode = this;
    const index = this.children.indexOf(before);
    if (index === -1) {
      this.children.push(child);
    } else {
      this.children.splice(index, 0, child);
    }
    return child;
  }

  remove() {
    if (!this.parentNode) return;
    this.parentNode.children = this.parentNode.children.filter((child) => child !== this);
    this.parentNode = null;
  }

  addEventListener(type, fn) {
    this.listeners[type] ||= [];
    this.listeners[type].push(fn);
  }

  async dispatch(type, event = {}) {
    const payload = {
      preventDefault() {},
      ...event,
    };
    for (const fn of this.listeners[type] || []) {
      await fn(payload);
    }
  }

  focus() {
    this.focused = true;
  }

  querySelectorAll(selector) {
    const out = [];
    const matches = (el) => {
      if (selector === '.tag') return el.className.split(/\s+/).includes('tag');
      if (selector === 'input') return el.tagName === 'INPUT';
      if (selector === 'input:checked') return el.tagName === 'INPUT' && el.checked;
      return false;
    };
    const walk = (el) => {
      for (const child of el.children) {
        if (matches(child)) out.push(child);
        walk(child);
      }
    };
    walk(this);
    return out;
  }
}

class FakeTextNode extends FakeElement {
  constructor(text) {
    super('#text');
    this.textContent = text;
  }
}

function installOptionsFakes() {
  const ids = [
    'calendars',
    'micDefaults',
    'micDefaultsInput',
    'signin-status',
    'mic-status',
    'grant-mic',
    'service-status',
    'viewerBase',
    'repo',
    'repoList',
    'xaiKey',
    'livePreview',
    'signin',
    'save',
    'saved',
  ];
  const elements = Object.fromEntries(ids.map((id) => [id, new FakeElement()]));
  elements.livePreview.type = 'checkbox';
  elements.micDefaults.appendChild(elements.micDefaultsInput);

  globalThis.document = {
    getElementById(id) {
      return elements[id];
    },
    createElement(tagName) {
      return new FakeElement(tagName);
    },
    createTextNode(text) {
      return new FakeTextNode(text);
    },
  };

  const syncStore = {
    viewerBase: 'https://nashcode.test:8443/',
    repo: 'notes',
    livePreview: false,
    micDefaults: ['Matthias'],
    calendarIds: ['primary'],
  };
  const localStore = {};
  const calls = { fetches: [], auth: [], storageSets: [] };

  globalThis.chrome = {
    runtime: { lastError: null },
    identity: {
      getAuthToken(opts, cb) {
        calls.auth.push(opts);
        cb('token-1');
      },
    },
    storage: {
      sync: {
        async get(keys) {
          return Object.fromEntries(keys.map((key) => [key, syncStore[key]]));
        },
        async set(patch) {
          calls.storageSets.push(patch);
          Object.assign(syncStore, patch);
        },
      },
      local: {
        async get(key) {
          return key in localStore ? { [key]: localStore[key] } : {};
        },
        async set(patch) {
          Object.assign(localStore, patch);
        },
      },
    },
  };

  Object.defineProperty(globalThis, 'navigator', {
    value: {
      permissions: {
        async query() {
          return { state: 'granted' };
        },
      },
      mediaDevices: {
        async getUserMedia() {
          return { getTracks: () => [{ stop() {} }] };
        },
      },
    },
    configurable: true,
  });

  globalThis.fetch = async (url) => {
    calls.fetches.push(String(url));
    if (String(url).endsWith('/brain')) {
      return {
        ok: true,
        status: 200,
        json: async () => ({ repos: [{ name: 'nashcode' }, { name: 'meetings' }] }),
      };
    }
    if (String(url).includes('/users/me/calendarList')) {
      return {
        ok: true,
        status: 200,
        json: async () => ({
          items: [
            { id: 'primary', summary: 'Primary', primary: true },
            { id: 'ops', summary: 'Operations' },
          ],
        }),
      };
    }
    throw new Error(`unexpected fetch ${url}`);
  };

  return { elements, syncStore, localStore, calls };
}

async function settle() {
  await new Promise((resolve) => setTimeout(resolve, 20));
}

test('options page restores and saves viewer, repo, mic, calendar, and preview config', async () => {
  const fake = installOptionsFakes();
  await import(`../options.js?test=${Date.now()}`);
  await settle();

  assert.equal(fake.elements.viewerBase.value, 'https://nashcode.test:8443/');
  assert.equal(fake.elements.repo.value, 'notes');
  assert.equal(fake.elements.livePreview.checked, false);
  assert.equal(fake.elements.micDefaults.querySelectorAll('.tag').length, 1);
  assert.deepEqual(
    fake.elements.calendars.querySelectorAll('input:checked').map((input) => input.value),
    ['primary'],
  );
  assert.equal(fake.calls.auth[0].interactive, false);
  assert.equal(fake.calls.fetches.some((url) => url === 'https://nashcode.test:8443/brain'), true);
  // /brain doubles as the repo-name source for the datalist.
  assert.deepEqual(
    fake.elements.repoList.children.map((opt) => opt.value),
    ['meetings', 'nashcode'],
  );

  const calendarInputs = fake.elements.calendars.querySelectorAll('input');
  calendarInputs[1].checked = true;
  fake.elements.micDefaultsInput.value = 'Rob';
  fake.elements.viewerBase.value = 'https://nashcode.local:8443///';
  fake.elements.repo.value = '  /meetings/  ';
  fake.elements.xaiKey.value = '  xai-abc123  ';
  fake.elements.livePreview.checked = true;

  await fake.elements.save.dispatch('click');
  await settle();

  assert.deepEqual(fake.syncStore.calendarIds, ['primary', 'ops']);
  assert.deepEqual(fake.syncStore.micDefaults, ['Matthias', 'Rob']);
  assert.equal(fake.syncStore.viewerBase, 'https://nashcode.local:8443');
  assert.equal(fake.syncStore.repo, 'meetings');
  assert.equal(fake.syncStore.livePreview, true);
  // xAI key persists to local storage (not sync), trimmed.
  assert.equal(fake.localStore.xaiKey, 'xai-abc123');
  assert.equal(fake.elements.saved.classList.contains('show'), true);
});

test('viewer reachability flags a non-ok /brain as bad', async () => {
  const fake = installOptionsFakes();
  globalThis.fetch = async (url) => {
    if (String(url).endsWith('/brain')) return { ok: false, status: 500, text: async () => 'err' };
    if (String(url).includes('/users/me/calendarList')) {
      return { ok: true, status: 200, json: async () => ({ items: [] }) };
    }
    throw new Error(`unexpected fetch ${url}`);
  };
  await import(`../options.js?svcbad=${Date.now()}`);
  await settle();
  assert.match(fake.elements['service-status'].className, /bad/);
  assert.match(fake.elements['service-status'].innerHTML, /Unexpected response \(500\)/);
});

test('viewer reachability flags a network error as unreachable', async () => {
  const fake = installOptionsFakes();
  globalThis.fetch = async (url) => {
    if (String(url).endsWith('/brain')) throw new Error('boom');
    if (String(url).includes('/users/me/calendarList')) {
      return { ok: true, status: 200, json: async () => ({ items: [] }) };
    }
    throw new Error(`unexpected fetch ${url}`);
  };
  await import(`../options.js?svcthrow=${Date.now()}`);
  await settle();
  assert.match(fake.elements['service-status'].className, /bad/);
  assert.match(fake.elements['service-status'].innerHTML, /Can't reach it/);
});
