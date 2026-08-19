//! Plans: create one, annotate it locally, read the replies back.
//!
//! A plan is a markdown file under `plans/` at the root of a repository. That
//! is the whole format. The viewer renders the directory and collects comments
//! on the rendered page, so `plans/` is where an agent writes what it intends
//! to do and `nashcode comments` is how it hears back.

use super::Ctx;
use crate::api::Client;
use crate::cli::{AnnotateArgs, CommentsArgs, PlanNewArgs};
use crate::timefmt::age_of;
use crate::vcs;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const PLANNOTATOR_HINT: &str =
    "plannotator is not on PATH. Install it, then run this again: https://github.com/plannotator/plannotator";

/// `my Great Plan!` -> `my-great-plan`.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() { "plan".to_string() } else { s }
}

/// The starting text of a new plan. Short on purpose: a template long enough
/// to skim is a template nobody fills in.
pub fn template(title: &str) -> String {
    format!(
        "# {title}\n\n\
         Status: draft\n\n\
         ## Problem\n\n\
         What is wrong today, and who it affects.\n\n\
         ## Approach\n\n\
         What to build, and why this shape and not another.\n\n\
         ## Steps\n\n\
         1. \n\n\
         ## Risks\n\n\
         What could go wrong, and what would show it early.\n"
    )
}

pub fn new(ctx: &Ctx, args: &PlanNewArgs) -> Result<()> {
    let title = args.title.join(" ");
    if title.trim().is_empty() {
        bail!("give the plan a title: nashcode plan new \"replace the parser\"");
    }
    let ws = vcs::require_cwd()?;
    let dir = ws.root.join("plans");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.md", slug(&title)));
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    std::fs::write(&path, template(&title)).with_context(|| format!("write {}", path.display()))?;

    let rel = path
        .strip_prefix(&ws.root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    ctx.out.emit(json!({ "path": path, "relative": rel, "title": title }), || {
        ctx.out.line(rel)
    });
    Ok(())
}

pub fn annotate(ctx: &Ctx, args: &AnnotateArgs) -> Result<()> {
    let path = PathBuf::from(&args.file);
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }

    // The viewer link is useful whether or not plannotator is installed.
    let viewer = viewer_plan_url(ctx, &path).unwrap_or(None);

    let Some(bin) = which("plannotator") else {
        ctx.out.emit(
            json!({ "file": args.file, "plannotator": Value::Null, "viewer_url": viewer }),
            || {
                ctx.out.line(PLANNOTATOR_HINT);
                if let Some(u) = &viewer {
                    ctx.out.line(u);
                }
            },
        );
        return Ok(());
    };

    if ctx.out.is_json() {
        ctx.out.json(&json!({
            "file": args.file,
            "plannotator": bin.to_string_lossy(),
            "viewer_url": viewer,
        }));
        return Ok(());
    }

    // Resolved before the launch so a missing viewer is a known fact by the time
    // the human is done writing, not a surprise discovered afterwards.
    let target = comment_target(ctx, &path);

    let dir = scratch_dir()?;
    let result_file = dir.join("decision.json");
    let status = std::process::Command::new(&bin)
        .arg("annotate")
        .arg(&path)
        .arg("--gate")
        .arg("--json")
        .arg("--result-file")
        .arg(&result_file)
        // --json makes plannotator print the decision record on stdout as well
        // as publishing it. We read the file, so the copy on our own stdout
        // would only be noise between the feedback and the posted id.
        .stdout(std::process::Stdio::null())
        .status()
        .with_context(|| format!("run {}", bin.display()));

    // Read the decision before judging the exit code. Publication is atomic, so
    // a file that exists is a whole decision, and a human who spent ten minutes
    // annotating should not lose it to whatever plannotator tripped over on the
    // way out.
    let raw = std::fs::read_to_string(&result_file);
    let _ = std::fs::remove_dir_all(&dir);
    let status = status?;
    let raw = match raw {
        Ok(raw) => {
            if !status.success() {
                ctx.out.warn(format!(
                    "plannotator exited {}, but it had already published a decision — using it",
                    status.code().unwrap_or(-1)
                ));
            }
            raw
        }
        Err(e) if !status.success() => {
            return Err(anyhow::Error::new(e).context(format!(
                "plannotator exited {} and published no decision",
                status.code().unwrap_or(-1)
            )));
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "plannotator exited 0 but published no decision to {}",
                result_file.display()
            )));
        }
    };

    let decision = match decision_body(&raw) {
        Ok(d) => d,
        Err(e) => {
            // Unreadable, but still the human's words. Put them where they can
            // be copied out, then fail.
            ctx.out.line(raw.trim());
            return Err(e);
        }
    };
    let Some(body) = decision else {
        ctx.out.line("dismissed, nothing posted");
        return Ok(());
    };

    let target = match target {
        Ok(t) => t,
        Err(why) => {
            // The human's words outlive the plumbing. Print them, say why they
            // went nowhere, and exit 0: nothing failed, there was just nowhere
            // to put them.
            ctx.out.line(&body);
            ctx.out.line(format!("not posted: {why}"));
            return Ok(());
        }
    };

    let payload = comment_payload(&target.file, &target.branch, &body).to_string();
    let client = Client::new(&target.viewer, &target.token);
    let reply = match client.post_url(&target.url, &payload) {
        Ok(r) => r,
        Err(e) => {
            ctx.out.line(&body);
            return Err(e.context("post the annotation to the viewer"));
        }
    };
    if !reply.ok() {
        ctx.out.line(&body);
        bail!(
            "{} returned HTTP {}\n{}",
            target.url,
            reply.status,
            reply.body.trim()
        );
    }

    let id = serde_json::from_str::<Value>(&reply.body)
        .ok()
        .and_then(|v| v.get("id").and_then(|i| i.as_i64()));
    ctx.out.line(match id {
        Some(id) => format!("posted #{id}"),
        None => "posted".to_string(),
    });
    if let Some(u) = &viewer {
        ctx.out.line(u);
    }
    Ok(())
}

/// What plannotator writes to `--result-file`: one record, one decision.
#[derive(Debug, Deserialize)]
struct Decision {
    decision: String,
    #[serde(default)]
    feedback: Option<String>,
}

/// The comment to post for a plannotator decision record, or `None` when the
/// human dismissed the review and there is nothing to say.
///
/// An approval posts "Approved." on purpose. The agent that pushed the plan is
/// polling the comment stream, and silence and approval look the same there.
pub fn decision_body(raw: &str) -> Result<Option<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("plannotator wrote an empty decision file");
    }
    let d: Decision =
        serde_json::from_str(raw).context("plannotator's decision file is not the JSON we expect")?;
    let feedback = d.feedback.as_deref().map(str::trim).filter(|f| !f.is_empty());
    match d.decision.as_str() {
        "annotated" => match feedback {
            Some(f) => Ok(Some(f.to_string())),
            None => bail!("plannotator reported annotations but no feedback"),
        },
        "approved" => Ok(Some(match feedback {
            Some(f) => format!("Approved.\n\n{f}"),
            None => "Approved.".to_string(),
        })),
        "dismissed" => Ok(None),
        other => bail!("plannotator reported an unknown decision `{other}`"),
    }
}

/// The body of `POST /:repo/comments` for a whole-file plan comment.
///
/// `branch` is not optional. The viewer rejects a comment without one, and it
/// anchors the comment to that branch's tip, so a wrong branch is as good as no
/// comment: every reader asks for a branch by name.
///
/// No `line`: the decision record has no per-annotation anchors, so the comment
/// belongs to the file. No `author`: the viewer reads the caller's Tailscale
/// identity, which is truer than anything this side could claim.
pub fn comment_payload(file: &str, branch: &str, body: &str) -> Value {
    json!({ "branch": branch, "file": file, "body": body })
}

/// Everything needed to post one comment about a plan.
struct CommentTarget {
    /// The viewer's base URL, for the HTTP client.
    viewer: String,
    /// The dgit token, unused by the viewer but part of the client's shape.
    token: String,
    /// `<viewer>/<repo>/comments`.
    url: String,
    /// The plan's path as the viewer names it: relative, forward slashes.
    file: String,
    branch: String,
}

/// Where a decision about `path` would be posted. The `Err` side is the reason
/// nothing can be, written for the human who just finished annotating.
fn comment_target(ctx: &Ctx, path: &Path) -> std::result::Result<CommentTarget, String> {
    let (name, p) = ctx.profile().map_err(|e| e.to_string())?;
    let viewer = p
        .viewer_url
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("profile `{name}` has no viewer URL"))?
        .trim_end_matches('/')
        .to_string();
    let ws = vcs::detect_cwd()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not inside a git or jj repository".to_string())?;
    let repo = ws
        .origin_repo_name()
        .map_err(|e| e.to_string())?
        .or_else(|| ws.default_repo_name())
        .ok_or_else(|| "cannot tell which repository this is".to_string())?;
    let file = relative_path(&ws.root, path).ok_or_else(|| {
        format!(
            "{} is outside {}, and the viewer only knows files in the repository",
            path.display(),
            ws.root.display()
        )
    })?;
    // No branch is a dead end, not a detail to leave out: the viewer refuses a
    // comment without one, and when its mirror is down it accepts any name and
    // files the comment where nobody will look.
    let branch = ws.current_branch().ok_or_else(|| {
        "cannot tell which branch this plan is on, and a comment is anchored to one".to_string()
    })?;
    Ok(CommentTarget {
        url: format!("{viewer}/{}/comments", pct(&repo)),
        viewer,
        token: p.token.clone(),
        file,
        branch,
    })
}

/// `path` as the viewer names it: relative to the workspace root, forward
/// slashes. Both sides are canonicalised first, so `./plans/x.md` and an
/// absolute path give the same answer.
///
/// `None` when the file is not under the root. An absolute path posted as a
/// file name is accepted by the viewer and then shown by nothing, so the caller
/// has to treat that as a refusal rather than send it.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Some(abs.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/"))
}

/// A fresh empty directory under the system temp dir.
///
/// plannotator refuses to overwrite its `--result-file` and wants the parent to
/// exist, so we make the parent and let it make the file. `create_dir` fails
/// when the name is taken, which is the whole collision check: pid plus a
/// counter, retried.
fn scratch_dir() -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let base = std::env::temp_dir();
    // 0700 from the start, not chmodded after: the feedback sits in a shared
    // /tmp for as long as the human is writing it.
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    for _ in 0..64 {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("nashcode-annotate-{}-{n}", std::process::id()));
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", dir.display())),
        }
    }
    bail!("could not make a scratch directory under {}", base.display());
}

/// The plan's URL on the viewer, when the active profile names one.
fn viewer_plan_url(ctx: &Ctx, path: &Path) -> Result<Option<String>> {
    let (_, p) = ctx.profile()?;
    let Some(viewer) = p.viewer_url.as_deref() else {
        return Ok(None);
    };
    let Some(ws) = vcs::detect_cwd()? else {
        return Ok(None);
    };
    let Some(repo) = ws.origin_repo_name()?.or_else(|| ws.default_repo_name()) else {
        return Ok(None);
    };
    let Some(rel) = relative_path(&ws.root, path) else {
        return Ok(None);
    };
    Ok(Some(format!(
        "{}/{}/tree/{}",
        viewer.trim_end_matches('/'),
        repo,
        rel
    )))
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

// --- comments --------------------------------------------------------------

/// One row of the viewer's comment feed, normalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: i64,
    pub author: String,
    pub created_at: String,
    pub line: Option<i64>,
    pub body: String,
}

/// Percent-encode a query-string value. Only the unreserved set survives.
pub fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `GET /<repo>/comments?file=…&branch=…&since=…` on the viewer.
pub fn comments_url(
    viewer: &str,
    repo: &str,
    file: &str,
    branch: Option<&str>,
    since: Option<&str>,
) -> String {
    let mut url = format!(
        "{}/{}/comments?file={}",
        viewer.trim_end_matches('/'),
        pct(repo),
        pct(file)
    );
    if let Some(b) = branch {
        url.push_str(&format!("&branch={}", pct(b)));
    }
    if let Some(s) = since {
        url.push_str(&format!("&since={}", pct(s)));
    }
    url
}

/// Read the rows out of the viewer's answer.
///
/// The contract is "ordered rows with integer ids". The rows may arrive as a
/// bare array or wrapped in an object; both are accepted, and each field has a
/// couple of accepted spellings, so a viewer revision that renames `created`
/// to `created_at` does not break the command. Whatever arrives is still passed
/// through verbatim under `--json`.
pub fn parse_comments(body: &str) -> Result<(Value, Vec<Comment>)> {
    let value: Value =
        serde_json::from_str(body).context("the viewer's /comments answer is not JSON")?;
    let rows = match &value {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o
            .get("comments")
            .or_else(|| o.get("rows"))
            .or_else(|| o.get("data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let pick = |row: &Value, keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| row.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string()
    };
    let comments = rows
        .iter()
        .map(|row| Comment {
            id: row.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
            author: pick(row, &["author", "author_login", "user", "login"]),
            created_at: pick(row, &["created_at", "created", "at", "timestamp"]),
            line: row
                .get("line")
                .and_then(|v| v.as_i64())
                .or_else(|| row.get("line_number").and_then(|v| v.as_i64())),
            body: pick(row, &["body", "text", "comment"]),
        })
        .collect();
    Ok((value, comments))
}

/// One block per comment: a header line, then the body indented under it.
pub fn render_comments(comments: &[Comment]) -> Vec<String> {
    if comments.is_empty() {
        return vec!["no comments".to_string()];
    }
    let mut out = Vec::new();
    for (i, c) in comments.iter().enumerate() {
        if i > 0 {
            out.push(String::new());
        }
        let who = if c.author.is_empty() { "someone" } else { &c.author };
        let line = c.line.map(|l| format!(" line {l}")).unwrap_or_default();
        out.push(format!("#{} {who}{line} · {}", c.id, age_of(&c.created_at)));
        for l in c.body.lines() {
            out.push(format!("    {l}"));
        }
    }
    out
}

pub fn comments(ctx: &Ctx, args: &CommentsArgs) -> Result<()> {
    let (name, p) = ctx.profile()?;
    let Some(viewer) = p.viewer_url.as_deref().filter(|v| !v.is_empty()) else {
        bail!(
            "profile `{name}` has no viewer URL, and comments live in the viewer, not in dgit.\n\
             Deploy one with `nashcode setup --viewer`, or add `viewer_url` to the profile in {}.",
            crate::profile::config_path()?.display()
        );
    };

    let repo = match &args.repo {
        Some(r) => r.clone(),
        None => {
            let ws = vcs::require_cwd()?;
            ws.origin_repo_name()?
                .or_else(|| ws.default_repo_name())
                .ok_or_else(|| {
                    anyhow::anyhow!("cannot tell which repository this is; pass --repo")
                })?
        }
    };

    let url = comments_url(
        viewer,
        &repo,
        &args.file,
        args.branch.as_deref(),
        args.since.as_deref(),
    );
    let client = Client::new(viewer, &p.token);
    let reply = client.get(&url)?;
    if !reply.ok() {
        bail!("{url} returned HTTP {}\n{}", reply.status, reply.body.trim());
    }
    let (raw, comments) = parse_comments(&reply.body)?;

    // --json passes the viewer's answer through untouched: an agent should see
    // exactly what the API said, not this CLI's reading of it.
    if ctx.out.is_json() {
        ctx.out.json(&raw);
        return Ok(());
    }
    for line in render_comments(&comments) {
        ctx.out.line(line);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_and_dashed() {
        assert_eq!(slug("Replace the Parser!"), "replace-the-parser");
        assert_eq!(slug("  ...  "), "plan");
        assert_eq!(slug("v2 / rewrite"), "v2-rewrite");
    }

    #[test]
    fn the_url_encodes_the_file_path() {
        let u = comments_url("https://v/", "myrepo", "plans/a b.md", Some("main"), None);
        assert_eq!(
            u,
            "https://v/myrepo/comments?file=plans%2Fa%20b.md&branch=main"
        );
    }

    #[test]
    fn since_is_passed_through_as_given() {
        let u = comments_url("https://v", "r", "p.md", None, Some("2026-08-18T00:00:00Z"));
        assert!(u.ends_with("&since=2026-08-18T00%3A00%3A00Z"));
    }

    #[test]
    fn rows_parse_wrapped_or_bare() {
        let wrapped = r#"{"comments":[{"id":7,"author":"rob","created_at":"2026-08-18T00:00:00Z","line":12,"body":"no"}]}"#;
        let (_, c) = parse_comments(wrapped).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].id, 7);
        assert_eq!(c[0].line, Some(12));

        let bare = r#"[{"id":1,"user":"ann","at":"2026-08-18T00:00:00Z","text":"yes"}]"#;
        let (_, c) = parse_comments(bare).unwrap();
        assert_eq!(c[0].author, "ann");
        assert_eq!(c[0].body, "yes");
    }

    #[test]
    fn each_decision_maps_to_the_body_the_agent_will_read() {
        assert_eq!(
            decision_body(r#"{"decision":"annotated","feedback":"line 4 is wrong"}"#).unwrap(),
            Some("line 4 is wrong".to_string())
        );
        assert_eq!(
            decision_body("{\"decision\":\"approved\",\"feedback\":\"ship it\"}\n").unwrap(),
            Some("Approved.\n\nship it".to_string())
        );
        assert_eq!(
            decision_body(r#"{"decision":"approved"}"#).unwrap(),
            Some("Approved.".to_string())
        );
        assert_eq!(decision_body(r#"{"decision":"dismissed"}"#).unwrap(), None);
    }

    #[test]
    fn an_approval_with_blank_feedback_is_a_bare_approval() {
        assert_eq!(
            decision_body(r#"{"decision":"approved","feedback":"   "}"#).unwrap(),
            Some("Approved.".to_string())
        );
    }

    #[test]
    fn a_decision_that_cannot_be_read_is_an_error() {
        assert!(decision_body("").is_err());
        assert!(decision_body("   \n").is_err());
        assert!(decision_body("not json").is_err());
        assert!(decision_body(r#"{"decision":"annotated"}"#).is_err());
        assert!(decision_body(r#"{"decision":"pondered"}"#).is_err());
    }

    #[test]
    fn the_comment_payload_is_whole_file_and_unauthored() {
        let v = comment_payload("plans/x.md", "review/x", "Approved.");
        assert_eq!(v["file"], "plans/x.md");
        assert_eq!(v["branch"], "review/x");
        assert_eq!(v["body"], "Approved.");
        let o = v.as_object().unwrap();
        assert!(!o.contains_key("line"));
        assert!(!o.contains_key("author"));
        assert_eq!(o.len(), 3);
    }

    #[test]
    fn the_file_is_named_relative_to_the_workspace_root() {
        // Real directories, so canonicalize does its job instead of falling
        // through: on macOS the temp dir is a symlink, and an uncanonicalised
        // root would never be a prefix of the canonical file.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join("plans")).unwrap();
        let plan = root.join("plans").join("x.md");
        std::fs::write(&plan, "# x").unwrap();
        assert_eq!(relative_path(&root, &plan).unwrap(), "plans/x.md");

        // The same file named the way a human types it from inside the repo.
        let dotted = root.join(".").join("plans").join("x.md");
        assert_eq!(relative_path(&root, &dotted).unwrap(), "plans/x.md");
    }

    #[test]
    fn a_file_outside_the_workspace_has_no_name_the_viewer_would_understand() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("elsewhere.md");
        std::fs::write(&outside, "# x").unwrap();
        assert_eq!(relative_path(&root, &outside), None);
    }

    #[test]
    fn scratch_directories_are_fresh_and_distinct() {
        let a = scratch_dir().unwrap();
        let b = scratch_dir().unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        assert!(!a.join("decision.json").exists());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn rendering_indents_the_body_under_its_header() {
        let c = vec![Comment {
            id: 3,
            author: "rob".into(),
            created_at: "not-a-date".into(),
            line: Some(4),
            body: "first\nsecond".into(),
        }];
        let lines = render_comments(&c);
        assert_eq!(lines[0], "#3 rob line 4 · not-a-date");
        assert_eq!(lines[1], "    first");
        assert_eq!(lines[2], "    second");
        assert_eq!(render_comments(&[]), vec!["no comments"]);
    }
}
