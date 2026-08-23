//! The one matching rule: contacts in, projects out.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::PeopleFile;

/// What a caller knows about someone: an address or a number, one of the two.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Contact {
    /// Absent rather than `null` on the wire: a match carries a list of these, and a
    /// reader wants the one key that is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
}

impl Contact {
    pub fn email(address: &str) -> Self {
        Self { email: Some(address.to_owned()), phone: None }
    }

    pub fn phone(number: &str) -> Self {
        Self { email: None, phone: Some(number.to_owned()) }
    }
}

/// One project the contacts reach, and who in it they reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    pub project: String,
    pub repo: Option<String>,
    pub folder: String,
    /// The matched person ids, in file order.
    pub people: Vec<String>,
    /// The caller's own contacts that matched somebody here, normalised, in the order
    /// the caller gave them. A client knows an attendee by the address it sent, not by
    /// a person id, so this is what a reason line can name.
    pub contacts: Vec<Contact>,
    pub score: usize,
}

/// The answer: the ranking, and whether the top of it is shared.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routing {
    pub matches: Vec<Match>,
    /// Two or more projects share the top score, so nothing here decides. The caller
    /// asks the operator instead of picking the first row.
    pub tie: bool,
}

impl PeopleFile {
    /// Which projects these contacts reach.
    ///
    /// A project scores one point per distinct person of its own that any contact
    /// matches, by email or by phone. Highest score first; equal scores keep file
    /// order, and the answer says so with `tie`. The operator's own addresses — `me`
    /// and every project's `email.account` — never score, because they are on
    /// everything and would make every project match.
    pub fn route(&self, contacts: &[Contact]) -> Routing {
        let excluded = self.own_addresses();

        // Normalised, in the order given, without the operator's own addresses and
        // without repeats. Order survives all the way to the answer, so a caller can
        // pair a contact with the attendee it came from.
        let mut asked: Vec<Contact> = Vec::new();
        for contact in contacts {
            let email = contact.email.as_deref().map(normalize).filter(|value| {
                !value.is_empty() && !excluded.contains(value)
            });
            let phone = contact.phone.as_deref().map(normalize).filter(|value| {
                !value.is_empty() && !excluded.contains(value)
            });
            if email.is_none() && phone.is_none() {
                continue;
            }
            let asked_for = Contact { email, phone };
            if !asked.contains(&asked_for) {
                asked.push(asked_for);
            }
        }
        let emails: BTreeSet<&str> =
            asked.iter().filter_map(|contact| contact.email.as_deref()).collect();
        let phones: BTreeSet<&str> =
            asked.iter().filter_map(|contact| contact.phone.as_deref()).collect();

        let mut matches: Vec<Match> = Vec::new();
        for project in &self.projects {
            let mut matched: Vec<String> = Vec::new();
            for id in &project.people {
                // A project that lists an id twice still counts it once: the score is
                // people reached, not lines written.
                if matched.iter().any(|seen| seen == id) {
                    continue;
                }
                let Some(person) = self.people.iter().find(|person| person.id == *id) else {
                    continue;
                };
                let reached = person
                    .emails
                    .iter()
                    .any(|address| emails.contains(normalize(address).as_str()))
                    || person
                        .phones
                        .iter()
                        .any(|number| phones.contains(normalize(number).as_str()));
                if reached {
                    matched.push(id.clone());
                }
            }
            if matched.is_empty() {
                continue;
            }
            // Which of the caller's contacts did the reaching, so a reason line can
            // name a person by what the caller already knows them as.
            let reached_by: Vec<Contact> = asked
                .iter()
                .filter(|contact| {
                    matched.iter().any(|id| {
                        self.people
                            .iter()
                            .find(|person| person.id == *id)
                            .is_some_and(|person| person_answers(person, contact))
                    })
                })
                .cloned()
                .collect();
            matches.push(Match {
                project: project.id.clone(),
                repo: project.repo.clone(),
                folder: project.folder.clone(),
                score: matched.len(),
                people: matched,
                contacts: reached_by,
            });
        }

        // `sort_by_key` is stable, so equal scores come back in file order.
        matches.sort_by_key(|hit| std::cmp::Reverse(hit.score));
        let tie = matches.len() > 1 && matches[0].score == matches[1].score;
        Routing { matches, tie }
    }

    /// Is this address one of the operator's own — in `me`, or some project's mail
    /// account? Such an address never scores, so a caller that shows why a project won
    /// can say why an address counted for nothing.
    pub fn is_own(&self, value: &str) -> bool {
        self.own_addresses().contains(&normalize(value))
    }

    /// `me` plus every project's mail account, normalised. Everything here belongs to
    /// the operator, and the operator is on every thread.
    pub(crate) fn own_addresses(&self) -> BTreeSet<String> {
        let mut own: BTreeSet<String> = self.me.iter().map(|entry| normalize(entry)).collect();
        for project in &self.projects {
            if let Some(account) = &project.email.account {
                own.insert(normalize(account));
            }
        }
        own.remove("");
        own
    }
}

/// Is this normalised contact one of the person's own addresses?
fn person_answers(person: &crate::model::Person, contact: &Contact) -> bool {
    let by_email = contact.email.as_deref().is_some_and(|address| {
        person.emails.iter().any(|known| normalize(known) == address)
    });
    let by_phone = contact.phone.as_deref().is_some_and(|number| {
        person.phones.iter().any(|known| normalize(known) == number)
    });
    by_email || by_phone
}

/// Lowercase and trimmed. Emails compare case-insensitively; a phone has no case, so
/// one rule covers both and a hand-typed `+1 555…` never half-matches.
///
/// Public because comparing addresses is not routing's private business: the desktop
/// app compares what a person typed against what is in the file, and a second spelling
/// of this rule is a second answer.
pub fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Two projects, three people. Rob is in both, which is what makes a tie possible.
    pub(crate) fn fixture() -> PeopleFile {
        PeopleFile::parse(
            r#"{
              "me": ["matthias@example.com", "+15559990000"],
              "people": [
                { "id": "rob", "name": "Rob Castro",
                  "phones": ["+15550001111"], "emails": ["rob@example.com"] },
                { "id": "joey", "name": "Joey Locker",
                  "phones": ["+15550002222"], "emails": ["joey@example.com"] },
                { "id": "brad", "name": "Brad Thompson",
                  "phones": [], "emails": ["brad@example.com"] }
              ],
              "projects": [
                { "id": "agstaff", "name": "agstaff", "folder": "~/Projects/agstaff",
                  "repo": "agstaff", "people": ["rob", "joey"],
                  "email": { "account": "matthias@example.com" } },
                { "id": "acres", "name": "Pristine Acres", "folder": "~/Projects/acres",
                  "people": ["brad", "rob"] }
              ]
            }"#,
        )
        .expect("the fixture is a valid file")
    }

    #[test]
    fn one_winner_carries_every_person_it_matched() {
        let routing = fixture().route(&[
            Contact::email("rob@example.com"),
            Contact::email("joey@example.com"),
        ]);
        assert!(!routing.tie);
        assert_eq!(routing.matches.len(), 2, "{routing:?}");
        assert_eq!(routing.matches[0].project, "agstaff");
        assert_eq!(routing.matches[0].score, 2);
        assert_eq!(routing.matches[0].people, ["rob", "joey"]);
        assert_eq!(routing.matches[0].repo.as_deref(), Some("agstaff"));
        assert_eq!(routing.matches[0].folder, "~/Projects/agstaff");
        // The addresses the caller asked with, so it can name who it recognised.
        assert_eq!(
            routing.matches[0].contacts,
            [Contact::email("rob@example.com"), Contact::email("joey@example.com")]
        );
        // Rob is in the second project too, so it is an answer — a weaker one, and
        // only Rob's address did the reaching.
        assert_eq!(routing.matches[1].project, "acres");
        assert_eq!(routing.matches[1].score, 1);
        assert_eq!(routing.matches[1].repo, None);
        assert_eq!(routing.matches[1].contacts, [Contact::email("rob@example.com")]);
    }

    #[test]
    fn nobody_known_means_no_match_at_all() {
        let routing = fixture().route(&[Contact::email("stranger@example.com")]);
        assert!(routing.matches.is_empty());
        assert!(!routing.tie);
    }

    #[test]
    fn an_equal_score_keeps_file_order_and_says_it_is_a_tie() {
        // Rob alone reaches both projects with one point each.
        let routing = fixture().route(&[Contact::phone("+15550001111")]);
        assert!(routing.tie, "{routing:?}");
        assert_eq!(routing.matches.len(), 2);
        assert_eq!(routing.matches[0].project, "agstaff", "file order breaks the tie for nobody");
        assert_eq!(routing.matches[1].project, "acres");
        assert_eq!(routing.matches[0].score, routing.matches[1].score);
    }

    #[test]
    fn the_operators_own_addresses_never_score() {
        // `me` and the project's mail account are on every thread there is.
        let routing = fixture().route(&[
            Contact::email("Matthias@Example.com"),
            Contact::phone("+15559990000"),
        ]);
        assert!(routing.matches.is_empty(), "{routing:?}");

        // And in the shape that actually arrives: the operator on the thread with a
        // client. Rob scores; the operator is not even in the reason.
        let routing = fixture().route(&[
            Contact::email("matthias@example.com"),
            Contact::email("rob@example.com"),
        ]);
        assert_eq!(routing.matches[0].project, "agstaff");
        assert_eq!(routing.matches[0].score, 1, "{routing:?}");
        assert_eq!(routing.matches[0].people, ["rob"]);
        assert_eq!(routing.matches[0].contacts, [Contact::email("rob@example.com")]);
        assert!(fixture().is_own("Matthias@Example.com"));
        assert!(!fixture().is_own("rob@example.com"));
    }

    #[test]
    fn a_project_that_lists_one_person_twice_scores_once() {
        let file = PeopleFile::parse(
            r#"{ "people": [ { "id": "rob", "name": "Rob Castro",
                               "emails": ["rob@example.com"] } ],
                 "projects": [ { "id": "agstaff", "people": ["rob", "rob"] } ] }"#,
        )
        .expect("a repeated id is a typo, not a broken join");
        let routing = file.route(&[Contact::email("rob@example.com")]);
        assert_eq!(routing.matches[0].score, 1, "{routing:?}");
        assert_eq!(routing.matches[0].people, ["rob"]);
        assert!(!routing.tie, "one project cannot tie with itself");
    }

    #[test]
    fn an_address_matches_whatever_case_it_arrives_in() {
        let routing = fixture().route(&[Contact::email("  ROB@Example.COM ")]);
        assert_eq!(routing.matches[0].people, ["rob"]);
        assert_eq!(
            routing.matches[0].contacts,
            [Contact::email("rob@example.com")],
            "the contact comes back normalised, not as it was typed"
        );
    }

    #[test]
    fn a_contact_that_reached_nobody_here_is_not_in_this_match() {
        // Joey is only in the first project; the second one owes his address nothing.
        let routing = fixture().route(&[
            Contact::email("joey@example.com"),
            Contact::email("brad@example.com"),
        ]);
        let acres = routing.matches.iter().find(|m| m.project == "acres").expect("acres");
        assert_eq!(acres.contacts, [Contact::email("brad@example.com")]);
    }

    #[test]
    fn one_person_reached_twice_is_still_one_point() {
        let routing = fixture().route(&[
            Contact::email("rob@example.com"),
            Contact::phone("+15550001111"),
        ]);
        assert_eq!(routing.matches[0].project, "agstaff");
        assert_eq!(routing.matches[0].score, 1, "{routing:?}");
    }
}
