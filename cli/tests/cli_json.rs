//! The envelope contract at the binary level: one JSON value on stdout, the
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

/// The one JSON value the run wrote to stdout.
fn envelope(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON value ({e})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn doctor_reports_the_check_shape_and_a_typed_exit_without_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("absent.toml");

    let out = nashcode(&config, &["--json", "doctor"]);
    // The report itself ran, so the envelope is ok; the missing profile is what
    // the exit code carries.
    assert_eq!(out.status.code(), Some(3));

    let v = envelope(&out);
    assert_eq!(v["ok"], true, "the report succeeded: {v}");
    assert_eq!(v["exit_code"], 3);
    assert_eq!(v["result"]["healthy"], false);

    let checks = v["result"]["checks"].as_array().unwrap();
    assert_eq!(checks[0]["name"], "profile");
    assert_eq!(checks[0]["status"], "fail");
    assert!(checks[0]["detail"].as_str().unwrap().contains("nashcode setup"));
    // A failure an agent can act on carries the command to run next.
    assert!(checks[0]["fix"].as_str().unwrap().contains("nashcode setup"));
    // Nothing after the profile could run, and a skip is never a pass.
    assert!(checks[1..].iter().all(|c| c["status"] == "skip"), "{v}");
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

    let v = envelope(&out);
    let loopback = v["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "loopback")
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

    let v = envelope(&nashcode(&config, &["--json", "profiles"]));
    assert_eq!(v["result"]["active"], "a");
    assert_eq!(v["result"]["profiles"].as_array().unwrap().len(), 2);

    let out = nashcode(&config, &["--json", "use", "b"]);
    assert!(out.status.success());
    assert_eq!(envelope(&out)["result"]["active"], "b");

    // --profile overrides the active selection without changing it.
    let v = envelope(&nashcode(&config, &["--json", "--profile", "a", "token"]));
    assert_eq!(v["result"]["token"], "aaaa");

    // The active profile (now b) holds no token: that is an error, not "".
    let out = nashcode(&config, &["--json", "token"]);
    assert_eq!(out.status.code(), Some(3), "a missing token is not found");
    let v = envelope(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "NOT_FOUND");
    assert!(v["error"]["message"].as_str().unwrap().contains("no token"));
    // Every error names the command to run next.
    assert!(v["fix"].as_str().unwrap().starts_with("nashcode "), "{v}");
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
        assert_eq!(out.status.code(), Some(2), "{args:?} is a bad invocation");
        let v = envelope(&out);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "USAGE", "{args:?}: {v}");
        assert!(
            v["error"]["message"].as_str().unwrap().contains("not a valid"),
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

    let v = envelope(&out);
    assert_eq!(v["result"]["relative"], "plans/replace-the-parser.md");
    let text = std::fs::read_to_string(repo.join("plans/replace-the-parser.md")).unwrap();
    assert!(text.starts_with("# Replace the Parser\n"));
    assert!(text.contains("## Steps"));

    // The plan is a dead end without a reviewer, so the trail carries on to one.
    let actions: Vec<&str> = v["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["command"].as_str())
        .collect();
    assert!(
        actions.contains(&"nashcode annotate plans/replace-the-parser.md"),
        "{actions:?}"
    );
    assert!(
        actions.contains(&"nashcode comments plans/replace-the-parser.md"),
        "{actions:?}"
    );

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
fn setup_dry_run_returns_the_scripts_and_writes_nothing() {
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

    // Stdout belongs to the envelope, so a preview is returned rather than
    // printed: every script the run would have executed, in order.
    let v = envelope(&out);
    assert_eq!(v["result"]["dry_run"], true);
    let scripts = v["result"]["scripts"].as_array().unwrap();
    let steps: Vec<&str> = scripts.iter().filter_map(|s| s["step"].as_str()).collect();
    assert!(steps.contains(&"preflight"), "{steps:?}");
    assert!(steps.contains(&"deploy"), "{steps:?}");
    assert!(steps.contains(&"tailscale up"), "{steps:?}");
    assert!(
        scripts
            .iter()
            .all(|s| s["script"].as_str().unwrap().contains("set -eu")),
        "every remote script is `set -eu`: unset is as fatal as failed"
    );
    assert!(!config.exists(), "dry run must not write a profile");
}

#[test]
fn every_missing_or_unusable_setup_answer_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");

    // SETUP_DOC promises that a missing answer is a usage error naming the
    // flag. These four are the ones that used to fall through to a generic 1.
    let cases: [(&[&str], &str); 4] = [
        // Credentials, with neither the flags nor the environment.
        (
            &["setup", "--dry-run", "--host", "me@h", "--provider", "tigris", "--bucket", "b"],
            "no bucket credentials",
        ),
        // An answer given as empty is an answer not given.
        (
            &["setup", "--dry-run", "--host", "me@h", "--provider", "tigris",
              "--bucket=", "--creds-on-host"],
            "--bucket is required",
        ),
        // A value systemd's unit file cannot carry.
        (
            &["setup", "--dry-run", "--host", "me@h", "--provider", "tigris",
              "--bucket", "has space", "--creds-on-host"],
            "cannot carry safely",
        ),
        // An endpoint R2 cannot default.
        (
            &["setup", "--dry-run", "--host", "me@h", "--provider", "r2",
              "--bucket", "b", "--creds-on-host"],
            "--endpoint is required",
        ),
    ];

    for (args, expected) in cases {
        let out = Command::new(env!("CARGO_BIN_EXE_nashcode"))
            .args(args)
            .env("NASHCODE_CONFIG", &config)
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let v = envelope(&out);
        assert_eq!(v["error"]["code"], "USAGE", "{args:?}: {v}");
        assert!(
            v["error"]["message"].as_str().unwrap().contains(expected),
            "{args:?}: {v}"
        );
        assert!(v["fix"].as_str().unwrap().starts_with("nashcode "), "{args:?}: {v}");
        assert!(!config.exists(), "{args:?} wrote a profile");
    }
}

#[test]
fn setup_names_the_flag_it_is_missing_rather_than_asking_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");

    // No --provider: nothing prompts any more, so this is a usage error whose
    // fix lists what to pass.
    let out = nashcode(&config, &["setup", "--host", "me@example-host", "--dry-run"]);
    assert_eq!(out.status.code(), Some(2));
    let v = envelope(&out);
    assert_eq!(v["error"]["code"], "USAGE");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("--provider is required"),
        "{v}"
    );
    assert!(v["fix"].as_str().unwrap().contains("--provider"), "{v}");
    assert!(!config.exists());
}
