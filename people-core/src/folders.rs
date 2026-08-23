//! The client folders are the project list.
//!
//! The operator's clients each have a directory — `~/NashvilleAutomation/<client>` —
//! and that directory already knows three of the four things a project needs: its
//! name, where to file into, and, from its git remote, which forge repo it is. Only
//! the people have to be typed. So the projects are read off the disk rather than
//! written out by hand, once, and then kept: [`PeopleFile::sync_folders`] adds what is
//! new and never removes anything, because a folder that has been archived is still a
//! project whose mail has to land somewhere.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{PeopleFile, Project};

/// What one sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncReport {
    /// The ids of the projects this run added, in the order the directory was read.
    pub added: Vec<String>,
    /// The directory names the `skip` list matched.
    pub skipped: Vec<String>,
    /// Directory names that yield no id at all — `---`, `...` — kept apart from
    /// [`SyncReport::skipped`] because the operator asked for those and did not ask
    /// for this. A folder here is one they have to rename or add to `skip`.
    pub unnameable: Vec<String>,
    /// Directories that already had a project. Nothing about them was touched.
    pub kept: usize,
}

impl PeopleFile {
    /// One project per child directory of `dir` that is not skipped and is not already
    /// in the file.
    ///
    /// A directory is already in the file when some project's `folder` names it, so
    /// renaming a project or editing its people never makes the sync add it twice.
    /// Nothing is ever removed and nothing already written is changed — not the
    /// people, not `seen`, not the name the operator gave it. This takes no clock for
    /// that reason: a sync is not a match, so it stamps nothing.
    ///
    /// The error is the directory that could not be read; a child that cannot be
    /// stat'd is skipped in silence, because an unreadable entry among thirty is not
    /// worth failing the other twenty-nine.
    ///
    /// A symlink is not followed, whatever it points at. The entry's own type is what
    /// is read, so a link to a client folder is passed over rather than added a second
    /// time under the link's name — and a link out of the clients directory cannot put
    /// a project anywhere the operator did not put a folder.
    pub fn sync_folders(&mut self, dir: &Path) -> Result<SyncReport, String> {
        let entries =
            std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let mut names: Vec<(String, std::path::PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // A dotted directory is machinery — `.git`, `.cache` — not a client.
            if name.starts_with('.') {
                continue;
            }
            names.push((name, path));
        }
        // `read_dir` returns whatever order the filesystem holds; the file this writes
        // is read by a person, so it is sorted.
        names.sort_by_key(|(name, _)| name.to_lowercase());

        let mut report = SyncReport::default();
        for (name, path) in names {
            if self.skip.iter().any(|pattern| matches_pattern(pattern, &name)) {
                report.skipped.push(name);
                continue;
            }
            let folder = home_relative(&path);
            if self.projects.iter().any(|project| same_folder(&project.folder, &folder)) {
                report.kept += 1;
                continue;
            }
            let base = slug(&name);
            let id = unique_id(&base, |candidate| {
                self.projects.iter().any(|project| project.id == candidate)
            });
            if id.is_empty() {
                // A name with no letters and no digits in it — `---` — is no id. It
                // is not "skipped": nobody asked for it to be passed over.
                report.unnameable.push(name);
                continue;
            }
            self.projects.push(Project {
                id: id.clone(),
                name,
                folder,
                repo: forge_repo(&path),
                ..Project::default()
            });
            report.added.push(id);
        }
        Ok(report)
    }
}

/// A directory name as an id: lowercase, and one dash wherever something else was.
///
/// Here rather than in the CLI because the desktop app mints ids too, and two
/// spellings of "what is this folder called" is two answers to the join key.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// `base`, or `base-2`, `base-3`, … — whichever the caller does not already have.
///
/// An empty base stays empty: `-2` names nothing, and the caller refuses it with a
/// better sentence than this can.
pub fn unique_id(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if base.is_empty() {
        return String::new();
    }
    if !taken(base) {
        return base.to_owned();
    }
    for n in 2..1000 {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    base.to_owned()
}

/// Does this `skip` pattern cover this directory name?
///
/// An exact name, or one `*` standing for any run of characters: `deploy-*`,
/// `*-backups`, `*`. Case is ignored, because the operator types the pattern and the
/// disk under it does not care either. This is not a glob library and does not want to
/// be: a skip list is read by the person who wrote it.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    let name = name.trim().to_lowercase();
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            // More than one star: the head of the first and the tail of the last. A
            // pattern that loose already said it does not care what is in the middle.
            let tail = tail.rsplit('*').next().unwrap_or(tail);
            name.len() >= head.len() + tail.len()
                && name.starts_with(head)
                && name.ends_with(tail)
        }
    }
}

/// A path written the way the file writes them: `~/NashvilleAutomation/x` when it is
/// under `$HOME`, and the whole path when it is not.
fn home_relative(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    let Some(home) = std::env::var("HOME").ok().filter(|home| !home.is_empty()) else {
        return text;
    };
    match text.strip_prefix(&home).and_then(|rest| rest.strip_prefix('/')) {
        Some(rest) => format!("~/{rest}"),
        None => text,
    }
}

/// Two spellings of one directory: `~/x` and `/Users/me/x`, with or without a trailing
/// slash.
fn same_folder(a: &str, b: &str) -> bool {
    expand(a) == expand(b)
}

fn expand(folder: &str) -> String {
    let folder = folder.trim().trim_end_matches('/');
    match folder.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => format!("{}/{rest}", home.trim_end_matches('/')),
            _ => folder.to_owned(),
        },
        None => folder.to_owned(),
    }
}

/// The forge repo this directory pushes to, when it has one and it is not GitHub.
///
/// Read out of `.git/config` with the standard library: nothing here shells out to git
/// or links libgit2 to answer one question about one line of an ini file. GitHub is
/// excluded on purpose — client work and open source live there, and this file is for
/// the private forge, so a GitHub-only client gets no `repo` and every consumer already
/// says what it cannot do without one.
fn forge_repo(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(".git").join("config")).ok()?;
    let url = origin_url(&text)?;
    let host = host_of(&url)?;
    if host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com") {
        return None;
    }
    let name = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()?
        .trim_end_matches(".git")
        .trim();
    if name.is_empty() { None } else { Some(name.to_owned()) }
}

/// The `url` of `[remote "origin"]`, out of a git config.
fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let header = line.trim_matches(['[', ']']).replace('"', "");
            let mut words = header.split_whitespace();
            in_origin = words.next() == Some("remote") && words.next() == Some("origin");
            continue;
        }
        if in_origin
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// The host of a git URL: `https://host/x`, `ssh://git@host:22/x`, or `git@host:x`. A
/// plain path has no host, and a directory that pushes to a directory is on no forge.
fn host_of(url: &str) -> Option<&str> {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => {
            // scp-style: `git@host:path`. A Windows drive or an absolute path is not.
            // A dot is what makes it a host rather than a drive letter or a path:
            // everything before the first colon can hold no colon of its own.
            let (before, _) = url.split_once(':')?;
            let host = before.rsplit('@').next()?;
            return Some(host).filter(|host| host.contains('.'));
        }
    };
    let authority = rest.split(['/', '?']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    if host.is_empty() { None } else { Some(host) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_the_folder_name_in_lowercase_with_dashes() {
        assert_eq!(slug("agstaff"), "agstaff");
        assert_eq!(slug("Pristine Acres"), "pristine-acres");
        assert_eq!(slug("  Rob & Joey  "), "rob-joey");
        assert_eq!(slug("---"), "");
    }

    #[test]
    fn a_taken_id_is_suffixed_until_it_is_its_own() {
        let taken = ["agstaff", "agstaff-2"];
        let held = |candidate: &str| taken.contains(&candidate);
        assert_eq!(unique_id("acres", held), "acres");
        assert_eq!(unique_id("agstaff", held), "agstaff-3");
        assert_eq!(unique_id("", held), "");
    }

    #[test]
    fn a_skip_pattern_is_a_name_or_one_star() {
        assert!(matches_pattern("agstaff", "agstaff"));
        assert!(matches_pattern("AgStaff", "agstaff"), "case is not the point");
        assert!(!matches_pattern("agstaff", "agstaff-2"));
        assert!(matches_pattern("deploy-*", "deploy-scripts"));
        assert!(!matches_pattern("deploy-*", "my-deploy"));
        assert!(matches_pattern("*-backups", "acres-backups"));
        assert!(!matches_pattern("*-backups", "backups"), "the dash is in the pattern");
        assert!(matches_pattern("*", "anything"));
        assert!(matches_pattern("old-*-stack", "old-web-stack"));
    }

    #[test]
    fn a_git_config_names_the_forge_repo_and_github_names_none() {
        let config = |url: &str| format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*\n");
        let repo = |url: &str| {
            let dir = std::env::temp_dir().join(format!(
                "people-core-git-{}-{}",
                std::process::id(),
                url.len()
            ));
            std::fs::create_dir_all(dir.join(".git")).unwrap();
            std::fs::write(dir.join(".git").join("config"), config(url)).unwrap();
            let answer = forge_repo(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            answer
        };
        assert_eq!(repo("https://nashcode.example.ts.net/agstaff"), Some("agstaff".to_owned()));
        assert_eq!(repo("https://nashcode.example.ts.net/agstaff.git"), Some("agstaff".to_owned()));
        assert_eq!(repo("git@forge.example.net:acres.git"), Some("acres".to_owned()));
        assert_eq!(repo("https://github.com/someone/public"), None, "GitHub is not the forge");
        assert_eq!(repo("git@github.com:someone/public.git"), None);
    }

    #[test]
    fn a_directory_with_no_git_at_all_has_no_repo() {
        let dir = std::env::temp_dir().join(format!("people-core-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(forge_repo(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_project_per_child_directory_unless_it_is_skipped_or_already_there() {
        let root = std::env::temp_dir().join(format!("people-core-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["agstaff", "Pristine Acres", "acres-backups", "deploy-scripts", ".hidden"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        // A loose file among the folders is not a client.
        std::fs::write(root.join("README.md"), "not a project").unwrap();
        // One of them pushes to the forge; the other to GitHub.
        std::fs::create_dir_all(root.join("agstaff").join(".git")).unwrap();
        std::fs::write(
            root.join("agstaff").join(".git").join("config"),
            "[remote \"origin\"]\n\turl = https://nashcode.example.ts.net/agstaff\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("Pristine Acres").join(".git")).unwrap();
        std::fs::write(
            root.join("Pristine Acres").join(".git").join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:client/acres.git\n",
        )
        .unwrap();

        let mut file = PeopleFile {
            skip: vec!["*-backups".to_owned(), "deploy-*".to_owned()],
            ..PeopleFile::default()
        };
        let report = file.sync_folders(&root).expect("the directory reads");

        assert_eq!(report.added, ["agstaff", "pristine-acres"]);
        assert_eq!(report.skipped, ["acres-backups", "deploy-scripts"]);
        assert!(report.unnameable.is_empty(), "{report:?}");
        assert_eq!(report.kept, 0);

        let agstaff = &file.projects[0];
        assert_eq!(agstaff.id, "agstaff");
        assert_eq!(agstaff.name, "agstaff");
        assert_eq!(agstaff.repo.as_deref(), Some("agstaff"));
        assert!(agstaff.folder.ends_with("/agstaff"), "{}", agstaff.folder);
        assert_eq!(agstaff.seen, None, "a folder on disk is not a match");
        assert!(agstaff.people.is_empty(), "the people are the part a person types");

        let acres = &file.projects[1];
        assert_eq!(acres.id, "pristine-acres");
        assert_eq!(acres.name, "Pristine Acres", "the name is the folder as it is spelled");
        assert_eq!(acres.repo, None, "a GitHub client has nowhere here to file");

        // Run it again: nothing new, nothing lost, and the edits in between survive.
        file.projects[0].people.push("rob".to_owned());
        file.projects[0].name = "AgStaff, LLC".to_owned();
        let again = file.sync_folders(&root).expect("the directory still reads");
        assert!(again.added.is_empty(), "{again:?}");
        assert_eq!(again.kept, 2);
        assert_eq!(file.projects.len(), 2);
        assert_eq!(file.projects[0].name, "AgStaff, LLC");
        assert_eq!(file.projects[0].people, ["rob"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_folder_with_the_same_name_gets_its_own_id() {
        let root = std::env::temp_dir().join(format!("people-core-sync2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Acres")).unwrap();

        // The id `acres` is already spoken for by a project somewhere else on disk.
        let mut file = PeopleFile {
            projects: vec![Project {
                id: "acres".to_owned(),
                folder: "~/Elsewhere/acres".to_owned(),
                ..Project::default()
            }],
            ..PeopleFile::default()
        };
        let report = file.sync_folders(&root).expect("the directory reads");
        assert_eq!(report.added, ["acres-2"]);
        assert_eq!(file.projects.len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_whose_name_is_no_id_is_reported_apart_from_the_skipped_ones() {
        let root = std::env::temp_dir().join(format!("people-core-noid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["---", "Acres", "old-logs"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }

        let mut file =
            PeopleFile { skip: vec!["old-*".to_owned()], ..PeopleFile::default() };
        let report = file.sync_folders(&root).expect("the directory reads");

        assert_eq!(report.added, ["acres"]);
        // `old-logs` was asked for by the `skip` list. `---` was not asked for by
        // anybody: it is a folder the operator has to rename.
        assert_eq!(report.skipped, ["old-logs"]);
        assert_eq!(report.unnameable, ["---"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_client_folder_is_not_a_second_project() {
        let root = std::env::temp_dir().join(format!("people-core-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("acres")).unwrap();
        // Both kinds: a link inside the clients directory, and one pointing out of it.
        std::os::unix::fs::symlink(root.join("acres"), root.join("acres-again")).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join("elsewhere")).unwrap();

        let mut file = PeopleFile::default();
        let report = file.sync_folders(&root).expect("the directory reads");

        assert_eq!(report.added, ["acres"], "the link is not followed: {report:?}");
        assert_eq!(file.projects.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_that_is_not_there_says_which_one() {
        let mut file = PeopleFile::default();
        let error = file.sync_folders(Path::new("/no/such/clients")).unwrap_err();
        assert!(error.starts_with("/no/such/clients"), "{error}");
        assert!(file.projects.is_empty());
    }
}
