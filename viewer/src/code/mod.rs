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
    ChunkVector, CodeBlob, CodeCounts, CodeRun, CodeRunRow, Db, RefHit, SymbolHit,
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

/// The ceiling on `/code/graph`, the one endpoint with no `limit` of its own. A repo
/// past it gets `truncated: true` and is expected to ask the per-symbol endpoints for
/// the rest; a diagram drawn from a hundred thousand edges was never going to render.
pub const MAX_GRAPH_EDGES: usize = 100_000;

/// The most total matches `/code/text` will read out of `git grep`.
pub const MAX_TEXT_MATCHES: usize = 1_000;

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
        // Unless the last run recorded a degradation — a model that would not load, an
        // indexer that was not there. Then every blob is fresh again, so the re-parse
        // carries the unfinished work back through. Counting chunks without vectors
        // would look like the same signal and is not: a chunk with a blank snippet is
        // never embedded, so that count never reaches zero and the repo would re-parse
        // forever.
        let degraded = self
            .db
            .code_last_run(repo)?
            .is_some_and(|last| !last.run.note.is_empty());
        let known: BTreeSet<String> = if degraded {
            BTreeSet::new()
        } else {
            self.db.code_known_blobs(repo, &candidates)?.into_iter().collect()
        };
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

        // Nothing new, and this exact commit is what the last run read: there is no
        // work of any kind to do. Worth checking explicitly because the expensive part
        // is not the parse — it is the SCIP overlay, which clones the repo and runs a
        // language indexer over it. Without this, a merge that only moved a card would
        // pay for a rust-analyzer pass.
        // `degraded` above has already made every blob fresh in that case, so this skip
        // only ever fires on a run that finished cleanly.
        if fresh.is_empty()
            && self
                .db
                .code_last_run(repo)?
                .is_some_and(|last| last.run.commit == commit)
        {
            return Ok(run);
        }

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
                // Binary, or gone. Record that it was looked at so the next run does
                // not read it again; a repo full of images would otherwise re-read
                // every one of them on every merge.
                self.db.code_mark_seen(repo, &entry.blob)?;
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
    let Some(bytes) = mirror.read_blob(blob, MAX_FILE_BYTES).await? else {
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
pub fn rank(chunks: &[ChunkVector], query: &[f32], limit: usize) -> Vec<(f32, i64)> {
    let mut scored: Vec<(f32, &ChunkVector)> = chunks
        .iter()
        .filter_map(|chunk| {
            let score = cosine(&chunk.vector, query);
            (score > 0.0).then_some((score, chunk))
        })
        .collect();
    // Highest score first; ties break on path then id, so the order is stable across
    // runs rather than whatever the scan happened to visit first.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.path.cmp(&b.1.path))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    scored.into_iter().take(limit).map(|(score, chunk)| (score, chunk.id)).collect()
}

/// What a semantic search answered, and how much it had to look at.
#[derive(Debug, Clone, Default)]
pub struct Similar {
    pub hits: Vec<SimilarHit>,
    pub scanned: usize,
}

/// Score a query vector against a repo's chunks and return the top-k, snippets and all.
///
/// Two passes on purpose. The scan reads only ids, paths, and vectors, because it
/// touches every chunk and a snippet runs to eight kilobytes; the snippets are then
/// fetched for the handful of rows that placed. The whole thing runs on a blocking
/// thread: it holds the global connection mutex and does real arithmetic, and neither
/// belongs on a runtime worker serving other requests.
pub async fn similar(db: &Db, repo: &str, query: Vec<f32>, limit: usize) -> Similar {
    let db = db.clone();
    let repo = repo.to_owned();
    tokio::task::spawn_blocking(move || {
        let chunks = db.code_chunk_vectors(&repo).unwrap_or_default();
        let scanned = chunks.len();
        let placed = rank(&chunks, &query, limit);
        let ids: Vec<i64> = placed.iter().map(|(_, id)| *id).collect();
        let rows = db.code_chunks_by_id(&repo, &ids).unwrap_or_default();
        let hits = placed
            .into_iter()
            .filter_map(|(score, id)| {
                let chunk = rows.get(&id)?;
                Some(SimilarHit {
                    path: chunk.path.clone(),
                    symbol: chunk.symbol.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    score,
                    snippet: chunk.snippet.clone(),
                })
            })
            .collect();
        Similar { hits, scanned }
    })
    .await
    .unwrap_or_default()
}

/// One line matched by the text search.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct TextHit {
    pub path: String,
    pub line: i64,
    pub text: String,
}

/// What the text search found, and whether it stopped early.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextResults {
    pub hits: Vec<TextHit>,
    /// True when git had more to say than the caps allowed.
    pub truncated: bool,
}

/// The output cap on one `git grep`. Generous next to any `limit` a caller asks for,
/// and small enough that a one-letter query against a large repo cannot be a memory
/// event. A matched line is also clipped, so one minified bundle line is not the
/// whole response.
const GREP_MAX_BYTES: usize = 4 * 1024 * 1024;
const GREP_MAX_LINE: usize = 500;

/// `git grep` against the mirror at a revision.
///
/// There is no stored full-text index and there should not be: git already holds
/// every byte, `git grep` reads packed objects directly, and an index would only be
/// a second copy that can disagree with the first.
/// Three caps, because git's own one is not enough. `--max-count` is *per file*, so a
/// common word in a large repo yields that many matches in each of thousands of files;
/// the output is therefore also capped in bytes, and the parse stops at
/// [`MAX_TEXT_MATCHES`] whatever the caller asked for. Whenever any of them bites, the
/// answer says `truncated: true` rather than looking complete.
pub async fn grep(
    mirror: &Repo,
    rev: &str,
    query: &str,
    limit: usize,
) -> Result<TextResults, IndexError> {
    if query.trim().is_empty() {
        return Ok(TextResults::default());
    }
    let wanted = limit.min(MAX_TEXT_MATCHES);
    let (out, capped) = mirror
        .run_capped(
            &[
                "grep",
                // Line numbers, skip binaries, literal text, NUL-separated fields so a
                // path containing a colon cannot be misread as a field boundary.
                "-n",
                "-I",
                "--fixed-strings",
                "--null",
                "--no-color",
                "--max-count",
                &wanted.to_string(),
                "-e",
                query,
                "--end-of-options",
                rev,
            ],
            GREP_MAX_BYTES,
        )
        .await?;
    // Exit 1 is "no matches", which is an answer. Anything else with no output is
    // treated the same way: an empty list beats a 500.
    if out.stdout.is_empty() {
        return Ok(TextResults::default());
    }

    let mut hits = Vec::new();
    let mut more = capped;
    for line in out.stdout.lines() {
        if hits.len() >= wanted {
            more = true;
            break;
        }
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
        hits.push(TextHit {
            path: path.to_owned(),
            line: number,
            text: clip(text, GREP_MAX_LINE),
        });
    }
    Ok(TextResults { hits, truncated: more })
}

/// Cut a string to a byte budget on a character boundary.
fn clip(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

/// One edge of the whole-repo graph dump.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Edge {
    /// `defines`, `calls`, or `references`.
    pub kind: &'static str,
    /// The file for a `defines` edge; the enclosing function for the others, falling
    /// back to the file when the call sits at module level.
    pub from: String,
    pub to: String,
    pub file: String,
    pub line: i64,
    pub source: String,
}

/// Everything the indexes know about a repo, as one document.
///
/// This is the bulk companion to the per-symbol endpoints. An agent drawing a diagram
/// needs the shape of the whole system, and paging through `?symbol=` calls to
/// assemble it would be a request per node. Absent indexes degrade the document, not
/// the request: worst case is the file inventory with `symbols: []`.
pub async fn graph(db: &Db, repo: &str) -> serde_json::Value {
    let db = db.clone();
    let repo = repo.to_owned();
    // Three whole tables and a JSON tree built from them. On a real-sized repo that is
    // seconds of allocation under the connection mutex, which must not sit on a
    // runtime worker that other requests are waiting on.
    tokio::task::spawn_blocking(move || {
        let symbols = db.code_all_symbols(&repo).unwrap_or_default();
        let references = db.code_all_refs(&repo).unwrap_or_default();

        // A ceiling, because this is the one endpoint with no `limit`. A caller that
        // hits it is told so rather than handed a silently short answer; the
        // per-symbol endpoints are how you then ask about the rest.
        let total = symbols.len() + references.len();
        let mut edges: Vec<Edge> = Vec::with_capacity(total.min(MAX_GRAPH_EDGES));
        for symbol in symbols.iter().take(MAX_GRAPH_EDGES) {
            edges.push(Edge {
                kind: "defines",
                from: symbol.path.clone(),
                to: symbol.name.clone(),
                file: symbol.path.clone(),
                line: symbol.start_line,
                source: symbol.source.clone(),
            });
        }
        for reference in references.iter().take(MAX_GRAPH_EDGES - edges.len()) {
            edges.push(Edge {
                // The graph layer records call edges; anything else it learns is a
                // plain reference. Both spellings are in the dump so a reader can tell
                // them apart.
                kind: if reference.kind == "call" { "calls" } else { "references" },
                from: reference.caller.clone().unwrap_or_else(|| reference.path.clone()),
                to: reference.name.clone(),
                file: reference.path.clone(),
                line: reference.line,
                source: reference.source.clone(),
            });
        }

        serde_json::json!({
            "generated_at": crate::db::now(),
            // The commit the index was last built from, so a reader can tell whether
            // the dump describes the tree they are looking at.
            "commit": db.code_last_run(&repo).ok().flatten().map(|row| row.run.commit),
            "files": db.code_file_list(&repo).unwrap_or_default(),
            "symbols": symbols,
            "edges": edges,
            "truncated": total > MAX_GRAPH_EDGES,
        })
    })
    .await
    .unwrap_or_else(|error| {
        serde_json::json!({
            "generated_at": crate::db::now(),
            "commit": serde_json::Value::Null,
            "files": [],
            "symbols": [],
            "edges": [],
            "truncated": false,
            "error": format!("the graph could not be assembled: {error}"),
        })
    })
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
    use crate::db::ChunkVector;

    fn chunk(path: &str, id: i64, vector: &[f32]) -> ChunkVector {
        ChunkVector { id, path: path.to_owned(), vector: vector.to_vec() }
    }

    /// The paths the ranking placed, in order.
    fn placed<'a>(chunks: &'a [ChunkVector], query: &[f32], limit: usize) -> Vec<&'a str> {
        rank(chunks, query, limit)
            .into_iter()
            .map(|(_, id)| {
                chunks.iter().find(|c| c.id == id).expect("a ranked id is a real chunk").path.as_str()
            })
            .collect()
    }

    #[test]
    fn ranking_puts_the_closest_vector_first() {
        let chunks = vec![
            chunk("far.rs", 1, &[0.1, 1.0, 0.0]),
            chunk("near.rs", 2, &[1.0, 0.1, 0.0]),
            chunk("middle.rs", 3, &[0.7, 0.7, 0.0]),
        ];
        assert_eq!(
            placed(&chunks, &[1.0, 0.0, 0.0], 10),
            vec!["near.rs", "middle.rs", "far.rs"]
        );
        let scored = rank(&chunks, &[1.0, 0.0, 0.0], 10);
        assert!(scored[0].0 > scored[1].0);
    }

    #[test]
    fn a_chunk_with_nothing_in_common_with_the_query_is_not_a_hit() {
        let chunks = vec![chunk("orthogonal.rs", 1, &[0.0, 1.0])];
        assert!(rank(&chunks, &[1.0, 0.0], 10).is_empty());
    }

    #[test]
    fn ranking_honours_the_limit() {
        let chunks = vec![
            chunk("a.rs", 1, &[1.0, 0.0]),
            chunk("b.rs", 2, &[0.9, 0.1]),
            chunk("c.rs", 3, &[0.8, 0.2]),
        ];
        assert_eq!(rank(&chunks, &[1.0, 0.0], 2).len(), 2);
    }

    #[test]
    fn ties_break_on_path_and_id_so_the_order_is_stable() {
        let chunks = vec![
            chunk("z.rs", 1, &[1.0, 0.0]),
            chunk("a.rs", 9, &[1.0, 0.0]),
            chunk("a.rs", 2, &[1.0, 0.0]),
        ];
        let order: Vec<(&str, i64)> = rank(&chunks, &[1.0, 0.0], 10)
            .into_iter()
            .map(|(_, id)| {
                let chunk = chunks.iter().find(|c| c.id == id).expect("real");
                (chunk.path.as_str(), chunk.id)
            })
            .collect();
        assert_eq!(order, vec![("a.rs", 2), ("a.rs", 9), ("z.rs", 1)]);
    }

    #[test]
    fn a_repo_that_never_indexed_still_has_a_brain_stanza() {
        let db = Db::in_memory().unwrap();
        let stanza = brain_stanza(&db, "demo");
        assert_eq!(stanza["indexed"], serde_json::json!(false));
        assert_eq!(stanza["chunks"], serde_json::json!(0));
    }
}
