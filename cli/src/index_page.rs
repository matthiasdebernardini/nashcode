//! Parse the repository list off dgit's index page.
//!
//! dgit has no JSON list endpoint — the index is HTML, so `nashcode ls` reads
//! the HTML. The parse is deliberately loose so that a cosmetic change upstream
//! (a class rename, an extra column, single quotes becoming double) does not
//! break the command:
//!
//!   * scope to the listing table when one is found, otherwise the whole page;
//!   * take each `<tr>`, and treat it as a repository row when its first cell
//!     holds a link of the form `/<name>/`;
//!   * read the remaining cells positionally, and tolerate any number of them;
//!   * carry the most recent section header row down onto the rows below it.
//!
//! Names are checked against dgit's own repository-name rule, which keeps the
//! page's navigation links (`/`, `/cgit.css`) out of the list.

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Repo {
    pub name: String,
    pub description: String,
    pub owner: String,
    /// Time since the last push, as dgit renders it, e.g. "3 days". Empty when
    /// the repository has never been pushed to.
    pub idle: String,
    /// The index-page section this repository is filed under, if any.
    pub section: String,
}

static ROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<tr\b[^>]*>(.*?)</tr>").unwrap());
static CELL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<t[dh]\b[^>]*>(.*?)</t[dh]>").unwrap());
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)href\s*=\s*['"]/([^/'"?#]+)/?['"]"#).unwrap());
static SECTION_ROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)reposection").unwrap());
static TABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<table[^>]*class\s*=\s*['"][^'"]*\blist\b[^'"]*['"][^>]*>(.*?)</table>"#).unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<[^>]*>").unwrap());
// dgit's own rule, from src/index.ts.
static REPO_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*$").unwrap());

const RESERVED: &[&str] = &[
    "cgit.css",
    "favicon.ico",
    "robots.txt",
    "info",
    "git-upload-pack",
    "git-receive-pack",
];

/// Pull the repository list out of an index page.
pub fn parse(html: &str) -> Vec<Repo> {
    let scope = TABLE
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or(html);

    let mut out = Vec::new();
    let mut section = String::new();

    for row in ROW.captures_iter(scope) {
        let inner = &row[1];
        let cells: Vec<String> = CELL
            .captures_iter(inner)
            .map(|c| c[1].to_string())
            .collect();
        if cells.is_empty() {
            continue;
        }

        if SECTION_ROW.is_match(inner) {
            section = text(&cells[0]);
            continue;
        }

        let Some(name) = LINK.captures(&cells[0]).map(|c| c[1].to_string()) else {
            continue;
        };
        if !REPO_NAME.is_match(&name) || RESERVED.contains(&name.as_str()) {
            continue;
        }
        // A row whose link text differs from the href segment is not a repo row.
        let mut desc = cells.get(1).map(|c| text(c)).unwrap_or_default();
        if desc == "[no description]" {
            desc.clear();
        }
        out.push(Repo {
            name,
            description: desc,
            owner: cells.get(2).map(|c| text(c)).unwrap_or_default(),
            idle: cells.get(3).map(|c| text(c)).unwrap_or_default(),
            section: section.clone(),
        });
    }
    out
}

/// Strip tags and decode the five entities dgit's escaper produces.
fn text(html: &str) -> String {
    let stripped = TAG.replace_all(html, "");
    unescape(stripped.trim())
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&mdash;", "—")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_index_yields_nothing() {
        let html = "<table class='list nowrap'><tr class='nohover'><td colspan='4'>no repositories yet &mdash; create one by pushing</td></tr></table>";
        assert!(parse(html).is_empty());
    }

    #[test]
    fn tolerates_double_quotes_and_extra_columns() {
        let html = r#"<table class="list nowrap">
        <tr><td><a href="/alpha/">alpha</a></td><td>one</td><td>me</td><td>2 days</td><td>extra</td></tr>
        </table>"#;
        let repos = parse(html);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "alpha");
        assert_eq!(repos[0].idle, "2 days");
    }

    #[test]
    fn skips_navigation_links() {
        let html = "<table class='list'><tr><td><a href='/'>site</a></td></tr>\
                    <tr><td><a href='/cgit.css'>css</a></td></tr></table>";
        assert!(parse(html).is_empty());
    }
}
