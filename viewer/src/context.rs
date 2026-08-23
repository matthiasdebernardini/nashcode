//! Context: a meeting, an email, a pasted chat, or a note becomes one committed
//! markdown file at `context/<kind>/YYYY/MM/<id>.md` on the default branch.
//!
//! The file is the record. Nothing about a context item lives in SQLite, so the two
//! questions this module answers — "what does this payload become" and "what is
//! already filed" — are both answered from git.
//!
//! The meeting wire types match the browser extension's payload field for field, so
//! the same extension files into a nashcode repo and into anything else that speaks
//! this shape. Timestamps travel as RFC3339 strings and land in the front matter
//! verbatim; only the id and the path need them parsed.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const CONTEXT_DIR: &str = "context";

/// The four kinds. Anything else is a `400`: a typo must not quietly open a fifth
/// directory nobody digests.
pub const KINDS: [&str; 4] = ["meeting", "email", "chat", "note"];

pub fn known_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

// ---- wire types ------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptPayload {
    /// Meeting title — the calendar event title when one was found, else whatever
    /// the user typed. May be empty; the id and the heading fall back to "meeting".
    #[serde(default)]
    pub title: String,
    pub started_at: String,
    pub ended_at: String,
    /// Meeting page URL (meet.google.com/…, zoom.us/…). This is the meeting's
    /// `source`: the provider's stable id for it.
    #[serde(default)]
    pub meeting_url: Option<String>,
    /// Which provider produced the final pass, e.g. "grok-batch".
    #[serde(default)]
    pub provider: Option<String>,
    pub speakers: Vec<Speaker>,
    /// False when the user skipped the speaker mapping screen.
    #[serde(default)]
    pub speakers_confirmed: bool,
    #[serde(default)]
    pub calendar_event: Option<CalendarEvent>,
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
    pub segments: Vec<Segment>,
}

/// Every kind that is not a meeting: a subject line, a time, and the text.
#[derive(Debug, Clone, Deserialize)]
pub struct ItemPayload {
    #[serde(default)]
    pub title: String,
    pub at: String,
    pub text: String,
    /// The provider's stable id: a Gmail message id, a chat thread plus day, a URL.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Speaker {
    /// Diarization label, e.g. "S1".
    pub id: String,
    /// Assigned display name; falls back to "Speaker N" when unconfirmed.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Segment {
    /// Speaker id referencing [`TranscriptPayload::speakers`].
    pub speaker: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attendee {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionItem {
    pub title: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub due: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// One payload of either shape, already checked, with the four fields the id and the
/// path are made of pulled out front.
#[derive(Debug, Clone)]
pub enum Payload {
    Meeting(TranscriptPayload),
    Item(ItemPayload),
}

impl Payload {
    /// Parse a request body for `kind`. The error is the 400 body, so it says what to
    /// fix — both the parse failure and the validation failure land here.
    pub fn parse(kind: &str, body: &[u8]) -> Result<Self, String> {
        let payload = if kind == "meeting" {
            Payload::Meeting(
                serde_json::from_slice::<TranscriptPayload>(body)
                    .map_err(|e| format!("that is not a meeting transcript: {e}"))?,
            )
        } else {
            Payload::Item(
                serde_json::from_slice::<ItemPayload>(body)
                    .map_err(|e| format!("that is not a {kind} item: {e}"))?,
            )
        };
        payload.validate()?;
        Ok(payload)
    }

    /// Reject payloads that cannot be filed meaningfully.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Payload::Meeting(m) => m.validate(),
            Payload::Item(item) => {
                if parse(&item.at).is_none() {
                    return Err("at is not an RFC3339 timestamp".into());
                }
                if item.text.trim().is_empty() {
                    return Err("text is empty — nothing to file".into());
                }
                Ok(())
            }
        }
    }

    /// The instant the item is filed under: a meeting's `started_at`, everything
    /// else's `at`.
    pub fn at(&self) -> &str {
        match self {
            Payload::Meeting(m) => &m.started_at,
            Payload::Item(item) => &item.at,
        }
    }

    /// The provider's stable id, when the caller gave one.
    pub fn source(&self) -> Option<&str> {
        match self {
            Payload::Meeting(m) => m.meeting_url.as_deref(),
            Payload::Item(item) => item.source.as_deref(),
        }
    }

    pub fn raw_title(&self) -> &str {
        match self {
            Payload::Meeting(m) => &m.title,
            Payload::Item(item) => &item.title,
        }
    }
}

// ---- validation ------------------------------------------------------------------

impl TranscriptPayload {
    /// Reject payloads that cannot be filed meaningfully. The message goes back to
    /// the caller as the 400 body, so it says what to fix.
    pub fn validate(&self) -> Result<(), String> {
        let started = parse(&self.started_at).ok_or("started_at is not an RFC3339 timestamp")?;
        let ended = parse(&self.ended_at).ok_or("ended_at is not an RFC3339 timestamp")?;
        if self.segments.is_empty() {
            return Err("transcript has no segments — nothing to file".into());
        }
        if self.speakers.is_empty() {
            return Err("transcript has no speakers — diarization output required".into());
        }
        if ended < started {
            return Err("ended_at is before started_at".into());
        }
        for item in &self.action_items {
            if item.title.trim().is_empty() {
                return Err("action item title is empty".into());
            }
        }
        for seg in &self.segments {
            if !self.speakers.iter().any(|s| s.id == seg.speaker) {
                return Err(format!(
                    "segment references unknown speaker '{}' (declared: {})",
                    seg.speaker,
                    self.speakers.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
        }
        Ok(())
    }

    /// The display name for a diarization label: the mapped name, else "Speaker N"
    /// numbered by the speaker's position in the payload.
    fn speaker_name(&self, id: &str) -> String {
        match self.speakers.iter().position(|s| s.id == id) {
            Some(index) => match self.speakers[index].name.as_deref().map(str::trim) {
                Some(name) if !name.is_empty() => name.to_owned(),
                _ => format!("Speaker {}", index + 1),
            },
            None => id.to_owned(),
        }
    }
}

fn parse(stamp: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(stamp, &Rfc3339)
        .ok()
        .map(|at| at.to_offset(time::UtcOffset::UTC))
}

// ---- naming ----------------------------------------------------------------------

/// An empty title reads as the kind: an untitled note is a "note", the way an
/// untitled meeting has always been a "meeting".
pub fn display_title<'a>(title: &'a str, kind: &'a str) -> &'a str {
    let title = title.trim();
    if title.is_empty() { kind } else { title }
}

/// `2026-06-12-1500-weekly-sync` plus its path.
///
/// With a `source` the id ends in the first 8 hex of its sha256 and `n` is ignored:
/// the same source always names the same file, which is what makes a repeated put
/// idempotent. Without one, `n` is the collision suffix — 1 is the bare id, 2 appends
/// `-2` — so a same-minute same-title item never overwrites the earlier one.
pub fn candidate(kind: &str, payload: &Payload, n: usize) -> (String, String) {
    let at = parse(payload.at()).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let mut id = format!(
        "{:04}-{:02}-{:02}-{:02}{:02}-{}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        slug_for(payload.raw_title(), kind)
    );
    match payload.source() {
        Some(source) => id.push_str(&format!("-{}", source_hash(source))),
        None if n > 1 => id.push_str(&format!("-{n}")),
        None => {}
    }
    let path = format!(
        "{CONTEXT_DIR}/{kind}/{:04}/{:02}/{id}.md",
        at.year(),
        u8::from(at.month())
    );
    (id, path)
}

/// The first 8 hex of `sha256(source)`. Long enough that two of a person's sources
/// will not collide, short enough to read in a filename.
pub fn source_hash(source: &str) -> String {
    let digest = Sha256::digest(source.as_bytes());
    digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect()
}

/// The slug part of an id, falling back to the kind for a title that has no
/// alphanumerics in it at all.
fn slug_for(title: &str, kind: &str) -> String {
    let slug = slugify(display_title(title, kind));
    if slug.is_empty() { kind.to_owned() } else { slug }
}

/// Lowercase, non-alphanumerics collapsed to single dashes, capped at 60 characters.
/// Empty when the title has nothing to slug; callers supply the fallback.
pub fn slugify(title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    slug.chars().take(60).collect()
}

// ---- rendering -------------------------------------------------------------------

/// The filed document: front matter an agent can read without parsing prose, then the
/// body. A meeting's body is the action items and the turns; everything else's is the
/// text verbatim under its title.
pub fn render_markdown(kind: &str, id: &str, ingested_at: &str, payload: &Payload) -> String {
    let title = display_title(payload.raw_title(), kind);
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("kind: {}\n", yaml_str(kind)));
    out.push_str(&format!("id: {}\n", yaml_str(id)));
    out.push_str(&format!("title: {}\n", yaml_str(title)));
    out.push_str(&format!("at: {}\n", yaml_str(payload.at())));
    out.push_str(&format!("ingested_at: {}\n", yaml_str(ingested_at)));
    if let Some(source) = payload.source() {
        out.push_str(&format!("source: {}\n", yaml_str(source)));
    }
    if let Payload::Meeting(meeting) = payload {
        out.push_str(&format!("ended_at: {}\n", yaml_str(&meeting.ended_at)));
        if let Some(provider) = &meeting.provider {
            out.push_str(&format!("provider: {}\n", yaml_str(provider)));
        }
        out.push_str(&format!("speakers_confirmed: {}\n", meeting.speakers_confirmed));
        if let Some(event) = &meeting.calendar_event {
            out.push_str(&format!("calendar_event_id: {}\n", yaml_str(&event.id)));
            let labels: Vec<String> = event
                .attendees
                .iter()
                .filter_map(|a| match (a.name.as_deref(), a.email.as_deref()) {
                    (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
                    (Some(name), None) => Some(name.to_owned()),
                    (None, Some(email)) => Some(email.to_owned()),
                    (None, None) => None,
                })
                .collect();
            if !labels.is_empty() {
                out.push_str("attendees:\n");
                for label in labels {
                    out.push_str(&format!("  - {}\n", yaml_str(&label)));
                }
            }
        }
    }
    // The digest fills these: the entity slugs this item resolved to, and whether it
    // has been read at all.
    out.push_str("entities: []\n");
    out.push_str("digested: false\n");
    out.push_str("---\n\n");

    out.push_str(&format!("# {title}\n\n"));

    match payload {
        Payload::Item(item) => {
            out.push_str(item.text.trim_end());
            out.push('\n');
        }
        Payload::Meeting(meeting) => render_meeting_body(&mut out, meeting),
    }
    out
}

fn render_meeting_body(out: &mut String, payload: &TranscriptPayload) {
    if !payload.action_items.is_empty() {
        out.push_str("## Action items\n\n");
        for item in &payload.action_items {
            let mut line = format!("- [ ] {}", item.title.trim());
            match (item.owner.as_deref(), item.due.as_deref()) {
                (Some(owner), Some(due)) => line.push_str(&format!(" ({owner}, {due})")),
                (Some(owner), None) => line.push_str(&format!(" ({owner})")),
                (None, Some(due)) => line.push_str(&format!(" ({due})")),
                (None, None) => {}
            }
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }

    out.push_str("## Transcript\n\n");
    // One line per turn: consecutive segments by the same speaker are one paragraph,
    // stamped at the moment that speaker started talking.
    let mut turns: Vec<(String, u64, String)> = Vec::new();
    for seg in &payload.segments {
        let text = seg.text.trim();
        let continues = turns.last().is_some_and(|(speaker, _, _)| *speaker == seg.speaker);
        if continues {
            let body = &mut turns.last_mut().expect("a last turn exists").2;
            if !text.is_empty() {
                if !body.is_empty() {
                    body.push(' ');
                }
                body.push_str(text);
            }
        } else {
            turns.push((seg.speaker.clone(), seg.start_ms, text.to_owned()));
        }
    }
    for (speaker, start_ms, body) in &turns {
        out.push_str(&format!(
            "**{}** [{}]: {body}\n\n",
            payload.speaker_name(speaker),
            offset(*start_ms)
        ));
    }
}

/// `mm:ss` into the meeting. Minutes keep counting past 59 rather than rolling over.
fn offset(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// Front-matter scalars are always double-quoted, so a title with `:` or `#` can
/// never break the parse.
///
/// Escaped by YAML's double-quoted rules, not by Rust's `{:?}`. Rust's Debug spells a
/// combining accent as `\u{301}` — braces and all — which YAML does not accept, so an
/// accented subject in NFD form used to produce front matter that would not parse. A
/// file whose front matter will not parse loses its `ingested_at`, which makes its
/// list cursor `|kind/id` and hides it behind every poller's first page.
///
/// Only what has to be escaped is: backslash, the closing quote, and the control
/// characters. Every other character — accents, CJK, emoji — travels verbatim, which
/// is both legal YAML and readable in a diff.
fn yaml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // C0 and DEL.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            // C1, and the two separators YAML reads as line breaks.
            c if (0x80..=0x9f).contains(&(c as u32))
                || c == '\u{2028}'
                || c == '\u{2029}' =>
            {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- reading back ----------------------------------------------------------------

/// The front matter of a filed item, as the list and get endpoints report it.
///
/// Everything is optional because the file on disk is the record and a hand-edited
/// one must still list rather than take the endpoint down. A missing field reads as
/// its empty value.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Front {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub ingested_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub digested: bool,
    #[serde(default)]
    pub entities: Vec<String>,
}

/// Split a filed item into its front matter and its body.
///
/// A file whose front matter will not parse still answers, with whatever the path
/// says: `context/<kind>/YYYY/MM/<id>.md` carries the kind and the id, so a broken
/// block loses the title and the dates, not the item.
pub fn read(path: &str, source: &str) -> (Front, String) {
    let (block, body) = crate::docs::split_front_matter(source);
    let mut front = block
        .and_then(|block| serde_yaml_bw::from_str::<Front>(block).ok())
        .unwrap_or_default();
    if front.kind.is_empty() {
        front.kind = kind_of(path).unwrap_or_default();
    }
    if front.id.is_empty() {
        front.id = id_of(path).unwrap_or_default();
    }
    (front, body.trim_start_matches('\n').to_owned())
}

/// `context/<kind>/YYYY/MM/<id>.md` -> `<kind>`.
pub fn kind_of(path: &str) -> Option<String> {
    let rest = path.strip_prefix(CONTEXT_DIR)?.strip_prefix('/')?;
    let kind = rest.split('/').next()?;
    known_kind(kind).then(|| kind.to_owned())
}

/// `context/<kind>/YYYY/MM/<id>.md` -> `<id>`.
pub fn id_of(path: &str) -> Option<String> {
    path.rsplit('/').next()?.strip_suffix(".md").map(str::to_owned)
}

/// The opaque list cursor: `<ingested_at>|<kind>/<id>`.
///
/// Compared as a plain string, which is the same order as the tuple
/// `(ingested_at, kind, id)`: `ingested_at` is the fixed-width UTC spelling the whole
/// tree uses, and no kind is a prefix of another.
pub fn cursor(front: &Front) -> String {
    format!("{}|{}/{}", front.ingested_at, front.kind, front.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript() -> TranscriptPayload {
        TranscriptPayload {
            title: "Weekly Sync: Rob & Matthias".into(),
            started_at: "2026-06-12T15:00:00Z".into(),
            ended_at: "2026-06-12T15:30:00Z".into(),
            meeting_url: Some("https://meet.google.com/abc-defg-hij".into()),
            provider: Some("grok-batch".into()),
            speakers: vec![
                Speaker {
                    id: "S1".into(),
                    name: Some("Matthias".into()),
                    channel: Some("mic".into()),
                },
                Speaker { id: "S2".into(), name: None, channel: Some("tab".into()) },
            ],
            speakers_confirmed: false,
            calendar_event: Some(CalendarEvent {
                id: "evt123".into(),
                title: Some("Weekly Sync".into()),
                attendees: vec![Attendee {
                    name: Some("Jane Doe".into()),
                    email: Some("jane@acme.com".into()),
                }],
            }),
            action_items: vec![ActionItem {
                title: "Send the follow-up deck".into(),
                owner: Some("Matthias".into()),
                due: Some("2026-06-15".into()),
                source: None,
            }],
            segments: vec![
                Segment {
                    speaker: "S1".into(),
                    start_ms: 5000,
                    end_ms: 9000,
                    text: "Morning everyone.".into(),
                },
                Segment {
                    speaker: "S1".into(),
                    start_ms: 9000,
                    end_ms: 11000,
                    text: "Let's start.".into(),
                },
                Segment {
                    speaker: "S2".into(),
                    start_ms: 65_500,
                    end_ms: 68_000,
                    text: "Hey!".into(),
                },
            ],
        }
    }

    fn meeting() -> Payload {
        Payload::Meeting(transcript())
    }

    fn email() -> Payload {
        Payload::Item(ItemPayload {
            title: "Re: invoice".into(),
            at: "2026-06-12T15:00:00Z".into(),
            text: "The invoice is paid.\n".into(),
            source: Some("18f2a".into()),
        })
    }

    #[test]
    fn a_meeting_id_is_date_time_slug_and_the_source_hash() {
        let (id, path) = candidate("meeting", &meeting(), 1);
        let hash = source_hash("https://meet.google.com/abc-defg-hij");
        assert_eq!(id, format!("2026-06-12-1500-weekly-sync-rob-matthias-{hash}"));
        assert_eq!(path, format!("context/meeting/2026/06/{id}.md"));
        assert_eq!(hash.len(), 8);
    }

    #[test]
    fn a_sourced_id_ignores_the_collision_counter() {
        // The same source always names the same file; that is what makes a repeated
        // put idempotent rather than a second copy.
        assert_eq!(candidate("meeting", &meeting(), 1), candidate("meeting", &meeting(), 7));
    }

    #[test]
    fn a_collision_bumps_a_numeric_suffix_when_there_is_no_source() {
        let mut bare = transcript();
        bare.meeting_url = None;
        let bare = Payload::Meeting(bare);
        assert_eq!(candidate("meeting", &bare, 1).0, "2026-06-12-1500-weekly-sync-rob-matthias");
        let (id, path) = candidate("meeting", &bare, 2);
        assert_eq!(id, "2026-06-12-1500-weekly-sync-rob-matthias-2");
        assert!(path.ends_with("-2.md"), "{path}");
    }

    #[test]
    fn the_id_is_utc_whatever_offset_arrives() {
        let mut p = transcript();
        p.started_at = "2026-06-12T11:00:00-04:00".into();
        p.meeting_url = None;
        assert_eq!(
            candidate("meeting", &Payload::Meeting(p), 1).0,
            "2026-06-12-1500-weekly-sync-rob-matthias"
        );
    }

    #[test]
    fn an_email_lands_under_its_own_kind() {
        let (id, path) = candidate("email", &email(), 1);
        assert!(id.starts_with("2026-06-12-1500-re-invoice-"), "{id}");
        assert_eq!(path, format!("context/email/2026/06/{id}.md"));
    }

    #[test]
    fn slug_falls_back_to_the_kind_for_a_title_with_nothing_in_it() {
        assert_eq!(slugify("Q3 Plan / Review"), "q3-plan-review");
        assert_eq!(slugify(""), "");
        assert_eq!(slug_for("", "note"), "note");
        assert_eq!(slug_for("!!!", "email"), "email");
        assert_eq!(slug_for("", "meeting"), "meeting");
    }

    #[test]
    fn markdown_has_front_matter_action_items_and_merged_turns() {
        let payload = meeting();
        let (id, _) = candidate("meeting", &payload, 1);
        let md = render_markdown("meeting", &id, "2026-06-12T15:31:00.000000Z", &payload);

        assert!(md.starts_with("---\n"));
        assert!(md.contains("kind: \"meeting\""));
        assert!(md.contains(&format!("id: {id:?}")));
        assert!(md.contains("title: \"Weekly Sync: Rob & Matthias\""));
        assert!(md.contains("at: \"2026-06-12T15:00:00Z\""));
        assert!(md.contains("ingested_at: \"2026-06-12T15:31:00.000000Z\""));
        assert!(md.contains("source: \"https://meet.google.com/abc-defg-hij\""));
        assert!(md.contains("ended_at: \"2026-06-12T15:30:00Z\""));
        assert!(md.contains("speakers_confirmed: false"));
        assert!(md.contains("calendar_event_id: \"evt123\""));
        assert!(md.contains("  - \"Jane Doe <jane@acme.com>\""));
        assert!(md.contains("entities: []"));
        assert!(md.contains("digested: false"));
        assert!(md.contains("# Weekly Sync: Rob & Matthias"));

        assert!(md.contains("## Action items"));
        assert!(md.contains("- [ ] Send the follow-up deck (Matthias, 2026-06-15)"));

        assert!(md.contains("## Transcript"));
        // Two consecutive S1 segments become one paragraph, stamped at the first.
        assert!(md.contains("**Matthias** [00:05]: Morning everyone. Let's start."), "{md}");
        // An unmapped speaker is numbered by position, and minutes pass 59 seconds.
        assert!(md.contains("**Speaker 2** [01:05]: Hey!"), "{md}");
        // The old spellings are gone, not kept beside the new ones.
        assert!(!md.contains("started_at:"), "{md}");
        assert!(!md.contains("meeting_url:"), "{md}");
    }

    #[test]
    fn a_payload_without_a_calendar_event_omits_those_keys() {
        let mut p = transcript();
        p.calendar_event = None;
        p.meeting_url = None;
        p.provider = None;
        p.action_items.clear();
        let md = render_markdown("meeting", "x", "2026-06-12T15:31:00.000000Z", &Payload::Meeting(p));
        assert!(!md.contains("calendar_event_id"));
        assert!(!md.contains("attendees:"));
        assert!(!md.contains("source:"));
        assert!(!md.contains("## Action items"));
    }

    #[test]
    fn a_note_body_is_its_text_verbatim_under_its_title() {
        let note = Payload::Item(ItemPayload {
            title: String::new(),
            at: "2026-06-12T15:00:00Z".into(),
            text: "  line one\n  line two\n".into(),
            source: None,
        });
        let md = render_markdown("note", "x", "2026-06-12T15:31:00.000000Z", &note);
        assert!(md.contains("kind: \"note\""));
        assert!(md.contains("title: \"note\""));
        assert!(md.ends_with("# note\n\n  line one\n  line two\n"), "{md}");
    }

    #[test]
    fn validation_rejects_the_unfilable() {
        assert!(meeting().validate().is_ok());

        let mut p = transcript();
        p.segments[0].speaker = "S9".into();
        assert!(p.validate().unwrap_err().contains("unknown speaker 'S9'"));

        let mut p = transcript();
        p.segments.clear();
        assert!(p.validate().unwrap_err().contains("no segments"));

        let mut p = transcript();
        p.speakers.clear();
        assert!(p.validate().unwrap_err().contains("no speakers"));

        let mut p = transcript();
        p.ended_at = "2026-06-12T14:00:00Z".into();
        assert_eq!(p.validate().unwrap_err(), "ended_at is before started_at");

        let mut p = transcript();
        p.started_at = "yesterday".into();
        assert!(p.validate().unwrap_err().contains("RFC3339"));

        let mut p = transcript();
        p.action_items[0].title = "  ".into();
        assert!(p.validate().unwrap_err().contains("action item title is empty"));
    }

    #[test]
    fn an_item_needs_a_time_and_some_text() {
        assert!(email().validate().is_ok());

        let empty = Payload::Item(ItemPayload {
            title: "x".into(),
            at: "2026-06-12T15:00:00Z".into(),
            text: "   \n".into(),
            source: None,
        });
        assert!(empty.validate().unwrap_err().contains("text is empty"));

        let undated = Payload::Item(ItemPayload {
            title: "x".into(),
            at: "yesterday".into(),
            text: "words".into(),
            source: None,
        });
        assert!(undated.validate().unwrap_err().contains("RFC3339"));
    }

    #[test]
    fn a_body_that_is_not_the_kinds_shape_is_a_reason_not_a_panic() {
        let err = Payload::parse("email", b"{\"title\":\"x\"}").unwrap_err();
        assert!(err.contains("not a email item"), "{err}");
        let err = Payload::parse("meeting", b"nonsense").unwrap_err();
        assert!(err.contains("not a meeting transcript"), "{err}");
    }

    #[test]
    fn known_kinds_are_the_four_and_nothing_else() {
        assert!(KINDS.iter().all(|k| known_kind(k)));
        assert!(!known_kind("meetings"));
        assert!(!known_kind(""));
    }

    #[test]
    fn a_filed_item_reads_back_as_front_matter_plus_body() {
        let payload = email();
        let (id, path) = candidate("email", &payload, 1);
        let md = render_markdown("email", &id, "2026-06-12T15:31:00.000000Z", &payload);
        let (front, body) = read(&path, &md);
        assert_eq!(front.kind, "email");
        assert_eq!(front.id, id);
        assert_eq!(front.title, "Re: invoice");
        assert_eq!(front.at, "2026-06-12T15:00:00Z");
        assert_eq!(front.ingested_at, "2026-06-12T15:31:00.000000Z");
        assert_eq!(front.source.as_deref(), Some("18f2a"));
        assert!(!front.digested);
        assert!(front.entities.is_empty());
        assert_eq!(body, "# Re: invoice\n\nThe invoice is paid.\n");
    }

    #[test]
    fn a_file_whose_front_matter_will_not_parse_still_names_itself_from_its_path() {
        let path = "context/note/2026/06/2026-06-12-1500-scratch.md";
        let (front, body) = read(path, "---\n: : :\n---\n\n# Scratch\n");
        assert_eq!(front.kind, "note");
        assert_eq!(front.id, "2026-06-12-1500-scratch");
        assert_eq!(body, "# Scratch\n");
    }

    /// A subject is a stranger's text. Whatever is in it, the front matter it lands in
    /// has to parse — a block that does not costs the item its `ingested_at`, and an
    /// item with no `ingested_at` sorts to the front of the cursor and is never seen
    /// again after the first page.
    #[test]
    fn a_hostile_title_still_round_trips_through_the_front_matter() {
        // An accent in NFD form (`e` + U+0301), a bell, a DEL, a tab, a newline, a
        // quote, a backslash, a colon, a `#`, and a line separator.
        let title = "Cafe\u{301} re\u{301}sume\u{301} \u{7}\u{7f}\ttwo\nlines \"q\" \\b: #x\u{2028}z";
        let payload = Payload::Item(ItemPayload {
            title: title.to_owned(),
            at: "2026-06-12T15:00:00Z".into(),
            text: "body".into(),
            source: Some("18f2a".into()),
        });
        let (id, path) = candidate("email", &payload, 1);
        let md = render_markdown("email", &id, "2026-06-12T15:31:00.000000Z", &payload);

        // The block parses, and every field survives — including the one the cursor
        // rests on.
        let (front, body) = read(&path, &md);
        assert_eq!(front.title, title, "front matter did not round-trip: {md}");
        assert_eq!(front.ingested_at, "2026-06-12T15:31:00.000000Z", "{md}");
        assert_eq!(front.kind, "email");
        assert_eq!(front.id, id);
        assert_eq!(front.source.as_deref(), Some("18f2a"));
        assert!(body.starts_with("# Cafe"), "{body}");
        assert_eq!(cursor(&front), format!("2026-06-12T15:31:00.000000Z|email/{id}"));

        // Rust's Debug spelling would have leaked into the file. YAML's has not.
        assert!(!md.contains("\\u{"), "{md}");
        assert!(md.contains("\\x07"), "{md}");
        assert!(md.contains("\\u2028"), "{md}");
        assert!(md.contains("\\t"), "{md}");
        assert!(md.contains("\\n"), "{md}");
        assert!(md.contains("\\\"q\\\""), "{md}");

        // And the split itself still finds a block at all.
        let (block, _) = crate::docs::split_front_matter(&md);
        assert!(block.is_some_and(|b| b.contains("title: ")), "{md}");
    }

    #[test]
    fn the_cursor_orders_by_ingest_then_kind_then_id() {
        let at = |ingested: &str, kind: &str, id: &str| {
            cursor(&Front {
                ingested_at: ingested.to_owned(),
                kind: kind.to_owned(),
                id: id.to_owned(),
                ..Front::default()
            })
        };
        let first = at("2026-06-12T15:31:00.000000Z", "email", "a");
        let same_ms_other_kind = at("2026-06-12T15:31:00.000000Z", "meeting", "a");
        let later = at("2026-06-12T15:31:00.000001Z", "chat", "a");
        assert!(first < same_ms_other_kind);
        assert!(same_ms_other_kind < later);
        assert_eq!(first, "2026-06-12T15:31:00.000000Z|email/a");
    }
}
