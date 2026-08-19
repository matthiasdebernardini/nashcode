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

use crate::bugs::{Issue, Project, envelope, group, ingest, logs, state};
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
        return accept_logs(cx, body).await;
    }
    if !is_envelope_path(cx) {
        return Err(not_found().into());
    }
    accept_envelope(cx, body).await
}

#[route(POST "/api/{project_id}/envelope")]
async fn ingest_bare(cx: &Cx, body: Body) -> Result<Response> {
    accept_envelope(cx, body).await
}

/// `POST /api/{project_id}/logs` — the second log door, for everything that is not a
/// Sentry SDK.
#[route(POST "/api/{project_id}/logs")]
async fn ingest_logs_bare(cx: &Cx, body: Body) -> Result<Response> {
    accept_logs(cx, body).await
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
    if wants_json(cx) {
        return Ok(json_response(StatusCode::OK, serde_json::to_string(&projects)?));
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
    // is the case a syntactic check alone gets wrong.
    let frame_links = frame_links(cx, &project, &detail.frames).await;
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
            if !detail.frames.is_empty() {
                <div class="Box mb-3">
                    <div class="Box-header"><strong>"Stack"</strong></div>
                    for (index, frame) in detail.frames.iter().enumerate() {
                        <div key=(index) class="Box-row text-small nashcode-code">
                            if let Some(href) = frame_links.get(&index).cloned() {
                                <a class="Link--primary" href=(href)>(frame.location.clone())</a>
                            } else {
                                (frame.location.clone())
                            }
                            if let Some(function) = &frame.function {
                                " in "(function.clone())
                            }
                            if let Some(context) = &frame.context {
                                "\n    "(context.clone())
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
#[derive(Debug, Default)]
struct Frame {
    /// `path/to/file.py:41`, or whatever of it the SDK sent.
    location: String,
    path: Option<String>,
    line: Option<i64>,
    function: Option<String>,
    context: Option<String>,
    in_app: bool,
}

/// The blob URL for each in-app frame whose file really exists in the declared repo,
/// by its position in the rendered list.
///
/// Only in-app frames are candidates: a frame out of site-packages names a file that
/// is not in this repo even when a file by that name happens to be.
async fn frame_links(
    cx: &Cx,
    project: &Project,
    frames: &[Frame],
) -> std::collections::HashMap<usize, String> {
    let mut links = std::collections::HashMap::new();
    let Some(repo) = project.repo.as_deref() else { return links };
    if !app(cx).config.knows_repo(repo) {
        return links;
    }
    let wanted: Vec<(usize, &str, Option<i64>)> = frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame.in_app)
        .filter_map(|(index, frame)| Some((index, frame.path.as_deref()?, frame.line)))
        .collect();

    let paths: Vec<&str> = wanted.iter().map(|(_, path, _)| *path).collect();
    let resolved = resolve_in_repo(cx, repo, &paths).await;
    for (index, path, line) in wanted {
        if resolved.contains(path) {
            links.insert(index, blob_anchor(repo, path, line));
        }
    }
    links
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

    let origins = code_links(cx, &project, &rows).await;

    if wants_json(cx) {
        let body = serde_json::json!({
            "project": project,
            "logs": rows,
            "query": { "q": raw, "level": level, "page": page },
            "more": more,
        });
        return Ok(json_response(StatusCode::OK, body.to_string()));
    }

    // Pair each row with its link before the view, so the markup only reads.
    let rendered: Vec<(crate::bugs::LogRow, Option<String>)> =
        rows.into_iter().map(|row| { let link = origins.get(&row.id).cloned(); (row, link) }).collect();

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
                    for (row, link) in &rendered {
                        log_row(key: row.id, row: row.clone(), link: link.clone())
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

#[component]
async fn log_row(row: crate::bugs::LogRow, link: Option<String>) -> Result {
    let origin = match (&row.code_file, row.code_line) {
        (Some(file), Some(line)) if line > 0 => Some(format!("{file}:{line}")),
        (Some(file), _) => Some(file.clone()),
        (None, _) => None,
    };
    view! {
        <div class="Box-row d-flex flex-items-baseline gap-2 text-small">
            <span class="color-fg-muted nashcode-code">(row.ts.clone())</span>
            <span class=(level_class(&row.severity_text))>(row.severity_text.clone())</span>
            <span class="flex-auto">(row.message.clone())</span>
            if let Some(origin) = origin {
                // A path that does not resolve in the declared repo is plain text.
                // A dead link is worse than no link.
                if let Some(href) = link {
                    <a class="Link--secondary nashcode-code" href=(href)>(origin)</a>
                } else {
                    <span class="color-fg-muted nashcode-code">(origin)</span>
                }
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

/// The blob URL for each row whose code origin actually exists in the declared repo.
///
/// Resolution is the code browser's own question, asked the same way: list the
/// parent directory at the default tip and look for the name. One listing per
/// distinct directory on the page, so a hundred rows out of three files cost three
/// `ls-tree` calls, not a hundred.
async fn code_links(
    cx: &Cx,
    project: &Project,
    rows: &[crate::bugs::LogRow],
) -> std::collections::HashMap<i64, String> {
    let mut links = std::collections::HashMap::new();
    let Some(repo) = project.repo.as_deref() else { return links };
    if !app(cx).config.knows_repo(repo) {
        return links;
    }
    let wanted: Vec<(i64, String, Option<i64>)> = rows
        .iter()
        .filter_map(|row| {
            let path = row.code_file.as_deref()?;
            linkable(path).then(|| (row.id, path.to_owned(), row.code_line))
        })
        .collect();

    let paths: Vec<&str> = wanted.iter().map(|(_, path, _)| path.as_str()).collect();
    let resolved = resolve_in_repo(cx, repo, &paths).await;
    for (id, path, line) in &wanted {
        if resolved.contains(path.as_str()) {
            links.insert(*id, blob_anchor(repo, path, *line));
        }
    }
    links
}

/// Which of `paths` actually name a file in `repo` at its default tip.
///
/// This is the code browser's own question, asked the same way it asks it: list the
/// parent directory and look for the name, rejecting a directory. One `ls-tree` per
/// distinct directory, so a page of a hundred rows out of three files costs three
/// calls rather than a hundred.
///
/// Everything here exists to keep a dead link off the page. SPEC is explicit that an
/// unresolvable path renders as text, and a file that has since been renamed is
/// exactly the case a purely syntactic check would get wrong.
async fn resolve_in_repo(
    cx: &Cx,
    repo: &str,
    paths: &[&str],
) -> std::collections::HashSet<String> {
    let mut resolved = std::collections::HashSet::new();
    if paths.is_empty() {
        return resolved;
    }
    let mirror = app(cx).mirrors.repo(repo);
    let Ok(branch) = mirror.default_branch().await else { return resolved };
    let Ok(tip) = mirror.tip(&branch).await else { return resolved };

    let mut listings: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for path in paths {
        if !linkable(path) || resolved.contains(*path) {
            continue;
        }
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        if !listings.contains_key(dir) {
            let entries = mirror
                .ls_tree(&tip, dir)
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| entry.kind != crate::git::EntryKind::Dir)
                .map(|entry| entry.name)
                .collect();
            listings.insert(dir.to_owned(), entries);
        }
        if listings[dir].iter().any(|name| name == file) {
            resolved.insert((*path).to_owned());
        }
    }
    resolved
}

/// The blob URL for a path, anchored at its line when it has a usable one.
fn blob_anchor(repo: &str, path: &str, line: Option<i64>) -> String {
    let url = crate::render::blob_url(repo, path);
    match line {
        Some(line) if line > 0 => format!("{url}#L{line}"),
        _ => url,
    }
}

/// A path that could name a file inside a repo. An absolute one comes from the
/// machine the process ran on, not from the repo, and `..` is nobody's source file.
fn linkable(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('.')
        && !path.split('/').any(|segment| segment == ".." || segment.is_empty())
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
