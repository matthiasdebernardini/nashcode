//! Trace routes and pages: record agent sessions, read them back, and render them.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{
    HeaderValue, StatusCode, content::Json, error::bad_request, header, path_param, request,
    response::{IntoResponse, Response},
    route,
};
use topcoat::view::view;

use crate::db::{TraceEvent, TraceSession};
use crate::traces::{self, TraceBatch};
use crate::web::components::shell;
use crate::web::{app, repo_ctx};

path_param!(repo);
path_param!(session);
path_param!(sha);

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

/// `POST /{repo}/traces/{session}/transcript` — the raw transcript, stored verbatim.
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
    let path = traces::transcript_path(&app(cx).config.traces, &name, &session);
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

/// `GET /{repo}/traces` — sessions, newest first. JSON for `Accept: application/json`.
#[route(GET "/{repo}/traces")]
async fn traces_index(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    let sessions = app(cx).db.trace_sessions(&name, 100)?;

    if wants_json(cx) {
        return Ok(json_response(StatusCode::OK, serde_json::to_string(&sessions)?));
    }

    let page = view! { cx =>
        shell(title: format!("{name} · traces"), repo: name.clone(), active: "traces", status: Some(ctx.status.clone()),
            <h3 class="mb-2"><i class="ph ph-robot"></i>" Agent sessions"</h3>
            <div class="Box">
                if sessions.is_empty() {
                    <div class="Box-body color-fg-muted">
                        "No traces yet. Wire the "<code>"nashgit-viewer hook"</code>" into your agent, or backfill with "<code>"nashgit-viewer trace push"</code>"."
                    </div>
                }
                let n = &name;
                for row in sessions {
                    session_row(key: row.session.clone(), repo: n.clone(), row: row)
                }
            </div>
        )
    }?;
    page.into_response(cx)
}

#[topcoat::view::component]
async fn session_row(#[into] repo: String, row: TraceSession) -> Result {
    view! {
        <div class="Box-row d-flex flex-items-center gap-2">
            <i class="ph ph-robot color-fg-muted"></i>
            <a class="Link--primary" href=(format!("/{repo}/traces/{}", row.session))>
                (row.session.clone())
            </a>
            if let Some(agent) = &row.agent {
                <span class="Label">(agent.clone())</span>
            }
            <span class="Counter">(format!("{} events", row.events))</span>
            if row.commits > 0 {
                <span class="Counter Counter--primary">(format!("{} commits", row.commits))</span>
            }
            <span class="ml-auto color-fg-muted text-small">(row.last_event_at.clone())</span>
        </div>
    }
}

/// `GET /{repo}/traces/{session}` — the transcript top to bottom, commits inline.
#[route(GET "/{repo}/traces/{session}")]
async fn trace_session_page(cx: &Cx) -> Result<Response> {
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

    // Interleave: after an event whose head moved, show the commit(s) it produced.
    let commit_set: std::collections::BTreeSet<&String> = commits.iter().collect();
    let mut rows: Vec<(TraceEvent, String, Vec<String>)> = Vec::new();
    let mut previous_head: Option<String> = None;
    for event in events {
        let parsed: serde_json::Value =
            serde_json::from_str(&event.payload).unwrap_or_default();
        let summary = traces::summarize(&event.kind, &parsed);
        let mut produced = Vec::new();
        if let Some(head) = &event.head {
            if previous_head.as_deref().is_some_and(|prev| prev != head)
                && commit_set.contains(head)
            {
                produced.push(head.clone());
            }
            previous_head = Some(head.clone());
        }
        rows.push((event, summary, produced));
    }

    let transcript_exists =
        traces::transcript_path(&app(cx).config.traces, &name, &session).exists();
    let page = view! { cx =>
        shell(title: format!("{name} · trace {session}"), repo: name.clone(), active: "traces", status: Some(ctx.status.clone()),
            <div class="d-flex flex-items-center gap-2 mb-2">
                <h3 class="mb-0"><i class="ph ph-robot"></i>" " (session.clone())</h3>
                if transcript_exists {
                    <a class="Link--secondary text-small" href=(format!("/{name}/traces/{session}/transcript"))>
                        "raw transcript"
                    </a>
                }
            </div>
            if !commits.is_empty() {
                <div class="Box-row d-flex flex-items-center gap-2 mb-3 text-small">
                    <i class="ph ph-git-commit"></i>
                    <span class="color-fg-muted">"Commits produced:"</span>
                    let n = &name;
                    for sha in commits.clone() {
                        <code key=(sha.clone()) class="commit-sha">(sha.chars().take(8).collect::<String>())</code>
                    }
                    <span class="d-none">(n.clone())</span>
                </div>
            }
            <div class="Box">
                let n = &name;
                for (event, summary, produced) in rows {
                    <div key=(event.seq) class="Box-row">
                        <div class="d-flex flex-items-center gap-2">
                            <span class="Label Label--secondary">(event.kind.clone())</span>
                            <span class="color-fg-muted text-small">(format!("#{}", event.seq))</span>
                            <span class="ml-auto color-fg-muted text-small">(event.created_at.clone())</span>
                        </div>
                        <div class="text-small mt-1 nashgit-code">(summary)</div>
                        for sha in produced {
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

// ---- prompts ---------------------------------------------------------------------

#[topcoat::router::query_params(error = bad_request)]
struct PromptQuery {
    /// Substring filter over the prompt text.
    q: Option<String>,
    /// Narrow to one session.
    session: Option<String>,
}

/// `GET /{repo}/prompts` — every prompt written in this repo, newest first.
///
/// The prompts are already inside the traces; this page exists because what you asked
/// for is the most re-readable part of a session and should not need digging out of an
/// event list.
#[route(GET "/{repo}/prompts")]
async fn prompts_index(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    // Owned copies: the parsed query borrows the request context.
    let query = topcoat::router::query_params::<PromptQuery>(cx)?;
    let needle = query.q.clone().map(|q| q.trim().to_owned()).filter(|q| !q.is_empty());
    let session = query.session.clone().filter(|s| !s.is_empty());
    let prompts = app(cx).db.prompts(&name, needle.as_deref(), session.as_deref(), 300)?;

    if wants_json(cx) {
        return Ok(json_response(StatusCode::OK, serde_json::to_string(&prompts)?));
    }

    // Which prompts were followed by a commit, so the page can say what came of them.
    let traced: std::collections::BTreeSet<String> = app(cx)
        .db
        .trace_sessions(&name, 500)?
        .into_iter()
        .filter(|s| s.commits > 0)
        .map(|s| s.session)
        .collect();

    let search_value = needle.unwrap_or_default();
    let count = prompts.len();
    let page = view! { cx =>
        shell(title: format!("{name} · prompts"), repo: name.clone(), active: "prompts", status: Some(ctx.status.clone()),
            <div class="d-flex flex-items-center gap-2 mb-2">
                <h3 class="mb-0"><i class="ph ph-chat-teardrop-text"></i>" Prompts"</h3>
                <span class="Counter">(count)</span>
                <form class="ml-auto" method="get" action=(format!("/{name}/prompts"))>
                    <input
                        class="form-control input-sm"
                        type="search"
                        name="q"
                        placeholder="search prompts"
                        value=(search_value.clone())
                    >
                </form>
            </div>
            <div class="Box">
                if prompts.is_empty() {
                    <div class="Box-body color-fg-muted">
                        if search_value.is_empty() {
                            "No prompts recorded yet. Wire the "<code>"nashgit-viewer hook"</code>" into your agent."
                        } else {
                            "No prompt matches that search."
                        }
                    </div>
                }
                let n = &name;
                for prompt in prompts {
                    <div key=(format!("{}-{}", prompt.session, prompt.seq)) class="Box-row">
                        <div class="nashgit-prompt-text">(prompt.text.clone())</div>
                        <div class="d-flex flex-items-center gap-2 mt-1 text-small color-fg-muted">
                            <i class="ph ph-robot"></i>
                            <a class="Link--secondary" href=(format!("/{n}/traces/{}", prompt.session))>
                                (prompt.session.clone())
                            </a>
                            if let Some(agent) = &prompt.agent {
                                <span class="Label">(agent.clone())</span>
                            }
                            if traced.contains(&prompt.session) {
                                <span class="Label Label--success">"led to a commit"</span>
                            }
                            <span class="ml-auto">(prompt.created_at.clone())</span>
                        </div>
                    </div>
                }
            </div>
        )
    }?;
    page.into_response(cx)
}
