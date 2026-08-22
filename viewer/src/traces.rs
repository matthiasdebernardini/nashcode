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
use sha2::{Digest, Sha256};

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
///
/// The name is a hash of the session id, not a scrubbed copy of it. Session ids come
/// from another program, so they cannot reach the filesystem raw — but replacing the
/// awkward characters maps distinct sessions onto one file: `a.b` and `a_b` would then
/// overwrite each other. A hash is injective enough and needs no escaping.
pub fn transcript_path(root: &std::path::Path, repo: &str, session: &str) -> std::path::PathBuf {
    let digest = <Sha256 as Digest>::digest(session.as_bytes());
    root.join(repo).join(format!("{digest:x}.jsonl"))
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
        let null = serde_json::Value::Null;
        let input = payload.get("tool_input").unwrap_or(&null);
        let detail = telling_argument(tool, input);
        return if detail.is_empty() {
            clip(tool)
        } else {
            clip(&format!("{tool}: {detail}"))
        };
    }

    // Nothing recognizable: the event name is still worth showing.
    clip(kind)
}

/// The one argument of a tool call worth reading at a glance: the command a shell ran,
/// the file a file tool touched.
pub fn telling_argument(tool: &str, input: &serde_json::Value) -> String {
    let arg = |name: &str| input.get(name).and_then(|v| v.as_str());
    let detail = match tool {
        "Bash" | "BashOutput" => arg("command"),
        _ => arg("file_path")
            .or_else(|| arg("notebook_path"))
            .or_else(|| arg("path"))
            .or_else(|| arg("pattern")),
    };
    detail.unwrap_or("").to_owned()
}

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 120 {
        return flat;
    }
    let short: String = flat.chars().take(117).collect();
    format!("{short}...")
}

// ---- reading a session as a conversation ------------------------------------------
//
// A session arrives in one of two shapes and must read the same either way:
//
// 1. live hook events — `prompt`, `tool_name`, `tool_input`, `tool_response`;
// 2. raw Claude Code transcript lines — `type` plus `message.content`, where the
//    content is either a string or an array of `text` / `thinking` / `tool_use` /
//    `tool_result` blocks.
//
// Anything else with readable content degrades to the one-line summary. A payload with
// no readable content at all reads as no pieces: harness bookkeeping lines
// (`file-history-snapshot`, `mode`, `permission-mode`, …) would only echo their own type
// name back, and a row that says the type twice is worse than no row. The page drops
// those; the JSON APIs still return every stored event.

use serde_json::Value;

/// The tools that change a file, so a diff is the honest way to show the call.
pub const EDITING_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// One piece of a conversation, as the Agent page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// What the person wrote. Markdown.
    Prompt(String),
    /// What the agent wrote. Markdown.
    Say(String),
    /// The agent's reasoning. The page collapses it.
    Thinking(String),
    /// A tool call.
    Call(Call),
    /// What a tool answered. Errors render open.
    Output { text: String, error: bool },
    /// A shape the renderer does not know, summarized in one line.
    Note(String),
}

/// A tool call, with enough of its input to be readable without unfolding anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Call {
    pub name: String,
    /// The argument worth reading at a glance: the command, the path.
    pub detail: String,
    /// The whole input, pretty-printed, for the disclosure.
    pub input: String,
    /// Set when the call edited a file, so the page can render the change.
    pub edit: Option<FileEdit>,
    /// The harness's id for this call. Its result arrives on a later line.
    pub id: Option<String>,
}

/// A file change a tool call made, as a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: String,
    /// A unified diff, ready for `@pierre/diffs`.
    pub patch: String,
}

/// Read one recorded event as the pieces of conversation it holds. Empty when the
/// payload holds none, which is how bookkeeping lines disappear from the page.
pub fn read(kind: &str, payload: &Value) -> Vec<Piece> {
    let some = |pieces: Vec<Piece>| (!pieces.is_empty()).then_some(pieces);
    if let Some(pieces) =
        transcript_pieces(payload).and_then(some).or_else(|| hook_pieces(payload).and_then(some))
    {
        return pieces;
    }

    // `summarize` falls back to the event name when it recognizes nothing, so a summary
    // that is only the name is the signal that there is nothing here to show.
    let summary = summarize(kind, payload);
    if summary == clip(kind) {
        return Vec::new();
    }
    vec![Piece::Note(summary)]
}

/// The file change reported on the result line that answers a tool call, keyed by the
/// call's id. Claude Code computes this against the file on disk, so it beats anything
/// the call's own arguments can be made to say.
pub fn result_edit(payload: &Value) -> Option<(String, FileEdit)> {
    let id = tool_result_id(payload)?;
    let result = payload.get("toolUseResult")?;
    let path = result.get("filePath").and_then(|v| v.as_str())?;
    let patch = patch_from_structured(path, result.get("structuredPatch")?)?;
    Some((id, FileEdit { path: path.to_owned(), patch }))
}

/// The `tool_use_id` a transcript line's `tool_result` block answers.
fn tool_result_id(payload: &Value) -> Option<String> {
    payload
        .get("message")?
        .get("content")?
        .as_array()?
        .iter()
        .find(|block| block.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
        .and_then(|block| block.get("tool_use_id"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

fn transcript_pieces(payload: &Value) -> Option<Vec<Piece>> {
    let line = payload.get("type")?.as_str()?;
    if !matches!(line, "user" | "assistant" | "system") {
        return None;
    }
    let mut pieces = Vec::new();
    match payload.get("message").and_then(|message| message.get("content")) {
        Some(Value::String(text)) => push_said(line, text, &mut pieces),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                push_block(line, block, &mut pieces);
            }
        }
        // A system line carries no message. Say what it was rather than nothing.
        _ => {
            let text = payload
                .get("content")
                .and_then(|v| v.as_str())
                .or_else(|| payload.get("subtype").and_then(|v| v.as_str()));
            if let Some(text) = text {
                pieces.push(Piece::Note(clip(text)));
            }
        }
    }
    Some(pieces)
}

fn hook_pieces(payload: &Value) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    if let Some(text) = payload
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        pieces.push(Piece::Prompt(text.to_owned()));
    }

    if let Some(tool) = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .filter(|tool| !tool.is_empty())
    {
        let null = Value::Null;
        let input = payload.get("tool_input").unwrap_or(&null);
        let mut call = call(tool, input, None);
        let response = payload.get("tool_response");
        // `PostToolUse` carries the harness's own patch; prefer it to a synthesized one.
        if let Some(response) = response
            && let Some(edit) = response_edit(tool, input, response)
        {
            call.edit = Some(edit);
        }
        pieces.push(Piece::Call(call));
        if let Some(response) = response {
            let text = flatten(Some(response));
            if !text.is_empty() {
                pieces.push(Piece::Output { text, error: response_failed(response) });
            }
        }
    }

    (!pieces.is_empty()).then_some(pieces)
}

fn response_failed(response: &Value) -> bool {
    response.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)
        || response.get("error").is_some()
        || response.get("success").and_then(|v| v.as_bool()) == Some(false)
}

fn response_edit(tool: &str, input: &Value, response: &Value) -> Option<FileEdit> {
    if !EDITING_TOOLS.contains(&tool) {
        return None;
    }
    let path = response
        .get("filePath")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| file_path(input))?;
    let patch = patch_from_structured(&path, response.get("structuredPatch")?)?;
    Some(FileEdit { path, patch })
}

fn push_said(line: &str, text: &str, out: &mut Vec<Piece>) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    out.push(match line {
        // Harness markup — command output, system reminders — is not what a person
        // asked for. One line of it is enough.
        "user" if trimmed.starts_with('<') => Piece::Note(clip(trimmed)),
        "user" => Piece::Prompt(trimmed.to_owned()),
        _ => Piece::Say(trimmed.to_owned()),
    });
}

fn push_block(line: &str, block: &Value, out: &mut Vec<Piece>) {
    let text = |name: &str| block.get(name).and_then(|v| v.as_str()).unwrap_or("");
    match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "text" => push_said(line, text("text"), out),
        "thinking" => {
            let thinking = text("thinking").trim();
            if !thinking.is_empty() {
                out.push(Piece::Thinking(thinking.to_owned()));
            }
        }
        "tool_use" => {
            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
            let null = Value::Null;
            let input = block.get("input").unwrap_or(&null);
            out.push(Piece::Call(call(name, input, block.get("id").and_then(|v| v.as_str()))));
        }
        "tool_result" => out.push(Piece::Output {
            text: flatten(block.get("content")),
            error: block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
        }),
        "" => {}
        other => out.push(Piece::Note(other.to_owned())),
    }
}

fn call(name: &str, input: &Value, id: Option<&str>) -> Call {
    Call {
        name: name.to_owned(),
        detail: telling_argument(name, input),
        input: if input.is_null() {
            String::new()
        } else {
            serde_json::to_string_pretty(input).unwrap_or_default()
        },
        edit: synthesized_edit(name, input),
        id: id.map(str::to_owned),
    }
}

/// A tool result's text, whatever the harness wrapped it in.
fn flatten(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(_)) => {
            serde_json::to_string_pretty(content.expect("matched")).unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn file_path(input: &Value) -> Option<String> {
    input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

// ---- diffs -------------------------------------------------------------------------

/// A unified diff from Claude Code's `structuredPatch`, which is the hunk data git
/// prints with the header left off.
pub fn patch_from_structured(path: &str, hunks: &Value) -> Option<String> {
    let hunks = hunks.as_array()?;
    if hunks.is_empty() {
        return None;
    }
    let mut patch = diff_header(path, false);
    for hunk in hunks {
        let number = |name: &str, fallback: i64| {
            hunk.get(name).and_then(serde_json::Value::as_i64).unwrap_or(fallback)
        };
        patch.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            number("oldStart", 1),
            number("oldLines", 0),
            number("newStart", 1),
            number("newLines", 0),
        ));
        for line in hunk.get("lines").and_then(|v| v.as_array()).into_iter().flatten() {
            let Some(line) = line.as_str() else { continue };
            patch.push_str(line);
            patch.push('\n');
        }
    }
    Some(patch)
}

/// The diff for a file-editing call, reconstructed from the call's own arguments.
///
/// This is the fallback for when no `structuredPatch` came back — an unfinished call,
/// or a harness that reports less. The hunk positions are the replacement's own, not
/// the file's: a synthesized diff shows *what* changed, not where.
fn synthesized_edit(tool: &str, input: &Value) -> Option<FileEdit> {
    if !EDITING_TOOLS.contains(&tool) {
        return None;
    }
    let path = file_path(input)?;
    let patch = match tool {
        "Write" => {
            let content = input.get("content").and_then(|v| v.as_str())?;
            let lines = split_lines(content);
            let mut patch = diff_header(&path, true);
            patch.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
            for line in lines {
                patch.push('+');
                patch.push_str(line);
                patch.push('\n');
            }
            patch
        }
        "MultiEdit" => {
            let edits = input.get("edits").and_then(|v| v.as_array())?;
            let mut patch = diff_header(&path, false);
            let (mut old_at, mut new_at) = (1i64, 1i64);
            let mut wrote = false;
            for edit in edits {
                let Some((old, new)) = replacement(edit) else { continue };
                push_hunk(&mut patch, &mut old_at, &mut new_at, old, new);
                wrote = true;
            }
            if !wrote {
                return None;
            }
            patch
        }
        _ => {
            let (old, new) = replacement(input)?;
            let mut patch = diff_header(&path, false);
            push_hunk(&mut patch, &mut 1, &mut 1, old, new);
            patch
        }
    };
    Some(FileEdit { path, patch })
}

/// The before/after strings of one replacement, under either name the tools use.
fn replacement(edit: &Value) -> Option<(&str, &str)> {
    let field = |name: &str| edit.get(name).and_then(|v| v.as_str()).unwrap_or("");
    let old = if field("old_string").is_empty() { field("old_source") } else { field("old_string") };
    let new = if field("new_string").is_empty() { field("new_source") } else { field("new_string") };
    (!(old.is_empty() && new.is_empty())).then_some((old, new))
}

fn push_hunk(patch: &mut String, old_at: &mut i64, new_at: &mut i64, old: &str, new: &str) {
    let (old_lines, new_lines) = (split_lines(old), split_lines(new));
    patch.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        old_at,
        old_lines.len(),
        new_at,
        new_lines.len()
    ));
    for line in &old_lines {
        patch.push('-');
        patch.push_str(line);
        patch.push('\n');
    }
    for line in &new_lines {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    *old_at += old_lines.len() as i64;
    *new_at += new_lines.len() as i64;
}

/// Transcript paths are absolute; a diff header wants a repo-relative one.
fn diff_header(path: &str, created: bool) -> String {
    let rel = path.trim_start_matches('/');
    let old = if created { "/dev/null".to_owned() } else { format!("a/{rel}") };
    format!("diff --git a/{rel} b/{rel}\n--- {old}\n+++ b/{rel}\n")
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n').unwrap_or(text).split('\n').collect()
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
        let root = std::path::Path::new("/traces");
        let path = transcript_path(root, "demo", "../../etc/passwd");
        assert!(path.starts_with("/traces/demo"), "the id cannot climb out of the directory");
        let name = path.file_name().expect("a file name").to_str().expect("utf-8");
        assert_eq!(name.len(), 64 + ".jsonl".len());
        assert!(name.trim_end_matches(".jsonl").chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Two session ids one character apart must not share a file. A sanitizing scheme
    /// folds them together and the second upload silently eats the first.
    #[test]
    fn session_ids_that_sanitize_alike_get_different_files() {
        let root = std::path::Path::new("/traces");
        assert_ne!(transcript_path(root, "demo", "a.b"), transcript_path(root, "demo", "a_b"));
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

    // ---- reading a session as a conversation --------------------------------------

    #[test]
    fn a_live_hook_prompt_reads_as_the_person_speaking() {
        let payload = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "add a retry note",
        });
        assert_eq!(read("UserPromptSubmit", &payload), vec![Piece::Prompt("add a retry note".into())]);
    }

    #[test]
    fn a_live_hook_tool_call_keeps_its_telling_argument() {
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "cargo nextest run"},
        });
        let pieces = read("PreToolUse", &payload);
        let Piece::Call(call) = &pieces[0] else { panic!("{pieces:?}") };
        assert_eq!(call.name, "Bash");
        assert_eq!(call.detail, "cargo nextest run");
        assert!(call.input.contains("cargo nextest run"), "the full input is kept");
        assert!(call.edit.is_none(), "a shell command is not a file change");
    }

    #[test]
    fn a_transcript_assistant_line_reads_as_text_thinking_and_calls() {
        let payload = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "weigh the options"},
                    {"type": "text", "text": "I'll check the tests first."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash",
                     "input": {"command": "cargo nextest run"}},
                ]
            }
        });
        let pieces = read("assistant", &payload);
        assert_eq!(pieces[0], Piece::Thinking("weigh the options".into()));
        assert_eq!(pieces[1], Piece::Say("I'll check the tests first.".into()));
        let Piece::Call(call) = &pieces[2] else { panic!("{pieces:?}") };
        assert_eq!(call.id.as_deref(), Some("toolu_1"));
    }

    #[test]
    fn a_transcript_user_line_reads_as_a_prompt_or_a_result() {
        let typed = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "rename the project"},
        });
        assert_eq!(read("user", &typed), vec![Piece::Prompt("rename the project".into())]);

        let failed = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "content": "error: no such file", "is_error": true}
            ]},
        });
        assert_eq!(
            read("user", &failed),
            vec![Piece::Output { text: "error: no such file".into(), error: true }]
        );
    }

    #[test]
    fn a_result_wrapped_in_text_blocks_still_reads() {
        let payload = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": [
                    {"type": "text", "text": "line one"},
                    {"type": "text", "text": "line two"},
                ]}
            ]},
        });
        assert_eq!(
            read("user", &payload),
            vec![Piece::Output { text: "line one\nline two".into(), error: false }]
        );
    }

    #[test]
    fn harness_markup_is_summarized_not_mistaken_for_a_prompt() {
        let payload = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "<system-reminder>be careful</system-reminder>"},
        });
        assert_eq!(
            read("user", &payload),
            vec![Piece::Note("<system-reminder>be careful</system-reminder>".into())]
        );
    }

    #[test]
    fn an_unknown_shape_degrades_to_one_line_and_an_empty_one_to_nothing() {
        // Recognizable content in an otherwise unknown shape still earns a row.
        let known = serde_json::json!({"something": "else", "prompt": "ship it"});
        assert_eq!(read("Notification", &known), vec![Piece::Prompt("ship it".into())]);

        // Nothing to say: the row would only repeat the event name back.
        let payload = serde_json::json!({"something": "else"});
        assert!(read("Notification", &payload).is_empty());

        // An empty assistant turn is a shape we know but has nothing in it.
        let empty = serde_json::json!({"type": "assistant", "message": {"content": []}});
        assert!(read("assistant", &empty).is_empty());

        // The bookkeeping lines a backfilled transcript opens with.
        for kind in ["file-history-snapshot", "mode", "permission-mode", "queue-operation"] {
            let line = serde_json::json!({"type": kind});
            assert!(read(kind, &line).is_empty(), "{kind} renders nothing");
        }
    }

    #[test]
    fn a_structured_patch_becomes_a_unified_diff() {
        let payload = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_9", "content": "ok"}
            ]},
            "toolUseResult": {
                "filePath": "/repo/src/web.rs",
                "structuredPatch": [{
                    "oldStart": 111, "oldLines": 2, "newStart": 111, "newLines": 3,
                    "lines": [" keep", "-gone", "+new", "+also new"],
                }],
            },
        });
        let (id, edit) = result_edit(&payload).expect("an edit");
        assert_eq!(id, "toolu_9");
        assert_eq!(edit.path, "/repo/src/web.rs");
        assert_eq!(
            edit.patch,
            "diff --git a/repo/src/web.rs b/repo/src/web.rs\n\
             --- a/repo/src/web.rs\n\
             +++ b/repo/src/web.rs\n\
             @@ -111,2 +111,3 @@\n keep\n-gone\n+new\n+also new\n"
        );
    }

    #[test]
    fn an_edit_without_a_patch_synthesizes_one_from_its_own_arguments() {
        let payload = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use", "id": "t", "name": "Edit",
                "input": {"file_path": "src/main.rs", "old_string": "one\ntwo", "new_string": "one\ntwo\nthree"},
            }]},
        });
        let pieces = read("assistant", &payload);
        let Piece::Call(call) = &pieces[0] else { panic!("{pieces:?}") };
        let edit = call.edit.as_ref().expect("a synthesized edit");
        assert_eq!(edit.path, "src/main.rs");
        assert!(edit.patch.starts_with("diff --git a/src/main.rs b/src/main.rs\n"), "{}", edit.patch);
        assert!(edit.patch.contains("@@ -1,2 +1,3 @@\n"), "{}", edit.patch);
        assert!(edit.patch.contains("-one\n-two\n+one\n+two\n+three\n"), "{}", edit.patch);
    }

    #[test]
    fn a_write_synthesizes_a_whole_new_file() {
        let input = serde_json::json!({"file_path": "notes.md", "content": "# title\nbody\n"});
        let edit = synthesized_edit("Write", &input).expect("an edit");
        assert_eq!(
            edit.patch,
            "diff --git a/notes.md b/notes.md\n\
             --- /dev/null\n\
             +++ b/notes.md\n\
             @@ -0,0 +1,2 @@\n+# title\n+body\n"
        );
    }

    #[test]
    fn a_multiedit_walks_its_hunks_forward() {
        let input = serde_json::json!({
            "file_path": "a.rs",
            "edits": [
                {"old_string": "a", "new_string": "b"},
                {"old_string": "c", "new_string": "d"},
            ],
        });
        let edit = synthesized_edit("MultiEdit", &input).expect("an edit");
        assert!(edit.patch.contains("@@ -1,1 +1,1 @@\n-a\n+b\n"), "{}", edit.patch);
        assert!(edit.patch.contains("@@ -2,1 +2,1 @@\n-c\n+d\n"), "{}", edit.patch);
    }

    #[test]
    fn only_file_editing_tools_get_a_diff() {
        let input = serde_json::json!({"file_path": "a.rs", "old_string": "a", "new_string": "b"});
        assert!(synthesized_edit("Read", &input).is_none());
        assert!(synthesized_edit("Edit", &input).is_some());
    }

    #[test]
    fn a_post_tool_use_hook_prefers_the_harness_patch_and_reports_its_failure() {
        let payload = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": "/repo/a.rs", "old_string": "a", "new_string": "b"},
            "tool_response": {
                "filePath": "/repo/a.rs",
                "structuredPatch": [{
                    "oldStart": 40, "oldLines": 1, "newStart": 40, "newLines": 1,
                    "lines": ["-a", "+b"],
                }],
                "is_error": true,
            },
        });
        let pieces = read("PostToolUse", &payload);
        let Piece::Call(call) = &pieces[0] else { panic!("{pieces:?}") };
        let edit = call.edit.as_ref().expect("an edit");
        assert!(edit.patch.contains("@@ -40,1 +40,1 @@"), "the harness patch wins: {}", edit.patch);
        let Piece::Output { error, .. } = &pieces[1] else { panic!("{pieces:?}") };
        assert!(error, "a failed call is an error");
    }
}
