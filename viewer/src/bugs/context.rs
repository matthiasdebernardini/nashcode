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

/// How much of a tree listing is worth holding. `sentry.release` is a free-text
/// attribute on every log row, so the number of trees a page could be asked to open is
/// bounded by the sender and not by us; the bytes each one costs have to be bounded
/// here. Eight mebibytes of path names is on the order of two hundred thousand files —
/// far past any repository this serves, and far short of a page that swaps the box.
pub const MAX_TREE_BYTES: usize = 8 * 1024 * 1024;

/// How many distinct trees one page may hold at once.
///
/// In practice a page names one release. Four leaves room for a deploy in progress and
/// a straggler, and refuses the pathological case: a hundred rows carrying a hundred
/// different release strings is a sender being odd, not a page worth a hundred trees.
pub const MAX_SOURCES: usize = 4;

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
    /// Every path in the tree at `rev`, unless [`Source::truncated`].
    files: Vec<String>,
    /// True when the tree listing hit [`MAX_TREE_BYTES`] and is only a prefix of the
    /// repository. Exact matches off a partial list are still sound — a path that is
    /// present is present — but *uniqueness* is not, so suffix matching is switched
    /// off rather than allowed to link somebody to the wrong file.
    truncated: bool,
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
    /// Read the tree at an already-resolved commit.
    ///
    /// Private, and the only way to build one: [`Catalog`] owns the question of *which*
    /// commit, because that is where the bound on how many trees a page opens lives.
    /// `None` when the mirror cannot answer — a degraded page, never an error.
    ///
    /// One `ls-tree -r`, byte-capped. The listing is what every path question on the
    /// page is then answered from, in memory.
    async fn open_at(repo: Repo, rev: String, tip_not_release: bool) -> Option<Self> {
        let (code, bytes, truncated) = repo
            .run_capped_bytes(
                &["ls-tree", "-r", "--name-only", "-z", "--end-of-options", &rev],
                MAX_TREE_BYTES,
            )
            .await
            .ok()?;
        if code.is_some_and(|code| code != 0) {
            return None;
        }
        let listing = String::from_utf8_lossy(&bytes);
        let mut files: Vec<String> = listing
            .split('\0')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect();
        // A cap that lands mid-name would invent a path that is not in the repository.
        if truncated {
            files.pop();
        }
        Some(Self { repo, rev, tip_not_release, files, truncated })
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
        // Uniqueness over a partial listing is not uniqueness. See `truncated`.
        if self.truncated {
            return Resolution::Missing;
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

/// One repository's sources for the life of one page render.
///
/// **Keyed on the resolved commit, never on the release string.** That distinction is
/// the whole design. `sentry.release` is a free-text attribute that any sender can put
/// anything in, so a hundred rows can carry a hundred different release strings. Keyed
/// on the string, each one that the mirror cannot resolve would open its own source —
/// `default_branch`, `tip`, and a full `ls-tree -r` apiece — and every one of them would
/// be the *same tree*. Keyed on the commit, they all collapse onto the tip source: one
/// listing, held once.
///
/// Two more bounds sit on top of that, because collapsing is not a bound. A release that
/// cannot be an object id costs no subprocess at all, since the syntax check happens
/// before the `rev_parse`; and one that can is remembered, so a repeated string is asked
/// about once. Past [`MAX_SOURCES`] distinct trees the page stops opening new ones and
/// falls back to the tip, which is honest — the snippet is then marked "tip, not
/// release", which is exactly what it is.
pub struct Catalog {
    repo: Repo,
    /// Open trees by commit.
    sources: HashMap<String, Option<Arc<Source>>>,
    /// Release string to commit. `None` means "resolved to nothing", which is the tip.
    resolved: HashMap<String, Option<String>>,
    /// The default-branch tip. The outer `Option` is "not asked yet".
    tip: Option<Option<String>>,
    /// Every git subprocess this catalog has caused. Read by the tests that pin the
    /// bound: the point of the cache is a number, so the number is observable.
    git_calls: usize,
}

impl Catalog {
    pub fn new(repo: Repo) -> Self {
        Self {
            repo,
            sources: HashMap::new(),
            resolved: HashMap::new(),
            tip: None,
            git_calls: 0,
        }
    }

    /// How many git subprocesses have been spawned for path resolution on this page.
    pub fn git_calls(&self) -> usize {
        self.git_calls
    }

    /// How many distinct trees are being held.
    pub fn sources_open(&self) -> usize {
        self.sources.values().filter(|source| source.is_some()).count()
    }

    /// The source for a release, opening it the first time and reusing it after.
    pub async fn source(&mut self, release: Option<&str>) -> Option<Arc<Source>> {
        let named = match release {
            Some(release) => self.rev_for(release).await,
            None => None,
        };
        let (rev, tip_not_release) = match named {
            Some(rev) => (rev, false),
            None => (self.tip_rev().await?, true),
        };
        if let Some(cached) = self.sources.get(&rev) {
            return cached.clone();
        }
        // Past the cap, everything lands on the tip rather than opening another tree.
        // The tip is already held, so this costs nothing and says what it is.
        if self.sources.len() >= MAX_SOURCES {
            let tip = self.tip_rev().await?;
            return self.sources.get(&tip).cloned().flatten();
        }

        self.git_calls += 1;
        let opened =
            Source::open_at(self.repo.clone(), rev.clone(), tip_not_release).await.map(Arc::new);
        self.sources.insert(rev, opened.clone());
        opened
    }

    /// The commit a release names, when the mirror has one. `None` means "use the tip".
    ///
    /// The syntax check comes first and costs nothing, which is what keeps a page of
    /// `v1.0.1`…`v1.0.100` from being a hundred subprocesses.
    async fn rev_for(&mut self, release: &str) -> Option<String> {
        let release = release.trim();
        if release.is_empty() {
            return None;
        }
        if let Some(known) = self.resolved.get(release) {
            return known.clone();
        }
        // `sentry-cli` writes releases like `my-app@9f3c1a2`, so the tail after a
        // separator is worth a try when the whole string is not an id. When there is no
        // separator the tail *is* the whole string, and asking git the same question
        // twice is one subprocess per row for nothing.
        let candidate = release.rsplit(['@', '+', '-']).next().unwrap_or(release);
        let mut tried: Vec<&str> = Vec::new();
        let mut found = None;
        for wanted in [release, candidate] {
            if !looks_like_sha(wanted) || tried.contains(&wanted) {
                continue;
            }
            tried.push(wanted);
            self.git_calls += 1;
            // `^{commit}` refuses anything that is not a commit, so a blob whose id
            // happens to share the prefix cannot become a revision.
            if let Ok(rev) = self.repo.rev_parse(&format!("{wanted}^{{commit}}")).await {
                let rev = rev.trim().to_owned();
                if !rev.is_empty() {
                    found = Some(rev);
                    break;
                }
            }
        }
        self.resolved.insert(release.to_owned(), found.clone());
        found
    }

    /// The default-branch tip, asked for at most once.
    async fn tip_rev(&mut self) -> Option<String> {
        if let Some(known) = &self.tip {
            return known.clone();
        }
        self.git_calls += 2;
        let found = match self.repo.default_branch().await {
            Ok(branch) => self.repo.tip(&branch).await.ok(),
            Err(_) => None,
        };
        self.tip = Some(found.clone());
        found
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

    /// A truncated listing can prove a path is present and cannot prove it is unique.
    /// Exact matching survives; suffix matching does not, and pretending otherwise
    /// would link somebody to a file that only looks like the right one.
    #[test]
    fn a_truncated_tree_still_matches_exactly_and_never_by_suffix() {
        // Never touched: `resolve` reads the listing and nothing else.
        let repo = Repo::mirror(std::path::PathBuf::from("/nonexistent"), crate::git::Auth::new(""));
        let source = Source {
            repo,
            rev: "abc".to_owned(),
            tip_not_release: false,
            files: tree(&["src/foo.py", "src/bar.py"]),
            truncated: true,
        };
        assert_eq!(source.resolve("src/foo.py"), Resolution::Exact("src/foo.py".to_owned()));
        assert_eq!(source.resolve("/app/src/foo.py"), Resolution::Missing);

        // Complete, the same question resolves.
        let whole = Source { truncated: false, ..source };
        assert_eq!(
            whole.resolve("/app/src/foo.py"),
            Resolution::Suffix("src/foo.py".to_owned())
        );
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
