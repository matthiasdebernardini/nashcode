//! The `--json` contract at the binary level: one JSON value on stdout, the
//! documented shape, and the documented exit codes. No test here touches a
//! network, a host, or the user's real profile store.

use std::process::Command;

fn nashgit(config: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nashgit"))
        .args(args)
        .env("NASHGIT_CONFIG", config)
        .output()
        .unwrap()
}

#[test]
fn doctor_reports_the_check_shape_and_exits_nonzero_without_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("absent.toml");

    let out = nashgit(&config, &["--json", "doctor"]);
    assert_eq!(out.status.code(), Some(1));

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["failed"], 1);
    let checks = v["checks"].as_array().unwrap();
    assert_eq!(checks[0]["id"], "profile");
    assert_eq!(checks[0]["status"], "fail");
    assert!(checks[0]["detail"].as_str().unwrap().contains("nashgit setup"));
}

#[test]
fn profiles_use_and_token_round_trip_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "active = \"a\"\n\n\
         [profiles.a]\nurl = \"https://a.example\"\ntoken = \"aaaa\"\n\n\
         [profiles.b]\nurl = \"https://b.example\"\n",
    )
    .unwrap();

    let out = nashgit(&config, &["--json", "profiles"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["active"], "a");
    assert_eq!(v["profiles"].as_array().unwrap().len(), 2);

    let out = nashgit(&config, &["--json", "use", "b"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["active"], "b");

    // --profile overrides the active selection without changing it.
    let out = nashgit(&config, &["--json", "--profile", "a", "token"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["token"], "aaaa");

    // The active profile (now b) holds no token: that is an error, not "".
    let out = nashgit(&config, &["--json", "token"]);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["error"].as_str().unwrap().contains("no token"));
}

#[test]
fn plan_new_writes_the_template_inside_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("proj");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let config = dir.path().join("absent.toml"); // plan new needs no profile

    let out = Command::new(env!("CARGO_BIN_EXE_nashgit"))
        .args(["--json", "plan", "new", "Replace", "the", "Parser"])
        .env("NASHGIT_CONFIG", &config)
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["relative"], "plans/replace-the-parser.md");
    let text = std::fs::read_to_string(repo.join("plans/replace-the-parser.md")).unwrap();
    assert!(text.starts_with("# Replace the Parser\n"));
    assert!(text.contains("## Steps"));

    // Re-running refuses to clobber the plan.
    let out = Command::new(env!("CARGO_BIN_EXE_nashgit"))
        .args(["plan", "new", "Replace the Parser"])
        .env("NASHGIT_CONFIG", &config)
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn setup_dry_run_prints_scripts_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");

    let out = nashgit(
        &config,
        &[
            "setup",
            "--dry-run",
            "--yes",
            "--host", "me@example-host",
            "--provider", "tigris",
            "--bucket", "example-cells",
            "--creds-on-host",
            "--site-name", "example-git",
            "--site-owner", "me",
        ],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("set -eu"), "dry run should print the scripts");
    assert!(text.contains("tailscale"), "{text}");
    assert!(!config.exists(), "dry run must not write a profile");
}
