//! Markdown rendering with nashcode's autolinks.
//!
//! Rendered markdown gets two link passes on top of pulldown-cmark:
//! - a bare token that names a file that exists under `plans/` or `tasks/` becomes a
//!   link to that file's rendered page;
//! - a backticked token that names an existing branch becomes a link to that branch's
//!   PR view.
//!
//! Both passes only ever *add* links; they never change the text.

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

/// Render markdown to HTML, autolinking against the repo's document index and branch
/// list when they are provided.
pub fn markdown(source: &str, repo: &str, index: Option<&DocIndex>, branches: &[String]) -> String {
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
                let dest_url = if allowed_url(&dest_url) { dest_url } else { "#".into() };
                events.push(Event::Start(Tag::Link { link_type, dest_url, title, id }));
            }
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                verbatim_depth += 1;
                let dest_url = if allowed_url(&dest_url) { dest_url } else { "".into() };
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
            by_branch: BTreeMap::new(),
            referencing_plan: BTreeMap::new(),
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
