const app = document.getElementById('app');
const statusEl = document.getElementById('status');
const toggle = document.getElementById('toggle');
const label = toggle.querySelector('.lbl');
const errEl = document.getElementById('err');

function showError(msg, withSettings = false) {
  errEl.textContent = '';
  if (!msg) {
    errEl.style.display = 'none';
    return;
  }
  errEl.append(msg);
  if (withSettings) {
    const a = document.createElement('a');
    a.href = '#';
    a.textContent = 'Open Settings';
    a.style.marginLeft = '6px';
    a.addEventListener('click', (e) => {
      e.preventDefault();
      chrome.runtime.openOptionsPage();
    });
    errEl.append(a);
  }
  errEl.style.display = 'block';
}

async function refresh() {
  const st = await chrome.runtime.sendMessage({ type: 'nashmeet:status' });
  if (st?.recording) {
    app.dataset.state = 'rec';
    const mins = Math.floor((Date.now() - Date.parse(st.startedAt)) / 60000);
    statusEl.textContent = `Recording “${st.title || 'meeting'}” · ${mins} min`;
    label.textContent = 'Stop & map speakers';
  } else {
    app.dataset.state = 'idle';
    statusEl.textContent = 'Not recording';
    label.textContent = 'Start recording this tab';
  }
}

toggle.addEventListener('click', async () => {
  showError('');
  const st = await chrome.runtime.sendMessage({ type: 'nashmeet:status' });
  if (st?.recording) {
    toggle.disabled = true;
    label.textContent = 'Stopping…';
    statusEl.textContent = 'Finishing up & transcribing…';
    await chrome.runtime.sendMessage({ type: 'nashmeet:stop' });
    window.close();
  } else {
    toggle.disabled = true;
    label.textContent = 'Starting…';
    try {
      const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
      if (!tab) throw new Error('No active tab to record');
      const res = await chrome.runtime.sendMessage({ type: 'nashmeet:start', tabId: tab.id });
      if (res?.error) throw new Error(res.error);
      await refresh();
    } catch (e) {
      const msg = e?.message || String(e);
      // A mic-permission failure is the common first-run wall — give a 1-click
      // path to the page where the grant can actually be made.
      showError(msg, /microphone|\bmic\b|settings/i.test(msg));
      label.textContent = 'Start recording this tab';
    } finally {
      toggle.disabled = false;
    }
  }
});

document.getElementById('open-options').addEventListener('click', (e) => {
  e.preventDefault();
  chrome.runtime.openOptionsPage();
});

refresh();
