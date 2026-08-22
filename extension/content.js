// nashmeet in-page recording indicator — the "● nashmeet recording" pill.
// Always visible while capture runs (including on shared screens), the
// click-to-stop control, AND the realtime preview surface: Chrome's local Web
// Speech API runs here in the page (where it reliably has mic access on a
// meeting site) and its text renders in the pill's caption. The preview is
// cosmetic — the authoritative transcript is the post-meeting Grok batch pass.
(() => {
  if (window.__nashmeetPillWired) return;
  window.__nashmeetPillWired = true;

  const PILL_ID = '__nashmeet_recording_pill';
  const CAP_ID = '__nashmeet_live_caption';
  const HEAD_ID = '__nashmeet_pill_head';
  let liveBuf = '';
  let recog = null;
  let stopWatchdog = null;

  function showPill(livePreview) {
    if (!document.getElementById(PILL_ID)) {
      const pill = document.createElement('div');
      pill.id = PILL_ID;
      Object.assign(pill.style, {
        position: 'fixed', top: '12px', right: '12px', zIndex: '2147483647',
        maxWidth: '340px', font: '600 12px/1.3 -apple-system, system-ui, sans-serif',
        userSelect: 'none',
      });

      const head = document.createElement('div');
      head.id = HEAD_ID;
      Object.assign(head.style, {
        background: 'rgba(180, 16, 16, 0.92)', color: '#fff', padding: '8px 12px',
        borderRadius: '999px', cursor: 'pointer', boxShadow: '0 2px 8px rgba(0,0,0,0.35)',
        transition: 'background 300ms ease',
      });
      head.addEventListener('click', () => {
        // Only act on a live recording — ignore clicks while already stopping or
        // transcribing so we don't fire stop twice.
        if (head.dataset.state && head.dataset.state !== 'recording') return;
        setHeadState('stopping');
        chrome.runtime.sendMessage({ type: 'nashmeet:pill:stop-clicked' });
      });

      const cap = document.createElement('div');
      cap.id = CAP_ID;
      Object.assign(cap.style, {
        display: 'none', marginTop: '6px', padding: '8px 12px', fontWeight: '400',
        background: 'rgba(20, 20, 20, 0.82)', color: '#eee', borderRadius: '12px',
        boxShadow: '0 2px 8px rgba(0,0,0,0.35)', maxHeight: '120px', overflow: 'hidden',
        // A spot-check that capture is live, not a transcript — kept to a few
        // words and faded in gently so it doesn't pull focus while you talk.
        opacity: '0', transition: 'opacity 700ms ease',
      });

      pill.appendChild(head);
      pill.appendChild(cap);
      document.documentElement.appendChild(pill);
      setHeadState('recording');
    }
    if (livePreview) startPreview();
  }

  // The pill walks recording → stopping → transcribing, ending when the
  // background tells us to hide. A safety watchdog also clears it, so a dropped
  // hide message can never leave it pinned on screen forever.
  function setHeadState(state) {
    const head = document.getElementById(HEAD_ID);
    if (!head) return;
    head.dataset.state = state;
    if (state === 'recording') {
      head.textContent = '● nashmeet recording — click to stop';
      head.style.background = 'rgba(180, 16, 16, 0.92)';
      head.style.cursor = 'pointer';
    } else if (state === 'stopping') {
      head.textContent = '… stopping';
      head.style.background = 'rgba(110, 110, 110, 0.92)';
      head.style.cursor = 'default';
      armWatchdog();
    } else if (state === 'transcribing') {
      head.textContent = '✦ transcribing… (in a new tab)';
      head.style.background = 'rgba(40, 90, 180, 0.92)';
      head.style.cursor = 'default';
      armWatchdog();
    }
  }

  function armWatchdog() {
    if (stopWatchdog) clearTimeout(stopWatchdog);
    // Long meetings take a while to transcribe; this is only a backstop against
    // a dropped hide message, not the normal clear path.
    stopWatchdog = setTimeout(hidePill, 30 * 60 * 1000);
  }

  function startPreview() {
    const Rec = window.SpeechRecognition || window.webkitSpeechRecognition;
    if (!Rec || recog) return;
    try {
      recog = new Rec();
      recog.continuous = true;
      recog.interimResults = true;
      recog.lang = 'en-US';
      recog.onresult = (ev) => {
        let interim = '';
        for (let i = ev.resultIndex; i < ev.results.length; i++) {
          const r = ev.results[i];
          if (r.isFinal) liveBuf = (liveBuf + ' ' + r[0].transcript.trim()).trim().slice(-280);
          else interim += r[0].transcript;
        }
        setCaption((liveBuf + (interim ? ' ' + interim : '')).trim().slice(-280));
      };
      // Chrome ends recognition periodically; restart while we're still recording.
      recog.onend = () => { if (recog) { try { recog.start(); } catch { /* stop race */ } } };
      recog.onerror = (e) => {
        if (e.error !== 'no-speech' && e.error !== 'aborted') console.warn('nashmeet preview', e.error);
      };
      recog.start();
    } catch (e) {
      console.warn('nashmeet preview unavailable', e);
    }
  }

  function stopPreview() {
    const r = recog;
    recog = null; // clear first so onend doesn't restart
    if (r) { try { r.onend = null; r.stop(); } catch { /* already stopped */ } }
  }

  // Show only the last few words — enough to confirm capture is live without
  // becoming a distracting running transcript.
  const CAPTION_WORDS = 7;
  function setCaption(text) {
    const cap = document.getElementById(CAP_ID);
    if (!cap || !text) return;
    const words = text.split(/\s+/).filter(Boolean).slice(-CAPTION_WORDS);
    if (!words.length) return;
    cap.textContent = words.join(' ');
    if (cap.style.display === 'none') {
      cap.style.display = 'block';
      // Next frame so the opacity transition actually animates from 0.
      requestAnimationFrame(() => { cap.style.opacity = '0.85'; });
    }
  }

  function hidePill() {
    stopPreview();
    if (stopWatchdog) { clearTimeout(stopWatchdog); stopWatchdog = null; }
    liveBuf = '';
    document.getElementById(PILL_ID)?.remove();
  }

  chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
    if (msg.type === 'nashmeet:pill:show') showPill(msg.livePreview !== false);
    if (msg.type === 'nashmeet:pill:transcribing') { stopPreview(); setHeadState('transcribing'); }
    if (msg.type === 'nashmeet:pill:hide') hidePill();
    sendResponse({ ok: true });
  });
})();
