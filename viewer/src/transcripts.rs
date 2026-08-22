//! Meeting transcripts: a finished recording becomes one committed markdown file
//! at `transcripts/YYYY/MM/<id>.md` on the default branch.
//!
//! The wire types match the browser extension's payload field for field, so the
//! same extension files into a nashcode repo and into anything else that speaks
//! this shape. Timestamps travel as RFC3339 strings and land in the front matter
//! verbatim; only the id and the path need them parsed.

use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const TRANSCRIPTS_DIR: &str = "transcripts";

#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptPayload {
    /// Meeting title — the calendar event title when one was found, else whatever
    /// the user typed. May be empty; the id and the heading fall back to "meeting".
    #[serde(default)]
    pub title: String,
    pub started_at: String,
    pub ended_at: String,
    /// Meeting page URL (meet.google.com/…, zoom.us/…) — provenance only.
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

    fn title(&self) -> &str {
        let title = self.title.trim();
        if title.is_empty() { "meeting" } else { title }
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

/// `2026-06-12-1500-weekly-sync` plus its path. `n` is the collision suffix: 1 is the
/// bare id, 2 appends `-2`, and so on, so a same-minute same-title meeting never
/// overwrites the earlier one.
pub fn candidate(payload: &TranscriptPayload, n: usize) -> (String, String) {
    let at = parse(&payload.started_at).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let base = format!(
        "{:04}-{:02}-{:02}-{:02}{:02}-{}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        slugify(payload.title())
    );
    let id = if n <= 1 { base } else { format!("{base}-{n}") };
    let path = format!(
        "{TRANSCRIPTS_DIR}/{:04}/{:02}/{id}.md",
        at.year(),
        u8::from(at.month())
    );
    (id, path)
}

pub fn slugify(title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if slug.is_empty() {
        "meeting".to_owned()
    } else {
        slug.chars().take(60).collect()
    }
}

/// The filed document: front matter an agent can read without parsing prose, then the
/// action items, then the turns. Consecutive turns by one speaker merge into one line.
pub fn render_markdown(id: &str, payload: &TranscriptPayload) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {}\n", yaml_str(id)));
    out.push_str(&format!("title: {}\n", yaml_str(payload.title())));
    out.push_str(&format!("started_at: {}\n", yaml_str(&payload.started_at)));
    out.push_str(&format!("ended_at: {}\n", yaml_str(&payload.ended_at)));
    if let Some(url) = &payload.meeting_url {
        out.push_str(&format!("meeting_url: {}\n", yaml_str(url)));
    }
    if let Some(provider) = &payload.provider {
        out.push_str(&format!("provider: {}\n", yaml_str(provider)));
    }
    out.push_str(&format!("speakers_confirmed: {}\n", payload.speakers_confirmed));
    if let Some(event) = &payload.calendar_event {
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
    // The digest pass that reads these files flips this when it has processed one.
    out.push_str("digested: false\n");
    out.push_str("---\n\n");

    out.push_str(&format!("# {}\n\n", payload.title()));

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
    out
}

/// `mm:ss` into the meeting. Minutes keep counting past 59 rather than rolling over.
fn offset(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// Front-matter scalars are always double-quoted, so a title with `:` or `#` can
/// never break the parse.
fn yaml_str(s: &str) -> String {
    format!("{s:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> TranscriptPayload {
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

    #[test]
    fn id_is_date_time_slug_and_the_path_is_year_month() {
        let (id, path) = candidate(&payload(), 1);
        assert_eq!(id, "2026-06-12-1500-weekly-sync-rob-matthias");
        assert_eq!(path, "transcripts/2026/06/2026-06-12-1500-weekly-sync-rob-matthias.md");
    }

    #[test]
    fn a_collision_bumps_a_numeric_suffix() {
        let (id, path) = candidate(&payload(), 2);
        assert_eq!(id, "2026-06-12-1500-weekly-sync-rob-matthias-2");
        assert!(path.ends_with("-2.md"), "{path}");
    }

    #[test]
    fn the_id_is_utc_whatever_offset_arrives() {
        let mut p = payload();
        p.started_at = "2026-06-12T11:00:00-04:00".into();
        assert_eq!(candidate(&p, 1).0, "2026-06-12-1500-weekly-sync-rob-matthias");
    }

    #[test]
    fn slug_falls_back_for_an_empty_title() {
        assert_eq!(slugify(""), "meeting");
        assert_eq!(slugify("!!!"), "meeting");
        assert_eq!(slugify("Q3 Plan / Review"), "q3-plan-review");
    }

    #[test]
    fn markdown_has_front_matter_action_items_and_merged_turns() {
        let payload = payload();
        let (id, _) = candidate(&payload, 1);
        let md = render_markdown(&id, &payload);

        assert!(md.starts_with("---\n"));
        assert!(md.contains("id: \"2026-06-12-1500-weekly-sync-rob-matthias\""));
        assert!(md.contains("title: \"Weekly Sync: Rob & Matthias\""));
        assert!(md.contains("started_at: \"2026-06-12T15:00:00Z\""));
        assert!(md.contains("speakers_confirmed: false"));
        assert!(md.contains("calendar_event_id: \"evt123\""));
        assert!(md.contains("  - \"Jane Doe <jane@acme.com>\""));
        assert!(md.contains("digested: false"));
        assert!(md.contains("# Weekly Sync: Rob & Matthias"));

        assert!(md.contains("## Action items"));
        assert!(md.contains("- [ ] Send the follow-up deck (Matthias, 2026-06-15)"));

        assert!(md.contains("## Transcript"));
        // Two consecutive S1 segments become one paragraph, stamped at the first.
        assert!(md.contains("**Matthias** [00:05]: Morning everyone. Let's start."), "{md}");
        // An unmapped speaker is numbered by position, and minutes pass 59 seconds.
        assert!(md.contains("**Speaker 2** [01:05]: Hey!"), "{md}");
    }

    #[test]
    fn a_payload_without_a_calendar_event_omits_those_keys() {
        let mut p = payload();
        p.calendar_event = None;
        p.meeting_url = None;
        p.provider = None;
        p.action_items.clear();
        let md = render_markdown("x", &p);
        assert!(!md.contains("calendar_event_id"));
        assert!(!md.contains("attendees:"));
        assert!(!md.contains("meeting_url"));
        assert!(!md.contains("## Action items"));
    }

    #[test]
    fn validation_rejects_the_unfilable() {
        assert!(payload().validate().is_ok());

        let mut p = payload();
        p.segments[0].speaker = "S9".into();
        assert!(p.validate().unwrap_err().contains("unknown speaker 'S9'"));

        let mut p = payload();
        p.segments.clear();
        assert!(p.validate().unwrap_err().contains("no segments"));

        let mut p = payload();
        p.speakers.clear();
        assert!(p.validate().unwrap_err().contains("no speakers"));

        let mut p = payload();
        p.ended_at = "2026-06-12T14:00:00Z".into();
        assert_eq!(p.validate().unwrap_err(), "ended_at is before started_at");

        let mut p = payload();
        p.started_at = "yesterday".into();
        assert!(p.validate().unwrap_err().contains("RFC3339"));

        let mut p = payload();
        p.action_items[0].title = "  ".into();
        assert!(p.validate().unwrap_err().contains("action item title is empty"));
    }
}
