//! The upstream column: `.nashcode/stack.toml`, the mirrors it names, and the brain
//! stanza that reports both.
//!
//! Every upstream here is a real bare repo published with `git update-server-info` and
//! served over git's dumb HTTP protocol from a loopback port. The fetches are therefore
//! real fetches, and the test server's request counter is what proves a pinned
//! dependency stops asking once it has what it was pinned to. Nothing touches the
//! network.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use common::{
    FileServer, TestBed, Work, get, git, make_remote, post_json, serve_dir, testbed_with,
};

/// A directory of bare repos, served over the dumb protocol: the upstreams under test.
struct Origin {
    keep: tempfile::TempDir,
    server: FileServer,
}

impl Origin {
    async fn new() -> Self {
        let keep = tempfile::tempdir().expect("tempdir");
        let server = serve_dir(keep.path().to_path_buf()).await;
        Self { keep, server }
    }

    /// A new upstream with one commit on `main`, published for dumb clients.
    fn repo(&self, name: &str) -> Work {
        let bare = make_remote(self.keep.path(), name);
        let work = Work::clone_from(&bare);
        work.write("README.md", &format!("# {name}\n"));
        work.commit_all("initial");
        work.push("main");
        self.publish(name);
        work
    }

    /// `git update-server-info` is what makes a bare repo readable without a git
    /// server, so every push to an upstream is followed by one.
    fn publish(&self, name: &str) {
        git(&self.bare(name), &["update-server-info"]);
    }

    fn bare(&self, name: &str) -> PathBuf {
        self.keep.path().join(format!("{name}.git"))
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{name}.git", self.server.url)
    }

    fn tip(&self, name: &str, branch: &str) -> String {
        git(&self.bare(name), &["rev-parse", &format!("refs/heads/{branch}")]).trim().to_owned()
    }

    fn requests(&self) -> usize {
        self.server.requests()
    }

    /// Take an upstream off the air without stopping the server, so the next fetch
    /// fails the way an upstream that moved or died does.
    fn unpublish(&self, name: &str) {
        let bare = self.bare(name);
        std::fs::rename(&bare, bare.with_extension("gone")).expect("rename");
    }
}

/// A testbed whose repos each declare the manifest given. An empty manifest means the
/// repo has no `.nashcode/stack.toml` at all.
fn bed_declaring(manifests: &[(&str, &str)]) -> TestBed {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    for (name, manifest) in manifests {
        let bare = make_remote(&remotes, name);
        let work = Work::clone_from(&bare);
        work.write("README.md", &format!("# {name}\n"));
        if !manifest.is_empty() {
            work.write(".nashcode/stack.toml", manifest);
        }
        work.commit_all("initial");
        work.push("main");
    }
    let names: Vec<&str> = manifests.iter().map(|(name, _)| *name).collect();
    testbed_with(root, &names, BTreeMap::new())
}

/// `POST /{repo}/stack/sync`, returning the stack it answered with.
async fn sync(bed: &TestBed, repo: &str) -> serde_json::Value {
    let (status, body) =
        post_json(&bed.router, &format!("/{repo}/stack/sync"), serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).expect("sync answers json");
    assert_eq!(answer["repo"], repo);
    answer["stack"].clone()
}

/// The `stack` stanza `GET /brain` reports for a repo.
async fn stanza(bed: &TestBed, repo: &str) -> serde_json::Value {
    let (status, body) = get(&bed.router, &format!("/brain?repo={repo}")).await;
    assert_eq!(status, 200, "{body}");
    let brain: serde_json::Value = serde_json::from_str(&body).expect("brain answers json");
    brain["repos"][0]["stack"].clone()
}

/// Where a URL's mirror should be, by the layout SPEC promises.
fn up(bed: &TestBed) -> PathBuf {
    bed.config.mirrors.join("up")
}

/// Every directory under `up/`, one line each, for an assertion that can be read.
fn mirror_dirs(bed: &TestBed) -> Vec<String> {
    fn walk(dir: &std::path::Path, into: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "git") {
                into.push(path.to_string_lossy().into_owned());
            } else if path.is_dir() {
                walk(&path, into);
            }
        }
    }
    let mut found = Vec::new();
    walk(&up(bed), &mut found);
    found.sort();
    found
}

// ---- the happy path --------------------------------------------------------------

#[tokio::test]
async fn a_pinned_and_a_tracked_dep_both_resolve_to_a_commit_on_disk() {
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
pin   = "{}"
layer = "server"

[[dep]]
name  = "celld"
url   = "{}"
track = "main"
layer = "runtime"
"#,
            origin.url("dgit"),
            &pin[..8],
            origin.url("celld"),
        ),
    )]);

    let stack = sync(&bed, "demo").await;
    assert_eq!(stack["error"], serde_json::Value::Null);

    let dgit = &stack["deps"][0];
    assert_eq!(dgit["name"], "dgit");
    assert_eq!(dgit["mode"], "pin");
    assert_eq!(dgit["layer"], "server");
    assert_eq!(dgit["want"], pin[..8].to_owned());
    assert_eq!(dgit["have"], pin, "the abbreviated pin did not resolve to a full sha");
    assert_eq!(dgit["fresh"], true);
    assert_eq!(dgit["error"], serde_json::Value::Null);
    assert!(dgit["last_fetched"].is_string(), "a fetched mirror has no timestamp");

    let celld = &stack["deps"][1];
    assert_eq!(celld["mode"], "track");
    assert_eq!(celld["want"], "main");
    assert_eq!(celld["have"], origin.tip("celld", "main"));
    assert_eq!(celld["fresh"], true);

    // The brain tells the same story, and the mirrors sit where SPEC says.
    assert_eq!(stanza(&bed, "demo").await, stack);
    let host = format!("127.0.0.1:{}", origin.server.url.rsplit(':').next().expect("port"));
    assert_eq!(
        mirror_dirs(&bed),
        vec![
            up(&bed).join(&host).join("celld.git").to_string_lossy().into_owned(),
            up(&bed).join(&host).join("dgit.git").to_string_lossy().into_owned(),
        ]
    );
}

#[tokio::test]
async fn a_repo_that_declares_nothing_has_no_stack_key_at_all() {
    let bed = bed_declaring(&[("demo", "")]);
    let (status, body) = get(&bed.router, "/brain?repo=demo").await;
    assert_eq!(status, 200, "{body}");
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(
        brain["repos"][0].get("stack").is_none(),
        "a repo with no manifest grew a stack stanza:\n{body}"
    );

    // Sync still answers, and says the same thing in one word.
    let (status, body) =
        post_json(&bed.router, "/demo/stack/sync", serde_json::json!({})).await;
    assert_eq!(status, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["stack"], serde_json::Value::Null);
}

#[tokio::test]
async fn an_unknown_repo_gets_the_same_404_every_other_repo_route_gives() {
    let bed = bed_declaring(&[("demo", "")]);
    let (status, _) = post_json(&bed.router, "/nope/stack/sync", serde_json::json!({})).await;
    assert_eq!(status, 404);
    let (status, _) =
        post_json(&bed.router, "/..%2Fetc/stack/sync", serde_json::json!({})).await;
    assert_eq!(status, 404);
}

// ---- manifests that are wrong ----------------------------------------------------

#[tokio::test]
async fn a_manifest_that_is_not_toml_is_one_error_and_breaks_nothing_else() {
    let bed = bed_declaring(&[("demo", "[[dep]\nname = \n")]);

    let stack = stanza(&bed, "demo").await;
    assert!(stack["error"].is_string(), "the parse error did not reach the brain: {stack}");
    assert_eq!(stack["deps"].as_array().map(Vec::len), Some(0));

    // Every other surface is untouched: this is a degradation, not a failure.
    for path in ["/demo", "/demo/board", "/demo/plans", "/demo/blob/README.md"] {
        let (status, body) = get(&bed.router, path).await;
        assert_eq!(status, 200, "{path} broke on a bad manifest:\n{body}");
    }
    let (status, body) = get(&bed.router, "/brain?repo=demo").await;
    assert_eq!(status, 200);
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(brain["repos"][0]["default_branch"], "main");
    assert!(!brain["repos"][0]["branches"].as_array().expect("branches").is_empty());

    assert!(!up(&bed).exists(), "a manifest that will not parse still made a mirror");
}

#[tokio::test]
async fn every_dep_the_manifest_got_wrong_says_so_and_is_never_fetched() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    let url = origin.url("dgit");

    let bed = bed_declaring(&[(
        "demo",
        &format!(
            r#"
[[dep]]
name  = "both"
url   = "{url}"
pin   = "1a2b3c4"
track = "main"

[[dep]]
name  = "neither"
url   = "{url}"

[[dep]]
name  = "twice"
url   = "{url}"
track = "main"

[[dep]]
name  = "twice"
url   = "{url}"
track = "main"
"#
        ),
    )]);

    let stack = sync(&bed, "demo").await;
    assert_eq!(stack["error"], serde_json::Value::Null);
    let deps = stack["deps"].as_array().expect("deps");
    assert_eq!(deps.len(), 4);

    let why = |index: usize| deps[index]["error"].as_str().unwrap_or_default().to_owned();
    assert!(why(0).contains("both pin and track"), "{}", why(0));
    assert!(why(1).contains("neither pin nor track"), "{}", why(1));
    assert_eq!(deps[2]["error"], serde_json::Value::Null, "the first `twice` was refused");
    assert!(why(3).contains("declared twice"), "{}", why(3));

    for index in [0, 1, 3] {
        assert_eq!(deps[index]["fresh"], false, "a refused dep called itself fresh: {}", deps[index]);
        assert_eq!(deps[index]["have"], serde_json::Value::Null);
    }
    // Only the one good dep is on disk. Three bad declarations fetched nothing.
    assert_eq!(deps[2]["fresh"], true, "the one good dep did not fetch: {}", deps[2]);
    assert_eq!(mirror_dirs(&bed).len(), 1);
}

#[tokio::test]
async fn a_url_that_is_not_http_is_reported_and_never_reached() {
    // The canary is a real, working upstream. The manifest points at it by a scheme we
    // refuse; if anything ever fetched it, the counter would say so.
    let origin = Origin::new().await;
    origin.repo("dgit");
    let served = origin.keep.path().join("dgit.git");
    let before = origin.requests();

    let bed = bed_declaring(&[(
        "demo",
        &format!(
            r#"
[[dep]]
name = "by-file"
url  = "file://{}"
pin  = "1a2b3c4"

[[dep]]
name = "by-ssh"
url  = "ssh://git@127.0.0.1/dgit.git"
track = "main"

[[dep]]
name = "by-path"
url  = "{}"
track = "main"

[[dep]]
name = "by-plain-http"
url  = "http://10.1.2.3/celld.git"
track = "main"
"#,
            served.display(),
            served.display(),
        ),
    )]);

    let stack = sync(&bed, "demo").await;
    let deps = stack["deps"].as_array().expect("deps");
    for dep in deps {
        let error = dep["error"].as_str().unwrap_or_default();
        assert!(!error.is_empty(), "{} was accepted: {dep}", dep["name"]);
        assert_eq!(dep["have"], serde_json::Value::Null);
        assert_eq!(dep["fresh"], false);
    }
    assert!(deps[0]["error"].as_str().expect("error").contains("only http and https"));
    assert!(deps[1]["error"].as_str().expect("error").contains("only http and https"));
    // Plain http off this box is refused before any subprocess runs: it would otherwise
    // downgrade the transport of a mirror somebody else declared over https.
    assert!(deps[3]["error"].as_str().expect("error").contains("must be https"));

    assert_eq!(origin.requests(), before, "a refused url reached the wire");
    assert!(!up(&bed).exists(), "a refused url made a directory");
}

#[tokio::test]
async fn a_url_that_climbs_cannot_land_outside_the_up_directory() {
    // Nothing listens on port 1, so no fetch can succeed and the only question left is
    // where the mirror directory was going to be.
    let bed = bed_declaring(&[(
        "demo",
        r#"
[[dep]]
name  = "climb"
url   = "http://127.0.0.1:1/a/../../../../escape"
track = "main"

[[dep]]
name  = "encoded"
url   = "http://127.0.0.1:1/%2e%2e/%2e%2e/escape"
track = "main"
"#,
    )]);

    let stack = sync(&bed, "demo").await;
    // Either answer is right. The URL parser resolves a climb into an ordinary path
    // before we see it, and a segment it cannot resolve is refused with an error. What
    // must never happen is a directory outside `up/`.
    assert_eq!(stack["deps"].as_array().map(Vec::len), Some(2));

    for path in mirror_dirs(&bed) {
        assert!(path.starts_with(&up(&bed).to_string_lossy().into_owned()), "{path} escaped");
    }
    for escaped in ["escape.git", "../escape.git", "a/escape.git"] {
        assert!(!bed.config.mirrors.join(escaped).exists(), "{escaped} was created");
    }
}

// ---- fetch discipline ------------------------------------------------------------

#[tokio::test]
async fn one_url_declared_by_two_repos_is_one_mirror() {
    let origin = Origin::new().await;
    origin.repo("celld");
    // The same upstream, spelled differently by each repo. Normalization is what makes
    // them one mirror rather than two.
    let manifest = |url: String| {
        format!("[[dep]]\nname = \"celld\"\nurl = \"{url}\"\ntrack = \"main\"\n")
    };
    let plain = origin.url("celld");
    let bed = bed_declaring(&[
        ("demo", manifest(plain.clone()).as_str()),
        ("other", manifest(format!("{}/", plain.trim_end_matches(".git"))).as_str()),
    ]);

    sync(&bed, "demo").await;
    sync(&bed, "other").await;

    assert_eq!(mirror_dirs(&bed).len(), 1, "two repos made two copies of one upstream");
    for repo in ["demo", "other"] {
        let stack = stanza(&bed, repo).await;
        assert_eq!(stack["deps"][0]["have"], origin.tip("celld", "main"), "{repo}");
    }
}

#[tokio::test]
async fn a_pin_already_on_disk_never_goes_back_to_the_wire() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    let pin = origin.tip("dgit", "main");
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"dgit\"\nurl = \"{}\"\npin = \"{pin}\"\n",
            origin.url("dgit")
        ),
    )]);

    let stack = sync(&bed, "demo").await;
    assert_eq!(stack["deps"][0]["have"], pin);
    let after_clone = origin.requests();
    assert!(after_clone > 0, "the clone made no requests at all");

    // Sync means "go to the wire now" for a tracked branch. A pin has nowhere to go:
    // upstream cannot change what that commit says.
    for _ in 0..3 {
        sync(&bed, "demo").await;
    }
    assert_eq!(origin.requests(), after_clone, "a satisfied pin asked upstream again");
    assert_eq!(stanza(&bed, "demo").await["deps"][0]["fresh"], true);
}

#[tokio::test]
async fn sync_picks_up_a_branch_that_moved_upstream() {
    let origin = Origin::new().await;
    let work = origin.repo("celld");
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"celld\"\nurl = \"{}\"\ntrack = \"main\"\nlayer = \"runtime\"\n",
            origin.url("celld")
        ),
    )]);
    // This test is about the fetch, not about the budget that spaces two of them out.
    bed.upstreams.set_sync_debounce(Duration::ZERO);

    let first = sync(&bed, "demo").await;
    assert_eq!(first["deps"][0]["have"], origin.tip("celld", "main"));

    work.write("src/spawn.rs", "fn spawn_worker() {}\n");
    work.commit_all("spawn workers");
    work.push("main");
    origin.publish("celld");
    let moved = origin.tip("celld", "main");
    assert_ne!(first["deps"][0]["have"], moved);

    let second = sync(&bed, "demo").await;
    assert_eq!(second["deps"][0]["have"], moved, "sync did not fetch the moved branch");
    assert_eq!(stanza(&bed, "demo").await["deps"][0]["have"], moved);
}

#[tokio::test]
async fn sync_will_not_go_back_to_the_wire_twice_in_a_minute() {
    let origin = Origin::new().await;
    origin.repo("celld");
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"celld\"\nurl = \"{}\"\ntrack = \"main\"\n",
            origin.url("celld")
        ),
    )]);

    let first = sync(&bed, "demo").await;
    let after_clone = origin.requests();
    assert!(after_clone > 0, "the clone made no requests at all");

    // Sync is a route anyone can call in a loop and the wire on the far end is somebody
    // else's. Inside the budget it answers from disk without asking again.
    for _ in 0..5 {
        let again = sync(&bed, "demo").await;
        assert_eq!(again["deps"][0]["have"], first["deps"][0]["have"]);
        assert_eq!(again["deps"][0]["fresh"], true);
    }
    assert_eq!(origin.requests(), after_clone, "sync ignored its own rate limit");
}

#[tokio::test]
async fn a_pin_the_upstream_does_not_publish_says_so() {
    let origin = Origin::new().await;
    origin.repo("dgit");
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"dgit\"\nurl = \"{}\"\npin = \"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\"\n",
            origin.url("dgit")
        ),
    )]);

    let dep = sync(&bed, "demo").await["deps"][0].clone();
    // The mirror fetched fine. The commit simply is not on any branch or tag upstream,
    // which is a different thing from being behind, and has to read that way.
    assert!(dep["last_fetched"].is_string(), "the mirror never fetched: {dep}");
    assert_eq!(dep["have"], serde_json::Value::Null);
    assert_eq!(dep["fresh"], false);
    assert!(
        dep["error"].as_str().unwrap_or_default().contains("not in the upstream's branches"),
        "an unreachable pin said {:?}",
        dep["error"]
    );
}

#[tokio::test]
async fn a_satisfied_pin_does_not_inherit_a_neighbours_failure() {
    let origin = Origin::new().await;
    origin.repo("celld");
    let pin = origin.tip("celld", "main");
    let url = origin.url("celld");
    // Both deps are the same upstream, so they share one mirror and one fetch state.
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"pinned\"\nurl = \"{url}\"\npin = \"{pin}\"\n\n\
             [[dep]]\nname = \"tracked\"\nurl = \"{url}\"\ntrack = \"main\"\n"
        ),
    )]);
    bed.upstreams.set_sync_debounce(Duration::ZERO);

    let stack = sync(&bed, "demo").await;
    assert_eq!(stack["deps"][0]["fresh"], true);
    assert_eq!(stack["deps"][1]["fresh"], true);

    origin.unpublish("celld");
    let stack = sync(&bed, "demo").await;

    // "Then never again" has to mean something: the pin has its commit, and the branch
    // its neighbour follows going dark is not its problem.
    assert_eq!(stack["deps"][0]["have"], pin);
    assert_eq!(stack["deps"][0]["error"], serde_json::Value::Null, "{}", stack["deps"][0]);
    assert_eq!(stack["deps"][0]["fresh"], true);

    assert!(stack["deps"][1]["error"].is_string(), "the tracked dep hid its failure");
    assert_eq!(stack["deps"][1]["fresh"], false);
}

#[tokio::test]
async fn the_brain_stanza_starts_the_fetch_behind_the_caller() {
    let origin = Origin::new().await;
    origin.repo("celld");
    let bed = bed_declaring(&[(
        "demo",
        &format!(
            "[[dep]]\nname = \"celld\"\nurl = \"{}\"\ntrack = \"main\"\n",
            origin.url("celld")
        ),
    )]);

    // Nothing in this test calls sync. The only thing that can fetch is the stanza
    // starting the work behind the caller's back, the way a page load does.
    let tip = serde_json::Value::String(origin.tip("celld", "main"));
    let mut have = serde_json::Value::Null;
    for _ in 0..200 {
        have = stanza(&bed, "demo").await["deps"][0]["have"].clone();
        if have == tip {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(have, tip, "reading the stanza never started the fetch");
    assert!(origin.requests() > 0);
}

#[tokio::test]
async fn an_upstream_that_cannot_be_reached_degrades_to_stale() {
    // Port 1 on loopback: nothing listens, and nothing ever will.
    let bed = bed_declaring(&[(
        "demo",
        "[[dep]]\nname = \"gone\"\nurl = \"http://127.0.0.1:1/gone.git\"\ntrack = \"main\"\n",
    )]);

    let stack = sync(&bed, "demo").await;
    let dep = &stack["deps"][0];
    assert_eq!(dep["mode"], "track");
    assert_eq!(dep["have"], serde_json::Value::Null);
    assert_eq!(dep["fresh"], false);
    assert_eq!(dep["last_fetched"], serde_json::Value::Null);
    assert!(dep["error"].is_string(), "a failed fetch left no explanation: {dep}");

    // The repo itself is unharmed: an upstream nobody can reach is not our outage.
    let (status, body) = get(&bed.router, "/demo").await;
    assert_eq!(status, 200, "{body}");
}
