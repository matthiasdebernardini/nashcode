//! Browsing the upstream column: the `/{repo}/stack` page, each dep's tree and blobs,
//! and the gitlinks that link through to a mirrored dep.
//!
//! The upstreams are the real bare repos `common::Origin` publishes over git's dumb
//! HTTP protocol, so a mirror on disk is a mirror that was really fetched. The server's
//! request counter carries a second claim through this file: browsing never goes to the
//! wire, not even for a revision the mirror does not have.

mod common;

use common::{Origin, TestBed, bed_declaring, get, git, make_remote, post_json, testbed_with};

/// `POST /{repo}/stack/sync`, which is how a test gets the mirrors on disk.
async fn sync(bed: &TestBed, repo: &str) {
    let (status, body) =
        post_json(&bed.router, &format!("/{repo}/stack/sync"), serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
}

/// A page that must render, with its body for the assertion that failed to read.
async fn page(bed: &TestBed, path: &str) -> String {
    let (status, body) = get(&bed.router, path).await;
    assert_eq!(status, 200, "{path} did not render:\n{body}");
    body
}

async fn status_of(bed: &TestBed, path: &str) -> u16 {
    get(&bed.router, path).await.0
}

// ---- the column ------------------------------------------------------------------

#[tokio::test]
async fn the_stack_page_shows_the_repo_and_every_dep_at_its_commit() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    origin.repo("celld");
    let pin = origin.tip("dgit", "main");
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            r#"
[[dep]]
name  = "dgit"
url   = "{}"
pin   = "{pin}"
layer = "server"

[[dep]]
name  = "celld"
url   = "{}"
track = "main"
layer = "runtime"
"#,
            origin.url("dgit"),
            origin.url("celld"),
        ),
    )]);
    sync(&bed, "demo").await;

    let body = page(&bed, "/demo/stack").await;
    // The repo itself, then each dep: name, layer, mode, url and the commit on disk.
    assert!(body.contains(">demo<"), "the repo is not on its own column page:\n{body}");
    for wanted in [
        "dgit",
        "celld",
        "server",
        "runtime",
        "pin",
        "track",
        &pin[..8],
        &origin.tip("celld", "main")[..8],
        &origin.url("dgit"),
    ] {
        assert!(body.contains(wanted), "the column never mentions {wanted:?}:\n{body}");
    }
    // Each entry opens that dep's tree.
    assert!(body.contains("href=\"/demo/stack/dgit\""), "{body}");
    assert!(body.contains("href=\"/demo/stack/celld\""), "{body}");

    // The two words that share a name each point at the other, on both pages.
    assert!(body.contains("href=\"/demo/stacks\""), "the stack page never names Stacks:\n{body}");
    let stacks = page(&bed, "/demo/stacks").await;
    assert!(stacks.contains("href=\"/demo/stack\""), "the stacks page never names Stack:\n{stacks}");
    // And the tab nav carries both, with this page's own tab marked.
    assert!(body.contains(" Stack</a>") && body.contains(" Stacks</a>"), "{body}");
    assert!(body.contains("href=\"/demo/stack\" aria-current=\"page\""), "{body}");
}

#[tokio::test]
async fn a_dep_with_no_mirror_says_why_instead_of_linking_anywhere() {
    // Port 1 on loopback: nothing listens, and nothing ever will.
    let bed = bed_declaring(&[(
        "demo",
        "[[dep]]\nname = \"gone\"\nurl = \"http://127.0.0.1:1/gone.git\"\ntrack = \"main\"\n\n\
         [[dep]]\nname = \"refused\"\nurl = \"ssh://git@example.invalid/a\"\npin = \"1a2b3c4\"\n",
    )]);
    sync(&bed, "demo").await;

    let body = page(&bed, "/demo/stack").await;
    assert!(!body.contains("href=\"/demo/stack/gone\""), "a dep with no mirror was a link:\n{body}");
    assert!(!body.contains("href=\"/demo/stack/refused\""), "{body}");
    assert!(body.contains("only http and https"), "the refused dep never says why:\n{body}");
    assert!(body.contains("color-border-danger-emphasis"), "no card said anything was wrong:\n{body}");

    // The dep is declared, so its page is not a 404 — it says the commit is not there.
    let waiting = page(&bed, "/demo/stack/gone").await;
    assert!(waiting.contains("not mirrored yet"), "{waiting}");
}

#[tokio::test]
async fn a_repo_that_declares_nothing_still_has_a_stack_page() {
    let bed = bed_declaring(&[("demo", "")]);
    let body = page(&bed, "/demo/stack").await;
    assert!(body.contains("declares no"), "{body}");
    assert_eq!(status_of(&bed, "/nope/stack").await, 404);
}

// ---- two clicks: column, tree, file ----------------------------------------------

#[tokio::test]
async fn two_clicks_reach_a_file_in_a_dep_at_the_pinned_commit() {
    let origin = Origin::new().await;
    let work = origin.repo("dgit");
    work.write("src/main.rs", "fn main() { println!(\"dgit\"); }\n");
    work.commit_all("a source file");
    work.push("main");
    origin.publish("dgit");
    let pin = origin.tip("dgit", "main");

    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"dgit\"\nurl = \"{}\"\npin = \"{pin}\"\n",
            origin.url("dgit")
        ),
    )]);
    sync(&bed, "demo").await;
    let after_fetch = origin.requests();

    // Click one: the column entry opens the dep's own tree.
    let tree = page(&bed, "/demo/stack/dgit").await;
    assert!(tree.contains("href=\"/demo/stack/dgit/tree/src\""), "no directory link:\n{tree}");
    assert!(tree.contains("href=\"/demo/stack/dgit/blob/README.md\""), "no file link:\n{tree}");
    assert!(tree.contains(&pin[..8]), "the tree never says which commit it is:\n{tree}");

    // Click two: the file itself, at that commit.
    let nested = page(&bed, "/demo/stack/dgit/tree/src").await;
    assert!(nested.contains("href=\"/demo/stack/dgit/blob/src/main.rs\""), "{nested}");
    let blob = page(&bed, "/demo/stack/dgit/blob/src/main.rs").await;
    assert!(blob.contains("println!"), "the blob has no content:\n{blob}");

    // Reading a mirror is reading. Nothing about it goes to the wire.
    assert_eq!(origin.requests(), after_fetch, "browsing the column fetched");
}

#[tokio::test]
async fn every_dep_surface_is_read_only() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"dgit\"\nurl = \"{}\"\ntrack = \"main\"\n",
            origin.url("dgit")
        ),
    )]);
    sync(&bed, "demo").await;

    for path in ["/demo/stack", "/demo/stack/dgit", "/demo/stack/dgit/blob/README.md"] {
        let body = page(&bed, path).await;
        for forbidden in ["/demo/edit", "New file", "nashcode-composer", "/demo/raw/"] {
            assert!(!body.contains(forbidden), "{path} offers {forbidden:?}:\n{body}");
        }
    }
    // And there is no write route hiding under one of them.
    let (status, _) =
        post_json(&bed.router, "/demo/stack/dgit/blob/README.md", serde_json::json!({})).await;
    assert!(status == 404 || status == 405, "a dep blob took a POST: {status}");
}

// ---- ?rev= -----------------------------------------------------------------------

#[tokio::test]
async fn rev_narrows_to_a_commit_the_mirror_already_has() {
    let origin = Origin::new().await;
    let work = origin.repo("dgit");
    let first = origin.tip("dgit", "main");
    work.write("VERSION", "2\n");
    work.commit_all("version two");
    work.push("main");
    origin.publish("dgit");
    let second = origin.tip("dgit", "main");

    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"dgit\"\nurl = \"{}\"\ntrack = \"main\"\n",
            origin.url("dgit")
        ),
    )]);
    sync(&bed, "demo").await;
    let after_fetch = origin.requests();

    // The declared revision is the tracked branch's tip.
    let tip = page(&bed, "/demo/stack/dgit").await;
    assert!(tip.contains("VERSION"), "{tip}");
    assert!(tip.contains(&second[..8]), "{tip}");

    // `?rev=` narrows to the earlier commit, and every link out stays pinned to it.
    let older = page(&bed, &format!("/demo/stack/dgit?rev={first}")).await;
    assert!(!older.contains("VERSION"), "?rev= showed the wrong tree:\n{older}");
    assert!(older.contains(&first[..8]), "{older}");
    assert!(
        older.contains(&format!("href=\"/demo/stack/dgit/blob/README.md?rev={first}\"")),
        "the rev did not travel with the links:\n{older}"
    );
    let blob = page(&bed, &format!("/demo/stack/dgit/blob/README.md?rev={first}")).await;
    assert!(blob.contains("# dgit"), "{blob}");

    // An abbreviated rev is a rev.
    assert_eq!(status_of(&bed, &format!("/demo/stack/dgit?rev={}", &first[..10])).await, 200);
    assert_eq!(origin.requests(), after_fetch, "a browse at a rev went to the wire");
}

#[tokio::test]
async fn a_rev_the_mirror_does_not_have_is_a_404_and_never_a_fetch() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"dgit\"\nurl = \"{}\"\ntrack = \"main\"\n",
            origin.url("dgit")
        ),
    )]);
    sync(&bed, "demo").await;
    let after_fetch = origin.requests();

    let refused = [
        // A well-formed commit id the mirror simply does not have.
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        // Not a commit id at all: a branch, a flag, a path, a short prefix.
        "main",
        "--upload-pack=touch",
        "..%2F..%2Fetc",
        "1a2b3c",
        "zzzzzzzz",
    ];
    for rev in refused {
        for path in ["", "/tree/src", "/blob/README.md"] {
            let url = format!("/demo/stack/dgit{path}?rev={rev}");
            assert_eq!(status_of(&bed, &url).await, 404, "{url} was not a 404");
        }
    }
    assert_eq!(origin.requests(), after_fetch, "a rev the mirror lacks caused a fetch");
}

// ---- the name is the declaring repo's --------------------------------------------

#[tokio::test]
async fn a_dep_is_browsable_only_under_the_repo_that_declared_it() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    let bed = bed_declaring(&[
        (
            "demo",
            &format!(
                "[[dep]]\nname = \"dgit\"\nurl = \"{}\"\ntrack = \"main\"\n",
                origin.url("dgit")
            ),
        ),
        ("other", ""),
    ]);
    sync(&bed, "demo").await;

    assert_eq!(status_of(&bed, "/demo/stack/dgit").await, 200);
    // Same box, same mirror on disk, different repo: the name means nothing here.
    for path in ["/other/stack/dgit", "/other/stack/dgit/tree/src", "/other/stack/dgit/blob/README.md"]
    {
        assert_eq!(status_of(&bed, path).await, 404, "{path} reached another repo's dep");
    }
    // A name nobody declared, and a path that is not a name.
    for path in ["/demo/stack/nope", "/demo/stack/nope/tree/x", "/demo/stack/nope/blob/x"] {
        assert_eq!(status_of(&bed, path).await, 404, "{path}");
    }
    assert_eq!(status_of(&bed, "/demo/stack/dgit/blob/does/not/exist.rs").await, 404);
}

// ---- gitlinks --------------------------------------------------------------------

/// A `demo` repo whose tree carries submodule gitlinks: `(path, url, commit)`, with a
/// `.gitmodules` that declares each one.
///
/// The gitlinks are written straight into the index — a real `git submodule add` would
/// clone the upstream into the fixture, and the tree entry is the whole of what the
/// viewer reads.
fn bed_with_gitlinks(manifest: &str, gitlinks: &[(&str, &str, &str)]) -> TestBed {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    let bare = make_remote(&remotes, "demo");
    let work = common::Work::clone_from(&bare);
    work.write("README.md", "# demo\n");
    work.write(".nashcode/stack.toml", manifest);
    let modules: String = gitlinks
        .iter()
        .map(|(path, url, _)| {
            format!("[submodule \"{path}\"]\n\tpath = {path}\n\turl = {url}\n")
        })
        .collect();
    work.write(".gitmodules", &modules);
    git(&work.dir, &["add", "-A"]);
    for (path, _, commit) in gitlinks {
        git(&work.dir, &["update-index", "--add", "--cacheinfo", &format!("160000,{commit},{path}")]);
    }
    git(&work.dir, &["commit", "-m", "vendor the column"]);
    work.push("main");
    testbed_with(root, &["demo"], std::collections::BTreeMap::new())
}

#[tokio::test]
async fn a_gitlink_onto_a_mirrored_dep_links_through_and_the_rest_stay_inert() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    let pin = origin.tip("dgit", "main");
    let url = origin.url("dgit");

    let bed = bed_with_gitlinks(
        &format!("[[dep]]\nname = \"dgit\"\nurl = \"{url}\"\npin = \"{pin}\"\n"),
        &[
            // The same upstream, spelled differently from the manifest: one normalizer
            // decides these are the same mirror, or the link never happens.
            ("vendor/dgit", &format!("{}/", url.trim_end_matches(".git")), &pin),
            // Nobody's dep.
            ("vendor/stranger", "https://example.invalid/stranger.git", &pin),
            // Not a URL a mirror can be keyed by at all.
            ("vendor/relative", "../sibling.git", &pin),
        ],
    );
    sync(&bed, "demo").await;

    let tree = page(&bed, "/demo/tree/vendor").await;
    assert!(
        tree.contains(&format!("href=\"/demo/stack/dgit?rev={pin}\"")),
        "the matching gitlink did not link through:\n{tree}"
    );
    for inert in ["stranger", "relative"] {
        assert!(tree.contains(inert), "{inert} left the listing:\n{tree}");
        assert!(
            !tree.contains(&format!("href=\"/demo/stack/{inert}")),
            "{inert} became a link:\n{tree}"
        );
    }
    // Two of the three keep the label they have always had.
    assert_eq!(tree.matches(">submodule<").count(), 2, "{tree}");

    // And the link works: it is the dep, at the gitlink's own commit.
    let landed = page(&bed, &format!("/demo/stack/dgit?rev={pin}")).await;
    assert!(landed.contains(&pin[..8]), "{landed}");
    assert!(landed.contains("README.md"), "{landed}");
}

#[tokio::test]
async fn a_gitlink_whose_commit_is_not_mirrored_still_only_ever_404s() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    let pin = origin.tip("dgit", "main");
    let stranded = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let bed = bed_with_gitlinks(
        &format!(
            "[[dep]]\nname = \"dgit\"\nurl = \"{}\"\npin = \"{pin}\"\n",
            origin.url("dgit")
        ),
        &[("vendor/dgit", &origin.url("dgit"), stranded)],
    );
    sync(&bed, "demo").await;
    let after_fetch = origin.requests();

    // The link is drawn from the tree, not from a fetch. Following it to a commit the
    // mirror has never seen is the ordinary 404, and still no request.
    let tree = page(&bed, "/demo/tree/vendor").await;
    assert!(tree.contains(&format!("href=\"/demo/stack/dgit?rev={stranded}\"")), "{tree}");
    assert_eq!(status_of(&bed, &format!("/demo/stack/dgit?rev={stranded}")).await, 404);
    assert_eq!(origin.requests(), after_fetch, "following a stale gitlink fetched");
}
