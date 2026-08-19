//! `nashcode index`: ask the viewer to rebuild a repository's code index.
//!
//! The viewer indexes on its own, every time a merge lands on the default branch. This
//! command exists for the two cases that trigger does not cover: the first index of a
//! repository that was pushed before the viewer knew how to index, and a rebuild after
//! a change to how indexing works.
//!
//! It queues and returns. Indexing runs on the viewer's CI queue — never on a request
//! path — so the answer is "queued", not "done", and `nashcode index --status` is how
//! you find out it finished.

use super::Ctx;
use crate::api::Client;
use crate::cli::IndexArgs;
use crate::timefmt::age_of;
use crate::vcs;
use anyhow::{Result, bail};
use serde_json::{Value, json};

/// The viewer URL for a repository's index status.
pub fn status_url(viewer: &str, repo: &str) -> String {
    format!("{}/{}/code", viewer.trim_end_matches('/'), pct(repo))
}

/// The viewer URL that queues a rebuild.
pub fn index_url(viewer: &str, repo: &str) -> String {
    format!("{}/{}/code/index", viewer.trim_end_matches('/'), pct(repo))
}

/// Percent-encode a path segment. Repository names are restricted enough that this
/// rarely does anything, but a name is user input and a URL is a URL.
fn pct(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// One line describing what the index holds, from the viewer's `/code` answer.
pub fn render_status(value: &Value) -> Vec<String> {
    let number = |key: &str| value.get(key).and_then(Value::as_i64).unwrap_or(0);
    if !value.get("indexed").and_then(Value::as_bool).unwrap_or(false) {
        return vec!["not indexed yet".to_string()];
    }

    let when = value
        .get("last_indexed_at")
        .and_then(Value::as_str)
        .map(age_of)
        .unwrap_or_else(|| "unknown".to_string());
    let mut lines = vec![format!(
        "indexed {when}: {} files, {} chunks ({} embedded), {} symbols",
        number("files"),
        number("chunks"),
        number("embedded_chunks"),
        number("symbols"),
    )];

    // Degradations are the point of this command's output. A run that quietly did
    // half the job is worse than one that failed.
    if let Some(note) = value.get("note").and_then(Value::as_str)
        && !note.trim().is_empty()
    {
        lines.push(format!("note: {note}"));
    }
    if value.get("embeddings_ready").and_then(Value::as_bool) == Some(false)
        && let Some(why) = value.get("embeddings_note").and_then(Value::as_str)
    {
        lines.push(format!("semantic search: {why}"));
    }
    lines
}

/// `nashcode index [repo] [--status]`.
pub fn run(ctx: &Ctx, args: &IndexArgs) -> Result<()> {
    let (name, profile) = ctx.profile()?;
    let Some(viewer) = profile.viewer_url.as_deref().filter(|v| !v.is_empty()) else {
        bail!(
            "profile `{name}` has no viewer URL, and the code index lives in the viewer, \
             not in dgit.\n\
             Deploy one with `nashcode setup --viewer`, or add `viewer_url` to the \
             profile in {}.",
            crate::profile::config_path()?.display()
        );
    };

    let repo = match &args.repo {
        Some(repo) => repo.clone(),
        None => {
            let workspace = vcs::require_cwd()?;
            workspace
                .origin_repo_name()?
                .or_else(|| workspace.default_repo_name())
                .ok_or_else(|| {
                    anyhow::anyhow!("cannot tell which repository this is; name one")
                })?
        }
    };

    let client = Client::new(viewer, &profile.token);

    if !args.status {
        let url = index_url(viewer, &repo);
        let reply = client.post_url(&url, "{}")?;
        if !reply.ok() {
            bail!("{url} returned HTTP {}\n{}", reply.status, reply.body.trim());
        }
        ctx.out.step(format!("queued an index run for {repo}"));
    }

    let url = status_url(viewer, &repo);
    let reply = client.get(&url)?;
    if !reply.ok() {
        bail!("{url} returned HTTP {}\n{}", reply.status, reply.body.trim());
    }
    let status: Value = serde_json::from_str(&reply.body)
        .unwrap_or_else(|_| json!({ "error": reply.body.trim() }));

    if ctx.out.is_json() {
        // The viewer's own answer, passed through, plus whether this call queued a
        // run. An agent should see what the API said, not this command's reading.
        let mut value = status;
        if let Some(object) = value.as_object_mut() {
            object.insert("queued".to_string(), json!(!args.status));
        }
        ctx.out.json(&value);
        return Ok(());
    }
    for line in render_status(&status) {
        ctx.out.line(line);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_urls_are_the_viewers_code_endpoints() {
        assert_eq!(status_url("https://v/", "demo"), "https://v/demo/code");
        assert_eq!(index_url("https://v", "demo"), "https://v/demo/code/index");
    }

    #[test]
    fn a_repo_name_is_encoded_into_the_path() {
        assert_eq!(index_url("https://v", "a b"), "https://v/a%20b/code/index");
    }

    #[test]
    fn an_unindexed_repo_says_so_in_one_line() {
        assert_eq!(render_status(&json!({ "indexed": false })), vec!["not indexed yet"]);
    }

    #[test]
    fn the_status_line_carries_the_counts_and_any_degradation() {
        let lines = render_status(&json!({
            "indexed": true,
            "last_indexed_at": "2026-08-19T00:00:00.000000Z",
            "files": 42,
            "chunks": 130,
            "embedded_chunks": 130,
            "symbols": 88,
            "note": "python indexer: cannot run",
            "embeddings_ready": true,
        }));
        assert!(lines[0].contains("42 files"));
        assert!(lines[0].contains("130 chunks (130 embedded)"));
        assert!(lines[0].contains("88 symbols"));
        assert_eq!(lines[1], "note: python indexer: cannot run");
    }

    #[test]
    fn an_unloaded_model_is_reported_rather_than_hidden() {
        let lines = render_status(&json!({
            "indexed": true,
            "files": 1, "chunks": 1, "embedded_chunks": 0, "symbols": 1,
            "embeddings_ready": false,
            "embeddings_note": "libonnxruntime not found",
        }));
        assert!(lines.last().unwrap().contains("libonnxruntime not found"));
    }
}
