//! The code-intelligence endpoints against real repos built with real git.
//!
//! Nothing here downloads a model or needs a network: the testbed's embedder is a
//! deterministic word-bag (see `common::WordBag`), which is exactly what the trait in
//! `code::embed` exists to allow.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{Work, get, make_remote, post_json, simple_bed};
use nashcode::code::{EmbedError, Embedder, Embeddings};
use serde_json::Value;

/// A small repo with real code in three languages plus one file no grammar reads.
fn code_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);

    work.write(
        "src/net.rs",
        "\
use std::time::Duration;

/// Sleep, then try again.
fn backoff(attempt: u32) {
    sleep(Duration::from_secs(attempt as u64));
}

pub fn retry(attempts: u32) {
    for attempt in 0..attempts {
        backoff(attempt);
        connect();
    }
}

fn connect() {
    open_socket();
}
",
    );
    work.write(
        "app/views.py",
        "\
class Widget:
    def draw(self):
        render(self)


def top():
    Widget().draw()
",
    );
    work.write(
        "web/app.ts",
        "\
export interface Options { retries: number }

export function connect(options: Options) {
  return retry(options.retries);
}
",
    );
    work.write("notes/plain.txt", "the word retry appears here, in prose\n");
    work.commit_all("initial");
    work.push("main");
    work
}

// ---- full text ---------------------------------------------------------------------

#[tokio::test]
async fn text_search_finds_a_literal_string_without_any_stored_index() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    // Deliberately never indexed: `git grep` needs no index at all.

    let (status, body) = get(&bed.router, "/demo/code/text?q=open_socket").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    let hits = answer["hits"].as_array().expect("hits");
    assert_eq!(hits.len(), 1, "{body}");
    assert_eq!(hits[0]["path"], "src/net.rs");
    assert_eq!(hits[0]["line"], 16);
    assert!(hits[0]["text"].as_str().expect("text").contains("open_socket"));
}

#[tokio::test]
async fn text_search_spans_languages_and_honours_the_limit() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;

    let (_, body) = get(&bed.router, "/demo/code/text?q=retry").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    let paths: Vec<&str> = answer["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit["path"].as_str().expect("path"))
        .collect();
    assert!(paths.contains(&"src/net.rs"));
    assert!(paths.contains(&"web/app.ts"));
    assert!(paths.contains(&"notes/plain.txt"), "prose is text too");

    let (_, capped) = get(&bed.router, "/demo/code/text?q=retry&limit=1").await;
    let capped: Value = serde_json::from_str(&capped).expect("json");
    assert_eq!(capped["hits"].as_array().expect("hits").len(), 1);
}

#[tokio::test]
async fn a_query_that_matches_nothing_is_an_empty_list_not_an_error() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    let (status, body) = get(&bed.router, "/demo/code/text?q=nothing_here_at_all").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(answer["hits"].as_array().expect("hits").is_empty());
}

#[tokio::test]
async fn text_search_reports_when_it_stopped_early() {
    let bed = simple_bed(|root| {
        let remote = make_remote(root, "demo");
        let work = Work::clone_from(&remote);
        // `git grep --max-count` is per file, so many files each holding the word is
        // exactly the shape that used to come back unbounded.
        for file in 0..40 {
            let body: String = (0..40).map(|_| "needle here\n").collect();
            work.write(&format!("src/f{file}.txt"), &body);
        }
        work.commit_all("initial");
        work.push("main");
        work
    });
    bed.mirrors.refresh_now("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/text?q=needle&limit=5").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["hits"].as_array().expect("hits").len(), 5);
    assert_eq!(answer["truncated"], true, "1600 matches, 5 returned: {body}");
}

#[tokio::test]
async fn a_complete_text_answer_is_not_marked_truncated() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    let (_, body) = get(&bed.router, "/demo/code/text?q=open_socket").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["truncated"], false, "{body}");
}

#[tokio::test]
async fn a_very_long_matched_line_is_clipped_rather_than_returned_whole() {
    let bed = simple_bed(|root| {
        let remote = make_remote(root, "demo");
        let work = Work::clone_from(&remote);
        // One minified bundle line: a real shape, and one that used to be the whole
        // response body.
        work.write("web/bundle.js", &format!("var x=\"{}\";needle\n", "a".repeat(200_000)));
        work.commit_all("initial");
        work.push("main");
        work
    });
    bed.mirrors.refresh_now("demo").await;

    let (_, body) = get(&bed.router, "/demo/code/text?q=needle").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    let text = answer["hits"][0]["text"].as_str().expect("text");
    assert!(text.len() < 1000, "the line came back whole: {} bytes", text.len());
    assert!(text.ends_with('…'), "and it says it was cut: {text:?}");
}

#[tokio::test]
async fn text_search_without_a_query_is_a_client_error() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    let (status, _) = get(&bed.router, "/demo/code/text").await;
    assert_eq!(status, 400);
}

// ---- the graph ---------------------------------------------------------------------

#[tokio::test]
async fn definitions_come_back_with_their_file_and_line_range() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/def?symbol=retry").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    let definitions = answer["definitions"].as_array().expect("definitions");
    assert_eq!(definitions.len(), 1, "{body}");
    assert_eq!(definitions[0]["path"], "src/net.rs");
    assert_eq!(definitions[0]["kind"], "function");
    assert_eq!(definitions[0]["start_line"], 8);
    assert_eq!(definitions[0]["source"], "treesitter");
}

#[tokio::test]
async fn a_symbol_defined_in_two_languages_answers_with_both() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (_, body) = get(&bed.router, "/demo/code/def?symbol=connect").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    let paths: Vec<&str> = answer["definitions"]
        .as_array()
        .expect("definitions")
        .iter()
        .map(|hit| hit["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, vec!["src/net.rs", "web/app.ts"], "{body}");
}

#[tokio::test]
async fn callers_names_the_function_a_call_sits_inside() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/callers?symbol=backoff").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    let callers = answer["callers"].as_array().expect("callers");
    assert_eq!(callers.len(), 1, "{body}");
    assert_eq!(callers[0]["caller"], "retry");
    assert_eq!(callers[0]["path"], "src/net.rs");
}

#[tokio::test]
async fn python_method_calls_are_attributed_too() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (_, body) = get(&bed.router, "/demo/code/callers?symbol=render").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    let callers = answer["callers"].as_array().expect("callers");
    assert_eq!(callers[0]["caller"], "draw", "{body}");
    assert_eq!(callers[0]["path"], "app/views.py");
}

#[tokio::test]
async fn references_include_every_use_across_files() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (_, body) = get(&bed.router, "/demo/code/refs?symbol=retry").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    let paths: Vec<&str> = answer["references"]
        .as_array()
        .expect("references")
        .iter()
        .map(|hit| hit["path"].as_str().expect("path"))
        .collect();
    assert!(paths.contains(&"web/app.ts"), "the TypeScript call counts: {body}");
}

#[tokio::test]
async fn a_symbol_the_graph_never_saw_points_at_text_search_instead_of_erroring() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/def?symbol=no_such_thing").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(answer["definitions"].as_array().expect("definitions").is_empty());
    assert!(
        answer["hint"].as_str().expect("hint").contains("/code/text"),
        "the hint names the next thing to try: {body}"
    );
}

#[tokio::test]
async fn a_repo_that_was_never_indexed_answers_empty_rather_than_failing() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    let (status, body) = get(&bed.router, "/demo/code/callers?symbol=retry").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(answer["callers"].as_array().expect("callers").is_empty());
    // "Never indexed" and "indexed, not there" are different facts and want different
    // next steps, so an empty answer says which one it is.
    assert_eq!(answer["indexed"], false, "{body}");
    assert!(answer["hint"].as_str().expect("hint").contains("never been indexed"), "{body}");
    assert!(answer["hint"].as_str().expect("hint").contains("code/index"), "{body}");
}

#[tokio::test]
async fn an_indexed_repo_missing_a_symbol_points_somewhere_different() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;
    let (_, body) = get(&bed.router, "/demo/code/callers?symbol=no_such_thing").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["indexed"], true, "{body}");
    let hint = answer["hint"].as_str().expect("hint");
    assert!(!hint.contains("never been indexed"), "{hint}");
    assert!(hint.contains("/code/text"), "{hint}");
}

// ---- embeddings --------------------------------------------------------------------

#[tokio::test]
async fn semantic_search_ranks_the_chunk_that_is_about_the_query() {
    let bed = simple_bed(code_fixture);
    let run = bed.index("demo").await;
    assert!(run.embedded > 0, "the fake embedder ran: {run:?}");

    let (status, body) = get(&bed.router, "/demo/code/similar?q=retry%20backoff%20sleep").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    let hits = answer["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "{body}");
    assert_eq!(hits[0]["path"], "src/net.rs");
    assert_eq!(hits[0]["symbol"], "backoff", "{body}");
    assert!(hits[0]["score"].as_f64().expect("score") > 0.0);
}

#[tokio::test]
async fn semantic_search_honours_the_limit_and_reports_what_it_scanned() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;
    let (_, body) = get(&bed.router, "/demo/code/similar?q=connect%20socket&limit=1").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["hits"].as_array().expect("hits").len(), 1);
    assert!(answer["scanned"].as_u64().expect("scanned") > 1);
}

#[tokio::test]
async fn without_a_loaded_model_semantic_search_says_so_and_points_at_text_search() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;
    // Replace the app's slot with an empty one: this is the shape of a box whose
    // ONNX runtime or model download is missing.
    let app = nashcode::web::App { embeddings: Embeddings::new(), ..bed.app.clone() };
    let router = nashcode::web::router(app);

    let (status, body) = get(&router, "/demo/code/similar?q=retry").await;
    assert_eq!(status, 503, "not a 500 and not a hang: {body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    let error = answer["error"].as_str().expect("error");
    assert!(error.contains("unavailable"), "{error}");
    assert!(error.contains("/code/text"), "it names the fallback: {error}");
}

/// An embedder that always fails, the way a half-installed runtime does.
struct Broken;

impl Embedder for Broken {
    fn model(&self) -> &str {
        "broken"
    }

    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Failed("the runtime went away".to_owned()))
    }
}

#[tokio::test]
async fn an_embedder_that_fails_mid_query_is_a_502_with_the_reason() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;
    let app = nashcode::web::App {
        embeddings: Embeddings::with(Arc::new(Broken)),
        ..bed.app.clone()
    };
    let router = nashcode::web::router(app);

    let (status, body) = get(&router, "/demo/code/similar?q=retry").await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("the runtime went away"), "{body}");
}

#[tokio::test]
async fn indexing_without_an_embedder_still_stores_chunks_and_the_graph() {
    let bed = simple_bed(code_fixture);
    // An indexer whose slot can never fill: no `embeddings` feature, no runtime, no
    // model. The graph and the chunks must still land.
    let indexer = nashcode::code::Indexer {
        config: bed.config.clone(),
        db: bed.db.clone(),
        mirrors: bed.mirrors.clone(),
        embeddings: Embeddings::new(),
    };
    bed.mirrors.refresh_now("demo").await;
    let run = indexer.index_default_branch("demo").await.expect("the run starts");

    assert!(run.chunks > 0, "{run:?}");
    assert!(run.symbols > 0, "{run:?}");
    assert_eq!(run.embedded, 0, "nothing was embedded: {run:?}");

    let (status, body) = get(&bed.router, "/demo/code/def?symbol=retry").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["definitions"].as_array().expect("definitions").len(), 1);
}

// ---- incrementality ----------------------------------------------------------------

#[tokio::test]
async fn a_second_run_over_unchanged_content_parses_and_embeds_nothing() {
    let bed = simple_bed(code_fixture);
    let first = bed.index("demo").await;
    assert!(first.files_indexed > 0, "{first:?}");

    let again = bed.index("demo").await;
    assert_eq!(again.files_seen, first.files_seen, "the same tree");
    assert_eq!(again.files_indexed, 0, "no blob was new: {again:?}");
    assert_eq!(again.embedded, 0, "and so nothing was embedded again: {again:?}");
}

#[tokio::test]
async fn a_blob_that_yields_no_chunks_is_still_only_parsed_once() {
    let bed = simple_bed(|root| {
        let remote = make_remote(root, "demo");
        let work = Work::clone_from(&remote);
        // Three shapes that store nothing: empty, binary, and whitespace.
        work.write("src/empty.rs", "");
        work.write_bytes("assets/logo.png", &[0x89, b'P', b'N', b'G', 0x00, 0x1a, 0x0a]);
        work.write("docs/blank.txt", "\n\n\n");
        work.write("src/real.rs", "pub fn kept() {}\n");
        work.commit_all("initial");
        work.push("main");
        work
    });

    let first = bed.index("demo").await;
    assert_eq!(first.files_seen, 4, "{first:?}");
    // The binary file is read and rejected rather than parsed, so it never counts as
    // indexed; the other three are parsed, two of them to nothing.
    assert_eq!(first.files_indexed, 3, "{first:?}");

    // Without a record of the blobs that produced nothing, all four would be re-read
    // and re-parsed on every merge forever.
    let again = bed.index("demo").await;
    assert_eq!(again.files_seen, 4, "the same tree: {again:?}");
    assert_eq!(again.files_indexed, 0, "nothing was read a second time: {again:?}");
}

#[tokio::test]
async fn only_the_file_whose_blob_changed_is_reindexed() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let work = common::Work::clone_from(&bed.remote_root().join("demo.git"));
    work.write("app/views.py", "def top():\n    pass\n");
    work.commit_all("shrink views");
    work.push("main");

    let run = bed.index("demo").await;
    assert_eq!(run.files_indexed, 1, "exactly the one changed file: {run:?}");

    // The symbols the old content held are gone with it.
    let (_, body) = get(&bed.router, "/demo/code/def?symbol=Widget").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(
        answer["definitions"].as_array().expect("definitions").is_empty(),
        "the deleted class no longer answers: {body}"
    );
}

#[tokio::test]
async fn a_renamed_file_keeps_its_chunks_and_moves_its_path() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let work = common::Work::clone_from(&bed.remote_root().join("demo.git"));
    common::git(&work.dir, &["mv", "src/net.rs", "src/network.rs"]);
    work.commit_all("rename net to network");
    work.push("main");

    let run = bed.index("demo").await;
    assert_eq!(run.files_indexed, 0, "the content did not change: {run:?}");

    let (_, body) = get(&bed.router, "/demo/code/def?symbol=retry").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["definitions"][0]["path"], "src/network.rs", "{body}");
}

#[tokio::test]
async fn a_deleted_file_stops_answering_queries() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let work = common::Work::clone_from(&bed.remote_root().join("demo.git"));
    common::git(&work.dir, &["rm", "app/views.py"]);
    work.commit_all("drop the python");
    work.push("main");
    bed.index("demo").await;

    let (_, body) = get(&bed.router, "/demo/code/def?symbol=Widget").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(answer["definitions"].as_array().expect("definitions").is_empty(), "{body}");

    let (_, similar) = get(&bed.router, "/demo/code/similar?q=widget%20draw%20render").await;
    let similar: Value = serde_json::from_str(&similar).expect("json");
    assert!(
        similar["hits"]
            .as_array()
            .expect("hits")
            .iter()
            .all(|hit| hit["path"] != "app/views.py"),
        "{similar}"
    );
}

// ---- the bulk graph dump -----------------------------------------------------------

#[tokio::test]
async fn the_graph_dump_carries_files_symbols_and_edges_in_one_call() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/graph").await;
    assert_eq!(status, 200, "{body}");
    let graph: Value = serde_json::from_str(&body).expect("json");

    assert_eq!(graph["repo"], "demo");
    assert!(graph["generated_at"].as_str().expect("generated_at").ends_with('Z'));
    assert_eq!(
        graph["commit"].as_str().expect("commit").len(),
        40,
        "the dump names the commit it describes: {body}"
    );

    // Every file in the tree, with its language and the blob behind it.
    let files = graph["files"].as_array().expect("files");
    assert_eq!(files.len(), 4, "{body}");
    let rust = files.iter().find(|f| f["path"] == "src/net.rs").expect("net.rs");
    assert_eq!(rust["lang"], "rust");
    assert_eq!(rust["blob"].as_str().expect("blob").len(), 40);
    let prose = files.iter().find(|f| f["path"] == "notes/plain.txt").expect("plain.txt");
    assert_eq!(prose["lang"], "other", "a file with no grammar is still inventoried");

    // Every symbol, with the file and line it is on.
    let symbols = graph["symbols"].as_array().expect("symbols");
    let retry = symbols.iter().find(|s| s["name"] == "retry").expect("retry");
    assert_eq!(retry["path"], "src/net.rs");
    assert_eq!(retry["kind"], "function");
    assert_eq!(retry["start_line"], 8);

    // Edges: a `defines` per symbol, a `calls` per call site.
    let edges = graph["edges"].as_array().expect("edges");
    assert!(
        edges.iter().any(|e| {
            e["kind"] == "defines" && e["from"] == "src/net.rs" && e["to"] == "retry"
        }),
        "the file defines the symbol: {body}"
    );
    let call = edges
        .iter()
        .find(|e| e["kind"] == "calls" && e["to"] == "backoff")
        .expect("the call to backoff");
    assert_eq!(call["from"], "retry", "the edge starts at the calling function");
    assert_eq!(call["file"], "src/net.rs");
    assert_eq!(call["source"], "treesitter");
}

#[tokio::test]
async fn a_call_outside_any_function_starts_its_edge_at_the_file() {
    let bed = simple_bed(|root| {
        let remote = make_remote(root, "demo");
        let work = Work::clone_from(&remote);
        // A module-level call: there is no enclosing function to name.
        work.write("app/boot.py", "import sys\n\nconfigure(sys.argv)\n\n\ndef later():\n    pass\n");
        work.commit_all("initial");
        work.push("main");
        work
    });
    bed.index("demo").await;

    let (_, body) = get(&bed.router, "/demo/code/graph").await;
    let graph: Value = serde_json::from_str(&body).expect("json");
    let edge = graph["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .find(|e| e["to"] == "configure")
        .expect("the module-level call");
    assert_eq!(edge["from"], "app/boot.py", "{body}");
}

#[tokio::test]
async fn a_repo_with_no_index_dumps_an_empty_graph_rather_than_an_error() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/graph").await;
    assert_eq!(status, 200, "{body}");
    let graph: Value = serde_json::from_str(&body).expect("json");
    assert!(graph["files"].as_array().expect("files").is_empty());
    assert!(graph["symbols"].as_array().expect("symbols").is_empty());
    assert!(graph["edges"].as_array().expect("edges").is_empty());
    assert!(graph["commit"].is_null(), "nothing has been indexed: {body}");
}

#[tokio::test]
async fn a_repo_of_only_unparseable_files_dumps_the_inventory_with_no_symbols() {
    let bed = simple_bed(|root| {
        let remote = make_remote(root, "demo");
        let work = Work::clone_from(&remote);
        work.write("README.md", "# just prose\n");
        work.write("data/rows.csv", "a,b\n1,2\n");
        work.commit_all("initial");
        work.push("main");
        work
    });
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code/graph").await;
    assert_eq!(status, 200, "{body}");
    let graph: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(graph["files"].as_array().expect("files").len(), 2);
    assert!(
        graph["symbols"].as_array().expect("symbols").is_empty(),
        "no grammar read these, and that is the documented worst case: {body}"
    );
    assert!(graph["edges"].as_array().expect("edges").is_empty());
}

// ---- status, queueing, and brain ---------------------------------------------------

#[tokio::test]
async fn the_status_endpoint_reports_the_counts_and_the_run() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/demo/code").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["indexed"], true);
    assert_eq!(answer["repo"], "demo");
    assert!(answer["chunks"].as_i64().expect("chunks") > 0);
    assert!(answer["symbols"].as_i64().expect("symbols") > 0);
    assert!(answer["age_seconds"].as_i64().expect("age") >= 0);
    assert_eq!(answer["embeddings_ready"], true);
}

#[tokio::test]
async fn the_index_endpoint_queues_a_run_rather_than_doing_one() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;

    let (status, body) = post_json(&bed.router, "/demo/code/index", serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["queued"], true);
    // The request path did no indexing: the queue is what indexes.
    let (_, after) = get(&bed.router, "/demo/code").await;
    let after: Value = serde_json::from_str(&after).expect("json");
    assert_eq!(after["indexed"], false, "{after}");
}

#[tokio::test]
async fn an_unknown_repo_is_a_404_on_every_code_endpoint() {
    let bed = simple_bed(code_fixture);
    for path in [
        "/nope/code",
        "/nope/code/graph",
        "/nope/code/text?q=x",
        "/nope/code/similar?q=x",
        "/nope/code/def?symbol=x",
        "/nope/code/refs?symbol=x",
        "/nope/code/callers?symbol=x",
    ] {
        let (status, _) = get(&bed.router, path).await;
        assert_eq!(status, 404, "{path}");
    }
}

#[tokio::test]
async fn brain_carries_a_code_stanza_per_repo() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (status, body) = get(&bed.router, "/brain").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    let code = &answer["repos"][0]["code"];
    assert_eq!(code["indexed"], true, "{body}");
    assert!(code["chunks"].as_i64().expect("chunks") > 0);
    assert!(code["symbols"].as_i64().expect("symbols") > 0);
    assert!(code["age_seconds"].as_i64().expect("age") >= 0);
}

#[tokio::test]
async fn brains_code_stanza_exists_before_the_first_index_run() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    let (_, body) = get(&bed.router, "/brain").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["repos"][0]["code"]["indexed"], false, "{body}");
}

// ---- the brain's tools -------------------------------------------------------------

/// A testbed whose `/brain/ask` points at a stub instead of the real API.
fn asking_bed(url: &str) -> common::TestBed {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    code_fixture(&remotes);
    let config = Arc::new(nashcode::config::Config {
        dgit_url: remotes.to_string_lossy().into_owned(),
        git_token: String::new(),
        repos: ["demo"].into_iter().collect(),
        mirrors: root.path().join("mirrors"),
        bind: "127.0.0.1:0".to_owned(),
        db_path: root.path().join("nashcode.db"),
        ci_logs: root.path().join("ci-logs"),
        traces: root.path().join("traces"),
        people_path: root.path().join("people.json"),
        webhooks: Default::default(),
        anthropic_key: Some("test-key".to_owned()),
        anthropic_url: url.to_owned(),
        brain_model: "claude-opus-5".to_owned(),
        bugs_bucket: None,
        bugs_s3_endpoint: None,
        bugs_ingest_url: "http://127.0.0.1:0".to_owned(),
        bugs_drain: None,
        pushover: None,
        public_url: "http://127.0.0.1:0".to_owned(),
        bugs_self_dsn: None,
    });
    common::testbed_from_config(root, config)
}

#[tokio::test]
async fn brain_ask_offers_the_code_tools_and_feeds_their_answers_back() {
    let asked = serde_json::json!({
        "content": [
            { "type": "text", "text": "Let me look." },
            {
                "type": "tool_use",
                "id": "call-1",
                "name": "code_callers",
                "input": { "repo": "demo", "symbol": "backoff" },
            },
        ],
        "stop_reason": "tool_use",
        "model": "claude-opus-5-20260115",
    });
    let answered = serde_json::json!({
        "content": [{ "type": "text", "text": "retry calls backoff, in src/net.rs." }],
        "stop_reason": "end_turn",
        "model": "claude-opus-5-20260115",
    });
    let mut stub =
        common::spawn_stub_seq("HTTP/1.1 200 OK", vec![asked.to_string(), answered.to_string()])
            .await;
    let bed = asking_bed(&stub.url);
    bed.index("demo").await;

    let (status, body) = post_json(
        &bed.router,
        "/brain/ask",
        serde_json::json!({ "question": "who calls backoff?", "repo": "demo" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["answer"], "retry calls backoff, in src/net.rs.");
    assert_eq!(answer["tools_used"][0], "code_callers(demo)");

    // The first request offered the tools.
    let first: Value =
        serde_json::from_str(&stub.received.recv().await.expect("first")).expect("json");
    let names: Vec<&str> = first["tools"]
        .as_array()
        .expect("tools offered")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        vec!["code_text", "code_similar", "code_def", "code_refs", "code_callers"]
    );

    // The second carried the tool's real answer back up.
    let second: Value =
        serde_json::from_str(&stub.received.recv().await.expect("second")).expect("json");
    let result = second["messages"][2]["content"][0].clone();
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "call-1");
    let content = result["content"].as_str().expect("tool result content");
    assert!(content.contains("\"caller\":\"retry\""), "{content}");
    assert!(content.contains("src/net.rs"), "{content}");
}

#[tokio::test]
async fn brain_ask_refuses_a_repo_it_was_never_configured_with() {
    // `repo` reaches Config::mirror_path and from there `git --git-dir`. Unchecked,
    // this reads any repository on the box; it must not get as far as the model.
    let never_answered = serde_json::json!({
        "content": [{ "type": "text", "text": "this must never be reached" }],
        "stop_reason": "end_turn",
        "model": "claude-opus-5",
    });
    let mut stub = common::spawn_stub("HTTP/1.1 200 OK", never_answered.to_string()).await;
    let bed = asking_bed(&stub.url);

    for hostile in ["../../../../srv/git/private", "..", "other-repo", "demo/../evil"] {
        let (status, body) = post_json(
            &bed.router,
            "/brain/ask",
            serde_json::json!({ "question": "anything", "repo": hostile }),
        )
        .await;
        assert_eq!(status, 404, "{hostile} was accepted: {body}");
    }
    assert!(
        stub.received.try_recv().is_err(),
        "a rejected repo must not reach the upstream API at all"
    );
}

#[tokio::test]
async fn a_tool_call_naming_a_repo_out_of_scope_is_refused() {
    let asked = serde_json::json!({
        "content": [{
            "type": "tool_use",
            "id": "call-1",
            "name": "code_def",
            "input": { "repo": "somewhere-else", "symbol": "retry" },
        }],
        "stop_reason": "tool_use",
        "model": "claude-opus-5",
    });
    let answered = serde_json::json!({
        "content": [{ "type": "text", "text": "I cannot see that repository." }],
        "stop_reason": "end_turn",
        "model": "claude-opus-5",
    });
    let mut stub =
        common::spawn_stub_seq("HTTP/1.1 200 OK", vec![asked.to_string(), answered.to_string()])
            .await;
    let bed = asking_bed(&stub.url);
    bed.index("demo").await;

    let (status, _) = post_json(
        &bed.router,
        "/brain/ask",
        serde_json::json!({ "question": "anything", "repo": "demo" }),
    )
    .await;
    assert_eq!(status, 200);

    let _first = stub.received.recv().await.expect("first");
    let second: Value =
        serde_json::from_str(&stub.received.recv().await.expect("second")).expect("json");
    let content = second["messages"][2]["content"][0]["content"]
        .as_str()
        .expect("tool result");
    assert!(content.contains("no repository named somewhere-else"), "{content}");
}

// ---- the CI-queue trigger ----------------------------------------------------------

fn worker(bed: &common::TestBed) -> nashcode::ci::CiWorker {
    nashcode::ci::CiWorker {
        config: bed.config.clone(),
        db: bed.db.clone(),
        hooks: nashcode::hooks::Webhooks::new(Default::default()),
        timeout: std::time::Duration::from_secs(60),
        indexer: Some(bed.indexer.clone()),
        queue: Some(bed.app.ci.clone()),
    }
}

fn job() -> nashcode::ci::IndexJob {
    nashcode::ci::IndexJob { repo: "demo".to_owned() }
}

#[tokio::test]
async fn a_job_indexes_the_default_branch_tip_as_it_stands_when_the_job_runs() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;

    worker(&bed).run_index(&job()).await;
    let (_, body) = get(&bed.router, "/demo/code").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["indexed"], true, "{body}");
    assert!(answer["symbols"].as_i64().expect("symbols") > 0);
    assert_eq!(
        answer["commit"],
        bed.remote_tip("demo", "main"),
        "the default branch tip, not some other branch's: {body}"
    );
}

#[tokio::test]
async fn a_second_job_over_an_unmoved_tree_does_no_work_at_all() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    let worker = worker(&bed);

    worker.run_index(&job()).await;
    let (_, first) = get(&bed.router, "/demo/code").await;
    let first: Value = serde_json::from_str(&first).expect("json");

    // A push to a branch that is not the default one queues a job too. It must not
    // re-run the parse, and above all must not re-run the SCIP overlay, which clones
    // the repo and shells out to a language indexer.
    worker.run_index(&job()).await;
    let (_, second) = get(&bed.router, "/demo/code").await;
    let second: Value = serde_json::from_str(&second).expect("json");

    assert_eq!(second["commit"], first["commit"]);
    assert_eq!(second["chunks"], first["chunks"]);
    assert_eq!(
        second["last_indexed_at"], first["last_indexed_at"],
        "an unmoved tree records no new run: {second}"
    );
}

#[tokio::test]
async fn a_moved_tree_is_indexed_on_the_next_job() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    let worker = worker(&bed);
    worker.run_index(&job()).await;

    let work = common::Work::clone_from(&bed.remote_root().join("demo.git"));
    work.write("src/extra.rs", "pub fn added() { connect(); }\n");
    work.commit_all("add a file");
    work.push("main");
    bed.mirrors.refresh_now("demo").await;

    worker.run_index(&job()).await;
    let (_, body) = get(&bed.router, "/demo/code").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["commit"], bed.remote_tip("demo", "main"), "{body}");

    let (_, def) = get(&bed.router, "/demo/code/def?symbol=added").await;
    let def: Value = serde_json::from_str(&def).expect("json");
    assert_eq!(def["definitions"].as_array().expect("definitions").len(), 1, "{def}");
}

#[tokio::test]
async fn duplicate_index_jobs_for_one_repo_collapse_into_one() {
    let db = nashcode::db::Db::in_memory().expect("db");
    let (queue, mut rx) = nashcode::ci::CiQueue::new(db);

    // A push of five branches fires the tip observer five times.
    for _ in 0..5 {
        queue.enqueue_index("demo");
    }
    let mut queued = 0;
    while let Ok(task) = rx.try_recv() {
        assert!(matches!(task, nashcode::ci::Task::Index(_)));
        queued += 1;
    }
    assert_eq!(queued, 1, "five events, one run");

    // Once the run has started, the next event may queue again — work that lands
    // during a run must be able to ask for a run that will see it.
    queue.index_started("demo");
    queue.enqueue_index("demo");
    assert!(rx.try_recv().is_ok(), "a job after the run started is not swallowed");
}

#[tokio::test]
async fn coalescing_is_per_repo_not_global() {
    let db = nashcode::db::Db::in_memory().expect("db");
    let (queue, mut rx) = nashcode::ci::CiQueue::new(db);
    queue.enqueue_index("one");
    queue.enqueue_index("two");
    queue.enqueue_index("one");

    let mut repos = Vec::new();
    while let Ok(nashcode::ci::Task::Index(job)) = rx.try_recv() {
        repos.push(job.repo);
    }
    assert_eq!(repos, vec!["one", "two"]);
}

#[tokio::test]
async fn an_index_job_for_a_repo_with_no_mirror_is_a_log_line_not_a_panic() {
    let bed = simple_bed(code_fixture);
    // No refresh: nothing is on disk for this repo yet.
    worker(&bed).run_index(&job()).await;
    let (status, body) = get(&bed.router, "/demo/code").await;
    assert_eq!(status, 200, "{body}");
}
