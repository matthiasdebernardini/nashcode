//! People: the operator pushes one file, and the viewer answers one question about
//! it — which project is this message about. It never hands the file back.

mod common;

use common::{TestBed, get, simple_bed, stacked_fixture};
use topcoat::router::{Body, Method, request::Request, to_bytes};

/// Two projects, three people, one person in both — enough for a winner and a tie.
fn people_file() -> serde_json::Value {
    serde_json::json!({
        "me": ["matthias@example.com"],
        "people": [
            { "id": "rob", "name": "Rob Castro",
              "phones": ["+15550001111"], "emails": ["rob@example.com"] },
            { "id": "joey", "name": "Joey Locker",
              "phones": ["+15550002222"], "emails": ["joey@example.com"] },
            { "id": "brad", "name": "Brad Thompson",
              "phones": [], "emails": ["brad@example.com"] }
        ],
        "projects": [
            { "id": "agstaff", "name": "agstaff", "folder": "~/Projects/agstaff",
              "repo": "demo", "people": ["rob", "joey"],
              "imsg": { "prompt": "file it", "enrich": true, "media_only": false },
              "email": { "account": "matthias@example.com", "query": null } },
            { "id": "acres", "name": "Pristine Acres", "folder": "~/Projects/acres",
              "people": ["brad", "rob"] }
        ]
    })
}

async fn bed() -> TestBed {
    let bed = simple_bed(|root: &std::path::Path| stacked_fixture(root, "demo"));
    bed.mirrors.refresh("demo").await;
    bed
}

/// PUT the file and unwrap the JSON answer.
async fn push(bed: &TestBed, file: serde_json::Value) -> (u16, serde_json::Value) {
    let (status, body) = common::request(
        &bed.router,
        Method::PUT,
        "/people",
        Some(("application/json", file.to_string())),
    )
    .await;
    let value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("json body: {e}\n{body}"));
    (status, value)
}

/// PUT with the headers Tailscale's proxy adds, which is how the viewer learns who is
/// asking.
async fn push_as(bed: &TestBed, login: &str, file: serde_json::Value) -> serde_json::Value {
    let request = Request::builder()
        .method(Method::PUT)
        .uri("/people")
        .header("content-type", "application/json")
        .header("tailscale-user-login", login)
        .body(Body::from(file.to_string()))
        .expect("request builds");
    let response = bed.router.handle(request).await;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.expect("body reads");
    let body = String::from_utf8_lossy(&bytes).into_owned();
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).expect("json body")
}

async fn route(bed: &TestBed, query: &str) -> (u16, serde_json::Value) {
    let (status, body) = get(&bed.router, &format!("/people/route?{query}")).await;
    let value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("json body: {e}\n{body}"));
    (status, value)
}

async fn brain_people(bed: &TestBed) -> serde_json::Value {
    let (status, body) = get(&bed.router, "/brain").await;
    assert_eq!(status, 200, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("brain is JSON");
    value["people"].clone()
}

#[tokio::test]
async fn nothing_is_known_until_something_is_pushed() {
    let bed = bed().await;

    let (status, answer) = route(&bed, "email=rob@example.com").await;
    assert_eq!(status, 404, "{answer}");
    assert_eq!(answer["error"], "no people file");
    assert!(brain_people(&bed).await.is_null(), "and the brain says so too");
}

#[tokio::test]
async fn a_pushed_file_answers_the_ranking_and_the_tie() {
    let bed = bed().await;

    let (status, stored) = push(&bed, people_file()).await;
    assert_eq!(status, 200, "{stored}");
    assert_eq!(stored["ok"], true);
    assert_eq!(stored["people"], 3);
    assert_eq!(stored["projects"], 2);
    assert!(stored["pushed_at"].as_str().is_some_and(|at| at.starts_with("20")), "{stored}");
    // No Tailscale headers on a loopback request, so the push is `local`.
    assert_eq!(stored["pushed_by"], "local");

    // Two of agstaff's people, one of the other project's: a winner, no tie.
    let (status, answer) =
        route(&bed, "email=rob@example.com&email=joey@example.com").await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["tie"], false);
    let matches = answer["matches"].as_array().expect("matches");
    assert_eq!(matches.len(), 2, "{answer}");
    assert_eq!(matches[0]["project"], "agstaff");
    assert_eq!(matches[0]["repo"], "demo");
    assert_eq!(matches[0]["folder"], "~/Projects/agstaff");
    assert_eq!(matches[0]["score"], 2);
    assert_eq!(matches[0]["people"], serde_json::json!(["rob", "joey"]));
    // The addresses the caller asked with come back, so the extension can write
    // "agstaff — Rob Castro is on the invite" without knowing any person id.
    assert_eq!(
        matches[0]["contacts"],
        serde_json::json!([{ "email": "rob@example.com" }, { "email": "joey@example.com" }]),
        "a contact carries only the key it has"
    );
    assert_eq!(matches[1]["project"], "acres");
    assert_eq!(matches[1]["score"], 1);
    assert!(matches[1]["repo"].is_null(), "a project with no repo says so");

    // Rob alone is in both projects: one point each, and nothing here decides.
    let (status, answer) = route(&bed, "phone=%2B15550001111").await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["tie"], true, "{answer}");
    assert_eq!(answer["matches"][0]["project"], "agstaff", "file order, still");
    assert_eq!(answer["matches"][1]["project"], "acres");
    assert_eq!(
        answer["matches"][0]["contacts"],
        serde_json::json!([{ "phone": "+15550001111" }])
    );

    // The operator is on every thread there is, so the operator matches nothing.
    let (status, answer) = route(&bed, "email=matthias@example.com").await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["matches"].as_array().expect("matches").len(), 0);
    assert_eq!(answer["tie"], false);
}

#[tokio::test]
async fn a_project_naming_nobody_is_refused_at_the_door() {
    let bed = bed().await;

    let (status, answer) = push(
        &bed,
        serde_json::json!({
            "people": [{ "id": "rob", "name": "Rob Castro", "emails": ["rob@example.com"] }],
            "projects": [{ "id": "agstaff", "people": ["rob", "joey"] }]
        }),
    )
    .await;
    assert_eq!(status, 400, "{answer}");
    let why = answer["error"].as_str().expect("a reason");
    assert!(why.contains("\"joey\""), "{why}");
    assert!(why.contains("no person has"), "{why}");

    // A refused push stores nothing, so the route is still unanswerable.
    let (status, _) = route(&bed, "email=rob@example.com").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn asking_about_nobody_is_a_usage_error() {
    let bed = bed().await;
    push(&bed, people_file()).await;

    for query in ["", "email=", "phone=%20", "name=rob"] {
        let (status, answer) = route(&bed, query).await;
        assert_eq!(status, 400, "{query:?} answered {answer}");
        assert!(answer["error"].as_str().is_some_and(|e| e.contains("somebody")), "{answer}");
    }
}

#[tokio::test]
async fn there_is_no_way_to_read_the_file_back() {
    let bed = bed().await;
    push(&bed, people_file()).await;

    // Phones and emails stay on the operator's machine. `people` is a reserved word,
    // so the repo page cannot claim the path either, and the only method the path
    // knows is PUT.
    let (status, body) = get(&bed.router, "/people").await;
    assert_eq!(status, 405, "GET /people answered {status}: {body}");
    assert!(!body.contains("rob@example.com"), "{body}");
    assert!(!body.contains("+15550001111"), "{body}");
}

#[tokio::test]
async fn the_brain_counts_the_copy_it_holds() {
    let bed = bed().await;
    assert!(brain_people(&bed).await.is_null());

    let (_, stored) = push(&bed, people_file()).await;
    let people = brain_people(&bed).await;
    assert_eq!(people["projects"], 2);
    assert_eq!(people["people"], 3);
    assert_eq!(people["pushed_at"], stored["pushed_at"]);
    assert_eq!(people["pushed_by"], stored["pushed_by"], "and whose copy it is");

    // A second push with less in it is visible at once: nothing caches this stanza.
    push(&bed, serde_json::json!({ "people": [], "projects": [] })).await;
    let people = brain_people(&bed).await;
    assert_eq!(people["projects"], 0);
    assert_eq!(people["people"], 0);
}

#[tokio::test]
async fn the_copy_says_who_pushed_it() {
    let bed = bed().await;

    // The viewer answers for everybody on the tailnet, so a copy that decides where
    // work is filed should name the person whose copy it is.
    let stored = push_as(&bed, "matthias@example.com", people_file()).await;
    assert_eq!(stored["pushed_by"], "matthias@example.com");
    let people = brain_people(&bed).await;
    assert_eq!(people["pushed_by"], "matthias@example.com");

    // A second push replaces the first, name and all.
    let stored = push_as(&bed, "rob@example.com", people_file()).await;
    assert_eq!(stored["pushed_by"], "rob@example.com");
    assert_eq!(brain_people(&bed).await["pushed_by"], "rob@example.com");
}
