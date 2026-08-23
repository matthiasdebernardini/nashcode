//! The contact map: every phone and every email in the file, and where it routes.
//!
//! [`PeopleFile::route`] answers "which project" for contacts a caller already has.
//! This answers the question the other way round, over the whole file: for each
//! address written down, who owns it and which projects that person is on. It is the
//! file read as the operator sees it, so an address that reaches nothing is visible
//! before a message arrives for it rather than after.
//!
//! Pure, and in this crate rather than in the desktop app, so the CLI and the viewer
//! can show the same table without a second implementation of the same join.

use serde::{Deserialize, Serialize};

use crate::model::{PeopleFile, label};
use crate::route::normalize;

/// A phone or an email: the two kinds of address a person can be reached at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContactKind {
    Phone,
    Email,
}

/// One address in the file, and everything routing will do with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRow {
    /// The address as it is written in the file, not normalised: the operator is
    /// looking for the line they typed.
    pub contact: String,
    pub kind: ContactKind,
    /// The person it belongs to. `None` is a `me` entry — the operator's own.
    pub person_id: Option<String>,
    /// What to call that person: their name, else their id. Empty for a `me` entry.
    pub person: String,
    /// The projects that person is on, named, in file order.
    pub projects: Vec<String>,
    /// The address is also one of the operator's own — it is in `me`, or it is some
    /// project's mail account — so it never scores, whoever else has it written down.
    pub own: bool,
}

impl ContactRow {
    /// The operator's own entry rather than a person's.
    pub fn is_me(&self) -> bool {
        self.person_id.is_none()
    }

    /// Nothing will ever route by this address: its person is on no project, or the
    /// address is one of the operator's own and so is excluded before scoring. A `me`
    /// entry is not "nowhere" — routing nowhere is its job.
    pub fn routes_nowhere(&self) -> bool {
        !self.is_me() && (self.own || self.projects.is_empty())
    }
}

/// Every address in the file, one row each.
///
/// People first, then the operator's own `me` entries; inside each group, sorted by
/// the address as routing compares it, so two spellings of one number stand next to
/// each other. A person who writes the same address twice gets two rows: the file
/// says it twice, and hiding one would hide the mistake.
pub fn contact_map(file: &PeopleFile) -> Vec<ContactRow> {
    let own = file.own_addresses();
    let mut rows: Vec<ContactRow> = Vec::new();

    for person in &file.people {
        let projects: Vec<String> = file
            .projects
            .iter()
            .filter(|project| project.people.iter().any(|id| id == &person.id))
            .map(|project| label(&project.id, &project.name).to_owned())
            .collect();
        let who = label(&person.id, &person.name).to_owned();

        let phones = person.phones.iter().map(|value| (value, ContactKind::Phone));
        let emails = person.emails.iter().map(|value| (value, ContactKind::Email));
        for (value, kind) in phones.chain(emails) {
            if value.trim().is_empty() {
                continue;
            }
            rows.push(ContactRow {
                contact: value.clone(),
                kind,
                person_id: Some(person.id.clone()),
                person: who.clone(),
                projects: projects.clone(),
                own: own.contains(&normalize(value)),
            });
        }
    }

    for entry in &file.me {
        if entry.trim().is_empty() {
            continue;
        }
        rows.push(ContactRow {
            contact: entry.clone(),
            kind: kind_of(entry),
            person_id: None,
            person: String::new(),
            projects: Vec::new(),
            own: true,
        });
    }

    rows.sort_by(|a, b| {
        a.is_me()
            .cmp(&b.is_me())
            .then_with(|| normalize(&a.contact).cmp(&normalize(&b.contact)))
            .then_with(|| a.person_id.cmp(&b.person_id))
    });
    rows
}

/// An address with an `@` in it is an email; anything else is a number. `me` is a
/// flat list of both, so the kind has to come from the text.
fn kind_of(entry: &str) -> ContactKind {
    if entry.contains('@') { ContactKind::Email } else { ContactKind::Phone }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(rows: &'a [ContactRow], contact: &str) -> &'a ContactRow {
        rows.iter().find(|row| row.contact == contact).unwrap_or_else(|| {
            panic!("no row for {contact:?} in {:?}", rows.iter().map(|r| &r.contact).collect::<Vec<_>>())
        })
    }

    #[test]
    fn every_address_gets_a_row_naming_its_person_and_their_projects() {
        let rows = contact_map(&crate::route::tests::fixture());

        // Three people with five addresses between them, plus two `me` entries.
        assert_eq!(rows.len(), 7, "{rows:?}");

        let phone = row(&rows, "+15550001111");
        assert_eq!(phone.kind, ContactKind::Phone);
        assert_eq!(phone.person, "Rob Castro");
        assert_eq!(phone.person_id.as_deref(), Some("rob"));
        // Rob is on both projects, in file order.
        assert_eq!(phone.projects, ["agstaff", "Pristine Acres"]);
        assert!(!phone.routes_nowhere());

        let email = row(&rows, "joey@example.com");
        assert_eq!(email.kind, ContactKind::Email);
        assert_eq!(email.projects, ["agstaff"], "Joey is only on the one");
    }

    #[test]
    fn the_operators_own_entries_are_their_own_group_and_route_nowhere_by_design() {
        let rows = contact_map(&crate::route::tests::fixture());
        let me: Vec<&ContactRow> = rows.iter().filter(|row| row.is_me()).collect();

        assert_eq!(me.len(), 2, "{rows:?}");
        assert!(me.iter().all(|row| row.own && row.projects.is_empty() && row.person.is_empty()));
        assert!(me.iter().all(|row| !row.routes_nowhere()), "reaching nothing is what `me` is for");
        // `me` is a flat list of both kinds, so the kind comes from the text: `+` sorts
        // before a letter, so the number is first and the `@` is what marks the other.
        assert_eq!(me[0].kind, ContactKind::Phone);
        assert_eq!(me[1].kind, ContactKind::Email, "an @ makes it an address");

        // The people come first, so the view can cut the group at the first `me` row.
        assert!(rows.iter().position(ContactRow::is_me) == Some(rows.len() - me.len()));
    }

    #[test]
    fn a_person_on_no_project_routes_nowhere_and_says_so() {
        let file = PeopleFile::parse(
            r#"{ "people": [ { "id": "sam", "name": "Sam Pike",
                               "emails": ["sam@example.com"] } ],
                 "projects": [] }"#,
        )
        .expect("a person nobody has claimed is a valid file");
        let rows = contact_map(&file);

        assert_eq!(rows.len(), 1);
        assert!(rows[0].projects.is_empty());
        assert!(rows[0].routes_nowhere());
        assert!(!rows[0].own);
    }

    #[test]
    fn an_address_the_operator_also_owns_routes_nowhere_however_it_is_filed() {
        // The project's mail account is on every thread, so it is excluded before
        // scoring — writing it on a person does not change that, and the map has to
        // say so or the operator will wait for mail that never files.
        let file = PeopleFile::parse(
            r#"{ "people": [ { "id": "rob", "name": "Rob Castro",
                               "emails": ["shared@example.com"] } ],
                 "projects": [ { "id": "agstaff", "name": "agstaff", "people": ["rob"],
                                 "email": { "account": "SHARED@example.com" } } ] }"#,
        )
        .expect("the file loads; the address is merely useless");
        let rows = contact_map(&file);

        assert_eq!(rows[0].projects, ["agstaff"], "he is still on the project");
        assert!(rows[0].own, "the address is the operator's too");
        assert!(rows[0].routes_nowhere(), "and so it will never score");
    }

    #[test]
    fn a_blank_line_is_not_an_address() {
        let file = PeopleFile::parse(
            r#"{ "me": ["  "],
                 "people": [ { "id": "rob", "phones": ["", " "], "emails": [""] } ],
                 "projects": [] }"#,
        )
        .expect("blank lines load");
        assert!(contact_map(&file).is_empty(), "an empty field is not a contact");
    }
}
