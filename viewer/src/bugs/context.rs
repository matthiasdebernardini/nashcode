//! Context capture: the three lines either side of the line that failed.
//!
//! A stack frame that says `src/pipeline.rs:214` is a fact you have to go and look up.
//! The same frame with the surrounding source under it is an answer. Everything here
//! exists to turn the first into the second without ever showing the wrong lines.
//!
//! Two rules shape the whole module.
//!
//! **Read on render, never at ingest.** The mirror is local and the index stores only
//! file, line and release, so the snippet costs one `git show` on a page that somebody
//! is looking at. Capturing at ingest would mean writing a copy of the repository into
//! the event store and pinning it forever.
//!
//! **The revision is part of the answer.** Source moves. A line number from last
//! Tuesday's release, read against today's tip, points at whatever happens to be there
//! now — which is worse than showing nothing, because it looks right. So the snippet is
//! read at the commit `sentry.release` names when the mirror knows that commit, and
//! when it does not the page says out loud that it is showing the tip instead.

use std::collections::HashMap;
use std::sync::Arc;

use crate::git::Repo;

/// How many lines either side of the failing one. Three is what fits on a phone and
/// what a person can take in without scrolling.
pub const CONTEXT_LINES: i64 = 3;

/// The most of one file that is worth reading to find seven lines of it. Source files
/// are kilobytes; anything past this is a checked-in artefact and there is nothing in
/// it a person wanted to see.
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// The shortest hex string worth trying as a commit. Below this a "release" like
/// `1.2.0` or `abc` would collide with a branch or tag name and resolve to the wrong
/// thing, silently.
const MIN_SHA_LEN: usize = 7;

/// One repository, at one revision, ready to answer questions about paths.
///
/// The tree listing is read once and reused, which is what makes suffix-matching a
/// hundred log rows cost one `ls-tree` rather than a hundred.
pub struct Source {
    repo: Repo,
    /// The commit everything is read at.
    pub rev: String,
    /// True when [`Source::rev`] is the default-branch tip because the release was
    /// missing or the mirror did not know it. The page has to say so.
    pub tip_not_release: bool,
    /// Every path in the tree at `rev`.
    files: Vec<String>,
}

/// What a reported path turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The reported path is a file in the repo, as written.
    Exact(String),
    /// A suffix of the reported path named exactly one file. The container prefix —
    /// `/app`, `/usr/src/app`, a build directory — is gone.
    Suffix(String),
    /// The longest suffix that matched anything matched several files. Guessing
    /// between them would put a person in the wrong file, which is worse than a plain
    /// string on the page.
    Ambiguous,
    /// Nothing in the tree ends this way.
    Missing,
}

impl Resolution {
    /// The repo-relative path, when there is one that can be linked and read.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Exact(path) | Self::Suffix(path) => Some(path),
            Self::Ambiguous | Self::Missing => None,
        }
    }

    /// True when the path came out of a suffix match, which the page marks so a reader
    /// can see that the link is an inference rather than the path the sender gave.
    pub fn inferred(&self) -> bool {
        matches!(self, Self::Suffix(_))
    }
}

/// Seven lines of source, and where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The repo-relative path the lines were read from.
    pub path: String,
    /// The line that failed.
    pub line: i64,
    /// The line number of `lines[0]`, one-based.
    pub start: i64,
    pub lines: Vec<String>,
    pub rev: String,
    /// True when `rev` is the tip and not the release the event named.
    pub tip_not_release: bool,
}

impl Snippet {
    /// The lines with their numbers, for rendering.
    pub fn numbered(&self) -> Vec<(i64, &str)> {
        self.lines
            .iter()
            .enumerate()
            .map(|(offset, text)| (self.start + offset as i64, text.as_str()))
            .collect()
    }
}

impl Source {
    /// Open a repository at the commit `release` names, or at the default tip.
    ///
    /// `None` when the mirror cannot answer at all — no branches, no tree. That is a
    /// degraded page, never an error: the log row or the frame renders as plain text
    /// and the rest of the page is unaffected.
    pub async fn open(repo: Repo, release: Option<&str>) -> Option<Self> {
        let (rev, tip_not_release) = match Self::commit_for(&repo, release).await {
            Some(rev) => (rev, false),
            None => {
                let branch = repo.default_branch().await.ok()?;
                (repo.tip(&branch).await.ok()?, true)
            }
        };
        let files = repo.list_files(&rev, "").await.ok()?;
        Some(Self { repo, rev, tip_not_release, files })
    }

    /// Resolve `release` to a commit the mirror actually has.
    ///
    /// Only something that looks like an object id is tried. A release named `v2.4.1`
    /// or `main` would resolve — to a tag or a branch, which is not what the sender
    /// meant and moves under us — so anything that is not hex of a plausible length is
    /// refused here rather than half-trusted later.
    async fn commit_for(repo: &Repo, release: Option<&str>) -> Option<String> {
        let release = release?.trim();
        let candidate = release.rsplit(['@', '+', '-']).next().unwrap_or(release);
        for wanted in [release, candidate] {
            if !looks_like_sha(wanted) {
                continue;
            }
            // `^{commit}` refuses anything that is not a commit, so a blob whose id
            // happens to share the prefix cannot become a revision.
            if let Ok(rev) = repo.rev_parse(&format!("{wanted}^{{commit}}")).await {
                let rev = rev.trim().to_owned();
                if !rev.is_empty() {
                    return Some(rev);
                }
            }
        }
        None
    }

    /// Which file in this tree the reported path means.
    pub fn resolve(&self, reported: &str) -> Resolution {
        let reported = reported.trim();
        if reported.is_empty() {
            return Resolution::Missing;
        }
        // Windows and some bundlers report backslashes. The tree never has them.
        let normalized = reported.replace('\\', "/");
        if self.files.iter().any(|file| file == &normalized) {
            return Resolution::Exact(normalized);
        }
        suffix_match(&self.files, &normalized)
    }

    /// The lines around `line` in `path`, read at this source's revision.
    ///
    /// `path` is a repo-relative path this source produced — never a reported one.
    pub async fn snippet(&self, path: &str, line: i64) -> Option<Snippet> {
        if line <= 0 {
            return None;
        }
        let (code, bytes, _capped) = self
            .repo
            .run_capped_bytes(
                &["show", "--end-of-options", &format!("{}:{path}", self.rev)],
                MAX_FILE_BYTES,
            )
            .await
            .ok()?;
        if code.is_some_and(|code| code != 0) {
            return None;
        }
        // A binary file has no lines worth showing, and lossy decoding would print a
        // screenful of replacement characters rather than admit it.
        let text = String::from_utf8(bytes).ok()?;
        let all: Vec<&str> = text.split('\n').collect();
        if line > all.len() as i64 {
            return None;
        }
        let start = (line - CONTEXT_LINES).max(1);
        let end = (line + CONTEXT_LINES).min(all.len() as i64);
        let lines: Vec<String> = all[(start - 1) as usize..end as usize]
            .iter()
            .map(|text| text.trim_end_matches('\r').to_owned())
            .collect();
        Some(Snippet {
            path: path.to_owned(),
            line,
            start,
            lines,
            rev: self.rev.clone(),
            tip_not_release: self.tip_not_release,
        })
    }

    /// How many files the tree has. For tests and for a log line about a slow repo.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

/// The longest suffix of `reported`, on segment boundaries, that names exactly one
/// file in `files`.
///
/// An SDK inside a container reports the path the process ran from — `/app/src/foo.py`,
/// `/usr/src/app/dist/index.js`, `/build/src/main.rs` — and the repo knows `src/foo.py`.
/// Matching the whole string finds nothing, so the prefix has to come off. Taking it
/// off one segment at a time and stopping at the first suffix that matches anything is
/// what makes the answer the *most specific* one available.
///
/// If that suffix matches more than one file, every shorter suffix matches at least as
/// many, so there is nothing to be gained by continuing and the answer is "ambiguous".
/// Two files called `utils.py` in different packages are exactly the case where a guess
/// sends a person to the wrong one and they believe it.
pub fn suffix_match(files: &[String], reported: &str) -> Resolution {
    let segments: Vec<&str> = reported.split('/').filter(|part| !part.is_empty()).collect();
    if segments.is_empty() {
        return Resolution::Missing;
    }
    for start in 0..segments.len() {
        let suffix = segments[start..].join("/");
        let tail = format!("/{suffix}");
        let mut found: Option<&String> = None;
        let mut several = false;
        for file in files {
            if *file == suffix || file.ends_with(&tail) {
                if found.is_some() {
                    several = true;
                    break;
                }
                found = Some(file);
            }
        }
        if several {
            return Resolution::Ambiguous;
        }
        if let Some(file) = found {
            return if start == 0 && file == &suffix {
                Resolution::Exact(file.clone())
            } else {
                Resolution::Suffix(file.clone())
            };
        }
    }
    Resolution::Missing
}

/// Could this string be an object id? Hex, and long enough that it is not a version
/// number wearing a disguise.
fn looks_like_sha(value: &str) -> bool {
    value.len() >= MIN_SHA_LEN
        && value.len() <= 40
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// One repository's sources, one per release seen on the page.
///
/// A page of log rows can name several releases; almost always it names one. Opening a
/// source per row would run `ls-tree -r` per row, so they are kept here and shared.
pub struct Catalog {
    repo: Repo,
    sources: HashMap<Option<String>, Option<Arc<Source>>>,
}

impl Catalog {
    pub fn new(repo: Repo) -> Self {
        Self { repo, sources: HashMap::new() }
    }

    /// The source for a release, opening it the first time and reusing it after.
    pub async fn source(&mut self, release: Option<&str>) -> Option<Arc<Source>> {
        let key = release.map(str::to_owned);
        if let Some(cached) = self.sources.get(&key) {
            return cached.clone();
        }
        let opened = Source::open(self.repo.clone(), release).await.map(Arc::new);
        self.sources.insert(key, opened.clone());
        opened
    }
}

/// The release an event or a log row was produced by, from wherever it hides.
///
/// Sentry puts it at the top level of an event and in a `sentry.release` log attribute;
/// the NDJSON door takes a plain `release`. All three mean the same thing.
pub fn release_of(value: &serde_json::Value) -> Option<String> {
    for key in ["release", "sentry.release"] {
        if let Some(found) = value.get(key).and_then(serde_json::Value::as_str) {
            let found = found.trim();
            if !found.is_empty() {
                return Some(found.to_owned());
            }
        }
    }
    // A tag, which is where a manually configured SDK often puts it.
    value
        .get("tags")
        .and_then(|tags| tags.get("release"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|found| !found.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn a_container_path_finds_its_file_by_the_longest_unique_suffix() {
        let files = tree(&["src/foo.py", "src/bar.py", "tests/conftest.py"]);
        assert_eq!(
            suffix_match(&files, "/app/src/foo.py"),
            Resolution::Suffix("src/foo.py".to_owned())
        );
        assert_eq!(
            suffix_match(&files, "/usr/src/app/tests/conftest.py"),
            Resolution::Suffix("tests/conftest.py".to_owned())
        );
        // The path exactly as the repo has it is not an inference.
        assert_eq!(suffix_match(&files, "src/foo.py"), Resolution::Exact("src/foo.py".to_owned()));
    }

    #[test]
    fn two_files_of_the_same_name_are_ambiguous_and_never_guessed() {
        let files = tree(&["a/utils.py", "b/utils.py"]);
        assert_eq!(suffix_match(&files, "/app/utils.py"), Resolution::Ambiguous);
        // Enough of the path to tell them apart resolves again.
        assert_eq!(
            suffix_match(&files, "/app/b/utils.py"),
            Resolution::Suffix("b/utils.py".to_owned())
        );
    }

    #[test]
    fn a_suffix_only_matches_on_a_segment_boundary() {
        // `notfoo.py` ends with `foo.py` as a string and is a different file.
        let files = tree(&["src/notfoo.py"]);
        assert_eq!(suffix_match(&files, "/app/foo.py"), Resolution::Missing);
        assert_eq!(
            suffix_match(&files, "/app/src/notfoo.py"),
            Resolution::Suffix("src/notfoo.py".to_owned())
        );
    }

    #[test]
    fn a_path_that_is_nowhere_in_the_tree_stays_nowhere() {
        let files = tree(&["src/foo.py"]);
        assert_eq!(suffix_match(&files, "/usr/lib/python3.12/site-packages/urllib3/x.py"), Resolution::Missing);
        assert_eq!(suffix_match(&files, ""), Resolution::Missing);
        assert_eq!(suffix_match(&files, "/"), Resolution::Missing);
    }

    #[test]
    fn the_deepest_match_wins_over_a_shallower_ambiguous_one() {
        // `handler.rs` appears twice; `api/handler.rs` appears once. Reporting the
        // longer path has to find the one, not report the two.
        let files = tree(&["src/api/handler.rs", "src/web/handler.rs"]);
        assert_eq!(
            suffix_match(&files, "/build/src/api/handler.rs"),
            Resolution::Suffix("src/api/handler.rs".to_owned())
        );
        assert_eq!(suffix_match(&files, "/build/handler.rs"), Resolution::Ambiguous);
    }

    #[test]
    fn only_something_shaped_like_an_object_id_is_tried_as_one() {
        assert!(looks_like_sha("9f3c1a2"));
        assert!(looks_like_sha(&"a".repeat(40)));
        for not in ["main", "v2.4.1", "abc", "release-2026-08-20", &"a".repeat(41), ""] {
            assert!(!looks_like_sha(not), "{not} was taken for a commit");
        }
    }

    #[test]
    fn a_release_is_found_wherever_the_sender_put_it() {
        assert_eq!(release_of(&json!({"release": "9f3c1a2"})).as_deref(), Some("9f3c1a2"));
        assert_eq!(release_of(&json!({"sentry.release": "9f3c1a2"})).as_deref(), Some("9f3c1a2"));
        assert_eq!(
            release_of(&json!({"tags": {"release": "9f3c1a2"}})).as_deref(),
            Some("9f3c1a2")
        );
        assert_eq!(release_of(&json!({"release": "  "})), None);
        assert_eq!(release_of(&json!({})), None);
    }

    #[test]
    fn a_snippet_numbers_its_lines_from_where_it_started() {
        let snippet = Snippet {
            path: "src/a.rs".to_owned(),
            line: 10,
            start: 7,
            lines: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            rev: "abc".to_owned(),
            tip_not_release: false,
        };
        assert_eq!(snippet.numbered(), vec![(7, "a"), (8, "b"), (9, "c")]);
    }
}
