//! The web layer: Topcoat router, app state, and request helpers.
//!
//! Pages live in [`pages`], JSON/raw endpoints in [`api`], shared markup in
//! [`components`]. Everything is registered by link-time discovery
//! (`Router::builder().discover()`), which is Topcoat's recommended shape.

pub mod api;
pub mod architecture;
pub mod bugs;
pub mod components;
pub mod pages;
pub mod stack;
pub mod traces;

use std::sync::Arc;

use topcoat::context::{Cx, app_context};
use topcoat::router::{
    HeaderValue, OriginPolicy, Router, RouterBuilderDiscoverExt, StatusCode, header,
    response::Response,
};

use crate::brain::Brain;
use crate::bugs::Bugs;
use crate::ci::CiQueue;
use crate::code::Embeddings;
use crate::config::Config;
use crate::db::Db;
use crate::docs::DocIndexCache;
use crate::mirror::{MirrorStatus, Mirrors};
use crate::ops::{Actor, Ops};
use crate::upstream::Upstreams;

/// Everything a handler needs, registered once as Topcoat app context.
#[derive(Clone)]
pub struct App {
    pub config: Arc<Config>,
    pub db: Db,
    pub mirrors: Mirrors,
    /// Read-only mirrors of the upstream code repos declare in `.nashcode/stack.toml`.
    /// Never a repo: nothing routes to one, nothing writes to one.
    pub upstreams: Upstreams,
    pub docs: DocIndexCache,
    pub ci: CiQueue,
    pub ops: Ops,
    pub brain: Brain,
    /// Error tracking. Disabled unless a bucket is configured; see [`crate::bugs`].
    pub bugs: Bugs,
    /// The embedding model, shared with the indexer so a query and an index run use
    /// the one loaded copy. Empty until the first index run fills it.
    pub embeddings: Embeddings,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("App")
    }
}

/// Build the router. `discover()` collects every `#[page]` and `#[route]` in the crate.
pub fn router(app: App) -> Router {
    // Whole agent transcripts arrive as one request on the traces endpoints;
    // the 2 MiB default rejects any real session.
    Router::builder()
        .layer(topcoat::router::BodyLimit::max(64 * 1024 * 1024))
        // The ingest route is the one surface that must take a cross-origin POST
        // from any page in the world: that is how a browser SDK reports an error.
        // It carries no ambient credential — the sentry_key in the request is the
        // whole of its auth — so origin verification has nothing to protect there.
        .origin_policy(OriginPolicy::new().exempt_paths([
            bugs::INGEST_PATH,
            bugs::INGEST_PATH_BARE,
            bugs::INGEST_PATH_LOGS,
        ]))
        .discover()
        .app_context(app)
        .build()
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

pub const ASSET_HASH: &str = env!("NASHCODE_ASSET_HASH");
/// The code-split JS tree: the `nashcode.js` entry plus its content-hashed
/// `chunk-*.js` siblings (shiki grammars and themes, loaded on demand).
static ASSETS: include_dir::Dir<'static> = include_dir::include_dir!("$OUT_DIR/assets");
const APP_CSS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nashcode.css"));

pub fn js_url() -> String {
    format!("/assets/nashcode.js?v={ASSET_HASH}")
}

pub fn css_url() -> String {
    format!("/assets/nashcode.css?v={ASSET_HASH}")
}

const FAVICON: &[u8] = include_bytes!("../assets/favicon.svg");

#[topcoat::router::route(GET "/favicon.svg")]
async fn asset_favicon() -> topcoat::Result<Response> {
    Ok(asset_response("image/svg+xml", FAVICON))
}

topcoat::router::path_param!(file);

/// The JS entry and every chunk it pulls in. Chunks import each other as
/// `./chunk-*.js`, which the browser resolves inside `/assets/`, so one flat route
/// serves the whole tree. Lookups hit the embedded directory only — there is no
/// filesystem underneath and so nothing to traverse out of.
#[topcoat::router::route(GET "/assets/{file}")]
async fn asset_js(cx: &Cx) -> topcoat::Result<Response> {
    let name = topcoat::router::path_param::<File>(cx);
    let Some(entry) = ASSETS.get_file(name) else {
        return Err(topcoat::router::error::not_found().into());
    };
    let content_type = match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    };
    Ok(asset_response(content_type, entry.contents()))
}

#[topcoat::router::route(GET "/assets/nashcode.css")]
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
