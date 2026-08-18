//! Traces: agent sessions as first-class artifacts.
//!
//! A trace is one agent run — its prompts, tool calls, and the commits it produced.
//! Events live in SQLite; raw transcripts live as files under `$NASHGIT_TRACES`. The
//! link to git is a commit SHA, attributed automatically: every event carries the
//! repo's `HEAD` at the moment it happened, and when `HEAD` moves between two events
//! of a session, the commits in between belong to that session.
//!
//! Traces live here rather than in git. They are large and append-heavy, and
//! committing them would bloat every clone. Plans and cards are git-native because
//! they are small and human-edited; a transcript is neither.

use std::path::PathBuf;

use serde::Deserialize;

use crate::config::Config;
use crate::db::{Db, NewTraceEvent};
use crate::git::Repo;

/// One event in a POST batch.
#[derive(Debug, Clone, Deserialize)]
pub struct EventIn {
    /// Position in the session. Batches set it so a retried POST stores one copy;
    /// live hook events omit it and the server assigns the next number.
    #[serde(default)]
    pub seq: Option<i64>,
    pub kind: String,
    /// The event body, stored verbatim.
    #[serde(default)]
    pub payload: serde_json::Value,
    /// The agent's repo `HEAD` when this happened.
    #[serde(default)]
    pub head: Option<String>,
}

/// The `POST /{repo}/traces/events` body.
#[derive(Debug, Clone, Deserialize)]
pub struct BatchIn {
    pub session: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub events: Vec<EventIn>,
}

/// What recording a batch did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BatchOutcome {
    pub stored: usize,
    pub duplicates: usize,
    pub attributed_commits: Vec<String>,
}

/// A session id is used in URLs and filenames; keep it boring.
pub fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !session.starts_with('.')
}

fn plausible_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Record a batch of events, attributing any commits `HEAD` passed over.
pub async fn record_batch(
    db: &Db,
    mirror: &Repo,
    repo: &str,
    batch: &BatchIn,
) -> Result<BatchOutcome, rusqlite::Error> {
    let mut stored = 0usize;
    let mut duplicates = 0usize;
    let mut attributed: Vec<String> = Vec::new();

    for event in &batch.events {
        let previous = db.trace_last_head(repo, &batch.session)?;

        let head = event.head.clone().filter(|h| plausible_sha(h));
        let new = NewTraceEvent {
            repo: repo.to_owned(),
            session: batch.session.clone(),
            seq: event.seq,
            kind: event.kind.clone(),
            payload: serde_json::to_string(&event.payload).unwrap_or_else(|_| "null".into()),
            head: head.clone(),
            agent: batch.agent.clone(),
        };
        if !db.add_trace_event(&new)? {
            duplicates += 1;
            continue;
        }
        stored += 1;

        // HEAD moved since the session's previous event: those commits are this
        // session's work.
        if let (Some(previous), Some(head)) = (previous, head)
            && previous != head
        {
            let commits = commits_between(mirror, &previous, &head).await;
            db.attribute_commits(repo, &batch.session, &commits)?;
            attributed.extend(commits);
        }
    }

    Ok(BatchOutcome { stored, duplicates, attributed_commits: attributed })
}

/// The commits `old..new`, asked of the mirror; when the mirror does not (yet) know
/// them, the new head alone is attributed — the link stays even if the push comes
/// later.
async fn commits_between(mirror: &Repo, old: &str, new: &str) -> Vec<String> {
    if let Ok(out) = mirror
        .run(&["rev-list", "--max-count=100", "--end-of-options", &format!("{old}..{new}")])
        .await
    {
        let shas: Vec<String> =
            out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
        if !shas.is_empty() {
            return shas;
        }
    }
    vec![new.to_owned()]
}

/// Where a session's raw transcript lives.
pub fn transcript_path(config: &Config, repo: &str, session: &str) -> PathBuf {
    config.traces.join(repo).join(format!("{session}.transcript"))
}

/// Store the raw transcript, verbatim.
pub fn store_transcript(
    config: &Config,
    repo: &str,
    session: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let path = transcript_path(config, repo, session);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// A one-line human summary of an event payload, for the session page.
pub fn summarize(kind: &str, payload: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|key| {
            value
                .get(key)
                .and_then(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    other if !other.is_null() => Some(other.to_string()),
                    _ => None,
                })
                .filter(|s| !s.trim().is_empty())
        })
    };
    let text = pick(&["prompt", "message", "tool_name", "text", "content", "command"])
        .unwrap_or_else(|| {
            let compact = value.to_string();
            if compact == "null" { String::new() } else { compact }
        });
    let mut summary: String = text.chars().take(200).collect();
    if summary.chars().count() < text.chars().count() {
        summary.push('…');
    }
    if summary.is_empty() { kind.to_owned() } else { summary }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_url_and_filename_safe() {
        assert!(valid_session("sess-01HXYZ.2"));
        assert!(!valid_session(""));
        assert!(!valid_session("../etc/passwd"));
        assert!(!valid_session("a/b"));
        assert!(!valid_session(".hidden"));
    }

    #[test]
    fn summaries_prefer_human_fields_and_truncate() {
        let long = format!("{{\"prompt\": \"{}\"}}", "x".repeat(500));
        let summary = summarize("UserPromptSubmit", &long);
        assert!(summary.chars().count() <= 201);
        assert!(summary.ends_with('…'));
        assert_eq!(summarize("Stop", "{\"tool_name\": \"Bash\"}"), "Bash");
        assert_eq!(summarize("Stop", "null"), "Stop");
    }
}
