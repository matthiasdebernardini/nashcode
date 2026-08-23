//! The file as the window holds it while it is being edited.
//!
//! [`people_core::PeopleFile`] is the contract on disk: typed lists, `Option`s, and
//! nothing half-typed. A text field holds none of those — it holds a string, and for
//! a list it holds one entry per line, including the blank line the operator is in
//! the middle of typing. [`Edit`] is that in-between form, and [`Edit::to_file`] is
//! where it becomes the contract again.
//!
//! Nothing here touches gpui or the disk. It is the app's domain model, so the round
//! trip and every list edit are provable without a window.

use people_core::{Email, Imsg, PeopleFile, Person, Project};

/// The whole file, in the form the fields edit.
///
/// `Eq` is not derived here for the reason `people_core` gives up its own: a carried
/// key can hold a JSON float. `==` still answers, which is all "unsaved" asks of it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edit {
    /// The operator's own addresses. v1 shows them in the contact map and does not
    /// edit them; they are carried through the round trip untouched.
    pub me: Vec<String>,
    pub people: Vec<PersonEdit>,
    pub projects: Vec<ProjectEdit>,
    /// Everything in the file this window does not model: `skip`, and any key
    /// somebody wrote in by hand. The fields above are blanked out of it, so it holds
    /// the remainder and nothing twice.
    carried: PeopleFile,
}

/// One person. `id` is the join key and is not edited here: renaming it has to
/// rewrite every project that lists it, and a half-typed id would collide with a
/// real one on the way through.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersonEdit {
    pub id: String,
    pub name: String,
    /// One phone per line.
    pub phones: String,
    /// One email per line.
    pub emails: String,
    /// This person's `signal`, `seen`, and any hand-written key. See [`Edit::carried`].
    carried: Person,
}

/// One project. `id` is likewise fixed: it is the name an answer comes back under.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectEdit {
    pub id: String,
    pub name: String,
    pub folder: String,
    /// Empty means no nashcode repo, which is the file's `null`.
    pub repo: String,
    /// Person ids, in the order the project lists them.
    pub people: Vec<String>,
    /// One iMessage group id per line.
    pub chat_ids: String,
    pub prompt: String,
    pub enrich: bool,
    pub media_only: bool,
    pub account: String,
    pub query: String,
    /// This project's `seen` and any hand-written key, including the ones inside its
    /// `imsg` and `email` blocks. See [`Edit::carried`].
    carried: Project,
}

impl Edit {
    pub fn from_file(file: &PeopleFile) -> Self {
        Self {
            me: file.me.clone(),
            people: file
                .people
                .iter()
                .map(|person| PersonEdit {
                    id: person.id.clone(),
                    name: person.name.clone(),
                    phones: lines(&person.phones),
                    emails: lines(&person.emails),
                    carried: carried_person(person),
                })
                .collect(),
            projects: file
                .projects
                .iter()
                .map(|project| ProjectEdit {
                    id: project.id.clone(),
                    name: project.name.clone(),
                    folder: project.folder.clone(),
                    repo: project.repo.clone().unwrap_or_default(),
                    people: project.people.clone(),
                    chat_ids: lines(&project.chat_ids),
                    prompt: project.imsg.prompt.clone(),
                    enrich: project.imsg.enrich,
                    media_only: project.imsg.media_only,
                    account: project.email.account.clone().unwrap_or_default(),
                    query: project.email.query.clone().unwrap_or_default(),
                    carried: carried_project(project),
                })
                .collect(),
            carried: PeopleFile {
                me: Vec::new(),
                people: Vec::new(),
                projects: Vec::new(),
                ..file.clone()
            },
        }
    }

    pub fn to_file(&self) -> PeopleFile {
        PeopleFile {
            me: self.me.clone(),
            people: self
                .people
                .iter()
                .map(|person| Person {
                    id: person.id.clone(),
                    name: person.name.clone(),
                    phones: entries(&person.phones),
                    emails: entries(&person.emails),
                    // Everything this window does not model, back where it was.
                    ..person.carried.clone()
                })
                .collect(),
            projects: self
                .projects
                .iter()
                .map(|project| Project {
                    id: project.id.clone(),
                    name: project.name.clone(),
                    folder: project.folder.clone(),
                    repo: optional(&project.repo),
                    people: project.people.clone(),
                    chat_ids: entries(&project.chat_ids),
                    imsg: Imsg {
                        prompt: project.prompt.clone(),
                        enrich: project.enrich,
                        media_only: project.media_only,
                        ..project.carried.imsg.clone()
                    },
                    email: Email {
                        account: optional(&project.account),
                        query: optional(&project.query),
                        ..project.carried.email.clone()
                    },
                    ..project.carried.clone()
                })
                .collect(),
            ..self.carried.clone()
        }
    }

    pub fn person(&self, id: &str) -> Option<&PersonEdit> {
        self.people.iter().find(|person| person.id == id)
    }

    pub fn person_mut(&mut self, id: &str) -> Option<&mut PersonEdit> {
        self.people.iter_mut().find(|person| person.id == id)
    }

    pub fn project(&self, id: &str) -> Option<&ProjectEdit> {
        self.projects.iter().find(|project| project.id == id)
    }

    pub fn project_mut(&mut self, id: &str) -> Option<&mut ProjectEdit> {
        self.projects.iter_mut().find(|project| project.id == id)
    }

    /// Add a person and answer with the id to select.
    pub fn add_person(&mut self) -> String {
        let name = "New person".to_owned();
        let id = self.free_person_id(&name, "");
        self.people.push(PersonEdit { id: id.clone(), name, ..PersonEdit::default() });
        id
    }

    /// Add a project and answer with the id to select.
    pub fn add_project(&mut self) -> String {
        let name = "New project".to_owned();
        let id = self.free_project_id(&name, "");
        self.projects.push(ProjectEdit {
            id: id.clone(),
            name,
            // The file's own default for a project nobody configured: enrichment on,
            // media-only off. `ProjectEdit::default()` cannot say that — `bool`
            // defaults to false — so a new project says it here.
            enrich: true,
            // A project nobody wrote by hand carries nothing.
            carried: carried_project(&Project::default()),
            ..ProjectEdit::default()
        });
        id
    }

    /// Remove a project. Nothing refers to a project id, so there is nothing to
    /// refuse — the only cost is the settings in it, and the file is not yet saved.
    pub fn delete_project(&mut self, id: &str) {
        self.projects.retain(|project| project.id != id);
    }

    /// Re-derive a person's id from their current name, and carry every project
    /// that lists them across to the new one.
    ///
    /// The id is the join key, and it is not typed: it is the name in slug form. That
    /// is why the window can rewrite it on every keystroke without ever breaking the
    /// join — the rename is one operation over the whole file, and the new id is
    /// uniquified before anything moves, so it can never collide with a real one.
    pub fn reslug_person(&mut self, id: &str) -> String {
        let Some(person) = self.person(id) else {
            return id.to_owned();
        };
        let fresh = self.free_person_id(&person.name.clone(), id);
        if fresh == id {
            return fresh;
        }
        if let Some(person) = self.person_mut(id) {
            person.id = fresh.clone();
        }
        for project in &mut self.projects {
            for listed in &mut project.people {
                if listed == id {
                    listed.clone_from(&fresh);
                }
            }
        }
        fresh
    }

    /// The same for a project. Nothing refers to a project id, so this only renames.
    pub fn reslug_project(&mut self, id: &str) -> String {
        let Some(project) = self.project(id) else {
            return id.to_owned();
        };
        let fresh = self.free_project_id(&project.name.clone(), id);
        if fresh != id && let Some(project) = self.project_mut(id) {
            project.id = fresh.clone();
        }
        fresh
    }

    /// A slug of `name` that no other person holds. `keep` is the id being renamed,
    /// which does not count as taken.
    fn free_person_id(&self, name: &str, keep: &str) -> String {
        unique(&slug(name, "person"), |candidate| {
            self.people.iter().any(|person| person.id == candidate && person.id != keep)
        })
    }

    fn free_project_id(&self, name: &str, keep: &str) -> String {
        unique(&slug(name, "project"), |candidate| {
            self.projects.iter().any(|project| project.id == candidate && project.id != keep)
        })
    }

    /// Remove a person, unless a project still lists them.
    ///
    /// Deleting them anyway would leave a dangling id, which is exactly what
    /// `PeopleFile::parse` refuses to load — the file would save and then never open
    /// again. The refusal names the projects, because taking the person off those is
    /// the fix.
    pub fn delete_person(&mut self, id: &str) -> Result<(), String> {
        let holders: Vec<&str> = self
            .projects
            .iter()
            .filter(|project| project.people.iter().any(|listed| listed == id))
            .map(|project| project.name.trim())
            .map(|name| if name.is_empty() { "(unnamed project)" } else { name })
            .collect();
        if !holders.is_empty() {
            let who = self.person(id).map(|p| display(&p.id, &p.name)).unwrap_or_else(|| id.into());
            return Err(format!(
                "{who} is still on {}. Take them off there first, or the file will not load.",
                holders.join(", ")
            ));
        }
        self.people.retain(|person| person.id != id);
        Ok(())
    }

    /// Put a person on a project. Listing them twice is not an error in the file,
    /// but it is never what anyone meant, so a repeat does nothing.
    pub fn add_to_project(&mut self, project: &str, person: &str) {
        if let Some(project) = self.project_mut(project)
            && !project.people.iter().any(|listed| listed == person)
        {
            project.people.push(person.to_owned());
        }
    }

    pub fn remove_from_project(&mut self, project: &str, person: &str) {
        if let Some(project) = self.project_mut(project) {
            project.people.retain(|listed| listed != person);
        }
    }

}

/// One person, with everything the window models blanked out, so what is left is
/// what has to be written back untouched.
///
/// Blanked rather than kept whole: `Edit` is compared with `==` to answer "unsaved",
/// and a copy of the edited fields sitting beside the edited fields would answer that
/// question twice, differently.
fn carried_person(person: &Person) -> Person {
    Person {
        id: String::new(),
        name: String::new(),
        phones: Vec::new(),
        emails: Vec::new(),
        ..person.clone()
    }
}

/// The same for a project, including inside its `imsg` and `email` blocks.
fn carried_project(project: &Project) -> Project {
    Project {
        id: String::new(),
        name: String::new(),
        folder: String::new(),
        repo: None,
        people: Vec::new(),
        chat_ids: Vec::new(),
        imsg: Imsg {
            prompt: String::new(),
            enrich: false,
            media_only: false,
            ..project.imsg.clone()
        },
        email: Email { account: None, query: None, ..project.email.clone() },
        ..project.clone()
    }
}

/// The id form of a name: lowercase, words joined by a single dash, nothing else.
/// `fallback` is what a name with no letters or digits in it becomes.
pub fn slug(name: &str, fallback: &str) -> String {
    let mut out = String::new();
    for character in name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() { fallback.to_owned() } else { trimmed.to_owned() }
}

/// `base`, else `base-2`, `base-3`, … until `taken` says no.
fn unique(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// What to call someone: their name when they have one, else their id.
pub fn display(id: &str, name: &str) -> String {
    if name.trim().is_empty() { id.to_owned() } else { name.trim().to_owned() }
}

/// A list as a text field holds it: one entry per line.
fn lines(values: &[String]) -> String {
    values.join("\n")
}

/// A text field back as a list. Blank lines are the operator mid-edit, not entries,
/// and a stray space around an address is a typo the file should not keep.
fn entries(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned).collect()
}

/// An empty field is the file's absent value, not an empty string: `repo: ""` would
/// mean "a repo whose name is nothing", and every consumer would try to open it.
fn optional(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PeopleFile {
        PeopleFile::parse(
            r#"{
              "me": ["operator@example.com", "+15559990000"],
              "people": [
                { "id": "rob", "name": "Rob Castro",
                  "phones": ["+15550001111"], "emails": ["rob@example.com"] },
                { "id": "joey", "name": "Joey Locker",
                  "phones": ["+15550002222"], "emails": [] }
              ],
              "projects": [
                { "id": "agstaff", "name": "agstaff", "folder": "~/Projects/agstaff",
                  "repo": "agstaff", "people": ["rob", "joey"],
                  "chat_ids": ["12", "34"],
                  "imsg": { "prompt": "File it.", "enrich": true, "media_only": false },
                  "email": { "account": "operator@example.com" } },
                { "id": "acres", "name": "Pristine Acres", "folder": "~/Projects/acres",
                  "people": ["rob"] }
              ]
            }"#,
        )
        .expect("the fixture is a valid file")
    }

    #[test]
    fn the_file_survives_the_round_trip_through_the_fields() {
        // The one invariant the whole window rests on: opening a file and saving it
        // without touching a key must not change it.
        let file = fixture();
        assert_eq!(Edit::from_file(&file).to_file(), file);
    }

    #[test]
    fn a_list_field_is_one_entry_per_line_and_blank_lines_are_not_entries() {
        let mut edit = Edit::from_file(&fixture());
        let rob = edit.person_mut("rob").expect("rob");
        assert_eq!(rob.phones, "+15550001111");
        // What the operator leaves behind while typing the next number.
        rob.phones = "+15550001111\n\n  +15550003333  \n".to_owned();

        let saved = edit.to_file();
        assert_eq!(saved.people[0].phones, ["+15550001111", "+15550003333"]);
    }

    #[test]
    fn an_empty_repo_field_is_no_repo_rather_than_a_repo_called_nothing() {
        let mut edit = Edit::from_file(&fixture());
        edit.project_mut("agstaff").expect("agstaff").repo = "   ".to_owned();

        let saved = edit.to_file();
        assert_eq!(saved.projects[0].repo, None);
        assert_eq!(saved.projects[1].repo, None, "acres never had one");
    }

    #[test]
    fn deleting_a_person_a_project_still_lists_is_refused_and_says_where() {
        let mut edit = Edit::from_file(&fixture());
        let refusal = edit.delete_person("rob").expect_err("rob is on two projects");

        assert!(refusal.contains("Rob Castro"), "{refusal}");
        assert!(refusal.contains("agstaff") && refusal.contains("Pristine Acres"), "{refusal}");
        assert!(edit.person("rob").is_some(), "the refusal did not delete him anyway");
    }

    #[test]
    fn a_person_no_project_lists_deletes() {
        let mut edit = Edit::from_file(&fixture());
        let spare = edit.add_person();
        assert_eq!(edit.delete_person(&spare), Ok(()));
        assert!(edit.person(&spare).is_none());

        // And taking Rob off both projects first is the documented way out.
        edit.remove_from_project("agstaff", "rob");
        edit.remove_from_project("acres", "rob");
        assert_eq!(edit.delete_person("rob"), Ok(()));
    }

    #[test]
    fn a_new_person_gets_an_id_nobody_else_has() {
        let mut edit = Edit::from_file(&fixture());
        let first = edit.add_person();
        let second = edit.add_person();

        assert_eq!(first, "new-person");
        assert_eq!(second, "new-person-2", "the second one is suffixed, not a collision");
        assert_eq!(edit.to_file().validate().iter().filter(|f| f.fatal).count(), 0);
    }

    #[test]
    fn a_new_project_arrives_with_the_files_own_defaults() {
        let mut edit = Edit::from_file(&fixture());
        let id = edit.add_project();
        let project = edit.project(&id).expect("the new project");

        assert_eq!(id, "new-project");
        assert!(project.enrich, "enrichment is on unless it is turned off");
        assert!(!project.media_only);
        assert!(project.people.is_empty());
        // And it round-trips as the file writes it.
        assert_eq!(Edit::from_file(&edit.to_file()), edit);
    }

    #[test]
    fn a_project_deletes_without_argument_because_nothing_refers_to_it() {
        let mut edit = Edit::from_file(&fixture());
        edit.delete_project("acres");

        assert!(edit.project("acres").is_none());
        assert!(edit.person("rob").is_some(), "his other project is gone, not him");
        assert_eq!(edit.to_file().validate().iter().filter(|f| f.fatal).count(), 0);
    }

    #[test]
    fn an_id_is_the_name_in_slug_form_and_a_rename_carries_every_project_with_it() {
        let mut edit = Edit::from_file(&fixture());
        edit.person_mut("rob").expect("rob").name = "Roberta  O'Neill-Castro!".to_owned();
        let fresh = edit.reslug_person("rob");

        assert_eq!(fresh, "roberta-o-neill-castro");
        assert!(edit.person("rob").is_none());
        // Both projects that listed him now list her, in the same places.
        assert_eq!(edit.project("agstaff").expect("agstaff").people, [fresh.clone(), "joey".into()]);
        assert_eq!(edit.project("acres").expect("acres").people, [fresh]);
        assert_eq!(edit.to_file().validate().iter().filter(|f| f.fatal).count(), 0);
    }

    #[test]
    fn a_rename_onto_a_taken_id_is_suffixed_rather_than_a_collision() {
        let mut edit = Edit::from_file(&fixture());
        edit.person_mut("joey").expect("joey").name = "Rob Castro".to_owned();
        let fresh = edit.reslug_person("joey");

        assert_eq!(fresh, "rob-castro", "the real rob still holds the id `rob`");
        edit.person_mut("rob").expect("rob").name = "Rob Castro".to_owned();
        assert_eq!(edit.reslug_person("rob"), "rob-castro-2");
        assert_eq!(edit.to_file().validate().iter().filter(|f| f.fatal).count(), 0);
    }

    #[test]
    fn a_name_with_nothing_to_slug_still_gets_an_id() {
        assert_eq!(slug("   ", "person"), "person");
        assert_eq!(slug("!!! ???", "project"), "project");
        assert_eq!(slug("  Pristine   Acres  ", "project"), "pristine-acres");
        assert_eq!(slug("agstaff", "project"), "agstaff");
    }

    #[test]
    fn membership_is_a_switch_rather_than_a_list_that_can_hold_a_name_twice() {
        let mut edit = Edit::from_file(&fixture());
        edit.add_to_project("acres", "joey");
        edit.add_to_project("acres", "joey");
        assert_eq!(edit.project("acres").expect("acres").people, ["rob", "joey"]);

        edit.remove_from_project("acres", "joey");
        edit.remove_from_project("acres", "joey");
        assert_eq!(edit.project("acres").expect("acres").people, ["rob"]);
    }
}
