//! The bugs surface: the Sentry ingest endpoint and the pages that read what it
//! collected.
//!
//! Two audiences share this file on purpose. `/api/{project_id}/envelope/` talks to
//! unmodified official SDKs and has to obey the protocol exactly — the response
//! shape, the rate-limit header, the CORS header set, and above all "never fail an
//! envelope because one item type is unfamiliar". `/bugs` talks to a person and
//! follows the viewer's own conventions: a page for a browser, the same data as JSON
//! for `Accept: application/json`, Tailscale headers stamping every mutation.
//!
//! Every route answers 404 when no bucket is configured. See [`crate::bugs`].

use serde::Deserialize;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{
    Body, HeaderValue, StatusCode, error::bad_request, error::not_found, header,
    path_param, query_params, request,
    response::{IntoResponse, Response},
    route,
};
use topcoat::view::{component, view};

use crate::bugs::{Issue, Project, context, envelope, group, ingest, logs, state};
use crate::web::components::shell;
use crate::web::{actor, app, see_other};

path_param!(project_name);
path_param!(issue_id: i64, error = bad_request);
// An id that is not a number is not a project we have, which is a 404 to an SDK.
path_param!(project_id: i64, error = not_found);
path_param!(*rest);

/// The route SDKs post to.
///
/// The protocol puts a trailing slash on it and every current SDK sends one, but a
/// route path here cannot end in a slash and a catch-all needs a non-empty
/// remainder, so `/api/{id}/envelope/` matches neither `.../envelope` nor
/// `.../envelope/{*rest}`. One catch-all under `/api/{id}/` catches both spellings
/// and answers 404 to every other Sentry endpoint (`/store/`, `/minidump/`), which
/// is what we want to say about them anyway.
pub const INGEST_PATH: &str = "/api/{project_id}/{*rest}";
pub const INGEST_PATH_BARE: &str = "/api/{project_id}/envelope";
pub const INGEST_PATH_LOGS: &str = "/api/{project_id}/logs";

/// The two path suffixes the catch-all serves, each with and without its trailing
/// slash. Anything else under `/api/{id}/` is a Sentry endpoint we do not implement,
/// and 404 is what we mean to say about it.
fn rest_is(cx: &Cx, wanted: &str) -> bool {
    let mut segments = path_param::<Rest>(cx).filter(|segment| !segment.is_empty());
    segments.next() == Some(wanted) && segments.next().is_none()
}

fn is_envelope_path(cx: &Cx) -> bool {
    rest_is(cx, "envelope")
}

/// Relay's CORS allow list, verbatim. The browser SDK sends no custom headers by
/// default, but `fetchOptions` and a couple of Chrome quirks still produce
/// preflights, and a preflight that fails silently loses every browser event.
const ALLOW_HEADERS: &str = "x-sentry-auth, x-requested-with, x-forwarded-for, origin, referer, \
     accept, content-type, authentication, authorization, content-encoding, transfer-encoding";

/// The three response headers an SDK reads to back off. Unexposed, browser backoff
/// breaks without a single visible symptom.
const EXPOSE_HEADERS: &str = "x-sentry-error, x-sentry-rate-limits, retry-after";

/// Tell every SDK to stop sending what we do not store, for a day, on every answer.
///
/// `error`, `default`, `log_item`, `monitor` and `session` are deliberately absent —
/// those are the categories we want — and the category list is never empty, because
/// an empty list means "everything" and would silence the errors too.
const RATE_LIMITS: &str = "86400:transaction;span;profile;profile_chunk;replay;trace_metric:project";

// ---- ingest ----------------------------------------------------------------------

/// `OPTIONS /api/{project_id}/envelope/` — the browser preflight.
#[route(OPTIONS "/api/{project_id}/{*rest}")]
async fn preflight(cx: &Cx) -> Result<Response> {
    if !is_envelope_path(cx) && !rest_is(cx, "logs") {
        return Err(not_found().into());
    }
    preflight_answer(cx)
}

#[route(OPTIONS "/api/{project_id}/envelope")]
async fn preflight_bare(cx: &Cx) -> Result<Response> {
    preflight_answer(cx)
}

fn preflight_answer(cx: &Cx) -> Result<Response> {
    if !app(cx).bugs.enabled() {
        return Err(not_found().into());
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert("access-control-allow-methods", HeaderValue::from_static("POST"));
    headers.insert("access-control-allow-headers", HeaderValue::from_static(ALLOW_HEADERS));
    headers.insert("access-control-max-age", HeaderValue::from_static("3600"));
    cors(&mut response);
    Ok(response)
}

/// `POST /api/{project_id}/envelope/` — the one ingest route.
#[route(POST "/api/{project_id}/{*rest}")]
async fn ingest_envelope(cx: &Cx, body: Body) -> Result<Response> {
    if rest_is(cx, "logs") {
        return quietly(accept_logs(cx, body)).await;
    }
    if !is_envelope_path(cx) {
        return Err(not_found().into());
    }
    quietly(accept_envelope(cx, body)).await
}

#[route(POST "/api/{project_id}/envelope")]
async fn ingest_bare(cx: &Cx, body: Body) -> Result<Response> {
    quietly(accept_envelope(cx, body)).await
}

/// Everything an ingest door does is inside the error-tracking pipeline, so nothing it
/// logs — at any depth, including from git, the object store or SQLite — is ever
/// reported to the self-DSN. Reporting it would post an envelope through this same
/// door, and a door that fails would then fail again on its own report.
async fn quietly<F: std::future::Future>(future: F) -> F::Output {
    crate::bugs::selfreport::quietly(future).await
}

/// `POST /api/{project_id}/logs` — the second log door, for everything that is not a
/// Sentry SDK.
#[route(POST "/api/{project_id}/logs")]
async fn ingest_logs_bare(cx: &Cx, body: Body) -> Result<Response> {
    quietly(accept_logs(cx, body)).await
}

/// NDJSON in, one JSON object per line. Authenticated by the same DSN key as the
/// envelope route and held to the same caps.
///
/// There is no envelope header here, so the third auth door — the `dsn` inside the
/// body — does not exist: a key in `X-Sentry-Auth` or `?sentry_key=` is the whole of
/// it. That is also why the body is never read before the key is judged.
async fn accept_logs(cx: &Cx, body: Body) -> Result<Response> {
    let bugs = &app(cx).bugs;
    if !bugs.enabled() {
        return Err(not_found().into());
    }
    let id = *path_param::<ProjectId>(cx)?;
    let Some(project) = bugs.project_by_id(id)? else {
        return Ok(sentry_error(StatusCode::NOT_FOUND, "unknown project"));
    };
    if !project.active {
        return Ok(sentry_error(StatusCode::NOT_FOUND, "unknown project"));
    }
    match header_key(cx).or_else(|| query_key(cx)) {
        Some(key) if key == project.key => {}
        Some(_) => return Ok(sentry_error(StatusCode::FORBIDDEN, "wrong key for this project")),
        None => return Ok(sentry_error(StatusCode::FORBIDDEN, "no sentry_key")),
    }

    let raw = match ingest::read_capped(body, ingest::MAX_COMPRESSED).await {
        Ok(raw) => raw,
        Err(ingest::IngestError::TooLarge(what)) => {
            return Ok(sentry_error(StatusCode::PAYLOAD_TOO_LARGE, what));
        }
        Err(error) => return Ok(sentry_error(StatusCode::BAD_REQUEST, &error.to_string())),
    };
    let encoding = request::headers(cx)
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match ingest::decompress(encoding.as_deref(), raw) {
        Ok(body) => body,
        Err(ingest::IngestError::TooLarge(what)) => {
            return Ok(sentry_error(StatusCode::PAYLOAD_TOO_LARGE, what));
        }
        Err(error) => return Ok(sentry_error(StatusCode::BAD_REQUEST, &error.to_string())),
    };

    // One bad line is one bad line. A shipper that tails a file will send a truncated
    // one eventually, and refusing the batch over it would lose the good lines too.
    //
    // A batch with too many lines in it is a different thing, and is refused whole: a
    // line cap alone bounds nothing, since 100 MiB of `0\n` is fifty million lines
    // that each pass it.
    let parsed = logs::parse_ndjson(&body, envelope::MAX_ITEM, logs::MAX_BATCH);
    if parsed.too_many {
        return Ok(sentry_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("more than {} log lines in one batch", logs::MAX_BATCH),
        ));
    }
    let accepted = match bugs.accept_logs(project.id, &parsed.records, logs::source::NDJSON).await {
        Ok(count) => count,
        Err(error) => {
            tracing::error!(%error, project = project.name, "bugs: cannot store a log batch");
            return Ok(sentry_error(StatusCode::BAD_GATEWAY, "cannot store the logs"));
        }
    };

    // The reject count is exact; the line numbers are a capped sample. Echoing one
    // number per bad line lets a small request buy an enormous answer.
    let payload = serde_json::json!({ "accepted": accepted, "rejected": parsed.report() });
    let mut response = json_response(StatusCode::OK, payload.to_string());
    cors(&mut response);
    Ok(response)
}

async fn accept_envelope(cx: &Cx, body: Body) -> Result<Response> {
    let bugs = &app(cx).bugs;
    if !bugs.enabled() {
        return Err(not_found().into());
    }
    let id = *path_param::<ProjectId>(cx)?;
    let Some(project) = bugs.project_by_id(id)? else {
        // An unknown project is a 404 even though the request is well-formed: a
        // sender pointed at a project that does not exist should hear so.
        return Ok(sentry_error(StatusCode::NOT_FOUND, "unknown project"));
    };
    // A revoked key says the same thing here as it does at the public edge: absent.
    if !project.active {
        return Ok(sentry_error(StatusCode::NOT_FOUND, "unknown project"));
    }

    // Header and query auth cost nothing and can refuse before the body is read.
    let declared = header_key(cx).or_else(|| query_key(cx));
    if let Some(key) = &declared
        && key != &project.key
    {
        return Ok(sentry_error(StatusCode::FORBIDDEN, "wrong key for this project"));
    }

    let encoding = request::headers(cx)
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let compressed = !matches!(
        encoding.as_deref().unwrap_or("").trim().to_ascii_lowercase().as_str(),
        "" | "identity"
    );
    let mut reader = ingest::Reader::new(body);

    // The third auth source is the `dsn` header inside the envelope, which is how
    // Relay >= 21.6 lets a client authenticate with no header and no query string.
    // In an uncompressed body that is the first line, so it can be read and judged
    // before the other 20 MiB are pulled. In a compressed one it cannot be read at
    // all without expanding the body, so that path gets a small budget instead.
    let mut authed = declared.is_some();
    if !authed && !compressed {
        let line = match reader.header_line().await {
            Ok(line) => line,
            Err(ingest::IngestError::NoHeaderLine) => {
                return Ok(sentry_error(StatusCode::FORBIDDEN, "no sentry_key"));
            }
            Err(error) => return Ok(sentry_error(StatusCode::BAD_REQUEST, &error.to_string())),
        };
        match envelope_dsn_key(line) {
            Some(key) if key == project.key => authed = true,
            Some(_) => {
                return Ok(sentry_error(StatusCode::FORBIDDEN, "wrong key for this project"));
            }
            None => return Ok(sentry_error(StatusCode::FORBIDDEN, "no sentry_key")),
        }
    }

    let (compressed_cap, decompressed_cap) = if authed {
        (ingest::MAX_COMPRESSED, ingest::MAX_DECOMPRESSED)
    } else {
        (ingest::MAX_UNAUTHED_COMPRESSED, ingest::MAX_UNAUTHED_DECOMPRESSED)
    };

    let raw = match reader.read_to_end(compressed_cap).await {
        Ok(raw) => raw,
        Err(ingest::IngestError::TooLarge(what)) => {
            return Ok(sentry_error(StatusCode::PAYLOAD_TOO_LARGE, what));
        }
        Err(error) => return Ok(sentry_error(StatusCode::BAD_REQUEST, &error.to_string())),
    };
    let body = match ingest::decompress_capped(encoding.as_deref(), raw, decompressed_cap) {
        Ok(body) => body,
        Err(ingest::IngestError::TooLarge(what)) => {
            return Ok(sentry_error(StatusCode::PAYLOAD_TOO_LARGE, what));
        }
        Err(error) => return Ok(sentry_error(StatusCode::BAD_REQUEST, &error.to_string())),
    };

    let split = match envelope::split(&body) {
        Ok(split) => split,
        Err(envelope::SplitError::ItemTooLarge { ty, len }) => {
            return Ok(sentry_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("a {ty} item of {len} bytes is over the 1 MiB limit"),
            ));
        }
        Err(error) => return Ok(sentry_error(StatusCode::BAD_REQUEST, &error.to_string())),
    };

    // The compressed half of the envelope-`dsn` door, now that the body is readable.
    if !authed {
        let key = split.dsn().and_then(|dsn| {
            dsn.parse::<sentry_types::Dsn>().ok().map(|dsn| dsn.public_key().to_owned())
        });
        match key {
            Some(key) if key == project.key => {}
            Some(_) => {
                return Ok(sentry_error(StatusCode::FORBIDDEN, "wrong key for this project"));
            }
            None => return Ok(sentry_error(StatusCode::FORBIDDEN, "no sentry_key")),
        }
    }

    // An envelope with no identifiable event gets one minted for it rather than a
    // bare `{}`. Relay answers `{}` in that case, but an SDK that reads the id back
    // has something to correlate either way, and no SDK has ever been harmed by an
    // id it did not ask for.
    let event_id = split.event_id().unwrap_or_else(crate::bugs::digest::new_event_id);

    // Durable first, understood later: the bytes are in the bucket before we answer.
    match bugs.accept(project.id, body).await {
        Ok(()) => {}
        Err(crate::bugs::AcceptError::Busy) => return Ok(busy()),
        Err(error) => {
            tracing::error!(%error, project = project.name, "bugs: cannot store an envelope");
            return Ok(sentry_error(StatusCode::BAD_GATEWAY, "cannot store the envelope"));
        }
    }

    let payload = serde_json::json!({ "id": event_id });
    let mut response = json_response(StatusCode::OK, payload.to_string());
    response
        .headers_mut()
        .insert("x-sentry-rate-limits", HeaderValue::from_static(RATE_LIMITS));
    cors(&mut response);
    Ok(response)
}

/// The digest queue is full: behind, not broken. An SDK reads `Retry-After` and backs
/// off on its own, which is the whole reason the queue is bounded.
fn busy() -> Response {
    let mut response = sentry_error(StatusCode::TOO_MANY_REQUESTS, "the digest queue is full");
    response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

/// The public key of the `dsn` in one envelope header line.
fn envelope_dsn_key(line: &[u8]) -> Option<String> {
    let headers: serde_json::Value = serde_json::from_slice(line).ok()?;
    let dsn = headers.get("dsn")?.as_str()?;
    dsn.parse::<sentry_types::Dsn>().ok().map(|dsn| dsn.public_key().to_owned())
}

/// `X-Sentry-Auth: Sentry sentry_key=..., sentry_version=7, ...`
fn header_key(cx: &Cx) -> Option<String> {
    let raw = request::headers(cx).get("x-sentry-auth")?.to_str().ok()?;
    raw.parse::<sentry_types::Auth>().ok().map(|auth| auth.public_key().to_owned())
}

/// `?sentry_key=...` — how the browser SDK authenticates, precisely so its POST stays
/// a CORS simple request.
fn query_key(cx: &Cx) -> Option<String> {
    let query = request::uri(cx).query()?;
    sentry_types::Auth::from_querystring(query.as_bytes())
        .ok()
        .map(|auth| auth.public_key().to_owned())
}

/// An error an SDK can read: the documented `{"detail": "..."}` body, the
/// `X-Sentry-Error` header Relay sets, and the CORS headers without which a browser
/// sees none of it.
fn sentry_error(status: StatusCode, detail: &str) -> Response {
    let body = serde_json::json!({ "detail": detail }).to_string();
    let mut response = json_response(status, body);
    if let Ok(value) = HeaderValue::from_str(detail) {
        response.headers_mut().insert("x-sentry-error", value);
    }
    cors(&mut response);
    response
}

/// Wildcard origin is correct here: the credential is the sentry_key in the request,
/// never the browser's origin.
fn cors(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert("access-control-expose-headers", HeaderValue::from_static(EXPOSE_HEADERS));
}

// ---- pages -----------------------------------------------------------------------

fn wants_json(cx: &Cx) -> bool {
    request::headers(cx)
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.starts_with("application/json"))
}

fn is_form(cx: &Cx) -> bool {
    request::content_type(cx)
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"))
}

fn json_response(status: StatusCode, body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// Guard every page: with no bucket there is no feature.
fn on(cx: &Cx) -> Result<()> {
    if app(cx).bugs.enabled() { Ok(()) } else { Err(not_found().into()) }
}

/// `GET /bugs` — every project with its open-issue count.
#[route(GET "/bugs")]
async fn projects_page(cx: &Cx) -> Result<Response> {
    on(cx)?;
    let projects = app(cx).bugs.projects()?;
    let budget = app(cx).bugs.push_budget()?;
    let notifies = app(cx).bugs.notifies();
    if wants_json(cx) {
        // An object rather than the bare array this used to be. Whether a notification
        // can still get out this month is part of the state of the feature, and a
        // reader that has to make a second request for it will not make it.
        let body = serde_json::json!({
            "projects": projects,
            "pushover": { "on": notifies, "budget": budget },
        });
        return Ok(json_response(StatusCode::OK, body.to_string()));
    }

    let page = view! { cx =>
        shell(title: "bugs".to_owned(),
            <h3 class="mb-2"><i class="ph ph-bug"></i>" Bugs"</h3>
            <p class="color-fg-muted">
                "One project is one DSN. Point an official Sentry SDK at it and its
                 errors land here."
            </p>
            if projects.is_empty() {
                <div class="Box mb-3"><div class="Box-body color-fg-muted">
                    "No projects yet."
                </div></div>
            } else {
                <div class="Box mb-3">
                    for summary in &projects {
                        <div key=(summary.project.id) class="Box-row d-flex flex-items-center gap-2">
                            <a class="Link--primary" href=(format!("/bugs/{}", summary.project.name))>
                                (summary.project.name.clone())
                            </a>
                            if summary.unresolved > 0 {
                                <span class="Label Label--danger">
                                    (summary.unresolved)" unresolved"
                                </span>
                            }
                            <span class="ml-auto text-small color-fg-muted">
                                (summary.issues)" issues · "(summary.events)" events"
                            </span>
                        </div>
                    }
                </div>
            }
            notifications(notifies: notifies, budget: budget)
            <div class="Box">
                <div class="Box-header"><strong>"New project"</strong></div>
                <div class="Box-body">
                    <form method="post" action="/bugs" class="d-flex gap-2 flex-items-center">
                        <input class="form-control" type="text" name="name" placeholder="name" required=(true)>
                        <input class="form-control" type="text" name="repo" placeholder="repo (optional)">
                        <button class="btn btn-primary" type="submit">"Create"</button>
                    </form>
                </div>
            </div>
        )
    }?;
    page.into_response(cx)
}

#[derive(Debug, Default, Deserialize)]
struct ProjectIn {
    name: Option<String>,
    repo: Option<String>,
}

/// `POST /bugs {name, repo?}` — create a project and mint its DSN.
#[route(POST "/bugs")]
async fn create_project(cx: &Cx, body: request::Bytes) -> Result<Response> {
    on(cx)?;
    let form = is_form(cx);
    let input: ProjectIn = if form {
        serde_urlencoded::from_bytes(&body).map_err(|e| bad_request(e.to_string()))?
    } else {
        serde_json::from_slice(&body).map_err(|e| bad_request(e.to_string()))?
    };

    let name = input.name.unwrap_or_default();
    let repo = input.repo.filter(|repo| !repo.trim().is_empty());
    // An unknown repo would render a dead cross-link on every issue.
    if let Some(repo) = &repo
        && !app(cx).config.knows_repo(repo)
    {
        return Err(bad_request(format!("unknown repo {repo}")).into());
    }

    let project = app(cx)
        .bugs
        .create_project(&name, repo.as_deref())
        .map_err(bad_request)?;

    if form {
        return see_other(&format!("/bugs/{}", project.name));
    }
    Ok(json_response(StatusCode::CREATED, serde_json::to_string(&project)?))
}

#[query_params(error = bad_request)]
struct IssuesQuery {
    /// `unresolved`, `resolved` or `muted`. Absent means all of them.
    state: Option<String>,
}

/// `GET /bugs/{project}` — the DSN, the SDK snippet, and the issues.
#[route(GET "/bugs/{project_name}")]
async fn project_page(cx: &Cx) -> Result<Response> {
    on(cx)?;
    let name = path_param::<ProjectName>(cx).to_owned();
    let Some(project) = app(cx).bugs.project(&name)? else {
        return Err(not_found().into());
    };
    let query = query_params::<IssuesQuery>(cx)?;
    let filter = match query.state.as_deref() {
        Some(value) if !state::known(value) => {
            return Err(bad_request("state is unresolved, resolved or muted").into());
        }
        other => other,
    };
    let issues = app(cx).bugs.issues(project.id, filter)?;
    let dsn = app(cx).bugs.dsn(&project);

    if wants_json(cx) {
        let body = serde_json::json!({ "project": project, "dsn": dsn, "issues": issues });
        return Ok(json_response(StatusCode::OK, body.to_string()));
    }

    let page = view! { cx =>
        shell(title: format!("{name} · bugs"),
            <div class="d-flex flex-items-center gap-2 mb-2">
                <h3><i class="ph ph-bug"></i>" "(name.clone())</h3>
                <a class="ml-auto Link--primary text-small" href=(format!("/bugs/{name}/logs"))>"Logs"</a>
                <a class="Link--primary text-small" href="/bugs">"All projects"</a>
            </div>
            dsn_card(dsn: dsn, project: project.clone())
            <div class="d-flex gap-2 mb-2">
                for (label, value) in state_tabs() {
                    <a key=(label) class=(if filter == value { "btn btn-sm btn-primary" } else { "btn btn-sm" })
                       href=(match value {
                           Some(value) => format!("/bugs/{name}?state={value}"),
                           None => format!("/bugs/{name}"),
                       })>(label)</a>
                }
            </div>
            if issues.is_empty() {
                <div class="Box"><div class="Box-body color-fg-muted">"Nothing here."</div></div>
            } else {
                <div class="Box">
                    let project = &name;
                    for issue in issues {
                        issue_row(key: issue.id, project: project.clone(), issue: issue)
                    }
                </div>
            }
        )
    }?;
    page.into_response(cx)
}

fn state_tabs() -> [(&'static str, Option<&'static str>); 4] {
    [
        ("All", None),
        ("Unresolved", Some(state::UNRESOLVED)),
        ("Resolved", Some(state::RESOLVED)),
        ("Muted", Some(state::MUTED)),
    ]
}

/// What the notification path can still do.
///
/// The monthly allowance is on this page because it is the number that decides whether
/// the next real incident reaches a phone, and there is no other place a person would
/// think to look for it before it runs out.
#[component]
async fn notifications(notifies: bool, budget: crate::bugs::pushover::Budget) -> Result {
    let allowance = match (budget.remaining, budget.limit) {
        (Some(remaining), Some(limit)) => Some(format!("{remaining} of {limit} left this month")),
        (Some(remaining), None) => Some(format!("{remaining} left this month")),
        _ => None,
    };
    view! {
        <div class="Box mb-3">
            <div class="Box-header d-flex flex-items-center gap-2">
                <strong>"Notifications"</strong>
                <span class="ml-auto text-small color-fg-muted">
                    if notifies { "Pushover" } else { "off" }
                </span>
            </div>
            <div class="Box-body text-small color-fg-muted">
                if !notifies {
                    "Set NASHCODE_PUSHOVER_TOKEN and NASHCODE_PUSHOVER_USER to be told about a
                     new issue or a regression. Everything on this page works without them."
                } else {
                    <span>
                        "New issues, regressions and the 10/100/1000 rungs. Never one per event."
                        if let Some(allowance) = &allowance { " · "(allowance.clone()) }
                        if budget.pending > 0 { " · "(budget.pending)" waiting" }
                    </span>
                    if let Some(until) = &budget.parked_until {
                        <p class="mt-1 mb-0">
                            "Held until "(until.clone())
                            if let Some(why) = &budget.parked_reason { " — "(why.clone()) }
                            "."
                        </p>
                    }
                }
            </div>
        </div>
    }
}

#[component]
async fn dsn_card(#[into] dsn: String, project: Project) -> Result {
    // The one snippet that turns a fresh project into a wired service.
    let snippet = format!(
        "import sentry_sdk\n\nsentry_sdk.init(\n    dsn=\"{dsn}\",\n    \
         send_default_pii=False,\n)"
    );
    view! {
        <div class="Box mb-3">
            <div class="Box-header d-flex flex-items-center gap-2">
                <strong>"DSN"</strong>
                if let Some(repo) = &project.repo {
                    <a class="ml-auto Link--primary text-small" href=(format!("/{repo}"))>(repo.clone())</a>
                }
            </div>
            <div class="Box-body">
                <pre class="text-small nashcode-code">(dsn)</pre>
                <p class="color-fg-muted text-small mt-2 mb-1">"Python:"</p>
                <pre class="text-small nashcode-code">(snippet)</pre>
            </div>
        </div>
    }
}

#[component]
async fn issue_row(#[into] project: String, issue: Issue) -> Result {
    view! {
        <div class="Box-row d-flex flex-items-center gap-2">
            <a class="Link--primary" href=(format!("/bugs/{project}/issues/{}", issue.id))>
                (issue.title.clone())
            </a>
            if issue.regression {
                <span class="Label Label--danger">"regression"</span>
            }
            if issue.state != state::UNRESOLVED {
                <span class="Label">(issue.state.clone())</span>
            }
            <span class="ml-auto text-small color-fg-muted">
                (issue.events)" events · "(issue.last_seen.clone())
            </span>
        </div>
    }
}

/// `GET /bugs/{project}/issues/{issue}` — one issue, rendered from the bucket object.
#[route(GET "/bugs/{project_name}/issues/{issue_id}")]
async fn issue_page(cx: &Cx) -> Result<Response> {
    on(cx)?;
    let name = path_param::<ProjectName>(cx).to_owned();
    let id = *path_param::<IssueId>(cx)?;
    let Some(project) = app(cx).bugs.project(&name)? else {
        return Err(not_found().into());
    };
    let Some(issue) = app(cx).bugs.issue(project.id, id)? else {
        return Err(not_found().into());
    };
    let latest = app(cx).bugs.latest_event(issue.id)?;

    // The index says where the payload is; the payload itself is the truth.
    let payload = match &latest {
        Some(row) => match app(cx).bugs.payload(&row.object_key).await {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).ok(),
            Err(error) => {
                tracing::warn!(%error, key = row.object_key, "bugs: cannot read the event object");
                None
            }
        },
        None => None,
    };

    if wants_json(cx) {
        let body = serde_json::json!({
            "issue": issue,
            "event": latest,
            "payload": payload,
        });
        return Ok(json_response(StatusCode::OK, body.to_string()));
    }

    let detail = payload.as_ref().map(Detail::of).unwrap_or_default();
    // Frames get the same treatment as log rows: a path that does not resolve in the
    // declared repo is text, not a link. A file renamed since the release that threw
    // is the case a syntactic check alone gets wrong — which is also why the source is
    // read at the release the event names rather than at the tip.
    let release = payload.as_ref().and_then(context::release_of);
    let origins = frame_origins(cx, &project, &detail.frames, release.as_deref()).await;
    // Paired before the view, so the markup only reads.
    let frames: Vec<(Frame, Origin)> = detail
        .frames
        .iter()
        .enumerate()
        .map(|(index, frame)| (frame.clone(), origins.get(&index).cloned().unwrap_or_default()))
        .collect();
    let page = view! { cx =>
        shell(title: format!("{} · bugs", issue.title),
            <div class="d-flex flex-items-center gap-2 mb-2">
                <a class="Link--primary text-small" href=(format!("/bugs/{name}"))>(name.clone())</a>
                <span class="color-fg-muted">"/"</span>
                <span class="text-small color-fg-muted">"issue "(issue.id)</span>
            </div>
            <h3 class="mb-1">(issue.title.clone())</h3>
            <p class="color-fg-muted text-small">
                (issue.events)" events · first seen "(issue.first_seen.clone())
                " · last seen "(issue.last_seen.clone())
                " · "(issue.state.clone())
                if issue.regression { " · regression" }
            </p>
            <div class="d-flex gap-2 mb-3">
                for (label, value) in [("Resolve", state::RESOLVED), ("Mute", state::MUTED), ("Reopen", state::UNRESOLVED)] {
                    <form key=(label) method="post"
                          action=(format!("/bugs/{name}/issues/{}/state", issue.id))>
                        <input type="hidden" name="state" value=(value)>
                        <button class="btn btn-sm" type="submit" disabled=(issue.state == value)>(label)</button>
                    </form>
                }
            </div>
            <div class="Box mb-3">
                <div class="Box-header"><strong>"Grouping"</strong></div>
                <div class="Box-body">
                    <pre class="text-small nashcode-code">(issue.grouping_key.clone())</pre>
                    <p class="text-small color-fg-muted mb-0">"mechanism "(issue.mechanism.clone())</p>
                </div>
            </div>
            if let Some(value) = &detail.value {
                <div class="Box mb-3">
                    <div class="Box-header"><strong>(detail.ty.clone().unwrap_or_else(|| "Message".to_owned()))</strong></div>
                    <div class="Box-body"><pre class="text-small nashcode-code">(value.clone())</pre></div>
                </div>
            }
            if !frames.is_empty() {
                <div class="Box mb-3">
                    <div class="Box-header"><strong>"Stack"</strong></div>
                    for (index, (frame, origin)) in frames.iter().enumerate() {
                        <div key=(index) class="Box-row text-small">
                            <div class="nashcode-code">
                                if let Some(href) = origin.href.clone() {
                                    <a class="Link--primary" href=(href)>(frame.location.clone())</a>
                                } else {
                                    (frame.location.clone())
                                }
                                if origin.inferred {
                                    <span class="Label ml-1" title="matched by path suffix">"matched"</span>
                                }
                                if let Some(function) = &frame.function {
                                    " in "(function.clone())
                                }
                            </div>
                            if let Some(snippet) = origin.snippet.clone() {
                                source_lines(snippet: snippet)
                            } else if let Some(context) = &frame.context {
                                <pre class="text-small nashcode-code mt-1 mb-0">(context.clone())</pre>
                            }
                        </div>
                    }
                </div>
            }
            if !detail.tags.is_empty() {
                <div class="Box">
                    <div class="Box-header"><strong>"Tags"</strong></div>
                    for (index, (key, value)) in detail.tags.iter().enumerate() {
                        <div key=(index) class="Box-row d-flex gap-2 text-small">
                            <span class="color-fg-muted">(key.clone())</span>
                            <span class="ml-auto">(value.clone())</span>
                        </div>
                    }
                </div>
            }
        )
    }?;
    page.into_response(cx)
}

/// The handful of fields the detail page shows, lifted out of the raw event.
#[derive(Debug, Default)]
struct Detail {
    ty: Option<String>,
    value: Option<String>,
    frames: Vec<Frame>,
    tags: Vec<(String, String)>,
}

/// One stack frame, split so the file part can become a link into the code browser.
#[derive(Debug, Default, Clone)]
struct Frame {
    /// `path/to/file.py:41`, or whatever of it the SDK sent.
    location: String,
    path: Option<String>,
    line: Option<i64>,
    function: Option<String>,
    context: Option<String>,
    in_app: bool,
}

/// Where one reported path turned out to live, and what is written there.
///
/// Every field is optional on purpose. A path the mirror cannot answer for degrades to
/// the plain string it arrived as, never to an error and never to a dead link.
#[derive(Debug, Clone, Default)]
struct Origin {
    /// The blob URL, anchored at the line.
    href: Option<String>,
    /// True when the path was found by matching a suffix rather than as written — the
    /// container-prefix case. Marked, because the link is then an inference.
    inferred: bool,
    /// The three lines either side.
    snippet: Option<context::Snippet>,
}

impl Origin {
    fn is_empty(&self) -> bool {
        self.href.is_none() && self.snippet.is_none()
    }
}

/// How many distinct `file:line` sites one page reads source for.
///
/// Reading is one `git show` per site, and a page of a hundred log rows out of one hot
/// loop is three sites, not a hundred — the cache below is what usually decides this.
/// The cap is for the page that really does name a hundred different files: the links
/// are all still there, and the first two dozen carry their source.
const SNIPPETS_PER_PAGE: usize = 24;

/// Resolve reported paths against the declared repo and read the source around them.
///
/// One catalog per page: the tree listing is read once per release seen, and each
/// distinct `file:line` is read at most once however many rows name it.
struct Capture {
    repo: String,
    catalog: context::Catalog,
    seen: std::collections::HashMap<(String, String, i64), Option<context::Snippet>>,
    budget: usize,
}

impl Capture {
    /// `None` when the project declares no repo, or one this viewer does not serve.
    fn open(cx: &Cx, project: &Project) -> Option<Self> {
        let repo = project.repo.as_deref()?;
        if !app(cx).config.knows_repo(repo) {
            return None;
        }
        Some(Self {
            repo: repo.to_owned(),
            catalog: context::Catalog::new(app(cx).mirrors.repo(repo)),
            seen: std::collections::HashMap::new(),
            budget: SNIPPETS_PER_PAGE,
        })
    }

    async fn origin(&mut self, reported: &str, line: Option<i64>, release: Option<&str>) -> Origin {
        let Some(source) = self.catalog.source(release).await else { return Origin::default() };
        let resolved = source.resolve(reported);
        let Some(path) = resolved.path() else { return Origin::default() };
        let origin = Origin {
            href: Some(blob_anchor(&self.repo, path, line)),
            inferred: resolved.inferred(),
            snippet: None,
        };
        let Some(line) = line.filter(|line| *line > 0) else { return origin };

        let key = (source.rev.clone(), path.to_owned(), line);
        if let Some(cached) = self.seen.get(&key) {
            return Origin { snippet: cached.clone(), ..origin };
        }
        if self.budget == 0 {
            return origin;
        }
        self.budget -= 1;
        let snippet = source.snippet(path, line).await;
        self.seen.insert(key, snippet.clone());
        Origin { snippet, ..origin }
    }
}

/// The origin of each in-app frame, by its position in the rendered list.
///
/// Only in-app frames are candidates: a frame out of site-packages names a file that
/// is not in this repo even when a file by that name happens to be.
async fn frame_origins(
    cx: &Cx,
    project: &Project,
    frames: &[Frame],
    release: Option<&str>,
) -> std::collections::HashMap<usize, Origin> {
    let mut origins = std::collections::HashMap::new();
    let Some(mut capture) = Capture::open(cx, project) else { return origins };
    for (index, frame) in frames.iter().enumerate() {
        if !frame.in_app {
            continue;
        }
        let Some(path) = frame.path.as_deref() else { continue };
        let origin = capture.origin(path, frame.line, release).await;
        if !origin.is_empty() {
            origins.insert(index, origin);
        }
    }
    origins
}

impl Detail {
    fn of(event: &serde_json::Value) -> Self {
        let mut detail = Self::default();
        // One helper, shared with grouping: this page used to read `exception.values`
        // only, so an event sent as a bare `"exception": [...]` grouped correctly and
        // then rendered without the exception it had grouped on.
        if let Some(exception) = group::last_exception(event) {
            detail.ty =
                exception.get("type").and_then(serde_json::Value::as_str).map(str::to_owned);
            detail.value =
                exception.get("value").and_then(serde_json::Value::as_str).map(str::to_owned);
            if let Some(frames) = group::frames(exception) {
                // Innermost first, which is the order a person reads a traceback in.
                detail.frames = frames.iter().rev().map(render_frame).collect();
            }
        }
        if detail.value.is_none() {
            detail.value = event
                .get("logentry")
                .and_then(|entry| entry.get("formatted"))
                .or_else(|| event.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
        }
        if let Some(tags) = event.get("tags").and_then(serde_json::Value::as_object) {
            detail.tags = tags
                .iter()
                .map(|(key, value)| {
                    let rendered = match value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    (key.clone(), rendered)
                })
                .collect();
        }
        detail
    }
}

fn render_frame(frame: &serde_json::Value) -> Frame {
    let text = |key: &str| {
        frame
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let path = text("filename");
    let line = frame.get("lineno").and_then(serde_json::Value::as_i64);
    let location = match (&path, line) {
        (Some(path), Some(line)) => format!("{path}:{line}"),
        (Some(path), None) => path.clone(),
        (None, _) => text("module").unwrap_or_else(|| "<unknown>".to_owned()),
    };
    Frame {
        location,
        path,
        line,
        function: text("function"),
        context: text("context_line"),
        in_app: frame.get("in_app").and_then(serde_json::Value::as_bool).unwrap_or(false),
    }
}

#[derive(Debug, Default, Deserialize)]
struct StateIn {
    state: Option<String>,
}

/// `POST /bugs/{project}/issues/{issue}/state {state}` — resolve, mute, or reopen.
/// The Tailscale headers say who did it, the same as every other mutation here.
#[route(POST "/bugs/{project_name}/issues/{issue_id}/state")]
async fn set_state(cx: &Cx, body: request::Bytes) -> Result<Response> {
    on(cx)?;
    let name = path_param::<ProjectName>(cx).to_owned();
    let id = *path_param::<IssueId>(cx)?;
    let form = is_form(cx);
    let input: StateIn = if form {
        serde_urlencoded::from_bytes(&body).map_err(|e| bad_request(e.to_string()))?
    } else {
        serde_json::from_slice(&body).map_err(|e| bad_request(e.to_string()))?
    };
    let Some(wanted) = input.state.filter(|value| state::known(value)) else {
        return Err(bad_request("state is unresolved, resolved or muted").into());
    };
    let Some(project) = app(cx).bugs.project(&name)? else {
        return Err(not_found().into());
    };
    let Some(issue) = app(cx).bugs.set_state(project.id, id, &wanted, &actor(cx).login)? else {
        return Err(not_found().into());
    };

    if form {
        return see_other(&format!("/bugs/{name}/issues/{}", issue.id));
    }
    Ok(json_response(StatusCode::OK, serde_json::to_string(&issue)?))
}

// ---- logs page ---------------------------------------------------------------------

/// How many log rows one page shows.
const LOGS_PER_PAGE: i64 = 100;

#[query_params(error = bad_request)]
struct LogsQuery {
    /// The search box, verbatim: free text plus any `file:` token.
    q: Option<String>,
    /// One severity band. Absent means all of them.
    level: Option<String>,
    /// Zero-based page, newest first.
    page: Option<i64>,
}

/// `GET /bugs/{project}/logs` — search the hot window.
#[route(GET "/bugs/{project_name}/logs")]
async fn logs_page(cx: &Cx) -> Result<Response> {
    on(cx)?;
    let name = path_param::<ProjectName>(cx).to_owned();
    let Some(project) = app(cx).bugs.project(&name)? else {
        return Err(not_found().into());
    };
    let params = query_params::<LogsQuery>(cx)?;
    let level = match params.level.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) if !logs::SEVERITIES.iter().any(|(name, _)| *name == value) => {
            return Err(bad_request("level is trace, debug, info, warn, error or fatal").into());
        }
        other => other.map(str::to_owned),
    };
    let raw = params.q.clone().unwrap_or_default();
    let (text, file) = logs::parse_search(&raw);
    let page = params.page.unwrap_or(0).max(0);

    let query = logs::Query {
        text,
        level: level.clone(),
        file,
        limit: LOGS_PER_PAGE,
        offset: page.saturating_mul(LOGS_PER_PAGE),
    };
    // One row over the page tells us whether there is another page without counting
    // the table, which on a hot window of millions is the difference that matters.
    let mut rows = app(cx).bugs.logs(project.id, &query)?;
    let more = rows.len() as i64 > LOGS_PER_PAGE;
    rows.truncate(LOGS_PER_PAGE as usize);

    let origins = code_origins(cx, &project, &rows).await;

    if wants_json(cx) {
        let body = serde_json::json!({
            "project": project,
            "logs": rows,
            "query": { "q": raw, "level": level, "page": page },
            "more": more,
        });
        return Ok(json_response(StatusCode::OK, body.to_string()));
    }

    // Pair each row with its origin before the view, so the markup only reads.
    let rendered: Vec<(crate::bugs::LogRow, Origin)> = rows
        .into_iter()
        .map(|row| {
            let origin = origins.get(&row.id).cloned().unwrap_or_default();
            (row, origin)
        })
        .collect();

    let page_view = view! { cx =>
        shell(title: format!("{name} · logs"),
            <div class="d-flex flex-items-center gap-2 mb-2">
                <a class="Link--primary text-small" href=(format!("/bugs/{name}"))>(name.clone())</a>
                <span class="color-fg-muted">"/"</span>
                <h3 class="mb-0"><i class="ph ph-list-magnifying-glass"></i>" Logs"</h3>
                <a class="ml-auto Link--primary text-small" href="/bugs">"All projects"</a>
            </div>
            <form method="get" class="d-flex gap-2 flex-items-center mb-2">
                <input class="form-control flex-auto" type="search" name="q" value=(raw.clone())
                       placeholder="search the message, or file:path/to/thing.rs">
                <select class="form-select" name="level">
                    <option value="" selected=(level.is_none())>"any level"</option>
                    for (band, _) in logs::SEVERITIES {
                        <option key=(band) value=(band) selected=(level.as_deref() == Some(band))>(band)</option>
                    }
                </select>
                <button class="btn" type="submit">"Search"</button>
            </form>
            if rendered.is_empty() {
                <div class="Box"><div class="Box-body color-fg-muted">
                    "Nothing here. Logs arrive as Sentry "<code>"log"</code>" items on the DSN, or as
                     NDJSON on "<code>(format!("POST /api/{}/logs", project.id))</code>"."
                </div></div>
            } else {
                <div class="Box">
                    for (row, origin) in &rendered {
                        log_row(key: row.id, row: row.clone(), origin: origin.clone())
                    }
                </div>
                <div class="d-flex gap-2 mt-2 flex-items-center">
                    if page > 0 {
                        <a class="btn btn-sm" href=(logs_url(&name, &raw, level.as_deref(), page - 1))>"Newer"</a>
                    }
                    <span class="text-small color-fg-muted">"page "(page + 1)</span>
                    if more {
                        <a class="btn btn-sm" href=(logs_url(&name, &raw, level.as_deref(), page + 1))>"Older"</a>
                    }
                </div>
            }
        )
    }?;
    page_view.into_response(cx)
}

fn logs_url(project: &str, q: &str, level: Option<&str>, page: i64) -> String {
    let mut url = format!("/bugs/{project}/logs?page={page}");
    if !q.is_empty() {
        url.push_str(&format!("&q={}", crate::render::encode_path(q)));
    }
    if let Some(level) = level {
        url.push_str(&format!("&level={level}"));
    }
    url
}

/// The three lines either side of the one that failed.
///
/// The revision is on the label, always. A snippet read at the tip because the mirror
/// did not know the release is *probably* the right lines and might not be, and the
/// only honest way to show it is to say which it is.
#[component]
async fn source_lines(snippet: context::Snippet) -> Result {
    let short: String = snippet.rev.chars().take(8).collect();
    view! {
        <div class="mt-1">
            <pre class="text-small nashcode-code mb-1">
                for (number, text) in snippet.numbered() {
                    <div key=(number) class=(if number == snippet.line { "color-fg-danger" } else { "" })>
                        (format!("{number:>5} "))
                        if number == snippet.line { "▸ " } else { "  " }
                        (text.to_owned())
                    </div>
                }
            </pre>
            <span class="text-small color-fg-muted">
                if snippet.tip_not_release {
                    "tip, not release · "(short)
                } else {
                    "at "(short)
                }
            </span>
        </div>
    }
}

#[component]
async fn log_row(row: crate::bugs::LogRow, origin: Origin) -> Result {
    let location = match (&row.code_file, row.code_line) {
        (Some(file), Some(line)) if line > 0 => Some(format!("{file}:{line}")),
        (Some(file), _) => Some(file.clone()),
        (None, _) => None,
    };
    view! {
        <div class="Box-row text-small">
            <div class="d-flex flex-items-baseline gap-2">
                <span class="color-fg-muted nashcode-code">(row.ts.clone())</span>
                <span class=(level_class(&row.severity_text))>(row.severity_text.clone())</span>
                <span class="flex-auto">(row.message.clone())</span>
                if let Some(location) = location {
                    // A path that does not resolve in the declared repo is plain text.
                    // A dead link is worse than no link.
                    if let Some(href) = origin.href.clone() {
                        <a class="Link--secondary nashcode-code" href=(href)>(location)</a>
                    } else {
                        <span class="color-fg-muted nashcode-code">(location)</span>
                    }
                }
            </div>
            // Folded away: a page of a hundred rows stays a page of a hundred rows,
            // and the one row you are reading opens.
            if let Some(snippet) = origin.snippet.clone() {
                <details>
                    <summary class="text-small color-fg-muted">"source"</summary>
                    source_lines(snippet: snippet)
                </details>
            }
        </div>
    }
}

fn level_class(level: &str) -> &'static str {
    match level {
        "fatal" | "error" => "Label Label--danger",
        "warn" => "Label Label--warning",
        "debug" | "trace" => "Label Label--secondary",
        _ => "Label",
    }
}

/// Where each log row's code origin lives, and the source around it.
///
/// The reported path is whatever the process saw — `/app/src/foo.py` from inside a
/// container, `src/foo.py` from a checkout — so it is resolved against the repo tree
/// rather than trusted, and the release the row names decides which tree.
async fn code_origins(
    cx: &Cx,
    project: &Project,
    rows: &[crate::bugs::LogRow],
) -> std::collections::HashMap<i64, Origin> {
    let mut origins = std::collections::HashMap::new();
    let Some(mut capture) = Capture::open(cx, project) else { return origins };
    for row in rows {
        let Some(path) = row.code_file.as_deref() else { continue };
        let release = context::release_of(&row.attributes);
        let origin = capture.origin(path, row.code_line, release.as_deref()).await;
        if !origin.is_empty() {
            origins.insert(row.id, origin);
        }
    }
    origins
}

/// The blob URL for a path, anchored at its line when it has a usable one.
fn blob_anchor(repo: &str, path: &str, line: Option<i64>) -> String {
    let url = crate::render::blob_url(repo, path);
    match line {
        Some(line) if line > 0 => format!("{url}#L{line}"),
        _ => url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one answer an overloaded ingest gives. Its shape is the contract: the
    /// status an SDK backs off on, the delay it waits, and the CORS header without
    /// which a browser SDK sees none of it.
    #[test]
    fn the_busy_answer_is_one_an_sdk_can_obey() {
        let response = busy();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "5");
        assert_eq!(response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*");
        assert_eq!(response.headers().get("x-sentry-error").unwrap(), "the digest queue is full");
    }
}
