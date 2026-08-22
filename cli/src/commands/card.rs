//! Cards: which ones are ready to start, and taking one.
//!
//! A card is markdown under `tasks/`, and `blocks:` in its front matter is the edge
//! that says another card cannot start yet. The viewer derives the answer — a `todo`
//! card whose blockers are all `done` is ready — and reports it in `/brain`, so this
//! side asks rather than re-deriving it from a tree it may not have fetched.
//!
//! `claim` is the other half: one write, one commit, one push, so two agents reading
//! the same ready list race on the push instead of on the file.

use super::Ctx;
use crate::api::Client;
use crate::cli::{ClaimArgs, ReadyArgs};
use crate::commands::brain;
use crate::exit::{Class, classed};
use crate::vcs::{self, Workspace};
use anyhow::{Context, Result};
use serde_json::{Value, json};

/// `nashcode ready [<repo>]`: the cards nothing open is waiting on.
///
/// One row per card, carrying the repository it is in, so the whole-tailnet form and
/// the single-repo form have the same shape.
pub fn ready(ctx: &Ctx, args: &ReadyArgs) -> Result<Vec<Value>> {
    let (viewer, token) = brain::viewer_url(ctx).map_err(|why| classed(Class::NotFound, why))?;
    let repo = match &args.repo {
        Some(name) => Some(name.clone()),
        None => vcs::detect_cwd()?
            .and_then(|ws| ws.origin_repo_name().ok().flatten().or_else(|| ws.default_repo_name())),
    };
    let url = brain::brain_url(&viewer, repo.as_deref());
    let client = Client::new(&viewer, &token);
    let reply = client.get_json(&url)?;
    if !reply.ok() {
        return Err(classed(
            Class::Api,
            format!("{url} returned HTTP {}\n{}", reply.status, reply.body.trim()),
        ));
    }
    let stanza: Value =
        serde_json::from_str(&reply.body).context("the viewer's /brain answer is not JSON")?;
    Ok(ready_rows(&stanza))
}

/// The ready cards in a `/brain` stanza, titled from the `todo` column when the
/// stanza carries one. A viewer too old to answer `ready` yields no rows rather than
/// an error: nothing is ready is a truthful answer to give an agent.
pub fn ready_rows(stanza: &Value) -> Vec<Value> {
    let mut rows = Vec::new();
    let repos = stanza.get("repos").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for repo in repos {
        let name = repo.get("name").and_then(Value::as_str).unwrap_or("?");
        let todo = repo.get("cards").and_then(|c| c.get("todo")).and_then(Value::as_array);
        let ready = repo.get("ready").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        for path in ready {
            let Some(path) = path.as_str() else { continue };
            let title = todo
                .and_then(|cards| {
                    cards
                        .iter()
                        .find(|card| card.get("path").and_then(Value::as_str) == Some(path))
                })
                .and_then(|card| card.get("title").and_then(Value::as_str));
            let mut row = json!({ "repo": name, "path": path });
            if let Some(title) = title {
                row["title"] = json!(title);
            }
            rows.push(row);
        }
    }
    rows
}

/// `nashcode claim <tasks/x.md>`: take the card and say so on the server.
pub fn claim(ctx: &Ctx, args: &ClaimArgs) -> Result<Value> {
    let ws = vcs::require_cwd()?;
    let path = std::path::PathBuf::from(&args.file);
    if !path.exists() {
        return Err(classed(Class::NotFound, format!("{} does not exist", path.display())));
    }
    let source = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let who = assignee(&ws)?;
    let claimed = claim_front_matter(&source, &who).ok_or_else(|| {
        classed(
            Class::Usage,
            format!(
                "{} has no front matter, so it is not a card the board can move",
                path.display()
            ),
        )
    })?;
    let file = std::fs::canonicalize(&path)
        .ok()
        .and_then(|abs| {
            let root = std::fs::canonicalize(&ws.root).unwrap_or_else(|_| ws.root.clone());
            Some(abs.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/"))
        })
        .unwrap_or_else(|| args.file.clone());

    // Already `doing` and already yours: there is nothing to commit, and a git that
    // says so is not a failure of the claim.
    if claimed == source {
        return Ok(json!({
            "path": file,
            "assignee": who,
            "status": "doing",
            "branch": ws.current_branch(),
            "pushed": false,
        }));
    }
    std::fs::write(&path, &claimed).with_context(|| format!("write {}", path.display()))?;
    let branch = commit_and_push(ctx, &ws, &file, &format!("claim {file}"))?;

    Ok(json!({
        "path": file,
        "assignee": who,
        "status": "doing",
        "branch": branch,
        "pushed": true,
    }))
}

/// The name a claim is filed under: whatever this working copy commits as.
///
/// git's `user.name` (jj's own, in a jj repository), then `$USER`. No new setting:
/// a card says who has it in the same words the commit that took it will.
fn assignee(ws: &Workspace) -> Result<String> {
    let configured = if ws.kind.is_jj() {
        vcs::jj(&ws.root, &["config", "get", "user.name"])
    } else {
        vcs::git(&ws.root, &["config", "user.name"])
    };
    let name = configured
        .ok()
        .filter(|run| run.ok())
        .map(|run| run.stdout.trim().to_string())
        .filter(|name| !name.is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()));
    name.ok_or_else(|| {
        classed(
            Class::Usage,
            "nobody to claim the card for: this working copy has no `user.name`",
        )
    })
}

/// Commit `file` and push it. Returns the branch it went to.
///
/// The same shape `init` uses, narrowed to one path: a claim must not sweep whatever
/// else is dirty in the tree into its commit.
fn commit_and_push(ctx: &Ctx, ws: &Workspace, file: &str, message: &str) -> Result<String> {
    let branch = ws.current_branch().unwrap_or_else(|| "main".to_string());
    if ws.kind.is_jj() {
        // jj has no index, so the whole working-copy change is the commit; the
        // bookmark is what `jj git push` can actually send.
        let commit = vcs::jj(&ws.root, &["commit", "-m", message])?;
        if !commit.ok() {
            return Err(classed(Class::Api, format!("jj commit failed: {}", commit.stderr.trim())));
        }
        let mut set = vcs::jj(&ws.root, &["bookmark", "set", &branch, "-r", "@-"])?;
        if !set.ok() {
            // jj before 0.21 called them branches.
            set = vcs::jj(&ws.root, &["branch", "set", &branch, "-r", "@-"])?;
        }
        if !set.ok() {
            return Err(classed(
                Class::Api,
                format!("could not point the `{branch}` bookmark at the claim: {}", set.stderr.trim()),
            ));
        }
        let push = vcs::jj(
            &ws.root,
            &["git", "push", "--allow-new", "--remote", "origin", "--bookmark", &branch],
        )?;
        if !push.ok() {
            return Err(vcs::transport_error("jj git push", &push.stderr));
        }
        ctx.out.step(format!("jj git push --bookmark {branch}"));
        return Ok(branch);
    }

    // Staged first so a card that is not in the tree yet is a commit rather than a
    // pathspec error; the commit itself is still scoped to the one path.
    let add = vcs::git(&ws.root, &["add", "--", file])?;
    if !add.ok() {
        return Err(classed(Class::Api, format!("git add failed: {}", add.stderr.trim())));
    }
    let commit = vcs::git(&ws.root, &["commit", "--quiet", "-m", message, "--", file])?;
    if !commit.ok() {
        return Err(classed(Class::Api, format!("git commit failed: {}", commit.stderr.trim())));
    }
    ctx.out.step(format!("git commit -m \"{message}\""));
    let push = vcs::git(&ws.root, &["push", "--quiet", "origin", &branch])?;
    if !push.ok() {
        return Err(vcs::transport_error("git push", &push.stderr));
    }
    ctx.out.step(format!("git push origin {branch}"));
    Ok(branch)
}

// --- the front-matter edit --------------------------------------------------

/// A claim is two fields in one write: the card is `doing`, and it is yours.
///
/// `None` when the file has no front-matter block, which is the one case where there
/// is nothing to rewrite and inventing a block would rewrite somebody's document.
pub fn claim_front_matter(source: &str, assignee: &str) -> Option<String> {
    let doing = set_field(source, "status", "doing")?;
    set_field(&doing, "assignee", assignee)
}

/// Byte bounds of the front-matter block: the text between the fences.
fn front_matter_bounds(source: &str) -> Option<(usize, usize)> {
    let open = if source.starts_with("---\n") {
        4
    } else if source.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let mut offset = 0;
    for line in source[open..].split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" || trimmed == "..." {
            return Some((open, open + offset));
        }
        offset += line.len();
    }
    // An unterminated fence is not front matter.
    None
}

/// Rewrite one front-matter key, adding it to the end of the block when it is not
/// there. Every other byte of the file is left alone: a card is a human-edited
/// document, and a claim must not reformat it.
fn set_field(source: &str, key: &str, value: &str) -> Option<String> {
    let (start, end) = front_matter_bounds(source)?;
    let front = &source[start..end];
    let value = scalar(value);

    let mut rewritten = String::with_capacity(front.len() + value.len());
    let mut replaced = false;
    for line in front.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        if !replaced && body.trim_start().starts_with(&format!("{key}:")) {
            let indent = &body[..body.len() - body.trim_start().len()];
            let newline = &line[body.len()..];
            rewritten.push_str(&format!("{indent}{key}: {value}{newline}"));
            replaced = true;
        } else {
            rewritten.push_str(line);
        }
    }
    if !replaced {
        if !rewritten.is_empty() && !rewritten.ends_with('\n') {
            rewritten.push('\n');
        }
        rewritten.push_str(&format!("{key}: {value}\n"));
    }
    Some(format!("{}{}{}", &source[..start], rewritten, &source[end..]))
}

/// A value safe to write as a YAML scalar. A name with a colon or a leading space in
/// it would otherwise turn the card into one the board cannot parse.
fn scalar(value: &str) -> String {
    let plain = !value.is_empty()
        && value == value.trim()
        && !value.contains(['"', '\'', ':', '#', '\n', '\r', '\t'])
        && !value.starts_with(['-', '[', '{', '&', '*', '!', '|', '>', '%', '@', '`', '?', ',']);
    if plain {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "---\nstatus: todo\ntitle: Ship the API\nbranch: feat/api\n---\n\n# Ship it\n\nBody.\n";

    #[test]
    fn a_claim_sets_the_status_and_the_assignee_and_nothing_else() {
        let claimed = claim_front_matter(CARD, "rob").unwrap();
        assert_eq!(
            claimed,
            "---\nstatus: doing\ntitle: Ship the API\nbranch: feat/api\nassignee: rob\n---\n\n# Ship it\n\nBody.\n"
        );
    }

    #[test]
    fn an_assignee_already_there_is_replaced_in_place() {
        let source = "---\nassignee: ann\nstatus: todo\n---\nbody\n";
        assert_eq!(
            claim_front_matter(source, "rob").unwrap(),
            "---\nassignee: rob\nstatus: doing\n---\nbody\n"
        );
    }

    #[test]
    fn a_file_with_no_front_matter_is_refused_rather_than_given_one() {
        assert!(claim_front_matter("# just a doc\n", "rob").is_none());
        // An unterminated fence is not a block either.
        assert!(claim_front_matter("---\nstatus: todo\n", "rob").is_none());
    }

    #[test]
    fn a_name_that_would_break_the_yaml_is_quoted() {
        let claimed = claim_front_matter(CARD, "Ada: builder").unwrap();
        assert!(claimed.contains("assignee: \"Ada: builder\""), "{claimed}");
        // An ordinary name stays bare, spaces and all.
        let claimed = claim_front_matter(CARD, "Ada Lovelace").unwrap();
        assert!(claimed.contains("assignee: Ada Lovelace"), "{claimed}");
    }

    #[test]
    fn crlf_and_an_empty_block_survive_the_edit() {
        let claimed = claim_front_matter("---\r\nstatus: todo\r\n---\r\nbody\r\n", "rob").unwrap();
        assert_eq!(claimed, "---\r\nstatus: doing\r\nassignee: rob\n---\r\nbody\r\n");
        let claimed = claim_front_matter("---\n---\nbody\n", "rob").unwrap();
        assert_eq!(claimed, "---\nstatus: doing\nassignee: rob\n---\nbody\n");
    }

    #[test]
    fn ready_rows_carry_the_repo_and_the_title_when_the_stanza_has_one() {
        let stanza = json!({
            "repos": [{
                "name": "demo",
                "ready": ["tasks/a.md", "tasks/b.md"],
                "cards": { "todo": [{ "path": "tasks/a.md", "title": "First" }] }
            }]
        });
        let rows = ready_rows(&stanza);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], json!({ "repo": "demo", "path": "tasks/a.md", "title": "First" }));
        // No title on the stanza is a row without one, not a row with an empty one.
        assert_eq!(rows[1], json!({ "repo": "demo", "path": "tasks/b.md" }));
    }

    #[test]
    fn a_viewer_that_does_not_answer_ready_yet_lists_nothing() {
        assert!(ready_rows(&json!({ "repos": [{ "name": "demo", "cards": {} }] })).is_empty());
        assert!(ready_rows(&json!({})).is_empty());
    }
}
