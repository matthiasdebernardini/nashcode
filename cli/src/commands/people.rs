//! `nashcode people`: the file that says who belongs to which project.
//!
//! The file is `~/.nashcode/people.json` (`NASHCODE_PEOPLE`, or `--file`, overrides
//! it) and it is the only source of truth. Every inbox — iMessage, Gmail, the meeting
//! extension — asks the same question of it: which project is this about. The model
//! and the matching rule are `people-core`, so this command, the viewer, and the
//! desktop app answer alike.
//!
//! `route` answers locally, from the file on this machine. `push` hands the viewer a
//! copy so a browser extension can ask the same question over the tailnet.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use people_core::{Contact, Email, Imsg, PeopleFile, Person, Project, Seen, slug, unique_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Ctx;
use crate::cli::{
    PeopleCheckArgs, PeopleImportArgs, PeopleLsArgs, PeoplePushArgs, PeopleRouteArgs,
    PeopleSeenArgs, PeopleSuggestArgs, PeopleSyncFoldersArgs,
};
use crate::exit::{Class, classed};

/// `nashcode people ls` — every project, who is in it, and who is in nothing.
///
/// Warmest first, never alphabetical: `seen` is a count decayed by its age, so the
/// client who wrote this morning is at the top and the one who left in March is at the
/// bottom. That holds for the people inside a project too.
pub fn ls(ctx: &Ctx, args: &PeopleLsArgs) -> Result<Value> {
    let (path, file) = load(args.file.as_deref())?;
    let now = crate::timefmt::now_rfc3339();

    let mut assigned: BTreeSet<&str> = BTreeSet::new();
    let mut projects = Vec::new();
    for project in people_core::by_frecency(&file.projects, |p| p.seen.as_ref(), &now) {
        let mut roster: Vec<&Person> = Vec::new();
        for id in &project.people {
            assigned.insert(id.as_str());
            if let Some(person) = file.people.iter().find(|person| person.id == *id) {
                roster.push(person);
            }
        }
        let members: Vec<Value> = people_core::by_frecency(&roster, |p| p.seen.as_ref(), &now)
            .into_iter()
            .map(|person| {
                json!({
                    "id": person.id,
                    "name": person.name,
                    "phones": person.phones.len(),
                    "emails": person.emails.len(),
                    "signal": person.signal,
                    "seen": person.seen,
                })
            })
            .collect();
        ctx.out.step(format!(
            "{} [{}] {}{} — {}",
            project.id,
            project.repo.as_deref().unwrap_or("no repo"),
            project.folder,
            seen_label(project.seen.as_ref()).map(|it| format!(" · {it}")).unwrap_or_default(),
            describe(&members)
        ));
        projects.push(json!({
            "id": project.id,
            "name": project.name,
            "repo": project.repo,
            "folder": project.folder,
            "seen": project.seen,
            "people": members,
        }));
    }

    let loose: Vec<Value> = file
        .people
        .iter()
        .filter(|person| !assigned.contains(person.id.as_str()))
        .map(|person| json!({ "id": person.id, "name": person.name }))
        .collect();
    if !loose.is_empty() {
        ctx.out.step(format!("in no project: {}", describe(&loose)));
    }

    // The viewer's copy is a different thing from this file, so say how old it is.
    // A viewer that is unreachable is not this command's failure: the file is here.
    let pushed_at = match crate::commands::brain::viewer_url(ctx) {
        Err(_) => Value::Null,
        Ok((viewer, _)) => match people_core::pushed_at(&viewer) {
            Ok(Some(at)) => json!(at),
            Ok(None) => Value::Null,
            Err(why) => {
                ctx.out.warn(format!("the viewer did not say when it was last pushed: {why}"));
                Value::Null
            }
        },
    };

    Ok(json!({
        "file": path.display().to_string(),
        "projects": projects,
        "unassigned": loose,
        "pushed_at": pushed_at,
    }))
}

/// `nashcode people route --email … --phone …` — which project these people are about.
pub fn route(ctx: &Ctx, args: &PeopleRouteArgs) -> Result<Value> {
    let contacts: Vec<Contact> = args
        .emails
        .iter()
        .map(|address| Contact::email(address))
        .chain(args.phones.iter().map(|number| Contact::phone(number)))
        .collect();
    if contacts.is_empty() {
        return Err(classed(
            Class::Usage,
            "ask about somebody: nashcode people route --email rob@example.com",
        ));
    }

    let (path, file) = load(args.file.as_deref())?;
    let routing = file.route(&contacts);

    for hit in &routing.matches {
        let names: Vec<&str> = hit
            .people
            .iter()
            .map(|id| {
                file.people
                    .iter()
                    .find(|person| person.id == *id)
                    .map(|person| person.name.as_str())
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(id.as_str())
            })
            .collect();
        ctx.out.step(format!("{} — {}", hit.project, names.join(", ")));
    }
    if routing.tie {
        ctx.out.step("tie: the top two score the same, so nothing here decides");
    }
    if routing.matches.is_empty() {
        ctx.out.step("no project knows any of them");
    }

    let mut value = serde_json::to_value(&routing)?;
    value["file"] = json!(path.display().to_string());
    Ok(value)
}

/// `nashcode people push` — give the viewer a copy of the file.
pub fn push(ctx: &Ctx, args: &PeoplePushArgs) -> Result<Value> {
    let (path, file) = load(args.file.as_deref())?;
    let (viewer, _token) = crate::commands::brain::viewer_url(ctx)
        .map_err(|why| classed(Class::NotFound, why))?;

    // No credential: the viewer authenticates through Tailscale's identity headers,
    // the way every other viewer call in this CLI does. The profile's token is dgit's.
    let reply = people_core::push(&viewer, None, &file).map_err(|why| {
        // A refused file is this machine's to fix; anything else is the deployment's.
        let class = if why.contains("HTTP 400") { Class::Usage } else { Class::Api };
        classed(class, why)
    })?;

    ctx.out.step(format!(
        "{} projects and {} people are now at {viewer}",
        reply.projects, reply.people
    ));
    Ok(json!({
        "ok": true,
        "file": path.display().to_string(),
        "viewer": viewer,
        "people": reply.people,
        "projects": reply.projects,
        "pushed_at": reply.pushed_at,
    }))
}

/// `nashcode people check` — everything wrong with the file, and a non-zero exit when
/// there is anything.
///
/// This is the one reader that does not refuse a broken file: a fatal finding is what
/// it exists to report, so it deserializes and validates rather than calling `parse`.
pub fn check(ctx: &Ctx, args: &PeopleCheckArgs) -> Result<Value> {
    let path = file_path(args.file.as_deref());
    let text = read(&path)?;
    let file: PeopleFile = serde_json::from_str(&text)
        .map_err(|error| classed(Class::Usage, format!("{}: {error}", path.display())))?;

    let findings = file.validate();
    if findings.is_empty() {
        return Ok(json!({ "ok": true, "file": path.display().to_string(), "findings": [] }));
    }
    // The findings travel in the error message, because an error envelope carries no
    // result: a caller that only reads the message still gets every sentence.
    let lines: Vec<String> = findings
        .iter()
        .map(|finding| {
            let kind = if finding.fatal { "refused" } else { "warning" };
            format!("{kind}: {}", finding.text)
        })
        .collect();
    for line in &lines {
        ctx.out.step(line);
    }
    Err(classed(
        Class::Usage,
        format!("{} has {} finding(s)\n{}", path.display(), findings.len(), lines.join("\n")),
    ))
}

/// `nashcode people import --routes <path> --context <path>` — build the file once,
/// from what the old per-inbox lists already knew.
///
/// One-shot. `~/.imsg-router/routes.json` and `~/.nashcode/context.toml` are being
/// deleted; this reads them one last time so nobody retypes two clients' phone
/// numbers. **Delete this subcommand after it has run.**
///
/// Nothing is written: the result goes into the envelope for the operator to read,
/// fix, and save. `routes.json` does not know which number belongs to whom, so every
/// person arrives as `<project>-<n>` with an empty name and the stderr note lists
/// them.
pub fn import(ctx: &Ctx, args: &PeopleImportArgs) -> Result<Value> {
    let routes_path = match args.routes.as_deref() {
        Some(path) => PathBuf::from(path),
        None => home().join(".imsg-router").join("routes.json"),
    };
    let raw = read(&routes_path)?;
    let routes: RoutesFile = serde_json::from_str(&raw)
        .map_err(|error| classed(Class::Usage, format!("{}: {error}", routes_path.display())))?;

    let mut file = PeopleFile::default();
    // Two routes can name folders inside one project, and a project id is a join key:
    // both would be the same id, and so would their people. Every minted id is checked
    // against the ones already minted and suffixed until it is its own.
    let mut project_ids: BTreeSet<String> = BTreeSet::new();
    let mut person_ids: BTreeSet<String> = BTreeSet::new();
    for route in &routes.routes {
        let folder = parent_name(&route.folder);
        let id = unique(&slug(folder), &mut project_ids);
        if id.is_empty() {
            ctx.out.warn(format!(
                "the route {:?} has the folder {:?}, which names no project; skipped",
                route.name, route.folder
            ));
            continue;
        }
        let mut members = Vec::new();
        // One number written twice in a route is one person, not two: minting two
        // would score the project twice for one human.
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for phone in &route.participants {
            let normalised = people_core::normalize(phone);
            if normalised.is_empty() || !seen.insert(normalised) {
                continue;
            }
            let person = Person {
                id: unique(&format!("{id}-{}", members.len() + 1), &mut person_ids),
                // routes.json holds numbers, not names. The operator fills these in.
                name: String::new(),
                phones: vec![phone.clone()],
                emails: Vec::new(),
                ..Person::default()
            };
            members.push(person.id.clone());
            file.people.push(person);
        }
        file.projects.push(Project {
            name: route.name.clone(),
            folder: parent_path(&route.folder),
            // The nashcode repo is the directory as it is spelled on disk —
            // `PristineAcres`, not the id's `pristine-acres` — because that is the
            // name a repo was pushed under.
            repo: Some(folder.to_owned()),
            people: members,
            chat_ids: route.chat_ids.clone(),
            imsg: Imsg {
                prompt: route.prompt.clone(),
                enrich: route.enrich,
                media_only: route.media_only,
                ..Imsg::default()
            },
            email: Email::default(),
            id,
            ..Project::default()
        });
    }

    // The email side, when it is still there. A missing file is normal: not every
    // machine ran the Gmail pusher.
    let context_path = match args.context.as_deref() {
        Some(path) => PathBuf::from(path),
        None => home().join(".nashcode").join("context.toml"),
    };
    match std::fs::read_to_string(&context_path) {
        Err(_) => ctx.out.step(format!("no {}, so no mail accounts", context_path.display())),
        Ok(text) => {
            let sources: ContextFile = toml::from_str(&text).map_err(|error| {
                classed(Class::Usage, format!("{}: {error}", context_path.display()))
            })?;
            for source in &sources.source {
                match file.projects.iter_mut().find(|project| project.id == source.repo) {
                    None => ctx.out.warn(format!(
                        "{} names the repo {:?}, which no route folder matches; its mail \
                         settings are dropped",
                        context_path.display(),
                        source.repo
                    )),
                    Some(project) => {
                        project.email.account = source.account.clone();
                        project.email.query = source.query.clone();
                    }
                }
            }
        }
    }

    let unnamed: Vec<&str> = file
        .people
        .iter()
        .filter(|person| person.name.trim().is_empty())
        .map(|person| person.id.as_str())
        .collect();
    if !unnamed.is_empty() {
        ctx.out.warn(format!(
            "these people have no name yet, because routes.json only knew their number: {}",
            unnamed.join(", ")
        ));
    }

    // The old files carry placeholders — a number nobody filled in is still in there —
    // so the result is checked here rather than at the next command. Findings travel
    // in the envelope too: an agent that saved the file blind should see them.
    let findings = file.validate();
    for finding in &findings {
        ctx.out.warn(&finding.text);
    }

    Ok(json!({
        "ok": true,
        "people": file.people.len(),
        "projects": file.projects.len(),
        "unnamed": unnamed,
        "findings": serde_json::to_value(&findings)?,
        "file": serde_json::to_value(&file)?,
    }))
}

/// `nashcode people sync-folders <dir> [--write]` — one project per client folder.
///
/// The operator's client folders are the project list: each directory under `<dir>`
/// already knows the project's name, the folder to file into, and — from its git
/// origin — the forge repo. Only the people have to be typed. Nothing is ever removed
/// and nothing already in the file is changed, so this is safe to re-run after a new
/// client folder appears.
///
/// Without `--write` it prints what it would add and writes nothing.
pub fn sync_folders(ctx: &Ctx, args: &PeopleSyncFoldersArgs) -> Result<Value> {
    let (path, mut file) = load(args.file.as_deref())?;
    let dir = expand_home(&args.dir);

    let report = file.sync_folders(&dir).map_err(|why| classed(Class::NotFound, why))?;

    for id in &report.added {
        let Some(project) = file.projects.iter().find(|project| project.id == *id) else {
            continue;
        };
        ctx.out.step(format!(
            "+ {} [{}] {}",
            project.id,
            project.repo.as_deref().unwrap_or("no repo"),
            project.folder
        ));
    }
    if !report.skipped.is_empty() {
        ctx.out.step(format!("skipped by `skip`: {}", report.skipped.join(", ")));
    }
    ctx.out.step(format!(
        "{} new, {} already there, {} skipped",
        report.added.len(),
        report.kept,
        report.skipped.len()
    ));

    if args.write {
        file.save(&path).map_err(|why| classed(Class::Usage, why))?;
        ctx.out.step(format!("saved {}", path.display()));
    } else if !report.added.is_empty() {
        ctx.out.step("nothing written — run it again with --write");
    }

    let added: Vec<Value> = report
        .added
        .iter()
        .filter_map(|id| file.projects.iter().find(|project| project.id == *id))
        .map(|project| {
            json!({
                "id": project.id,
                "name": project.name,
                "folder": project.folder,
                "repo": project.repo,
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "file": path.display().to_string(),
        "dir": dir.display().to_string(),
        "written": args.write,
        "added": added,
        "skipped": report.skipped,
        "kept": report.kept,
    }))
}

/// `nashcode people seen <id>` — count one match against a person or a project.
///
/// The routers call this when they file a message, which is what makes `ls` warmest
/// first. Deliberately tiny: one bump, one save, no viewer.
pub fn seen(ctx: &Ctx, args: &PeopleSeenArgs) -> Result<Value> {
    let (path, mut file) = load(args.file.as_deref())?;
    let now = crate::timefmt::now_rfc3339();

    // A person id and a project id are different namespaces, so one word can be both.
    // Bumping both is the honest answer: something matched under that name.
    let person = file.bump_person(&args.id, &now);
    let project = file.bump_project(&args.id, &now);
    if !person && !project {
        return Err(classed(
            Class::NotFound,
            format!("no person and no project has the id {:?}", args.id),
        ));
    }
    file.save(&path).map_err(|why| classed(Class::Usage, why))?;

    let what = match (person, project) {
        (true, true) => "person and project",
        (true, false) => "person",
        _ => "project",
    };
    ctx.out.step(format!("{} seen: {what}, at {now}", args.id));
    Ok(json!({
        "ok": true,
        "file": path.display().to_string(),
        "id": args.id,
        "person": person,
        "project": project,
        "at": now,
    }))
}

/// The newest Gmail messages `suggest` reads per project. A cap, not a page size: a
/// client with a thousand threads is not worth a thousand round trips to find the
/// four people on them.
const GMAIL_MESSAGES: usize = 25;

/// How far back `suggest` looks in Gmail.
const GMAIL_WINDOW: &str = "newer_than:365d";

/// One person a source has seen writing about a project, who is not in the file yet.
#[derive(Debug, Clone, Serialize)]
struct Candidate {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phone: Option<String>,
    /// Where the operator can go and look, in their own words: a chat's name, a
    /// message's date.
    where_seen: String,
    /// When that source last saw them, as the source spelled it.
    #[serde(skip_serializing_if = "Option::is_none")]
    last: Option<String>,
}

impl Candidate {
    /// The address, whichever kind it is.
    fn address(&self) -> &str {
        self.email.as_deref().or(self.phone.as_deref()).unwrap_or_default()
    }
}

/// `nashcode people suggest [--project <id>]` — who else writes about this project.
///
/// Reads two places the operator already has open and writes nothing. Accepting a
/// suggestion is the operator's act, in the desktop app or by hand.
///
/// - **Messages**, through `imsg chats --limit 300 --json`: a chat whose name contains
///   the project's name or id offers its participants.
/// - **Gmail**, through `gws`: the newest 25 messages of the last year whose search
///   matches the project name offer their `From:` addresses.
///
/// **What leaves this machine.** The Gmail search sends the project's *name* as the
/// query and nothing else — no phone number, no address, nothing from `people.json`.
/// The Messages side sends nothing at all: `imsg` reads the local database. Everything
/// the two answer with is compared against the file here, on this machine.
///
/// A missing `imsg` or `gws` is not a failure: that source comes back empty with one
/// note on stderr, because the other source is still worth having.
pub fn suggest(ctx: &Ctx, args: &PeopleSuggestArgs) -> Result<Value> {
    let (path, file) = load(args.file.as_deref())?;
    let now = crate::timefmt::now_rfc3339();

    let wanted: Vec<&Project> = match args.project.as_deref() {
        Some(id) => vec![
            file.projects.iter().find(|project| project.id == id).ok_or_else(|| {
                classed(Class::NotFound, format!("no project has the id {id:?}"))
            })?,
        ],
        None => people_core::by_frecency(&file.projects, |p| p.seen.as_ref(), &now),
    };

    // Everything already written down: the people's own addresses, the operator's own,
    // and every project's mail account. A candidate is by definition none of these.
    let known = known_addresses(&file);

    let imsg = Tool::new(ctx, "imsg");
    let gws = Tool::new(ctx, "gws");
    // One call for every project: `imsg chats` does not take a search.
    let chats = imsg.run(&["chats", "--limit", "300", "--json"]);
    ctx.out.step(format!(
        "Gmail: the newest {GMAIL_MESSAGES} messages of the last year per project"
    ));

    let mut rows: Vec<Value> = Vec::new();
    for project in wanted {
        let mut found: Vec<Candidate> = Vec::new();
        if let Some(chats) = &chats {
            found.extend(chat_candidates(chats, project, &known));
        }
        found.extend(gmail_candidates(&gws, project, &known));
        dedupe(&mut found);

        if found.is_empty() {
            ctx.out.step(format!("{} — nobody new", project.id));
        } else {
            ctx.out.step(format!("{} — {} candidate(s)", project.id, found.len()));
            for candidate in &found {
                ctx.out.step(format!(
                    "    {}  {}  ({})",
                    candidate.name,
                    candidate.address(),
                    candidate.where_seen
                ));
            }
        }
        for candidate in found {
            let mut row = serde_json::to_value(&candidate)?;
            row["project"] = json!(project.id);
            rows.push(row);
        }
    }

    Ok(json!({
        "ok": true,
        "file": path.display().to_string(),
        "gmail_messages_per_project": GMAIL_MESSAGES,
        "candidates": rows,
    }))
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
        .map(people_core::normalize)
        .filter(|value| !value.is_empty())
        .collect()
}

/// The same person twice — a chat and a mail thread — is one candidate. The first
/// sighting wins, because the sources are asked in the order the operator trusts them.
fn dedupe(found: &mut Vec<Candidate>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    found.retain(|candidate| seen.insert(people_core::normalize(candidate.address())));
}

/// The participants of every Messages chat whose name names this project.
///
/// `imsg chats --json` answers NDJSON: one chat per line, `{id, display_name,
/// participants, last_message_at}`. A line that is not a chat is skipped rather than
/// failing the run — a tool that prints a banner should not cost the operator the
/// whole source.
fn chat_candidates(ndjson: &str, project: &Project, known: &BTreeSet<String>) -> Vec<Candidate> {
    // An empty needle is inside every string, so a project with no name is matched by
    // its id alone, and one with neither matches nothing.
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
        if !needles.iter().any(|needle| haystack.contains(needle.as_str())) {
            continue;
        }
        let last = chat["last_message_at"].as_str().map(str::to_owned);
        for handle in chat["participants"].as_array().into_iter().flatten() {
            let Some(handle) = handle.as_str().map(str::trim).filter(|h| !h.is_empty()) else {
                continue;
            };
            if known.contains(&people_core::normalize(handle)) {
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
fn gmail_candidates(gws: &Tool<'_>, project: &Project, known: &BTreeSet<String>) -> Vec<Candidate> {
    let query = project.name.trim();
    let query = if query.is_empty() { project.id.trim() } else { query };
    if query.is_empty() {
        return Vec::new();
    }
    // The project's NAME is the whole query. Nothing out of the file goes with it.
    let list = json!({
        "userId": "me",
        "q": format!("{query} {GMAIL_WINDOW}"),
        "maxResults": GMAIL_MESSAGES,
    })
    .to_string();
    let Some(answer) = gws.run(&["gmail", "users", "messages", "list", "--params", &list]) else {
        return Vec::new();
    };
    let Ok(answer) = serde_json::from_str::<Value>(&answer) else {
        return Vec::new();
    };

    let ids: Vec<String> = answer["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["id"].as_str())
        .take(GMAIL_MESSAGES)
        .map(str::to_owned)
        .collect();

    let mut found = Vec::new();
    for id in ids {
        let get = json!({
            "userId": "me",
            "id": id,
            "format": "metadata",
            "metadataHeaders": ["From", "Date"],
        })
        .to_string();
        let Some(text) = gws.run(&["gmail", "users", "messages", "get", "--params", &get]) else {
            break;
        };
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(candidate) = message_candidate(&message, known) {
            found.push(candidate);
        }
    }
    found
}

/// One Gmail message as a candidate, or `None` when its sender is already known.
///
/// Only `From` and `Date` are asked for, so the subject is not there to name: the
/// sighting is the date, which is what the operator would search their own mailbox by.
fn message_candidate(message: &Value, known: &BTreeSet<String>) -> Option<Candidate> {
    let from = header(message, "from")?;
    let (name, address) = parse_from(from);
    if address.is_empty() || known.contains(&people_core::normalize(&address)) {
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
fn parse_from(value: &str) -> (String, String) {
    let value = value.trim();
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

/// A binary this command shells out to, which may not be installed.
///
/// One note per tool per run, not one per project: thirty-five "gws is not on PATH"
/// lines say nothing the first one did not.
struct Tool<'a> {
    ctx: &'a Ctx,
    bin: &'static str,
    told: std::cell::Cell<bool>,
}

impl<'a> Tool<'a> {
    fn new(ctx: &'a Ctx, bin: &'static str) -> Self {
        Self { ctx, bin, told: std::cell::Cell::new(false) }
    }

    /// Its standard output, or `None` with one note on stderr.
    fn run(&self, args: &[&str]) -> Option<String> {
        let bin = self.bin;
        match std::process::Command::new(bin).args(args).output() {
            Err(error) => {
                self.say(format!("{bin} did not run ({error}), so that source is empty"));
                None
            }
            Ok(output) if !output.status.success() => {
                let why = String::from_utf8_lossy(&output.stderr);
                self.say(format!(
                    "{bin} exited {}: {}",
                    output.status.code().unwrap_or(-1),
                    why.trim()
                ));
                None
            }
            Ok(output) => Some(String::from_utf8_lossy(&output.stdout).into_owned()),
        }
    }

    fn say(&self, note: String) {
        if !self.told.replace(true) {
            self.ctx.out.warn(note);
        }
    }
}

// ---- the old files ---------------------------------------------------------

/// `~/.imsg-router/routes.json`, the shape the Swift router reads today.
#[derive(Debug, Deserialize)]
struct RoutesFile {
    #[serde(default)]
    routes: Vec<RouteIn>,
}

#[derive(Debug, Deserialize)]
struct RouteIn {
    #[serde(default)]
    name: String,
    #[serde(default)]
    participants: Vec<String>,
    #[serde(default)]
    chat_ids: Vec<String>,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    media_only: bool,
    #[serde(default = "yes")]
    enrich: bool,
    #[serde(default)]
    prompt: String,
}

fn yes() -> bool {
    true
}

/// `~/.nashcode/context.toml`, one `[[source]]` per client.
#[derive(Debug, Default, Deserialize)]
struct ContextFile {
    #[serde(default)]
    source: Vec<SourceIn>,
}

#[derive(Debug, Deserialize)]
struct SourceIn {
    #[serde(default)]
    repo: String,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

// ---- shared plumbing -------------------------------------------------------

/// The file this invocation acts on: `--file`, else `NASHCODE_PEOPLE`, else
/// `~/.nashcode/people.json`.
fn file_path(named: Option<&str>) -> PathBuf {
    match named.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => people_core::default_path(),
    }
}

/// Read and validate. A file that is not there and a file that is broken are
/// different failures, because the fix is different.
fn load(named: Option<&str>) -> Result<(PathBuf, PeopleFile)> {
    let path = file_path(named);
    if !path.exists() {
        return Err(classed(
            Class::NotFound,
            format!("there is no people file at {}", path.display()),
        ));
    }
    let file = PeopleFile::load(&path).map_err(|why| classed(Class::Usage, why))?;
    Ok((path, file))
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|error| classed(Class::NotFound, format!("{}: {error}", path.display())))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned()))
}

/// A leading `~/` as `$HOME`. A shell would have done this; an agent that passed the
/// path as one quoted string did not, and `~/NashvilleAutomation` is how the operator
/// says where the clients are.
fn expand_home(path: &str) -> PathBuf {
    let path = path.trim();
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

/// `Rob Castro (1 phone/1 email) · 3× · 2d ago`, joined. Empty reads as "nobody"
/// rather than as an empty line nobody can interpret.
fn describe(members: &[Value]) -> String {
    if members.is_empty() {
        return "nobody".to_owned();
    }
    members
        .iter()
        .map(|member| {
            let id = member["id"].as_str().unwrap_or_default();
            let name = member["name"].as_str().filter(|name| !name.trim().is_empty()).unwrap_or(id);
            let seen: Option<Seen> = serde_json::from_value(member["seen"].clone()).ok().flatten();
            let warmth =
                seen_label(seen.as_ref()).map(|it| format!(" · {it}")).unwrap_or_default();
            match (member["phones"].as_u64(), member["emails"].as_u64()) {
                (Some(phones), Some(emails)) => {
                    format!("{name} ({phones} phone/{emails} email){warmth}")
                }
                _ => format!("{name}{warmth}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `3× · 2d ago`, or nothing when it has never matched.
fn seen_label(seen: Option<&Seen>) -> Option<String> {
    let seen = seen?;
    let age = crate::timefmt::seconds_since(&seen.last)
        .map_or_else(|| seen.last.clone(), short_age);
    Some(format!("{}× · {age}", seen.count))
}

/// An age in one word, because it sits at the end of a line that already has four
/// things on it. `crate::timefmt::ago` is the long form, for a line of its own.
fn short_age(seconds: i64) -> String {
    match seconds {
        s if s < 0 => "ahead".to_owned(),
        s if s < 60 => "now".to_owned(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 7 * 86_400 => format!("{}d ago", s / 86_400),
        s if s < 365 * 86_400 => format!("{}w ago", s / (7 * 86_400)),
        s => format!("{}y ago", s / (365 * 86_400)),
    }
}

/// The project folder of a route folder: `~/Projects/agstaff/imsg-inbox` is the inbox
/// inside `~/Projects/agstaff`, and the project is the folder above.
fn parent_path(folder: &str) -> String {
    let trimmed = folder.trim().trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.to_owned(),
        _ => trimmed.to_owned(),
    }
}

/// The name of that folder: `~/Projects/agstaff/imsg-inbox` -> `agstaff`.
fn parent_name(folder: &str) -> &str {
    let parent = {
        let trimmed = folder.trim().trim_end_matches('/');
        match trimmed.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => parent,
            _ => trimmed,
        }
    };
    parent.rsplit('/').next().unwrap_or_default()
}

/// `people_core::unique_id`, remembering what it has already handed out. The crate's
/// version asks the caller what is taken; an import mints ids faster than it writes
/// them anywhere, so the set of taken ids is this one.
fn unique(base: &str, taken: &mut BTreeSet<String>) -> String {
    let id = unique_id(base, |candidate| taken.contains(candidate));
    if !id.is_empty() {
        taken.insert(id.clone());
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_folder_names_the_project_above_it() {
        assert_eq!(parent_path("~/Projects/agstaff/imsg-inbox"), "~/Projects/agstaff");
        assert_eq!(parent_name("~/Projects/agstaff/imsg-inbox"), "agstaff");
        assert_eq!(parent_name("~/Projects/Pristine Acres/imsg-inbox/"), "Pristine Acres");
        // A folder with nothing above it is the project itself.
        assert_eq!(parent_path("agstaff"), "agstaff");
        assert_eq!(parent_name("agstaff"), "agstaff");
    }

    #[test]
    fn an_id_is_the_folder_name_in_lowercase_with_dashes() {
        assert_eq!(slug("agstaff"), "agstaff");
        assert_eq!(slug("Pristine Acres"), "pristine-acres");
        assert_eq!(slug("  Rob & Joey  "), "rob-joey");
        assert_eq!(slug("---"), "");
    }

    #[test]
    fn a_missing_file_is_not_found_rather_than_broken() {
        let error = load(Some("/no/such/people.json")).unwrap_err();
        assert_eq!(crate::exit::class_of(&error), Some(Class::NotFound));
        assert!(format!("{error}").contains("/no/such/people.json"), "{error}");
    }

    #[test]
    fn a_second_route_in_one_project_folder_gets_its_own_id() {
        let mut taken = BTreeSet::new();
        assert_eq!(unique("agstaff", &mut taken), "agstaff");
        assert_eq!(unique("agstaff", &mut taken), "agstaff-2");
        assert_eq!(unique("agstaff", &mut taken), "agstaff-3");
        // A person id minted from the first project must not be claimed twice either.
        assert_eq!(unique("", &mut taken), "");
    }

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
    fn a_from_header_is_a_name_and_an_address_or_just_an_address() {
        assert_eq!(
            parse_from("Rob Castro <rob@example.com>"),
            ("Rob Castro".to_owned(), "rob@example.com".to_owned())
        );
        assert_eq!(
            parse_from("\"Castro, Rob\" <rob@example.com>"),
            ("Castro, Rob".to_owned(), "rob@example.com".to_owned())
        );
        assert_eq!(parse_from("  <rob@example.com> "), (String::new(), "rob@example.com".to_owned()));
        assert_eq!(parse_from("rob@example.com"), (String::new(), "rob@example.com".to_owned()));
        // Nothing to write to is nobody to suggest.
        assert_eq!(parse_from("Mailer Daemon"), ("".to_owned(), String::new()));
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
    fn warmth_reads_as_a_count_and_one_word_of_age() {
        assert_eq!(seen_label(None), None);
        assert_eq!(short_age(30), "now");
        assert_eq!(short_age(2 * 3600), "2h ago");
        assert_eq!(short_age(2 * 86_400), "2d ago");
        assert_eq!(short_age(3 * 7 * 86_400), "3w ago");
        // An unreadable stamp survives as itself rather than as "now".
        let broken = Seen { count: 3, last: "whenever".to_owned() };
        assert_eq!(seen_label(Some(&broken)).as_deref(), Some("3× · whenever"));
    }

    #[test]
    fn describe_names_a_person_by_their_name_and_falls_back_to_their_id() {
        let members = vec![
            json!({ "id": "rob", "name": "Rob Castro", "phones": 1, "emails": 1 }),
            json!({ "id": "agstaff-2", "name": "", "phones": 1, "emails": 0 }),
        ];
        assert_eq!(
            describe(&members),
            "Rob Castro (1 phone/1 email), agstaff-2 (1 phone/0 email)"
        );
        assert_eq!(describe(&[]), "nobody");
    }
}
