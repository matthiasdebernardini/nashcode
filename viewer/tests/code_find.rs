//! `GET /:repo/code/find` — the fused query, against real repos built with real git.
//!
//! The endpoint's whole promise is routing: one question, four layers, one fixed
//! ranking, and a header saying which commit the answer describes. These tests hold
//! that promise to the ranking, to the caps, and to every degraded path — no index, no
//! model, no match, no such repo — because a search an agent runs on reflex must never
//! fail the session.
//!
//! Nothing here downloads a model or touches a network: the testbed's embedder is the
//! deterministic word-bag in `common::WordBag`.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{Work, get, make_remote, simple_bed};
use nashcode::code::{EmbedError, Embedder, Embeddings, MAX_FIND_HITS};
use serde_json::Value;

/// A small repo with real code in three languages plus prose.
///
/// Tuned so the text pass lands on both sides of the thin threshold: `retry` has three
/// text hits and buys no embeddings pass, `render` has one and does.
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
        "docs/notes.md",
        "# notes\n\nretry is documented here.\nretry again, for the third hit.\n",
    );
    work.commit_all("initial");
    work.push("main");
    work
}

/// A repo wide enough that one name's definitions alone can spend the row budget.
fn wide_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);
    for file in 0..105 {
        work.write(
            &format!("src/f{file}.rs"),
            &format!("pub fn needle() {{}}\n\npub fn caller{file}() {{\n    needle();\n}}\n"),
        );
    }
    work.commit_all("initial");
    work.push("main");
    work
}

/// The hits of one layer, in the order the answer gave them.
fn layer<'a>(answer: &'a Value, name: &str) -> Vec<&'a Value> {
    answer["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .filter(|hit| hit["layer"] == name)
        .collect()
}

/// Every layer label, in answer order.
fn order(answer: &Value) -> Vec<&str> {
    answer["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit["layer"].as_str().expect("layer"))
        .collect()
}

async fn find(bed: &common::TestBed, query: &str) -> Value {
    let (status, body) = get(&bed.router, &format!("/demo/code/find?q={query}")).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).expect("json")
}

#[tokio::test]
async fn an_exact_symbol_answers_definitions_first_with_reference_and_caller_counts() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let answer = find(&bed, "retry").await;
    let definitions = layer(&answer, "definition");
    assert_eq!(definitions.len(), 1, "{answer}");
    let definition = definitions[0];
    assert_eq!(definition["path"], "src/net.rs");
    assert_eq!(definition["name"], "retry");
    assert_eq!(definition["kind"], "function");
    assert_eq!(definition["line"], 8);
    // The snippet is the real source line, not a rebuild of it.
    assert_eq!(definition["text"], "pub fn retry(attempts: u32) {");
    // The counts ride on the definition, so one call answers "who uses this".
    // Nothing calls `retry` in this fixture, and zero is a fact worth carrying: a
    // reader must be able to tell "nobody calls this" from "the counts are missing".
    assert_eq!(definition["references"], 0, "{definition}");
    assert_eq!(definition["callers"], 0, "{definition}");

    // Ranking is fixed, and the definition leads it.
    assert_eq!(order(&answer)[0], "definition");
    let seen: Vec<&str> = order(&answer);
    let rank = |layer: &str| ["definition", "reference", "text", "semantic"]
        .iter()
        .position(|l| *l == layer)
        .expect("a known layer");
    assert!(
        seen.windows(2).all(|pair| rank(pair[0]) <= rank(pair[1])),
        "layers came back out of rank order: {seen:?}"
    );
}

#[tokio::test]
async fn a_symbol_the_graph_calls_carries_its_references_after_the_definitions() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let answer = find(&bed, "backoff").await;
    let references = layer(&answer, "reference");
    assert!(!references.is_empty(), "backoff is called from retry: {answer}");
    // A reference says where it is and, when the graph knows, what encloses it.
    assert_eq!(references[0]["path"], "src/net.rs");
    assert!(references[0]["line"].as_i64().expect("line") > 0);
    assert!(references[0].get("references").is_none(), "counts ride on definitions only");

    // And the definition of a symbol something calls carries both counts.
    let definition = layer(&answer, "definition")[0];
    assert_eq!(definition["name"], "backoff");
    assert_eq!(definition["references"], references.len(), "{answer}");
    assert_eq!(definition["callers"], 1, "retry is the one caller: {answer}");
}

#[tokio::test]
async fn text_hits_come_back_for_a_phrase_no_symbol_could_match() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    // A space means this can never be a symbol, so the graph is not even asked.
    let answer = find(&bed, "is%20documented").await;
    assert!(layer(&answer, "definition").is_empty(), "{answer}");
    assert!(layer(&answer, "reference").is_empty(), "{answer}");
    let text = layer(&answer, "text");
    assert_eq!(text.len(), 1, "{answer}");
    assert_eq!(text[0]["path"], "docs/notes.md");
    assert_eq!(text[0]["text"], "retry is documented here.");
    assert_eq!(answer["counts"]["text"], 1);
}

#[tokio::test]
async fn the_semantic_layer_appears_only_when_the_text_pass_is_thin() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    // Three text hits is not thin: the reader has enough exact evidence.
    let fat = find(&bed, "retry").await;
    assert_eq!(fat["counts"]["text"], 3, "{fat}");
    assert!(layer(&fat, "semantic").is_empty(), "{fat}");

    // One is thin, so the embeddings pass runs and says so on every row.
    let thin = find(&bed, "render").await;
    assert_eq!(thin["counts"]["text"], 1, "{thin}");
    let semantic = layer(&thin, "semantic");
    assert!(!semantic.is_empty(), "{thin}");
    assert!(semantic[0]["score"].as_f64().expect("score") > 0.0);
    assert_eq!(thin["semantic_available"], true);
    // Semantic ranks last, whatever it scored.
    assert_eq!(*order(&thin).last().expect("hits"), "semantic");
}

#[tokio::test]
async fn no_model_means_no_semantic_layer_rather_than_an_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    code_fixture(&remotes);
    let mut bed = common::testbed_with(root, &["demo"], std::collections::BTreeMap::new());
    // Index with a working embedder, then take the model away, which is what a
    // restarted viewer that has not loaded one looks like.
    bed.index("demo").await;
    bed.app.embeddings = Embeddings::new();
    bed.router = nashcode::web::router(bed.app.clone());

    let answer = find(&bed, "render").await;
    assert_eq!(answer["semantic_available"], false, "{answer}");
    assert!(layer(&answer, "semantic").is_empty(), "{answer}");
    // Thin text is still text: the exact layer is unaffected by a missing model.
    assert_eq!(answer["counts"]["text"], 1, "{answer}");
    assert!(answer["semantic_note"].as_str().is_some(), "{answer}");
}

#[tokio::test]
async fn a_query_the_embedder_cannot_read_still_answers_its_exact_layers() {
    /// An embedder that fails every call, the way a half-loaded model does.
    #[derive(Debug)]
    struct Broken;
    impl Embedder for Broken {
        fn model(&self) -> &str {
            "broken"
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Err(EmbedError::NotBuilt)
        }
    }

    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    code_fixture(&remotes);
    let mut bed = common::testbed_with(root, &["demo"], std::collections::BTreeMap::new());
    bed.index("demo").await;
    bed.app.embeddings = Embeddings::with(Arc::new(Broken));
    bed.router = nashcode::web::router(bed.app.clone());

    let answer = find(&bed, "render").await;
    assert_eq!(answer["counts"]["text"], 1, "{answer}");
    assert!(layer(&answer, "semantic").is_empty(), "{answer}");
}

#[tokio::test]
async fn the_answer_says_which_commit_it_describes_and_how_old_that_is() {
    let bed = simple_bed(code_fixture);
    let run = bed.index("demo").await;

    let answer = find(&bed, "retry").await;
    assert_eq!(answer["indexed"], true);
    assert_eq!(answer["commit"], run.commit);
    assert_eq!(answer["rev"], run.commit, "every layer reads one tree");
    assert!(answer["age_seconds"].as_i64().expect("an age") >= 0);
    assert!(answer["indexed_at"].as_str().is_some(), "{answer}");
    assert_eq!(answer["repo"], "demo");
    assert_eq!(answer["query"], "retry");
}

#[tokio::test]
async fn a_repo_that_was_never_indexed_answers_empty_with_the_index_hint() {
    let bed = simple_bed(code_fixture);
    bed.mirrors.refresh_now("demo").await;
    // Deliberately never indexed.

    let (status, body) = get(&bed.router, "/demo/code/find?q=retry").await;
    assert_eq!(status, 200, "never an error: {body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["indexed"], false);
    assert!(answer["hits"].as_array().expect("hits").is_empty(), "{answer}");
    let hint = answer["hint"].as_str().expect("a hint");
    assert!(hint.contains("/demo/code/index"), "{hint}");
    assert!(answer.get("commit").is_none(), "{answer}");
}

#[tokio::test]
async fn a_query_that_matches_nothing_is_an_empty_list_with_a_different_hint() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let answer = find(&bed, "nothing_here_at_all").await;
    assert_eq!(answer["indexed"], true);
    assert!(answer["hits"].as_array().expect("hits").is_empty(), "{answer}");
    let hint = answer["hint"].as_str().expect("a hint");
    assert!(hint.contains("nothing matches"), "{hint}");
    // "Never indexed" and "indexed, not there" are different facts.
    assert!(!hint.contains("never been indexed"), "{hint}");
}

#[tokio::test]
async fn the_caps_hold_per_layer_and_across_the_whole_answer() {
    // Enough definitions of one name to spend the whole budget before the text pass.
    let bed = simple_bed(wide_fixture);
    bed.index("demo").await;

    // `limit` is the per-layer cap, clamped to MAX_LIMIT.
    let (status, body) = get(&bed.router, "/demo/code/find?q=needle&limit=1000").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["counts"]["definition"], 100, "{}", answer["counts"]);
    assert_eq!(answer["counts"]["reference"], 100, "{}", answer["counts"]);
    // The budget is spent in ranking order, so the cheap layers lose, not the graph.
    assert_eq!(answer["counts"]["text"], 0, "{}", answer["counts"]);
    assert_eq!(answer["counts"]["semantic"], 0, "{}", answer["counts"]);
    assert_eq!(answer["hits"].as_array().expect("hits").len(), MAX_FIND_HITS);
    assert_eq!(answer["truncated"], true);

    // And a small limit is honoured exactly.
    let (_, body) = get(&bed.router, "/demo/code/find?q=needle&limit=2").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["counts"]["definition"], 2);
    assert_eq!(answer["counts"]["reference"], 2);
    assert_eq!(answer["counts"]["text"], 2);
    assert_eq!(answer["truncated"], true);
}

#[tokio::test]
async fn case_insensitive_search_folds_every_layer_not_just_the_text() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    // Exactly as typed, `RETRY` matches nothing exactly: no definition, no reference,
    // no text. (The embeddings still guess, which is what they are for.)
    let exact = find(&bed, "RETRY").await;
    assert_eq!(exact["counts"]["definition"], 0, "{exact}");
    assert_eq!(exact["counts"]["reference"], 0, "{exact}");
    assert_eq!(exact["counts"]["text"], 0, "{exact}");

    let folded = find(&bed, "RETRY&case=insensitive").await;
    let definitions = layer(&folded, "definition");
    assert_eq!(definitions.len(), 1, "the graph folds too: {folded}");
    assert_eq!(definitions[0]["name"], "retry");
    // Including the snippet, which is read back out of git with the same fold.
    assert_eq!(definitions[0]["text"], "pub fn retry(attempts: u32) {");
    assert_eq!(folded["counts"]["text"], 3, "{folded}");

    // And a call site folds, so `-i` finds callers too.
    let folded = find(&bed, "BACKOFF&case=insensitive").await;
    assert!(!layer(&folded, "reference").is_empty(), "{folded}");
    assert_eq!(layer(&folded, "definition")[0]["callers"], 1, "{folded}");
}

#[tokio::test]
async fn the_path_filters_run_before_the_row_budget_not_after() {
    let bed = simple_bed(wide_fixture);
    bed.index("demo").await;

    // Without server-side filtering the budget is spent on a hundred definitions
    // from files the caller excluded, and this comes back with nothing in it.
    let (status, body) =
        get(&bed.router, "/demo/code/find?q=needle&limit=1000&globs=src/f7.rs").await;
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["counts"]["definition"], 1, "{}", answer["counts"]);
    assert_eq!(layer(&answer, "definition")[0]["path"], "src/f7.rs");
    assert!(answer["counts"]["text"].as_i64().expect("text") > 0, "{}", answer["counts"]);
    for hit in answer["hits"].as_array().expect("hits") {
        assert_eq!(hit["path"], "src/f7.rs", "{hit}");
    }

    // `types=` and `paths=` narrow the same way.
    let (_, body) = get(&bed.router, "/demo/code/find?q=needle&types=py").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(answer["hits"].as_array().expect("hits").is_empty(), "no python here: {answer}");

    let (_, body) = get(&bed.router, "/demo/code/find?q=needle&paths=src/f7.rs").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["counts"]["definition"], 1, "{answer}");
}

#[tokio::test]
async fn the_embedder_is_never_asked_for_a_query_the_text_pass_already_answered() {
    /// An embedder that counts how often it was asked.
    #[derive(Debug)]
    struct Counting(Arc<std::sync::atomic::AtomicUsize>);
    impl Embedder for Counting {
        fn model(&self) -> &str {
            "counting"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    code_fixture(&remotes);
    let mut bed = common::testbed_with(root, &["demo"], std::collections::BTreeMap::new());
    bed.index("demo").await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    bed.app.embeddings = Embeddings::with(Arc::new(Counting(calls.clone())));
    bed.router = nashcode::web::router(bed.app.clone());
    let asked = || calls.load(std::sync::atomic::Ordering::SeqCst);

    // Three text hits: the model is not loaded, not run, and not scanned against.
    find(&bed, "retry").await;
    assert_eq!(asked(), 0, "a query the text pass answered must not pay for embeddings");

    // `limit` says how many rows to *show*. It must not decide whether the answer
    // was thin, or `?limit=1` would buy an embeddings pass on every search.
    let (_, body) = get(&bed.router, "/demo/code/find?q=retry&limit=1").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["counts"]["text"], 1, "{answer}");
    assert_eq!(asked(), 0, "a small limit is not a thin answer");

    // One text hit is thin, and that is what the pass is for.
    find(&bed, "render").await;
    assert_eq!(asked(), 1);
}

#[tokio::test]
async fn a_spent_budget_does_not_buy_an_embeddings_pass_nobody_can_see() {
    #[derive(Debug)]
    struct Counting(Arc<std::sync::atomic::AtomicUsize>);
    impl Embedder for Counting {
        fn model(&self) -> &str {
            "counting"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    wide_fixture(&remotes);
    let mut bed = common::testbed_with(root, &["demo"], std::collections::BTreeMap::new());
    bed.index("demo").await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    bed.app.embeddings = Embeddings::with(Arc::new(Counting(calls.clone())));
    bed.router = nashcode::web::router(bed.app.clone());

    // The graph layers spend the whole budget, so the text pass shows nothing — but
    // it *found* plenty, and a semantic pass whose rows could not be printed anyway
    // is a model call and a full cosine scan for nothing.
    let (_, body) = get(&bed.router, "/demo/code/find?q=needle&limit=1000").await;
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["counts"]["text"], 0, "{}", answer["counts"]);
    assert_eq!(answer["counts"]["semantic"], 0, "{}", answer["counts"]);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_definition_whose_line_cannot_be_read_keeps_its_name_as_the_snippet() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;
    // The symbol table lives in SQLite; the source line lives in git. Take git away
    // and the definitions still answer — with the name where the line would be,
    // because an empty content field would break `path:line:content` for every reader.
    std::fs::remove_dir_all(&bed.config.mirrors).expect("the mirror goes away");

    let answer = find(&bed, "retry").await;
    let definitions = layer(&answer, "definition");
    assert_eq!(definitions.len(), 1, "{answer}");
    assert_eq!(definitions[0]["text"], "retry", "the name, never an empty line");
    assert_eq!(definitions[0]["line"], 8, "the line number is still true");
    assert_eq!(answer["counts"]["text"], 0, "and the text pass simply has nothing");
}

/// Print the answer `cli/tests/fixtures/code_find.json` is captured from.
///
/// Ignored, because it asserts nothing: it exists so the CLI's fixture has a runnable
/// recipe rather than a story about how it was once made. Regenerate with
///
/// ```text
/// cargo nextest run -p nashcode --test code_find --run-ignored only --no-capture \
///   -E 'test(print_the_cli_fixture)'
/// ```
///
/// then pin `indexed_at` and `age_seconds` so the humanised age stays deterministic.
#[tokio::test]
#[ignore = "prints the CLI's fixture; asserts nothing"]
async fn print_the_cli_fixture() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;
    let (_, body) = get(&bed.router, "/demo/code/find?q=backoff").await;
    println!("{body}");
}

#[tokio::test]
async fn a_repo_the_viewer_does_not_know_is_a_404_and_a_missing_query_is_a_400() {
    let bed = simple_bed(code_fixture);
    bed.index("demo").await;

    let (status, _) = get(&bed.router, "/nosuchrepo/code/find?q=retry").await;
    assert_eq!(status, 404);

    let (status, _) = get(&bed.router, "/demo/code/find").await;
    assert_eq!(status, 400);
    let (status, _) = get(&bed.router, "/demo/code/find?q=%20").await;
    assert_eq!(status, 400);
}
