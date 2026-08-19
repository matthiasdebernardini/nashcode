//! The accurate overlay: SCIP indexes, produced by real language indexers and read
//! in process.
//!
//! tree-sitter resolves names by shape — `retry()` is `retry`, wherever `retry` was
//! declared. A language indexer resolves them by compiling, so `self.retry()` and
//! `other.retry()` become different symbols and a rename is a rename. Where the
//! indexer for a language is installed, its answer replaces the tree-sitter answer for
//! every file it covered; where it is not, the tree-sitter answer stands. That is the
//! whole degradation ladder: SCIP, then tree-sitter, then `git grep`. None of the
//! rungs is an error.
//!
//! Two things SCIP does not give us, which this module builds:
//!
//! * **An occurrence does not name the function it sits inside.** A reference carries
//!   a range and a symbol and nothing else. So every definition's `enclosing_range`
//!   goes into an interval map per document, and each reference is attributed to the
//!   innermost definition whose range contains it. That map is what `/code/callers`
//!   is made of.
//! * **A symbol string is not a name.** `rust-analyzer cargo nashcode 0.1.0 code/
//!   Indexer#index().` is precise and unreadable. The last descriptor's name is what a
//!   person types, so that is what is stored, which keeps the query contract identical
//!   whichever layer answered.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use protobuf::Message as _;

use crate::db::{CodeRef, CodeSymbol, Db};
use crate::git::{Repo, clone_local};

/// The source label stored against everything this module writes.
pub const SOURCE: &str = "scip";

/// How long any one indexer may run before it is killed. An indexer that has not
/// finished by then has hit a pathological input, and the tree-sitter graph is a
/// perfectly good answer in the meantime.
const INDEXER_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// SCIP's Definition role, the low bit of `symbol_roles`.
const ROLE_DEFINITION: i32 = 0x1;

/// One indexer we know how to run.
struct Indexer {
    /// The binary, as it must appear on `PATH`.
    program: &'static str,
    /// Its arguments, with `{out}` standing for the index file to write.
    args: &'static [&'static str],
    /// What it covers, for the note when it is missing.
    language: &'static str,
    /// A file extension whose presence means this indexer has something to do.
    marker: &'static str,
}

/// The indexers, in the order they run. Each is optional; none is required.
const INDEXERS: &[Indexer] = &[
    Indexer {
        program: "rust-analyzer",
        args: &["scip", ".", "--output", "{out}"],
        language: "rust",
        marker: "Cargo.toml",
    },
    Indexer {
        program: "scip-typescript",
        args: &["index", "--output", "{out}"],
        language: "typescript",
        marker: "package.json",
    },
    Indexer {
        program: "scip-python",
        args: &["index", ".", "--output", "{out}"],
        language: "python",
        marker: "pyproject.toml",
    },
];

/// Run whichever indexers are installed and load what they produce.
///
/// Returns a note describing anything that did not run, which is empty when every
/// applicable indexer ran. An `Err` means the overlay could not be attempted at all;
/// the caller records it as a note too, because the tree-sitter graph is already
/// written by the time this is called.
pub async fn overlay(
    db: &Db,
    repo: &str,
    mirror: &Repo,
    commit: &str,
) -> Result<String, String> {
    let applicable: Vec<&Indexer> = INDEXERS
        .iter()
        .filter(|indexer| which(indexer.program).is_some())
        .collect();
    if applicable.is_empty() {
        // Not a degradation worth a note on every run: most repos have no indexer
        // installed and the tree-sitter graph is the designed answer for them.
        return Ok(String::new());
    }

    let scratch = tempfile::tempdir().map_err(|error| format!("no scratch dir: {error}"))?;
    let checkout = scratch.path().join("repo");
    clone_local(mirror.path(), &checkout)
        .await
        .map_err(|error| format!("scratch clone for SCIP failed: {error}"))?;
    let work = Repo::worktree(&checkout, Default::default());
    work.run(&["checkout", "--detach", commit])
        .await
        .map_err(|error| format!("cannot check out {commit} for SCIP: {error}"))?;

    // The path table already points at this commit, so the blob for a path is the
    // one the indexer is about to talk about.
    let blobs = path_blobs(db, repo);

    let mut notes: Vec<String> = Vec::new();
    for indexer in applicable {
        if !checkout.join(indexer.marker).exists() {
            continue;
        }
        let output = scratch.path().join(format!("{}.scip", indexer.language));
        match run_indexer(indexer, &checkout, &output).await {
            Ok(()) => match load(&output) {
                Ok(documents) => apply(db, repo, &blobs, documents, &mut notes),
                Err(why) => notes.push(format!("{} index unreadable: {why}", indexer.language)),
            },
            Err(why) => notes.push(format!("{} indexer: {why}", indexer.language)),
        }
    }
    Ok(notes.join("; "))
}

/// Write one document's symbols and references over whatever that blob held.
fn apply(
    db: &Db,
    repo: &str,
    blobs: &BTreeMap<String, String>,
    documents: Vec<Document>,
    notes: &mut Vec<String>,
) {
    for document in documents {
        let Some(blob) = blobs.get(&document.path) else {
            // The indexer saw a file the tree does not have: generated output, or a
            // path it normalised differently. Nothing to attach it to.
            continue;
        };
        if let Err(error) = db.code_put_graph(repo, blob, &document.symbols, &document.refs) {
            let message = format!("cannot store the SCIP graph: {error}");
            if !notes.contains(&message) {
                notes.push(message);
            }
        }
    }
}

/// The repo's current path-to-blob map, which is how a SCIP document finds its
/// content-addressed home.
fn path_blobs(db: &Db, repo: &str) -> BTreeMap<String, String> {
    db.with(|conn| {
        let mut statement =
            conn.prepare("SELECT path, blob FROM code_files WHERE repo = ?1")?;
        let rows = statement
            .query_map(rusqlite::params![repo], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok(rows)
    })
    .unwrap_or_default()
}

/// Is a program on `PATH`? A missing indexer must cost nothing, so this is a `PATH`
/// walk rather than spawning the program to find out.
fn which(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(program);
        candidate
            .metadata()
            .ok()
            .filter(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .map(|_| candidate)
    })
}

/// Run one indexer in the checkout, against a deadline.
async fn run_indexer(
    indexer: &Indexer,
    checkout: &Path,
    output: &Path,
) -> Result<(), String> {
    let args: Vec<String> = indexer
        .args
        .iter()
        .map(|arg| arg.replace("{out}", &output.to_string_lossy()))
        .collect();

    let mut command = tokio::process::Command::new(indexer.program);
    command
        .args(&args)
        .current_dir(checkout)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn().map_err(|error| format!("cannot run: {error}"))?;
    let finished = tokio::time::timeout(INDEXER_TIMEOUT, child.wait_with_output()).await;
    match finished {
        Err(_elapsed) => Err(format!("gave up after {}s", INDEXER_TIMEOUT.as_secs())),
        Ok(Err(error)) => Err(format!("did not finish: {error}")),
        Ok(Ok(done)) if done.status.success() => Ok(()),
        Ok(Ok(done)) => {
            let stderr = String::from_utf8_lossy(&done.stderr);
            let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().join(" / ");
            Err(format!("exited {}: {tail}", done.status))
        }
    }
}

/// One SCIP document, reduced to the two tables we own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub path: String,
    pub symbols: Vec<CodeSymbol>,
    pub refs: Vec<CodeRef>,
}

/// Read an index file. A truncated or absent file is an error, never a panic.
pub fn load(path: &Path) -> Result<Vec<Document>, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    parse(&bytes)
}

/// Parse a SCIP index into documents.
pub fn parse(bytes: &[u8]) -> Result<Vec<Document>, String> {
    let index = ::scip::types::Index::parse_from_bytes(bytes)
        .map_err(|error| format!("not a SCIP index: {error}"))?;
    Ok(index.documents.iter().map(read_document).collect())
}

/// Everything one document contributes, with references attributed to the definition
/// that encloses them.
fn read_document(document: &::scip::types::Document) -> Document {
    // Definitions first: they are the interval map every reference is measured
    // against, so they must all be known before any reference is placed.
    let mut definitions: Vec<Interval> = Vec::new();
    let mut symbols: Vec<CodeSymbol> = Vec::new();
    let kinds = kind_map(document);

    for occurrence in &document.occurrences {
        if occurrence.symbol_roles & ROLE_DEFINITION == 0 {
            continue;
        }
        let Some(name) = short_name(&occurrence.symbol) else { continue };
        let Some(span) = span(&occurrence.range) else { continue };
        // `enclosing_range` is the whole body; `range` is just the identifier. The
        // body is what a reference can fall inside, so it is what the map holds.
        let body = span_of(&occurrence.enclosing_range).unwrap_or(span);
        definitions.push(Interval { name: name.clone(), start: body.0, end: body.1 });
        symbols.push(CodeSymbol {
            name,
            kind: kinds
                .get(occurrence.symbol.as_str())
                .cloned()
                .unwrap_or_else(|| "symbol".to_owned()),
            start_line: span.0 as i64 + 1,
            end_line: body.1 as i64 + 1,
            source: SOURCE.to_owned(),
        });
    }

    let mut refs: Vec<CodeRef> = Vec::new();
    for occurrence in &document.occurrences {
        if occurrence.symbol_roles & ROLE_DEFINITION != 0 {
            continue;
        }
        let Some(name) = short_name(&occurrence.symbol) else { continue };
        let Some((line, _)) = span(&occurrence.range) else { continue };
        refs.push(CodeRef {
            name,
            caller: enclosing(&definitions, line),
            line: line as i64 + 1,
            // SCIP does not separate "called" from "mentioned", so every occurrence
            // is a call edge for `/code/callers` and a reference for `/code/refs`.
            // Over-reporting a caller beats silently dropping one.
            kind: "call".to_owned(),
            source: SOURCE.to_owned(),
        });
    }

    // Deduplicate: one symbol may be defined once and occur many times on a line.
    symbols.sort_by(|a, b| (a.start_line, &a.name).cmp(&(b.start_line, &b.name)));
    symbols.dedup();
    refs.sort_by(|a, b| (a.line, &a.name, &a.caller).cmp(&(b.line, &b.name, &b.caller)));
    refs.dedup();

    Document { path: document.relative_path.clone(), symbols, refs }
}

/// The kind each symbol was declared with, from the document's symbol table.
fn kind_map(document: &::scip::types::Document) -> BTreeMap<String, String> {
    document
        .symbols
        .iter()
        .map(|info| {
            let kind = format!("{:?}", info.kind.enum_value_or_default()).to_ascii_lowercase();
            (info.symbol.clone(), kind)
        })
        .collect()
}

/// A definition's span, for the interval map.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Interval {
    name: String,
    start: usize,
    end: usize,
}

/// The innermost definition containing a line.
fn enclosing(definitions: &[Interval], line: usize) -> Option<String> {
    definitions
        .iter()
        .filter(|interval| interval.start <= line && line <= interval.end)
        .min_by_key(|interval| interval.end - interval.start)
        .map(|interval| interval.name.clone())
}

/// The `(start line, end line)` of a SCIP range, zero-based.
///
/// A range is `[startLine, startChar, endLine, endChar]`, or the three-element
/// `[startLine, startChar, endChar]` when it does not cross a line. Anything else is
/// not a range this version of the format defines.
fn span(range: &[i32]) -> Option<(usize, usize)> {
    match range.len() {
        3 => Some((range[0].max(0) as usize, range[0].max(0) as usize)),
        4 => Some((range[0].max(0) as usize, range[2].max(0) as usize)),
        _ => None,
    }
}

/// Same, but `None` for an empty range, which is how an absent `enclosing_range`
/// arrives.
fn span_of(range: &[i32]) -> Option<(usize, usize)> {
    (!range.is_empty()).then(|| span(range)).flatten()
}

/// The name a person would type, from a SCIP symbol string.
///
/// Local symbols (`local 3`) name nothing outside their file and are dropped. For a
/// global symbol the last descriptor is the thing itself, and its `name` is the
/// identifier; the descriptors before it are the module path.
pub fn short_name(symbol: &str) -> Option<String> {
    if symbol.is_empty() || ::scip::symbol::is_local_symbol(symbol) {
        return None;
    }
    let parsed = ::scip::symbol::parse_symbol(symbol).ok()?;
    let last = parsed.descriptors.last()?;
    let name = last.name.trim();
    // A parameter or a type parameter is not a thing anyone looks up by name.
    let suffix = last.suffix.enum_value_or_default();
    if matches!(
        suffix,
        ::scip::types::descriptor::Suffix::Parameter
            | ::scip::types::descriptor::Suffix::TypeParameter
            | ::scip::types::descriptor::Suffix::Local
    ) {
        return None;
    }
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::{EnumOrUnknown, MessageField};
    use scip::types::{Document as ScipDocument, Index, Occurrence, SymbolInformation};

    fn occurrence(symbol: &str, range: Vec<i32>, roles: i32, enclosing: Vec<i32>) -> Occurrence {
        Occurrence {
            range,
            symbol: symbol.to_owned(),
            symbol_roles: roles,
            enclosing_range: enclosing,
            ..Default::default()
        }
    }

    /// A one-document index: `retry` is defined over lines 10-20 and calls `sleep`
    /// on line 15; `main` is defined on line 30 and calls `retry` on line 31.
    fn fixture() -> Vec<u8> {
        let retry = "rust-analyzer cargo demo 0.1.0 net/retry().";
        let sleep = "rust-analyzer cargo demo 0.1.0 time/sleep().";
        let main = "rust-analyzer cargo demo 0.1.0 demo/main().";
        let document = ScipDocument {
            relative_path: "src/net.rs".to_owned(),
            occurrences: vec![
                occurrence(retry, vec![10, 3, 10, 8], ROLE_DEFINITION, vec![10, 0, 20, 1]),
                occurrence(sleep, vec![15, 8, 15, 13], 0, vec![]),
                occurrence(main, vec![30, 3, 30, 7], ROLE_DEFINITION, vec![30, 0, 33, 1]),
                occurrence(retry, vec![31, 4, 31, 9], 0, vec![]),
            ],
            symbols: vec![
                SymbolInformation {
                    symbol: retry.to_owned(),
                    kind: EnumOrUnknown::new(scip::types::symbol_information::Kind::Function),
                    signature_documentation: MessageField::none(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        Index { documents: vec![document], ..Default::default() }
            .write_to_bytes()
            .expect("the fixture index serialises")
    }

    #[test]
    fn definitions_come_back_with_their_body_span() {
        let documents = parse(&fixture()).expect("parses");
        let document = &documents[0];
        assert_eq!(document.path, "src/net.rs");
        let retry = document
            .symbols
            .iter()
            .find(|s| s.name == "retry")
            .expect("retry is defined");
        // One-based, and the end is the body's end, not the identifier's.
        assert_eq!(retry.start_line, 11);
        assert_eq!(retry.end_line, 21);
        assert_eq!(retry.kind, "function");
        assert_eq!(retry.source, SOURCE);
    }

    #[test]
    fn a_reference_is_attributed_to_the_definition_that_encloses_it() {
        let documents = parse(&fixture()).expect("parses");
        let refs = &documents[0].refs;
        let sleep = refs.iter().find(|r| r.name == "sleep").expect("sleep is referenced");
        assert_eq!(sleep.caller.as_deref(), Some("retry"), "line 15 is inside retry's body");
        assert_eq!(sleep.line, 16);

        let retry = refs.iter().find(|r| r.name == "retry").expect("retry is referenced");
        assert_eq!(retry.caller.as_deref(), Some("main"), "line 31 is inside main's body");
    }

    #[test]
    fn a_definition_occurrence_is_not_also_a_reference() {
        let documents = parse(&fixture()).expect("parses");
        assert_eq!(
            documents[0].refs.iter().filter(|r| r.line == 11).count(),
            0,
            "the definition site is a symbol, not a reference to itself"
        );
    }

    #[test]
    fn a_symbol_string_reduces_to_the_name_a_person_types() {
        assert_eq!(
            short_name("rust-analyzer cargo demo 0.1.0 net/retry().").as_deref(),
            Some("retry")
        );
        assert_eq!(
            short_name("scip-typescript npm demo 1.0.0 `src/app.ts`/Widget#").as_deref(),
            Some("Widget")
        );
        assert_eq!(short_name("local 12"), None, "locals name nothing outside their file");
        assert_eq!(short_name(""), None);
        assert_eq!(short_name("not a symbol"), None);
    }

    #[test]
    fn a_file_that_is_not_a_scip_index_is_an_error_not_a_panic() {
        assert!(parse(b"this is not protobuf at all, not even close").is_err());
    }

    #[test]
    fn a_three_element_range_is_a_single_line() {
        assert_eq!(span(&[7, 2, 9]), Some((7, 7)));
        assert_eq!(span(&[7, 2, 9, 4]), Some((7, 9)));
        assert_eq!(span(&[7]), None);
        assert_eq!(span_of(&[]), None);
    }

    #[test]
    fn an_absent_indexer_is_simply_not_found() {
        assert!(which("nashcode-no-such-indexer-exists").is_none());
        assert!(which("sh").is_some(), "sh is on PATH everywhere this runs");
    }
}
