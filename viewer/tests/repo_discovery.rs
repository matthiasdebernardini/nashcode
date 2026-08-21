//! Repo discovery: the viewer asks the git server what it is serving, once a poll
//! cycle, and never unlearns a name.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::{Work, get, git, make_remote, stacked_fixture, testbed_from_config, testbed_with};
use nashcode::config::Config;

/// The whole point: nothing is configured, somebody pushes, and one cycle later the
/// repo is mirrored, listed, and browsable. No environment change, no restart.
#[tokio::test]
async fn a_repo_pushed_after_startup_shows_up_within_one_cycle() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(root.path().join("remotes")).expect("mkdir");
    let bed = testbed_with(root, &[], BTreeMap::new());

    let (status, body) = get(&bed.router, "/").await;
    assert_eq!(status, 200);
    assert!(body.contains("No repos yet"), "an empty viewer says so: {body}");

    // A push creates the repo on the server, which is all dgit needs.
    stacked_fixture(&bed.remote_root(), "fresh");
    assert!(!bed.config.knows_repo("fresh"), "not known before a cycle runs");

    bed.mirrors.refresh_all().await;

    assert!(bed.config.knows_repo("fresh"), "one cycle learns the name");
    assert!(bed.config.mirror_path("fresh").exists(), "first sight clones the mirror");

    let (status, body) = get(&bed.router, "/").await;
    assert_eq!(status, 200);
    assert!(body.contains("fresh"), "the index lists it: {body}");
    assert!(!body.contains("No repos yet"), "and no longer says it is empty");

    let (status, body) = get(&bed.router, "/brain").await;
    assert_eq!(status, 200);
    let brain: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(brain["repos"][0]["name"], "fresh", "{body}");

    // The gate every request asks agrees, so the repo's pages work.
    let (status, _) = get(&bed.router, "/fresh").await;
    assert_eq!(status, 200);
}

/// `NASHCODE_REPOS` is a seed, and a seed is never washed away: a cycle that does not
/// list the seeded name leaves it in place, mirror and all.
#[tokio::test]
async fn a_seeded_repo_survives_a_cycle_that_does_not_list_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    stacked_fixture(&remotes, "seeded");
    let bed = testbed_with(root, &["seeded"], BTreeMap::new());
    bed.mirrors.refresh_all().await;
    assert!(bed.config.mirror_path("seeded").exists());

    // Take it off the server. dgit would stop listing it; the viewer keeps it.
    let bare = bed.remote_root().join("seeded.git");
    std::fs::rename(&bare, bare.with_extension("gone")).expect("rename");
    stacked_fixture(&bed.remote_root(), "other");

    bed.mirrors.refresh_all().await;

    assert!(bed.config.knows_repo("seeded"), "a name is never dropped");
    assert!(bed.config.knows_repo("other"), "and the new one is learnt anyway");
    let (status, body) = get(&bed.router, "/").await;
    assert_eq!(status, 200);
    assert!(body.contains("seeded") && body.contains("other"), "{body}");
}

/// The production path: `DGIT_URL` is a URL, so the list is dgit's HTML index page,
/// fetched with the same basic auth the clones use.
#[tokio::test]
async fn a_url_dgit_is_read_from_its_index_page() {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    for name in ["alpha", "beta"] {
        let bare = make_remote(&remotes, name);
        let work = Work::clone_from(&bare);
        work.write("README.md", &format!("# {name}\n"));
        work.commit_all("initial");
        work.push("main");
        // What makes a bare repo clonable without a git server.
        git(&bare, &["update-server-info"]);
    }
    let dgit = FakeDgit::start(remotes, Index::Served).await;

    let bed = testbed_with(root, &[], BTreeMap::new());
    let config = Arc::new(Config {
        dgit_url: dgit.url.clone(),
        git_token: "sekrit".to_owned(),
        repos: Default::default(),
        db_path: bed.root.path().join("served.db"),
        ..(*bed.config).clone()
    });
    let served = testbed_from_config(tempfile::tempdir().expect("tempdir"), config);

    served.mirrors.refresh_all().await;

    assert!(served.config.knows_repo("alpha"));
    assert!(served.config.knows_repo("beta"));
    assert!(!served.config.knows_repo("cgit.css"), "navigation links are not repos");
    assert!(!served.config.knows_repo("brain"), "a name the router owns is refused");
    assert!(served.config.mirror_path("alpha").exists(), "and the mirror cloned over HTTP");
    assert_eq!(dgit.index_auth(), Some("x:sekrit".to_owned()), "the index fetch is authed");
}

/// A git server that answers the index page with a 500 leaves the set exactly as it
/// was. Degrade, never lose: the alternative is an outage that 404s every repo.
#[tokio::test]
async fn a_broken_index_page_changes_nothing() {
    let root = tempfile::tempdir().expect("tempdir");
    let remotes = root.path().join("remotes");
    std::fs::create_dir_all(&remotes).expect("mkdir");
    stacked_fixture(&remotes, "kept");
    let dgit = FakeDgit::start(remotes, Index::Broken).await;

    let bed = testbed_with(root, &["kept"], BTreeMap::new());
    let config = Arc::new(Config {
        dgit_url: dgit.url.clone(),
        repos: ["kept"].into_iter().collect(),
        db_path: bed.root.path().join("broken.db"),
        ..(*bed.config).clone()
    });
    let served = testbed_from_config(tempfile::tempdir().expect("tempdir"), config);

    served.mirrors.refresh_all().await;

    assert_eq!(served.config.repos.names(), vec!["kept".to_owned()], "the set stands");
    let (status, _) = get(&served.router, "/").await;
    assert_eq!(status, 200, "and the index page still renders");
}

/// Whether [`FakeDgit`] serves an index page or fails the way a broken server does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Index {
    Served,
    Broken,
}

/// A dgit-shaped HTTP server: the index page at `/`, and the bare repos under it for
/// the dumb-protocol clone that follows.
struct FakeDgit {
    url: String,
    auth: Arc<Mutex<Option<String>>>,
}

/// dgit's index page, near enough: a `class="list"` table with one row per repo, plus
/// the stylesheet link that must not be read as one.
const INDEX_HTML: &str = "<html><body><table class='list nowrap'>\
    <tr><td><a href='/alpha/'>alpha</a></td><td>first</td><td>me</td><td>2 days</td></tr>\
    <tr><td><a href='/beta/'>beta</a></td><td>[no description]</td><td>me</td><td></td></tr>\
    <tr><td><a href='/brain/'>brain</a></td><td>a route, not a repo</td></tr>\
    <tr><td><a href='/cgit.css'>css</a></td></tr>\
    </table></body></html>";

impl FakeDgit {
    async fn start(root: PathBuf, index: Index) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let auth = Arc::new(Mutex::new(None));
        let seen = auth.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let (root, seen) = (root.clone(), seen.clone());
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..read]).into_owned();
                    let target = head.split_whitespace().nth(1).unwrap_or_default();
                    let path = target.split(['?', '#']).next().unwrap_or_default();

                    let (kind, body) = if path == "/" {
                        *seen.lock().expect("lock") = basic_auth(&head);
                        match index {
                            Index::Served => ("text/html", Some(INDEX_HTML.as_bytes().to_vec())),
                            Index::Broken => {
                                let _ = socket
                                    .write_all(
                                        b"HTTP/1.1 500 Internal Server Error\r\n\
                                          content-length: 0\r\nconnection: close\r\n\r\n",
                                    )
                                    .await;
                                let _ = socket.shutdown().await;
                                return;
                            }
                        }
                    } else {
                        let file = path.trim_start_matches('/');
                        let safe = !file.is_empty()
                            && file.split('/').all(|part| !part.is_empty() && part != "..");
                        // octet-stream, so git falls back to the dumb protocol rather
                        // than believing it found a smart one.
                        ("application/octet-stream", safe.then(|| std::fs::read(root.join(file)).ok()).flatten())
                    };
                    let (status, body) = match body {
                        Some(body) => ("200 OK", body),
                        None => ("404 Not Found", Vec::new()),
                    };
                    let mut reply = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    reply.extend_from_slice(&body);
                    let _ = socket.write_all(&reply).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { url, auth }
    }

    /// The `user:password` the index fetch presented, if any.
    fn index_auth(&self) -> Option<String> {
        self.auth.lock().expect("lock").clone()
    }
}

/// Decode the `Authorization: Basic` header out of a raw request head.
fn basic_auth(head: &str) -> Option<String> {
    use base64::Engine;
    let line =
        head.lines().find(|line| line.to_ascii_lowercase().starts_with("authorization:"))?;
    let encoded = line.split_whitespace().nth(2)?;
    let raw = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    String::from_utf8(raw).ok()
}
