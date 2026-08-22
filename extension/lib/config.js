// Central knobs. Everything here is overridable from the options page
// (chrome.storage.sync), these are just the defaults.

export const DEFAULTS = {
  // The nashcode viewer on the tailnet. Transcripts POST to
  // `${viewerBase}/${repo}/transcripts` and land as markdown on the repo's
  // default branch. Only a pre-fill: the options page overrides it into
  // chrome.storage.sync, and someone on another tailnet just sees
  // checkService() go red until they point it at their own viewer.
  viewerBase: 'https://nashcode.tail76ec53.ts.net:8443',
  // Which nashcode repo the transcripts get filed into. No default — an empty
  // repo name is the one setting nashmeet cannot guess, so the options page
  // offers the live repo list from GET /brain.
  repo: '',
  // Names prechecked for mic-channel speakers (you first, then anyone sharing
  // your webcam). Empty by default — the first-run setup asks each user for
  // their own name; hardcoding specific people would mislabel everyone else.
  micDefaults: [],
  // Selected calendar ids to search for the overlapping event.
  calendarIds: [],
  // Provider order for the final batch pass — first that succeeds wins. Grok is
  // the only provider wired today; add another by registering it in
  // transcribe.js PROVIDERS and listing its name here.
  providerOrder: ['grok'],
  // Live streaming preview during the meeting (cosmetic; the batch pass is
  // the artifact). Off only saves a few cents.
  livePreview: true,

  // Long-meeting chunking (offscreen.js). The recorder rotates — cutting the
  // webm container at a detected pause — so each Grok request stays under xAI's
  // batch size cap. Only meetings longer than the target chunk at all; shorter
  // ones stay one byte-identical request. Defaults are conservative until the
  // real cap is probed (see README): they split some <1 hr meetings but are
  // safe under essentially any plausible cap. A very large target effectively
  // disables chunking.
  //
  //   segmentTargetSeconds — min elapsed before a detected pause may cut.
  //   segmentMaxSeconds     — hard cap: force a cut even with no pause (the
  //                           only lossy case — a clip can land mid-word).
  //   segmentSilenceRms     — RMS below this counts as silence (0..1).
  //   segmentSilenceHoldMs  — silence must hold this long before it qualifies.
  segmentTargetSeconds: 1500,
  segmentMaxSeconds: 1800,
  segmentSilenceRms: 0.01,
  segmentSilenceHoldMs: 800,
};

export async function getConfig() {
  const stored = await chrome.storage.sync.get(Object.keys(DEFAULTS));
  // ponytail: the xAI key lives in chrome.storage.local, never sync — sync would
  // push a personal API key to Google's servers across the user's devices.
  const { xaiKey = '' } = await chrome.storage.local.get('xaiKey');
  return { ...DEFAULTS, ...stored, xaiKey };
}

export async function setConfig(patch) {
  await chrome.storage.sync.set(patch);
}

export async function setXaiKey(xaiKey) {
  await chrome.storage.local.set({ xaiKey: (xaiKey || '').trim() });
}
