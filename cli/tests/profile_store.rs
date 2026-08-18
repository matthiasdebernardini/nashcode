//! The profile store round-trips through its TOML file, keeps the token under
//! 0600, and resolves `--profile` overrides the way every command relies on.

use nashgit_cli::profile::{Profile, Store};

fn sample() -> Profile {
    Profile {
        url: "https://box.example.ts.net".into(),
        ssh: "me@box".into(),
        token: "deadbeef".into(),
        viewer_url: Some("https://box.example.ts.net:8443".into()),
        listen_port: Some(9944),
        provider: Some("tigris".into()),
        bucket: Some("s3://example-cells".into()),
        endpoint: Some("https://t3.storage.dev".into()),
        region: Some("auto".into()),
        site_name: Some("example-git".into()),
        site_owner: Some("me".into()),
    }
}

#[test]
fn a_store_survives_the_disk_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nashgit").join("config.toml");

    let mut store = Store::default();
    store.insert("box", sample());
    store.insert("other", Profile { url: "https://o".into(), ..Default::default() });
    store.set_active("other").unwrap();
    store.save_to(&path).unwrap();

    let back = Store::load_from(&path).unwrap();
    assert_eq!(back, store);
    assert_eq!(back.active.as_deref(), Some("other"));
    assert_eq!(back.profiles["box"], sample());
}

#[cfg(unix)]
#[test]
fn the_file_is_0600_and_the_directory_0700() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nashgit").join("config.toml");
    let mut store = Store::default();
    store.insert("box", sample());
    store.save_to(&path).unwrap();

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
}

#[test]
fn saving_replaces_atomically_and_leaves_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nashgit").join("config.toml");

    let mut store = Store::default();
    store.insert("box", sample());
    store.save_to(&path).unwrap();

    // Overwrite with a different store: the new content lands whole.
    let mut second = Store::default();
    second.insert("other", Profile { url: "https://o".into(), ..Default::default() });
    second.save_to(&path).unwrap();
    assert_eq!(Store::load_from(&path).unwrap(), second);

    // Only the store itself remains — the temp file was renamed into place,
    // so a crash mid-write could never have truncated the real file.
    let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["config.toml"], "{names:?}");
}

#[test]
fn resolve_prefers_the_override_and_reports_missing_names() {
    let mut store = Store::default();
    store.insert("box", sample()); // first insert becomes active
    store.insert("two", Profile::default());

    let (name, _) = store.resolve(None).unwrap();
    assert_eq!(name, "box");
    let (name, _) = store.resolve(Some("two")).unwrap();
    assert_eq!(name, "two");

    let err = store.resolve(Some("nope")).unwrap_err().to_string();
    assert!(err.contains("nope"), "{err}");

    let empty = Store::default();
    let err = empty.resolve(None).unwrap_err().to_string();
    assert!(err.contains("nashgit setup"), "{err}");
}

#[test]
fn a_non_default_listen_port_round_trips_and_an_old_profile_defaults_to_8080() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut store = Store::default();
    store.insert("box", sample()); // listen_port = Some(9944)
    store.save_to(&path).unwrap();
    let back = Store::load_from(&path).unwrap();
    assert_eq!(back.profiles["box"].listen_port, Some(9944));
    assert_eq!(back.profiles["box"].listen_port(), 9944);

    // A profile written before the field existed still parses, and the
    // accessor answers 8080.
    std::fs::write(
        &path,
        "active = \"old\"\n\n[profiles.old]\nurl = \"https://o.example\"\n",
    )
    .unwrap();
    let old = Store::load_from(&path).unwrap();
    assert_eq!(old.profiles["old"].listen_port, None);
    assert_eq!(old.profiles["old"].listen_port(), 8080);
}

#[test]
fn a_missing_file_loads_as_an_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::load_from(&dir.path().join("absent.toml")).unwrap();
    assert!(store.profiles.is_empty());
    assert!(store.active.is_none());
}
