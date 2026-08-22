---
name: meeting-digest
description: Turn a filed meeting transcript (transcripts/YYYY/MM/*.md, front matter `digested: false`) into a clean digest and kanban updates. Use when the user says "digest the meeting", "process the transcript", "update the board from the meeting", or after nashmeet files a transcript. Also run headless by bin/meeting-digest.
---

# meeting-digest

Input: every `transcripts/**/*.md` with `digested: false` in its front matter. If the user names a file, do only that one.

For each transcript:

1. Read it. The `## Transcript` section is raw speech-to-text with speaker labels.
2. Replace the `## Transcript` section with a cleaned one. Keep every speaker turn and its timestamp. Remove filler ("um", "you know", false starts), fix obvious STT errors, do not change meaning. Do not shorten a turn by more than a third.
3. Insert `## Summary` (3-6 sentences) and `## Decisions` (bullet list, or "None") before `## Action items`.
4. Rewrite `## Action items` as a bullet list. Each item: `- [ ] <what> (<owner>, <due or "no date">) -> tasks/<slug>.md`. Keep items already there. Add items you find in the transcript. Do not invent work.
5. For each action item, create or update a card under `tasks/`:
   - New work: write `tasks/<slug>.md` with front matter `status: todo`, `title`, `assignee` (the owner's first name, lowercase, or omit), `source: transcripts/.../<id>.md`, then one paragraph of what to do and why.
   - Existing card that the meeting moved (someone said it is started, done, or dropped): change only its `status:` line. Do not touch the rest of the file. Valid statuses: `todo`, `doing`, `done`.
   - Match existing cards by title words. If unsure, create nothing and list the item under `## Unmatched` in the transcript.
6. Set `digested: true` in the transcript front matter.
7. Commit once per transcript: `meeting: digest <id>`. Push to the `nashcode` remote (or `origin` if there is no `nashcode` remote). Do not create a branch; digests go on the default branch like board moves do.

Rules:
- Never delete a transcript line that contains a number, a date, a name, or a price.
- Never edit a card's body that you did not write.
- Speakers named "Speaker N" stay as they are.
- Write in plain English. Short sentences. No hype.

Done: print each transcript id, the cards created, the cards moved, and the commit sha.
