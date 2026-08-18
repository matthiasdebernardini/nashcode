//! `nashgit invite` end to end through the fake-ssh seam — but this shim
//! *executes* the script it receives, under a fake `$HOME` and a PATH of stub
//! binaries (sudo, node, celld, systemctl, curl). So these tests watch the
//! real behavior — the mapping file changing, GIT_TOKENS being regenerated —
//! not just the script text. No network, no host.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    /// Build the fake host: an executing ssh shim, stub bins, a fake home
    /// with `~/dgit`, and a profile store pointing at it. `curl_code` is what
    /// the auth probe will see (400 = accepted, 401 = rejected).
    fn new(curl_code: &str) -> Host {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let stubs = d.join("stubs");
        let home = d.join("home");
        fs::create_dir_all(stubs.join("")).unwrap();
        fs::create_dir_all(home.join("dgit")).unwrap();
        // A deployed host always has the wrangler config; the node stub does
        // not write it, so seed it.
        fs::write(home.join("dgit/wrangler.celld.jsonc"), "{}\n").unwrap();

        let write_bin = |name: &str, body: &str| {
            let p = stubs.join(name);
            fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            }
        };
        // sudo: drop -n/-E, run the command as-is.
        write_bin(
            "sudo",
            "while [ \"$1\" = -n ] || [ \"$1\" = -E ]; do shift; done\nexec \"$@\"\n",
        );
        // node is only called to patch wrangler vars: record the vars file.
        write_bin("node", &format!("cat \"$3\" >> {}/vars.log\necho >> {}/vars.log\n", d.display(), d.display()));
        write_bin("celld", "exit 0\n");
        write_bin("systemctl", "exit 0\n");
        write_bin("curl", &format!("printf '{curl_code}'\n"));

        // The ssh shim: record argv, then run the piped script here.
        let shim = d.join("fake-ssh");
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\necho \"$@\" >> {d}/argv.log\nHOME='{home}' PATH='{stubs}':\"$PATH\" exec bash -s\n",
                d = d.display(),
                home = home.display(),
                stubs = stubs.display(),
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fs::write(
            d.join("config.toml"),
            "active = \"box\"\n\n[profiles.box]\nurl = \"https://box.example.ts.net\"\n\
             ssh = \"me@example-host\"\ntoken = \"main-token\"\n\
             bucket = \"s3://example-cells\"\nregion = \"auto\"\n",
        )
        .unwrap();

        Host { dir }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_nashgit"))
            .args(args)
            .env("NASHGIT_CONFIG", self.dir.path().join("config.toml"))
            .env("NASHGIT_SSH_BIN", self.dir.path().join("fake-ssh"))
            .output()
            .unwrap()
    }

    fn mapping_path(&self) -> PathBuf {
        self.dir.path().join("home/git-invites.toml")
    }

    fn mapping(&self) -> String {
        fs::read_to_string(self.mapping_path()).unwrap_or_default()
    }

    fn token_of(&self, name: &str) -> String {
        self.mapping()
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name} = \"")))
            .map(|rest| rest.trim_end_matches('"').to_string())
            .unwrap_or_default()
    }

    /// The most recent GIT_TOKENS value handed to the wrangler patch.
    fn last_vars(&self) -> String {
        let log = fs::read_to_string(self.dir.path().join("vars.log")).unwrap_or_default();
        log.lines().rev().find(|l| !l.is_empty()).unwrap_or("").to_string()
    }
}

fn json(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not one JSON value\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn invite_writes_the_mapping_and_regenerates_git_tokens() {
    let host = Host::new("400");

    let out = host.run(&["--json", "invite", "alice"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);

    // The mapping holds alice's token, 0600, and it matches what the CLI said.
    let alice = host.token_of("alice");
    assert_eq!(alice.len(), 48);
    assert_eq!(v["name"], "alice");
    assert_eq!(v["token"], alice.as_str());
    assert_eq!(v["probe"], "400");
    assert_eq!(v["verified"], true);
    assert!(v["credential_line"].as_str().unwrap().contains("git credential approve"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(host.mapping_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // GIT_TOKENS was regenerated from the mapping.
    assert_eq!(host.last_vars(), format!("{{\"GIT_TOKENS\": \"{alice}\"}}"));

    // A second invite joins the list, comma-separated.
    host.run(&["--json", "invite", "bob"]);
    let bob = host.token_of("bob");
    let vars = host.last_vars();
    assert!(vars.contains(&alice) && vars.contains(&bob), "{vars}");
    assert!(vars.contains(&format!("{alice},{bob}")) || vars.contains(&format!("{bob},{alice}")), "{vars}");

    // The ssh argv never carries a token; it travels on stdin.
    let argv = fs::read_to_string(host.dir.path().join("argv.log")).unwrap();
    assert!(!argv.contains(&alice) && !argv.contains(&bob), "{argv}");
    for line in argv.lines() {
        assert!(line.ends_with("me@example-host bash -s"), "{line}");
    }
}

#[test]
fn reinviting_a_name_rotates_that_persons_token() {
    let host = Host::new("400");
    host.run(&["--json", "invite", "alice"]);
    let first = host.token_of("alice");

    let out = host.run(&["--json", "invite", "alice"]);
    assert!(out.status.success());
    let second = host.token_of("alice");

    assert_ne!(first, second, "re-invite must rotate the token");
    let mapping = host.mapping();
    assert_eq!(
        mapping.lines().filter(|l| l.starts_with("alice = ")).count(),
        1,
        "one line per person: {mapping}"
    );
    // Only the new token is live.
    let vars = host.last_vars();
    assert!(vars.contains(&second) && !vars.contains(&first), "{vars}");
}

#[test]
fn revoke_removes_the_name_and_verifies_the_token_is_dead() {
    let host = Host::new("400");
    host.run(&["--json", "invite", "alice"]);
    host.run(&["--json", "invite", "bob"]);
    let alice = host.token_of("alice");
    let bob = host.token_of("bob");

    // From here the probe answers 401: the revoked token is rejected.
    let curl = host.dir.path().join("stubs/curl");
    fs::write(&curl, "#!/bin/sh\nprintf '401'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = host.run(&["--json", "invite", "--revoke", "alice"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["revoked"], "alice");
    assert_eq!(v["probe"], "401");
    assert_eq!(v["verified"], true);

    let mapping = host.mapping();
    assert!(!mapping.contains("alice"), "{mapping}");
    assert!(mapping.contains("bob"), "{mapping}");
    let vars = host.last_vars();
    assert!(vars.contains(&bob) && !vars.contains(&alice), "{vars}");

    // Revoking an unknown name is an error, not a silent no-op.
    let out = host.run(&["--json", "invite", "--revoke", "mallory"]);
    assert!(!out.status.success());
    let v = json(&out);
    assert!(v["error"].as_str().unwrap().contains("revoke"), "{v}");
}

#[test]
fn list_prints_names_and_never_token_material() {
    let host = Host::new("400");
    host.run(&["--json", "invite", "alice"]);
    host.run(&["--json", "invite", "bob"]);
    let alice = host.token_of("alice");
    let bob = host.token_of("bob");

    let out = host.run(&["--json", "invite", "--list"]);
    assert!(out.status.success());
    let v = json(&out);
    assert_eq!(v, serde_json::json!({ "names": ["alice", "bob"] }));

    let human = host.run(&["invite", "--list"]);
    let text = String::from_utf8_lossy(&human.stdout).to_string()
        + &String::from_utf8_lossy(&human.stderr);
    assert!(text.contains("alice") && text.contains("bob"), "{text}");
    for tok in [&alice, &bob] {
        assert!(!text.contains(tok.as_str()), "token leaked by --list");
    }

    // An empty mapping lists cleanly too.
    fs::remove_file(host.mapping_path()).unwrap();
    let v = json(&host.run(&["--json", "invite", "--list"]));
    assert_eq!(v["names"].as_array().unwrap().len(), 0);
}

#[test]
fn a_failed_probe_fails_the_invite() {
    // The server rejects the new token (401 where 400 was expected): the
    // invite must report failure, not print a token that does not work.
    let host = Host::new("401");
    let out = host.run(&["--json", "invite", "alice"]);
    assert!(!out.status.success());
    let v = json(&out);
    assert!(v["error"].as_str().unwrap().contains("invite failed"), "{v}");
}

#[test]
fn bad_arguments_and_bad_profiles_fail_clearly() {
    let host = Host::new("400");

    // No action.
    let out = host.run(&["--json", "invite"]);
    assert!(!out.status.success());
    assert!(json(&out)["error"].as_str().unwrap().contains("--list"), "usage hint expected");

    // Conflicting actions are a clap error before anything runs.
    let out = host.run(&["invite", "alice", "--list"]);
    assert!(!out.status.success());

    // A bad name never reaches the host.
    let out = host.run(&["--json", "invite", "not a name"]);
    assert!(!out.status.success());
    assert!(!host.mapping_path().exists());

    // A profile with no ssh destination cannot invite.
    fs::write(
        host.dir.path().join("config.toml"),
        "active = \"box\"\n\n[profiles.box]\nurl = \"https://box.example.ts.net\"\n",
    )
    .unwrap();
    let out = host.run(&["--json", "invite", "alice"]);
    assert!(!out.status.success());
    assert!(json(&out)["error"].as_str().unwrap().contains("ssh"));
}
