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
use people_core::{Contact, Email, Imsg, PeopleFile, Person, Project};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Ctx;
use crate::cli::{
    PeopleCheckArgs, PeopleImportArgs, PeopleLsArgs, PeoplePushArgs, PeopleRouteArgs,
};
use crate::exit::{Class, classed};

/// `nashcode people ls` — every project, who is in it, and who is in nothing.
pub fn ls(ctx: &Ctx, args: &PeopleLsArgs) -> Result<Value> {
    let (path, file) = load(args.file.as_deref())?;

    let mut assigned: BTreeSet<&str> = BTreeSet::new();
    let mut projects = Vec::new();
    for project in &file.projects {
        let mut members = Vec::new();
        for id in &project.people {
            assigned.insert(id.as_str());
            let Some(person) = file.people.iter().find(|person| person.id == *id) else {
                continue;
            };
            members.push(json!({
                "id": person.id,
                "name": person.name,
                "phones": person.phones.len(),
                "emails": person.emails.len(),
            }));
        }
        ctx.out.step(format!(
            "{} [{}] {} — {}",
            project.id,
            project.repo.as_deref().unwrap_or("no repo"),
            project.folder,
            describe(&members)
        ));
        projects.push(json!({
            "id": project.id,
            "name": project.name,
            "repo": project.repo,
            "folder": project.folder,
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
            },
            email: Email::default(),
            id,
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

/// `Rob Castro (1 phone, 2 emails)`, joined. Empty reads as "nobody" rather than as
/// an empty line nobody can interpret.
fn describe(members: &[Value]) -> String {
    if members.is_empty() {
        return "nobody".to_owned();
    }
    members
        .iter()
        .map(|member| {
            let id = member["id"].as_str().unwrap_or_default();
            let name = member["name"].as_str().filter(|name| !name.trim().is_empty()).unwrap_or(id);
            match (member["phones"].as_u64(), member["emails"].as_u64()) {
                (Some(phones), Some(emails)) => {
                    format!("{name} ({phones} phone/{emails} email)")
                }
                _ => name.to_owned(),
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

/// `base`, or `base-2`, `base-3`, … — whichever is not taken yet. An empty base stays
/// empty, because the caller refuses it rather than minting `-2`.
fn unique(base: &str, taken: &mut BTreeSet<String>) -> String {
    if base.is_empty() {
        return String::new();
    }
    if taken.insert(base.to_owned()) {
        return base.to_owned();
    }
    for n in 2..1000 {
        let candidate = format!("{base}-{n}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
    }
    base.to_owned()
}

/// A directory name as an id: lowercase, and one dash wherever something else was.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
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
            describe(&members),
            "Rob Castro (1 phone/1 email), agstaff-2 (1 phone/0 email)"
        );
        assert_eq!(describe(&[]), "nobody");
    }
}
