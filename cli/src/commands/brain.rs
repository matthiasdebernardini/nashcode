//! `nashcode brain`: the viewer's work state, digested for an agent's first read.
//!
//! `GET /brain` is an aggregate, not a report. It carries every branch, every plan,
//! every card and a hundred activity rows per repository, and the facts an agent
//! needs at the start of a session are buried in it. This command is the transform:
//! branches with their tip and CI state, what the code index holds and how old it
//! is, the plan files and how many comments are waiting on them, the latest
//! architecture submission, and the last five things that happened.
//!
//! It never fails the caller. A session-start hook runs it, and a viewer that is
//! down, unreachable, or unconfigured must not take the session down with it: one
//! line saying so, exit 0.

use super::Ctx;
use crate::api::Client;
use crate::cli::BrainArgs;
use crate::timefmt::{ago, age_of};
use crate::vcs;
use serde_json::{Value, json};
use std::time::Duration;

/// Short enough to read, long enough to paste into `git show`.
const TIP_LEN: usize = 7;

/// How many activity rows survive the digest.
const RECENT: usize = 5;

/// How many repositories the whole-brain digest reports in full.
///
/// Without a filter this reads every repository the viewer knows, and a first
/// read of a session is not the place to print two thousand of them. Past the cap
/// the digest says how many it left out and stops.
const REPO_CAP: usize = 20;

/// A session-start hook waits on this. The default 60s client turns a dead viewer
/// into a minute of silence at the top of every session.
const TIMEOUT: Duration = Duration::from_secs(10);

/// `GET <viewer>/brain[?repo=<name>]`.
pub fn brain_url(viewer: &str, repo: Option<&str>) -> String {
    let base = format!("{}/brain", viewer.trim_end_matches('/'));
    match repo {
        Some(name) => format!("{base}?repo={}", crate::commands::plan::pct(name)),
        None => base,
    }
}

/// The whole stanza, reduced to what an agent reads first.
///
/// Shape: `{"status": "ok", "generated_at": …, "repos": [<repo digest>]}`. One
/// repo or all of them take the same shape, so a caller never branches on which
/// it asked for. Every section is optional: a stanza that has not grown a field
/// yet loses that line and nothing else.
///
/// `status` rather than `ok`, because the envelope around this is always
/// `ok: true` — a viewer that is down is a fact about the deployment, not a
/// failure of the command.
pub fn digest(stanza: &Value) -> Value {
    let all = stanza
        .get("repos")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let repos: Vec<Value> = all.iter().take(REPO_CAP).map(repo_digest).collect();
    let mut out = json!({ "status": "ok", "repos": repos });
    if all.len() > REPO_CAP {
        out["elided"] = json!(all.len() - REPO_CAP);
    }
    if let Some(at) = stanza.get("generated_at").and_then(Value::as_str) {
        out["generated_at"] = json!(at);
    }
    scrub(&mut out);
    out
}

/// Strip terminal control bytes from every string in a built digest.
///
/// One sweep at the end rather than a `clean` at each of a dozen sinks. The
/// sinks are the problem: they are a list that grows, and the one somebody
/// forgets is the one carrying a repository name from a stranger's push. A
/// sweep covers the field nobody has added yet.
///
/// JSON encoding would escape these anyway. This is for the consumer that does
/// not decode — a shell pipeline, a log line, a person with `cat`.
fn scrub(value: &mut Value) {
    match value {
        Value::String(text) if text.chars().any(|c| c.is_control() || c == '\u{7f}') => {
            *text = clean(text);
        }
        Value::Array(rows) => rows.iter_mut().for_each(scrub),
        Value::Object(map) => map.values_mut().for_each(scrub),
        _ => {}
    }
}

/// The unavailable shape. Same `status` key as the success shape, so a caller
/// reads one field to know which of the two it got.
pub fn failure(reason: &str) -> Value {
    let mut out = json!({ "status": "unavailable", "error": reason });
    scrub(&mut out);
    out
}

fn repo_digest(repo: &Value) -> Value {
    let name = repo.get("name").and_then(Value::as_str).unwrap_or("?");

    // A mirror that could not be refreshed answers with the reason and nothing
    // else. Say that, rather than rendering five empty sections.
    if repo.get("available").and_then(Value::as_bool) == Some(false) {
        let why = repo
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unavailable");
        return json!({ "name": name, "available": false, "error": one_line(why) });
    }

    let mut out = json!({ "name": name, "available": true });
    if repo.get("stale").and_then(Value::as_bool) == Some(true) {
        out["stale"] = json!(true);
    }
    // `git_repo_json` reports an inference failure in place of the branches.
    if let Some(error) = repo.get("error").and_then(Value::as_str) {
        out["error"] = json!(one_line(error));
    }
    if let Some(default) = repo.get("default_branch").and_then(Value::as_str) {
        out["default_branch"] = json!(default);
    }

    let branches = branches_digest(repo);
    if !branches.is_empty() {
        out["branches"] = json!(branches);
    }
    if let Some(code) = code_digest(repo) {
        out["code"] = code;
    }
    if let Some(memory) = memory_digest(repo) {
        out["memory"] = memory;
    }
    let plans = plans_digest(repo);
    if !plans.is_empty() {
        out["plans"] = json!(plans);
    }
    if let Some(arch) = architecture_digest(repo) {
        out["architecture"] = arch;
    }
    let recent = recent_digest(repo);
    if !recent.is_empty() {
        out["recent"] = json!(recent);
    }
    out
}

/// Branches, default first and the rest by name.
///
/// The stanza's own order comes from a map walk, so it is not an order at all.
/// A digest an agent diffs between two reads has to be stable.
fn branches_digest(repo: &Value) -> Vec<Value> {
    let default = repo
        .get("default_branch")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut rows: Vec<Value> = repo
        .get("branches")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|branch| {
                    let tip = branch.get("tip").and_then(Value::as_str).unwrap_or_default();
                    json!({
                        "branch": branch.get("branch").and_then(Value::as_str).unwrap_or("?"),
                        "tip": short(tip),
                        "ahead": branch.get("ahead").and_then(Value::as_i64).unwrap_or(0),
                        "ci": branch.get("ci").and_then(Value::as_str).unwrap_or("none"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| {
        let key = |v: &Value| {
            let name = v["branch"].as_str().unwrap_or_default().to_owned();
            (name != default, name)
        };
        key(a).cmp(&key(b))
    });
    rows
}

/// What the code index holds, and how stale the answer is.
fn code_digest(repo: &Value) -> Option<Value> {
    let code = repo.get("code")?;
    let indexed = code.get("indexed").and_then(Value::as_bool).unwrap_or(false);
    let number = |key: &str| code.get(key).and_then(Value::as_i64).unwrap_or(0);
    let mut out = json!({
        "indexed": indexed,
        "files": number("files"),
        "symbols": number("symbols"),
        "chunks": number("chunks"),
    });
    // Three readings of one fact. The timestamp is the stable machine value, the
    // seconds are what the viewer computed, and the phrase is what a human reads;
    // an agent should never have to parse English to compare two ages.
    if let Some(at) = code.get("last_indexed_at").and_then(Value::as_str) {
        out["last_indexed_at"] = json!(at);
    }
    if let Some(secs) = code.get("age_seconds").and_then(Value::as_i64) {
        out["age_seconds"] = json!(secs);
        out["age"] = json!(ago(secs));
    } else if let Some(at) = code.get("last_indexed_at").and_then(Value::as_str) {
        out["age"] = json!(age_of(at));
    }
    Some(out)
}

/// What the digest has written into `brain/entities/`, and how much is waiting.
///
/// This is the section a session reads before its first search, so the entities keep
/// their fact lines verbatim: a slug and a date alone would send every agent straight
/// back to `grep`, which is the read this whole stanza exists to save.
///
/// Absent when the viewer has nothing to say — no entities, nothing undigested,
/// nothing disputed — the same rule the other sections follow. `path` is dropped: the
/// slug names the file, and `brain/entities/<slug>.md` is the whole of the mapping.
fn memory_digest(repo: &Value) -> Option<Value> {
    let memory = repo.get("memory")?;
    let number = |key: &str| memory.get(key).and_then(Value::as_i64).unwrap_or(0);
    let entities: Vec<Value> = memory
        .get("entities")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|entity| {
                    let slug = entity.get("slug").and_then(Value::as_str)?;
                    let mut out = json!({ "slug": slug });
                    if let Some(at) = entity.get("updated_at").and_then(Value::as_str)
                        && !at.is_empty()
                    {
                        out["updated_at"] = json!(at);
                        out["age"] = json!(age_of(at));
                    }
                    let facts: Vec<&str> = entity
                        .get("facts")
                        .and_then(Value::as_array)
                        .map(|facts| facts.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    if !facts.is_empty() {
                        out["facts"] = json!(facts);
                    }
                    Some(out)
                })
                .collect()
        })
        .unwrap_or_default();
    let undigested = number("undigested");
    let conflicts = number("conflicts");
    if entities.is_empty() && undigested == 0 && conflicts == 0 {
        return None;
    }
    Some(json!({
        "entities": entities,
        "undigested": undigested,
        "conflicts": conflicts,
    }))
}

/// Plan files, each with the number of comments filed against it.
fn plans_digest(repo: &Value) -> Vec<Value> {
    let counts = repo.get("open_comment_counts");
    repo.get("plans")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|plan| {
                    let path = plan.get("path").and_then(Value::as_str)?;
                    let open = counts
                        .and_then(|c| c.get(path))
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    Some(json!({ "path": path, "open_comments": open }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The latest architecture submission. The stanza may not carry this yet; when it
/// does not, the section is absent rather than empty.
fn architecture_digest(repo: &Value) -> Option<Value> {
    let arch = repo.get("architecture")?;
    let author = arch.get("latest_author").and_then(Value::as_str);
    let at = arch.get("latest_at").and_then(Value::as_str);
    let submissions = arch.get("submissions").and_then(Value::as_i64);
    if author.is_none() && at.is_none() && submissions.is_none() {
        return None;
    }
    let mut out = json!({});
    if let Some(n) = submissions {
        out["submissions"] = json!(n);
    }
    if let Some(who) = author {
        out["latest_author"] = json!(who);
    }
    if let Some(when) = at {
        out["latest_at"] = json!(when);
        out["age"] = json!(age_of(when));
    }
    Some(out)
}

/// The last five activity rows.
///
/// The stanza sorts activity oldest-first, and the digest keeps that order: the
/// tail of the list is the recent end, and reversing it would make the same five
/// facts read backwards from the source an agent may also fetch.
fn recent_digest(repo: &Value) -> Vec<Value> {
    let rows = repo
        .get("activity")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let start = rows.len().saturating_sub(RECENT);
    rows[start..]
        .iter()
        .map(|event| {
            // Absent rather than empty, the same rule the sections follow: a
            // missing branch is not a branch called "".
            let mut out = json!({});
            for key in ["type", "branch", "status", "author"] {
                if let Some(text) = event.get(key).and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out[key] = json!(text);
                }
            }
            if let Some(at) = event.get("at").and_then(Value::as_str)
                && !at.is_empty()
            {
                out["at"] = json!(at);
                out["age"] = json!(age_of(at));
            }
            out
        })
        .collect()
}

fn short(tip: &str) -> String {
    tip.chars().take(TIP_LEN).collect()
}

/// One line, whatever arrived.
///
/// A git error or a config parse failure is often several lines, and the promise
/// this command makes to a session-start hook is exactly one. Collapsing every
/// run of whitespace to a single space keeps that promise and drops the newlines
/// and tabs in the same pass.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Untrusted text, safe to print to a terminal.
///
/// A repository name, a branch, a plan path and an error string all come off the
/// wire, and a terminal reads C0 control bytes as commands: an ESC in a repo name
/// can clear the screen of whoever ran this. Drop the control range and DEL and
/// keep everything else, so a name in any script still reads as itself.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control() && *c != '\u{7f}').collect()
}

/// `nashcode brain [repo]`.
///
/// Never fails. This runs from a session-start hook, and the cost of a wrong
/// answer is one unhelpful field; the cost of a nonzero exit is the session. So
/// there is no `Result` here to get wrong: every path answers with a value, and
/// the caller wraps it in an `ok: true` envelope whose `status` says which kind
/// of answer it is.
pub fn run(ctx: &Ctx, args: &BrainArgs) -> Value {
    let (viewer, token) = match viewer_url(ctx) {
        Ok(pair) => pair,
        Err(reason) => return unavailable(&reason),
    };

    // An explicit name wins. Otherwise the repository `origin` points at, the way
    // `comments` resolves it — and outside a repository, no filter at all, which
    // is the whole-tailnet digest.
    let repo = match &args.repo {
        Some(name) => Some(name.clone()),
        None => vcs::detect_cwd()
            .ok()
            .flatten()
            .and_then(|ws| ws.origin_repo_name().ok().flatten().or_else(|| ws.default_repo_name())),
    };

    let url = brain_url(&viewer, repo.as_deref());
    let client = Client::with_timeout(&viewer, &token, TIMEOUT);
    let reply = match client.get_json(&url) {
        Ok(reply) => reply,
        Err(e) => return unavailable(&format!("viewer unreachable: {e:#}")),
    };
    if !reply.ok() {
        return unavailable(&format!("{url} returned HTTP {}", reply.status));
    }
    let stanza: Value = match serde_json::from_str(&reply.body) {
        Ok(value) => value,
        Err(e) => return unavailable(&format!("the viewer's /brain answer is not JSON: {e}")),
    };

    let digest = digest(&stanza);

    // "No repositories" is the truth for an empty viewer and a lie for a name it
    // has never heard of. An agent that mistyped a repo deserves to be told.
    if digest["repos"].as_array().is_some_and(|rows| rows.is_empty())
        && let Some(asked) = &repo
    {
        return unavailable(&format!("no repository named {asked} on the viewer"));
    }

    digest
}

/// The soft answer: exit 0, `status: unavailable`, and the reason.
///
/// The reason is collapsed to a single line here rather than at each call site: a
/// git error or a TOML parse failure arrives with newlines in it, and the one-line
/// promise is what makes this command safe in a session-start hook.
fn unavailable(reason: &str) -> Value {
    failure(&one_line(reason))
}

/// The viewer this profile names, or the reason there is none.
///
/// A missing profile and a missing viewer URL are configuration, not faults, and
/// they take the same soft path as a dead viewer: this command has no exit code
/// its caller can act on. `grep` shares both the resolution and that judgement.
pub fn viewer_url(ctx: &Ctx) -> std::result::Result<(String, String), String> {
    let (name, profile) = ctx.profile().map_err(|e| format!("{e:#}"))?;
    let viewer = profile
        .viewer_url
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            // Shared with `grep`, so it names neither command's subject: both the
            // brain and the code index live in the viewer.
            format!("profile `{name}` has no viewer URL; add one with `nashcode setup --viewer`")
        })?;
    Ok((viewer.trim_end_matches('/').to_string(), profile.token.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stanza in the shape `GET /brain` really answers, trimmed to what the
    /// digest reads and stretched to cover the edges a real fixture cannot hold
    /// at once (an unavailable repo, a missing architecture, no activity). The
    /// unaltered article, straight off the viewer, is what `brain_cli` runs on.
    fn stanza() -> Value {
        json!({
            "generated_at": "2026-08-19T12:00:00Z",
            "repos": [{
                "name": "demo",
                "available": true,
                "stale": false,
                "default_branch": "main",
                "branches": [
                    { "branch": "feature/x", "tip": "d4e5f6a7b8c9", "parent": "main", "ahead": 2, "ci": "fail" },
                    { "branch": "main", "tip": "a1b2c3d4e5f6", "parent": null, "ahead": 0, "ci": "ok" }
                ],
                "plans": [
                    { "path": "plans/a.md", "title": "A", "summary": "", "refs": { "branch": "main", "plan": null, "tasks": [] } },
                    { "path": "plans/b.md", "title": "B", "summary": "", "refs": { "branch": null, "plan": null, "tasks": [] } }
                ],
                "cards": {},
                "open_comment_counts": { "plans/a.md": 2 },
                "code": {
                    "indexed": true,
                    "last_indexed_at": "2026-08-16T12:00:00Z",
                    "age_seconds": 259_200,
                    "commit": "a1b2c3d",
                    "files": 42,
                    "chunks": 130,
                    "symbols": 88,
                    "embedded_chunks": 130
                },
                "architecture": {
                    "submissions": 3,
                    "latest_at": "2026-08-19T10:00:00Z",
                    "latest_author": "rob"
                },
                "activity": (1..=7)
                    .map(|i| json!({
                        "type": "merge",
                        "author": "rob",
                        "at": format!("2026-08-1{i}T00:00:00Z"),
                        "branch": format!("b{i}"),
                        "detail": ""
                    }))
                    .collect::<Vec<_>>()
            }]
        })
    }

    fn only_repo(d: &Value) -> Value {
        d["repos"][0].clone()
    }

    #[test]
    fn the_url_carries_the_repo_filter_only_when_there_is_one() {
        assert_eq!(brain_url("https://v/", Some("demo")), "https://v/brain?repo=demo");
        assert_eq!(brain_url("https://v", None), "https://v/brain");
        assert_eq!(brain_url("https://v", Some("a b")), "https://v/brain?repo=a%20b");
    }

    #[test]
    fn the_digest_keeps_the_facts_and_drops_the_aggregate() {
        let d = digest(&stanza());
        assert_eq!(d["status"], "ok");
        assert_eq!(d["generated_at"], "2026-08-19T12:00:00Z");
        let repo = only_repo(&d);
        assert_eq!(repo["name"], "demo");
        assert_eq!(repo["default_branch"], "main");
        // Cards and per-plan summaries are aggregate, not digest.
        assert!(repo.get("cards").is_none());
        assert!(repo["plans"][0].get("summary").is_none());
    }

    #[test]
    fn branches_lead_with_the_default_and_carry_tip_ahead_and_ci() {
        let repo = only_repo(&digest(&stanza()));
        let branches = repo["branches"].as_array().unwrap();
        assert_eq!(branches[0]["branch"], "main");
        assert_eq!(branches[0]["tip"], "a1b2c3d");
        assert_eq!(branches[0]["ahead"], 0);
        assert_eq!(branches[0]["ci"], "ok");
        assert_eq!(branches[1]["branch"], "feature/x");
        assert_eq!(branches[1]["tip"], "d4e5f6a");
        assert_eq!(branches[1]["ahead"], 2);
        assert_eq!(branches[1]["ci"], "fail");
    }

    #[test]
    fn the_code_stanza_becomes_counts_plus_a_humanised_age() {
        let repo = only_repo(&digest(&stanza()));
        let code = &repo["code"];
        assert_eq!(code["indexed"], true);
        assert_eq!(code["files"], 42);
        assert_eq!(code["symbols"], 88);
        assert_eq!(code["chunks"], 130);
        assert_eq!(code["age"], "3 days ago");
        assert_eq!(code["age_seconds"], 259_200);
    }

    #[test]
    fn a_repo_that_was_never_indexed_says_so_without_an_age() {
        let mut s = stanza();
        s["repos"][0]["code"] = json!({ "indexed": false, "files": 0, "chunks": 0, "symbols": 0 });
        let repo = only_repo(&digest(&s));
        assert_eq!(repo["code"]["indexed"], false);
        assert!(repo["code"].get("age").is_none());
    }

    #[test]
    fn plans_carry_their_open_comment_count() {
        let repo = only_repo(&digest(&stanza()));
        let plans = repo["plans"].as_array().unwrap();
        assert_eq!(plans[0]["path"], "plans/a.md");
        assert_eq!(plans[0]["open_comments"], 2);
        assert_eq!(plans[1]["path"], "plans/b.md");
        assert_eq!(plans[1]["open_comments"], 0);
    }

    #[test]
    fn architecture_survives_when_it_is_there_and_vanishes_when_it_is_not() {
        let repo = only_repo(&digest(&stanza()));
        assert_eq!(repo["architecture"]["submissions"], 3);
        assert_eq!(repo["architecture"]["latest_author"], "rob");
        assert!(repo["architecture"]["age"].is_string());

        // The viewer has not grown the field yet: degrade, do not invent.
        let mut s = stanza();
        s["repos"][0].as_object_mut().unwrap().remove("architecture");
        assert!(only_repo(&digest(&s)).get("architecture").is_none());
    }

    #[test]
    fn recent_is_the_last_five_activity_rows_in_the_stanzas_own_order() {
        let repo = only_repo(&digest(&stanza()));
        let recent = repo["recent"].as_array().unwrap();
        assert_eq!(recent.len(), RECENT);
        let branches: Vec<&str> = recent.iter().map(|e| e["branch"].as_str().unwrap()).collect();
        assert_eq!(branches, ["b3", "b4", "b5", "b6", "b7"]);
        assert_eq!(recent[0]["type"], "merge");
    }

    #[test]
    fn fewer_than_five_events_are_not_padded_and_do_not_panic() {
        let mut s = stanza();
        s["repos"][0]["activity"] = json!([]);
        assert!(only_repo(&digest(&s)).get("recent").is_none());

        s["repos"][0]["activity"] = json!([{ "type": "comment", "branch": "main", "at": "2026-08-19T11:00:00Z" }]);
        assert_eq!(only_repo(&digest(&s))["recent"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_ci_row_keeps_its_status_and_a_merge_row_has_none() {
        let mut s = stanza();
        s["repos"][0]["activity"] = json!([
            { "type": "ci_run", "author": "ci", "at": "2026-08-19T11:00:00Z", "branch": "main", "status": "fail" }
        ]);
        let repo = only_repo(&digest(&s));
        assert_eq!(repo["recent"][0]["status"], "fail");
        assert!(only_repo(&digest(&stanza()))["recent"][0].get("status").is_none());
    }

    #[test]
    fn an_unavailable_repo_is_the_reason_and_nothing_else() {
        let s = json!({
            "generated_at": "2026-08-19T12:00:00Z",
            "repos": [{ "name": "dead", "available": false, "error": "mirror fetch failed" }]
        });
        let repo = only_repo(&digest(&s));
        assert_eq!(repo["available"], false);
        assert_eq!(repo["error"], "mirror fetch failed");
        // No empty sections dressed up as an answer.
        assert_eq!(repo.as_object().unwrap().len(), 3, "{repo}");
    }

    #[test]
    fn the_code_stanza_keeps_the_machine_timestamp_beside_the_prose() {
        let repo = only_repo(&digest(&stanza()));
        assert_eq!(repo["code"]["last_indexed_at"], "2026-08-16T12:00:00Z");
    }

    #[test]
    fn an_event_missing_a_branch_omits_the_key_rather_than_emitting_an_empty_one() {
        let mut s = stanza();
        s["repos"][0]["activity"] = json!([{ "type": "comment", "at": "2026-08-19T11:00:00Z" }]);
        let event = only_repo(&digest(&s))["recent"][0].clone();
        assert!(event.get("branch").is_none(), "{event}");
        assert!(event.get("status").is_none(), "{event}");
        assert_eq!(event["type"], "comment");

        // And an empty string on the wire is treated as absent, not printed.
        s["repos"][0]["activity"] = json!([{ "type": "", "branch": "", "at": "2026-08-19T11:00:00Z" }]);
        let event = only_repo(&digest(&s))["recent"][0].clone();
        assert!(event.get("branch").is_none() && event.get("type").is_none(), "{event}");
    }

    #[test]
    fn a_viewer_with_too_many_repos_is_capped_and_says_how_many_it_left_out() {
        let repos: Vec<Value> = (0..REPO_CAP + 3)
            .map(|i| json!({ "name": format!("r{i}"), "available": true }))
            .collect();
        let d = digest(&json!({ "repos": repos }));
        assert_eq!(d["repos"].as_array().unwrap().len(), REPO_CAP);
        assert_eq!(d["elided"], 3);

        // At the cap exactly, nothing is elided and nothing is said.
        let repos: Vec<Value> = (0..REPO_CAP)
            .map(|i| json!({ "name": format!("r{i}"), "available": true }))
            .collect();
        let d = digest(&json!({ "repos": repos }));
        assert!(d.get("elided").is_none());
    }

    #[test]
    fn a_multi_line_reason_is_collapsed_to_the_one_line_this_command_promises() {
        assert_eq!(one_line("a\nb\n  c\t d "), "a b c d");
        let value = failure(&one_line("parse error\n  at line 3\n  in config.toml"));
        assert_eq!(value["error"], "parse error at line 3 in config.toml");

        // The same applies to a mirror message arriving inside a repo row.
        let s = json!({
            "repos": [{ "name": "dead", "available": false, "error": "fatal: bad band\nremote hung up" }]
        });
        assert_eq!(only_repo(&digest(&s))["error"], "fatal: bad band remote hung up");
    }

    #[test]
    fn control_characters_from_the_wire_never_reach_the_digest() {
        // Every string sink the digest has, all carrying an escape at once:
        // the sweep covers the ones nobody thought to list.
        let esc = "\u{1b}[2J\u{7f}";
        let s = json!({
            "generated_at": "2026-08-19T12:00:00Z",
            "repos": [{
                "name": format!("evil{esc}repo"),
                "available": true,
                "error": format!("mirror{esc}broke"),
                "default_branch": format!("ma{esc}in"),
                "branches": [{ "branch": format!("ma{esc}in"), "tip": "abc1234", "ahead": 0,
                               "ci": format!("o{esc}k") }],
                "plans": [{ "path": format!("plans/a{esc}.md"), "title": "A",
                            "refs": { "branch": "main", "plan": null, "tasks": [] } }],
                "open_comment_counts": {},
                "code": { "indexed": true, "files": 1, "chunks": 1, "symbols": 1,
                          "last_indexed_at": "2026-08-16T12:00:00Z" },
                "architecture": { "submissions": 1, "latest_author": format!("ro{esc}b"),
                                  "latest_at": "2026-08-19T10:00:00Z" },
                "activity": [{ "type": format!("merg{esc}e"), "branch": format!("b{esc}1"),
                               "at": "2026-08-19T11:00:00Z" }]
            }]
        });

        let digested = digest(&s);
        let mut strings = Vec::new();
        collect_strings(&digested, &mut strings);
        assert!(strings.len() > 8, "the fixture stopped exercising the sinks");
        for text in &strings {
            assert!(
                !text.chars().any(|c| c.is_control() || c == '\u{7f}'),
                "escape survived into {text:?}"
            );
        }

        // Only the control bytes go. What is left is inert text, still readable,
        // rather than a name silently replaced by nothing.
        let repo = only_repo(&digested);
        assert_eq!(repo["name"], "evil[2Jrepo");
        assert_eq!(repo["branches"][0]["branch"], "ma[2Jin");
        assert_eq!(repo["plans"][0]["path"], "plans/a[2J.md");
        assert_eq!(repo["architecture"]["latest_author"], "ro[2Jb");
        assert_eq!(repo["recent"][0]["type"], "merg[2Je");

        // And the soft answer is swept too.
        assert_eq!(failure("viewer\u{1b}[2J down")["error"], "viewer[2J down");
    }

    /// Every string anywhere in a value.
    fn collect_strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) => out.push(s.clone()),
            Value::Array(rows) => rows.iter().for_each(|r| collect_strings(r, out)),
            Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
            _ => {}
        }
    }

    #[test]
    fn a_dead_viewer_is_a_status_not_an_error_envelope() {
        let value = failure("viewer unreachable: connection refused");
        assert_eq!(value["status"], "unavailable");
        assert_eq!(value["error"], "viewer unreachable: connection refused");
        // No `ok` key: the envelope around this one is always ok.
        assert!(value.get("ok").is_none(), "{value}");
    }

    #[test]
    fn an_empty_brain_is_an_empty_list_rather_than_a_missing_key() {
        let d = digest(&json!({ "repos": [] }));
        assert_eq!(d["status"], "ok");
        assert_eq!(d["repos"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn memory_carries_the_slug_the_date_and_the_facts() {
        let mut s = stanza();
        s["repos"][0]["memory"] = json!({
            "entities": [{
                "slug": "postgres-migration",
                "path": "brain/entities/postgres-migration.md",
                "updated_at": "2026-08-19T10:00:00Z",
                "facts": ["Scheduled for July (as of 2026-06-10, context/meeting/2026/06/b.md)"],
            }],
            "undigested": 2,
            "conflicts": 1,
        });
        let memory = only_repo(&digest(&s))["memory"].clone();
        assert_eq!(memory["entities"][0]["slug"], "postgres-migration");
        assert_eq!(memory["entities"][0]["updated_at"], "2026-08-19T10:00:00Z");
        assert!(memory["entities"][0]["age"].is_string());
        assert_eq!(memory["entities"][0]["facts"].as_array().unwrap().len(), 1);
        assert_eq!(memory["undigested"], 2);
        assert_eq!(memory["conflicts"], 1);
        // The slug names the file; the path is the aggregate's business.
        assert!(memory["entities"][0].get("path").is_none(), "{memory}");
    }

    #[test]
    fn a_memory_with_nothing_in_it_is_not_printed() {
        let mut s = stanza();
        s["repos"][0]["memory"] = json!({ "entities": [], "undigested": 0, "conflicts": 0 });
        assert!(only_repo(&digest(&s)).get("memory").is_none());

        // Undigested files alone are worth saying: the memory is behind the store.
        s["repos"][0]["memory"] = json!({ "entities": [], "undigested": 3, "conflicts": 0 });
        assert_eq!(only_repo(&digest(&s))["memory"]["undigested"], 3);
    }

    #[test]
    fn every_section_is_optional() {
        let s = json!({ "repos": [{ "name": "bare", "available": true }] });
        let repo = only_repo(&digest(&s));
        assert_eq!(repo, json!({ "name": "bare", "available": true }));
    }
}
