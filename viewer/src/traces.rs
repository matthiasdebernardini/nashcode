//! Turning agent hook events into a trace, and a trace into commits.
//!
//! The transcript and the diff are the same artifact seen from two sides. A commit says
//! what changed; the trace that produced it says why, and what was tried first.
//!
//! The link needs nothing from the agent. Every event carries the repo's `HEAD` at the
//! moment it happened, so when `HEAD` moves between two events of a session, the commits
//! in between belong to that session. No commit trailer to remember, no naming
//! convention to get wrong, no cooperation that can be skipped.
//!
//! Storage lives in nashcode rather than in git. Transcripts are large and append-heavy;
//! committing them would bloat every clone. Plans and cards are git-native because they
//! are small and human-edited. A transcript is neither.

use serde::{Deserialize, Serialize};

use crate::db::{Db, DbResult, NewTraceEvent};
use crate::git::Repo;

/// One moment in an agent run, as the CLI posts it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncomingEvent {
    /// The event's place in the session. Supplied by the client so a retried batch
    /// stores one copy; omitted for a live single event, which appends.
    #[serde(default)]
    pub seq: Option<i64>,
    /// What happened: `prompt`, `tool`, `result`, `stop`, whatever the harness calls it.
    #[serde(default)]
    pub kind: String,
    /// The repo `HEAD` when this happened. This is the whole linking mechanism.
    #[serde(default)]
    pub head: Option<String>,
    /// The harness payload, kept verbatim.
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// A batch of events for one session.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceBatch {
    pub session: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub events: Vec<IncomingEvent>,
}

/// What an ingest actually did, so the API can answer honestly.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Ingested {
    pub stored: usize,
    pub duplicates: usize,
    /// Commits newly attributed to this session.
    pub commits: Vec<String>,
}

/// Record a batch and attribute any commit the session's `HEAD` moved onto.
///
/// `repo_handle` is the mirror, used only to expand a `HEAD` jump into the individual
/// commits between the two points. When it cannot answer (the work is not pushed yet,
/// which is the common case) the destination commit alone is attributed.
pub async fn ingest(
    db: &Db,
    repo: &str,
    batch: &TraceBatch,
    repo_handle: Option<&Repo>,
) -> DbResult<Ingested> {
    let mut result = Ingested::default();
    let mut previous = db.trace_last_head(repo, &batch.session)?;

    for event in &batch.events {
        let payload = if event.payload.is_null() {
            "{}".to_owned()
        } else {
            event.payload.to_string()
        };

        let stored = db.add_trace_event(&NewTraceEvent {
            repo: repo.to_owned(),
            session: batch.session.clone(),
            seq: event.seq,
            kind: if event.kind.is_empty() { "event".to_owned() } else { event.kind.clone() },
            payload,
            head: event.head.clone(),
            agent: batch.agent.clone(),
        })?;

        if !stored {
            result.duplicates += 1;
            continue;
        }
        result.stored += 1;

        let Some(head) = event.head.as_deref() else { continue };
        let moved = matches!(&previous, Some(before) if before != head);
        if moved {
            let from = previous.as_deref().unwrap_or_default();
            let commits = expand(repo_handle, from, head).await;
            db.attribute_commits(repo, &batch.session, &commits)?;
            result.commits.extend(commits);
        }
        previous = Some(head.to_owned());
    }

    result.commits.dedup();
    Ok(result)
}

/// The commits between two points, newest last. Falls back to just the destination when
/// the mirror does not have the range, which happens whenever the work is still local.
async fn expand(repo: Option<&Repo>, from: &str, to: &str) -> Vec<String> {
    if let Some(repo) = repo
        && let Ok(out) = repo.run(&["rev-list", "--reverse", &format!("{from}..{to}")]).await
    {
        let listed: Vec<String> =
            out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
        if !listed.is_empty() {
            return listed;
        }
    }
    vec![to.to_owned()]
}

/// Where a session's raw transcript is kept.
pub fn transcript_path(root: &std::path::Path, repo: &str, session: &str) -> std::path::PathBuf {
    root.join(repo).join(format!("{}.jsonl", sanitize(session)))
}

/// Session ids come from another program, so they never reach the filesystem raw.
fn sanitize(session: &str) -> String {
    session
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(120)
        .collect()
}

/// A one-line description of an event, for the session page and `trace show`.
pub fn summarize(kind: &str, payload: &serde_json::Value) -> String {
    // Read the payload, not the event name. Harnesses name their hooks differently
    // (`UserPromptSubmit`, `prompt`, `PreToolUse`, `tool`), but they all carry the same
    // handful of fields, so matching on the fields works across all of them.
    let text = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    if !text.is_empty() {
        return clip(text);
    }

    if let Some(tool) = payload.get("tool_name").and_then(|v| v.as_str())
        && !tool.is_empty()
    {
        let input = payload.get("tool_input");
        let arg = |name: &str| input.and_then(|i| i.get(name)).and_then(|v| v.as_str());
        let detail = match tool {
            "Bash" => arg("command").unwrap_or(""),
            _ => arg("file_path").or_else(|| arg("path")).or_else(|| arg("pattern")).unwrap_or(""),
        };
        return if detail.is_empty() {
            clip(tool)
        } else {
            clip(&format!("{tool}: {detail}"))
        };
    }

    // Nothing recognizable: the event name is still worth showing.
    clip(kind)
}

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 120 {
        return flat;
    }
    let short: String = flat.chars().take(117).collect();
    format!("{short}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: i64, kind: &str, head: Option<&str>) -> IncomingEvent {
        IncomingEvent {
            seq: Some(seq),
            kind: kind.to_owned(),
            head: head.map(str::to_owned),
            payload: serde_json::json!({"n": seq}),
        }
    }

    fn batch(session: &str, events: Vec<IncomingEvent>) -> TraceBatch {
        TraceBatch {
            session: session.to_owned(),
            agent: Some("claude-code".to_owned()),
            events,
        }
    }

    #[tokio::test]
    async fn a_head_move_attributes_the_commit_to_the_session() {
        let db = Db::in_memory().unwrap();
        let result = ingest(
            &db,
            "demo",
            &batch(
                "s1",
                vec![
                    event(1, "prompt", Some("aaa")),
                    event(2, "tool", Some("aaa")),
                    // HEAD moved: the session committed.
                    event(3, "tool", Some("bbb")),
                    event(4, "stop", Some("bbb")),
                ],
            ),
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.stored, 4);
        // The first HEAD is where the session started, so it is not attributed.
        assert_eq!(result.commits, vec!["bbb"]);
        assert_eq!(db.trace_session_commits("demo", "s1").unwrap(), vec!["bbb"]);
    }

    #[tokio::test]
    async fn every_commit_in_a_session_is_attributed() {
        let db = Db::in_memory().unwrap();
        ingest(
            &db,
            "demo",
            &batch(
                "s2",
                vec![
                    event(1, "prompt", Some("c0")),
                    event(2, "tool", Some("c1")),
                    event(3, "tool", Some("c2")),
                ],
            ),
            None,
        )
        .await
        .unwrap();
        assert_eq!(db.trace_session_commits("demo", "s2").unwrap(), vec!["c1", "c2"]);
    }

    #[tokio::test]
    async fn the_same_batch_twice_stores_one_copy() {
        let db = Db::in_memory().unwrap();
        let events = vec![event(1, "prompt", Some("aaa")), event(2, "tool", Some("bbb"))];
        let first = ingest(&db, "demo", &batch("s3", events.clone()), None).await.unwrap();
        let second = ingest(&db, "demo", &batch("s3", events), None).await.unwrap();

        assert_eq!(first.stored, 2);
        assert_eq!(second.stored, 0);
        assert_eq!(second.duplicates, 2);
        assert!(second.commits.is_empty(), "no commit attributed twice");
        assert_eq!(db.trace_events("demo", "s3").unwrap().len(), 2);
        assert_eq!(db.trace_session_commits("demo", "s3").unwrap(), vec!["bbb"]);
    }

    #[tokio::test]
    async fn a_later_batch_continues_the_same_session() {
        let db = Db::in_memory().unwrap();
        ingest(&db, "demo", &batch("s4", vec![event(1, "prompt", Some("aaa"))]), None)
            .await
            .unwrap();
        // The move is detected across batches, not only inside one.
        let second = ingest(&db, "demo", &batch("s4", vec![event(2, "tool", Some("bbb"))]), None)
            .await
            .unwrap();
        assert_eq!(second.commits, vec!["bbb"]);
    }

    #[tokio::test]
    async fn events_without_a_head_do_not_break_attribution() {
        let db = Db::in_memory().unwrap();
        let result = ingest(
            &db,
            "demo",
            &batch(
                "s5",
                vec![
                    event(1, "prompt", None),
                    event(2, "tool", Some("aaa")),
                    event(3, "tool", None),
                    event(4, "tool", Some("bbb")),
                ],
            ),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.commits, vec!["bbb"]);
    }

    #[tokio::test]
    async fn a_commit_points_back_at_its_session() {
        let db = Db::in_memory().unwrap();
        ingest(
            &db,
            "demo",
            &batch("s6", vec![event(1, "prompt", Some("aaa")), event(2, "tool", Some("bbb"))]),
            None,
        )
        .await
        .unwrap();
        assert_eq!(db.trace_sessions_for_commit("demo", "bbb").unwrap(), vec!["s6"]);
        assert!(db.trace_sessions_for_commit("demo", "aaa").unwrap().is_empty());
    }

    #[test]
    fn a_session_id_never_reaches_the_filesystem_raw() {
        let path = transcript_path(std::path::Path::new("/traces"), "demo", "../../etc/passwd");
        assert_eq!(path, std::path::Path::new("/traces/demo/______etc_passwd.jsonl"));
    }

    #[test]
    fn summaries_name_the_tool_and_what_it_touched() {
        let bash = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "cargo nextest run"}
        });
        assert_eq!(summarize("tool", &bash), "Bash: cargo nextest run");

        let edit = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": "src/main.rs"}
        });
        assert_eq!(summarize("tool", &edit), "Edit: src/main.rs");

        let prompt = serde_json::json!({"prompt": "  fix   the   parser  "});
        assert_eq!(summarize("prompt", &prompt), "fix the parser");

        // Nothing recognizable still yields something scannable.
        assert_eq!(summarize("stop", &serde_json::json!({})), "stop");
    }

    #[test]
    fn long_summaries_are_clipped_not_wrapped() {
        let long = "x".repeat(400);
        let payload = serde_json::json!({"prompt": long});
        let summary = summarize("prompt", &payload);
        assert_eq!(summary.chars().count(), 120);
        assert!(summary.ends_with("..."));
    }
}
