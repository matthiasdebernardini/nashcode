//! jj is shelled out behind the `$NASHCODE_JJ_BIN` / `$NASHCODE_GIT_BIN` seam.
//! These tests point both at recording shims and check which tool `nashcode`
//! reaches for in each repository layout: `jj git remote add` in a jj repo,
//! `git remote add` in a git one — same URL, same credential path either way.
//!
//! nextest gives each test its own process, so the env vars cannot leak.

use nashcode_cli::vcs;
use std::fs;
use std::path::Path;

fn shim(dir: &Path, name: &str, stdout: &str) -> std::path::PathBuf {
    let bin = dir.join(name);
    fs::write(
        &bin,
        format!(
            "#!/bin/sh\n\
             echo \"{name} $@\" >> {log}/calls.log\n\
             printf '%s' '{stdout}'\n\
             exit 0\n",
            name = name,
            log = dir.display(),
            stdout = stdout,
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn calls(dir: &Path) -> String {
    fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

#[test]
fn a_jj_repo_gets_jj_git_remote_add() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("myrepo");
    fs::create_dir_all(repo.join(".jj")).unwrap();
    fs::create_dir_all(repo.join(".git")).unwrap();

    // `remote list` answers empty, so set_origin must choose `add`.
    let jj = shim(dir.path(), "jj", "");
    let git = shim(dir.path(), "git", "");
    unsafe {
        std::env::set_var("NASHCODE_JJ_BIN", &jj);
        std::env::set_var("NASHCODE_GIT_BIN", &git);
    }

    let ws = vcs::detect(&repo).unwrap();
    let cmd = ws.set_origin("https://example-host/myrepo.git").unwrap();
    assert_eq!(cmd, "jj git remote add origin https://example-host/myrepo.git");

    let log = calls(dir.path());
    assert!(log.contains("jj git remote list"), "{log}");
    assert!(log.contains("jj git remote add origin https://example-host/myrepo.git"), "{log}");
    assert!(
        !log.lines().any(|l| l.starts_with("git ")),
        "plain git must not run: {log}"
    );
}

#[test]
fn a_git_repo_gets_git_remote_add() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("myrepo");
    fs::create_dir_all(repo.join(".git")).unwrap();

    let jj = shim(dir.path(), "jj", "");
    let git = shim(dir.path(), "git", "");
    unsafe {
        std::env::set_var("NASHCODE_JJ_BIN", &jj);
        std::env::set_var("NASHCODE_GIT_BIN", &git);
    }

    let ws = vcs::detect(&repo).unwrap();
    let cmd = ws.set_origin("https://example-host/myrepo.git").unwrap();
    assert_eq!(cmd, "git remote add origin https://example-host/myrepo.git");
    assert!(!calls(dir.path()).contains("jj "), "jj must not run in a git repo");
}

#[test]
fn an_existing_origin_is_replaced_not_duplicated() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("myrepo");
    fs::create_dir_all(repo.join(".jj")).unwrap();

    // `jj git remote list` output: "origin <url>" — so set-url is chosen.
    let jj = shim(dir.path(), "jj", "origin https://old/x.git\n");
    unsafe { std::env::set_var("NASHCODE_JJ_BIN", &jj) };

    let ws = vcs::detect(&repo).unwrap();
    let cmd = ws.set_origin("https://example-host/myrepo.git").unwrap();
    assert!(cmd.starts_with("jj git remote set-url origin"), "{cmd}");
}

#[test]
fn jj_availability_reads_the_test_seam_first() {
    unsafe { std::env::set_var("NASHCODE_JJ_AVAILABLE", "0") };
    assert!(!vcs::jj_available());
    unsafe { std::env::set_var("NASHCODE_JJ_AVAILABLE", "1") };
    assert!(vcs::jj_available());
}

#[test]
fn jj_is_opt_in_for_new_and_clone_but_default_for_init() {
    unsafe {
        std::env::remove_var("NASHCODE_JJ");
        std::env::set_var("NASHCODE_JJ_AVAILABLE", "1");
    }
    // new/clone: only the flag or the env var opt in — jj being installed
    // does not change what clone produces.
    assert!(!vcs::jj_requested(false));
    assert!(vcs::jj_requested(true));
    unsafe { std::env::set_var("NASHCODE_JJ", "1") };
    assert!(vcs::jj_requested(false));

    // init creates a working copy from nothing: there jj wins when present.
    unsafe { std::env::remove_var("NASHCODE_JJ") };
    assert!(vcs::prefer_jj(false, false));
    assert!(!vcs::prefer_jj(false, true)); // --git overrides
    unsafe { std::env::set_var("NASHCODE_JJ_AVAILABLE", "0") };
    assert!(!vcs::prefer_jj(false, false));
}
