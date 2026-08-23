# nashmeet — Chrome extension

Records a meeting's tab audio and your microphone, transcribes both through
Grok, names the speakers from your Google Calendar, and files the result as a
markdown transcript in a nashcode repo.

It works on any meeting site — Meet, Zoom, Teams — because it captures audio,
not the page. Recording is local first: the audio lands in IndexedDB every five
seconds, so a dropped network or a crashed tab still gives you a full
transcript.

Forked from `agmeet` and re-pointed at nashcode. nashcode keeps no audio. Only
the transcript is filed.

## Dev install

1. Open chrome://extensions and turn on Developer mode.
2. Click "Load unpacked" and select this directory.
3. Copy the extension ID that Chrome assigns.
4. Add that ID to the Google OAuth client at https://console.cloud.google.com/auth/clients — the client ID in `manifest.json` is the `agmeet` one and it is reused here.
5. Reload the extension at chrome://extensions.
6. Open the extension options and fill them in (below).

## Options

1. **nashcode viewer URL** — defaults to https://nashcode.tail76ec53.ts.net:8443. Green means reachable.
2. **Repo** — the nashcode repo to file into. The field offers the live repo list from the viewer.
3. **xAI API key** — get one at https://console.x.ai. Required. It stays on this computer.
4. **Grant microphone access** — click it once. Chrome only lets this page ask.
5. **Sign in with Google** — then tick the calendars to search for the meeting event.
6. **Who's on your mic** — your name first, then anyone sharing your webcam.
7. Click **Save settings**.

## What happens

Click the toolbar icon on a meeting tab. A red badge and an in-page pill show
that recording is on; click the pill to stop. The mapping screen then opens:
Grok transcribes both channels, the calendar event pre-fills the speaker names,
you confirm, and the transcript is filed.

nashmeet POSTs to `${viewerBase}/${repo}/context/meeting`. The viewer commits the
markdown to `context/meeting/YYYY/MM/<id>.md` on the repo's default branch and
answers with the path and the commit. The mapping screen shows both and links
to the repo. The local recording is deleted only after that answer arrives.

The id ends in a hash of the meeting URL, so a meeting filed twice is filed once:
the second POST answers `200 {existing: true}` with no new commit, and the screen
says "Already filed". Re-filing after a hiccup is safe.

After filing, run `/context-digest` in Claude Code inside the repo (or
`bin/context-digest`). It cleans up the raw transcript, writes what the meeting
decided into `brain/entities/`, and updates the kanban.

## Long meetings

xAI's speech endpoint is batch-only and caps request size, so the recorder
rotates: once a segment passes `segmentTargetSeconds` and the channel goes
quiet, the MediaRecorder cuts and restarts. Cutting inside a pause splits no
word. Each segment is transcribed on its own and stitched back onto one
timeline. Meetings shorter than the target produce one segment and one request.

The knobs live in `lib/config.js`. Defaults are 25 min target, 30 min hard cap.
Raise the target to keep typical meetings single-pass.

## Layout

- `background.js` — service worker: session start/stop, badge, pill injection
- `offscreen.js` — capture engine: tabCapture + mic, one rotating MediaRecorder per channel
- `content.js` — the in-page "● nashmeet recording" pill
- `mapping.html/js` — transcribe, name the speakers, file to nashcode
- `options.html/js` — Google sign-in, calendars, viewer URL, repo, xAI key
- `lib/mapping-core.js` — pure logic (id format, channel merge, prefill rules)
- `lib/providers/grok.js` + `lib/transcribe.js` — provider plus fallback machine
- `lib/recorder-db.js` — IndexedDB recording store

## Tests

    npm test

## Notes

- The laptop must be on the tailnet. The viewer is not reachable anywhere else.
- Without the mic grant, "Start recording" fails the moment it opens the mic.
- Without a repo set, the mapping screen refuses to file and keeps the recording.
- A split meeting shows the same person on several speaker cards. Give them the
  same name and they collapse into one speaker on file.
- Grok is the only provider wired today. `lib/transcribe.js` takes more.
