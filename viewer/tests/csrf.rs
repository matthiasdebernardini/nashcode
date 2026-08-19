//! Cross-site request forgery, on every write path.
//!
//! The write paths take the actor from Tailscale headers alone, so a page open in the
//! same browser could otherwise auto-submit a form and commit as whoever is logged in.
//! `.nashcode/ci` is executable and runs with `GIT_TOKEN` in its environment, so this
//! is not a defacement bug — it is remote code execution.
//!
//! The guard is Topcoat's own `OriginPolicy`, on by default and never disabled by
//! `web::router`. These tests pin the three behaviours the viewer depends on, so the
//! day someone reaches for `dangerous_disable` the suite says no.

mod common;

use std::path::Path;

use common::{Work, get, make_remote, post_form, post_form_from, post_json, simple_bed};

fn ci_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);
    work.write("README.md", "# demo\n");
    work.write(".nashcode/ci", "#!/bin/sh\nexit 0\n");
    work.write("tasks/one.md", "---\nstatus: todo\n---\n\n# One\n");
    work.commit_all("initial");
    work.push("main");
    work
}

/// Every state-changing endpoint a browser can reach, with a body each one accepts.
const WRITE_PATHS: [(&str, &[(&str, &str)]); 2] = [
    ("/demo/edit", &[("path", "README.md"), ("content", "owned\n"), ("message", "x")]),
    ("/demo/comments", &[("branch", "main"), ("body", "owned")]),
];

#[tokio::test]
async fn a_cross_site_post_is_refused_before_the_handler_runs() {
    let bed = simple_bed(ci_fixture);
    let before = bed.remote_tip("demo", "main");

    for (path, fields) in WRITE_PATHS {
        for site in ["cross-site", "same-site"] {
            let (status, location, _) =
                post_form_from(&bed.router, path, fields, &[("sec-fetch-site", site)]).await;
            assert_eq!(status, 403, "{path} accepted a {site} POST");
            assert!(location.is_none(), "{path} redirected a {site} POST");
        }
    }

    // The one that matters most: the CI script a cross-site page would love to own.
    let (status, _, _) = post_form_from(
        &bed.router,
        "/demo/edit",
        &[("path", ".nashcode/ci"), ("content", "#!/bin/sh\necho pwned\n"), ("message", "x")],
        &[("sec-fetch-site", "cross-site")],
    )
    .await;
    assert_eq!(status, 403, "a cross-site page rewrote the CI script");
    assert_eq!(before, bed.remote_tip("demo", "main"), "a refused POST still pushed");
}

#[tokio::test]
async fn a_same_origin_post_from_the_viewers_own_pages_still_works() {
    let bed = simple_bed(ci_fixture);
    let (status, location, body) = post_form_from(
        &bed.router,
        "/demo/edit",
        &[("path", "README.md"), ("content", "# edited\n"), ("message", "from our own form")],
        &[("sec-fetch-site", "same-origin")],
    )
    .await;
    assert_eq!(status, 303, "the viewer's own form was refused:\n{body}");
    assert_eq!(location.as_deref(), Some("/demo/blob/README.md"));

    // A direct navigation (a bookmark, a typed URL) declares `none`, not `same-origin`.
    let (status, _, _) = post_form_from(
        &bed.router,
        "/demo/edit",
        &[("path", "README.md"), ("content", "# again\n"), ("message", "typed")],
        &[("sec-fetch-site", "none")],
    )
    .await;
    assert_eq!(status, 303, "a direct navigation was refused");
}

#[tokio::test]
async fn clients_that_send_no_fetch_metadata_keep_working() {
    let bed = simple_bed(ci_fixture);

    // The nashcode CLI, curl, and the trace hook send no `sec-fetch-site`; they carry
    // no ambient credentials either, so there is nothing for a forgery to borrow.
    let (status, location, body) = post_form(
        &bed.router,
        "/demo/edit",
        &[("path", "notes.md"), ("content", "# from a script\n"), ("message", "cli")],
    )
    .await;
    assert_eq!(status, 303, "a headerless POST was refused:\n{body}");
    assert_eq!(location.as_deref(), Some("/demo/blob/notes.md"));

    // The JSON API side, too: board moves and comments are how agents write.
    let (status, body) =
        post_json(&bed.router, "/demo/board/move", serde_json::json!({"file": "tasks/one.md", "status": "doing"})).await;
    assert_eq!(status, 200, "a headerless board move was refused:\n{body}");

    let (status, body) = post_json(
        &bed.router,
        "/demo/comments",
        serde_json::json!({"branch": "main", "body": "posted by an agent"}),
    )
    .await;
    assert_eq!(status, 201, "a headerless comment was refused:\n{body}");

    let (status, page) = get(&bed.router, "/demo/blob/notes.md").await;
    assert_eq!(status, 200);
    assert!(page.contains("from a script"), "the script's commit did not land:\n{page}");
}

#[tokio::test]
async fn reading_is_never_blocked_by_the_origin_check() {
    let bed = simple_bed(ci_fixture);
    for path in ["/demo", "/demo/docs", "/demo/blob/README.md", "/demo/board"] {
        let (status, body) =
            common::get_from(&bed.router, path, &[("sec-fetch-site", "cross-site")]).await;
        assert_eq!(status, 200, "a cross-site GET of {path} was refused:\n{body}");
    }
}
