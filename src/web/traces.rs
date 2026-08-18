//! Trace routes and pages: record agent sessions, read them back, and render them.

use serde::Deserialize;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{
    HeaderValue, StatusCode, content::Json, error::bad_request, header, path_param, request,
    response::{IntoResponse, Response},
    route,
};
use topcoat::view::view;

use crate::db::{TraceEvent, TraceSession};
use crate::traces::{self, BatchIn};
use crate::web::components::{shell, unavailable_card};
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
async fn post_events(cx: &Cx, Json(batch): Json<BatchIn>) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    if !traces::valid_session(&batch.session) {
        return Err(bad_request("session id must be [A-Za-z0-9._-], not starting with a dot").into());
    }
    if batch.events.is_empty() {
        return Err(bad_request("events must not be empty").into());
    }
    if batch.events.len() > 5000 {
        return Err(bad_request("too many events in one batch; split it").into());
    }

    let mirror = app(cx).mirrors.repo(&name);
    let outcome = traces::record_batch(&app(cx).db, &mirror, &name, &batch).await?;
    Ok(json_response(StatusCode::OK, serde_json::to_string(&outcome)?))
}

/// `POST /{repo}/traces/{session}/transcript` — the raw transcript, stored verbatim.
#[route(POST "/{repo}/traces/{session}/transcript")]
async fn post_transcript(cx: &Cx, body: topcoat::router::request::Bytes) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    known_repo(cx, &name)?;
    let session = path_param::<Session>(cx).to_owned();
    if !traces::valid_session(&session) {
        return Err(bad_request("bad session id").into());
    }
    if body.is_empty() {
        return Err(bad_request("empty transcript").into());
    }
    traces::store_transcript(&app(cx).config, &name, &session, &body)
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
    if !traces::valid_session(&session) {
        return Err(bad_request("bad session id").into());
    }
    let path = traces::transcript_path(&app(cx).config, &name, &session);
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

#[derive(Debug, Default, Deserialize)]
struct Nothing {}

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
                        "No traces yet. Wire the "<code>"nashgit hook"</code>" into your agent, or backfill with "<code>"nashgit trace push"</code>"."
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
    if !traces::valid_session(&session) {
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
        let summary = traces::summarize(&event.kind, &event.payload);
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
        traces::transcript_path(&app(cx).config, &name, &session).exists();
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
