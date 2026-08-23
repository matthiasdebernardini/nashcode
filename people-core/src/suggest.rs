//! Who else writes about a project, read off the two inboxes the operator already has
//! open.
//!
//! This is discovery, not a decision: it writes nothing, it changes no file, and every
//! candidate it finds is somebody a human still has to accept. Both `nashcode people
//! suggest` and the desktop inspector's "Suggested" section call
//! [`candidates_for`], so a name the terminal offers and a name the window offers are
//! the same name, found the same way.
//!
//! - **Messages**, through `imsg chats --limit 300 --json`: a chat whose name holds the
//!   project's name or id as a whole word offers its participants.
//! - **Gmail**, through `gws`: the newest [`GMAIL_MESSAGES`] messages of the last year
//!   whose search matches the project name offer their `From:` addresses, and no more
//!   than [`GMAIL_MESSAGE_BUDGET`] messages are read across every project in one run.
//!
//! ## What leaves this machine
//!
//! The Gmail search sends the project's **name** as the query and nothing else — no
//! phone number, no address, nothing else out of `people.json`. The Messages side
//! sends nothing at all: `imsg` reads the local database. Everything the two answer
//! with is compared against the file here, on this machine.
//!
//! A missing `imsg` or `gws` is not a failure: that source comes back empty with one
//! note in [`take_notes`], because the other source is still worth having.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{PeopleFile, Project};
use crate::route::normalize;

/// The newest Gmail messages read per project. A cap, not a page size: a client with a
/// thousand threads is not worth a thousand round trips to find the four people on
/// them.
pub const GMAIL_MESSAGES: usize = 25;

/// Gmail messages read in one run, over every project together.
///
/// Twenty-five each is a small number until there are thirty-five clients, and then it
/// is eight hundred and seventy-five round trips for one `suggest`. Four projects' worth
/// is what a person waits through; past that the run stops and says which projects it
/// did not ask about, so the next one can be pointed at them with `--project`.
pub const GMAIL_MESSAGE_BUDGET: usize = 100;

/// How many Gmail reads this run has left of [`GMAIL_MESSAGE_BUDGET`].
///
/// The caller looping over projects reads this to know when to stop and whom it is
/// leaving unasked; the loop here reads it to know when to stop mid-project.
pub fn gmail_reads_left() -> usize {
    GMAIL_MESSAGE_BUDGET.saturating_sub(reads().load(Ordering::Relaxed))
}

/// Give the budget back, for a process that runs more than one sweep — the desktop
/// app, whose window is open all day. A CLI run exits instead.
pub fn reset_gmail_budget() {
    reads().store(0, Ordering::Relaxed);
}

/// Gmail messages read so far in this process.
fn reads() -> &'static AtomicUsize {
    static READS: AtomicUsize = AtomicUsize::new(0);
    &READS
}

/// One `messages get` out of the budget, or `false` when there is none left.
fn spend_a_read() -> bool {
    reads()
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |spent| {
            (spent < GMAIL_MESSAGE_BUDGET).then_some(spent + 1)
        })
        .is_ok()
}

/// How far back Gmail is searched.
const GMAIL_WINDOW: &str = "newer_than:365d";

/// One person a source has seen writing about a project, who is not in the file yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Where the operator can go and look, in their own words: a chat's name, a
    /// message's date.
    pub where_seen: String,
    /// When that source last saw them, as the source spelled it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
}

impl Candidate {
    /// The address, whichever kind it is.
    pub fn address(&self) -> &str {
        self.email.as_deref().or(self.phone.as_deref()).unwrap_or_default()
    }
}

/// Everybody the two sources have seen writing about `project` who is not in `file`.
///
/// Pure enough to reason about and impure exactly twice: it runs `imsg` and `gws`. It
/// takes the whole file rather than a list of addresses because "already known" means
/// *anywhere* in the file — another project's person is not a candidate here either,
/// they are somebody to add to this project by hand.
pub fn candidates_for(project: &Project, file: &PeopleFile) -> Vec<Candidate> {
    let known = known_addresses(file);
    let mut found: Vec<Candidate> = Vec::new();
    if let Some(chats) = chats() {
        found.extend(chat_candidates(chats, project, &known));
    }
    found.extend(gmail_candidates(project, &known));
    dedupe(&mut found);
    found
}

/// Every address the file already knows, normalised: the people's, the operator's own
/// `me` entries, and every project's mail account.
fn known_addresses(file: &PeopleFile) -> BTreeSet<String> {
    let people = file
        .people
        .iter()
        .flat_map(|person| person.phones.iter().chain(person.emails.iter()))
        .map(String::as_str);
    let accounts = file.projects.iter().filter_map(|project| project.email.account.as_deref());
    people
        .chain(file.me.iter().map(String::as_str))
        .chain(accounts)
        .map(normalize)
        .filter(|value| !value.is_empty())
        .collect()
}

/// The same person twice — a chat and a mail thread — is one candidate. The first
/// sighting wins, because the sources are asked in the order the operator trusts them.
fn dedupe(found: &mut Vec<Candidate>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    found.retain(|candidate| seen.insert(normalize(candidate.address())));
}

/// Every Messages chat on this machine, read once per process.
///
/// `imsg chats` takes no search, so it would otherwise be one full read of the chat
/// list per project — thirty-five reads of the same answer for a file with thirty-five
/// clients. Once per process is right for the CLI, which exits after one run, and
/// right for the desktop app, whose suggestions are cached per project for the session
/// anyway: a chat list from earlier in the session is exactly as fresh as the
/// suggestions drawn from it.
fn chats() -> Option<&'static str> {
    static CHATS: OnceLock<Option<String>> = OnceLock::new();
    CHATS.get_or_init(|| run("imsg", &["chats", "--limit", "300", "--json"])).as_deref()
}

/// The participants of every Messages chat whose name holds this project's name or id
/// as a whole word.
///
/// `imsg chats --json` answers NDJSON: one chat per line, `{id, display_name,
/// participants, last_message_at}`. A line that is not a chat is skipped rather than
/// failing the run — a tool that prints a banner should not cost the operator the
/// whole source.
fn chat_candidates(ndjson: &str, project: &Project, known: &BTreeSet<String>) -> Vec<Candidate> {
    // A project with no name is matched by its id alone, and one with neither matches
    // nothing at all.
    let needles: Vec<String> = [project.name.as_str(), project.id.as_str()]
        .iter()
        .map(|needle| needle.trim().to_lowercase())
        .filter(|needle| !needle.is_empty())
        .collect();

    let mut found = Vec::new();
    for line in ndjson.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(chat) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let name = chat["display_name"].as_str().unwrap_or_default();
        let haystack = name.to_lowercase();
        if !needles.iter().any(|needle| holds_word(&haystack, needle)) {
            continue;
        }
        let last = chat["last_message_at"].as_str().map(str::to_owned);
        for handle in chat["participants"].as_array().into_iter().flatten() {
            let Some(handle) = handle.as_str().map(str::trim).filter(|h| !h.is_empty()) else {
                continue;
            };
            if known.contains(&normalize(handle)) {
                continue;
            }
            // An Apple ID handle is an email; everything else is a number.
            let (email, phone) = match handle.contains('@') {
                true => (Some(handle.to_owned()), None),
                false => (None, Some(handle.to_owned())),
            };
            found.push(Candidate {
                // Messages knows the handle, not the human. The operator names them.
                name: handle.to_owned(),
                email,
                phone,
                where_seen: format!("Messages chat {name:?}"),
                last: last.clone(),
            });
        }
    }
    found
}

/// The `From:` of the newest Gmail messages that name this project.
///
/// At most [`GMAIL_MESSAGES`] of them, and never more than what is left of
/// [`GMAIL_MESSAGE_BUDGET`] for the whole run.
fn gmail_candidates(project: &Project, known: &BTreeSet<String>) -> Vec<Candidate> {
    let query = project.name.trim();
    let query = if query.is_empty() { project.id.trim() } else { query };
    if query.is_empty() {
        return Vec::new();
    }
    // Nothing left in the run's budget is nothing to ask with: the listing is a round
    // trip too, and its answer could not be read.
    if gmail_reads_left() == 0 {
        say(
            "gmail-budget",
            format!(
                "the run's {GMAIL_MESSAGE_BUDGET} Gmail messages are spent, so no mail was \
                 read for {:?} or for the projects after it; ask about them with --project",
                project.id
            ),
        );
        return Vec::new();
    }
    // The project's NAME is the whole query. Nothing else out of the file goes with it.
    let list = json!({
        "userId": "me",
        "q": format!("{query} {GMAIL_WINDOW}"),
        "maxResults": GMAIL_MESSAGES,
    })
    .to_string();
    let Some(answer) = run("gws", &["gmail", "users", "messages", "list", "--params", &list])
    else {
        return Vec::new();
    };
    let Ok(answer) = serde_json::from_str::<Value>(&answer) else {
        return Vec::new();
    };
    if let Some(why) = body_error(&answer) {
        say("gws", why);
        return Vec::new();
    }

    let ids: Vec<String> = answer["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["id"].as_str())
        .take(GMAIL_MESSAGES)
        .map(str::to_owned)
        .collect();

    let mut found = Vec::new();
    let wanted = ids.len();
    // The index is the count already read, which is what the note has to say.
    for (read, id) in ids.into_iter().enumerate() {
        if !spend_a_read() {
            say(
                "gmail-budget",
                format!(
                    "the run's {GMAIL_MESSAGE_BUDGET} Gmail messages are spent, so only \
                     {read} of {wanted} were read for {:?}; ask about the rest with \
                     --project",
                    project.id
                ),
            );
            break;
        }
        let get = json!({
            "userId": "me",
            "id": id,
            "format": "metadata",
            "metadataHeaders": ["From", "Date"],
        })
        .to_string();
        let Some(text) = run("gws", &["gmail", "users", "messages", "get", "--params", &get])
        else {
            break;
        };
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(why) = body_error(&message) {
            say("gws", why);
            break;
        }
        if let Some(candidate) = message_candidate(&message, known) {
            found.push(candidate);
        }
    }
    found
}

/// The failure a `gws` answer carries in its body rather than in its exit code.
///
/// Google answers a dead token with `{"error": {"code": 401, "message": "Invalid
/// Credentials"}}` and `gws` prints it and exits 0. Read only the status and every
/// project reports "nobody new" for a mailbox that was never opened — which is the
/// same sentence as good news.
fn body_error(answer: &Value) -> Option<String> {
    let error = answer.get("error").filter(|error| !error.is_null())?;
    // `{"error": "..."}` from a wrapper, or Google's own object.
    let message = error
        .as_str()
        .or_else(|| error["message"].as_str())
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or("no message");
    let code = match error["code"].as_i64() {
        Some(code) => code.to_string(),
        None => error["status"].as_str().unwrap_or("no code").to_owned(),
    };
    Some(format!("gws answered an error ({code}): {message}, so that source is empty"))
}

/// One Gmail message as a candidate, or `None` when its sender is already known.
///
/// Only `From` and `Date` are asked for, so the subject is not there to name: the
/// sighting is the date, which is what the operator would search their own mailbox by.
fn message_candidate(message: &Value, known: &BTreeSet<String>) -> Option<Candidate> {
    let from = header(message, "from")?;
    let (name, address) = parse_from(from);
    if address.is_empty() || known.contains(&normalize(&address)) {
        return None;
    }
    let date = header(message, "date").unwrap_or("an undated message");
    Some(Candidate {
        name: if name.is_empty() { address.clone() } else { name },
        email: Some(address),
        phone: None,
        where_seen: format!("Gmail: message from {date}"),
        last: header(message, "date").map(str::to_owned),
    })
}

/// Everything up to the comma that separates two mailboxes.
///
/// A comma inside a quoted display name — `"Castro, Rob" <rob@x>` — and one inside the
/// angle brackets are not separators, so the state of both is carried along.
fn first_mailbox(value: &str) -> &str {
    let (mut quoted, mut angled) = (false, false);
    for (at, c) in value.char_indices() {
        match c {
            '"' if !angled => quoted = !quoted,
            '<' if !quoted => angled = true,
            '>' if !quoted => angled = false,
            ',' if !quoted && !angled => return &value[..at],
            _ => {}
        }
    }
    value
}

/// Does `haystack` hold `needle` as a whole word? Both are already lowercase.
///
/// `contains` would let a project called `Ag` match the chat "Vintage friends", and a
/// suggestion is only worth reading if it is nearly always right. A word ends where an
/// alphanumeric character stops, so `pristine-acres` is still found in "re:
/// PRISTINE-ACRES fencing".
fn holds_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        if !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        {
            return true;
        }
        // Past this sighting's first character, so an overlapping one is still found.
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// One header of a `format=metadata` message, by name, without case.
fn header<'a>(message: &'a Value, name: &str) -> Option<&'a str> {
    let headers = match message["payload"]["headers"].as_array() {
        Some(headers) => headers,
        None => message["headers"].as_array()?,
    };
    headers
        .iter()
        .find(|header| header["name"].as_str().is_some_and(|it| it.eq_ignore_ascii_case(name)))
        .and_then(|header| header["value"].as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// `Rob Castro <rob@example.com>` -> `("Rob Castro", "rob@example.com")`.
///
/// A bare address has no name; a quoted name keeps its comma and loses its quotes.
/// Anything with no `@` in it is not an address, and comes back empty rather than as a
/// candidate nobody can write to.
///
/// A `From:` may carry more than one mailbox — `A <a@x>, B <b@x>` is legal — and the
/// first one is the sender. Reading the last would name one person and write to
/// another.
fn parse_from(value: &str) -> (String, String) {
    let value = first_mailbox(value.trim()).trim();
    let (name, address) = match (value.rfind('<'), value.rfind('>')) {
        (Some(open), Some(close)) if close > open => (&value[..open], &value[open + 1..close]),
        _ => ("", value),
    };
    let name = name.trim().trim_matches('"').trim();
    let address = address.trim();
    if !address.contains('@') {
        return (name.to_owned(), String::new());
    }
    (name.to_owned(), address.to_owned())
}

/// What the sources said when they could not answer, and nothing since the last time
/// it was asked.
///
/// One note per tool per process, not one per project: thirty-five "gws is not on
/// PATH" lines say nothing the first one did not. The caller decides where a note goes
/// — the CLI writes it to stderr, the desktop app draws it under the empty list — and
/// draining is what keeps it from being said twice.
pub fn take_notes() -> Vec<String> {
    std::mem::take(&mut *notes().lock().unwrap_or_else(|poison| poison.into_inner()))
}

/// The same notes, left where they are.
///
/// A process that exits after one run wants [`take_notes`]: it says each note once and
/// then it is gone. A window does not. Its lookups are cached per project, so the
/// second project draining the notes would leave the third to report "nobody new" for
/// a source that is still not installed — the reason having been spent on somebody
/// else's card. `gws` is still not on `PATH` on the eleventh lookup, so the eleventh
/// lookup may still say so.
pub fn notes_now() -> Vec<String> {
    notes().lock().unwrap_or_else(|poison| poison.into_inner()).clone()
}

fn notes() -> &'static Mutex<Vec<String>> {
    static NOTES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    NOTES.get_or_init(|| Mutex::new(Vec::new()))
}

/// What this module has already complained about — a tool, or the spent budget — so it
/// is not complained about again.
fn told() -> &'static Mutex<BTreeSet<&'static str>> {
    static TOLD: OnceLock<Mutex<BTreeSet<&'static str>>> = OnceLock::new();
    TOLD.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// A binary's standard output, or `None` with one note.
///
/// The note is one short clause, because it is drawn in a panel beside a list and read
/// beside another one just like it. A tool that is simply not installed says so in
/// those words rather than in the operating system's; anything else keeps the reason,
/// because then the reason is the news.
fn run(bin: &'static str, args: &[&str]) -> Option<String> {
    match std::process::Command::new(bin).args(args).output() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            say(bin, format!("{bin} is not on PATH, so that source is empty"));
            None
        }
        Err(error) => {
            say(bin, format!("{bin} did not run ({error}), so that source is empty"));
            None
        }
        Ok(output) if !output.status.success() => {
            let why = String::from_utf8_lossy(&output.stderr);
            say(
                bin,
                format!("{bin} exited {}: {}", output.status.code().unwrap_or(-1), why.trim()),
            );
            None
        }
        Ok(output) => Some(String::from_utf8_lossy(&output.stdout).into_owned()),
    }
}

/// One note per subject per process: thirty-five copies of one sentence say nothing the
/// first one did not.
fn say(subject: &'static str, note: String) {
    let mut told = told().lock().unwrap_or_else(|poison| poison.into_inner());
    if told.insert(subject) {
        notes().lock().unwrap_or_else(|poison| poison.into_inner()).push(note);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_named_after_a_project_offers_everyone_on_it_but_the_people_already_known() {
        // What `imsg chats --json` prints: one chat per line. The last line is not a
        // chat at all, which must not cost the source the two above it.
        let ndjson = r#"{"id":"7","display_name":"AgStaff crew","participants":["+15550001111","+15550003333","new@example.com"],"last_message_at":"2026-08-22T18:04:00Z"}
{"id":"8","display_name":"Book club","participants":["+15550009999"],"last_message_at":"2026-08-01T10:00:00Z"}
not json at all"#;
        let project = Project {
            id: "agstaff".to_owned(),
            name: "AgStaff".to_owned(),
            ..Project::default()
        };
        let known: BTreeSet<String> = ["+15550001111".to_owned()].into_iter().collect();

        let found = chat_candidates(ndjson, &project, &known);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].phone.as_deref(), Some("+15550003333"));
        assert_eq!(found[0].name, "+15550003333", "Messages knows the handle, not the human");
        assert_eq!(found[0].where_seen, "Messages chat \"AgStaff crew\"");
        assert_eq!(found[0].last.as_deref(), Some("2026-08-22T18:04:00Z"));
        // An Apple ID handle is an address, not a number.
        assert_eq!(found[1].email.as_deref(), Some("new@example.com"));
        assert_eq!(found[1].phone, None);
    }

    #[test]
    fn a_chat_matches_the_project_id_as_well_as_its_name_and_ignores_case() {
        let line = "{\"id\":\"1\",\"display_name\":\"re: PRISTINE-ACRES fencing\",\"participants\":[\"+15550004444\"]}";
        let project = Project {
            id: "pristine-acres".to_owned(),
            name: "Pristine Acres".to_owned(),
            ..Project::default()
        };
        assert_eq!(chat_candidates(line, &project, &BTreeSet::new()).len(), 1);

        // A project with no name and no id matches nothing, rather than everything:
        // an empty needle is inside every string.
        let anonymous = Project::default();
        assert!(chat_candidates(line, &anonymous, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_project_name_matches_a_chat_as_a_word_and_not_as_a_run_of_letters() {
        let chats = "{\"id\":\"1\",\"display_name\":\"Vintage friends\",\"participants\":[\"+15550004444\"]}\n{\"id\":\"2\",\"display_name\":\"Ag — the barn\",\"participants\":[\"+15550005555\"]}";
        let project =
            Project { id: "ag".to_owned(), name: "Ag".to_owned(), ..Project::default() };

        let found = chat_candidates(chats, &project, &BTreeSet::new());
        assert_eq!(found.len(), 1, "only the chat that says the word: {found:?}");
        assert_eq!(found[0].phone.as_deref(), Some("+15550005555"));
    }

    #[test]
    fn a_word_is_bounded_by_anything_that_is_not_a_letter_or_a_digit() {
        assert!(holds_word("agstaff crew", "agstaff"));
        assert!(holds_word("re: pristine-acres fencing", "pristine-acres"));
        assert!(holds_word("acres", "acres"), "the whole string is a word");
        assert!(holds_word("(acres)", "acres"));
        assert!(!holds_word("vintage friends", "ag"));
        assert!(!holds_word("agstaff crew", "ag"));
        assert!(!holds_word("acres", ""), "a project with no name matches nothing");
        // A second sighting counts when the first one was inside another word.
        assert!(holds_word("agstaff and ag", "ag"));
    }

    #[test]
    fn a_from_header_with_two_mailboxes_names_the_first_one() {
        assert_eq!(
            parse_from("A <a@example.com>, B <b@example.com>"),
            ("A".to_owned(), "a@example.com".to_owned())
        );
        assert_eq!(
            parse_from("\"Castro, Rob\" <rob@example.com>, Joey <joey@example.com>"),
            ("Castro, Rob".to_owned(), "rob@example.com".to_owned())
        );
        assert_eq!(
            parse_from("a@example.com, b@example.com"),
            (String::new(), "a@example.com".to_owned())
        );
    }

    #[test]
    fn an_error_in_the_body_is_a_failure_however_gws_exited() {
        let note = body_error(&json!({
            "error": { "code": 401, "message": "Invalid Credentials", "status": "UNAUTHENTICATED" }
        }))
        .expect("an error object is a failure");
        assert!(note.contains("401"), "{note}");
        assert!(note.contains("Invalid Credentials"), "{note}");

        // Google without a numeric code, and a wrapper that answers a bare string.
        let status = body_error(&json!({ "error": { "status": "PERMISSION_DENIED" } })).unwrap();
        assert!(status.contains("PERMISSION_DENIED"), "{status}");
        let bare = body_error(&json!({ "error": "token expired" })).unwrap();
        assert!(bare.contains("token expired"), "{bare}");

        // A real answer is not an error, and neither is an explicit null.
        assert_eq!(body_error(&json!({ "messages": [ { "id": "m1" } ] })), None);
        assert_eq!(body_error(&json!({ "error": null })), None);
    }

    #[test]
    fn a_from_header_is_a_name_and_an_address_or_just_an_address() {
        assert_eq!(
            parse_from("Rob Castro <rob@example.com>"),
            ("Rob Castro".to_owned(), "rob@example.com".to_owned())
        );
        assert_eq!(
            parse_from("\"Castro, Rob\" <rob@example.com>"),
            ("Castro, Rob".to_owned(), "rob@example.com".to_owned())
        );
        assert_eq!(
            parse_from("  <rob@example.com> "),
            (String::new(), "rob@example.com".to_owned())
        );
        assert_eq!(parse_from("rob@example.com"), (String::new(), "rob@example.com".to_owned()));
        // Nothing to write to is nobody to suggest.
        assert_eq!(parse_from("Mailer Daemon"), (String::new(), String::new()));
    }

    #[test]
    fn a_gmail_message_becomes_one_candidate_unless_its_sender_is_already_known() {
        let message = json!({
            "id": "18f2a0b1",
            "payload": { "headers": [
                { "name": "From", "value": "Joey Locker <joey@example.com>" },
                { "name": "Date", "value": "Tue, 4 Aug 2026 09:12:00 -0500" },
            ] },
        });
        let candidate = message_candidate(&message, &BTreeSet::new()).expect("a new sender");
        assert_eq!(candidate.name, "Joey Locker");
        assert_eq!(candidate.email.as_deref(), Some("joey@example.com"));
        assert_eq!(candidate.where_seen, "Gmail: message from Tue, 4 Aug 2026 09:12:00 -0500");
        assert_eq!(candidate.last.as_deref(), Some("Tue, 4 Aug 2026 09:12:00 -0500"));

        let known: BTreeSet<String> = ["joey@example.com".to_owned()].into_iter().collect();
        assert!(message_candidate(&message, &known).is_none());
        // No From at all: nothing to suggest, and nothing to crash on.
        assert!(message_candidate(&json!({ "payload": { "headers": [] } }), &known).is_none());
    }

    #[test]
    fn one_person_seen_in_two_places_is_one_candidate() {
        let mut found = vec![
            Candidate {
                name: "Joey".to_owned(),
                email: Some("Joey@Example.com".to_owned()),
                phone: None,
                where_seen: "Messages chat \"agstaff\"".to_owned(),
                last: None,
            },
            Candidate {
                name: "Joey Locker".to_owned(),
                email: Some("joey@example.com".to_owned()),
                phone: None,
                where_seen: "Gmail: message from Tue, 4 Aug 2026".to_owned(),
                last: None,
            },
        ];
        dedupe(&mut found);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].where_seen, "Messages chat \"agstaff\"", "the first sighting wins");
    }

    #[test]
    fn a_person_on_another_project_is_not_a_candidate_here_either() {
        let file = PeopleFile::parse(
            r#"{
              "me": ["operator@example.com"],
              "people": [
                { "id": "rob", "name": "Rob", "phones": ["+15550001111"],
                  "emails": ["Rob@Example.com"] }
              ],
              "projects": [
                { "id": "acres", "name": "Acres", "people": ["rob"],
                  "email": { "account": "work@example.com" } }
              ]
            }"#,
        )
        .expect("a valid file");
        let known = known_addresses(&file);

        // Case and whitespace are not identity: the file writes what the operator
        // typed, and a source answers with what the server holds.
        assert!(known.contains("rob@example.com"));
        assert!(known.contains("+15550001111"));
        assert!(known.contains("operator@example.com"), "the operator's own");
        assert!(known.contains("work@example.com"), "and every mail account");
        assert_eq!(known.len(), 4, "{known:?}");
    }
}
