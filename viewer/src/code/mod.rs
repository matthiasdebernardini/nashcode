//! Code intelligence: three indexes over a repo's default branch, all refreshed by
//! one trigger and all answerable without a model in the loop.
//!
//! * **Full text** stores nothing. `git grep` against the mirror answers it at query
//!   time, which is exact, always current, and costs no disk.
//! * **Embeddings** answer "what is this *about*". Code is chunked per function where
//!   tree-sitter parses it and per fifty-line window where it cannot, embedded in
//!   process, and stored as vectors in SQLite. Queries brute-force cosine over the
//!   repo's chunks. That is a deliberate ceiling: these are personal repos, and an
//!   approximate-nearest-neighbour index earns its place only when a scan measurably
//!   hurts.
//! * **The graph** answers "who calls this". tree-sitter builds it in process on
//!   every index run; where a real language indexer is installed, SCIP replaces that
//!   file's entries with the accurate ones (see [`scip`]).
//!
//! Two rules run through all of it.
//!
//! **Indexing never happens on a request path.** The trigger is a merge to the default
//! branch, on the CI queue, and `POST /:repo/code/index` enqueues the same job for a
//! manual run. Everything a request does is read.
//!
//! **Work is keyed by content.** A chunk, a symbol, and a call edge all hang off the
//! blob SHA of the file they came from, and the path table alone knows where that blob
//! currently lives. An index run parses only blobs this repo has never seen, so a
//! full rebuild is the degenerate case of everything having changed, and a renamed
//! file costs nothing at all.

pub mod embed;
pub mod lang;
pub mod scip;

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::config::Config;
use crate::db::{
    CodeBlob, CodeCounts, CodeRun, CodeRunRow, Db, RefHit, StoredChunk, SymbolHit,
};
use crate::git::Repo;
use crate::mirror::Mirrors;

pub use embed::{EmbedError, Embedder, Embeddings, cosine};
pub use lang::Lang;

/// Files bigger than this are skipped. A megabyte of one file is a generated artefact
/// or a vendored bundle, and embedding it buries everything a person wrote.
const MAX_FILE_BYTES: usize = 1024 * 1024;

/// How many chunks go to the encoder at once.
const EMBED_BATCH: usize = 32;

/// Default result count for the query endpoints.
pub const DEFAULT_LIMIT: usize = 10;

/// The most any query endpoint will return, however large `limit` asks for.
pub const MAX_LIMIT: usize = 100;

/// Why an index run could not start. A run that starts always finishes with a record,
/// however much of it degraded.
#[derive(Debug)]
pub enum IndexError {
    NoMirror(String),
    Git(String),
    Db(String),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMirror(repo) => write!(f, "no mirror for {repo} yet"),
            Self::Git(why) => write!(f, "git failed: {why}"),
            Self::Db(why) => write!(f, "database failed: {why}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<crate::git::GitError> for IndexError {
    fn from(error: crate::git::GitError) -> Self {
        Self::Git(error.to_string())
    }
}

impl From<rusqlite::Error> for IndexError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(error.to_string())
    }
}

/// The indexing pipeline. Cheap to clone; the CI worker holds one.
#[derive(Clone)]
pub struct Indexer {
    pub config: Arc<Config>,
    pub db: Db,
    pub mirrors: Mirrors,
    pub embeddings: Embeddings,
}

impl std::fmt::Debug for Indexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Indexer")
    }
}

impl Indexer {
    /// Index a repo at a commit. Returns what the run did, including what degraded.
    ///
    /// The only errors are the ones that make a run impossible: no mirror, or a git
    /// or database failure before any work started. A missing model, a language with
    /// no grammar, and an absent SCIP indexer are all notes on a successful run.
    pub async fn index(&self, repo: &str, commit: &str) -> Result<CodeRun, IndexError> {
        let mirror = self.mirrors.repo(repo);
        if !mirror.exists() {
            return Err(IndexError::NoMirror(repo.to_owned()));
        }

        let tree = read_tree(&mirror, commit).await?;
        let mut run = CodeRun {
            repo: repo.to_owned(),
            commit: commit.to_owned(),
            files_seen: tree.len() as i64,
            ..Default::default()
        };
        let mut notes: Vec<String> = Vec::new();

        // Point the path table at this commit first. Doing it up front means a run
        // that dies partway has already retired paths that no longer exist, which is
        // the failure everyone notices: a query answering with a deleted file.
        let files: Vec<(String, String, String)> = tree
            .iter()
            .map(|entry| {
                (entry.path.clone(), entry.blob.clone(), Lang::of_path(&entry.path).name().to_owned())
            })
            .collect();
        self.db.code_set_files(repo, &files)?;

        // Content already indexed needs nothing. This is the whole of incrementality.
        let candidates: Vec<String> =
            tree.iter().map(|entry| entry.blob.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        let known: BTreeSet<String> =
            self.db.code_known_blobs(repo, &candidates)?.into_iter().collect();
        let fresh: Vec<&TreeEntry> = tree
            .iter()
            .filter(|entry| !known.contains(&entry.blob))
            // Size comes from the same `ls-tree`, so an artefact this big is skipped
            // without ever being read into memory.
            .filter(|entry| entry.size as usize <= MAX_FILE_BYTES)
            // One representative path per new blob: the content is what we parse.
            .fold(
                (BTreeSet::new(), Vec::new()),
                |(mut seen, mut out), entry| {
                    if seen.insert(entry.blob.clone()) {
                        out.push(entry);
                    }
                    (seen, out)
                },
            )
            .1;

        // The encoder is loaded once per run, on a blocking thread, and only when
        // there is something to embed. A failure is a note; the chunks are still
        // stored, still searchable by text, and pick up vectors on a later run.
        let embedder = if fresh.is_empty() {
            None
        } else {
            let slot = self.embeddings.clone();
            match tokio::task::spawn_blocking(move || slot.load()).await {
                Ok(Ok(embedder)) => Some(embedder),
                Ok(Err(error)) => {
                    notes.push(error.to_string());
                    None
                }
                Err(error) => {
                    notes.push(format!("the embedder task did not finish: {error}"));
                    None
                }
            }
        };

        for entry in fresh {
            let Some(source) = read_source(&mirror, &entry.blob).await? else {
                continue;
            };
            let parsed = lang::parse(Lang::of_path(&entry.path), &source);
            let mut blob = CodeBlob {
                repo: repo.to_owned(),
                blob: entry.blob.clone(),
                chunks: parsed.chunks,
                symbols: parsed.symbols,
                refs: parsed.refs,
            };

            if let Some(embedder) = &embedder {
                match embed_chunks(embedder.clone(), &mut blob).await {
                    Ok(count) => run.embedded += count as i64,
                    Err(error) => {
                        // One reason, once: a broken encoder breaks every batch, and
                        // a note per file would bury the run record.
                        let message = error.to_string();
                        if !notes.contains(&message) {
                            notes.push(message);
                        }
                    }
                }
            }

            run.files_indexed += 1;
            run.chunks += blob.chunks.len() as i64;
            run.symbols += blob.symbols.len() as i64;
            self.db.code_put_blob(&blob)?;
        }

        // The accurate overlay, where the tools for it are installed. Every failure
        // mode here leaves the tree-sitter graph exactly as it was.
        match scip::overlay(&self.db, repo, &mirror, commit).await {
            Ok(report) => {
                if !report.is_empty() {
                    notes.push(report);
                }
            }
            Err(why) => notes.push(why),
        }

        run.note = notes.join("; ");
        self.db.code_record_run(&run)?;
        if !run.note.is_empty() {
            tracing::info!(repo, commit, note = %run.note, "code index degraded");
        }
        Ok(run)
    }

    /// Index the tip of a repo's default branch. This is what a merge triggers.
    pub async fn index_default_branch(&self, repo: &str) -> Result<CodeRun, IndexError> {
        let mirror = self.mirrors.repo(repo);
        if !mirror.exists() {
            return Err(IndexError::NoMirror(repo.to_owned()));
        }
        let branch = mirror.default_branch().await?;
        let tip = mirror.tip(&branch).await?;
        self.index(repo, &tip).await
    }
}

/// Embed every chunk that has no vector yet, in batches. Returns how many landed.
async fn embed_chunks(
    embedder: Arc<dyn Embedder>,
    blob: &mut CodeBlob,
) -> Result<usize, EmbedError> {
    let pending: Vec<usize> = (0..blob.chunks.len())
        .filter(|i| blob.chunks[*i].vector.is_none() && !blob.chunks[*i].snippet.trim().is_empty())
        .collect();
    if pending.is_empty() {
        return Ok(0);
    }

    let texts: Vec<String> = pending.iter().map(|i| blob.chunks[*i].snippet.clone()).collect();
    let model = embedder.model().to_owned();
    // ONNX inference is CPU-bound and long; it must not sit on a runtime worker.
    let vectors = tokio::task::spawn_blocking(move || {
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for batch in texts.chunks(EMBED_BATCH) {
            out.extend(embedder.embed(batch)?);
        }
        Ok::<_, EmbedError>(out)
    })
    .await
    .map_err(|error| EmbedError::Failed(format!("the embedding task did not finish: {error}")))??;

    let mut landed = 0;
    for (index, vector) in pending.into_iter().zip(vectors) {
        blob.chunks[index].vector = Some(vector);
        blob.chunks[index].model = model.clone();
        landed += 1;
    }
    Ok(landed)
}

/// One file in a commit's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub blob: String,
    pub size: u64,
}

/// Every blob in a commit, with its path and size. One `ls-tree` for the whole repo:
/// asking per file would be a process per file, and the size comes free here, so an
/// oversized blob is skipped without ever being read.
async fn read_tree(mirror: &Repo, commit: &str) -> Result<Vec<TreeEntry>, IndexError> {
    let raw = mirror
        .run(&[
            "ls-tree",
            "-r",
            "-z",
            "--format=%(objecttype)%x09%(objectname)%x09%(objectsize)%x09%(path)",
            "--end-of-options",
            commit,
        ])
        .await?;
    Ok(raw
        .split('\0')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            // The path is last and may itself contain tabs, so it takes the remainder.
            let mut fields = record.splitn(4, '\t');
            let kind = fields.next()?;
            let blob = fields.next()?;
            let size = fields.next()?;
            let path = fields.next()?;
            // Trees and submodules have no content to index. Symlinks are blobs, but
            // their content is a path, so the extension filter drops them in practice
            // and a stray one costs one window chunk.
            (kind == "blob").then(|| TreeEntry {
                path: path.to_owned(),
                blob: blob.to_owned(),
                size: size.trim().parse().unwrap_or(0),
            })
        })
        .collect())
}

/// A blob's text, or `None` when it is not text at all.
async fn read_source(mirror: &Repo, blob: &str) -> Result<Option<String>, IndexError> {
    let Some(bytes) = mirror.read_blob(blob).await? else {
        // The object went away between the ls-tree and now. Not fatal, and not a
        // reason to abandon the rest of the run.
        return Ok(None);
    };
    // A NUL byte in the first block is git's own binary test, and it is the right
    // one: text files do not contain NUL.
    if bytes.iter().take(8000).any(|byte| *byte == 0) {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

// ---- queries ---------------------------------------------------------------------

/// One hit from the semantic search.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct SimilarHit {
    pub path: String,
    pub symbol: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub score: f32,
    pub snippet: String,
}

/// Brute-force cosine over a repo's chunks, best first.
///
/// Deliberately a scan. The whole table for a personal repo is a few thousand short
/// vectors; the scan costs less than the round trip that asked for it, and it has no
/// index to rebuild, no recall cliff, and no tuning.
pub fn rank(chunks: &[StoredChunk], query: &[f32], limit: usize) -> Vec<SimilarHit> {
    let mut scored: Vec<(f32, &StoredChunk)> = chunks
        .iter()
        .filter_map(|chunk| {
            let vector = chunk.vector.as_ref()?;
            let score = cosine(vector, query);
            (score > 0.0).then_some((score, chunk))
        })
        .collect();
    // Highest score first; ties break on path then line, so the order is stable
    // across runs rather than whatever the scan happened to visit first.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.path.cmp(&b.1.path))
            .then_with(|| a.1.start_line.cmp(&b.1.start_line))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(score, chunk)| SimilarHit {
            path: chunk.path.clone(),
            symbol: chunk.symbol.clone(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            score,
            snippet: chunk.snippet.clone(),
        })
        .collect()
}

/// One line matched by the text search.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct TextHit {
    pub path: String,
    pub line: i64,
    pub text: String,
}

/// `git grep` against the mirror at a revision.
///
/// There is no stored full-text index and there should not be: git already holds
/// every byte, `git grep` reads packed objects directly, and an index would only be
/// a second copy that can disagree with the first.
pub async fn grep(
    mirror: &Repo,
    rev: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<TextHit>, IndexError> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let out = mirror
        .try_run(&[
            "grep",
            // Line numbers, skip binaries, literal text, NUL-separated fields so a
            // path containing a colon cannot be misread as a field boundary.
            "-n",
            "-I",
            "--fixed-strings",
            "--null",
            "--no-color",
            "--max-count",
            &limit.to_string(),
            "-e",
            query,
            "--end-of-options",
            rev,
        ])
        .await?;
    // Exit 1 is "no matches", which is an answer. Anything else with no output is
    // treated the same way: an empty list beats a 500.
    if !out.ok() && out.code != Some(1) && out.stdout.is_empty() {
        return Ok(Vec::new());
    }

    let mut hits = Vec::new();
    for line in out.stdout.lines() {
        // `<rev>:<path>\0<line>\0<text>` — the revision prefix ends at the first colon
        // and the path ends at the first NUL.
        let Some((_rev, rest)) = line.split_once(':') else { continue };
        let mut fields = rest.splitn(3, '\0');
        let (Some(path), Some(number), Some(text)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(number) = number.trim().parse::<i64>() else { continue };
        hits.push(TextHit { path: path.to_owned(), line: number, text: text.to_owned() });
        if hits.len() >= limit {
            break;
        }
    }
    Ok(hits)
}

/// The `code` stanza `/brain` carries for a repo.
pub fn brain_stanza(db: &Db, repo: &str) -> serde_json::Value {
    let counts: CodeCounts = db.code_counts(repo).unwrap_or_default();
    let last: Option<CodeRunRow> = db.code_last_run(repo).ok().flatten();
    match last {
        None => serde_json::json!({
            "indexed": false,
            "files": counts.files,
            "chunks": counts.chunks,
            "symbols": counts.symbols,
            "embedded_chunks": counts.embedded,
        }),
        Some(row) => serde_json::json!({
            "indexed": true,
            "last_indexed_at": row.created_at,
            "age_seconds": age_seconds(&row.created_at),
            "commit": row.run.commit,
            "files": counts.files,
            "chunks": counts.chunks,
            "symbols": counts.symbols,
            "embedded_chunks": counts.embedded,
            "note": row.run.note,
        }),
    }
}

/// How long ago a stored timestamp was, in whole seconds. `None` when it will not
/// parse, which cannot happen for a value this code wrote.
fn age_seconds(at: &str) -> Option<i64> {
    let parsed = time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
        .ok()?;
    Some((time::OffsetDateTime::now_utc() - parsed).whole_seconds().max(0))
}

/// Definitions of a symbol.
pub fn definitions(db: &Db, repo: &str, symbol: &str) -> Vec<SymbolHit> {
    db.code_definitions(repo, symbol).unwrap_or_default()
}

/// Every reference to a symbol.
pub fn references(db: &Db, repo: &str, symbol: &str) -> Vec<RefHit> {
    db.code_references(repo, symbol, false).unwrap_or_default()
}

/// The distinct functions that call a symbol, each with one example site.
pub fn callers(db: &Db, repo: &str, symbol: &str) -> Vec<RefHit> {
    let mut seen: BTreeSet<(Option<String>, String)> = BTreeSet::new();
    db.code_references(repo, symbol, true)
        .unwrap_or_default()
        .into_iter()
        .filter(|hit| seen.insert((hit.caller.clone(), hit.path.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::StoredChunk;

    fn chunk(path: &str, line: i64, vector: &[f32]) -> StoredChunk {
        StoredChunk {
            path: path.to_owned(),
            symbol: None,
            start_line: line,
            end_line: line + 5,
            snippet: format!("{path}:{line}"),
            vector: Some(vector.to_vec()),
            model: "test".to_owned(),
        }
    }

    #[test]
    fn ranking_puts_the_closest_vector_first() {
        let chunks = vec![
            chunk("far.rs", 1, &[0.1, 1.0, 0.0]),
            chunk("near.rs", 1, &[1.0, 0.1, 0.0]),
            chunk("middle.rs", 1, &[0.7, 0.7, 0.0]),
        ];
        let hits = rank(&chunks, &[1.0, 0.0, 0.0], 10);
        assert_eq!(
            hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["near.rs", "middle.rs", "far.rs"]
        );
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn a_chunk_with_nothing_in_common_with_the_query_is_not_a_hit() {
        let chunks = vec![chunk("orthogonal.rs", 1, &[0.0, 1.0])];
        assert!(rank(&chunks, &[1.0, 0.0], 10).is_empty());
    }

    #[test]
    fn ranking_honours_the_limit_and_skips_unembedded_chunks() {
        let mut chunks = vec![
            chunk("a.rs", 1, &[1.0, 0.0]),
            chunk("b.rs", 1, &[0.9, 0.1]),
            chunk("c.rs", 1, &[0.8, 0.2]),
        ];
        chunks.push(StoredChunk { vector: None, ..chunk("never.rs", 1, &[1.0, 0.0]) });
        let hits = rank(&chunks, &[1.0, 0.0], 2);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.path != "never.rs"));
    }

    #[test]
    fn ties_break_on_path_and_line_so_the_order_is_stable() {
        let chunks = vec![
            chunk("z.rs", 1, &[1.0, 0.0]),
            chunk("a.rs", 9, &[1.0, 0.0]),
            chunk("a.rs", 2, &[1.0, 0.0]),
        ];
        let hits = rank(&chunks, &[1.0, 0.0], 10);
        assert_eq!(
            hits.iter().map(|h| (h.path.as_str(), h.start_line)).collect::<Vec<_>>(),
            vec![("a.rs", 2), ("a.rs", 9), ("z.rs", 1)]
        );
    }

    #[test]
    fn a_repo_that_never_indexed_still_has_a_brain_stanza() {
        let db = Db::in_memory().unwrap();
        let stanza = brain_stanza(&db, "demo");
        assert_eq!(stanza["indexed"], serde_json::json!(false));
        assert_eq!(stanza["chunks"], serde_json::json!(0));
    }
}
