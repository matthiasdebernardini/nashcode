//! jj awareness starts with detection, and detection reads only the directory
//! layout. These fixtures are the three layouts that exist in the wild:
//! plain git, colocated jj (`.jj` and `.git`), and jj-only.

use nashcode_cli::vcs::{Kind, classify, detect};
use std::fs;
use std::path::Path;

fn mkdir(p: &Path) {
    fs::create_dir_all(p).unwrap();
}

#[test]
fn a_plain_git_layout_is_git() {
    let dir = tempfile::tempdir().unwrap();
    mkdir(&dir.path().join(".git"));
    assert_eq!(classify(dir.path()), Some(Kind::Git));
}

#[test]
fn a_colocated_layout_is_jj_and_git_tooling_still_applies() {
    let dir = tempfile::tempdir().unwrap();
    mkdir(&dir.path().join(".git"));
    mkdir(&dir.path().join(".jj"));
    let kind = classify(dir.path()).unwrap();
    assert_eq!(kind, Kind::JjColocated);
    assert!(kind.is_jj());
}

#[test]
fn a_jj_only_layout_is_jj() {
    let dir = tempfile::tempdir().unwrap();
    mkdir(&dir.path().join(".jj"));
    assert_eq!(classify(dir.path()), Some(Kind::JjOnly));
}

#[test]
fn an_unversioned_directory_is_neither() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(classify(dir.path()), None);
}

#[test]
fn a_git_worktree_pointer_file_still_counts() {
    // In a `git worktree`, `.git` is a file, not a directory.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
    assert_eq!(classify(dir.path()), Some(Kind::Git));
}

#[test]
fn detect_walks_up_to_the_nearest_root_and_stops_there() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    mkdir(&root.join(".jj"));
    let deep = root.join("a/b/c");
    mkdir(&deep);

    let ws = detect(&deep).unwrap();
    assert_eq!(ws.kind, Kind::JjOnly);
    assert_eq!(
        ws.root.canonicalize().unwrap(),
        root.canonicalize().unwrap()
    );
    assert_eq!(ws.default_repo_name().as_deref(), Some("repo"));

    // A nested repo wins over an outer one.
    let inner = root.join("vendor/tool");
    mkdir(&inner.join(".git"));
    let ws = detect(&inner).unwrap();
    assert_eq!(ws.kind, Kind::Git);
    assert_eq!(ws.default_repo_name().as_deref(), Some("tool"));
}

#[test]
fn detection_needs_no_git_or_jj_binary() {
    // The seams exist so a test can point both binaries at nothing at all;
    // classification must still answer from the layout alone.
    let dir = tempfile::tempdir().unwrap();
    mkdir(&dir.path().join(".jj"));
    mkdir(&dir.path().join(".git"));
    // (classify takes no Command path — this asserts the contract holds.)
    assert_eq!(classify(dir.path()), Some(Kind::JjColocated));
}
