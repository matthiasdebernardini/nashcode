import { repoNames } from './lib/brain.js';
import { getConfig, setConfig, setXaiKey } from './lib/config.js';
import { listCalendars } from './lib/calendar.js';

const calendarsEl = document.getElementById('calendars');

// --- Mic-default tag editor -------------------------------------------------
// Names render as removable chips (ordered — order = mic-speaker precheck
// order). Enter or comma adds the typed name; ✕ or Backspace-on-empty removes.
const tagsEl = document.getElementById('micDefaults');
const tagInput = document.getElementById('micDefaultsInput');
let micTags = [];

function renderTags() {
  // Clear everything except the trailing input.
  [...tagsEl.querySelectorAll('.tag')].forEach((t) => t.remove());
  micTags.forEach((name, i) => {
    const chip = document.createElement('span');
    chip.className = 'tag';
    const ord = document.createElement('span');
    ord.className = 'ord';
    ord.textContent = i + 1;
    chip.appendChild(ord);
    chip.appendChild(document.createTextNode(name));
    const x = document.createElement('span');
    x.className = 'x';
    x.textContent = '✕';
    x.title = `remove ${name}`;
    x.addEventListener('click', () => {
      micTags.splice(i, 1);
      renderTags();
    });
    chip.appendChild(x);
    tagsEl.insertBefore(chip, tagInput);
  });
}

function addTag(raw) {
  const name = raw.trim();
  if (name && !micTags.some((t) => t.toLowerCase() === name.toLowerCase())) {
    micTags.push(name);
    renderTags();
  }
}

tagsEl.addEventListener('click', () => tagInput.focus());
tagInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' || e.key === ',') {
    e.preventDefault();
    addTag(tagInput.value);
    tagInput.value = '';
  } else if (e.key === 'Backspace' && tagInput.value === '' && micTags.length) {
    micTags.pop();
    renderTags();
  }
});
// Paste of "Matthias, Rob" splits into chips.
tagInput.addEventListener('paste', (e) => {
  const text = e.clipboardData?.getData('text') || '';
  if (text.includes(',')) {
    e.preventDefault();
    text.split(',').forEach(addTag);
    tagInput.value = '';
  }
});

const signinStatusEl = document.getElementById('signin-status');

// Translate chrome.identity's raw OAuth errors into something actionable.
function explainOAuth(message) {
  const m = (message || '').toLowerCase();
  if (m.includes('bad client id') || m.includes('client id')) {
    return {
      cls: 'warn',
      title: 'Not connected to a Google client yet',
      detail:
        "The extension's manifest is still missing its Google OAuth client ID. " +
        'Create one in Google Cloud Console (Chrome-extension type, bound to this ' +
        "extension's ID), drop it into manifest.json, and reload the extension.",
    };
  }
  if (m.includes('user did not approve') || m.includes('access_denied')) {
    return { cls: 'warn', title: 'Sign-in was cancelled', detail: 'Click "Sign in with Google" to try again.' };
  }
  if (m.includes('no auth token') || m.includes('not signed in')) {
    return { cls: '', title: 'Not signed in', detail: 'Click "Sign in with Google" to connect your calendar.' };
  }
  return { cls: 'bad', title: "Couldn't sign in", detail: message || 'Unknown error.' };
}

function setSigninStatus({ cls, title, detail }) {
  signinStatusEl.innerHTML = '';
  const s = document.createElement('div');
  s.className = `status ${cls}`;
  const dot = document.createElement('span');
  dot.className = `dot ${cls === 'ok' ? 'ok' : cls === 'bad' ? 'bad' : cls === 'warn' ? 'warn' : ''}`;
  s.appendChild(dot);
  const txt = document.createElement('span');
  txt.innerHTML = `${title}${detail ? ` <span class="detail">— ${detail}</span>` : ''}`;
  s.appendChild(txt);
  signinStatusEl.appendChild(s);
  if (cls === 'warn' && title.includes('client')) {
    const c = document.createElement('div');
    c.className = 'callout';
    c.innerHTML =
      'One-time setup: this extension needs a Google OAuth <b>client ID</b>. ' +
      'See the nashmeet README → "Dev install" for the exact Google Cloud steps.';
    signinStatusEl.appendChild(c);
  }
}

async function renderCalendars(selected, interactive) {
  let cals;
  try {
    cals = await listCalendars(interactive);
  } catch (e) {
    setSigninStatus(explainOAuth(e.message));
    calendarsEl.innerHTML = '';
    return;
  }
  setSigninStatus({
    cls: 'ok',
    title: 'Signed in',
    detail: `${cals.length} calendar${cals.length === 1 ? '' : 's'} available — tick the ones nashmeet should search.`,
  });
  calendarsEl.innerHTML = '';
  for (const c of cals) {
    const label = document.createElement('label');
    const box = document.createElement('input');
    box.type = 'checkbox';
    box.value = c.id;
    box.checked = selected.includes(c.id) || (selected.length === 0 && c.primary);
    label.appendChild(box);
    label.appendChild(document.createTextNode(`${c.summary}${c.primary ? ' (primary)' : ''}`));
    calendarsEl.appendChild(label);
  }
}

// --- Microphone permission --------------------------------------------------
// The recorder lives in an offscreen document, which cannot surface Chrome's
// mic prompt. This page can — and a grant here persists for the whole
// extension origin, the offscreen doc included. So this is the only place mic
// access can actually be won; without it offscreen.js's getUserMedia throws
// NotAllowedError and "Start recording" dies on open.
const micStatusEl = document.getElementById('mic-status');
const grantMicBtn = document.getElementById('grant-mic');

function setMicStatus(cls, text, detail) {
  micStatusEl.className = `status ${cls}`;
  micStatusEl.innerHTML =
    `<span class="dot ${cls}"></span><span>${text}${detail ? ` <span class="detail">— ${detail}</span>` : ''}</span>`;
}

function reflectMicState(state) {
  if (state === 'granted') {
    setMicStatus('ok', 'Microphone access granted', 'recording can capture your voice');
    grantMicBtn.style.display = 'none';
  } else if (state === 'denied') {
    setMicStatus('bad', 'Microphone blocked',
      'allow it via the address-bar site icon (or chrome://settings/content/microphone), then reload this page');
    grantMicBtn.style.display = '';
  } else {
    setMicStatus('warn', 'Not granted yet', "click below — Chrome will ask once");
    grantMicBtn.style.display = '';
  }
}

async function checkMic() {
  try {
    const p = await navigator.permissions.query({ name: 'microphone' });
    reflectMicState(p.state);
    p.onchange = () => reflectMicState(p.state);
  } catch {
    // permissions.query may not support "microphone" everywhere — fall back to the button.
    setMicStatus('warn', 'Grant microphone access', 'click below — Chrome will ask once');
  }
}

grantMicBtn.addEventListener('click', async () => {
  setMicStatus('', 'Requesting…');
  try {
    const s = await navigator.mediaDevices.getUserMedia({ audio: true });
    s.getTracks().forEach((t) => t.stop()); // we only needed the grant, not the stream
    reflectMicState('granted');
  } catch (e) {
    if (e && e.name === 'NotAllowedError') reflectMicState('denied');
    else setMicStatus('bad', "Couldn't get the microphone", e?.message || String(e));
  }
});

// --- Service reachability ---------------------------------------------------
const serviceStatusEl = document.getElementById('service-status');
let healthDebounce = null;

function setServiceStatus(cls, text, detail) {
  serviceStatusEl.className = `status ${cls}`;
  serviceStatusEl.innerHTML =
    `<span class="dot ${cls}"></span><span>${text}${detail ? ` <span class="detail">— ${detail}</span>` : ''}</span>`;
}

// One call does both jobs: /brain proves the viewer is up AND carries the repo
// list, so the Repo field offers real names instead of a blank box.
async function checkService(base) {
  const url = (base || '').trim().replace(/\/+$/, '');
  if (!url) return setServiceStatus('warn', 'No viewer URL set');
  setServiceStatus('', 'Checking…');
  try {
    const r = await fetch(`${url}/brain`, { cache: 'no-store' });
    if (!r.ok) return setServiceStatus('bad', `Unexpected response (${r.status})`);
    setServiceStatus('ok', 'Reachable', url.replace(/^https?:\/\//, ''));
    const brain = await r.json().catch(() => null);
    renderRepoOptions(repoNames(brain));
  } catch {
    setServiceStatus('bad', "Can't reach it", 'on the tailnet? is the viewer up?');
  }
}


function renderRepoOptions(names) {
  const list = document.getElementById('repoList');
  if (!list) return;
  list.innerHTML = '';
  for (const name of names) {
    const opt = document.createElement('option');
    opt.value = name;
    list.appendChild(opt);
  }
}

const serviceInput = document.getElementById('viewerBase');
const repoInput = document.getElementById('repo');

async function load() {
  if (new URLSearchParams(globalThis.location?.search || '').get('firstrun')) {
    const fr = document.getElementById('firstrun');
    if (fr) fr.hidden = false;
  }
  const cfg = await getConfig();
  micTags = [...cfg.micDefaults];
  renderTags();
  serviceInput.value = cfg.viewerBase;
  repoInput.value = cfg.repo || '';
  document.getElementById('xaiKey').value = cfg.xaiKey || '';
  document.getElementById('livePreview').checked = cfg.livePreview;
  checkService(cfg.viewerBase);
  checkMic();
  // Non-interactive first: only shows "signed in" if a token is already cached;
  // otherwise it lands on the friendly "not signed in / no client" status.
  setSigninStatus({ cls: '', title: 'Not signed in', detail: 'Click "Sign in with Google" to connect your calendar.' });
  renderCalendars(cfg.calendarIds, false).catch(() => {});
}

// Re-check reachability as the URL is edited.
serviceInput.addEventListener('input', () => {
  clearTimeout(healthDebounce);
  healthDebounce = setTimeout(() => checkService(serviceInput.value), 500);
});

document.getElementById('signin').addEventListener('click', async () => {
  const cfg = await getConfig();
  await renderCalendars(cfg.calendarIds, true);
});

document.getElementById('save').addEventListener('click', async () => {
  const calendarIds = [...calendarsEl.querySelectorAll('input:checked')].map((b) => b.value);
  // Fold a half-typed name (no Enter pressed) into the saved set.
  if (tagInput.value.trim()) {
    addTag(tagInput.value);
    tagInput.value = '';
  }
  await setConfig({
    calendarIds,
    micDefaults: micTags,
    viewerBase: serviceInput.value.trim().replace(/\/+$/, ''),
    repo: repoInput.value.trim().replace(/^\/+|\/+$/g, ''),
    livePreview: document.getElementById('livePreview').checked,
  });
  await setXaiKey(document.getElementById('xaiKey').value);
  const saved = document.getElementById('saved');
  saved.classList.add('show');
  setTimeout(() => saved.classList.remove('show'), 1600);
});

load();
