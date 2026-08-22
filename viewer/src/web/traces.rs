//! The Agent tab: record agent sessions, read them back, and render them as
//! conversations.
//!
//! One tab, two pages. `/{repo}/agent` lists the sessions and searches every prompt in
//! the repo; `/{repo}/agent/{session}` reads one run top to bottom. The `/traces` and
//! `/prompts` URLs that came before redirect here for a browser, but their JSON keeps
//! working at the old paths — agents push to them.

use std::collections::BTreeMap;

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{
    HeaderValue, StatusCode, content::Json, error::bad_request, header, path_param, request,
    response::{IntoResponse, Response},
    route,
};
use topcoat::view::view;

use crate::db::{Prompt, TraceSession};
use crate::render;
use crate::traces::{self, Piece, TraceBatch};
use crate::web::components::{Raw, shell};
use crate::web::{app, repo_ctx};

path_param!(repo);
path_param!(session);
path_param!(sha);

/// A permanent move. The old page is gone; the URL is not.
fn moved(to: &str) -> Result<Response> {
    let mut response = Response::new(topcoat::router::Body::empty());
    *response.status_mut() = StatusCode::MOVED_PERMANENTLY;
    response.headers_mut().insert(header::LOCATION, HeaderValue::from_str(to)?);
    Ok(response)
}

/// The request's query string, `?` included, so a redirect keeps the search.
fn query_suffix(cx: &Cx) -> String {
    request::uri(cx).query().filter(|q| !q.is_empty()).map_or(String::new(), |q| format!("?{q}"))
}

fn json_response(status: StatusCode, body: String) -> Response {
    let mut response = Response::new(topcoat::router::Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// The read endpoints double as pages: JSON when the client asks for it, HTML for a
/// browser.
fn wants_json(cx: &Cx) -> bool {
    request::headers(cx)
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.starts_with("application/json"))
}

/// Session ids arrive from other programs; keep the URL surface boring.
fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !session.starts_with('.')
}

fn known_repo(cx: &Cx, name: &str) -> Result<()> {
    if app(cx).config.knows_repo(name) {
        Ok(())
    } else {
        Err(topcoat::router::error::not_found().into())
    }
}

// ---- write side ------------------------------------------------------------------

/// `POST /{repo}/traces/events` — a batch of events. Idempotent on `(session, seq)`.
#[route(POST "/{repo}/traces/events")]
async fn post_events(cx: &Cx, Json(batch): Json<TraceBatch>) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    if !valid_session(&batch.session) {
        return Err(bad_request("session id must be [A-Za-z0-9._-], not starting with a dot").into());
    }
    if batch.events.is_empty() {
        return Err(bad_request("events must not be empty").into());
    }
    if batch.events.len() > 5000 {
        return Err(bad_request("too many events in one batch; split it").into());
    }

    let mirror = app(cx).mirrors.repo(&name);
    let outcome = traces::ingest(&app(cx).db, &name, &batch, Some(&mirror)).await?;
    Ok(json_response(StatusCode::OK, serde_json::to_string(&outcome)?))
}

#[topcoat::router::query_params(error = bad_request)]
struct TranscriptQuery {
    /// `?replace=1` — overwrite a transcript this session already has.
    replace: Option<String>,
}

/// `POST /{repo}/traces/{session}/transcript` — the raw transcript, stored verbatim.
///
/// A transcript is written once. A second upload for the same session is a mistake far
/// more often than an intention — two runs sharing an id, a backfill pointed at the
/// wrong session — so it is refused unless the caller says `?replace=1`.
#[route(POST "/{repo}/traces/{session}/transcript")]
async fn post_transcript(cx: &Cx, body: topcoat::router::request::Bytes) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    let session = path_param::<Session>(cx).to_owned();
    if !valid_session(&session) {
        return Err(bad_request("bad session id").into());
    }
    if body.is_empty() {
        return Err(bad_request("empty transcript").into());
    }
    let query = topcoat::router::query_params::<TranscriptQuery>(cx)?;
    let replace = query.replace.as_deref() == Some("1");
    let path = traces::transcript_path(&app(cx).config.traces, &name, &session);
    if path.exists() && !replace {
        return Ok(json_response(
            StatusCode::CONFLICT,
            serde_json::json!({
                "error": format!("session {session} already has a transcript; \
                                  post again with ?replace=1 to overwrite it"),
            })
            .to_string(),
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| bad_request(format!("cannot store transcript: {error}")))?;
    }
    std::fs::write(&path, &body)
        .map_err(|error| bad_request(format!("cannot store transcript: {error}")))?;
    Ok(json_response(
        StatusCode::OK,
        serde_json::json!({ "ok": true, "bytes": body.len() }).to_string(),
    ))
}

// ---- read side -------------------------------------------------------------------

/// `GET /{repo}/traces/{session}/transcript` — the stored bytes back, verbatim.
#[route(GET "/{repo}/traces/{session}/transcript")]
async fn get_transcript(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    let session = path_param::<Session>(cx).to_owned();
    if !valid_session(&session) {
        return Err(bad_request("bad session id").into());
    }
    let path = traces::transcript_path(&app(cx).config.traces, &name, &session);
    let Ok(bytes) = std::fs::read(&path) else {
        return Err(topcoat::router::error::not_found().into());
    };
    let mut response = Response::new(topcoat::router::Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Ok(response)
}

/// `GET /{repo}/commits/{sha}/trace` — the session(s) that produced a commit.
#[route(GET "/{repo}/commits/{sha}/trace")]
async fn commit_trace(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    let sha = path_param::<Sha>(cx).to_owned();
    let sessions = app(cx).db.trace_sessions_for_commit(&name, &sha)?;
    Ok(json_response(
        StatusCode::OK,
        serde_json::json!({ "commit": sha, "sessions": sessions }).to_string(),
    ))
}

/// `GET /{repo}/traces` — the sessions as JSON. The page moved to `/{repo}/agent`.
#[route(GET "/{repo}/traces")]
async fn traces_index(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    if !wants_json(cx) {
        return moved(&format!("/{name}/agent"));
    }
    let sessions = app(cx).db.trace_sessions(&name, 100)?;
    Ok(json_response(StatusCode::OK, serde_json::to_string(&sessions)?))
}

/// `GET /{repo}/traces/{session}` — one session as JSON. The page moved to
/// `/{repo}/agent/{session}`.
#[route(GET "/{repo}/traces/{session}")]
async fn trace_session_json(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    let session = path_param::<Session>(cx).to_owned();
    if !valid_session(&session) {
        return Err(bad_request("bad session id").into());
    }
    if !wants_json(cx) {
        return moved(&format!("/{name}/agent/{session}"));
    }
    let events = app(cx).db.trace_events(&name, &session)?;
    if events.is_empty() {
        return Err(topcoat::router::error::not_found().into());
    }
    let commits = app(cx).db.trace_session_commits(&name, &session)?;
    Ok(json_response(
        StatusCode::OK,
        serde_json::json!({
            "session": session,
            "events": events,
            "commits": commits,
        })
        .to_string(),
    ))
}

// ---- the Agent tab ---------------------------------------------------------------

#[topcoat::router::query_params(error = bad_request)]
struct PromptQuery {
    /// Substring filter over the prompt text.
    q: Option<String>,
    /// Narrow to one session.
    session: Option<String>,
}

/// What a session is called: its first prompt, or its id when nobody wrote one.
fn titles(prompts: &[Prompt]) -> BTreeMap<String, String> {
    // Prompts come back newest first, so the last one seen per session is its first.
    let mut by_session = BTreeMap::new();
    for prompt in prompts {
        by_session.insert(prompt.session.clone(), prompt.text.clone());
    }
    by_session
}

/// The prompt search both `/prompts` and `/agent` run, so the two can never drift.
fn search(cx: &Cx, repo: &str) -> Result<Vec<Prompt>> {
    // Owned copies: the parsed query borrows the request context.
    let query = topcoat::router::query_params::<PromptQuery>(cx)?;
    let needle = query.q.clone().map(|q| q.trim().to_owned()).filter(|q| !q.is_empty());
    let session = query.session.clone().filter(|s| !s.is_empty());
    Ok(app(cx).db.prompts(repo, needle.as_deref(), session.as_deref(), 300)?)
}

/// `GET /{repo}/prompts` — the prompt search as JSON. The page moved into
/// `/{repo}/agent`, which runs the same search.
#[route(GET "/{repo}/prompts")]
async fn prompts_json(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    if !wants_json(cx) {
        return moved(&format!("/{name}/agent{}", query_suffix(cx)));
    }
    Ok(json_response(StatusCode::OK, serde_json::to_string(&search(cx, &name)?)?))
}

/// `GET /{repo}/agent` — every session in the repo, and a search across every prompt.
#[route(GET "/{repo}/agent")]
async fn agent_index(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    let matches = search(cx, &name)?;

    if wants_json(cx) {
        return Ok(json_response(StatusCode::OK, serde_json::to_string(&matches)?));
    }

    let query = topcoat::router::query_params::<PromptQuery>(cx)?;
    let needle = query.q.clone().map(|q| q.trim().to_owned()).filter(|q| !q.is_empty());
    let narrowed = query.session.clone().filter(|s| !s.is_empty());
    let searching = needle.is_some() || narrowed.is_some();

    let titles = titles(&app(cx).db.prompts(&name, None, None, 2000)?);
    let mut sessions = app(cx).db.trace_sessions(&name, 100)?;
    if searching {
        let hit: std::collections::BTreeSet<&str> =
            matches.iter().map(|prompt| prompt.session.as_str()).collect();
        sessions.retain(|row| hit.contains(row.session.as_str()));
    }

    let search_value = needle.unwrap_or_default();
    let session_count = sessions.len();
    let match_count = matches.len();
    let page = view! { cx =>
        shell(title: format!("{name} · agent"), repo: name.clone(), active: "agent", status: Some(ctx.status.clone()),
            <div class="d-flex flex-items-center gap-2 mb-2">
                <h3 class="mb-0"><i class="ph ph-robot"></i>" Agent sessions"</h3>
                <span class="Counter">(session_count)</span>
                <form class="ml-auto" method="get" action=(format!("/{name}/agent"))>
                    <input
                        class="form-control input-sm"
                        type="search"
                        name="q"
                        placeholder="search prompts"
                        value=(search_value.clone())
                    >
                </form>
            </div>
            <div class="Box mb-3">
                if sessions.is_empty() {
                    <div class="Box-body color-fg-muted">
                        if searching {
                            "No session matches that search."
                        } else {
                            "No agent runs yet. Wire the "<code>"nashcode-viewer hook"</code>" into your agent, or backfill with "<code>"nashcode-viewer trace push"</code>"."
                        }
                    </div>
                }
                let n = &name;
                let t = &titles;
                for row in sessions {
                    session_row(
                        key: row.session.clone(),
                        repo: n.clone(),
                        title: t.get(&row.session).cloned().unwrap_or_else(|| row.session.clone()),
                        row: row,
                    )
                }
            </div>
            if searching {
                <h4 class="mb-2"><i class="ph ph-chat-teardrop-text"></i>" Matching prompts "<span class="Counter">(match_count)</span></h4>
                <div class="Box">
                    if matches.is_empty() {
                        <div class="Box-body color-fg-muted">"No prompt matches that search."</div>
                    }
                    let n = &name;
                    for prompt in matches {
                        <div key=(format!("{}-{}", prompt.session, prompt.seq)) class="Box-row">
                            <div class="nashcode-prompt-text">(prompt.text.clone())</div>
                            <div class="d-flex flex-items-center gap-2 mt-1 text-small color-fg-muted">
                                <i class="ph ph-robot"></i>
                                <a class="Link--secondary" href=(format!("/{n}/agent/{}", prompt.session))>
                                    (prompt.session.clone())
                                </a>
                                if let Some(agent) = &prompt.agent {
                                    <span class="Label">(agent.clone())</span>
                                }
                                <span class="ml-auto">(prompt.created_at.clone())</span>
                            </div>
                        </div>
                    }
                </div>
            }
        )
    }?;
    page.into_response(cx)
}

#[topcoat::view::component]
async fn session_row(#[into] repo: String, #[into] title: String, row: TraceSession) -> Result {
    view! {
        <div class="Box-row">
            <div class="d-flex flex-items-center gap-2">
                <i class="ph ph-robot color-fg-muted"></i>
                <a class="Link--primary" href=(format!("/{repo}/agent/{}", row.session))>
                    (title)
                </a>
                <span class="ml-auto color-fg-muted text-small">(row.last_event_at.clone())</span>
            </div>
            <div class="d-flex flex-items-center gap-2 mt-1 text-small color-fg-muted">
                <code>(row.session.clone())</code>
                if let Some(agent) = &row.agent {
                    <span class="Label">(agent.clone())</span>
                }
                <span class="Counter">(format!("{} events", row.events))</span>
                if row.commits > 0 {
                    <span class="Counter Counter--primary">(format!("{} commits", row.commits))</span>
                }
            </div>
        </div>
    }
}

/// One event, read as conversation, with the commits it produced.
struct Turn {
    seq: i64,
    kind: String,
    created_at: String,
    shown: Vec<Shown>,
    /// Commits that landed while this event was the newest one.
    produced: Vec<String>,
}

/// A piece of conversation with the page-local identity a Pierre diff needs.
#[derive(Clone)]
struct Shown {
    piece: Piece,
    mount: String,
    /// The `@pierre/diffs` payload, when this piece carries a file change.
    diff: Option<String>,
}

/// A tool result can be a whole file. Show enough to recognize it, not all of it.
const RESULT_LIMIT: usize = 4000;

fn trim_output(text: &str) -> String {
    if text.chars().count() <= RESULT_LIMIT {
        return text.to_owned();
    }
    let head: String = text.chars().take(RESULT_LIMIT).collect();
    format!("{head}\n… truncated")
}

/// `</script>` inside a JSON blob would end the tag early; escape the slash.
fn escape_json_for_script(json: &str) -> String {
    json.replace('<', "\\u003c")
}

/// `GET /{repo}/agent/{session}` — one run, read as the conversation it was.
#[route(GET "/{repo}/agent/{session}")]
async fn agent_session(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    let session = path_param::<Session>(cx).to_owned();
    if !valid_session(&session) {
        return Err(bad_request("bad session id").into());
    }
    let events = app(cx).db.trace_events(&name, &session)?;
    if events.is_empty() {
        return Err(topcoat::router::error::not_found().into());
    }
    let commits = app(cx).db.trace_session_commits(&name, &session)?;

    if wants_json(cx) {
        return Ok(json_response(
            StatusCode::OK,
            serde_json::json!({
                "session": session,
                "events": events,
                "commits": commits,
            })
            .to_string(),
        ));
    }

    let payloads: Vec<serde_json::Value> = events
        .iter()
        .map(|event| serde_json::from_str(&event.payload).unwrap_or_default())
        .collect();

    // The real patch arrives on the result line that answers a call, so collect those
    // first and hand each call the diff computed against the file on disk.
    let mut patches: BTreeMap<String, traces::FileEdit> = BTreeMap::new();
    for payload in &payloads {
        if let Some((id, edit)) = traces::result_edit(payload) {
            patches.insert(id, edit);
        }
    }

    // Interleave: after an event whose head moved, show the commit(s) it produced.
    let attributed: std::collections::BTreeSet<&String> = commits.iter().collect();
    let mut turns: Vec<Turn> = Vec::new();
    let mut previous_head: Option<String> = None;
    let mut mounts = 0usize;
    for (event, payload) in events.iter().zip(&payloads) {
        let mut shown = Vec::new();
        for mut piece in traces::read(&event.kind, payload) {
            if let Piece::Call(call) = &mut piece
                && let Some(id) = &call.id
                && let Some(edit) = patches.get(id)
            {
                call.edit = Some(edit.clone());
            }
            if let Piece::Output { text, .. } = &mut piece {
                *text = trim_output(text);
            }
            let mount = format!("diff-{mounts}");
            mounts += 1;
            let diff = match &piece {
                Piece::Call(call) => call.edit.as_ref().map(|edit| {
                    serde_json::json!({
                        "mount": mount,
                        "file": edit.path,
                        "patch": edit.patch,
                        "annotations": [],
                    })
                    .to_string()
                }),
                _ => None,
            };
            shown.push(Shown { piece, mount, diff });
        }

        let mut produced = Vec::new();
        if let Some(head) = &event.head {
            if previous_head.as_deref().is_some_and(|prev| prev != head)
                && attributed.contains(head)
            {
                produced.push(head.clone());
            }
            previous_head = Some(head.clone());
        }

        // Nothing to read and no commit to point at: the row would say the event name
        // twice and nothing else. A commit always earns its row, whatever the payload.
        if shown.is_empty() && produced.is_empty() {
            continue;
        }
        turns.push(Turn {
            seq: event.seq,
            kind: event.kind.clone(),
            created_at: event.created_at.clone(),
            shown,
            produced,
        });
    }

    let title = titles(&app(cx).db.prompts(&name, None, Some(&session), 2000)?)
        .remove(&session)
        .unwrap_or_else(|| session.clone());
    let transcript_exists =
        traces::transcript_path(&app(cx).config.traces, &name, &session).exists();
    let page = view! { cx =>
        shell(title: format!("{name} · {session}"), repo: name.clone(), active: "agent", status: Some(ctx.status.clone()),
            <h3 class="mb-1"><i class="ph ph-robot"></i>" " (title)</h3>
            <div class="d-flex flex-items-center gap-2 mb-3 text-small color-fg-muted">
                <code>(session.clone())</code>
                if transcript_exists {
                    <a class="Link--secondary" href=(format!("/{name}/traces/{session}/transcript"))>
                        "raw transcript"
                    </a>
                }
                if !commits.is_empty() {
                    <i class="ph ph-git-commit"></i>
                    <span>"Commits produced:"</span>
                    for sha in commits.clone() {
                        <code key=(sha.clone()) class="commit-sha">(sha.chars().take(8).collect::<String>())</code>
                    }
                }
            </div>
            <div class="Box">
                let n = &name;
                for turn in turns {
                    <div key=(turn.seq) class="Box-row">
                        <div class="d-flex flex-items-center gap-2 text-small color-fg-muted mb-1">
                            <span class="Label Label--secondary">(turn.kind.clone())</span>
                            <span>(format!("#{}", turn.seq))</span>
                            <span class="ml-auto">(turn.created_at.clone())</span>
                        </div>
                        for item in turn.shown {
                            piece_view(key: item.mount.clone(), repo: n.clone(), item: item)
                        }
                        for sha in turn.produced {
                            <div key=(sha.clone()) class="mt-1 text-small">
                                <i class="ph ph-git-commit color-fg-success"></i>
                                " committed "
                                <code class="commit-sha">(sha.chars().take(8).collect::<String>())</code>
                                " — "
                                <a href=(format!("/{n}/commits/{sha}/trace"))>"trace link"</a>
                            </div>
                        }
                    </div>
                }
            </div>
        )
    }?;
    page.into_response(cx)
}

/// One piece of the conversation. Prose renders as markdown; reasoning, tool input, and
/// tool output fold away; an error unfolds itself.
#[topcoat::view::component]
async fn piece_view(#[into] repo: String, item: Shown) -> Result {
    let Shown { piece, mount, diff } = item;
    view! {
        match piece {
            Piece::Prompt(text) => {
                <div class="d-flex gap-2 mt-1">
                    <i class="ph ph-user color-fg-muted"></i>
                    <div class="markdown-body nashcode-annotation-body">
                        (Raw(render::markdown(&text, &repo, None, &[])))
                    </div>
                </div>
            }
            Piece::Say(text) => {
                <div class="markdown-body nashcode-annotation-body mt-1">
                    (Raw(render::markdown(&text, &repo, None, &[])))
                </div>
            }
            Piece::Thinking(text) => {
                <details class="mt-1">
                    <summary class="color-fg-muted text-small">
                        <i class="ph ph-brain"></i>" Thinking"
                    </summary>
                    <div class="markdown-body nashcode-annotation-body">
                        (Raw(render::markdown(&text, &repo, None, &[])))
                    </div>
                </details>
            }
            Piece::Call(call) => {
                <div class="mt-1">
                    <div class="d-flex flex-items-center gap-2">
                        <i class="ph ph-wrench color-fg-muted"></i>
                        <strong>(call.name.clone())</strong>
                        if !call.detail.is_empty() {
                            <span class="text-small nashcode-code">(call.detail.clone())</span>
                        }
                    </div>
                    if !call.input.is_empty() {
                        <details>
                            <summary class="color-fg-muted text-small">"input"</summary>
                            <pre class="text-small nashcode-code nashcode-ci-log">(call.input.clone())</pre>
                        </details>
                    }
                    if let Some(edit) = &call.edit {
                        <div class="Box mt-2">
                            <div class="Box-header d-flex flex-items-center gap-2">
                                <code class="commit-sha">(edit.path.clone())</code>
                            </div>
                            <div class="nashcode-diff-mount" id=(mount.clone())>
                                <pre class="p-3 text-small nashcode-code nashcode-diff-fallback">(edit.patch.clone())</pre>
                            </div>
                            if let Some(json) = &diff {
                                <script type="application/json" class="nashcode-diff-data">(Raw(escape_json_for_script(json)))</script>
                            }
                        </div>
                    }
                </div>
            }
            Piece::Output { text, error } => {
                <details class=(if error { "mt-1 flash flash-error" } else { "mt-1" }) open=(error.then_some("open"))>
                    <summary class="color-fg-muted text-small">
                        if error {
                            <i class="ph ph-x-circle color-fg-danger"></i>" error"
                        } else {
                            <i class="ph ph-arrow-elbow-down-right"></i>" result"
                        }
                    </summary>
                    <pre class="text-small nashcode-code nashcode-ci-log">(text)</pre>
                </details>
            }
            Piece::Note(text) => {
                <div class="text-small mt-1 nashcode-code color-fg-muted">(text)</div>
            }
        }
    }
}

