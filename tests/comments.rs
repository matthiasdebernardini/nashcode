//! The comment API round trip: post, render inline, outdated after the branch moves,
//! a post anchored to a file that is in no diff, and the `?since=` cursor.

mod common;

use common::{Work, get, post_json, simple_bed, stacked_fixture};

#[tokio::test]
async fn comment_round_trip_renders_inline_then_goes_outdated() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));

    // Line-anchored comment on the file part-1 changes.
    let (status, body) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({
            "branch": "part-1",
            "file": "src/app.txt",
            "line": 2,
            "body": "why two?",
            "author": "ada@example.invalid",
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let stored: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(stored["author"], "ada@example.invalid");
    assert!(stored["id"].as_i64().is_some());
    assert!(!stored["commit"].as_str().unwrap_or_default().is_empty());

    // It renders inline: the branch page embeds it as a diff annotation.
    let (_, page) = get(&bed.router, "/demo/part-1").await;
    assert!(page.contains("why two?"), "comment not rendered inline");
    assert!(page.contains("\"lineNumber\":2"), "annotation payload missing: {page}");
    assert!(!page.contains("outdated"), "fresh comment must not be outdated");

    // The branch moves and rewrites that file: the comment falls out of line.
    let work = Work::clone_from(&bed.remote_root().join("demo.git"));
    common::git(&work.dir, &["checkout", "part-1"]);
    work.write("src/app.txt", "totally rewritten\n");
    work.commit_all("rewrite");
    work.push("part-1");
    bed.mirrors.refresh_now("demo").await;

    let (_, page) = get(&bed.router, "/demo/part-1").await;
    assert!(page.contains("outdated"), "moved anchor must show as outdated");
    assert!(page.contains("why two?"), "outdated comment still visible");
    assert!(!page.contains("\"lineNumber\":2"), "stale anchor must leave the diff");
}

#[tokio::test]
async fn a_comment_can_anchor_to_a_file_outside_any_diff() {
    let bed = simple_bed(|root| {
        let work = stacked_fixture(root, "demo");
        work.write("plans/api.md", "# API plan\n\nLine one.\nLine two.\nLine three.\n");
        work.commit_all("plan");
        work.push("main");
        work
    });

    let (status, body) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({
            "branch": "main",
            "file": "plans/api.md",
            "line": 3,
            "body": "tighten this paragraph",
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    // No Tailscale headers on the request: the author falls back to `local`.
    let stored: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(stored["author"], "local");

    // The plan page shows it inline.
    let (status, page) = get(&bed.router, "/demo/plans/api.md").await;
    assert_eq!(status, 200);
    assert!(page.contains("tighten this paragraph"), "plan comment missing");
    assert!(page.contains("line 3"), "line badge missing");

    // The branch page lists it under comments on other files.
    let (_, branch_page) = get(&bed.router, "/demo/main").await;
    assert!(branch_page.contains("tighten this paragraph"));
}

#[tokio::test]
async fn the_since_cursor_orders_by_created_at_and_never_repeats() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));

    let mut created: Vec<(i64, String)> = Vec::new();
    for text in ["first", "second", "third"] {
        let (status, body) = post_json(
            &bed.router,
            "/demo/comments",
            serde_json::json!({ "branch": "main", "body": text }),
        )
        .await;
        assert_eq!(status, 201);
        let stored: serde_json::Value = serde_json::from_str(&body).expect("json");
        created.push((
            stored["id"].as_i64().expect("id"),
            stored["created_at"].as_str().expect("created_at").to_owned(),
        ));
    }

    // Full read: ordered by created_at then id.
    let (status, body) = get(&bed.router, "/demo/comments?branch=main").await;
    assert_eq!(status, 200);
    let all: Vec<serde_json::Value> = serde_json::from_str(&body).expect("json list");
    let ids: Vec<i64> = all.iter().map(|c| c["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, created.iter().map(|(id, _)| *id).collect::<Vec<_>>());

    // Cursor after the first: exactly the later two, no repeats.
    let since = &created[0].1;
    let (status, body) =
        get(&bed.router, &format!("/demo/comments?branch=main&since={}", urlenc(since))).await;
    assert_eq!(status, 200, "{body}");
    let later: Vec<serde_json::Value> = serde_json::from_str(&body).expect("json list");
    let later_ids: Vec<i64> = later.iter().map(|c| c["id"].as_i64().unwrap()).collect();
    assert_eq!(later_ids, vec![created[1].0, created[2].0]);

    // Cursor at the last: empty.
    let since = &created[2].1;
    let (_, body) =
        get(&bed.router, &format!("/demo/comments?since={}", urlenc(since))).await;
    let none: Vec<serde_json::Value> = serde_json::from_str(&body).expect("json list");
    assert!(none.is_empty());

    // A garbage cursor is a client error, not a 500.
    let (status, _) = get(&bed.router, "/demo/comments?since=yesterday").await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn bad_comment_posts_are_client_errors() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    // Missing body.
    let (status, _) =
        post_json(&bed.router, "/demo/comments", serde_json::json!({ "branch": "main" })).await;
    assert_eq!(status, 400);
    // Unknown branch.
    let (status, _) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({ "branch": "ghost", "body": "hi" }),
    )
    .await;
    assert_eq!(status, 400);
    // A line without a file.
    let (status, _) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({ "branch": "main", "line": 3, "body": "hi" }),
    )
    .await;
    assert_eq!(status, 400);
    // Unknown repo.
    let (status, _) = post_json(
        &bed.router,
        "/ghost/comments",
        serde_json::json!({ "branch": "main", "body": "hi" }),
    )
    .await;
    assert_eq!(status, 404);
}

fn urlenc(value: &str) -> String {
    value.replace('+', "%2B").replace(':', "%3A")
}

#[tokio::test]
async fn only_the_author_can_delete_through_the_ui_route() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    // Posted without Tailscale headers: the author is `local`.
    let (_, body) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({ "branch": "main", "body": "mine" }),
    )
    .await;
    let stored: serde_json::Value = serde_json::from_str(&body).expect("json");
    let id = stored["id"].as_i64().expect("id");

    // A different author's comment survives the local user's delete.
    let (_, other) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({ "branch": "main", "body": "theirs", "author": "ada@example.invalid" }),
    )
    .await;
    let other: serde_json::Value = serde_json::from_str(&other).expect("json");
    let other_id = other["id"].as_i64().expect("id");

    let (status, _) =
        post_json(&bed.router, &format!("/demo/comments/{other_id}/delete"), serde_json::json!({}))
            .await;
    assert_eq!(status, 403, "someone else's comment must not delete");

    let (status, _) =
        post_json(&bed.router, &format!("/demo/comments/{id}/delete"), serde_json::json!({}))
            .await;
    assert_eq!(status, 303, "own comment deletes and redirects back");

    let (_, list) = get(&bed.router, "/demo/comments").await;
    let remaining: Vec<serde_json::Value> = serde_json::from_str(&list).expect("json");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["body"], "theirs");
}
