//! The web layer: Topcoat router, app state, and request helpers.
//!
//! Pages live in [`pages`], JSON/raw endpoints in [`api`], shared markup in
//! [`components`]. Everything is registered by link-time discovery
//! (`Router::builder().discover()`), which is Topcoat's recommended shape.

pub mod api;
pub mod components;
pub mod pages;

use std::sync::Arc;

use topcoat::context::{Cx, app_context};
use topcoat::router::{
    HeaderValue, Router, RouterBuilderDiscoverExt, StatusCode, header,
    response::Response,
};

use crate::brain::Brain;
use crate::ci::CiQueue;
use crate::config::Config;
use crate::db::Db;
use crate::docs::DocIndexCache;
use crate::mirror::{MirrorStatus, Mirrors};
use crate::ops::{Actor, Ops};

/// Everything a handler needs, registered once as Topcoat app context.
#[derive(Clone)]
pub struct App {
    pub config: Arc<Config>,
    pub db: Db,
    pub mirrors: Mirrors,
    pub docs: DocIndexCache,
    pub ci: CiQueue,
    pub ops: Ops,
    pub brain: Brain,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("App")
    }
}

/// Build the router. `discover()` collects every `#[page]` and `#[route]` in the crate.
pub fn router(app: App) -> Router {
    Router::builder().discover().app_context(app).build()
}

/// The app state, from any handler.
pub fn app(cx: &Cx) -> &App {
    app_context::<App>(cx)
}

/// Who is asking, from the Tailscale serve headers. Requests without them (direct
/// loopback hits) show as `local`.
pub fn actor(cx: &Cx) -> Actor {
    let headers = topcoat::router::request::headers(cx);
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let login = get("tailscale-user-login").unwrap_or_else(|| "local".to_owned());
    let name = get("tailscale-user-name").unwrap_or_else(|| login.clone());
    Actor { login, name }
}

/// A validated repo plus its mirror health. Every `/{repo}/...` handler starts here.
pub struct RepoCtx {
    pub name: String,
    pub status: MirrorStatus,
}

/// Look the repo up and refresh its mirror (debounced). Unknown repo -> 404; an
/// unavailable mirror is *not* an error here — pages render an error card instead.
pub async fn repo_ctx(cx: &Cx, name: &str) -> topcoat::Result<RepoCtx> {
    let app = app(cx);
    if !app.config.knows_repo(name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let status = app.mirrors.refresh(name).await;
    Ok(RepoCtx { name: name.to_owned(), status })
}

/// 303 See Other, for the redirect-after-POST pages.
pub fn see_other(to: &str) -> topcoat::Result<Response> {
    let mut response = Response::new(topcoat::router::Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_str(to)?);
    Ok(response)
}

// ---- embedded assets -------------------------------------------------------------

pub const ASSET_HASH: &str = env!("NASHGIT_ASSET_HASH");
const APP_JS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nashgit.js"));
const APP_CSS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nashgit.css"));

pub fn js_url() -> String {
    format!("/assets/nashgit.js?v={ASSET_HASH}")
}

pub fn css_url() -> String {
    format!("/assets/nashgit.css?v={ASSET_HASH}")
}

#[topcoat::router::route(GET "/assets/nashgit.js")]
async fn asset_js() -> topcoat::Result<Response> {
    Ok(asset_response("text/javascript; charset=utf-8", APP_JS))
}

#[topcoat::router::route(GET "/assets/nashgit.css")]
async fn asset_css() -> topcoat::Result<Response> {
    Ok(asset_response("text/css; charset=utf-8", APP_CSS))
}

fn asset_response(content_type: &str, body: &'static [u8]) -> Response {
    let mut response = Response::new(topcoat::router::Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("static content type"),
    );
    // The URL carries a content hash, so the payload itself can cache forever.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response
}
