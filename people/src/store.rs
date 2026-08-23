//! The disk and the network: where the file is, what state it is in, which viewer
//! to push to, and the two decisions the toolbar and the file watcher make.
//!
//! No gpui here. Everything a window would ask — may I save, may I push, the file
//! changed under me, what do I do — is a pure function of facts this module can
//! state, so all of it is provable without opening a window.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local};
use people_core::{PeopleFile, PushReply};
use serde::Deserialize;

/// What the window is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The first read has not landed yet.
    Loading,
    /// There is no file at the path. The window offers to make one.
    Missing,
    /// The file is there and will not parse. Nothing is editable: an editor that
    /// guessed at a broken file would save the guess over the original.
    Broken,
    /// A file is loaded and can be edited.
    Editing,
}

/// What a read of the path found.
///
/// No `Eq`: a loaded file can carry a hand-written JSON float. See
/// [`people_core::PeopleFile`].
#[derive(Debug, Clone, PartialEq)]
pub enum Disk {
    Missing,
    /// Unreadable or unparseable, with the reason as `people-core` worded it.
    Broken(String),
    Loaded(Box<PeopleFile>),
}

/// Read the file. A missing file is not an error: it is the state before the first
/// project exists, and the window has a button for it.
pub fn read(path: &Path) -> Disk {
    match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Disk::Missing,
        Err(error) => Disk::Broken(format!("{}: {error}", path.display())),
        Ok(text) => match PeopleFile::parse(&text) {
            Ok(file) => Disk::Loaded(Box::new(file)),
            Err(why) => Disk::Broken(why),
        },
    }
}

/// The file's modification time, or `None` when there is no file. Both answers are
/// ordinary: [`disk_change`] compares them and does not care which is which.
pub fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// What the watcher should do about a file whose modification time moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskChange {
    /// The file is where the window left it.
    Nothing,
    /// Take the new file: nothing would be lost.
    Reload,
    /// There are unsaved edits. Say so and let the operator choose; an editor that
    /// reloaded here would throw away work nobody agreed to lose.
    Ask,
}

pub fn disk_change(known: Option<SystemTime>, found: Option<SystemTime>, unsaved: bool) -> DiskChange {
    if known == found {
        DiskChange::Nothing
    } else if unsaved {
        DiskChange::Ask
    } else {
        DiskChange::Reload
    }
}

/// A toolbar command: whether it can run, and — when it cannot — the one sentence
/// that says why. A disabled button with no reason is a dead end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub enabled: bool,
    pub why: Option<String>,
}

impl Command {
    fn go() -> Self {
        Self { enabled: true, why: None }
    }

    fn no(why: impl Into<String>) -> Self {
        Self { enabled: false, why: Some(why.into()) }
    }
}

pub fn save_command(stage: Stage, unsaved: bool, busy: bool) -> Command {
    if busy {
        return Command::no("Something is already running");
    }
    match stage {
        Stage::Loading => Command::no("The file has not loaded yet"),
        Stage::Missing => Command::no("There is no file yet; create one first"),
        Stage::Broken => Command::no("The file will not parse; fix it by hand first"),
        Stage::Editing if !unsaved => Command::no("Nothing has changed"),
        Stage::Editing => Command::go(),
    }
}

/// Push sends what is on disk, so it waits for the save.
///
/// The alternative — pushing the edits in the window — would let the viewer answer
/// "which project" from a file that exists nowhere else, and the operator would have
/// no way to see what the viewer holds.
pub fn push_command(
    stage: Stage,
    unsaved: bool,
    busy: bool,
    viewer: Result<&Viewer, &str>,
) -> Command {
    if busy {
        return Command::no("Something is already running");
    }
    match stage {
        Stage::Loading => return Command::no("The file has not loaded yet"),
        Stage::Missing => return Command::no("There is no file yet; create one first"),
        Stage::Broken => return Command::no("The file will not parse; fix it by hand first"),
        Stage::Editing => {}
    }
    if let Err(why) = viewer {
        return Command::no(why);
    }
    if unsaved {
        return Command::no("Save first: the viewer holds a copy of the file on disk");
    }
    Command::go()
}

/// Why this file may not be written, in the file's own words.
///
/// A fatal finding is a file that will not load again — a duplicate id, a project
/// naming a person nobody has — so saving it would lock the operator out of their own
/// data through the only editor they have. A warning is a file that loads and routes
/// badly, which is theirs to judge and not the editor's to refuse.
pub fn refusals(file: &PeopleFile) -> Vec<String> {
    file.validate()
        .into_iter()
        .filter(|finding| finding.fatal)
        .map(|finding| finding.text)
        .collect()
}

/// The viewer to push to, and where the answer came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    /// Base URL, no trailing slash.
    pub base: String,
    pub token: Option<String>,
    /// What to tell the operator when they ask which viewer this is.
    pub source: String,
}

/// Which viewer, in order: `NASHCODE_URL`, then the CLI's active profile.
///
/// `NASHCODE_URL` deliberately carries no token: it can point anywhere, and the
/// profile's token belongs to the profile's server. A profile's token travels with
/// its own `viewer_url`.
pub fn viewer() -> Result<Viewer, String> {
    if let Ok(url) = std::env::var("NASHCODE_URL")
        && !url.trim().is_empty()
    {
        return Ok(Viewer {
            base: url.trim().trim_end_matches('/').to_owned(),
            token: None,
            source: "NASHCODE_URL".to_owned(),
        });
    }

    let path = config_path()?;
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!("No viewer: set NASHCODE_URL, or run `nashcode setup` ({} is not there)", path.display())
    })?;
    let store: ProfileStore = toml::from_str(&text)
        .map_err(|error| format!("{} will not parse: {error}", path.display()))?;
    let name = store
        .active
        .clone()
        .ok_or_else(|| format!("{} names no active profile; run `nashcode use <profile>`", path.display()))?;
    let profile = store
        .profiles
        .get(&name)
        .ok_or_else(|| format!("{} has no profile named `{name}`", path.display()))?;
    let base = profile
        .viewer_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| format!("Profile `{name}` has no viewer URL; set NASHCODE_URL"))?;

    Ok(Viewer {
        base: base.trim_end_matches('/').to_owned(),
        token: Some(profile.token.clone()).filter(|token| !token.trim().is_empty()),
        source: format!("profile `{name}`"),
    })
}

/// Where the CLI keeps its profiles. The same order `cli/src/profile.rs` uses; read
/// directly rather than through the CLI crate, because a desktop app should not pull
/// in a command-line tool to learn one URL.
fn config_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("NASHCODE_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("nashcode").join("config.toml"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "No home directory; set NASHCODE_URL or NASHCODE_CONFIG".to_owned())?;
    Ok(PathBuf::from(home).join(".config").join("nashcode").join("config.toml"))
}

/// Only the two fields this app needs out of the CLI's store.
#[derive(Debug, Default, Deserialize)]
struct ProfileStore {
    active: Option<String>,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileEntry {
    #[serde(default)]
    viewer_url: Option<String>,
    #[serde(default)]
    token: String,
}

/// Send the file. Blocking; the caller runs it off the main thread.
pub fn push(viewer: &Viewer, file: &PeopleFile) -> Result<PushReply, String> {
    people_core::push(&viewer.base, viewer.token.as_deref(), file)
}

/// When the viewer's copy last arrived. Blocking; the caller runs it off the main
/// thread.
pub fn pushed_at(viewer: &Viewer) -> Result<Option<String>, String> {
    people_core::pushed_at(&viewer.base)
}

/// A wall-clock time for the status line: the date only when it is not today, so a
/// line the operator reads twenty times a day stays short.
pub fn short_time(at: DateTime<Local>) -> String {
    if at.date_naive() == Local::now().date_naive() {
        at.format("%H:%M:%S").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// The same, for the RFC3339 the viewer answers with. An unparseable stamp is shown
/// as it came: it is the viewer's word, and hiding it would hide the disagreement.
pub fn short_stamp(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(at) => short_time(at.with_timezone(&Local)),
        Err(_) => rfc3339.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;
    use std::time::Duration;

    fn viewer_fixture() -> Viewer {
        Viewer { base: "https://viewer.example".into(), token: None, source: "a test".into() }
    }

    #[test]
    fn an_untouched_file_that_moved_on_disk_is_reloaded_without_asking() {
        let then = SystemTime::UNIX_EPOCH;
        let now = then + Duration::from_secs(1);
        assert_eq!(disk_change(Some(then), Some(now), false), DiskChange::Reload);
    }

    #[test]
    fn a_file_that_moved_under_unsaved_edits_asks_rather_than_discarding_them() {
        let then = SystemTime::UNIX_EPOCH;
        let now = then + Duration::from_secs(1);
        assert_eq!(disk_change(Some(then), Some(now), true), DiskChange::Ask);
    }

    #[test]
    fn a_file_that_did_not_move_is_left_alone_however_the_window_stands() {
        let then = Some(SystemTime::UNIX_EPOCH);
        assert_eq!(disk_change(then, then, true), DiskChange::Nothing);
        assert_eq!(disk_change(then, then, false), DiskChange::Nothing);
        // A file that was never there and still is not.
        assert_eq!(disk_change(None, None, true), DiskChange::Nothing);
        // One that appeared, or was deleted, is a change like any other.
        assert_eq!(disk_change(None, then, false), DiskChange::Reload);
        assert_eq!(disk_change(then, None, true), DiskChange::Ask);
    }

    #[test]
    fn save_is_offered_only_when_there_is_a_loaded_file_with_a_change_in_it() {
        assert!(save_command(Stage::Editing, true, false).enabled);
        for refused in [
            save_command(Stage::Editing, false, false),
            save_command(Stage::Editing, true, true),
            save_command(Stage::Loading, true, false),
            save_command(Stage::Missing, true, false),
            save_command(Stage::Broken, true, false),
        ] {
            assert!(!refused.enabled);
            assert!(refused.why.is_some(), "a disabled command has to say why");
        }
    }

    #[test]
    fn push_waits_for_the_save_and_for_a_viewer_to_push_to() {
        let viewer = viewer_fixture();
        assert!(push_command(Stage::Editing, false, false, Ok(&viewer)).enabled);

        let unsaved = push_command(Stage::Editing, true, false, Ok(&viewer));
        assert!(!unsaved.enabled);
        assert!(unsaved.why.expect("a reason").contains("Save first"));

        let nowhere = push_command(Stage::Editing, false, false, Err("No viewer: set NASHCODE_URL"));
        assert!(!nowhere.enabled);
        assert_eq!(nowhere.why.as_deref(), Some("No viewer: set NASHCODE_URL"));

        assert!(!push_command(Stage::Broken, false, false, Ok(&viewer)).enabled);
        assert!(!push_command(Stage::Editing, false, true, Ok(&viewer)).enabled);
    }

    #[test]
    fn a_missing_file_reads_as_missing_and_a_broken_one_says_why() {
        let dir = std::env::temp_dir().join(format!("nashcode-people-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let path = dir.join("people.json");

        assert_eq!(read(&path), Disk::Missing);
        assert_eq!(mtime(&path), None);

        std::fs::write(&path, "{ not json").expect("write");
        assert!(matches!(read(&path), Disk::Broken(_)));

        std::fs::write(&path, r#"{"people":[],"projects":[]}"#).expect("write");
        assert_eq!(read(&path), Disk::Loaded(Box::default()));
        assert!(mtime(&path).is_some());

        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn a_read_that_lands_after_a_keystroke_asks_instead_of_overwriting_it() {
        // The watcher takes this decision twice: once on the poll, and once when the
        // background read lands. Between the two, somebody can type. The facts are
        // the same file and the same stamp; the only thing that moved is `unsaved`,
        // and it is what turns a silent reload into a question.
        let known = Some(SystemTime::UNIX_EPOCH);
        let found = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        assert_eq!(disk_change(known, found, false), DiskChange::Reload);
        assert_eq!(disk_change(known, found, true), DiskChange::Ask);
    }

    #[test]
    fn a_fatal_finding_stops_the_save_and_a_warning_does_not() {
        // Fatal: a project naming somebody no person is. The file would save and
        // then never parse again, and this window is the only way back into it.
        let mut broken = Edit::default();
        let project = broken.add_project();
        broken.add_to_project(&project, "ghost");
        let refused = refusals(&broken.to_file());
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains("ghost"), "{refused:?}");

        // A warning: a person nothing can reach. The file loads, so it is the
        // operator's to judge — half a person typed in is not a reason to lose them.
        let mut thin = Edit::default();
        thin.add_person();
        let file = thin.to_file();
        assert!(!file.validate().is_empty(), "a person with no address is worth saying");
        assert_eq!(refusals(&file), Vec::<String>::new());
    }

    #[test]
    fn an_unparseable_push_stamp_is_shown_as_the_viewer_said_it() {
        assert_eq!(short_stamp("whenever"), "whenever");
        assert_eq!(short_stamp("2026-01-02T03:04:05Z").len(), "2026-01-02 03:04".len());
    }
}
