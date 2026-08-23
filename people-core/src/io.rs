//! Reading and writing: where the file is, and the shape the viewer stores.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::model::PeopleFile;

impl PeopleFile {
    /// Read the file from disk. A missing file is an error the caller words.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Self::parse(&text).map_err(|why| format!("{}: {why}", path.display()))
    }

    /// Write the file, whole or not at all.
    ///
    /// The desktop app saves while the router and the CLI read the same path, so the
    /// write goes to a temporary file beside the real one and is then renamed over it.
    /// A reader sees the old file or the new one, never half of either.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        write_atomically(path, &self.to_pretty_json())
    }
}

/// The viewer's copy: the file as pushed, and when it arrived.
///
/// The viewer stores the wrapper rather than the bare file so that `/brain` can say
/// how fresh its copy is without a second file to keep in step with the first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pushed {
    /// RFC3339, the moment of the push.
    pub pushed_at: String,
    /// Who pushed it: the Tailscale login of the request, `local` for a request that
    /// carried no identity. A copy that answers for everyone should say whose it is.
    pub pushed_by: String,
    pub file: PeopleFile,
}

impl Pushed {
    /// The stored copy, or `None` when nothing has been pushed yet.
    ///
    /// An unreadable or half-written file reads as no copy: the answer to "which
    /// project" is then `404`, which a caller already handles, rather than a `500`
    /// nobody planned for.
    pub fn read(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        write_atomically(path, &text)
    }
}

/// Where the file is: `$NASHCODE_PEOPLE`, else `$HOME/.nashcode/people.json`.
pub fn default_path() -> PathBuf {
    match std::env::var("NASHCODE_PEOPLE") {
        Ok(path) if !path.trim().is_empty() => PathBuf::from(path.trim()),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
            PathBuf::from(home).join(".nashcode").join("people.json")
        }
    }
}

/// Counts the writes this process has started, so two of them never choose one
/// temporary name. The process id alone is not enough: a viewer writes from whichever
/// task took the request, and every one of them shares its pid.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// Write beside the target, then rename over it. The temporary name carries the
/// process id and a counter, so no two writers — in this process or another — can land
/// on one another's half-written file.
fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ticket = WRITES.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.{}.{ticket}.tmp", std::process::id()));
    std::fs::write(&temp, text).map_err(|error| format!("{}: {error}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|error| {
        // Leave nothing behind for the next run to trip over.
        let _ = std::fs::remove_file(&temp);
        format!("{}: {error}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_file_loads_back_unchanged() {
        let dir = std::env::temp_dir().join(format!("people-core-{}", std::process::id()));
        let path = dir.join("people.json");
        let file = crate::route::tests::fixture();
        file.save(&path).expect("the save writes");
        assert_eq!(PeopleFile::load(&path).expect("the load reads"), file);

        // And the wrapper the viewer keeps.
        let pushed = Pushed {
            pushed_at: "2026-08-23T12:00:00Z".to_owned(),
            pushed_by: "matthias@example.com".to_owned(),
            file,
        };
        pushed.write(&path).expect("the push writes");
        assert_eq!(Pushed::read(&path), Some(pushed));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two writers in one process — which is what a viewer serving two pushes is —
    /// must not share a temporary name. Every save either wins or is overwritten by a
    /// whole one; none of them fails, and none leaves a `.tmp` behind.
    #[test]
    fn two_writers_in_one_process_do_not_collide() {
        let dir = std::env::temp_dir().join(format!("people-core-race-{}", std::process::id()));
        let path = dir.join("people.json");
        let file = crate::route::tests::fixture();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let (path, file) = (path.clone(), file.clone());
                scope.spawn(move || file.save(&path).expect("every save writes"));
            }
        });
        assert_eq!(PeopleFile::load(&path).expect("the survivor is a whole file"), file);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("the directory is there")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "a temporary file outlived its write: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_pushed_reads_as_no_copy_rather_than_an_error() {
        assert_eq!(Pushed::read(Path::new("/no/such/people.json")), None);
    }

    #[test]
    fn a_missing_file_says_which_file() {
        let error = PeopleFile::load(Path::new("/no/such/people.json")).unwrap_err();
        assert!(error.starts_with("/no/such/people.json"), "{error}");
    }
}
