//! The comment API round trip: post, render inline, outdated after the branch moves,
//! a post anchored to a file that is in no diff, the `?since=` cursor, and the markup
//! the click-a-line composer is cloned from.

mod common;

use common::{Work, get, post_form_from, post_json, post_json_from, simple_bed, stacked_fixture};

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
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let stored: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(stored["author"], "local");
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
async fn no_composer_asks_for_a_line_number_to_be_typed() {
    let bed = simple_bed(|root| {
        let work = stacked_fixture(root, "demo");
        work.write("plans/api.md", "# API plan\n\nLine one.\n");
        work.commit_all("plan");
        work.push("main");
        work
    });

    for path in ["/demo/part-1", "/demo/plans/api.md"] {
        let (status, page) = get(&bed.router, path).await;
        assert_eq!(status, 200);
        assert!(page.contains("nashcode-composer"), "{path} lost its composer");
        assert!(!page.contains("placeholder=\"line #\""), "{path} still asks for a typed line");
        assert!(
            !page.contains("type=\"number\" name=\"line\""),
            "{path} still has the numeric line input"
        );
    }
}

#[tokio::test]
async fn the_diff_carries_the_template_a_line_click_clones() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    let (status, page) = get(&bed.router, "/demo/part-1").await;
    assert_eq!(status, 200);

    // The template the browser clones: inert markup, the real action, and the hidden
    // fields a line click fills in.
    let start = page.find("nashcode-inline-composer-template").expect("template missing");
    let end = page[start..].find("</template>").expect("template unclosed") + start;
    let template = &page[start..end];
    assert!(template.contains("action=\"/demo/comments\""), "{template}");
    assert!(template.contains("name=\"branch\" value=\"part-1\""), "{template}");
    assert!(template.contains("name=\"file\" value=\"src/app.txt\""), "{template}");
    assert!(template.contains("type=\"hidden\" name=\"line\""), "{template}");
    assert!(template.contains("nashcode-inline-composer-cancel"), "{template}");
    assert!(template.contains("textarea name=\"body\""), "{template}");
}

#[tokio::test]
async fn the_inline_composers_form_post_anchors_the_comment_to_the_line() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));

    // Exactly what the cloned template submits: hidden branch, file, and line.
    let (status, location, body) = post_form_from(
        &bed.router,
        "/demo/comments",
        &[("branch", "part-1"), ("file", "src/app.txt"), ("line", "2"), ("body", "anchored here")],
        &[("sec-fetch-site", "same-origin")],
    )
    .await;
    assert_eq!(status, 303, "the composer's own form was refused:\n{body}");
    assert!(location.is_some(), "a form post redirects back to the page");

    // And it comes back as a line annotation on the diff, in place.
    let (_, page) = get(&bed.router, "/demo/part-1").await;
    assert!(page.contains("anchored here"), "comment not rendered inline");
    assert!(page.contains("\"lineNumber\":2"), "annotation payload missing: {page}");
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
async fn hostile_comment_html_renders_escaped_everywhere() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    let payload = "<img src=x onerror=alert(1)> and <script>alert(2)</script>";

    // Line-anchored on the diffed file: reaches the annotation payload the browser
    // injects via innerHTML.
    let (status, _) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({
            "branch": "part-1",
            "file": "src/app.txt",
            "line": 2,
            "body": payload,
        }),
    )
    .await;
    assert_eq!(status, 201);
    // Branch-level too: reaches the comment blocks in the page body.
    let (status, _) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({ "branch": "part-1", "body": payload }),
    )
    .await;
    assert_eq!(status, 201);

    let (_, page) = get(&bed.router, "/demo/part-1").await;
    assert!(!page.contains("<img src=x"), "raw img tag leaked: {page}");
    assert!(!page.contains("<script>alert"), "raw script tag leaked");
    // The text itself is still readable, escaped.
    assert!(
        page.contains("&lt;img src=x onerror=alert(1)&gt;")
            || page.contains("\\u003cimg src=x onerror=alert(1)\\u003e")
            || page.contains("\\u003cimg"),
        "escaped form missing: {page}"
    );
}

#[tokio::test]
async fn javascript_links_in_comments_render_dead() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));
    for body in [
        "[click](javascript:alert(1))",
        "[click](JaVaScRiPt:alert(1))",
        "[click](data:text/html,<script>alert(1)</script>)",
    ] {
        let (status, _) = post_json(
            &bed.router,
            "/demo/comments",
            serde_json::json!({ "branch": "part-1", "body": body }),
        )
        .await;
        assert_eq!(status, 201);
    }

    let (_, page) = get(&bed.router, "/demo/part-1").await;
    let lower = page.to_ascii_lowercase();
    assert!(!lower.contains("javascript:"), "live javascript: href leaked: {page}");
    assert!(!lower.contains("href=\"data:"), "live data: href leaked");
    assert!(page.contains("click"), "the link text is still readable");
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
    let (_, other) = post_json_from(
        &bed.router,
        "/demo/comments",
        serde_json::json!({ "branch": "main", "body": "theirs" }),
        &[("tailscale-user-login", "ada@example.invalid")],
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

#[tokio::test]
async fn the_comment_author_is_the_actor_not_the_payload() {
    let bed = simple_bed(|root| stacked_fixture(root, "demo"));

    // Alice posts and claims to be mallory. The claim is dropped.
    let (status, body) = post_json_from(
        &bed.router,
        "/demo/comments",
        serde_json::json!({ "branch": "main", "body": "first", "author": "mallory" }),
        &[("tailscale-user-login", "alice@example.invalid")],
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let stored: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(stored["author"], "alice@example.invalid", "client author must be ignored");
    assert!(stored["on_behalf_of"].is_null());

    // Alice's agent posts for bob: both identities are kept, alice stays the actor.
    let (status, body) = post_json_from(
        &bed.router,
        "/demo/comments",
        serde_json::json!({
            "branch": "main",
            "body": "relayed",
            "author": "mallory",
            "on_behalf_of": "bob@example.invalid",
        }),
        &[("tailscale-user-login", "alice@example.invalid")],
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let relayed: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(relayed["author"], "alice@example.invalid");
    assert_eq!(relayed["on_behalf_of"], "bob@example.invalid");

    // The list carries both fields, never "mallory".
    let (_, list) = get(&bed.router, "/demo/comments").await;
    assert!(!list.contains("mallory"), "an impersonated author reached storage: {list}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&list).expect("json");
    assert_eq!(rows.len(), 2);

    // The page renders the byline as "principal via actor".
    let (_, page) = get(&bed.router, "/demo/main").await;
    assert!(
        page.contains("bob@example.invalid via alice@example.invalid"),
        "byline missing from the page: {page}"
    );
}
