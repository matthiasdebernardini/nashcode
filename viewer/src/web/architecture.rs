//! The Architecture tab: the shape of the system, drawn and submitted.
//!
//! The loop is agent-driven. `GET /{repo}/code/graph` (in [`super::api`]) dumps
//! everything the analysis knows in one call, an agent draws a mermaid diagram from
//! it, and `POST /{repo}/architecture` stores the drawing verbatim. `GET
//! /{repo}/architecture` renders the latest one, or answers JSON when a client asks.
//!
//! Diagram text is untrusted. It reaches the browser as escaped text inside a `<pre>`
//! and is handed to mermaid from `textContent` — the server never splices it into HTML.

use serde::Deserialize;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{
    HeaderValue, StatusCode, error::bad_request, header, path_param, query_params, request,
    response::{IntoResponse, Response},
    route,
};
use topcoat::view::view;

use crate::db::{ArchitectureEntry, NewArchitecture};
use crate::render;
use crate::web::components::{Raw, shell};
use crate::web::{actor, app, repo_ctx};

path_param!(repo);

/// A diagram bigger than this is a dump, not a drawing.
const MERMAID_LIMIT: usize = 64 * 1024;

fn json_response(status: StatusCode, body: String) -> Response {
    let mut response = Response::new(topcoat::router::Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// The read endpoint doubles as a page: JSON when the client asks for it, HTML for a
/// browser.
fn wants_json(cx: &Cx) -> bool {
    request::headers(cx)
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.starts_with("application/json"))
}

// ---- submit ----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SubmissionIn {
    mermaid: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /{repo}/architecture {mermaid, title?, note?}` — store a diagram.
///
/// The server does not parse mermaid. A diagram that will not render is the author's
/// problem, shown as mermaid's own error box in the browser.
#[route(POST "/{repo}/architecture")]
async fn submit(cx: &Cx, body: topcoat::router::request::Bytes) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    if !app(cx).config.knows_repo(&name) {
        return Err(topcoat::router::error::not_found().into());
    }
    let input: SubmissionIn =
        serde_json::from_slice(&body).map_err(|error| bad_request(error.to_string()))?;

    if input.mermaid.trim().is_empty() {
        return Err(bad_request("mermaid is required").into());
    }
    if input.mermaid.len() > MERMAID_LIMIT {
        return Err(bad_request("mermaid must be 64 KiB or less").into());
    }

    let trim = |field: Option<String>| field.filter(|value| !value.trim().is_empty());
    let stored = app(cx).db.add_architecture(NewArchitecture {
        repo: name,
        mermaid: input.mermaid,
        title: trim(input.title),
        note: trim(input.note),
        author: actor(cx).login,
    })?;
    Ok(json_response(StatusCode::CREATED, serde_json::to_string(&stored)?))
}

// ---- read ------------------------------------------------------------------------

#[topcoat::router::query_params(error = bad_request)]
struct ArchitectureQuery {
    /// Present at all (`?history`) means list them instead of reading one.
    history: Option<String>,
    id: Option<i64>,
}

/// `GET /{repo}/architecture` — the tab, or JSON: the latest submission, `?history`
/// for the list, `?id=` for one.
#[route(GET "/{repo}/architecture")]
async fn tab(cx: &Cx) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    let query = query_params::<ArchitectureQuery>(cx)?;
    let (history_wanted, id) = (query.history.is_some(), query.id);

    let history = app(cx).db.architecture_history(&name)?;
    let showing = match id {
        Some(id) => app(cx).db.architecture(&name, id)?,
        None => app(cx).db.latest_architecture(&name)?,
    };

    if wants_json(cx) {
        if history_wanted {
            return Ok(json_response(StatusCode::OK, serde_json::to_string(&history)?));
        }
        let Some(submission) = showing else {
            return Err(topcoat::router::error::not_found().into());
        };
        return Ok(json_response(StatusCode::OK, serde_json::to_string(&submission)?));
    }
    if id.is_some() && showing.is_none() {
        return Err(topcoat::router::error::not_found().into());
    }

    // Nothing submitted: fall back to the repo's own ARCHITECTURE.md, else teach the
    // loop with the POST recipe.
    let fallback = match (&showing, ctx.status.available) {
        (None, true) => committed_diagrams(cx, &name).await,
        _ => Vec::new(),
    };

    let page = view! { cx =>
        shell(title: format!("{name} · architecture"), repo: name.clone(), active: "architecture", status: Some(ctx.status.clone()),
            <h3 class="mb-2"><i class="ph ph-graph"></i>" Architecture"</h3>
            if let Some(submission) = &showing {
                <div class="Box mb-3">
                    <div class="Box-header d-flex flex-items-center gap-2">
                        <strong>(submission.title.clone().unwrap_or_else(|| "Untitled diagram".to_owned()))</strong>
                        <span class="ml-auto text-small color-fg-muted">
                            (submission.author.clone()) " · " (submission.created_at.clone())
                        </span>
                    </div>
                    if let Some(note) = &submission.note {
                        <div class="Box-row markdown-body nashcode-annotation-body">
                            (Raw(render::markdown(note, &name, None, &[])))
                        </div>
                    }
                    diagram(source: submission.mermaid.clone())
                </div>
            } else if !fallback.is_empty() {
                <p class="color-fg-muted">
                    "Nothing submitted yet. This is "<code>"ARCHITECTURE.md"</code>" at the default branch."
                </p>
                for (index, source) in fallback.into_iter().enumerate() {
                    <div key=(index) class="Box mb-3">
                        diagram(source: source)
                    </div>
                }
            } else {
                empty_state(repo: name.clone())
            }
            if !history.is_empty() {
                <h4 class="mb-2"><i class="ph ph-clock-counter-clockwise"></i>" History "<span class="Counter">(history.len())</span></h4>
                <div class="Box">
                    let n = &name;
                    let current = showing.as_ref().map(|submission| submission.id);
                    for entry in history {
                        history_row(key: entry.id, repo: n.clone(), entry: entry, current: current)
                    }
                </div>
            }
        )
    }?;
    page.into_response(cx)
}

/// The diagram itself: escaped text the browser reads back out of `textContent`. The
/// `<pre>` is also the fallback when mermaid fails to load or the diagram will not
/// parse.
#[topcoat::view::component]
async fn diagram(#[into] source: String) -> Result {
    view! {
        <div class="nashcode-mermaid">
            <div class="nashcode-mermaid-out"></div>
            <pre class="nashcode-mermaid-source text-small nashcode-code">(source)</pre>
        </div>
    }
}

#[topcoat::view::component]
async fn history_row(
    #[into] repo: String,
    entry: ArchitectureEntry,
    #[default] current: Option<i64>,
) -> Result {
    view! {
        <div class="Box-row d-flex flex-items-center gap-2">
            <i class="ph ph-graph color-fg-muted"></i>
            <a class="Link--primary" href=(format!("/{repo}/architecture?id={}", entry.id))>
                (entry.title.clone().unwrap_or_else(|| format!("Submission {}", entry.id)))
            </a>
            if current == Some(entry.id) {
                <span class="Label Label--accent">"showing"</span>
            }
            <span class="ml-auto text-small color-fg-muted">
                (entry.author.clone()) " · " (entry.created_at.clone())
            </span>
        </div>
    }
}

/// No submission and no `ARCHITECTURE.md`: show the call that fills the page.
#[topcoat::view::component]
async fn empty_state(#[into] repo: String) -> Result {
    let recipe = format!(
        "curl -s \"$NASHCODE/{repo}/code/graph\"\n\
         curl -X POST \"$NASHCODE/{repo}/architecture\" \\\n  \
         -H 'content-type: application/json' \\\n  \
         -d '{{\"title\":\"Request path\",\"mermaid\":\"graph TD; web-->db;\"}}'"
    );
    view! {
        <div class="Box">
            <div class="Box-body">
                <p class="color-fg-muted">
                    "No diagram yet. Read the graph, draw it, submit it — or commit an "
                    <code>"ARCHITECTURE.md"</code>" with a mermaid block."
                </p>
                <pre class="text-small nashcode-code">(recipe)</pre>
            </div>
        </div>
    }
}

/// The mermaid blocks of the repo's `ARCHITECTURE.md` at the default branch tip.
async fn committed_diagrams(cx: &Cx, name: &str) -> Vec<String> {
    let repo = app(cx).mirrors.repo(name);
    let Ok(branch) = repo.default_branch().await else { return Vec::new() };
    let Ok(tip) = repo.tip(&branch).await else { return Vec::new() };
    let Ok(Some(bytes)) = repo.show_file(&tip, "ARCHITECTURE.md").await else {
        return Vec::new();
    };
    mermaid_blocks(&String::from_utf8_lossy(&bytes))
}

/// The ```` ```mermaid ```` fenced blocks of a markdown document, in order.
fn mermaid_blocks(source: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<Vec<&str>> = None;
    for line in source.lines() {
        let fence = line.trim_start().strip_prefix("```");
        match &mut open {
            None => {
                if fence.is_some_and(|info| info.trim().eq_ignore_ascii_case("mermaid")) {
                    open = Some(Vec::new());
                }
            }
            Some(lines) => {
                if fence.is_some() {
                    blocks.push(lines.join("\n"));
                    open = None;
                } else {
                    lines.push(line);
                }
            }
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mermaid_fences_are_extracted() {
        let source = "# Shape\n\n```rust\nfn main() {}\n```\n\n```mermaid\ngraph TD;\n  a-->b;\n```\ntail\n";
        assert_eq!(mermaid_blocks(source), vec!["graph TD;\n  a-->b;".to_owned()]);
    }

    #[test]
    fn an_unclosed_fence_yields_nothing() {
        assert!(mermaid_blocks("```mermaid\ngraph TD;\n").is_empty());
    }
}
