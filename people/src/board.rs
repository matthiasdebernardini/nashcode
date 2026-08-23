//! What the canvas draws, as data: three lanes of cards and the links between them.
//!
//! The window shows one picture — a contact reaches a person, a person belongs to
//! projects — so the model is that picture and not three lists. Everything here is a
//! pure function of a [`PeopleFile`]: the same file always gives the same board, and
//! the selection rule, the bands, and the per-card warnings can all be proved without
//! opening a window.
//!
//! Nothing here knows about gpui. Geometry belongs to [`crate::links`]; this decides
//! only *which* cards exist and *which* of them are joined.

use std::collections::{BTreeMap, BTreeSet};

use people_core::{ContactKind, ContactRow, PeopleFile, contact_map, model::is_e164};

use crate::edit::display;

/// A card's identity, and the selection's. Never a lane position: cards move when a
/// name changes, and a selection that meant "row 3" would follow the wrong card.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CardId {
    /// One address. `owner` is the person who holds it, or `None` for a `me` entry.
    /// Two people may write the same number down, and one person may write it down
    /// twice, so `index` says which of that owner's copies this card is. Without it
    /// the two cards would share one identity — and therefore one set of bounds, one
    /// wire, and one selection.
    Contact { owner: Option<String>, value: String, index: usize },
    Person(String),
    Project(String),
}

impl CardId {
    /// The domain key an element id is built from. A contact's key carries its owner
    /// and its occurrence, because the address alone does not name one card.
    pub fn key(&self) -> String {
        match self {
            CardId::Contact { owner, value, index } => {
                format!("{}#{index}#{value}", owner.as_deref().unwrap_or("me"))
            }
            CardId::Person(id) | CardId::Project(id) => id.clone(),
        }
    }
}

/// Where a card sits in its lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    /// The lane proper: this card is part of a route.
    Routes,
    /// Below the fold: nothing will ever route by it.
    Nowhere,
    /// The operator's own addresses. Not a fault — never scoring is their job.
    Mine,
}

impl Band {
    /// The order the bands are drawn in, from the top of a lane down.
    fn rank(self) -> u8 {
        match self {
            Band::Routes => 0,
            Band::Nowhere => 1,
            Band::Mine => 2,
        }
    }
}

/// The three lanes, left to right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Contacts,
    People,
    Projects,
}

impl Lane {
    pub const ALL: [Lane; 3] = [Lane::Contacts, Lane::People, Lane::Projects];

    pub fn title(self) -> &'static str {
        match self {
            Lane::Contacts => "Contacts",
            Lane::People => "People",
            Lane::Projects => "Projects",
        }
    }
}

/// One address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactCard {
    pub id: CardId,
    /// The address as the file writes it.
    pub value: String,
    pub kind: ContactKind,
    /// The person who holds it, or `None` for a `me` entry.
    pub owner: Option<String>,
    pub band: Band,
    /// One short line about what is wrong with this address.
    pub warning: Option<String>,
}

/// One person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonCard {
    pub id: CardId,
    pub person: String,
    /// Their name, or their id when they have none.
    pub name: String,
    pub phones: usize,
    pub emails: usize,
    pub band: Band,
    pub warning: Option<String>,
}

/// One project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCard {
    pub id: CardId,
    pub project: String,
    pub name: String,
    /// The nashcode repo, when it has one.
    pub repo: Option<String>,
    pub folder: String,
    pub people: usize,
    pub warning: Option<String>,
}

/// A drawn join between two cards, left lane to right lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub from: CardId,
    pub to: CardId,
}

/// The whole picture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Board {
    pub contacts: Vec<ContactCard>,
    pub people: Vec<PersonCard>,
    pub projects: Vec<ProjectCard>,
    /// Contact→person and person→project, in lane order, so a link's index is stable
    /// for one frame and can name it to the hover test.
    pub links: Vec<Link>,
}

impl Board {
    /// Read the file into the picture.
    pub fn from_file(file: &PeopleFile) -> Self {
        let rows = contact_map(file);

        // An address is not an identity: two people may hold the same number, and one
        // person may list it twice. The occurrence within an owner is what tells the
        // resulting cards apart.
        let mut seen: BTreeMap<(Option<&str>, &str), usize> = BTreeMap::new();
        let mut contacts: Vec<ContactCard> = rows
            .iter()
            .map(|row| {
                let held = (row.person_id.as_deref(), row.contact.as_str());
                let seat = seen.entry(held).or_insert(0);
                let index = *seat;
                *seat += 1;
                contact_card(row, index)
            })
            .collect();
        // Sorted into bands here, once, rather than at every reading of the lane: the
        // canvas draws a lane top to bottom, and a band is a run of cards in it.
        contacts.sort_by_key(|card| card.band.rank());

        let mut people: Vec<PersonCard> = file
            .people
            .iter()
            .map(|person| {
                let on = file
                    .projects
                    .iter()
                    .filter(|project| project.people.iter().any(|listed| listed == &person.id))
                    .count();
                let reachable = !person.phones.is_empty() || !person.emails.is_empty();
                PersonCard {
                    id: CardId::Person(person.id.clone()),
                    person: person.id.clone(),
                    name: display(&person.id, &person.name),
                    phones: person.phones.len(),
                    emails: person.emails.len(),
                    band: if on == 0 { Band::Nowhere } else { Band::Routes },
                    warning: if reachable {
                        None
                    } else {
                        Some("no phone and no email, so nothing can match them".to_owned())
                    },
                }
            })
            .collect();
        people.sort_by_key(|card| card.band.rank());

        let projects: Vec<ProjectCard> = file
            .projects
            .iter()
            .map(|project| {
                let dangling: Vec<&str> = project
                    .people
                    .iter()
                    .filter(|id| !file.people.iter().any(|person| &&person.id == id))
                    .map(String::as_str)
                    .collect();
                ProjectCard {
                    id: CardId::Project(project.id.clone()),
                    project: project.id.clone(),
                    name: display(&project.id, &project.name),
                    repo: project.repo.clone().filter(|repo| !repo.trim().is_empty()),
                    folder: project.folder.clone(),
                    people: project.people.len(),
                    warning: if !dangling.is_empty() {
                        Some(format!("lists {}, whom no person has", dangling.join(", ")))
                    } else if project.people.is_empty() {
                        Some("nobody on it, so nothing routes here".to_owned())
                    } else {
                        None
                    },
                }
            })
            .collect();

        let mut links: Vec<Link> = Vec::new();
        for card in &contacts {
            if let Some(owner) = &card.owner {
                links.push(Link { from: card.id.clone(), to: CardId::Person(owner.clone()) });
            }
        }
        for project in &file.projects {
            let mut drawn: BTreeSet<&str> = BTreeSet::new();
            for id in &project.people {
                // A project that lists the same person twice draws one link.
                if !drawn.insert(id.as_str()) || !people.iter().any(|card| &card.person == id) {
                    continue;
                }
                links.push(Link {
                    from: CardId::Person(id.clone()),
                    to: CardId::Project(project.id.clone()),
                });
            }
        }

        Self { contacts, people, projects, links }
    }

    pub fn is_empty(&self) -> bool {
        self.contacts.is_empty() && self.people.is_empty() && self.projects.is_empty()
    }

    /// The ids in one lane, top to bottom. The cards are already in band order, so
    /// this is the order they are drawn in and the order the arrow keys walk.
    pub fn lane(&self, lane: Lane) -> Vec<CardId> {
        match lane {
            Lane::Contacts => self.contacts.iter().map(|card| card.id.clone()).collect(),
            Lane::People => self.people.iter().map(|card| card.id.clone()).collect(),
            Lane::Projects => self.projects.iter().map(|card| card.id.clone()).collect(),
        }
    }

    /// Which lane a card is in.
    pub fn lane_of(id: &CardId) -> Lane {
        match id {
            CardId::Contact { .. } => Lane::Contacts,
            CardId::Person(_) => Lane::People,
            CardId::Project(_) => Lane::Projects,
        }
    }

    /// The card is still on the board.
    pub fn holds(&self, id: &CardId) -> bool {
        self.lane(Board::lane_of(id)).iter().any(|card| card == id)
    }

    /// Everything a selection lights up.
    ///
    /// The traversal follows the picture rather than the graph: from a contact the eye
    /// runs right — to the person, then to that person's projects. From a project it
    /// runs left. From a person it runs both ways. A plain graph walk would keep
    /// going, and selecting one number would light up half the board through a
    /// project's other members.
    pub fn connected(&self, selected: &CardId) -> BTreeSet<CardId> {
        let mut lit: BTreeSet<CardId> = BTreeSet::new();
        lit.insert(selected.clone());

        let projects_of = |person: &str| -> Vec<CardId> {
            self.links
                .iter()
                .filter(|link| link.from == CardId::Person(person.to_owned()))
                .map(|link| link.to.clone())
                .collect()
        };
        let contacts_of = |person: &str| -> Vec<CardId> {
            self.links
                .iter()
                .filter(|link| link.to == CardId::Person(person.to_owned()))
                .map(|link| link.from.clone())
                .collect()
        };
        let people_of = |project: &CardId| -> Vec<String> {
            self.links
                .iter()
                .filter(|link| &link.to == project)
                .filter_map(|link| match &link.from {
                    CardId::Person(id) => Some(id.clone()),
                    _ => None,
                })
                .collect()
        };

        match selected {
            CardId::Contact { owner: Some(owner), .. } => {
                lit.insert(CardId::Person(owner.clone()));
                lit.extend(projects_of(owner));
            }
            // A `me` entry belongs to nobody, so it lights up alone.
            CardId::Contact { owner: None, .. } => {}
            CardId::Person(person) => {
                lit.extend(contacts_of(person));
                lit.extend(projects_of(person));
            }
            CardId::Project(_) => {
                for person in people_of(selected) {
                    lit.extend(contacts_of(&person));
                    lit.insert(CardId::Person(person));
                }
            }
        }
        lit
    }
}

fn contact_card(row: &ContactRow, index: usize) -> ContactCard {
    let warning = match row.kind {
        ContactKind::Phone if !is_e164(&row.contact) => Some(
            "not E.164; write it as a plus sign, the country code, and digits".to_owned(),
        ),
        _ if row.own && !row.is_me() => {
            Some("one of your own addresses, so it never scores".to_owned())
        }
        _ => None,
    };
    ContactCard {
        id: CardId::Contact {
            owner: row.person_id.clone(),
            value: row.contact.clone(),
            index,
        },
        value: row.contact.clone(),
        kind: row.kind,
        owner: row.person_id.clone(),
        band: if row.is_me() {
            Band::Mine
        } else if row.routes_nowhere() {
            Band::Nowhere
        } else {
            Band::Routes
        },
        warning,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Two projects, three people. Rob is on both. Sam is on neither and his phone is
    /// not E.164, so he exercises both the nowhere band and a card warning.
    pub(crate) fn fixture() -> PeopleFile {
        PeopleFile::parse(
            r#"{
              "me": ["operator@example.com", "+15559990000"],
              "people": [
                { "id": "rob", "name": "Rob Castro",
                  "phones": ["+15550001111"], "emails": ["rob@example.com"] },
                { "id": "joey", "name": "Joey Locker",
                  "phones": ["+15550002222"], "emails": [] },
                { "id": "sam", "name": "Sam Pike", "phones": ["555-000-3333"], "emails": [] }
              ],
              "projects": [
                { "id": "agstaff", "name": "agstaff", "folder": "~/Projects/agstaff",
                  "repo": "agstaff", "people": ["rob", "joey"],
                  "email": { "account": "operator@example.com" } },
                { "id": "acres", "name": "Pristine Acres", "folder": "~/Projects/acres",
                  "people": ["rob"] }
              ]
            }"#,
        )
        .expect("the fixture is a valid file")
    }

    /// The first — and, in this fixture, only — copy of an address an owner holds.
    fn contact(value: &str, owner: Option<&str>) -> CardId {
        CardId::Contact { owner: owner.map(str::to_owned), value: value.to_owned(), index: 0 }
    }

    #[test]
    fn the_file_becomes_three_lanes_with_the_dead_ends_below_the_fold() {
        let board = Board::from_file(&fixture());

        // Contacts lane: the three that route, then Sam's number, then the two the
        // operator owns.
        let lane = board.lane(Lane::Contacts);
        assert_eq!(lane.len(), 6, "{lane:?}");
        assert_eq!(lane[0], contact("+15550001111", Some("rob")));
        assert_eq!(lane[2], contact("rob@example.com", Some("rob")), "the three that route");
        assert_eq!(lane[3], contact("555-000-3333", Some("sam")), "then the band that does not");
        assert_eq!(lane[4], contact("+15559990000", None), "and the operator's own last");
        // A band is a run, not a label sprinkled through the lane.
        let bands: Vec<Band> = board.contacts.iter().map(|card| card.band).collect();
        assert_eq!(
            bands,
            [Band::Routes, Band::Routes, Band::Routes, Band::Nowhere, Band::Mine, Band::Mine]
        );

        // People lane: the two on a project, then Sam.
        assert_eq!(
            board.lane(Lane::People),
            [
                CardId::Person("rob".into()),
                CardId::Person("joey".into()),
                CardId::Person("sam".into())
            ]
        );
        assert_eq!(board.people[2].band, Band::Nowhere, "Sam is on no project");
        assert_eq!(board.people[0].phones, 1);
        assert_eq!(board.people[0].emails, 1);

        // Projects lane keeps file order and carries the repo.
        assert_eq!(board.projects[0].repo.as_deref(), Some("agstaff"));
        assert_eq!(board.projects[1].repo, None);
        assert_eq!(board.projects[1].people, 1);
    }

    #[test]
    fn a_warning_sits_on_the_card_it_is_about() {
        let board = Board::from_file(&fixture());

        let sam = board
            .contacts
            .iter()
            .find(|card| card.value == "555-000-3333")
            .expect("Sam's number");
        assert!(sam.warning.as_deref().expect("a warning").contains("E.164"));

        // The two that route say nothing.
        assert!(board.contacts.iter().filter(|c| c.band == Band::Routes).all(|c| c.warning.is_none()));

        let joey = &board.people[1];
        assert!(joey.warning.is_none(), "one phone is enough to be reachable");
    }

    #[test]
    fn a_project_with_nobody_on_it_says_so_on_its_own_card() {
        let file = PeopleFile::parse(
            r#"{ "people": [], "projects": [ { "id": "orphan", "name": "Orphan" } ] }"#,
        )
        .expect("an empty project loads");
        let board = Board::from_file(&file);
        assert_eq!(
            board.projects[0].warning.as_deref(),
            Some("nobody on it, so nothing routes here")
        );
    }

    #[test]
    fn every_contact_reaches_its_person_and_every_person_their_projects() {
        let board = Board::from_file(&fixture());

        // Four person-held contacts, plus rob×2 and joey×1 project links.
        assert_eq!(board.links.len(), 4 + 3, "{:?}", board.links);
        assert!(board.links.contains(&Link {
            from: contact("rob@example.com", Some("rob")),
            to: CardId::Person("rob".into()),
        }));
        assert!(board.links.contains(&Link {
            from: CardId::Person("rob".into()),
            to: CardId::Project("acres".into()),
        }));
        // The operator's own addresses belong to nobody, so they join nothing.
        assert!(!board.links.iter().any(|link| link.from == contact("+15559990000", None)));
    }

    /// Two people who wrote the same number down, and one of them who wrote it down
    /// twice. Every card on this board has to keep an identity of its own: cards
    /// sharing one would share one element id, one set of bounds, one wire, and one
    /// selection, and the picture would say two people are one.
    #[test]
    fn one_number_on_two_people_is_two_cards_two_wires_and_two_selections() {
        let file = PeopleFile::parse(
            r#"{
              "people": [
                { "id": "rob", "name": "Rob", "phones": ["+15550001111"] },
                { "id": "joey", "name": "Joey",
                  "phones": ["+15550001111", "+15550001111"] }
              ],
              "projects": []
            }"#,
        )
        .expect("a shared number is a legal file");
        let board = Board::from_file(&file);

        let shared: Vec<&CardId> = board
            .contacts
            .iter()
            .filter(|card| card.value == "+15550001111")
            .map(|card| &card.id)
            .collect();
        assert_eq!(shared.len(), 3, "{:?}", board.contacts);

        let mut keys: Vec<String> = board.contacts.iter().map(|card| card.id.key()).collect();
        keys.sort();
        let total = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), total, "two cards share an element id");

        // Joey's two copies are one number twice, not one card.
        assert_eq!(
            board.contacts.iter().filter(|card| card.owner.as_deref() == Some("joey")).count(),
            2
        );

        // One wire per card, so a duplicate is visible rather than hidden under itself.
        assert_eq!(board.links.len(), 3, "{:?}", board.links);
        for id in &shared {
            assert_eq!(board.links.iter().filter(|link| &&link.from == id).count(), 1);
        }

        // And selecting one of Joey's copies leaves the other alone.
        let joeys: Vec<&CardId> = shared
            .iter()
            .copied()
            .filter(|id| matches!(id, CardId::Contact { owner: Some(who), .. } if who == "joey"))
            .collect();
        let lit = board.connected(joeys[0]);
        assert!(lit.contains(joeys[0]));
        assert!(!lit.contains(joeys[1]), "the other copy is a card of its own");
    }

    #[test]
    fn selecting_a_contact_lights_its_person_and_that_persons_projects_and_stops() {
        let board = Board::from_file(&fixture());
        let lit = board.connected(&contact("+15550001111", Some("rob")));

        assert!(lit.contains(&contact("+15550001111", Some("rob"))));
        assert!(lit.contains(&CardId::Person("rob".into())));
        assert!(lit.contains(&CardId::Project("agstaff".into())));
        assert!(lit.contains(&CardId::Project("acres".into())));
        // It stops there: Joey is on agstaff, but nothing about Rob's number is his.
        assert!(!lit.contains(&CardId::Person("joey".into())));
        assert!(!lit.contains(&contact("rob@example.com", Some("rob"))), "the other address too");
        assert_eq!(lit.len(), 4);
    }

    #[test]
    fn selecting_a_person_lights_both_sides_of_them() {
        let board = Board::from_file(&fixture());
        let lit = board.connected(&CardId::Person("rob".into()));

        assert!(lit.contains(&contact("+15550001111", Some("rob"))));
        assert!(lit.contains(&contact("rob@example.com", Some("rob"))));
        assert!(lit.contains(&CardId::Project("agstaff".into())));
        assert!(lit.contains(&CardId::Project("acres".into())));
        assert!(!lit.contains(&CardId::Person("joey".into())));
        assert_eq!(lit.len(), 5);
    }

    #[test]
    fn selecting_a_project_lights_its_people_and_their_addresses() {
        let board = Board::from_file(&fixture());
        let lit = board.connected(&CardId::Project("agstaff".into()));

        assert!(lit.contains(&CardId::Person("rob".into())));
        assert!(lit.contains(&CardId::Person("joey".into())));
        assert!(lit.contains(&contact("+15550002222", Some("joey"))));
        assert!(lit.contains(&contact("rob@example.com", Some("rob"))));
        // Rob is on Pristine Acres too; selecting agstaff does not light it.
        assert!(!lit.contains(&CardId::Project("acres".into())));
        assert_eq!(lit.len(), 6);
    }

    #[test]
    fn one_of_the_operators_own_addresses_lights_up_alone() {
        let board = Board::from_file(&fixture());
        let lit = board.connected(&contact("+15559990000", None));
        assert_eq!(lit.len(), 1, "it belongs to nobody, which is the point of it");
    }
}
