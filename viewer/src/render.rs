//! Markdown rendering with nashcode's autolinks.
//!
//! Rendered markdown gets two link passes on top of pulldown-cmark:
//! - a bare token that names a file that exists under `plans/` or `tasks/` becomes a
//!   link to that file's rendered page;
//! - a backticked token that names an existing branch becomes a link to that branch's
//!   PR view.
//!
//! Both passes only ever *add* links; they never change the text.
//!
//! The wiki adds a third pass, and it is the one exception: a page rendered through
//! [`markdown_in_docs`] resolves the author's *relative* links against the document's
//! own directory, so `../guide.md` next to it on GitHub reaches the same file here.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::docs::DocIndex;

/// Where a repo-relative document path renders. `plans/a.md` -> `/{repo}/plans/a.md`,
/// `tasks/b.md` -> `/{repo}/tasks/b.md`; anything else has no page of its own.
pub fn doc_url(repo: &str, path: &str) -> Option<String> {
    if path.starts_with("plans/") || path.starts_with("tasks/") {
        Some(format!("/{repo}/{path}"))
    } else {
        None
    }
}

pub fn branch_url(repo: &str, branch: &str) -> String {
    format!("/{repo}/{branch}")
}

/// Percent-encode a repo-relative path for a URL, one segment at a time.
///
/// `/` stays a separator; everything outside the unreserved set is escaped. Without
/// this a file named `a#b.md` truncates at the fragment and `notes v2.md` breaks on the
/// space. Topcoat decodes catch-all segments individually, so the round trip is exact.
pub fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Where a markdown file reads in the wiki. Every markdown file in the repo has one.
pub fn docs_url(repo: &str, path: &str) -> String {
    format!("/{repo}/docs/{}", encode_path(path))
}

/// Where a file reads in the code browser.
pub fn blob_url(repo: &str, path: &str) -> String {
    format!("/{repo}/blob/{}", encode_path(path))
}

/// Is this path a markdown file, by its extension?
pub fn is_markdown_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// Render markdown to HTML, autolinking against the repo's document index and branch
/// list when they are provided.
pub fn markdown(source: &str, repo: &str, index: Option<&DocIndex>, branches: &[String]) -> String {
    markdown_with(source, repo, index, branches, None, "")
}

/// Render one wiki page. `dir` is the document's own repo-relative directory (empty at
/// the root), and relative links resolve against it: markdown targets go to `/docs/`,
/// everything else to `/blob/`. Relative image sources go to `/raw/{branch}/`, because
/// an `<img>` needs the bytes and not a page about them.
pub fn markdown_in_docs(
    source: &str,
    repo: &str,
    index: Option<&DocIndex>,
    branches: &[String],
    dir: &str,
    branch: &str,
) -> String {
    markdown_with(source, repo, index, branches, Some(dir), branch)
}

fn markdown_with(
    source: &str,
    repo: &str,
    index: Option<&DocIndex>,
    branches: &[String],
    docs_dir: Option<&str>,
    raw_branch: &str,
) -> String {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(source, options);

    let mut events: Vec<Event<'_>> = Vec::new();
    // Autolinking must not touch text inside code blocks or existing links.
    let mut verbatim_depth = 0usize;

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_)))
            | Event::Start(Tag::CodeBlock(CodeBlockKind::Indented)) => {
                verbatim_depth += 1;
                events.push(event);
            }
            // Author-supplied link destinations are untrusted: a `javascript:` (or
            // `data:` etc.) href is click-gated script execution. Unsafe schemes are
            // replaced, never passed through.
            Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
                verbatim_depth += 1;
                let dest_url = if !allowed_url(&dest_url) {
                    "#".into()
                } else if let Some(dir) = docs_dir
                    && let Some(rewritten) = resolve_doc_link(&dest_url, repo, dir)
                {
                    rewritten.into()
                } else {
                    dest_url
                };
                events.push(Event::Start(Tag::Link { link_type, dest_url, title, id }));
            }
            // An image needs the file's *bytes*, not a page about it, so a relative
            // src resolves to the raw endpoint rather than to /blob/.
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                verbatim_depth += 1;
                let dest_url = if !allowed_url(&dest_url) {
                    "".into()
                } else if let Some(dir) = docs_dir
                    && let Some(rewritten) = resolve_doc_image(&dest_url, repo, dir, raw_branch)
                {
                    rewritten.into()
                } else {
                    dest_url
                };
                events.push(Event::Start(Tag::Image { link_type, dest_url, title, id }));
            }
            Event::End(TagEnd::CodeBlock) | Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                verbatim_depth = verbatim_depth.saturating_sub(1);
                events.push(event);
            }
            Event::Text(text) if verbatim_depth == 0 && index.is_some() => {
                autolink_text(&text, repo, index.expect("checked"), &mut events);
            }
            Event::Code(code) if verbatim_depth == 0 => {
                let token = code.trim();
                if branches.iter().any(|b| b == token) {
                    events.push(Event::Html(
                        format!(
                            "<a href=\"{}\"><code>{}</code></a>",
                            escape_attr(&branch_url(repo, token)),
                            escape_text(token)
                        )
                        .into(),
                    ));
                } else {
                    events.push(Event::Code(code));
                }
            }
            // Raw HTML in the source is untrusted: comment bodies arrive through a
            // public POST API and plan/card bodies through a push. Re-emitting these
            // events as text makes push_html escape them.
            Event::Html(html) => events.push(Event::Text(html)),
            Event::InlineHtml(html) => events.push(Event::Text(html)),
            other => events.push(other),
        }
    }

    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());
    html
}

/// Is this link destination safe to render as a live href/src?
///
/// Allowed: `http:`, `https:`, `mailto:`, and scheme-less (relative or fragment)
/// URLs. Everything else — `javascript:`, `data:`, `vbscript:`, `file:`, unknown
/// schemes — is refused. Detection mirrors what browsers tolerate before scheme
/// parsing: tabs and newlines are removed anywhere, leading and trailing controls
/// and spaces are stripped, and the scheme match is case-insensitive.
fn allowed_url(url: &str) -> bool {
    let cleaned: String = url.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let cleaned = cleaned.trim_matches(|c: char| c.is_control() || c == ' ');
    let lower = cleaned.to_ascii_lowercase();
    match lower.find(':') {
        // A colon only introduces a scheme when it comes before any /, ?, or #.
        Some(colon) if colon < lower.find(['/', '?', '#']).unwrap_or(usize::MAX) => {
            matches!(&lower[..colon], "http" | "https" | "mailto")
        }
        _ => true,
    }
}

/// Does this destination carry a scheme (`https:`, `mailto:`)? A colon only introduces
/// one when it comes before any `/`, `?`, or `#`.
fn has_scheme(url: &str) -> bool {
    match url.find(':') {
        Some(colon) => colon < url.find(['/', '?', '#']).unwrap_or(usize::MAX),
        None => false,
    }
}

/// Resolve one relative destination inside a wiki page against the page's directory.
///
/// Returns `(repo-relative path, the #fragment or ?query that rode along)`. Absolute
/// URLs, site-root paths, and bare fragments get `None` — they are already correct.
/// So does anything that would climb above the repo root, because there is nothing
/// there and rewriting it would only hide the author's mistake.
fn resolve_in_repo<'a>(dest: &'a str, dir: &str) -> Option<(String, &'a str)> {
    if dest.is_empty() || dest.starts_with('#') || dest.starts_with('/') || has_scheme(dest) {
        return None;
    }
    let cut = dest.find(['#', '?']).unwrap_or(dest.len());
    let (path, suffix) = dest.split_at(cut);
    if path.is_empty() {
        return None;
    }

    let mut segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    let resolved = segments.join("/");
    (!resolved.is_empty()).then_some((resolved, suffix))
}

/// Rewrite one relative link inside a wiki page to the viewer's own URL for it.
///
/// Markdown targets land on `/docs/`, everything else on `/blob/`.
fn resolve_doc_link(dest: &str, repo: &str, dir: &str) -> Option<String> {
    let (resolved, suffix) = resolve_in_repo(dest, dir)?;
    let base = if is_markdown_path(&resolved) {
        docs_url(repo, &resolved)
    } else {
        blob_url(repo, &resolved)
    };
    Some(format!("{base}{suffix}"))
}

/// Rewrite one relative image source to the raw endpoint, which serves bytes.
///
/// Without a branch to serve from there is no raw URL to build, so the source is left
/// as the author wrote it.
fn resolve_doc_image(dest: &str, repo: &str, dir: &str, branch: &str) -> Option<String> {
    if branch.is_empty() {
        return None;
    }
    let (resolved, suffix) = resolve_in_repo(dest, dir)?;
    Some(format!(
        "/{repo}/raw/{}/{}{suffix}",
        encode_path(branch),
        encode_path(&resolved)
    ))
}

/// Split a text node on whitespace and link every token that names an existing
/// plan or card file.
fn autolink_text<'a>(text: &str, repo: &str, index: &DocIndex, events: &mut Vec<Event<'a>>) {
    let mut out = String::new();
    let mut rest = text;
    let mut linked_any = false;

    while !rest.is_empty() {
        let token_start = rest.find(|c: char| !c.is_whitespace());
        let Some(start) = token_start else {
            out.push_str(&escape_text(rest));
            break;
        };
        out.push_str(&escape_text(&rest[..start]));
        rest = &rest[start..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let raw_token = &rest[..end];
        rest = &rest[end..];

        // Trailing punctuation is prose, not path.
        let token = raw_token.trim_end_matches(['.', ',', ';', ':', ')', ']', '!', '?']);
        let trailer = &raw_token[token.len()..];

        let linkable = (token.starts_with("plans/") || token.starts_with("tasks/"))
            && index.exists(token);
        match doc_url(repo, token) {
            Some(url) if linkable => {
                out.push_str(&format!(
                    "<a href=\"{}\">{}</a>",
                    escape_attr(&url),
                    escape_text(token)
                ));
                linked_any = true;
            }
            _ => out.push_str(&escape_text(token)),
        }
        out.push_str(&escape_text(trailer));
    }

    if linked_any {
        events.push(Event::Html(out.into()));
    } else {
        // No links added: keep the original text event so pulldown escapes it itself.
        events.push(Event::Text(text.to_owned().into()));
    }
}

pub fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn escape_attr(text: &str) -> String {
    escape_text(text).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn index_with(paths: &[&str]) -> DocIndex {
        DocIndex {
            commit: "abc".into(),
            documents: BTreeMap::new(),
            all_paths: paths.iter().map(|p| (*p).to_owned()).collect::<BTreeSet<_>>(),
            ..DocIndex::default()
        }
    }

    #[test]
    fn a_known_path_token_becomes_a_link() {
        let index = index_with(&["plans/api.md"]);
        let html = markdown("see plans/api.md.", "demo", Some(&index), &[]);
        assert!(html.contains("<a href=\"/demo/plans/api.md\">plans/api.md</a>"), "{html}");
        // The trailing period stays plain text.
        assert!(html.contains("</a>."));
    }

    #[test]
    fn an_unknown_path_stays_plain() {
        let index = index_with(&[]);
        let html = markdown("see plans/nope.md", "demo", Some(&index), &[]);
        assert!(!html.contains("<a "), "{html}");
    }

    #[test]
    fn a_backticked_branch_links_to_the_pr_view() {
        let html = markdown("merge `feat/x` soon", "demo", None, &["feat/x".to_owned()]);
        assert!(html.contains("<a href=\"/demo/feat/x\"><code>feat/x</code></a>"), "{html}");
    }

    #[test]
    fn code_blocks_are_left_alone() {
        let index = index_with(&["plans/api.md"]);
        let html = markdown("```\nplans/api.md\n```", "demo", Some(&index), &[]);
        assert!(!html.contains("<a "), "{html}");
    }

    #[test]
    fn a_backticked_non_branch_stays_code() {
        let html = markdown("`not-a-branch`", "demo", None, &["main".to_owned()]);
        assert!(html.contains("<code>not-a-branch</code>"));
        assert!(!html.contains("<a "), "{html}");
    }

    #[test]
    fn raw_block_html_is_escaped_not_executed() {
        let html = markdown("<script>alert(1)</script>", "demo", None, &[]);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn raw_inline_html_is_escaped_not_executed() {
        let html = markdown("look <img src=x onerror=alert(1)> here", "demo", None, &[]);
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"), "{html}");
    }

    #[test]
    fn unsafe_link_schemes_are_refused_and_safe_ones_kept() {
        // Refused, including the browser-tolerated obfuscations.
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "  javascript:alert(1)",
            "\u{1}javascript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "file:///etc/passwd",
            "totally-made-up:payload",
        ] {
            assert!(!allowed_url(bad), "must refuse {bad:?}");
        }
        // Kept.
        for good in [
            "https://example.invalid/x",
            "http://example.invalid",
            "mailto:ada@example.invalid",
            "/demo/plans/api.md",
            "plans/api.md",
            "#section",
            "?branch=main",
            "/path/with:colon",
        ] {
            assert!(allowed_url(good), "must keep {good:?}");
        }
    }

    #[test]
    fn a_javascript_link_renders_without_its_scheme() {
        let html = markdown("[click me](javascript:alert(1))", "demo", None, &[]);
        assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
        assert!(html.contains("click me"), "the text survives: {html}");
        assert!(html.contains("href=\"#\""), "neutralized to a dead href: {html}");
    }

    #[test]
    fn a_data_url_link_is_neutralized() {
        let html = markdown("[x](data:text/html;base64,PHNjcmlwdD4=)", "demo", None, &[]);
        assert!(!html.contains("data:"), "{html}");
    }

    #[test]
    fn an_entity_obfuscated_scheme_is_neutralized() {
        // &#9; decodes to a tab inside the destination; browsers strip it.
        let html = markdown("[x](&#9;javascript:alert(1))", "demo", None, &[]);
        assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
    }

    #[test]
    fn an_image_with_a_javascript_src_is_neutralized() {
        let html = markdown("![alt](javascript:alert(1))", "demo", None, &[]);
        assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
    }

    #[test]
    fn ordinary_links_and_images_still_work() {
        let html = markdown(
            "[a](https://example.invalid) [b](/demo/x) [c](mailto:x@example.invalid) ![i](https://example.invalid/i.png)",
            "demo",
            None,
            &[],
        );
        assert!(html.contains("href=\"https://example.invalid\""), "{html}");
        assert!(html.contains("href=\"/demo/x\""), "{html}");
        assert!(html.contains("href=\"mailto:x@example.invalid\""), "{html}");
        assert!(html.contains("src=\"https://example.invalid/i.png\""), "{html}");
    }

    #[test]
    fn wiki_relative_links_resolve_against_the_pages_own_directory() {
        let html = markdown_in_docs("[a](sibling.md) [b](../top.md)", "demo", None, &[], "docs/deep", "main");
        assert!(html.contains("href=\"/demo/docs/docs/deep/sibling.md\""), "{html}");
        assert!(html.contains("href=\"/demo/docs/docs/top.md\""), "{html}");
    }

    #[test]
    fn a_wiki_link_to_a_non_markdown_file_goes_to_the_blob_view() {
        let html = markdown_in_docs("[src](../src/lib.rs)", "demo", None, &[], "docs", "main");
        assert!(html.contains("href=\"/demo/blob/src/lib.rs\""), "{html}");
    }

    #[test]
    fn a_wiki_link_keeps_its_fragment_and_leaves_absolute_links_alone() {
        let html = markdown_in_docs(
            "[a](guide.md#setup) [b](/demo/stacks) [c](https://example.invalid/x) [d](#here)",
            "demo",
            None,
            &[],
            "docs",
            "main",
        );
        assert!(html.contains("href=\"/demo/docs/docs/guide.md#setup\""), "{html}");
        assert!(html.contains("href=\"/demo/stacks\""), "{html}");
        assert!(html.contains("href=\"https://example.invalid/x\""), "{html}");
        assert!(html.contains("href=\"#here\""), "{html}");
    }

    #[test]
    fn a_wiki_link_climbing_above_the_repo_root_is_left_as_written() {
        let html = markdown_in_docs("[x](../../etc/passwd)", "demo", None, &[], "docs", "main");
        assert!(html.contains("href=\"../../etc/passwd\""), "{html}");
    }

    #[test]
    fn plans_pages_still_leave_relative_links_alone() {
        let html = markdown("[a](sibling.md)", "demo", None, &[]);
        assert!(html.contains("href=\"sibling.md\""), "{html}");
    }

    #[test]
    fn wiki_images_resolve_to_the_raw_endpoint() {
        let html = markdown_in_docs(
            "![a](diagram.png) ![b](../assets/logo.svg)",
            "demo",
            None,
            &[],
            "docs",
            "main",
        );
        assert!(html.contains("src=\"/demo/raw/main/docs/diagram.png\""), "{html}");
        assert!(html.contains("src=\"/demo/raw/main/assets/logo.svg\""), "{html}");
    }

    #[test]
    fn an_absolute_image_and_an_unsafe_one_are_left_to_the_existing_rules() {
        let html = markdown_in_docs(
            "![a](https://example.invalid/i.png) ![b](javascript:alert(1)) ![c](/demo/raw/main/x.png)",
            "demo",
            None,
            &[],
            "docs",
            "main",
        );
        assert!(html.contains("src=\"https://example.invalid/i.png\""), "{html}");
        assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
        assert!(html.contains("src=\"/demo/raw/main/x.png\""), "{html}");
    }

    #[test]
    fn a_generated_url_escapes_the_characters_that_would_truncate_it() {
        // A real filename holding one of these used to produce a URL that stopped
        // early: `a#b.md` became a fragment, `notes v2.md` broke on the space.
        assert_eq!(encode_path("docs/a#b.md"), "docs/a%23b.md");
        assert_eq!(encode_path("notes v2.md"), "notes%20v2.md");
        assert_eq!(encode_path("a?b/c.md"), "a%3Fb/c.md");
        assert_eq!(encode_path("caf\u{e9}.md"), "caf%C3%A9.md");
        assert_eq!(docs_url("demo", "docs/a#b.md"), "/demo/docs/docs/a%23b.md");
        assert_eq!(blob_url("demo", "a b.rs"), "/demo/blob/a%20b.rs");
        // Ordinary paths come through untouched, so every existing URL is unchanged.
        assert_eq!(encode_path("src/web/pages.rs"), "src/web/pages.rs");
        assert_eq!(encode_path("a-b_c.d~e/f.md"), "a-b_c.d~e/f.md");
    }

    #[test]
    fn a_hash_an_author_typed_in_a_link_is_still_a_fragment() {
        // The other direction, unchanged: in markdown, `#` in a destination means a
        // fragment. An author who means a literal hash escapes it themselves.
        let html = markdown_in_docs("[x](guide.md#setup)", "demo", None, &[], "docs", "main");
        assert!(html.contains("href=\"/demo/docs/docs/guide.md#setup\""), "{html}");
    }

    #[test]
    fn html_inside_an_autolinked_document_is_still_escaped() {
        let index = index_with(&["plans/api.md"]);
        let html = markdown(
            "see plans/api.md <script>alert(1)</script>",
            "demo",
            Some(&index),
            &[],
        );
        assert!(html.contains("<a href=\"/demo/plans/api.md\">"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }
}
