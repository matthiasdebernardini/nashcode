//! Plans: create one, annotate it locally, read the replies back.
//!
//! A plan is a markdown file under `plans/` at the root of a repository. That
//! is the whole format. The viewer renders the directory and collects comments
//! on the rendered page, so `plans/` is where an agent writes what it intends
//! to do and `nashcode comments` is how it hears back.

use super::Ctx;
use crate::api::Client;
use crate::cli::{AnnotateArgs, CommentsArgs, PlanNewArgs};
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

pub fn new(_ctx: &Ctx, args: &PlanNewArgs) -> Result<Value> {
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
    Ok(json!({ "path": path, "relative": rel, "title": title }))
}

pub fn annotate(ctx: &Ctx, args: &AnnotateArgs) -> Result<Value> {
    let path = PathBuf::from(&args.file);
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }

    // The viewer link is useful whether or not plannotator is installed.
    let viewer = viewer_plan_url(ctx, &path).unwrap_or(None);
    let found = which("plannotator");
    let where_it_is = match &found {
        Some(bin) => json!(bin.to_string_lossy()),
        None => Value::Null,
    };
    let base = json!({
        "file": args.file,
        "plannotator": where_it_is,
        "viewer_url": viewer,
    });

    let Some(bin) = found else {
        let mut value = base;
        value["launched"] = json!(false);
        value["status"] = json!("plannotator not installed");
        value["hint"] = json!(PLANNOTATOR_HINT);
        return Ok(value);
    };

    // `--no-launch` is the inspect-only form: say where everything is and open
    // nothing. Launching is the default now, because the agent runs this FOR a
    // human who is about to read the plan.
    if args.no_launch {
        let mut value = base;
        value["launched"] = json!(false);
        value["status"] = json!("not launched (--no-launch)");
        return Ok(value);
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
        // as publishing it. We read the file, and stdout belongs to the
        // envelope, so the child's copy is routed to /dev/null.
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

    let mut value = base;
    value["launched"] = json!(true);

    let decision = match decision_body(&raw) {
        Ok(d) => d,
        Err(e) => {
            // Unreadable, but still the human's words. Carry them in the error
            // rather than dropping them on the floor.
            return Err(e.context(format!("the decision plannotator wrote was: {}", raw.trim())));
        }
    };
    let Some(body) = decision else {
        value["status"] = json!("dismissed, nothing posted");
        return Ok(value);
    };
    value["feedback"] = json!(body);

    let target = match target {
        Ok(t) => t,
        Err(why) => {
            // The human's words outlive the plumbing. Return them, say why they
            // went nowhere, and succeed: nothing failed, there was just nowhere
            // to put them.
            value["status"] = json!("not posted");
            value["not_posted"] = json!(why);
            return Ok(value);
        }
    };

    let payload = comment_payload(&target.file, &target.branch, &body).to_string();
    let client = Client::new(&target.viewer, &target.token);
    let reply = match client.post_url(&target.url, &payload) {
        Ok(r) => r,
        Err(e) => return Err(e.context(unposted("post the annotation to the viewer", &body))),
    };
    if !reply.ok() {
        bail!(
            "{}",
            unposted(
                &format!(
                    "{} returned HTTP {}\n{}",
                    target.url,
                    reply.status,
                    reply.body.trim()
                ),
                &body
            )
        );
    }

    let id = serde_json::from_str::<Value>(&reply.body)
        .ok()
        .and_then(|v| v.get("id").and_then(|i| i.as_i64()));
    value["status"] = json!("posted");
    value["branch"] = json!(target.branch);
    value["comment_id"] = match id {
        Some(id) => json!(id),
        None => Value::Null,
    };
    Ok(value)
}

/// A failure message that still carries the feedback it could not deliver.
///
/// The envelope has no field for "here is the thing that failed to send", so it
/// rides in the message: losing ten minutes of somebody's review to a 400 is the
/// one outcome this command must never have.
fn unposted(why: &str, body: &str) -> String {
    format!("{why}\n\nunposted feedback:\n{body}")
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

pub fn comments(ctx: &Ctx, args: &CommentsArgs) -> Result<Vec<Value>> {
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
    rows_of(&reply.body)
}

/// The viewer's rows, verbatim, as a list of objects.
///
/// The wrapper the viewer chose (a bare array, or an object with `comments` /
/// `rows` / `data`) is this command's business, not the caller's; the rows
/// inside it are passed through untouched, so a field the viewer grows arrives
/// without a CLI release.
pub fn rows_of(body: &str) -> Result<Vec<Value>> {
    let value: Value =
        serde_json::from_str(body).context("the viewer's /comments answer is not JSON")?;
    Ok(match value {
        Value::Array(a) => a,
        Value::Object(o) => o
            .get("comments")
            .or_else(|| o.get("rows"))
            .or_else(|| o.get("data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    })
}

/// The newest `created_at` across the rows, for the `--since` an agent should
/// poll with next. Timestamps are RFC 3339 with a fixed offset, so the string
/// order is the time order.
pub fn newest_timestamp(rows: &[Value]) -> Option<String> {
    rows.iter()
        .filter_map(|row| {
            ["created_at", "created", "at", "timestamp"]
                .iter()
                .find_map(|k| row.get(*k).and_then(Value::as_str))
        })
        .filter(|s| !s.is_empty())
        .max()
        .map(str::to_string)
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
    fn rows_arrive_wrapped_or_bare_and_are_passed_through_untouched() {
        let wrapped = r#"{"comments":[{"id":7,"author":"rob","created_at":"2026-08-18T00:00:00Z","line":12,"body":"no"}]}"#;
        let rows = rows_of(wrapped).unwrap();
        assert_eq!(rows.len(), 1);
        // Untouched: the viewer's own keys, not a normalised copy of them.
        assert_eq!(rows[0]["id"], 7);
        assert_eq!(rows[0]["line"], 12);
        assert_eq!(rows[0]["author"], "rob");

        let bare = r#"[{"id":1,"user":"ann","at":"2026-08-18T00:00:00Z","text":"yes"}]"#;
        let rows = rows_of(bare).unwrap();
        assert_eq!(rows[0]["user"], "ann");
        assert_eq!(rows[0]["text"], "yes");

        assert!(rows_of("not json").is_err());
        assert!(rows_of("{}").unwrap().is_empty());
    }

    #[test]
    fn the_newest_timestamp_is_what_an_agent_polls_from_next() {
        let rows = rows_of(
            r#"[{"id":1,"created_at":"2026-08-18T00:00:00Z"},
                {"id":2,"created_at":"2026-08-19T09:30:00Z"},
                {"id":3,"created":"2026-08-17T00:00:00Z"}]"#,
        )
        .unwrap();
        assert_eq!(
            newest_timestamp(&rows),
            Some("2026-08-19T09:30:00Z".to_string())
        );
        assert_eq!(newest_timestamp(&[]), None);
        // A row with no readable timestamp cannot invent one.
        assert_eq!(newest_timestamp(&rows_of(r#"[{"id":1}]"#).unwrap()), None);
    }
}
