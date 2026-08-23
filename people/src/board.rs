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
//!
//! ## Order, and how much of it is drawn
//!
//! Never alphabetical. Every lane is ordered by [`people_core::by_frecency`] — a
//! `seen` count halved every fortnight — so the client who wrote this morning is at
//! the top and the one who left in March is at the bottom. The contacts lane has no
//! frecency of its own: an address is warm because the person holding it is, so it
//! follows the people lane's order.
//!
//! With thirty-five client folders the picture stops being a picture, so a lane past
//! [`EXPANDED`] draws its warm head in full and collapses the tail to one line each.
//! [`Board::expanded`] is that rule and it is pure: a lane's shape is a function of its
//! order and the selection, and nothing about a frame.

use std::collections::{BTreeMap, BTreeSet};

use people_core::{
    ContactKind, ContactRow, PeopleFile, Seen, by_frecency, contact_map, model::is_e164,
    seen_label,
};

use crate::edit::display;

/// How many cards a lane draws in full before the rest collapse to one line.
///
/// Ten is what fits above the fold beside an inspector at the window's opening height,
/// which is the number that matters: the eleventh full card is one the operator has to
/// scroll to anyway, and a name is enough to find it by.
pub const EXPANDED: usize = 10;

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
    /// `3× · 2d ago`, when something has matched them. The number the order is made
    /// of, said out loud, so a lane the operator did not sort is still one they can
    /// check.
    pub warmth: Option<String>,
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
    /// See [`PersonCard::warmth`].
    pub warmth: Option<String>,
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
    /// Read the file into the picture, as of now.
    ///
    /// The clock is read once, here, and handed to every ordering below it: three
    /// lanes measured a millisecond apart would be three orders that disagreed about
    /// the same file.
    pub fn from_file(file: &PeopleFile) -> Self {
        Self::at(file, &now())
    }

    /// The same, at a stated instant, which is what makes the order testable.
    pub fn at(file: &PeopleFile, now: &str) -> Self {
        // Warmest first. `by_frecency` is stable, so everything nobody has seen keeps
        // the order the operator wrote it in.
        let warm_people = by_frecency(&file.people, |person| person.seen.as_ref(), now);
        let warm_projects = by_frecency(&file.projects, |project| project.seen.as_ref(), now);

        // An address has no warmth of its own; it inherits its holder's place. A `me`
        // entry belongs to nobody, so it sorts last — and its band puts it there too.
        let rank: BTreeMap<&str, usize> = warm_people
            .iter()
            .enumerate()
            .map(|(place, person)| (person.id.as_str(), place))
            .collect();

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
        // Two stable passes, warmth then band, so a card lands in its band and reads
        // warmest-first inside it. Sorted here, once, rather than at every reading of
        // the lane: the canvas draws a lane top to bottom, and a band is a run of it.
        contacts.sort_by_key(|card| {
            card.owner.as_deref().and_then(|owner| rank.get(owner)).copied().unwrap_or(usize::MAX)
        });
        contacts.sort_by_key(|card| card.band.rank());

        let mut people: Vec<PersonCard> = warm_people
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
                    warmth: warmth(person.seen.as_ref(), now),
                    warning: if reachable {
                        None
                    } else {
                        Some("no phone and no email, so nothing can match them".to_owned())
                    },
                }
            })
            .collect();
        people.sort_by_key(|card| card.band.rank());

        let projects: Vec<ProjectCard> = warm_projects
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
                    warmth: warmth(project.seen.as_ref(), now),
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
        // Warmest project first, so the wires arrive in the order the lane draws.
        for project in &warm_projects {
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

    /// Which of this lane's cards are drawn in full; the rest are one line each.
    pub fn expanded(&self, lane: Lane, selected: Option<&CardId>) -> BTreeSet<CardId> {
        let lit = selected.map(|id| self.connected(id));
        expanded_ids(&self.lane(lane), lit.as_ref())
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

/// Which cards a lane of `order` draws in full.
///
/// A short lane draws all of it: collapsing five projects would hide nothing and cost
/// the picture its shape. A long one draws its warm head — the first [`EXPANDED`],
/// which the lane's own order has already put there — and the tail as one line each,
/// **plus** everything `lit` reaches. That exception is the whole point: a project
/// thirtieth by frecency is still the one the operator just clicked, and a chain that
/// collapsed halfway through would answer "where does this route?" with half a wire.
pub fn expanded_ids(order: &[CardId], lit: Option<&BTreeSet<CardId>>) -> BTreeSet<CardId> {
    if order.len() <= EXPANDED {
        return order.iter().cloned().collect();
    }
    let mut open: BTreeSet<CardId> = order.iter().take(EXPANDED).cloned().collect();
    if let Some(lit) = lit {
        open.extend(order.iter().filter(|id| lit.contains(id)).cloned());
    }
    open
}

/// `3× · 2d ago`, in the one spelling `nashcode people ls` uses.
fn warmth(seen: Option<&Seen>, now: &str) -> Option<String> {
    seen_label(seen, now)
}

/// Now, in UTC, spelled the way a `seen.last` is spelled.
///
/// UTC and not local time: the file is compared against stamps the routers write on
/// this machine and stamps the viewer writes on another, and one offset for both is
/// the only way the arithmetic holds.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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
        let board = Board::at(&fixture(), NOW);

        // Contacts lane: the three that route — Rob's two together, because a lane of
        // addresses follows the people lane — then Sam's number, then the operator's.
        let lane = board.lane(Lane::Contacts);
        assert_eq!(lane.len(), 6, "{lane:?}");
        assert_eq!(lane[0], contact("+15550001111", Some("rob")));
        assert_eq!(lane[1], contact("rob@example.com", Some("rob")));
        assert_eq!(lane[2], contact("+15550002222", Some("joey")), "the three that route");
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
        let board = Board::at(&fixture(), NOW);

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
        let board = Board::at(&file, NOW);
        assert_eq!(
            board.projects[0].warning.as_deref(),
            Some("nobody on it, so nothing routes here")
        );
    }

    #[test]
    fn every_contact_reaches_its_person_and_every_person_their_projects() {
        let board = Board::at(&fixture(), NOW);

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
        let board = Board::at(&file, NOW);

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
        let board = Board::at(&fixture(), NOW);
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
        let board = Board::at(&fixture(), NOW);
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
        let board = Board::at(&fixture(), NOW);
        let lit = board.connected(&CardId::Project("agstaff".into()));

        assert!(lit.contains(&CardId::Person("rob".into())));
        assert!(lit.contains(&CardId::Person("joey".into())));
        assert!(lit.contains(&contact("+15550002222", Some("joey"))));
        assert!(lit.contains(&contact("rob@example.com", Some("rob"))));
        // Rob is on Pristine Acres too; selecting agstaff does not light it.
        assert!(!lit.contains(&CardId::Project("acres".into())));
        assert_eq!(lit.len(), 6);
    }

    /// Thirteen projects and four people, each with a `seen` that puts it somewhere
    /// definite. Used for the order and for the collapse, because both are about the
    /// same list being longer than a screen.
    fn warm() -> PeopleFile {
        let mut json = String::from(r#"{ "people": ["#);
        // The people, coldest written first, so file order and warmth disagree.
        for (n, last) in [
            (1, "2025-10-27T12:00:00Z"),
            (2, "2026-08-23T12:00:00Z"),
            (3, "2026-06-24T12:00:00Z"),
            (4, "2026-08-09T12:00:00Z"),
        ] {
            json.push_str(&format!(
                r#"{{ "id": "p{n}", "name": "Person {n}", "phones": ["+1555000{n}{n}{n}{n}"],
                     "seen": {{ "count": 4, "last": "{last}" }} }},"#
            ));
        }
        json.pop();
        json.push_str(r#"], "projects": ["#);
        // Thirteen projects: the last written is the warmest, so "top ten" can never
        // be "the first ten in the file".
        for n in 1..=13 {
            json.push_str(&format!(
                r#"{{ "id": "j{n}", "name": "Job {n}", "people": ["p1", "p2", "p3", "p4"],
                     "seen": {{ "count": {n}, "last": "2026-08-23T00:00:00Z" }} }},"#
            ));
        }
        json.pop();
        json.push_str("] }");
        PeopleFile::parse(&json).expect("the warm fixture is a valid file")
    }

    /// Twelve people, one address each, so the contacts lane is longer than the
    /// canvas can draw and its own order decides which twelve.
    fn crowded() -> PeopleFile {
        let mut json = String::from(r#"{ "people": ["#);
        for n in 1..=12 {
            json.push_str(&format!(
                r#"{{ "id": "p{n}", "name": "Person {n}", "phones": ["+1555000{n:04}"],
                     "seen": {{ "count": {n}, "last": "2026-08-23T00:00:00Z" }} }},"#
            ));
        }
        json.pop();
        json.push_str(r#"], "projects": [{ "id": "j1", "name": "Job 1", "people": ["#);
        json.push_str(&(1..=12).map(|n| format!(r#""p{n}""#)).collect::<Vec<_>>().join(","));
        json.push_str("] }] }");
        PeopleFile::parse(&json).expect("the crowded fixture is a valid file")
    }

    const NOW: &str = "2026-08-23T12:00:00Z";

    #[test]
    fn every_lane_is_warmest_first_and_never_alphabetical() {
        let board = Board::at(&warm(), NOW);

        // People: the one seen today, then a fortnight, then two months, then a year.
        let people: Vec<&str> = board.people.iter().map(|card| card.person.as_str()).collect();
        assert_eq!(people, ["p2", "p4", "p3", "p1"]);

        // Projects: same stamp for all thirteen, so the count decides, biggest first.
        let projects: Vec<&str> =
            board.projects.iter().map(|card| card.project.as_str()).collect();
        assert_eq!(projects[0], "j13");
        assert_eq!(projects[12], "j1");

        // And an address is warm because its holder is: the contacts lane follows the
        // people lane rather than the file.
        let owners: Vec<&str> =
            board.contacts.iter().filter_map(|card| card.owner.as_deref()).collect();
        assert_eq!(owners, ["p2", "p4", "p3", "p1"]);
    }

    #[test]
    fn a_card_says_the_warmth_its_place_was_decided_by() {
        let board = Board::at(&warm(), NOW);
        assert_eq!(board.people[0].warmth.as_deref(), Some("4× · now"));
        assert_eq!(board.people[1].warmth.as_deref(), Some("4× · 2w ago"));
        assert_eq!(board.projects[0].warmth.as_deref(), Some("13× · 12h ago"));

        // Nobody has seen the plain fixture, so nothing on it claims to be warm.
        let cold = Board::at(&fixture(), NOW);
        assert!(cold.people.iter().all(|card| card.warmth.is_none()));
        assert!(cold.projects.iter().all(|card| card.warmth.is_none()));
    }

    #[test]
    fn a_lane_that_fits_is_drawn_whole() {
        let board = Board::at(&fixture(), NOW);
        let open = board.expanded(Lane::Projects, None);
        assert_eq!(open.len(), 2, "two projects collapse nothing");
        assert_eq!(board.expanded(Lane::People, None).len(), 3);
    }

    #[test]
    fn a_long_lane_draws_its_warm_head_and_collapses_the_rest() {
        let board = Board::at(&warm(), NOW);
        let open = board.expanded(Lane::Projects, None);

        assert_eq!(open.len(), EXPANDED);
        // The warm head, which is the tail of the file.
        for n in 4..=13 {
            assert!(open.contains(&CardId::Project(format!("j{n}"))), "j{n} is in the top ten");
        }
        for n in 1..=3 {
            assert!(!open.contains(&CardId::Project(format!("j{n}"))), "j{n} is a one-liner");
        }
    }

    #[test]
    fn a_selection_expands_its_whole_chain_however_cold_it_is() {
        let board = Board::at(&warm(), NOW);
        // j1 is thirteenth by frecency: without a selection it is one line.
        let cold = CardId::Project("j1".to_owned());
        assert!(!board.expanded(Lane::Projects, None).contains(&cold));

        let open = board.expanded(Lane::Projects, Some(&cold));
        assert!(open.contains(&cold), "the card that was clicked is drawn in full");
        assert_eq!(open.len(), EXPANDED + 1, "and nothing warm was pushed out to fit it");

        // The chain, not just the card: everybody is on every project, so selecting
        // the coldest one opens the people it routes through in their lane too.
        let people = board.expanded(Lane::People, Some(&cold));
        assert!(people.contains(&CardId::Person("p1".to_owned())));
    }

    /// A lane of addresses is not the exception. The canvas scrolls as one piece, so
    /// a contacts lane that drew all seventy of a thirty-five-client file in full
    /// would push the two lanes beside it off the fold.
    #[test]
    fn a_long_contacts_lane_collapses_like_the_other_two() {
        let board = Board::at(&crowded(), NOW);
        let order = board.lane(Lane::Contacts);
        assert_eq!(order.len(), 12);

        let open = board.expanded(Lane::Contacts, None);
        assert_eq!(open.len(), EXPANDED);
        assert!(open.contains(&order[0]), "the warmest address is drawn in full");

        // The coldest is a one-liner until it is the selection, and then it is not.
        let cold = order[11].clone();
        assert!(!open.contains(&cold));
        let lit = board.expanded(Lane::Contacts, Some(&cold));
        assert!(lit.contains(&cold), "the card that was clicked is drawn in full");
        assert_eq!(lit.len(), EXPANDED + 1, "and nothing warm was pushed out to fit it");
    }

    #[test]
    fn the_rule_itself_is_about_an_order_and_a_selection_and_nothing_else() {
        let order: Vec<CardId> =
            (0..12).map(|n| CardId::Project(format!("j{n}"))).collect();
        assert_eq!(expanded_ids(&order[..EXPANDED], None).len(), EXPANDED, "exactly ten fits");
        assert_eq!(expanded_ids(&order, None).len(), EXPANDED);

        // A lit set naming cards in another lane adds nothing here: the rule takes the
        // intersection, so a person cannot open a project row.
        let elsewhere: BTreeSet<CardId> =
            [CardId::Person("p1".to_owned())].into_iter().collect();
        assert_eq!(expanded_ids(&order, Some(&elsewhere)).len(), EXPANDED);

        let last: BTreeSet<CardId> = [order[11].clone()].into_iter().collect();
        assert_eq!(expanded_ids(&order, Some(&last)).len(), EXPANDED + 1);
    }

    #[test]
    fn one_of_the_operators_own_addresses_lights_up_alone() {
        let board = Board::at(&fixture(), NOW);
        let lit = board.connected(&contact("+15559990000", None));
        assert_eq!(lit.len(), 1, "it belongs to nobody, which is the point of it");
    }
}
