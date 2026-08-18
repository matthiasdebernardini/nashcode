//! The `--json` contract at the binary level: one JSON value on stdout, the
//! documented shape, and the documented exit codes. No test here touches a
//! network, a host, or the user's real profile store.

use std::process::Command;

fn nashcode(config: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(args)
        .env("NASHCODE_CONFIG", config)
        .output()
        .unwrap()
}

#[test]
fn doctor_reports_the_check_shape_and_exits_nonzero_without_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("absent.toml");

    let out = nashcode(&config, &["--json", "doctor"]);
    assert_eq!(out.status.code(), Some(1));

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["failed"], 1);
    let checks = v["checks"].as_array().unwrap();
    assert_eq!(checks[0]["id"], "profile");
    assert_eq!(checks[0]["status"], "fail");
    assert!(checks[0]["detail"].as_str().unwrap().contains("nashcode setup"));
}

#[test]
fn doctor_probes_the_profiles_listen_port_not_a_hardcoded_one() {
    let dir = tempfile::tempdir().unwrap();

    // A fake ssh that records the doctor script it was sent.
    let shim = dir.path().join("fake-ssh");
    std::fs::write(
        &shim,
        format!("#!/bin/sh\ncat > {}/doctor-script.log\nexit 0\n", dir.path().display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // url points at a closed local port so the server checks fail fast
    // offline; the host checks are what this test is about.
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "active = \"box\"\n\n[profiles.box]\nurl = \"http://127.0.0.1:1\"\n\
         ssh = \"me@example-host\"\nlisten_port = 9944\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(["--json", "doctor"])
        .env("NASHCODE_CONFIG", &config)
        .env("NASHCODE_SSH_BIN", &shim)
        .output()
        .unwrap();

    let script = std::fs::read_to_string(dir.path().join("doctor-script.log")).unwrap();
    assert!(script.contains("http://127.0.0.1:9944/"), "{script}");
    assert!(!script.contains("8080"), "hardcoded port survives: {script}");

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let loopback = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "loopback")
        .expect("a loopback check");
    assert!(
        loopback["detail"].as_str().unwrap().contains("127.0.0.1:9944"),
        "{loopback}"
    );
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

    let out = nashcode(&config, &["--json", "profiles"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["active"], "a");
    assert_eq!(v["profiles"].as_array().unwrap().len(), 2);

    let out = nashcode(&config, &["--json", "use", "b"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["active"], "b");

    // --profile overrides the active selection without changing it.
    let out = nashcode(&config, &["--json", "--profile", "a", "token"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["token"], "aaaa");

    // The active profile (now b) holds no token: that is an error, not "".
    let out = nashcode(&config, &["--json", "token"]);
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["error"].as_str().unwrap().contains("no token"));
}

#[test]
fn a_bad_repo_name_dies_before_it_can_become_a_url_path() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "active = \"a\"\n\n[profiles.a]\nurl = \"https://a.example\"\ntoken = \"t\"\n",
    )
    .unwrap();

    // `x/config` would address dgit's admin endpoint, not a repository.
    for args in [
        vec!["--json", "rm", "x/config", "--yes"],
        vec!["--json", "gc", "x/config"],
        vec!["--json", "desc", "x/config", "--desc", "d"],
        vec!["--json", "clone", "../evil"],
    ] {
        let out = nashcode(&config, &args);
        assert!(!out.status.success(), "{args:?} should fail");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            v["error"].as_str().unwrap().contains("not a valid"),
            "{args:?}: {v}"
        );
    }
}

#[test]
fn plan_new_writes_the_template_inside_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("proj");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let config = dir.path().join("absent.toml"); // plan new needs no profile

    let out = Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(["--json", "plan", "new", "Replace", "the", "Parser"])
        .env("NASHCODE_CONFIG", &config)
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
    let out = Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(["plan", "new", "Replace the Parser"])
        .env("NASHCODE_CONFIG", &config)
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn setup_dry_run_prints_scripts_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");

    let out = nashcode(
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
