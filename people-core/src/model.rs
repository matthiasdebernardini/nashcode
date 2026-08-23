//! The file itself: what is in it, and what is wrong with it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The whole file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeopleFile {
    /// The operator's own emails and phones. They never score, so a thread the
    /// operator is on does not match every project they belong to.
    pub me: Vec<String>,
    pub people: Vec<Person>,
    pub projects: Vec<Project>,
}

/// One human. `id` is the join key a project refers to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Person {
    pub id: String,
    pub name: String,
    /// E.164, e.g. `+15550001111`.
    pub phones: Vec<String>,
    pub emails: Vec<String>,
}

/// One project: a folder to file into, the people who ask about it, and what each
/// inbox needs to know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub folder: String,
    /// The nashcode repo. Absent for a GitHub-only client: meetings and email then
    /// have nowhere to file, and the consumer says so rather than guessing.
    pub repo: Option<String>,
    /// Person ids, in the order they were written.
    pub people: Vec<String>,
    /// iMessage group ids. Matched in Swift, before participants; nothing here reads
    /// them.
    pub chat_ids: Vec<String>,
    pub imsg: Imsg,
    pub email: Email,
}

/// What the iMessage router needs per project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Imsg {
    pub prompt: String,
    pub enrich: bool,
    pub media_only: bool,
}

/// Enrichment on, media-only off: the settings a new project wants, so a project
/// added by hand with no `imsg` block behaves like the ones already there.
impl Default for Imsg {
    fn default() -> Self {
        Self { prompt: String::new(), enrich: true, media_only: false }
    }
}

/// What the email pusher needs per project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Email {
    /// The mailbox to search. It is the operator's own address, so it never scores.
    pub account: Option<String>,
    /// A hand-written Gmail query that replaces the one built from the project's
    /// people.
    pub query: Option<String>,
}

/// One thing wrong with the file, in a sentence a person can act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The file is refused rather than loaded: the join key is broken.
    pub fatal: bool,
    pub text: String,
}

impl Finding {
    fn fatal(text: impl Into<String>) -> Self {
        Self { fatal: true, text: text.into() }
    }

    fn warn(text: impl Into<String>) -> Self {
        Self { fatal: false, text: text.into() }
    }
}

impl PeopleFile {
    /// Parse the file and refuse it when a finding is fatal.
    ///
    /// The error is every fatal finding, one per line, because a hand-edited file
    /// usually has all of them at once and a person fixing it wants the list.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let refused: Vec<String> = file
            .validate()
            .into_iter()
            .filter(|finding| finding.fatal)
            .map(|finding| finding.text)
            .collect();
        if refused.is_empty() { Ok(file) } else { Err(refused.join("\n")) }
    }

    /// Everything wrong with the file. See the crate docs for fatal versus warning.
    pub fn validate(&self) -> Vec<Finding> {
        let mut findings = Vec::new();

        let mut seen_people: BTreeSet<&str> = BTreeSet::new();
        for person in &self.people {
            if person.id.trim().is_empty() {
                findings.push(Finding::fatal(format!(
                    "a person has no id (name {:?}); give them one, because a project can \
                     only refer to an id",
                    person.name
                )));
            } else if !seen_people.insert(person.id.as_str()) {
                findings.push(Finding::fatal(format!(
                    "two people share the id {:?}; rename one and update every project that \
                     lists it",
                    person.id
                )));
            }
            if person.phones.is_empty() && person.emails.is_empty() {
                findings.push(Finding::warn(format!(
                    "{:?} has neither a phone nor an email, so no message can ever match them",
                    label(&person.id, &person.name)
                )));
            }
            for phone in &person.phones {
                if !is_e164(phone) {
                    findings.push(Finding::warn(format!(
                        "{:?} has the phone {phone:?}, which is not E.164; write it as a plus \
                         sign, the country code, and digits, e.g. +15550001111",
                        label(&person.id, &person.name)
                    )));
                }
            }
        }

        // One human written down twice is not a duplicate id, so nothing above catches
        // it — and it costs a project two points where it should score one.
        let mut owners: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for person in &self.people {
            for value in person.phones.iter().chain(person.emails.iter()) {
                let normalised = crate::route::normalize(value);
                if normalised.is_empty() {
                    continue;
                }
                let holders = owners.entry(normalised).or_default();
                if !holders.contains(&person.id.as_str()) {
                    holders.push(person.id.as_str());
                }
            }
        }
        for (value, holders) in &owners {
            if holders.len() > 1 {
                findings.push(Finding::warn(format!(
                    "{value:?} is on {} people ({}); one human written down twice scores \
                     twice, so merge them into one id",
                    holders.len(),
                    holders.join(", ")
                )));
            }
        }

        // An address of the operator's own never scores, whoever else has it written
        // down — so a person carrying one is unreachable by it and does not know why.
        let own = self.own_addresses();
        for person in &self.people {
            for value in person.phones.iter().chain(person.emails.iter()) {
                if own.contains(&crate::route::normalize(value)) {
                    findings.push(Finding::warn(format!(
                        "{:?} has {value:?}, which is also yours (it is in `me`, or it is a \
                         project's mail account); your own addresses never score, so nobody \
                         can be matched by it",
                        label(&person.id, &person.name)
                    )));
                }
            }
        }

        let mut seen_projects: BTreeSet<&str> = BTreeSet::new();
        for project in &self.projects {
            if project.id.trim().is_empty() {
                findings.push(Finding::fatal(format!(
                    "a project has no id (name {:?}); give it one, because the id is what an \
                     answer names",
                    project.name
                )));
            } else if !seen_projects.insert(project.id.as_str()) {
                findings.push(Finding::fatal(format!(
                    "two projects share the id {:?}; give one of them a new id",
                    project.id
                )));
            }
            if project.people.is_empty() {
                findings.push(Finding::warn(format!(
                    "project {:?} lists no people, so nothing can ever route to it",
                    label(&project.id, &project.name)
                )));
            }
            for id in &project.people {
                if !self.people.iter().any(|person| person.id == *id) {
                    findings.push(Finding::fatal(format!(
                        "project {:?} lists the person id {id:?}, which no person has; add the \
                         person or fix the id",
                        label(&project.id, &project.name)
                    )));
                }
            }
        }

        findings
    }

    /// The file as it is written to disk: two-space JSON, fields in struct order.
    pub fn to_pretty_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// `^\+[1-9]\d{1,14}$`, checked by hand rather than by pulling in a regex engine for
/// one pattern.
pub fn is_e164(phone: &str) -> bool {
    let Some(digits) = phone.strip_prefix('+') else {
        return false;
    };
    let mut chars = digits.chars();
    match chars.next() {
        Some(first) if first.is_ascii_digit() && first != '0' => {}
        _ => return false,
    }
    let rest = digits.len() - 1;
    (1..=14).contains(&rest) && digits.chars().all(|c| c.is_ascii_digit())
}

/// What to call something: its name when it has one, else its id.
pub(crate) fn label<'a>(id: &'a str, name: &'a str) -> &'a str {
    if name.trim().is_empty() { id } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dangling_person_id_is_refused_rather_than_loaded() {
        let error = PeopleFile::parse(
            r#"{ "people": [], "projects": [
                 { "id": "agstaff", "people": ["rob"] } ] }"#,
        )
        .expect_err("a project naming nobody is not a file to route with");
        assert!(error.contains("\"rob\""), "{error}");
        assert!(error.contains("no person has"), "{error}");
    }

    #[test]
    fn two_people_with_one_id_are_refused() {
        let error = PeopleFile::parse(
            r#"{ "people": [ { "id": "rob" }, { "id": "rob" } ], "projects": [] }"#,
        )
        .expect_err("the join key has to be unique");
        assert!(error.contains("share the id"), "{error}");
    }

    #[test]
    fn two_projects_with_one_id_are_refused() {
        let error = PeopleFile::parse(
            r#"{ "people": [], "projects": [ { "id": "agstaff" }, { "id": "agstaff" } ] }"#,
        )
        .expect_err("an answer names a project by its id");
        assert!(error.contains("two projects share the id"), "{error}");
    }

    #[test]
    fn a_phone_that_is_not_e164_loads_and_is_reported() {
        let file = PeopleFile::parse(
            r#"{ "people": [ { "id": "rob", "name": "Rob Castro",
                               "phones": ["(555) 000-1111"] } ],
                 "projects": [ { "id": "agstaff", "people": ["rob"] } ] }"#,
        )
        .expect("a bad phone number is worth saying, not worth refusing");
        let findings = file.validate();
        assert!(findings.iter().all(|f| !f.fatal), "{findings:?}");
        assert!(
            findings.iter().any(|f| f.text.contains("E.164") && f.text.contains("Rob Castro")),
            "{findings:?}"
        );
    }

    #[test]
    fn e164_is_a_plus_a_leading_digit_and_up_to_fifteen_digits() {
        assert!(is_e164("+15550001111"));
        assert!(is_e164("+12"));
        assert!(is_e164("+123456789012345"));
        assert!(!is_e164("+1234567890123456"), "sixteen digits is not a number");
        assert!(!is_e164("+1"), "a country code alone is not a number");
        assert!(!is_e164("+05550001111"), "no country code starts with zero");
        assert!(!is_e164("15550001111"), "the plus is not optional");
        assert!(!is_e164("+1 555 000 1111"), "spaces are not digits");
        assert!(!is_e164(""));
    }

    #[test]
    fn a_project_with_nobody_in_it_is_a_warning() {
        let file = PeopleFile::parse(
            r#"{ "people": [], "projects": [ { "id": "orphan", "name": "Orphan" } ] }"#,
        )
        .expect("an empty project loads");
        let findings = file.validate();
        assert!(
            findings.iter().any(|f| !f.fatal && f.text.contains("lists no people")),
            "{findings:?}"
        );
    }

    #[test]
    fn a_person_nobody_can_reach_is_a_warning() {
        let file = PeopleFile::parse(
            r#"{ "people": [ { "id": "ghost", "name": "Ghost" } ],
                 "projects": [ { "id": "p", "people": ["ghost"] } ] }"#,
        )
        .expect("a person with no address loads");
        let findings = file.validate();
        assert!(
            findings.iter().any(|f| !f.fatal && f.text.contains("neither a phone nor an email")),
            "{findings:?}"
        );
    }

    #[test]
    fn one_number_on_two_people_is_a_warning() {
        // Two records of one human: the project scores two where it should score one.
        let file = PeopleFile::parse(
            r#"{ "people": [
                   { "id": "rob", "name": "Rob Castro", "phones": ["+15550001111"] },
                   { "id": "rob-old", "name": "Rob", "phones": [" +15550001111 "],
                     "emails": ["rob@example.com"] },
                   { "id": "joey", "emails": ["ROB@example.com"] } ],
                 "projects": [ { "id": "agstaff", "people": ["rob", "rob-old"] } ] }"#,
        )
        .expect("two records of one person still load");
        let findings = file.validate();
        assert!(
            findings
                .iter()
                .any(|f| !f.fatal && f.text.contains("\"+15550001111\"") && f.text.contains("rob, rob-old")),
            "the same number, one of them padded: {findings:?}"
        );
        assert!(
            findings
                .iter()
                .any(|f| !f.fatal && f.text.contains("\"rob@example.com\"") && f.text.contains("rob-old, joey")),
            "and the same address in two cases: {findings:?}"
        );
        // A number spaced out is a different string, and E.164 already says so; this
        // warning is about one address on two ids, not about spelling.
        assert!(
            !findings.iter().any(|f| f.text.contains("is on 1 people")),
            "{findings:?}"
        );
    }

    #[test]
    fn one_of_your_own_addresses_on_a_person_is_a_warning() {
        let file = PeopleFile::parse(
            r#"{ "me": ["matthias@example.com"],
                 "people": [
                   { "id": "rob", "name": "Rob Castro",
                     "emails": ["Matthias@Example.com"], "phones": ["+15559990000"] } ],
                 "projects": [ { "id": "agstaff", "people": ["rob"],
                                 "email": { "account": "+15559990000" } } ] }"#,
        )
        .expect("it loads; it just will not match");
        let findings = file.validate();
        let own: Vec<&str> = findings
            .iter()
            .filter(|f| !f.fatal && f.text.contains("which is also yours"))
            .map(|f| f.text.as_str())
            .collect();
        assert_eq!(own.len(), 2, "the `me` entry and the project's account: {findings:?}");
        assert!(own[0].contains("Rob Castro"), "{own:?}");
    }

    #[test]
    fn a_missing_block_takes_the_default_rather_than_failing() {
        let file = PeopleFile::parse(r#"{ "projects": [ { "id": "solo" } ] }"#)
            .expect("a hand-edited file with half the keys still loads");
        assert!(file.people.is_empty());
        assert!(file.projects[0].imsg.enrich, "enrichment is on unless it is turned off");
        assert!(!file.projects[0].imsg.media_only);
        assert_eq!(file.projects[0].email.account, None);
    }

    #[test]
    fn the_written_file_reads_back_the_same() {
        let file = crate::route::tests::fixture();
        let text = file.to_pretty_json();
        assert!(text.contains("  \"me\": ["), "two-space indent: {text}");
        assert_eq!(PeopleFile::parse(&text).expect("round trip"), file);
    }
}
