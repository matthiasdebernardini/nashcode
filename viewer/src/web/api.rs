//! JSON and raw endpoints: the public comment API, raw file serving, board moves,
//! branch actions (merge / restack / rerun), and the brain.

use std::collections::BTreeMap;

use serde::Deserialize;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{
    HeaderValue, StatusCode, content::Json, error::bad_request, header, path_param,
    query_params, request, response::Response, route,
};

use crate::db::{CommentFilter, NewComment, normalize_timestamp};
use crate::ops::OpError;
use crate::web::pages::split_action;
use crate::web::{actor, app, repo_ctx, see_other};

path_param!(repo);
path_param!(comment_id: i64, error = bad_request);
path_param!(*rest);

fn json_error(status: StatusCode, message: &str) -> Result<Response> {
    let body = serde_json::json!({ "error": message }).to_string();
    let mut response = Response::new(topcoat::router::Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}

fn op_error(error: OpError) -> Result<Response> {
    match &error {
        OpError::NotFound(_) => json_error(StatusCode::NOT_FOUND, &error.to_string()),
        OpError::Blocked(_) | OpError::Conflict { .. } => {
            json_error(StatusCode::CONFLICT, &error.to_string())
        }
        OpError::Git(_) => json_error(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

/// Is this a browser form post (redirect back) rather than an API call (JSON out)?
fn is_form(cx: &Cx) -> bool {
    request::content_type(cx)
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"))
}

fn back_url(cx: &Cx, fallback: &str) -> String {
    request::headers(cx)
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}

// ---- comments --------------------------------------------------------------------

#[topcoat::router::query_params(error = bad_request)]
struct CommentsQuery {
    branch: Option<String>,
    file: Option<String>,
    since: Option<String>,
}

/// `GET /{repo}/comments?branch=&file=&since=` — the poller's read side. Rows come
/// back ordered by `(created_at, id)`; `since` is RFC3339 and strictly exclusive.
#[route(GET "/{repo}/comments")]
async fn comments_get(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let query = query_params::<CommentsQuery>(cx)?;
    let since = match &query.since {
        None => None,
        Some(raw) => match normalize_timestamp(raw) {
            Some(normalized) => Some(normalized),
            None => return Err(bad_request("since must be an RFC3339 timestamp").into()),
        },
    };
    let filter = CommentFilter {
        repo: name,
        branch: query.branch.clone(),
        file: query.file.clone(),
        since,
    };
    let comments = app(cx).db.comments(&filter)?;
    let body = serde_json::to_string(&comments)?;
    let mut response = Response::new(topcoat::router::Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}

/// The POST body, from JSON or from the UI composer's form.
#[derive(Debug, Default, Deserialize)]
struct CommentIn {
    branch: Option<String>,
    file: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    line: Option<i64>,
    body: Option<String>,
    author: Option<String>,
}

/// Accept `12`, `"12"`, and `""` (an empty form field) for the line number.
fn lenient_i64<'de, D>(deserializer: D) -> std::result::Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(i64),
        Text(String),
        None,
    }
    Ok(match Raw::deserialize(deserializer).unwrap_or(Raw::None) {
        Raw::Number(n) => Some(n),
        Raw::Text(text) => text.trim().parse().ok(),
        Raw::None => None,
    })
}

/// `POST /{repo}/comments` — the public write side. `file`/`line` optional (omit both
/// for a PR-level comment); `author` falls back to `Tailscale-User-Login`, then
/// `local`. Responds 201 with the stored comment.
#[route(POST "/{repo}/comments")]
async fn comments_post(cx: &Cx, body: topcoat::router::request::Bytes) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    let form = is_form(cx);

    let input: CommentIn = if form {
        serde_urlencoded::from_bytes(&body).map_err(|e| bad_request(e.to_string()))?
    } else {
        serde_json::from_slice(&body).map_err(|e| bad_request(e.to_string()))?
    };

    let Some(branch) = input.branch.clone().filter(|b| !b.trim().is_empty()) else {
        return Err(bad_request("branch is required").into());
    };
    let Some(text) = input.body.clone().filter(|b| !b.trim().is_empty()) else {
        return Err(bad_request("body is required").into());
    };
    if input.line.is_some() && input.file.is_none() {
        return Err(bad_request("line needs a file").into());
    }
    if input.line.is_some_and(|line| line < 1) {
        return Err(bad_request("line must be positive").into());
    }

    // The anchor commit is the branch tip at post time. Unknown branch: if the mirror
    // is unavailable we still take the comment (degrade, don't lose feedback) with an
    // empty commit; on a live mirror an unknown branch is a client error.
    let commit = match app(cx).mirrors.repo(&name).tip(&branch).await {
        Ok(tip) => tip,
        Err(_) if !ctx.status.available => String::new(),
        Err(_) => return Err(bad_request(format!("unknown branch {branch}")).into()),
    };

    let author = input
        .author
        .clone()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| actor(cx).login);

    let stored = app(cx)
        .db
        .add_comment(NewComment {
            repo: name.clone(),
            branch,
            file: input.file.clone().filter(|f| !f.trim().is_empty()),
            line: input.line,
            commit,
            author,
            body: text,
        })
        ?;

    if form {
        return see_other(&back_url(cx, &format!("/{name}")));
    }
    let body = serde_json::to_string(&stored)?;
    let mut response = Response::new(topcoat::router::Body::from(body));
    *response.status_mut() = StatusCode::CREATED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}

/// Delete your own comment. The UI posts here; there is no JSON delete surface.
#[route(POST "/{repo}/comments/{comment_id}/delete")]
async fn comment_delete(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let id = *path_param::<CommentId>(cx).map_err(|_| bad_request("bad comment id"))?;
    let who = actor(cx);
    let deleted = app(cx)
        .db
        .delete_comment(&name, id, &who.login)
        ?;
    if !deleted {
        return json_error(StatusCode::FORBIDDEN, "only the author can delete a comment");
    }
    see_other(&back_url(cx, &format!("/{name}")))
}

// ---- raw files -------------------------------------------------------------------

/// `GET /{repo}/raw/{branch}/{*path}` — the file bytes, verbatim. The branch part is
/// matched greedily against real branch names so branches containing `/` work.
#[route(GET "/{repo}/raw/{*rest}")]
async fn raw_file(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return json_error(StatusCode::NOT_FOUND, "no mirror for this repo yet");
    }
    let segments: Vec<String> = path_param::<Rest>(cx).map(str::to_owned).collect();
    if segments.len() < 2 {
        return Err(bad_request("expected /{repo}/raw/{branch}/{path}").into());
    }
    let repo = app(cx).mirrors.repo(&name);
    let branches = repo.branches().await.unwrap_or_default();

    // Longest branch-name prefix wins; fall back to one segment (a tag or commit id).
    let mut split_at = 1;
    for i in (1..segments.len()).rev() {
        let candidate = segments[..i].join("/");
        if branches.contains(&candidate) {
            split_at = i;
            break;
        }
    }
    let rev = segments[..split_at].join("/");
    let path = segments[split_at..].join("/");
    if path.is_empty() {
        return Err(bad_request("missing file path").into());
    }

    let Some(bytes) = repo.show_file(&rev, &path).await? else {
        return Err(topcoat::router::error::not_found().into());
    };
    let mut response = Response::new(topcoat::router::Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Ok(response)
}

// ---- board -----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MoveIn {
    file: String,
    status: String,
}

/// `POST /{repo}/board/move {file, status}` — rewrite only the front-matter status,
/// commit to the default branch as the Tailscale user, push, refetch. The push must
/// succeed before this reports success.
#[route(POST "/{repo}/board/move")]
async fn board_move(cx: &Cx, Json(input): Json<MoveIn>) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return json_error(StatusCode::BAD_GATEWAY, "no mirror for this repo yet");
    }
    if !input.file.starts_with("tasks/") || input.file.contains("..") {
        return Err(bad_request("file must live under tasks/").into());
    }
    let status = input.status.trim().to_lowercase();
    let ok_status = !status.is_empty()
        && status.len() <= 40
        && status.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok_status || status == crate::docs::NEEDS_ATTENTION {
        return Err(bad_request("not a valid status").into());
    }

    let repo = app(cx).mirrors.repo(&name);
    let default_branch = repo.default_branch().await?;
    let tip = repo.tip(&default_branch).await?;
    let Some(bytes) = repo.show_file(&tip, &input.file).await? else {
        return Err(topcoat::router::error::not_found().into());
    };
    let source = String::from_utf8_lossy(&bytes).into_owned();
    let Some(rewritten) = crate::docs::rewrite_status(&source, &status) else {
        return Err(bad_request("card has no front matter to rewrite").into());
    };

    let who = actor(cx);
    let message = format!("Move {} to {status}", input.file);
    match app(cx).ops.commit_file(&name, &input.file, &rewritten, &message, &who).await {
        Ok(new_tip) => {
            let body = serde_json::json!({ "ok": true, "commit": new_tip }).to_string();
            let mut response = Response::new(topcoat::router::Body::from(body));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            Ok(response)
        }
        Err(error) => op_error(error),
    }
}

// ---- transcripts -----------------------------------------------------------------

/// `POST /{repo}/transcripts` — file a finished meeting transcript as
/// `transcripts/YYYY/MM/<id>.md` on the default branch, committed as the Tailscale
/// user and pushed. The id is the UTC start minute plus a slug of the title; a name
/// already taken at the default-branch tip gets a `-2`, `-3`, … suffix instead of
/// overwriting the earlier meeting. Responds 201 with the id, path, and commit.
#[route(POST "/{repo}/transcripts")]
async fn transcript_post(
    cx: &Cx,
    Json(payload): Json<crate::transcripts::TranscriptPayload>,
) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return json_error(StatusCode::BAD_GATEWAY, "no mirror for this repo yet");
    }
    if let Err(message) = payload.validate() {
        return Err(bad_request(message).into());
    }

    let repo = app(cx).mirrors.repo(&name);
    let default_branch = repo.default_branch().await?;
    let tip = repo.tip(&default_branch).await?;
    let mut chosen = None;
    for n in 1..=99 {
        let (id, path) = crate::transcripts::candidate(&payload, n);
        if repo.show_file(&tip, &path).await?.is_none() {
            chosen = Some((id, path));
            break;
        }
    }
    let Some((id, path)) = chosen else {
        return json_error(StatusCode::CONFLICT, "too many transcripts share this id");
    };

    let markdown = crate::transcripts::render_markdown(&id, &payload);
    let who = actor(cx);
    let message = format!("meeting: {id}");
    match app(cx).ops.commit_file(&name, &path, &markdown, &message, &who).await {
        Ok(commit) => {
            let body =
                serde_json::json!({ "ok": true, "id": id, "path": path, "commit": commit })
                    .to_string();
            let mut response = Response::new(topcoat::router::Body::from(body));
            *response.status_mut() = StatusCode::CREATED;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            Ok(response)
        }
        Err(error) => op_error(error),
    }
}

// ---- branch actions --------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct MergeForm {
    #[serde(default)]
    allow_red: Option<String>,
    #[serde(default)]
    delete_branch: Option<String>,
}

fn truthy(field: &Option<String>) -> bool {
    matches!(field.as_deref(), Some("1") | Some("true") | Some("on"))
}

/// `POST /{repo}/{branch}/(merge|restack|ci/rerun|delete)`.
///
/// Form posts (the UI's buttons) redirect back with `?error=` on failure; API calls
/// get JSON and a status code (404 / 409 / 502).
#[route(POST "/{repo}/{*rest}")]
async fn branch_action(cx: &Cx, body: topcoat::router::request::Bytes) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let rest: String = path_param::<Rest>(cx).collect::<Vec<_>>().join("/");
    let (branch, action) = split_action(&rest);
    let branch = branch.to_owned();
    let who = actor(cx);
    let form = is_form(cx);

    let respond = |result: std::result::Result<String, OpError>| -> Result<Response> {
        match result {
            Ok(to) => {
                if form {
                    see_other(&to)
                } else {
                    let body = serde_json::json!({ "ok": true }).to_string();
                    let mut response = Response::new(topcoat::router::Body::from(body));
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json; charset=utf-8"),
                    );
                    Ok(response)
                }
            }
            Err(error) => {
                if form {
                    let message: String =
                        serde_urlencoded::to_string([("error", error.to_string())])
                            .unwrap_or_default();
                    see_other(&format!("/{name}/{branch}?{message}"))
                } else {
                    op_error(error)
                }
            }
        }
    };

    match action {
        Some("merge") => {
            let input: MergeForm = if form {
                serde_urlencoded::from_bytes(&body).unwrap_or_default()
            } else {
                serde_json::from_slice(&body).unwrap_or_default()
            };
            let result = app(cx)
                .ops
                .merge(&name, &branch, &who, truthy(&input.allow_red), truthy(&input.delete_branch))
                .await
                .map(|outcome| format!("/{name}/{}", outcome.into));
            respond(result)
        }
        Some("restack") => {
            let result = app(cx)
                .ops
                .restack(&name, &branch, &who)
                .await
                .map(|_| format!("/{name}/{branch}"));
            respond(result)
        }
        Some("ci/rerun") => {
            let repo = app(cx).mirrors.repo(&name);
            let result = match repo.tip(&branch).await {
                Ok(tip) => {
                    app(cx).ci.enqueue(&name, &branch, &tip);
                    Ok(format!("/{name}/{branch}/ci"))
                }
                Err(_) => Err(OpError::NotFound(format!("branch {branch}"))),
            };
            respond(result)
        }
        // The wedge escape. `ci/rerun` asks a new question about the branch's current
        // tip; requeue finishes the run that was already answering for this one.
        // ponytail: requeues the tip's latest run; take a run id in the body when an
        // older run needs rescuing.
        Some("ci/requeue") => {
            let repo = app(cx).mirrors.repo(&name);
            let result = match repo.tip(&branch).await {
                Ok(tip) => match app(cx).db.latest_run(&name, &tip).ok().flatten() {
                    Some(run) if app(cx).ci.requeue(&run) => Ok(format!("/{name}/{branch}/ci")),
                    _ => Err(OpError::NotFound(format!("a CI run for {branch}"))),
                },
                Err(_) => Err(OpError::NotFound(format!("branch {branch}"))),
            };
            respond(result)
        }
        Some("delete") => {
            let result = app(cx)
                .ops
                .delete_branch(&name, &branch, &who)
                .await
                .map(|_| format!("/{name}"));
            respond(result)
        }
        _ => Err(topcoat::router::error::not_found().into()),
    }
}

// ---- code intelligence -------------------------------------------------------------

/// Every code endpoint answers JSON and none of them ever indexes: indexing runs on
/// the CI queue. A repo that has never been indexed answers with an empty list and
/// says so, which is an answer an agent can act on.
fn json_ok(value: serde_json::Value) -> Result<Response> {
    let mut response = Response::new(topcoat::router::Body::from(value.to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}

#[topcoat::router::query_params(error = bad_request)]
struct CodeQuery {
    q: Option<String>,
    symbol: Option<String>,
    limit: Option<String>,
    /// Which revision to search. Defaults to the default branch.
    rev: Option<String>,
    /// Comma-separated diagram labels, for `/code/where`.
    names: Option<String>,
    /// `insensitive` folds case on every layer of `/code/find`. Anything else, or
    /// nothing at all, is an exact match.
    case: Option<String>,
    /// `/code/find` path filters, one entry per line (`%0A`). Newline rather than
    /// comma because a glob may contain a comma — `{a,b}` is one pattern — and
    /// `serde_urlencoded` cannot decode a repeated key into a list.
    types: Option<String>,
    globs: Option<String>,
    paths: Option<String>,
}

/// One filter list: newline-separated, empty entries dropped.
fn lines_of(raw: Option<&String>) -> Vec<String> {
    raw.map(String::as_str)
        .unwrap_or_default()
        .split('\n')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

impl CodeQuery {
    /// The result cap, clamped. A missing or unparseable value is the default rather
    /// than an error: an agent that guesses `limit=all` deserves results, not a 400.
    fn limit(&self) -> usize {
        self.limit
            .as_deref()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(crate::code::DEFAULT_LIMIT)
            .min(crate::code::MAX_LIMIT)
    }

    fn required(&self, field: &str, value: Option<&String>) -> Result<String> {
        match value.map(|v| v.trim()).filter(|v| !v.is_empty()) {
            Some(text) => Ok(text.to_owned()),
            None => Err(bad_request(format!("{field} is required")).into()),
        }
    }
}

/// `GET /{repo}/code/text?q=&rev=&limit=` — full text, straight out of git.
///
/// There is no stored index behind this. `git grep` reads the mirror's packed objects
/// at query time, so the answer is exact and can never be stale.
#[route(GET "/{repo}/code/text")]
async fn code_text(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    let query = query_params::<CodeQuery>(cx)?;
    let text = query.required("q", query.q.as_ref())?;
    if !ctx.status.available {
        return json_ok(serde_json::json!({
            "query": text, "hits": [], "error": "no mirror for this repo yet",
        }));
    }

    let repo = app(cx).mirrors.repo(&name);
    let rev = match query.rev.clone().filter(|r| !r.trim().is_empty()) {
        Some(rev) => rev,
        None => repo.default_branch().await.unwrap_or_else(|_| "HEAD".to_owned()),
    };
    let found = crate::code::grep(&repo, &rev, &text, query.limit())
        .await
        .unwrap_or_default();
    json_ok(serde_json::json!({
        "query": text,
        "rev": rev,
        "hits": found.hits,
        "truncated": found.truncated,
    }))
}

/// `GET /{repo}/code/similar?q=&limit=` — semantic search over the stored chunks.
///
/// The query is embedded with the model the index used, then scored against every
/// chunk by brute force. The model is loaded by the index run, never by this handler:
/// loading downloads hundreds of megabytes, and a request must never wait for that.
#[route(GET "/{repo}/code/similar")]
async fn code_similar(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let query = query_params::<CodeQuery>(cx)?;
    let text = query.required("q", query.q.as_ref())?;

    let Some(embedder) = app(cx).embeddings.ready() else {
        // Not an outage and not a 500: the machinery is simply not loaded. Say why,
        // and say what makes it loaded.
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!(
                "semantic search is unavailable: {}. Text search still works: \
                 GET /{name}/code/text?q=",
                app(cx).embeddings.why_not()
            ),
        );
    };

    let one = vec![text.clone()];
    let embedded = tokio::task::spawn_blocking(move || embedder.embed(&one)).await;
    let vector = match embedded {
        Ok(Ok(mut vectors)) if !vectors.is_empty() => vectors.remove(0),
        Ok(Ok(_)) => return json_error(StatusCode::BAD_GATEWAY, "the model returned no vector"),
        Ok(Err(error)) => return json_error(StatusCode::BAD_GATEWAY, &error.to_string()),
        Err(error) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("the embedding task did not finish: {error}"),
            );
        }
    };

    let found = crate::code::similar(&app(cx).db, &name, vector, query.limit()).await;
    json_ok(serde_json::json!({
        "query": text,
        "scanned": found.scanned,
        "hits": found.hits,
    }))
}

/// `GET /{repo}/code/find?q=&limit=&case=&types=&globs=&paths=` — the fused query,
/// one call instead of four.
///
/// The caller sends what it has and the server routes. An exact symbol match answers
/// definitions first, with the reference and caller counts attached, then that symbol's
/// references; the text pass over the same commit comes next; and a thin text pass buys
/// an embeddings pass labelled `semantic`. Every row says which layer produced it and
/// the answer says which commit it describes, because the index lags the working tree.
///
/// The path filters are the server's job, not the caller's, because they have to run
/// before the row budget: a client that filters a capped answer silently loses the
/// rows the cap already spent on files it did not want.
///
/// Never an error. No mirror, no model, no index, nothing matching — all of them are
/// an empty list with a header saying which nothing it is. This is the endpoint
/// `nashcode grep` runs on, and an agent's search must never fail the session.
#[route(GET "/{repo}/code/find")]
async fn code_find(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let query = query_params::<CodeQuery>(cx)?;
    let text = query.required("q", query.q.as_ref())?;
    let options = crate::code::FindOptions {
        limit: query.limit(),
        ignore_case: query.case.as_deref().map(str::trim) == Some("insensitive"),
        filter: crate::code::PathFilter::new(
            &lines_of(query.types.as_ref()),
            &lines_of(query.globs.as_ref()),
            &lines_of(query.paths.as_ref()),
        ),
    };

    // No mirror refresh here, unlike `/code/text`. Every layer of this answer is read
    // at the *indexed* commit, which is on disk by definition, and a search an agent
    // runs on reflex must not wait on a network fetch it did not ask for.
    let mirror = app(cx).mirrors.repo(&name);
    let mut found = crate::code::find(
        &app(cx).db,
        &name,
        &mirror,
        app(cx).embeddings.ready(),
        &text,
        &options,
    )
    .await;
    if !found.semantic_available {
        let why = app(cx).embeddings.why_not();
        if !why.is_empty() {
            found.semantic_note = Some(why);
        }
    }
    json_ok(serde_json::to_value(found)?)
}

/// `GET /{repo}/code/def?symbol=` — where a name is defined.
#[route(GET "/{repo}/code/def")]
async fn code_def(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let query = query_params::<CodeQuery>(cx)?;
    let symbol = query.required("symbol", query.symbol.as_ref())?;
    let hits = crate::code::definitions(&app(cx).db, &name, &symbol);
    json_ok(graph_answer(cx, &name, &symbol, "definitions", hits, query.limit()))
}

/// `GET /{repo}/code/refs?symbol=` — every use of a name.
#[route(GET "/{repo}/code/refs")]
async fn code_refs(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let query = query_params::<CodeQuery>(cx)?;
    let symbol = query.required("symbol", query.symbol.as_ref())?;
    let hits = crate::code::references(&app(cx).db, &name, &symbol);
    json_ok(graph_answer(cx, &name, &symbol, "references", hits, query.limit()))
}

/// `GET /{repo}/code/callers?symbol=` — the functions that call a name.
#[route(GET "/{repo}/code/callers")]
async fn code_callers(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let query = query_params::<CodeQuery>(cx)?;
    let symbol = query.required("symbol", query.symbol.as_ref())?;
    let hits = crate::code::callers(&app(cx).db, &name, &symbol);
    json_ok(graph_answer(cx, &name, &symbol, "callers", hits, query.limit()))
}

/// `GET /{repo}/code/where?names=a,b,c` — batch-resolve diagram labels to code.
///
/// The architecture tab draws whatever an agent posted; this is what lets a reader
/// check it. One call per page, not one per node: a diagram is thirty boxes and thirty
/// round trips would make the affordance cost more than it is worth.
///
/// Never an error for a label it cannot place. A repo with no index, a label naming a
/// Python module, a label naming nothing at all — all three answer with that label
/// simply absent, and the node stays inert on the page.
#[route(GET "/{repo}/code/where")]
async fn code_where(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let query = query_params::<CodeQuery>(cx)?;

    // Empty segments are typing, not a request: `a,,b` asks about two labels. A list
    // that is *all* empty segments is the same mistake as leaving `names` off
    // altogether, so both spellings get the same answer rather than one 400 and one
    // cheerful empty object.
    let names: Vec<String> = query
        .names
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .collect();
    if names.is_empty() {
        return Err(bad_request("names is required").into());
    }
    if names.len() > crate::code::MAX_WHERE_NAMES {
        return Err(bad_request(format!(
            "at most {} names per call, got {}",
            crate::code::MAX_WHERE_NAMES,
            names.len()
        ))
        .into());
    }

    let found = crate::code::locate(&app(cx).db, &name, names).await;
    json_ok(serde_json::json!({ "names": found }))
}

/// The shared body of the three graph endpoints.
///
/// An empty result from an unindexed repo and an empty result from a symbol that does
/// not exist are different facts, so the answer says which: no index means "index it",
/// an index means "it is not there". Neither is an error — a language the graph cannot
/// parse is expected to fall through to `/code/text`.
fn graph_answer<T: serde::Serialize>(
    cx: &Cx,
    repo: &str,
    symbol: &str,
    field: &str,
    hits: Vec<T>,
    limit: usize,
) -> serde_json::Value {
    let mut value = serde_json::json!({ "symbol": symbol });
    let object = value.as_object_mut().expect("built as an object");
    let empty = hits.is_empty();
    object.insert(
        field.to_owned(),
        serde_json::json!(hits.into_iter().take(limit).collect::<Vec<_>>()),
    );
    if empty {
        // "Never indexed" and "indexed, not there" are different facts and want
        // different next steps. Reporting one hint for both sent a caller looking for
        // a symbol that was never going to be there yet.
        let indexed = app(cx)
            .db
            .code_last_run(repo)
            .ok()
            .flatten()
            .is_some();
        object.insert("indexed".to_owned(), serde_json::json!(indexed));
        object.insert(
            "hint".to_owned(),
            serde_json::json!(if indexed {
                format!(
                    "indexed, but nothing is defined under that name; the graph covers \
                     Rust, Python, and TypeScript — try GET /{repo}/code/text?q={symbol}"
                )
            } else {
                format!(
                    "this repo has never been indexed, so the graph is empty; run \
                     POST /{repo}/code/index (or `nashcode index {repo}`) and ask again"
                )
            }),
        );
    }
    value
}

/// `GET /{repo}/code/graph` — the whole analysis in one call.
///
/// The bulk companion to the per-symbol endpoints: an agent drawing an architecture
/// diagram needs every node and edge at once, and paging through `?symbol=` calls to
/// assemble that would be a request per node. A repo with no index answers with an
/// empty document, never an error.
#[route(GET "/{repo}/code/graph")]
async fn code_graph(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let mut graph = crate::code::graph(&app(cx).db, &name).await;
    if let Some(object) = graph.as_object_mut() {
        object.insert("repo".to_owned(), serde_json::json!(name));
    }
    json_ok(graph)
}

/// `POST /{repo}/code/index` — queue an index run. This is what `nashcode index` calls.
///
/// It queues and returns; it never indexes inline. The run happens on the CI queue
/// behind whatever is already there, which is the only place indexing is allowed to
/// happen at all.
#[route(POST "/{repo}/code/index")]
async fn code_index(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return json_error(StatusCode::BAD_GATEWAY, "no mirror for this repo yet");
    }
    // The run reads the default branch tip when it comes off the queue, which is what
    // a person asking for a rebuild means. A run already queued absorbs this one.
    app(cx).ci.enqueue_index(&name);
    json_ok(serde_json::json!({ "ok": true, "repo": name, "queued": true }))
}

/// `GET /{repo}/code` — what the index holds and when it last ran.
#[route(GET "/{repo}/code")]
async fn code_status(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let mut stanza = crate::code::brain_stanza(&app(cx).db, &name);
    if let Some(object) = stanza.as_object_mut() {
        object.insert("repo".to_owned(), serde_json::json!(name));
        object.insert(
            "embeddings_ready".to_owned(),
            serde_json::json!(app(cx).embeddings.ready().is_some()),
        );
        let why = app(cx).embeddings.why_not();
        if !why.is_empty() {
            object.insert("embeddings_note".to_owned(), serde_json::json!(why));
        }
    }
    json_ok(stanza)
}

// ---- brain -----------------------------------------------------------------------

#[topcoat::router::query_params(error = bad_request)]
struct BrainQuery {
    repo: Option<String>,
    since: Option<String>,
}

/// `GET /brain` — the deterministic aggregate. No model in the loop.
#[route(GET "/brain")]
async fn brain_get(cx: &Cx) -> Result<Response> {
    let query = query_params::<BrainQuery>(cx)?;
    let app = app(cx);
    let value = app
        .brain
        .aggregate(
            &app.config,
            &app.db,
            &app.mirrors,
            &app.docs,
            query.repo.as_deref(),
            query.since.as_deref(),
        )
        .await;
    let mut response = Response::new(topcoat::router::Body::from(value.to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct AskIn {
    question: String,
    #[serde(default)]
    repo: Option<String>,
}

/// `POST /brain/ask {question, repo?}` — mounted in effect only when
/// `ANTHROPIC_API_KEY` is set; without it the route answers 404.
#[route(POST "/brain/ask")]
async fn brain_ask(cx: &Cx, Json(input): Json<AskIn>) -> Result<Response> {
    let app = app(cx);
    if app.config.anthropic_key.is_none() {
        return Err(topcoat::router::error::not_found().into());
    }
    if input.question.trim().is_empty() {
        return Err(bad_request("question is required").into());
    }
    // `repo` reaches `Config::mirror_path` and from there `git --git-dir`, so an
    // unchecked value is a way to read any repository on the box. Every other handler
    // takes its repo from a path parameter it has already validated; this one takes it
    // from a JSON body, which is why the check has to be here.
    if let Some(repo) = &input.repo
        && !app.config.knows_repo(repo)
    {
        return Err(topcoat::router::error::not_found().into());
    }

    let state = app
        .brain
        .aggregate(&app.config, &app.db, &app.mirrors, &app.docs, input.repo.as_deref(), None)
        .await;

    // The full text of every plan and card under the repo filter.
    let mut documents: BTreeMap<String, String> = BTreeMap::new();
    for name in app.config.repos.names() {
        if let Some(filter) = &input.repo
            && filter != &name
        {
            continue;
        }
        let repo = app.mirrors.repo(&name);
        let Ok(default_branch) = repo.default_branch().await else { continue };
        let Ok(tip) = repo.tip(&default_branch).await else { continue };
        let index = app.docs.get(&name, &repo, &tip).await;
        for document in index.documents.values() {
            documents.insert(format!("{name}/{}", document.path), document.body.clone());
        }
    }

    // The tools reach only the repos the question was scoped to, so `?repo=` is a
    // real boundary rather than a hint.
    let tools = crate::brain::Tools {
        db: app.db.clone(),
        mirrors: app.mirrors.clone(),
        embeddings: app.embeddings.clone(),
        repos: match &input.repo {
            Some(only) => vec![only.clone()],
            None => app.config.repos.names(),
        },
    };

    match crate::brain::ask(&app.config, state, documents, &input.question, Some(&tools)).await {
        Ok(answer) => {
            let body = serde_json::to_string(&answer)?;
            let mut response = Response::new(topcoat::router::Body::from(body));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            Ok(response)
        }
        Err(crate::brain::AskError::Upstream { status, message }) => json_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            &message,
        ),
    }
}
