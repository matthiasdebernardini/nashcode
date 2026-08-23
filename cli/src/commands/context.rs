//! `nashcode context`: the setter and the getter for the context store.
//!
//! A meeting, an email, a pasted chat, or a note becomes one committed file in the
//! repository it is about. This command is the whole client side of that: `put` files
//! one item, `ls` walks what is filed, `get` reads one back. The digest that turns
//! those files into `brain/entities/` runs on the operator's machine, not here.
//!
//! `put` is safe to re-run. An item with a `--source` — a Gmail message id, a chat
//! thread plus day, a URL — always names the same file, so a pusher that crashed
//! before it wrote its marker earns `existing: true` on the next pass instead of a
//! duplicate.

use super::Ctx;
use crate::api::Client;
use crate::cli::{ContextGetArgs, ContextLsArgs, ContextPutArgs};
use crate::commands::plan::pct;
use crate::exit::{Class, classed};
use crate::vcs;
use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use std::io::Read;

/// The kinds the viewer files. Checked here so a typo costs nothing and says what the
/// four are, rather than travelling to the server to be told.
pub const KINDS: [&str; 4] = ["meeting", "email", "chat", "note"];

/// `nashcode context put <kind> [file] --title <t> [--at <rfc3339>] [--source <id>]`.
///
/// A `meeting` is the browser extension's transcript JSON and carries its own title
/// and times, so the flags are ignored for it and the body travels verbatim.
pub fn put(ctx: &Ctx, args: &ContextPutArgs) -> Result<Value> {
    let kind = kind_of(&args.kind)?;
    let (viewer, token) = viewer_of(ctx)?;
    let repo = repo_of(ctx, args.repo.as_deref())?;

    let text = read_input(args.file.as_deref())?;
    let body = if kind == "meeting" {
        // The extension already produced the whole payload. Parsing it here would only
        // add a second opinion about a shape the viewer validates anyway.
        serde_json::from_str::<Value>(&text)
            .map_err(|e| classed(Class::Usage, format!("a meeting is transcript JSON: {e}")))?
    } else {
        let title = args.title.clone().unwrap_or_default();
        if title.trim().is_empty() {
            return Err(classed(
                Class::Usage,
                format!("give the item a title: nashcode context put {kind} --title \"Re: invoice\""),
            ));
        }
        if text.trim().is_empty() {
            return Err(classed(Class::Usage, "there is no text to file"));
        }
        let mut body = json!({
            "title": title,
            "at": args.at.clone().unwrap_or_else(crate::timefmt::now_rfc3339),
            "text": text,
        });
        if let Some(source) = &args.source {
            body["source"] = json!(source);
        }
        body
    };

    let url = format!("{viewer}/{}/context/{kind}", pct(&repo));
    let client = Client::new(&viewer, &token);
    let reply = client.post_url(&url, &body.to_string())?;
    if !reply.ok() {
        return Err(http_error(&url, reply.status, &reply.body));
    }
    let mut value: Value = serde_json::from_str(&reply.body)
        .context("the viewer's answer to a context put is not JSON")?;
    value["repo"] = json!(repo);
    value["kind"] = json!(kind);
    // Always present, so a caller branches on a field rather than on its absence.
    if value.get("existing").is_none() {
        value["existing"] = json!(false);
    }
    Ok(value)
}

/// `nashcode context ls [--kind <kind>] [--since <cursor>]`.
///
/// `since` is the `next_since` of a previous answer and is strictly exclusive, so
/// handing it back is the whole of the polling loop: nothing repeats, and a backfilled
/// item — one whose `at` is older than everything around it — still arrives.
pub fn ls(ctx: &Ctx, args: &ContextLsArgs) -> Result<Value> {
    let (viewer, token) = viewer_of(ctx)?;
    let repo = repo_of(ctx, args.repo.as_deref())?;
    let mut url = format!("{viewer}/{}/context", pct(&repo));
    let mut sep = '?';
    if let Some(kind) = &args.kind {
        let kind = kind_of(kind)?;
        url.push_str(&format!("{sep}kind={}", pct(kind)));
        sep = '&';
    }
    if let Some(since) = &args.since {
        url.push_str(&format!("{sep}since={}", pct(since)));
    }

    let client = Client::new(&viewer, &token);
    let reply = client.get_json(&url)?;
    if !reply.ok() {
        return Err(http_error(&url, reply.status, &reply.body));
    }
    let mut value: Value = serde_json::from_str(&reply.body)
        .context("the viewer's answer to a context list is not JSON")?;
    value["repo"] = json!(repo);
    Ok(value)
}

/// `nashcode context get <kind> <id>`.
pub fn get(ctx: &Ctx, args: &ContextGetArgs) -> Result<Value> {
    let kind = kind_of(&args.kind)?;
    if args.id.trim().is_empty() {
        return Err(classed(
            Class::Usage,
            format!("name the item: nashcode context get {kind} <id>"),
        ));
    }
    let (viewer, token) = viewer_of(ctx)?;
    let repo = repo_of(ctx, args.repo.as_deref())?;
    let url = format!("{viewer}/{}/context/{kind}/{}", pct(&repo), pct(&args.id));

    let client = Client::new(&viewer, &token);
    let reply = client.get_json(&url)?;
    if !reply.ok() {
        return Err(http_error(&url, reply.status, &reply.body));
    }
    let mut value: Value = serde_json::from_str(&reply.body)
        .context("the viewer's answer to a context get is not JSON")?;
    value["repo"] = json!(repo);
    Ok(value)
}

/// One of the four, or a usage error naming all four.
fn kind_of(kind: &str) -> Result<&'static str> {
    KINDS.iter().find(|known| **known == kind.trim()).copied().ok_or_else(|| {
        classed(
            Class::Usage,
            format!("no context kind named '{kind}'; the kinds are {}", KINDS.join(", ")),
        )
    })
}

/// The item's text: the named file, or standard input when no file is named.
fn read_input(file: Option<&str>) -> Result<String> {
    match file {
        Some(path) => {
            let path = std::path::Path::new(path);
            if !path.exists() {
                return Err(classed(Class::NotFound, format!("{} does not exist", path.display())));
            }
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
        }
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("read the item from standard input")?;
            Ok(text)
        }
    }
}

/// The viewer this profile names. Context lives in the viewer, not in dgit.
fn viewer_of(ctx: &Ctx) -> Result<(String, String)> {
    crate::commands::brain::viewer_url(ctx).map_err(|why| classed(Class::NotFound, why))
}

/// The repository the item is about: the flag, else whatever `origin` points at.
fn repo_of(_ctx: &Ctx, named: Option<&str>) -> Result<String> {
    if let Some(name) = named.map(str::trim).filter(|name| !name.is_empty()) {
        return Ok(name.to_owned());
    }
    let ws = vcs::require_cwd()?;
    ws.origin_repo_name()?
        .or_else(|| ws.default_repo_name())
        .ok_or_else(|| classed(Class::Usage, "cannot tell which repository this is; pass --repo"))
}

/// The viewer's status code, as a class an agent branches on.
///
/// A 400 is this invocation's fault, a 404 is a name that is not there, and everything
/// else — a 409 the server could not resolve, a 502 from a mirror that will not talk —
/// is the deployment's, which is what `nashcode doctor` is for.
fn http_error(url: &str, status: u16, body: &str) -> anyhow::Error {
    let class = match status {
        400 => Class::Usage,
        401 | 403 => Class::Auth,
        404 => Class::NotFound,
        _ => Class::Api,
    };
    let reason = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned());
    classed(class, format!("{url} returned HTTP {status}\n{reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_four_kinds_are_accepted() {
        assert_eq!(kind_of("email").unwrap(), "email");
        assert_eq!(kind_of("  note  ").unwrap(), "note");
        let err = kind_of("voicemail").unwrap_err();
        assert!(format!("{err}").contains("meeting, email, chat, note"), "{err}");
        assert_eq!(crate::exit::class_of(&err), Some(Class::Usage));
    }

    #[test]
    fn the_status_decides_the_class_not_the_body() {
        // A reviewer's words in a 502 body must not turn it into a 404.
        let api = http_error("http://v/r/context", 502, r#"{"error":"does not exist"}"#);
        assert_eq!(crate::exit::class_of(&api), Some(Class::Api));
        assert!(format!("{api}").contains("does not exist"));

        let usage = http_error("http://v/r/context", 400, r#"{"error":"text is empty"}"#);
        assert_eq!(crate::exit::class_of(&usage), Some(Class::Usage));

        let missing = http_error("http://v/r/context", 404, "");
        assert_eq!(crate::exit::class_of(&missing), Some(Class::NotFound));

        // A body that is not JSON still reads as itself.
        let raw = http_error("http://v/r/context", 409, "  too many share this id  ");
        assert!(format!("{raw}").ends_with("too many share this id"), "{raw}");
    }

    #[test]
    fn a_named_file_that_is_not_there_is_not_found_rather_than_an_empty_item() {
        let err = read_input(Some("/no/such/file/anywhere.txt")).unwrap_err();
        assert_eq!(crate::exit::class_of(&err), Some(Class::NotFound));
    }
}
