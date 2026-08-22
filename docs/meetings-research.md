# Meeting capture for nashmeet: research notes

Date: 2026-08-22. Sources: Exa + Firecrawl searches, pages read in full. Findings feed `plans/meetings.md`.

## 1. MV3 (Manifest V3, Chrome's current extension platform) recording path

- The service worker has no DOM and cannot hold a MediaStream. Pattern: worker calls `chrome.tabCapture.getMediaStreamId()` after a user click, sends the ID to an offscreen document, and the offscreen page calls `getUserMedia` with `chromeMediaSource: 'tab'`. Chrome 116 enabled this. https://developer.chrome.com/docs/extensions/reference/api/tabCapture
- Capturing a tab mutes it for the user. Reconnect the stream to `AudioContext.destination`.
- Mic permission needs a visible extension page and a click. The popup prompt can fail silently. https://www.recall.ai/blog/how-to-build-a-chrome-recording-extension
- Failure modes: tab close kills capture; a background tab throttles, so tab audio can freeze while the mic keeps running; the worker suspending mid-stop hangs the UI. Keep recorder state in the offscreen document.
- `ondataavailable` slices spike far above the timeslice (50 MB seen at 200 ms). Large blobs in IndexedDB have crashed extensions. https://blog.addpipe.com/dealing-with-huge-mediarecorder-slices/
- CWS (Chrome Web Store) review rejects excess permissions, single-purpose violations, and undisclosed data transmission. `tabCapture` itself is allowed. https://developer.chrome.com/docs/webstore/troubleshooting

## 2. How the products work

- Three forms: bot joins the call (Otter, Fireflies, Fathom, MeetGeek), browser extension (Tactiq, tl;dv), desktop audio capture (Granola, Screenpipe). https://www.mirrorcaption.com/en/blog/ai-notetaker-without-bot
- Recall.ai sells bots and a desktop SDK. Its edge is per-participant audio streams, which give real speaker names. About $0.50 per hour. https://www.forasoft.com/blog/article/meeting-bot-api-architecture
- Vexa is the credible open-source bot: Apache-2.0, self-hosted, Meet/Teams/Zoom/Jitsi. https://github.com/vexa-ai/vexa
- Speaker names come from: Meet caption DOM (exact labels, lossy text), ASR (automatic speech recognition) diarization (better text, anonymous `speaker 0/1`), calendar attendees (the candidate list to map onto).
- Recall's read-aloud test: Meet native 85.7%, bot+ASR 83.1%, caption scraping 82.0%. https://www.recall.ai/blog/how-to-get-transcripts-from-google-meet-developer-edition
- Recall's case against extensions: Chromium only, tab-throttling gaps, one mixed stream, users forget to press record. A competitor's brief, but the technical claims match the docs. https://medium.com/@amanda.zhu/why-a-chrome-extension-is-the-wrong-choice-for-building-a-botless-meeting-recorder-41b3695e5c84

## 3. Batch STT (speech-to-text) with diarization

| API | Diarization | Size cap | Price/hr | Browser-callable |
|---|---|---|---|---|
| xAI `/v1/stt` | yes, per-word speaker index | 500 MB | $0.10 | yes |
| Deepgram nova-3 | +$0.12/hr | 2 GB | ~$0.29 | no, CORS blocked |
| AssemblyAI U-3.5 Pro | +$0.02/hr | 5 GB / 10 h | $0.21 | key must stay server-side |
| OpenAI gpt-4o-transcribe | separate `-diarize` model | 25 MB | $0.36 | discouraged |
| ElevenLabs Scribe v2 | yes, word timings | — | $0.22 | yes |

xAI also takes `keyterm` biasing (100 terms) for client names and jargon. https://docs.x.ai/developers/rest-api-reference/inference/speech-to-text.md · https://deepgram.com/pricing · https://www.assemblyai.com/pricing

## 4. Caption-scraping extensions

- TranscripTonic (MIT, 10k users) reads Meet captions, keeps data on-device, posts to a webhook. It breaks on Meet UI changes and ships a "recover last meeting" button. https://github.com/vivek-nexus/transcriptonic
- gmeetcaptions: `MutationObserver` plus a 1 s poll; a turn is final after four unchanged polls. Semantic selectors first (`[role="region"][aria-label="Captions"]`), obfuscated class names as fallback. https://www.s-anand.net/blog/google-meet-captions-local-transcript-recorder/
- Recall ships a reference extension that does both caption scraping and tab recording. https://github.com/recallai/chrome-recording-transcription-extension
- Caption text is lossy: Meet paraphrases, truncates, drops punctuation.

## 5. Transcript to action items to cards

- Split the job. One prompt for summary + action items + sentiment degrades all three. https://open-multi-agent.com/blog/meeting-summarizer-parallel-agents/
- Force a schema: owner, action, due, dependency. Require a source quote per item. Unknown owner is "Unassigned", never a guess. https://python.useinstructor.com/examples/action_items/
- Git-backed prior art: `backbrief` (Zoom to git vault to Linear), `granola-to-minutes` (front-matter markdown), `zoom-to-markdown` (GitHub Action). https://github.com/EvgenSmith/backbrief · https://github.com/calvindotsg/granola-to-minutes

## Recommendation

Keep the agmeet architecture. tabCapture + mic in an offscreen document is the only compliant MV3 path. Grok STT is the best fit: $0.10/hr, 500 MB cap, real diarization, no CORS wall. The speaker-mapping screen is the correct answer to anonymous speaker indices. Filing to our own server keeps audio off third-party servers.

Change later, in this order:

1. Add a Meet caption content script next to the audio. Align captions with diarized text by timestamp and most meetings need no mapping screen.
2. Shrink the mapping screen to a confirmation. Pre-fill from calendar attendees.
3. Harden chunks: cap IndexedDB blob size, add a crash recovery path.

Risks:

1. Silent gaps from tab switching. Detect tab RMS at zero and warn in-session.
2. CWS review and Meet DOM churn. Disclose the server POST, keep permissions minimal, pin selectors.
3. Coverage ceiling: no native Zoom, Teams, or Safari. Decide if that is out of scope.
