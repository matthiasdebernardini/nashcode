//! `nashcode people` end to end: the real binary, a real file on disk, and — for the
//! push — a canned viewer answer served from a listener on 127.0.0.1 that this test
//! owns. Nothing here needs a network or a host.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// What the CLI actually sent: the request line and the body.
struct Sent {
    line: String,
    body: String,
}

/// Serve one canned answer to exactly one request on a loopback port, and hand the
/// request back to the test.
fn one_shot(status: &'static str, body: &'static str) -> (u16, std::thread::JoinHandle<Sent>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let mut raw = Vec::new();
        let mut head_end = None;
        let mut length = 0usize;
        loop {
            let n = stream.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..n]);
            if head_end.is_none()
                && let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n")
            {
                head_end = Some(at + 4);
                let head = String::from_utf8_lossy(&raw[..at]).to_lowercase();
                length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
            }
            match head_end {
                Some(at) if raw.len() >= at + length => break,
                _ if n == 0 => break,
                _ => {}
            }
        }
        let at = head_end.unwrap_or(raw.len());
        let sent = Sent {
            line: String::from_utf8_lossy(&raw).lines().next().unwrap_or_default().to_string(),
            body: String::from_utf8_lossy(&raw[at..]).to_string(),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        sent
    });
    (port, handle)
}

fn write_config(dir: &std::path::Path, viewer: u16) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "active = \"test\"\n\n[profiles.test]\nurl = \"http://127.0.0.1:1\"\n\
             token = \"tt\"\nviewer_url = \"http://127.0.0.1:{viewer}\"\n"
        ),
    )
    .unwrap();
    path
}

fn nashcode(config: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(args)
        .env("NASHCODE_CONFIG", config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

/// The same, with `PATH` replaced: `suggest` shells out to `imsg` and `gws`, and no
/// test of it may touch the real Messages database or the real mailbox.
fn nashcode_with_path(
    config: &std::path::Path,
    path: &str,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nashcode"))
        .args(args)
        .env("NASHCODE_CONFIG", config)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

/// A shell script on a `PATH` of its own, standing in for a binary this machine may
/// or may not have.
fn stub(dir: &std::path::Path, name: &str, script: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn envelope(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON value ({e})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

const PEOPLE: &str = r#"{
  "me": ["matthias@example.com"],
  "people": [
    { "id": "rob", "name": "Rob Castro",
      "phones": ["+15550001111"], "emails": ["rob@example.com"] },
    { "id": "joey", "name": "Joey Locker",
      "phones": ["+15550002222"], "emails": ["joey@example.com"] }
  ],
  "projects": [
    { "id": "agstaff", "name": "agstaff", "folder": "~/Projects/agstaff",
      "repo": "agstaff", "people": ["rob", "joey"] }
  ]
}"#;

fn people_file(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("people.json");
    std::fs::write(&path, PEOPLE).unwrap();
    path
}

#[test]
fn route_ranks_from_the_file_on_this_machine_and_names_who_matched() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let file = people_file(temp.path());

    // Both flags repeat, which the flag map alone could not carry.
    let out = nashcode(
        &config,
        &[
            "people",
            "route",
            "--email",
            "rob@example.com",
            "--email",
            "joey@example.com",
            "--file",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    let hit = &value["result"]["matches"][0];
    assert_eq!(hit["project"], "agstaff");
    assert_eq!(hit["score"], 2);
    assert_eq!(hit["people"], serde_json::json!(["rob", "joey"]));
    assert_eq!(value["result"]["tie"], false);

    // The line a person reads is on stderr, where no parser trips over it.
    let notes = String::from_utf8_lossy(&out.stderr);
    assert!(notes.contains("agstaff — Rob Castro, Joey Locker"), "{notes}");
}

#[test]
fn asking_about_nobody_is_a_usage_error_before_anything_is_read() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let out = nashcode(&config, &["people", "route", "--file", "/no/such/people.json"]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    let value = envelope(&out);
    assert_eq!(value["error"]["code"], "USAGE");
}

#[test]
fn check_is_quiet_on_a_good_file_and_names_every_finding_on_a_bad_one() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let good = people_file(temp.path());

    let out = nashcode(&config, &["people", "check", "--file", good.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    assert_eq!(value["result"]["ok"], true);
    assert_eq!(value["result"]["findings"], serde_json::json!([]));

    // A dangling id and a phone that is not E.164: one refusal, one warning, and a
    // non-zero exit for either.
    let bad = temp.path().join("bad.json");
    std::fs::write(
        &bad,
        r#"{ "people": [ { "id": "rob", "name": "Rob Castro", "phones": ["555-0000"] } ],
             "projects": [ { "id": "agstaff", "people": ["rob", "joey"] } ] }"#,
    )
    .unwrap();
    let out = nashcode(&config, &["people", "check", "--file", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    let message = envelope(&out)["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.contains("refused: project \"agstaff\""), "{message}");
    assert!(message.contains("warning:") && message.contains("E.164"), "{message}");
}

#[test]
fn push_puts_the_file_at_the_viewer() {
    let temp = tempfile::tempdir().unwrap();
    let (port, server) = one_shot(
        "200 OK",
        r#"{"ok":true,"people":2,"projects":1,"pushed_at":"2026-08-23T12:00:00.000000Z"}"#,
    );
    let config = write_config(temp.path(), port);
    let file = people_file(temp.path());

    let out = nashcode(&config, &["people", "push", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let sent = server.join().unwrap();
    assert_eq!(sent.line, "PUT /people HTTP/1.1", "{}", sent.line);
    assert!(sent.body.contains("\"id\": \"rob\""), "the body is the file: {}", sent.body);

    let value = envelope(&out);
    assert_eq!(value["result"]["people"], 2);
    assert_eq!(value["result"]["projects"], 1);
    assert_eq!(value["result"]["pushed_at"], "2026-08-23T12:00:00.000000Z");
}

#[test]
fn a_viewer_that_refuses_the_file_says_why_and_exits_two() {
    let temp = tempfile::tempdir().unwrap();
    let (port, server) = one_shot("400 Bad Request", r#"{"error":"project \"x\" lists nobody"}"#);
    let config = write_config(temp.path(), port);
    let file = people_file(temp.path());

    let out = nashcode(&config, &["people", "push", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    let _ = server.join();
    let message = envelope(&out)["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.contains("lists nobody"), "{message}");
}

#[test]
fn import_builds_a_file_from_the_two_old_lists_and_writes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);

    let routes = temp.path().join("routes.json");
    std::fs::write(
        &routes,
        r#"{ "enrichment": {}, "routes": [
             { "name": "agstaff",
               "participants": ["+15550001111", "+15550002222", " +15550001111 "],
               "chat_ids": ["chat123"], "folder": "~/Projects/agstaff/imsg-inbox",
               "media_only": false, "enrich": true, "prompt": "file it" },
             { "name": "Brad Thompson", "participants": ["REPLACE_WITH_BRAD_NUMBER_E164"],
               "chat_ids": [], "folder": "~/Projects/PristineAcres/imsg-inbox",
               "media_only": true, "enrich": false, "prompt": "" },
             { "name": "agstaff media", "participants": ["+15550004444"],
               "chat_ids": [], "folder": "~/Projects/agstaff/media-inbox",
               "media_only": true, "enrich": true, "prompt": "" } ] }"#,
    )
    .unwrap();
    let context = temp.path().join("context.toml");
    std::fs::write(
        &context,
        "[[source]]\nrepo = \"agstaff\"\naccount = \"matthias@example.com\"\n\
         query = \"from:rob@example.com\"\n",
    )
    .unwrap();

    let out = nashcode(
        &config,
        &[
            "people",
            "import",
            "--routes",
            routes.to_str().unwrap(),
            "--context",
            context.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let file = envelope(&out)["result"]["file"].clone();

    let projects = file["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 3);
    // The route folder is the inbox; the project is the folder above it.
    assert_eq!(projects[0]["id"], "agstaff");
    assert_eq!(projects[0]["folder"], "~/Projects/agstaff");
    assert_eq!(projects[0]["repo"], "agstaff");
    // The third number is the first one again, spaced differently: one person.
    assert_eq!(projects[0]["people"], serde_json::json!(["agstaff-1", "agstaff-2"]));
    assert_eq!(projects[0]["chat_ids"], serde_json::json!(["chat123"]));
    assert_eq!(projects[0]["imsg"]["prompt"], "file it");
    // The mail account comes from context.toml, matched by project id.
    assert_eq!(projects[0]["email"]["account"], "matthias@example.com");
    assert_eq!(projects[0]["email"]["query"], "from:rob@example.com");
    assert_eq!(projects[1]["id"], "pristineacres", "a folder name becomes an id");
    assert_eq!(projects[1]["repo"], "PristineAcres", "the repo is the folder as spelled");
    assert_eq!(projects[1]["imsg"]["media_only"], true);
    assert_eq!(projects[1]["imsg"]["enrich"], false);
    assert!(projects[1]["email"]["account"].is_null(), "no source, no account");
    // Two routes inside one project folder: the second id is its own.
    assert_eq!(projects[2]["id"], "agstaff-2");
    assert_eq!(projects[2]["repo"], "agstaff");
    assert_eq!(projects[2]["people"], serde_json::json!(["agstaff-2-1"]));

    // routes.json knows numbers, not names, so every name is empty and said so.
    let people = file["people"].as_array().unwrap();
    assert_eq!(people.len(), 4);
    assert_eq!(people[0]["id"], "agstaff-1");
    assert_eq!(people[0]["phones"], serde_json::json!(["+15550001111"]));
    assert_eq!(people[0]["name"], "");
    let notes = String::from_utf8_lossy(&out.stderr);
    assert!(notes.contains("agstaff-1, agstaff-2, pristineacres-1"), "{notes}");

    // The placeholder nobody filled in is named, in the envelope and on stderr.
    let findings = envelope(&out)["result"]["findings"].as_array().unwrap().clone();
    let placeholder = findings
        .iter()
        .find(|f| f["text"].as_str().unwrap().contains("REPLACE_WITH_BRAD_NUMBER_E164"))
        .unwrap_or_else(|| panic!("no finding about the placeholder: {findings:?}"));
    assert_eq!(placeholder["fatal"], false);
    assert!(placeholder["text"].as_str().unwrap().contains("E.164"), "{placeholder}");
    assert!(notes.contains("REPLACE_WITH_BRAD_NUMBER_E164"), "{notes}");

    // Nothing was written: the operator saves the result themselves.
    assert!(!temp.path().join("people.json").exists());
}

#[test]
fn a_repeated_flag_with_no_value_does_not_swallow_the_next_flag() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let file = people_file(temp.path());

    // `--email --json` names no address. Reading `--json` as one would search for an
    // attendee called "--json" and answer "nobody knows them", which reads like a
    // fact about the file rather than a typo.
    let out = nashcode(
        &config,
        &["people", "route", "--email", "--json", "--file", file.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    let value = envelope(&out);
    assert_eq!(value["ok"], false, "{value}");
}

#[test]
fn sync_folders_adds_one_project_per_client_folder_and_writes_only_when_told() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let file = people_file(temp.path());

    let clients = temp.path().join("clients");
    for name in ["Blue Barn", "acres-backups"] {
        std::fs::create_dir_all(clients.join(name)).unwrap();
    }
    std::fs::create_dir_all(clients.join("Blue Barn").join(".git")).unwrap();
    std::fs::write(
        clients.join("Blue Barn").join(".git").join("config"),
        "[remote \"origin\"]\n\turl = https://nashcode.example.ts.net/blue-barn\n",
    )
    .unwrap();

    // A skip list has to come from the file, so put one there.
    let mut written: serde_json::Value = serde_json::from_str(PEOPLE).unwrap();
    written["skip"] = serde_json::json!(["*-backups"]);
    std::fs::write(&file, serde_json::to_string_pretty(&written).unwrap()).unwrap();

    // Dry run first: it says what it would do and leaves the file alone.
    let before = std::fs::read_to_string(&file).unwrap();
    let out = nashcode(
        &config,
        &["people", "sync-folders", clients.to_str().unwrap(), "--file", file.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    assert_eq!(value["result"]["written"], false);
    assert_eq!(value["result"]["added"][0]["id"], "blue-barn");
    assert_eq!(value["result"]["added"][0]["name"], "Blue Barn");
    assert_eq!(value["result"]["added"][0]["repo"], "blue-barn");
    assert_eq!(value["result"]["skipped"], serde_json::json!(["acres-backups"]));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "a dry run wrote to the file");

    // And with --write the project is in the file, beside the one already there.
    let out = nashcode(
        &config,
        &[
            "people",
            "sync-folders",
            clients.to_str().unwrap(),
            "--write",
            "--file",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(envelope(&out)["result"]["written"], true);
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let ids: Vec<&str> =
        saved["projects"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["agstaff", "blue-barn"]);
    assert_eq!(saved["skip"], serde_json::json!(["*-backups"]), "the skip list survived the save");

    // Run it again: nothing new, nothing lost.
    let out = nashcode(
        &config,
        &[
            "people",
            "sync-folders",
            clients.to_str().unwrap(),
            "--write",
            "--file",
            file.to_str().unwrap(),
        ],
    );
    let value = envelope(&out);
    assert_eq!(value["result"]["added"], serde_json::json!([]));
    assert_eq!(value["result"]["kept"], 1);
}

#[test]
fn suggest_reads_messages_and_gmail_and_sends_only_the_project_name_to_the_search() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let file = people_file(temp.path());

    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // One chat that names the project, one that does not. Rob's number is already on
    // a person, so only the stranger is a candidate.
    stub(
        &bin,
        "imsg",
        r#"cat <<'NDJSON'
{"id":"7","display_name":"AgStaff crew","participants":["+15550001111","+15550003333"],"last_message_at":"2026-08-22T18:04:00Z"}
{"id":"8","display_name":"Book club","participants":["+15550009999"],"last_message_at":"2026-08-01T10:00:00Z"}
NDJSON"#,
    );
    // Every argument gws was given, so the test can prove what left the machine.
    let log = temp.path().join("gws.log");
    stub(
        &bin,
        "gws",
        &format!(
            r#"printf '%s\n' "$*" >> {log}
case "$4" in
  list) echo '{{"messages":[{{"id":"m1"}}]}}' ;;
  get) echo '{{"payload":{{"headers":[{{"name":"From","value":"Dana Ruiz <dana@example.com>"}},{{"name":"Date","value":"Tue, 4 Aug 2026 09:12:00 -0500"}}]}}}}' ;;
esac"#,
            log = log.display()
        ),
    );
    let path = format!("{}:/usr/bin:/bin", bin.display());

    let out = nashcode_with_path(
        &config,
        &path,
        &["people", "suggest", "--file", file.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    let candidates = value["result"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2, "{value}");

    assert_eq!(candidates[0]["project"], "agstaff");
    assert_eq!(candidates[0]["phone"], "+15550003333");
    assert_eq!(candidates[0]["where_seen"], "Messages chat \"AgStaff crew\"");
    assert_eq!(candidates[0]["last"], "2026-08-22T18:04:00Z");

    assert_eq!(candidates[1]["name"], "Dana Ruiz");
    assert_eq!(candidates[1]["email"], "dana@example.com");
    assert!(
        candidates[1]["where_seen"].as_str().unwrap().starts_with("Gmail: message from Tue, 4 Aug"),
        "{value}"
    );
    assert_eq!(value["result"]["gmail_messages_per_project"], 25);

    // What actually left this machine: the project's name, a date bound, and nothing
    // out of the file.
    let sent = std::fs::read_to_string(&log).unwrap();
    assert!(sent.contains("agstaff newer_than:365d"), "{sent}");
    assert!(sent.contains("\"maxResults\":25"), "{sent}");
    assert!(!sent.contains("+1555"), "a phone number reached the search: {sent}");
    assert!(!sent.contains("rob@example.com"), "an address reached the search: {sent}");

    // And nothing was written: accepting a suggestion is the operator's act.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), PEOPLE);
}

#[test]
fn suggest_without_the_tools_says_so_and_still_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let file = people_file(temp.path());

    let empty = temp.path().join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let out = nashcode_with_path(
        &config,
        empty.to_str().unwrap(),
        &["people", "suggest", "--file", file.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(envelope(&out)["result"]["candidates"], serde_json::json!([]));
    let notes = String::from_utf8_lossy(&out.stderr);
    assert!(notes.contains("imsg did not run"), "{notes}");
    assert!(notes.contains("gws did not run"), "{notes}");
}

#[test]
fn seen_bumps_the_file_and_ls_puts_the_warmest_first() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let file = temp.path().join("people.json");
    std::fs::write(
        &file,
        r#"{
          "people": [
            { "id": "rob", "name": "Rob Castro", "emails": ["rob@example.com"] },
            { "id": "joey", "name": "Joey Locker", "emails": ["joey@example.com"] }
          ],
          "projects": [
            { "id": "cold", "name": "cold", "folder": "~/x/cold", "people": ["rob"] },
            { "id": "warm", "name": "warm", "folder": "~/x/warm", "people": ["rob", "joey"] }
          ]
        }"#,
    )
    .unwrap();

    let out = nashcode(&config, &["people", "seen", "warm", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    assert_eq!(value["result"]["project"], true);
    assert_eq!(value["result"]["person"], false);

    let out = nashcode(&config, &["people", "seen", "joey", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(envelope(&out)["result"]["person"], true);

    // The bump is in the file, and it is the only thing that changed.
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["projects"][1]["seen"]["count"], 1);
    assert!(saved["projects"][0]["seen"].is_null(), "the cold project was not touched");
    assert_eq!(saved["people"][1]["seen"]["count"], 1);

    // An id nobody has is not found, and it does not write.
    let out = nashcode(&config, &["people", "seen", "nobody", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(envelope(&out)["error"]["code"], "NOT_FOUND");

    // `ls` orders by warmth, not by the file: `warm` was written second and is first,
    // and inside it Joey is ahead of Rob for the same reason.
    let out = nashcode(&config, &["people", "ls", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    assert_eq!(value["result"]["projects"][0]["id"], "warm");
    assert_eq!(value["result"]["projects"][1]["id"], "cold");
    assert_eq!(value["result"]["projects"][0]["people"][0]["id"], "joey");

    let notes = String::from_utf8_lossy(&out.stderr);
    assert!(notes.contains("1× · now"), "the count and the age are on the line: {notes}");
    let warm = notes.find("warm [").expect("a line for warm");
    let cold = notes.find("cold [").expect("a line for cold");
    assert!(warm < cold, "the warmest project is printed first: {notes}");
}

#[test]
fn check_warns_when_signal_marks_a_number_that_is_not_there() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_config(temp.path(), 1);
    let file = temp.path().join("signal.json");
    std::fs::write(
        &file,
        r#"{ "people": [ { "id": "david", "name": "David Reed", "signal": true,
                           "emails": ["david@example.com"] } ],
             "projects": [ { "id": "p", "name": "p", "people": ["david"] } ] }"#,
    )
    .unwrap();

    let out = nashcode(&config, &["people", "check", "--file", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    let message = envelope(&out)["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.contains("signal: true") && message.contains("David Reed"), "{message}");
}
