//! The file itself: what is in it, and what is wrong with it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The whole file.
///
/// `Eq` is gone from this family of types because [`Person::extra`] holds arbitrary
/// JSON and a JSON number is a float: two files still compare with `==`, they just do
/// not promise reflexivity over a `NaN` somebody typed by hand.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeopleFile {
    /// The operator's own emails and phones. They never score, so a thread the
    /// operator is on does not match every project they belong to.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub me: Vec<String>,
    /// Directory names `sync-folders` passes over. An exact name, or one `*`:
    /// `deploy-*`, `*-backups`. Compared without case, because the operator types
    /// these and the disk under them does not care either.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub people: Vec<Person>,
    pub projects: Vec<Project>,
    /// Keys nothing here models. See [`Person::extra`].
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// How often something was the answer, and when it last was.
///
/// It lives in the file rather than in a database beside it because the file is the
/// only thing every consumer already reads: the routers, the CLI, and the desktop app
/// each learn what is warm from the same three lines, and a machine that copies
/// `people.json` copies the order with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Seen {
    /// How many times it has matched.
    pub count: u64,
    /// RFC 3339, the last time it did.
    pub last: String,
}

impl Seen {
    /// One more match, now.
    ///
    /// Takes the slot rather than the value so a caller with `Option<Seen>` — which is
    /// every caller, because a thing nobody has seen yet has no `seen` key — does not
    /// write the "first time" branch again.
    pub fn bump(slot: &mut Option<Self>, now: &str) {
        match slot {
            Some(seen) => {
                seen.count = seen.count.saturating_add(1);
                seen.last = now.to_owned();
            }
            None => *slot = Some(Self { count: 1, last: now.to_owned() }),
        }
    }
}

/// One human. `id` is the join key a project refers to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Person {
    pub id: String,
    pub name: String,
    /// E.164, e.g. `+15550001111`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub phones: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<String>,
    /// The first number in `phones` is also this person's Signal number. Signal is a
    /// third handle kind for a router that does not exist yet, not a third list: the
    /// number is already written down, and this says what else it reaches.
    #[serde(skip_serializing_if = "is_false")]
    pub signal: bool,
    /// How often this person has been matched, and when last. Absent until something
    /// matches them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen: Option<Seen>,
    /// Every key on this person that nothing here models, kept as it was written.
    ///
    /// The file is hand-editable, so it is allowed to carry more than the code knows
    /// about — a `"notes"` on a project, a `"schema_version"` at the top. A save that
    /// dropped those would punish the operator for writing them, so they are parked
    /// here on load and written back on save. An empty map serialises to nothing, so a
    /// file that never had a stray key never gains one.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One project: a folder to file into, the people who ask about it, and what each
/// inbox needs to know.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub folder: String,
    /// The nashcode repo. Absent for a GitHub-only client: meetings and email then
    /// have nowhere to file, and the consumer says so rather than guessing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Person ids, in the order they were written.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub people: Vec<String>,
    /// iMessage group ids. Matched in Swift, before participants; nothing here reads
    /// them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chat_ids: Vec<String>,
    /// Written only when it says something the default does not, because a block that
    /// repeats the default is a block the operator has to read and skip.
    #[serde(skip_serializing_if = "Imsg::is_default")]
    pub imsg: Imsg,
    pub email: Email,
    /// How often this project has been the answer, and when last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen: Option<Seen>,
    /// Keys nothing here models. See [`Person::extra`].
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// What the iMessage router needs per project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Imsg {
    pub prompt: String,
    pub enrich: bool,
    pub media_only: bool,
    /// Keys nothing here models. See [`Person::extra`].
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Imsg {
    /// Is this the block a project with no `imsg` key gets anyway?
    ///
    /// serde needs a path rather than a closure, and it hands the field by reference,
    /// which is why this is a method and not `|imsg| imsg == &Imsg::default()`.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Enrichment on, media-only off: the settings a new project wants, so a project
/// added by hand with no `imsg` block behaves like the ones already there.
impl Default for Imsg {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            enrich: true,
            media_only: false,
            extra: serde_json::Map::new(),
        }
    }
}

/// What the email pusher needs per project.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Email {
    /// The mailbox to search. It is the operator's own address, so it never scores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// A hand-written Gmail query that replaces the one built from the project's
    /// people.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Keys nothing here models. See [`Person::extra`].
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
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
            // Signal says "the number in `phones` is also Signal". With no number it
            // says nothing at all, and it will keep saying nothing after a Signal
            // router exists.
            if person.signal && person.phones.is_empty() {
                findings.push(Finding::warn(format!(
                    "{:?} is marked signal: true but has no phone; Signal marks the number in \
                     `phones`, so give them one or drop the flag",
                    label(&person.id, &person.name)
                )));
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

    /// One more match for this person, now. `false` when no person has that id.
    pub fn bump_person(&mut self, id: &str, now: &str) -> bool {
        match self.people.iter_mut().find(|person| person.id == id) {
            Some(person) => {
                Seen::bump(&mut person.seen, now);
                true
            }
            None => false,
        }
    }

    /// One more match for this project, now. `false` when no project has that id.
    pub fn bump_project(&mut self, id: &str, now: &str) -> bool {
        match self.projects.iter_mut().find(|project| project.id == id) {
            Some(project) => {
                Seen::bump(&mut project.seen, now);
                true
            }
            None => false,
        }
    }

    /// The file as it is written to disk: two-space JSON, fields in struct order.
    pub fn to_pretty_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// `false` is the absence of a flag, so it is not written down. serde needs a path,
/// not a closure, which is why this is a function and not `|flag| !flag`.
fn is_false(flag: &bool) -> bool {
    !*flag
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
        let mut file = crate::route::tests::fixture();
        file.skip = vec!["deploy-*".to_owned(), "*-backups".to_owned()];
        file.people[0].signal = true;
        file.people[0].seen = Some(Seen { count: 3, last: "2026-08-21T09:00:00Z".to_owned() });
        file.projects[0].seen = Some(Seen { count: 9, last: "2026-08-23T09:00:00Z".to_owned() });

        let text = file.to_pretty_json();
        assert!(text.contains("  \"me\": ["), "two-space indent: {text}");
        assert!(text.contains("\"signal\": true"), "{text}");
        assert!(text.contains("\"count\": 3"), "{text}");
        assert_eq!(PeopleFile::parse(&text).expect("round trip"), file);
    }

    #[test]
    fn the_keys_nobody_set_are_not_written_at_all() {
        // An untouched file gains no `signal`, no `seen`, no `skip`: a key that means
        // "false" or "never" is noise in a file a person edits by hand. Nor an empty
        // list, nor a `null`, nor an `imsg` block that only repeats the default.
        let text = crate::route::tests::fixture().to_pretty_json();
        assert!(!text.contains("\"signal\""), "{text}");
        assert!(!text.contains("\"seen\""), "{text}");
        assert!(!text.contains("\"skip\""), "{text}");
        assert!(!text.contains("\"chat_ids\""), "{text}");
        assert!(!text.contains("\"imsg\""), "{text}");
        assert!(!text.contains("\"query\""), "{text}");
        assert!(!text.contains("[]"), "no empty list: {text}");
        assert!(!text.contains("null"), "and no null: {text}");
        // What is set is still written, in full.
        assert!(text.contains("\"repo\": \"agstaff\""), "{text}");
        assert!(text.contains("\"account\": \"matthias@example.com\""), "{text}");
        assert_eq!(PeopleFile::parse(&text).expect("round trip"), crate::route::tests::fixture());
    }

    #[test]
    fn an_imsg_block_is_written_only_when_it_says_something() {
        let mut file = crate::route::tests::fixture();
        file.projects[0].imsg.enrich = false;
        let text = file.to_pretty_json();
        assert!(text.contains("\"enrich\": false"), "{text}");
        assert_eq!(PeopleFile::parse(&text).expect("round trip"), file);

        // A block that only holds a key this code does not model is still a block
        // somebody typed, so it survives.
        let mut kept = crate::route::tests::fixture();
        kept.projects[0].imsg.extra.insert("attachments".to_owned(), serde_json::json!(true));
        assert!(kept.to_pretty_json().contains("\"attachments\""), "{}", kept.to_pretty_json());
    }

    #[test]
    fn a_seen_keeps_no_key_but_its_own_two() {
        // `Seen` is two numbers and carries no `extra`: a stray key on it is dropped
        // rather than parked, which every other type here would do.
        let file = PeopleFile::parse(
            r#"{ "people": [ { "id": "rob", "emails": ["rob@example.com"],
                               "seen": { "count": 2, "last": "2026-08-23T09:00:00Z",
                                         "source": "imsg" } } ],
                 "projects": [] }"#,
        )
        .expect("a stray key on `seen` is not an error");
        assert_eq!(file.people[0].seen.as_ref().map(|seen| seen.count), Some(2));
        assert!(!file.to_pretty_json().contains("source"), "{}", file.to_pretty_json());
    }

    #[test]
    fn a_key_nothing_here_models_survives_load_and_save() {
        // The file is hand-editable, so it is allowed to say more than this code
        // reads. Dropping the extra on save would teach the operator not to write in
        // their own file.
        let written = r#"{
          "schema_version": 2,
          "people": [ { "id": "rob", "name": "Rob Castro", "emails": ["rob@example.com"],
                        "birthday": "1990-01-01" } ],
          "projects": [ { "id": "agstaff", "people": ["rob"], "notes": "pays late",
                          "imsg": { "prompt": "file it", "attachments": true },
                          "email": { "account": "me@example.com", "label": "clients" } } ]
        }"#;
        let file = PeopleFile::parse(written).expect("stray keys are not an error");
        assert_eq!(file.extra["schema_version"], serde_json::json!(2));
        assert_eq!(file.people[0].extra["birthday"], serde_json::json!("1990-01-01"));
        assert_eq!(file.projects[0].extra["notes"], serde_json::json!("pays late"));
        assert_eq!(file.projects[0].imsg.extra["attachments"], serde_json::json!(true));
        assert_eq!(file.projects[0].email.extra["label"], serde_json::json!("clients"));

        let again = PeopleFile::parse(&file.to_pretty_json()).expect("and it is still a file");
        assert_eq!(again, file, "load -> save -> load kept every key");
        assert!(file.to_pretty_json().contains("\"pays late\""));
    }

    #[test]
    fn a_signal_flag_with_no_number_marks_nothing() {
        let file = PeopleFile::parse(
            r#"{ "people": [ { "id": "david", "name": "David Reed", "signal": true,
                               "emails": ["david@example.com"] } ],
                 "projects": [ { "id": "p", "people": ["david"] } ] }"#,
        )
        .expect("it loads; the flag is merely empty");
        let findings = file.validate();
        assert!(
            findings
                .iter()
                .any(|f| !f.fatal && f.text.contains("signal: true") && f.text.contains("David Reed")),
            "{findings:?}"
        );
    }

    #[test]
    fn a_bump_counts_the_first_match_and_every_one_after_it() {
        let mut file = crate::route::tests::fixture();
        assert!(file.bump_person("rob", "2026-08-20T10:00:00Z"));
        assert_eq!(file.people[0].seen, Some(Seen { count: 1, last: "2026-08-20T10:00:00Z".into() }));
        assert!(file.bump_person("rob", "2026-08-23T10:00:00Z"));
        assert_eq!(file.people[0].seen, Some(Seen { count: 2, last: "2026-08-23T10:00:00Z".into() }));

        assert!(file.bump_project("acres", "2026-08-23T10:00:00Z"));
        assert_eq!(file.projects[1].seen.as_ref().map(|seen| seen.count), Some(1));

        // An id nobody has says so rather than inventing a row.
        assert!(!file.bump_person("nobody", "2026-08-23T10:00:00Z"));
        assert!(!file.bump_project("nobody", "2026-08-23T10:00:00Z"));
    }
}
