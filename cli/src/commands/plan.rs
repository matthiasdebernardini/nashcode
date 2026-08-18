//! Plans: create one, annotate it locally, read the replies back.
//!
//! A plan is a markdown file under `plans/` at the root of a repository. That
//! is the whole format. The viewer renders the directory and collects comments
//! on the rendered page, so `plans/` is where an agent writes what it intends
//! to do and `nashgit comments` is how it hears back.

use super::Ctx;
use crate::api::Client;
use crate::cli::{AnnotateArgs, CommentsArgs, PlanNewArgs};
use crate::timefmt::age_of;
use crate::vcs;
use anyhow::{Context, Result, bail};
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
        bail!("give the plan a title: nashgit plan new \"replace the parser\"");
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

    if let Some(u) = &viewer {
        ctx.out.step(u);
    }
    if ctx.out.is_json() {
        ctx.out.json(&json!({
            "file": args.file,
            "plannotator": bin.to_string_lossy(),
            "viewer_url": viewer,
        }));
        return Ok(());
    }
    let status = std::process::Command::new(&bin)
        .arg("annotate")
        .arg(&path)
        .status()
        .with_context(|| format!("run {}", bin.display()))?;
    if !status.success() {
        bail!("plannotator exited {}", status.code().unwrap_or(-1));
    }
    Ok(())
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
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(&ws.root).unwrap_or_else(|_| ws.root.clone());
    let rel = abs.strip_prefix(&root).unwrap_or(&abs).to_string_lossy();
    Ok(Some(format!(
        "{}/{}/tree/{}",
        viewer.trim_end_matches('/'),
        repo,
        rel.replace('\\', "/")
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
             Deploy one with `nashgit setup --viewer`, or add `viewer_url` to the profile in {}.",
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
