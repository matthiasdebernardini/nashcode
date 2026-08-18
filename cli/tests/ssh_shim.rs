//! Everything server-side runs through the system `ssh`. `$NASHGIT_SSH_BIN`
//! is the seam: these tests point it at a shim that records its argv and its
//! stdin, so the generated remote scripts are checked end to end with no
//! network and no host.
//!
//! nextest runs each test in its own process, so setting the env var here
//! cannot leak into another test.

use nashgit_cli::remote::{self, Deploy};
use nashgit_cli::ssh::Ssh;
use std::fs;
use std::path::Path;

/// Write the fake `ssh`: append argv to `argv.log`, append stdin to
/// `script-<n>.log`, exit 0.
fn install_shim(dir: &Path) -> std::path::PathBuf {
    let shim = dir.join("fake-ssh");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\n\
             echo \"$@\" >> {log}/argv.log\n\
             n=$(ls {log} | grep -c '^script-' || true)\n\
             cat > {log}/script-$n.log\n\
             exit 0\n",
            log = dir.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    }
    shim
}

fn sample_deploy() -> Deploy {
    Deploy {
        bucket: "s3://example-cells".into(),
        endpoint: Some("https://t3.storage.dev".into()),
        region: "auto".into(),
        access_key_id: Some("AKIAEXAMPLE".into()),
        secret_access_key: Some("sup3r-s3cret".into()),
        session_token: None,
        site_name: "example-git".into(),
        site_desc: "a dgit host".into(),
        site_owner: "me".into(),
        token: "t0k3n-t0k3n".into(),
        dgit_dir: "/home/me/dgit".into(),
        service_user: "me".into(),
        listen: "127.0.0.1:8080".into(),
        celld_bin: "/home/me/.local/bin/celld".into(),
        esbuild_bin: "/usr/bin/esbuild".into(),
        viewer: false,
        viewer_port: 8090,
        https_port: 443,
        viewer_https_port: 8443,
    }
}

#[test]
fn the_install_script_runs_twice_and_sends_the_same_idempotent_script() {
    let dir = tempfile::tempdir().unwrap();
    let shim = install_shim(dir.path());
    unsafe { std::env::set_var("NASHGIT_SSH_BIN", &shim) };

    let ssh = Ssh::new("me@example-host");
    let script = remote::install_script();
    ssh.script(&script).unwrap();
    ssh.script(&script).unwrap(); // the re-run a resumed setup performs

    let first = fs::read_to_string(dir.path().join("script-0.log")).unwrap();
    let second = fs::read_to_string(dir.path().join("script-1.log")).unwrap();
    assert_eq!(first, second, "a re-run must send the identical script");
    assert_eq!(first, script);

    // Idempotence lives in the script itself: every install is guarded by a
    // presence check with an explicit skip path.
    for tool in ["git", "node", "esbuild", "tailscale", "celld"] {
        assert!(
            first.contains(&format!("skip {tool} (present")),
            "no skip path for {tool}"
        );
    }
    assert!(first.starts_with("set -eu\n"));
}

#[test]
fn scripts_travel_on_stdin_and_secrets_never_reach_argv() {
    let dir = tempfile::tempdir().unwrap();
    let shim = install_shim(dir.path());
    unsafe { std::env::set_var("NASHGIT_SSH_BIN", &shim) };

    let d = sample_deploy();
    let ssh = Ssh::new("me@example-host");
    ssh.script(&remote::deploy_script(&d)).unwrap();
    ssh.script(&remote::service_script(&d)).unwrap();
    ssh.script(&remote::verify_script(&d.token, &d.listen)).unwrap();

    let argv = fs::read_to_string(dir.path().join("argv.log")).unwrap();
    for line in argv.lines() {
        // The local ssh argv is only options + destination + `bash -s`.
        assert!(line.ends_with("me@example-host bash -s"), "unexpected argv: {line}");
        assert!(!line.contains("sup3r-s3cret"), "secret key in argv");
        assert!(!line.contains("t0k3n"), "push token in argv");
    }

    // The scripts (which travel on stdin, invisible to `ps`) do carry them.
    let sent = fs::read_to_string(dir.path().join("script-0.log")).unwrap();
    assert!(sent.contains("sup3r-s3cret"));
}

#[test]
fn dry_run_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let shim = install_shim(dir.path());
    unsafe { std::env::set_var("NASHGIT_SSH_BIN", &shim) };

    let ssh = Ssh::new("me@example-host").dry_run(true);
    let out = ssh.script(&remote::install_script()).unwrap();
    assert!(out.ok());
    assert!(
        !dir.path().join("argv.log").exists(),
        "dry-run must not spawn ssh at all"
    );
}
