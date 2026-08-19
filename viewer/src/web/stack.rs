//! `POST /{repo}/stack/sync`: fetch this repo's upstream column now.
//!
//! "Stack" here is the dependency column — the code a repo declares it is built on, in
//! `.nashcode/stack.toml` — and not the branch stacks the `/{repo}/stacks` tab shows.
//! SPEC keeps the two apart; so does this file. [`crate::upstream`] holds the machinery.
//!
//! The background clock refreshes tracked deps every half hour and the brain stanza
//! starts anything overdue behind the caller's back. This route is the third door: the
//! one an agent knocks on when half an hour is too long to wait. It fetches inline, so
//! the answer already reflects the wire — subject to
//! [`SYNC_DEBOUNCE`](crate::upstream::SYNC_DEBOUNCE), because a route anyone can call in
//! a loop, pointed at somebody else's server, needs a budget.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{
    HeaderValue, StatusCode, header, path_param,
    response::Response,
    route,
};

use crate::web::app;

path_param!(repo);

/// `POST /{repo}/stack/sync` — re-read the manifest, fetch what it names, and answer
/// with the stack that came out. `stack` is null for a repo that declares no manifest.
#[route(POST "/{repo}/stack/sync")]
async fn sync(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let app = app(cx);
    if !app.config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    // The repo's own mirror is where the manifest is read from, so it has to be there
    // first. A mirror that cannot refresh still answers from disk.
    app.mirrors.refresh(&name).await;
    let stack = app.upstreams.sync(&app.mirrors.repo(&name)).await;

    let body = serde_json::json!({ "repo": name, "stack": stack }).to_string();
    let mut response = Response::new(topcoat::router::Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}
