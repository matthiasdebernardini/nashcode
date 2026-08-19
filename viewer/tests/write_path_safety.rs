//! What the single-file write path refuses.
//!
//! `ops::commit_file` writes into a scratch checkout of a repo whose contents nobody
//! vetted. A repo may hold a committed symlink, and a path may look like a git flag.
//! Neither may reach outside the clone or change what git is asked to do.

mod common;

use std::path::Path;

use common::{Work, git, make_remote, post_form, simple_bed};

/// A repo with one ordinary file, ready for whatever hostile thing a test adds.
fn hostile_fixture(root: &Path) -> Work {
    let remote = make_remote(root, "demo");
    let work = Work::clone_from(&remote);
    work.write("README.md", "# demo\n");
    work.commit_all("initial");
    work
}

/// Commit `link -> target` as a real symlink.
///
/// git stores the target as blob content under mode `120000`, so this needs no
/// filesystem trickery — which is exactly why a pushed repo can carry one.
fn commit_symlink(work: &Work, link: &str, target: &str) {
    use std::io::Write;
    let output = std::process::Command::new("git")
        .current_dir(&work.dir)
        .args(["hash-object", "-w", "--stdin", "-t", "blob"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.as_mut().expect("stdin").write_all(target.as_bytes())?;
            child.wait_with_output()
        })
        .expect("hash-object runs");
    assert!(output.status.success(), "hash-object failed");
    let blob = String::from_utf8_lossy(&output.stdout).trim().to_owned();

    git(&work.dir, &["update-index", "--add", "--cacheinfo", &format!("120000,{blob},{link}")]);
    git(&work.dir, &["commit", "-m", "add a symlink"]);
}

#[tokio::test]
async fn a_committed_symlink_cannot_be_written_through() {
    let outside = tempfile::tempdir().expect("tempdir");
    let target = outside.path().to_string_lossy().into_owned();
    let sentinel = outside.path().join("owned.txt");

    let bed = simple_bed(|root| {
        let work = hostile_fixture(root);
        commit_symlink(&work, "escape", &target);
        work.push("main");
        work
    });
    let before = bed.remote_tip("demo", "main");

    let (status, location, body) = post_form(
        &bed.router,
        "/demo/edit",
        &[
            ("path", "escape/owned.txt"),
            ("content", "this must never reach the filesystem\n"),
            ("message", "escape"),
        ],
    )
    .await;

    assert!(!sentinel.exists(), "a file was written outside the scratch clone");
    assert!(location.is_none(), "the escape reported success");
    assert_eq!(status, 200, "expected the form back, not a redirect");
    assert!(body.contains("Nothing was committed"), "no refusal on the page:\n{body}");
    assert_eq!(before, bed.remote_tip("demo", "main"), "the escape still pushed");
}

#[tokio::test]
async fn a_symlinked_file_itself_cannot_be_overwritten() {
    let outside = tempfile::tempdir().expect("tempdir");
    let sentinel = outside.path().join("target.txt");
    std::fs::write(&sentinel, "original\n").expect("write");

    let bed = simple_bed(|root| {
        let work = hostile_fixture(root);
        commit_symlink(&work, "notes.md", &sentinel.to_string_lossy());
        work.push("main");
        work
    });

    let (status, location, body) = post_form(
        &bed.router,
        "/demo/edit",
        &[("path", "notes.md"), ("content", "replaced\n"), ("message", "x")],
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("read"),
        "original\n",
        "the file the symlink pointed at was rewritten"
    );
    assert!(location.is_none(), "the write reported success");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("symbolic link"), "the refusal does not say why:\n{body}");
}

#[tokio::test]
async fn a_path_that_reads_as_a_git_flag_is_committed_as_a_file() {
    let bed = simple_bed(|root| {
        let work = hostile_fixture(root);
        work.push("main");
        work
    });

    // Without `--`, `git add -i` opens the interactive picker and hangs or fails;
    // either way the file is not what gets added.
    let (status, location, body) =
        post_form(&bed.router, "/demo/edit", &[("path", "-i"), ("content", "flag\n"), ("message", "x")])
            .await;
    assert_eq!(status, 303, "a file named -i was not committed:\n{body}");
    assert_eq!(location.as_deref(), Some("/demo/blob/-i"));

    let listing = git(&bed.remote_root().join("demo.git"), &["ls-tree", "--name-only", "main"]);
    assert!(listing.contains("-i"), "the file is not in the tree: {listing}");
}

#[tokio::test]
async fn a_directory_path_is_refused_rather_than_written_over() {
    let bed = simple_bed(|root| {
        let work = hostile_fixture(root);
        work.write("src/lib.rs", "fn main() {}\n");
        work.commit_all("a directory to aim at");
        work.push("main");
        work
    });
    let (status, location, body) =
        post_form(&bed.router, "/demo/edit", &[("path", "src"), ("content", "x\n"), ("message", "x")])
            .await;
    assert_eq!(status, 200, "{body}");
    assert!(location.is_none(), "writing over a directory reported success");
    assert!(body.contains("Nothing was committed"), "no refusal:\n{body}");
}
