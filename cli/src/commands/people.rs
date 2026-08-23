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
use serde::Deserialize;
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
            people_core::seen_label(project.seen.as_ref(), &now)
                .map(|it| format!(" · {it}"))
                .unwrap_or_default(),
            describe(&members, &now)
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
        ctx.out.step(format!("in no project: {}", describe(&loose, &now)));
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
    // Its own line, because it is the operator's to fix: `skip` turned those away on
    // purpose, and these have a name no id can be made of.
    if !report.unnameable.is_empty() {
        ctx.out.step(format!(
            "no id can be made of these names, so they are not projects: {}",
            report.unnameable.join(", ")
        ));
    }
    ctx.out.step(format!(
        "{} new, {} already there, {} skipped, {} unnameable",
        report.added.len(),
        report.kept,
        report.skipped.len(),
        report.unnameable.len()
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
        "unnameable": report.unnameable,
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

/// `nashcode people suggest [--project <id>]` — who else writes about this project.
///
/// The discovery itself is [`people_core::suggest`], which the desktop inspector's
/// "Suggested" section calls too: a name the terminal offers and a name the window
/// offers have to be the same name, found the same way. This command is the printing
/// and the envelope around it, and it writes nothing — accepting a suggestion is the
/// operator's act, in the desktop app or by hand.
pub fn suggest(ctx: &Ctx, args: &PeopleSuggestArgs) -> Result<Value> {
    let (path, file) = load(args.file.as_deref())?;
    let now = crate::timefmt::now_rfc3339();

    let wanted: Vec<Project> = match args.project.as_deref() {
        Some(id) => vec![
            file.projects
                .iter()
                .find(|project| project.id == id)
                .cloned()
                .ok_or_else(|| {
                    classed(Class::NotFound, format!("no project has the id {id:?}"))
                })?,
        ],
        None => people_core::by_frecency(&file.projects, |p| p.seen.as_ref(), &now)
            .into_iter()
            .cloned()
            .collect(),
    };

    ctx.out.step(format!(
        "Gmail: the newest {} messages of the last year per project, and {} in the whole \
         run",
        people_core::suggest::GMAIL_MESSAGES,
        people_core::suggest::GMAIL_MESSAGE_BUDGET
    ));

    let mut rows: Vec<Value> = Vec::new();
    // The projects whose mail the run had no budget left to read. Messages costs
    // nothing — it is one local read, already done — so those projects are still
    // asked; only Gmail stops.
    let mut unasked: Vec<&str> = Vec::new();
    for project in &wanted {
        let had_budget = people_core::suggest::gmail_reads_left() > 0;
        let found = people_core::candidates_for(project, &file);
        if !had_budget {
            unasked.push(project.id.as_str());
        }
        // A source that could not answer says so once, on stderr, next to the project
        // that first asked it — not thirty-five times at the end.
        let notes = people_core::suggest::take_notes();
        let answered = notes.is_empty();
        for note in notes {
            ctx.out.warn(note);
        }

        if found.is_empty() {
            // A source that could not answer is not an empty inbox. The two must not
            // read the same, or a dead token looks like a quiet week.
            ctx.out.step(match answered {
                true => format!("{} — nobody new", project.id),
                false => {
                    format!("{} — nobody new, and a source above did not answer", project.id)
                }
            });
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

    if !unasked.is_empty() {
        ctx.out.warn(format!(
            "the run's {} Gmail messages were spent before {}; ask about those with \
             --project",
            people_core::suggest::GMAIL_MESSAGE_BUDGET,
            unasked.join(", ")
        ));
    }

    Ok(json!({
        "ok": true,
        "file": path.display().to_string(),
        "gmail_messages_per_project": people_core::suggest::GMAIL_MESSAGES,
        "gmail_messages_per_run": people_core::suggest::GMAIL_MESSAGE_BUDGET,
        "gmail_unasked": unasked,
        "candidates": rows,
    }))
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
///
/// `now` is passed in rather than read here so that one listing dates every row
/// against one instant: two rows measured a second apart would be a warmth order the
/// numbers beside it did not agree with.
fn describe(members: &[Value], now: &str) -> String {
    if members.is_empty() {
        return "nobody".to_owned();
    }
    members
        .iter()
        .map(|member| {
            let id = member["id"].as_str().unwrap_or_default();
            let name = member["name"].as_str().filter(|name| !name.trim().is_empty()).unwrap_or(id);
            let seen: Option<Seen> = serde_json::from_value(member["seen"].clone()).ok().flatten();
            let warmth = people_core::seen_label(seen.as_ref(), now)
                .map(|it| format!(" · {it}"))
                .unwrap_or_default();
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
    fn describe_names_a_person_by_their_name_and_falls_back_to_their_id() {
        let members = vec![
            json!({ "id": "rob", "name": "Rob Castro", "phones": 1, "emails": 1 }),
            json!({ "id": "agstaff-2", "name": "", "phones": 1, "emails": 0 }),
        ];
        assert_eq!(
            describe(&members, "2026-08-23T12:00:00Z"),
            "Rob Castro (1 phone/1 email), agstaff-2 (1 phone/0 email)"
        );
        assert_eq!(describe(&[], "2026-08-23T12:00:00Z"), "nobody");

        // And the warmth beside a name is people-core's one spelling of it.
        let warm = vec![json!({
            "id": "rob", "name": "Rob", "phones": 1, "emails": 0,
            "seen": { "count": 3, "last": "2026-08-21T12:00:00Z" },
        })];
        assert_eq!(
            describe(&warm, "2026-08-23T12:00:00Z"),
            "Rob (1 phone/0 email) · 3× · 2d ago"
        );
    }
}
