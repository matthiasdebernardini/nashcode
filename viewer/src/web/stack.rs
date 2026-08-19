//! The upstream column: the `/{repo}/stack` page, the read-only browser over each
//! dep's mirror, and `POST /{repo}/stack/sync`.
//!
//! "Stack" here is the dependency column — the code a repo declares it is built on, in
//! `.nashcode/stack.toml` — and not the branch stacks the `/{repo}/stacks` tab shows.
//! SPEC keeps the two apart; so does this file, and so does one line on each page.
//! [`crate::upstream`] holds the machinery.
//!
//! Three rules shape the browsing surfaces:
//!
//! **The browse surfaces never fetch.** A dep's tree and blobs render the commit that
//! is on disk, or say the mirror does not have it yet. `?rev=` narrows to a commit the
//! mirror already has; one it does not have is a 404, never a reason to go to the wire.
//! The column page is the one exception, and only in the background: like the brain
//! stanza, `GET /{repo}/stack` starts whatever is overdue behind the caller's back and
//! waits for none of it. The background clock refreshes tracked deps every half hour,
//! and `POST /{repo}/stack/sync` is the third door — the one an agent knocks on when
//! half an hour is too long to wait, and the only one that waits for the wire.
//!
//! **A dep is browsable under the repo that declared it, and nowhere else.** `{dep}` is
//! a manifest name looked up in that repo's own manifest. A name no manifest carries is
//! a 404, so no route can reach a mirror the repo never named.
//!
//! **Read-only, visibly.** No edit pencil, no new file, no comments, no raw download:
//! a mirror of somebody else's repository is something to read. The markup is the code
//! browser's, minus every affordance that writes.

use std::collections::HashMap;

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{
    HeaderValue, StatusCode, header, page, path_param, query_params,
    response::Response,
    route,
};
use topcoat::view::{component, view};

use crate::git::{EntryKind, TreeEntry};
use crate::render;
use crate::upstream::{self, Browse};
use crate::web::components::{Raw, shell, unavailable_card};
use crate::web::pages::{entry_icon, human_size, numbered_code, shiki_lang};
use crate::web::{app, repo_ctx};

path_param!(repo);
path_param!(dep);
path_param!(*rest);

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

// ---- /{repo}/stack — the column --------------------------------------------------

/// One row of the column, assembled before anything is rendered.
struct Card {
    name: String,
    layer: Option<String>,
    url: String,
    /// `pin` or `track`. `None` for a declaration that was refused: validation strips
    /// the mode off one, which is also how this page tells "refused" from "stale".
    mode: Option<&'static str>,
    want: Option<String>,
    commit: Option<String>,
    fresh: bool,
    last_fetched: Option<String>,
    error: Option<String>,
    /// Where the dep's tree opens. `None` when there is nothing on disk to open.
    href: Option<String>,
}

/// `GET /{repo}/stack` — the repo, then every dep at the commit its mirror answers.
///
/// One page, N trees: each entry opens that dep's own tree, and nothing is ever merged
/// into a single fake one. A dep whose mirror is absent or whose declaration was
/// refused renders as a card that says why, the way an unavailable repo does.
#[page("/{repo}/stack")]
async fn stack_page(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: format!("{name} · stack"), repo: name.clone(), active: "stack",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }

    let mirror = app(cx).mirrors.repo(&name);
    // Reading the column also starts whatever is overdue, behind this caller's back —
    // the same opportunistic refresh the brain stanza does. Nothing is waited on.
    let stack = app(cx).upstreams.stack(&mirror).await;
    let branch = mirror.default_branch().await.ok();
    let tip = match &branch {
        Some(branch) => mirror.tip(branch).await.ok(),
        None => None,
    };

    let manifest_error = stack.as_ref().and_then(|stack| stack.error.clone());
    let declared = stack.is_some();
    let cards: Vec<Card> = stack
        .map(|stack| {
            stack
                .deps
                .into_iter()
                .map(|dep| Card {
                    href: dep.have.is_some().then(|| dep_url(&name, &dep.name, None)),
                    name: dep.name,
                    layer: dep.layer,
                    url: dep.url,
                    mode: dep.mode,
                    want: dep.want,
                    commit: dep.have,
                    fresh: dep.fresh,
                    last_fetched: dep.last_fetched,
                    error: dep.error,
                })
                .collect()
        })
        .unwrap_or_default();

    view! { cx =>
        shell(title: format!("{name} · stack"), repo: name.clone(), active: "stack", status: Some(ctx.status.clone()),
            <h3 class="mb-1"><i class="ph ph-cube"></i>" Stack"</h3>
            <p class="color-fg-muted text-small mb-3">
                "The upstream column this repo declares in "<code>".nashcode/stack.toml"</code>". "
                "Branch stacks are the "<a href=(format!("/{name}/stacks"))>"Stacks"</a>" tab."
            </p>
            if let Some(error) = &manifest_error {
                <div class="Box mb-3 color-border-danger-emphasis">
                    <div class="Box-body">
                        <strong><i class="ph ph-warning"></i>" This manifest will not parse"</strong>
                        <pre class="mt-2 mb-0 text-small color-fg-danger">(error.clone())</pre>
                    </div>
                </div>
            }
            <div class="Box mb-3">
                <div class="Box-header d-flex flex-items-center gap-2">
                    <i class="ph ph-git-branch"></i>
                    <a class="Link--primary nashcode-display" href=(format!("/{name}"))>(name.clone())</a>
                    <span class="Label">"this repo"</span>
                    if let Some(branch) = &branch {
                        <span class="color-fg-muted text-small">(branch.clone())</span>
                    }
                    if let Some(tip) = &tip {
                        <code class="ml-auto commit-sha text-small">(short(tip))</code>
                    }
                </div>
            </div>
            if !declared {
                <div class="Box"><div class="Box-body color-fg-muted">
                    "This repo declares no "<code>".nashcode/stack.toml"</code>", so it has no stack."
                </div></div>
            } else if cards.is_empty() && manifest_error.is_none() {
                <div class="Box"><div class="Box-body color-fg-muted">
                    "The manifest declares no dependencies."
                </div></div>
            }
            for card in cards {
                <div key=(card.name.clone()) class=(if card.href.is_some() {
                    "Box mb-3"
                } else {
                    "Box mb-3 color-border-danger-emphasis"
                })>
                    <div class="Box-header d-flex flex-items-center gap-2">
                        <i class="ph ph-cube color-fg-muted"></i>
                        match &card.href {
                            Some(href) => <a class="Link--primary nashcode-display" href=(href.clone())>(card.name.clone())</a>,
                            None => <strong class="nashcode-display">(card.name.clone())</strong>,
                        }
                        if let Some(layer) = &card.layer {
                            <span class="Label">(layer.clone())</span>
                        }
                        if let Some(mode) = card.mode {
                            <span class="Label Label--secondary">(mode)</span>
                        }
                        if let Some(want) = &card.want {
                            <code class="text-small">(want.clone())</code>
                        }
                        <span class="ml-auto d-flex flex-items-center gap-2 text-small">
                            if card.fresh {
                                <span class="color-fg-success"><i class="ph ph-check-circle"></i>" fresh"</span>
                            } else if card.mode.is_none() {
                                // Nothing was ever fetched for this one and nothing ever
                                // will be: the declaration itself was refused, which is
                                // not the same as a mirror that is behind.
                                <span class="color-fg-danger"><i class="ph ph-x-circle"></i>" refused"</span>
                            } else {
                                <span class="color-fg-attention"><i class="ph ph-warning"></i>" stale"</span>
                            }
                            if let Some(commit) = &card.commit {
                                <code class="commit-sha">(short(commit))</code>
                            }
                        </span>
                    </div>
                    <div class="Box-row d-flex flex-items-center gap-2 text-small color-fg-muted">
                        <span>(card.url.clone())</span>
                        if let Some(fetched) = &card.last_fetched {
                            <span class="ml-auto">(format!("fetched {fetched}"))</span>
                        }
                    </div>
                    if let Some(error) = &card.error {
                        <div class="Box-row text-small color-fg-danger">
                            <i class="ph ph-warning"></i>" "(error.clone())
                        </div>
                    } else if card.href.is_none() {
                        <div class="Box-row text-small color-fg-muted">
                            "No mirror yet. The next refresh fills it in."
                        </div>
                    }
                </div>
            }
        )
    }
}

// ---- /{repo}/stack/{dep} — one dep's tree ----------------------------------------

#[topcoat::router::query_params(error = bad_request)]
struct RevQuery {
    rev: Option<String>,
}

/// A dep opened at a commit, with everything the pages need already resolved.
struct Open {
    dep: Browse,
    mirror: crate::git::Repo,
    /// The commit being browsed: the declaration's, or the one `?rev=` asked for.
    commit: String,
    /// The `?rev=` to keep on every link out of this page, empty when there was none.
    /// Browsing at the declared commit stays on the clean URL; browsing a gitlink stays
    /// pinned to it.
    query: String,
}

/// Resolve `{repo}`, `{dep}` and `?rev=` into a mirror and a commit.
///
/// `Ok(None)` is a dep that exists but has nothing on disk to show yet — the caller
/// renders a card. Everything a reader could have got wrong is a 404: an unknown repo,
/// a name this repo's manifest does not carry, a `?rev=` that is not a commit id, and a
/// commit the mirror does not have.
async fn open(cx: &Cx, name: &str, dep_name: &str) -> Result<Option<Open>> {
    let mirror_of_repo = app(cx).mirrors.repo(name);
    let Some(dep) = app(cx).upstreams.browse(&mirror_of_repo, dep_name).await else {
        return Err(topcoat::router::error::not_found().into());
    };
    let mirror = dep.mirror();

    let asked = query_params::<RevQuery>(cx)?.rev.clone().filter(|rev| !rev.is_empty());
    let (commit, query) = match asked {
        Some(rev) => {
            // The pin grammar first, before the string is anywhere near an argv. Then
            // one question for the mirror: `rev-parse --verify <rev>^{commit}` both
            // asks whether the commit is on disk and answers with its full id, since
            // peeling to `^{commit}` has to read the object to do it. A rev the mirror
            // does not have fails here, and failing here is a 404 — never a fetch.
            if !upstream::is_commit_id(&rev) {
                return Err(topcoat::router::error::not_found().into());
            }
            let Ok(full) = mirror.rev_parse(&format!("{rev}^{{commit}}")).await else {
                return Err(topcoat::router::error::not_found().into());
            };
            if full.is_empty() {
                return Err(topcoat::router::error::not_found().into());
            }
            let query = format!("?rev={}", render::encode_path(&full));
            (full, query)
        }
        None => match dep.commit.clone() {
            Some(commit) => (commit, String::new()),
            None => return Ok(None),
        },
    };
    Ok(Some(Open { dep, mirror, commit, query }))
}

#[page("/{repo}/stack/{dep}")]
async fn dep_root(cx: &Cx) -> Result {
    dep_tree_page(cx, String::new()).await
}

#[page("/{repo}/stack/{dep}/tree/{*rest}")]
async fn dep_tree(cx: &Cx) -> Result {
    let dir = path_param::<Rest>(cx).collect::<Vec<_>>().join("/");
    dep_tree_page(cx, dir).await
}

/// A directory listing in a dep's mirror, at the commit the column resolved.
async fn dep_tree_page(cx: &Cx, dir: String) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let dep_name = path_param::<Dep>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: format!("{name} · stack"), repo: name.clone(), active: "stack",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let Some(open) = open(cx, &name, &dep_name).await? else {
        return waiting_page(cx, &name, &dep_name).await;
    };

    let dir = dir.trim_matches('/').to_owned();
    let Some(mut entries) = open.mirror.ls_tree(&open.commit, &dir).await? else {
        return Err(topcoat::router::error::not_found().into());
    };
    entries.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.cmp(&b.name)));
    // A gitlink inside a dep is matched against the same repo's manifest: the column is
    // the declaring repo's, however deep the tree being read sits.
    let links = submodule_links(cx, &name, &open.mirror, &open.commit, &entries).await;

    let title = if dir.is_empty() {
        format!("{name} · {dep_name}")
    } else {
        format!("{name} · {dep_name} · {dir}")
    };
    view! { cx =>
        shell(title: title, repo: name.clone(), active: "stack", status: Some(ctx.status.clone()),
            dep_header(repo: name.clone(), dep: dep_name.clone(), url: open.dep.url.clone(),
                       commit: open.commit.clone(), pinned: !open.query.is_empty())
            dep_crumbs(repo: name.clone(), dep: dep_name.clone(), path: dir.clone(), query: open.query.clone())
            <div class="Box mb-3">
                <div class="Box-header d-flex flex-items-center gap-2">
                    <i class="ph ph-folder-open"></i>
                    <strong>(if dir.is_empty() { dep_name.clone() } else { dir.clone() })</strong>
                    <span class="Counter">(entries.len())</span>
                </div>
                if entries.is_empty() {
                    <div class="Box-row color-fg-muted">"This directory is empty."</div>
                }
                let repo_ref = &name;
                let dep_ref = &dep_name;
                let query_ref = &open.query;
                let links_ref = &links;
                for entry in entries {
                    <div key=(entry.path.clone()) class="Box-row d-flex flex-items-center gap-2">
                        <i class=(entry_icon(&entry))></i>
                        match dep_entry_url(repo_ref, dep_ref, &entry, query_ref)
                            .or_else(|| links_ref.get(&entry.path).cloned()) {
                            Some(url) => <a class="Link--primary" href=(url)>(entry.name.clone())</a>,
                            None => <span>(entry.name.clone()) <span class="Label">"submodule"</span></span>,
                        }
                        if let Some(size) = entry.size {
                            <span class="ml-auto color-fg-muted text-small">(human_size(size))</span>
                        }
                    </div>
                }
            </div>
        )
    }
}

// ---- /{repo}/stack/{dep}/blob/{*path} --------------------------------------------

/// `GET /{repo}/stack/{dep}/blob/{*path}` — one file in a dep's mirror.
///
/// Markdown is not rendered here, deliberately: the renderer autolinks against the
/// *viewing* repo's documents and branches, so a dep's README would grow links to files
/// that are somewhere else entirely. Upstream source reads as source.
#[page("/{repo}/stack/{dep}/blob/{*rest}")]
async fn dep_blob(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let dep_name = path_param::<Dep>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: format!("{name} · stack"), repo: name.clone(), active: "stack",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let Some(open) = open(cx, &name, &dep_name).await? else {
        return waiting_page(cx, &name, &dep_name).await;
    };

    let path = path_param::<Rest>(cx).collect::<Vec<_>>().join("/").trim_matches('/').to_owned();
    let (parent, file_name) = match path.rsplit_once('/') {
        Some((dir, file)) => (dir.to_owned(), file.to_owned()),
        None => (String::new(), path.clone()),
    };
    if file_name.is_empty() {
        return Err(topcoat::router::error::not_found().into());
    }

    // The parent listing answers two questions at once: does this path exist, and is it
    // a blob? `git show <rev>:<dir>` succeeds on a directory and prints its entries.
    let entry = open
        .mirror
        .ls_tree(&open.commit, &parent)
        .await?
        .unwrap_or_default()
        .into_iter()
        .find(|entry| entry.name == file_name);
    let Some(entry) = entry.filter(|entry| entry.kind != EntryKind::Dir) else {
        return Err(topcoat::router::error::not_found().into());
    };
    let Some(bytes) = open.mirror.show_file(&open.commit, &path).await? else {
        return Err(topcoat::router::error::not_found().into());
    };

    let size = bytes.len() as u64;
    let text = String::from_utf8(bytes)
        .ok()
        .map(|text| numbered_code(&text, shiki_lang(&file_name)));

    view! { cx =>
        shell(title: format!("{name} · {dep_name} · {path}"), repo: name.clone(), active: "stack", status: Some(ctx.status.clone()),
            dep_header(repo: name.clone(), dep: dep_name.clone(), url: open.dep.url.clone(),
                       commit: open.commit.clone(), pinned: !open.query.is_empty())
            dep_crumbs(repo: name.clone(), dep: dep_name.clone(), path: path.clone(), query: open.query.clone())
            <div class="Box">
                <div class="Box-header d-flex flex-items-center gap-2">
                    <i class=(entry_icon(&entry))></i>
                    <strong>(file_name.clone())</strong>
                    <span class="color-fg-muted text-small">(human_size(size))</span>
                    <span class="ml-auto text-small color-fg-muted">"read-only mirror"</span>
                </div>
                match &text {
                    Some(html) => (Raw(html.clone())),
                    None => <div class="Box-body color-fg-muted">
                        <i class="ph ph-file-x"></i>
                        (format!(" Binary file, {size} bytes."))
                    </div>,
                }
            </div>
        )
    }
}

// ---- the dep surfaces' own markup ------------------------------------------------

/// What every dep page says about where the reader is: which upstream, at which commit,
/// and that it is a mirror rather than a repo here.
#[component]
async fn dep_header(
    #[into] repo: String,
    #[into] dep: String,
    #[into] url: String,
    #[into] commit: String,
    #[default] pinned: bool,
) -> Result {
    view! {
        <div class="Box mb-2">
            <div class="Box-body d-flex flex-items-center gap-2 text-small">
                <i class="ph ph-cube color-fg-muted"></i>
                <a class="Link--primary nashcode-display" href=(format!("/{repo}/stack"))>"stack"</a>
                <span class="color-fg-muted">"/"</span>
                <strong class="nashcode-display">(dep.clone())</strong>
                <span class="color-fg-muted">(url.clone())</span>
                if pinned {
                    <span class="Label Label--accent">"at this commit"</span>
                }
                <code class="ml-auto commit-sha">(short(&commit))</code>
            </div>
        </div>
    }
}

/// `dep / dir / file` — every step but the last links back up the dep's own tree.
#[component]
async fn dep_crumbs(
    #[into] repo: String,
    #[into] dep: String,
    #[into] path: String,
    #[into] query: String,
) -> Result {
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let mut crumbs: Vec<(String, Option<String>)> = vec![(
        dep.clone(),
        (!segments.is_empty()).then(|| dep_url(&repo, &dep, Some(&query))),
    )];
    let mut walked = String::new();
    for (index, segment) in segments.iter().enumerate() {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(segment);
        let last = index + 1 == segments.len();
        crumbs.push((
            (*segment).to_owned(),
            (!last).then(|| {
                format!(
                    "/{repo}/stack/{}/tree/{}{query}",
                    render::encode_path(&dep),
                    render::encode_path(&walked)
                )
            }),
        ));
    }
    view! {
        <div class="d-flex flex-items-center gap-1 mb-2 h4 nashcode-display">
            for (index, (label, href)) in crumbs.into_iter().enumerate() {
                if index > 0 {
                    <span key=(format!("sep-{index}")) class="color-fg-muted">"/"</span>
                }
                match href {
                    Some(href) => <a key=(format!("crumb-{index}")) class="Link--primary no-underline" href=(href)>(label)</a>,
                    None => <strong key=(format!("crumb-{index}"))>(label)</strong>,
                }
            }
        </div>
    }
}

/// A dep that is declared and accepted, but whose commit is not on disk yet. Never a
/// 404 and never a 500: the mirror is somebody else's server having a slow day.
async fn waiting_page(cx: &Cx, repo: &str, dep: &str) -> Result {
    let repo = repo.to_owned();
    let dep = dep.to_owned();
    view! { cx =>
        shell(title: format!("{repo} · {dep}"), repo: repo.clone(), active: "stack",
            <div class="Box color-border-danger-emphasis">
                <div class="Box-body">
                    <h3 class="mb-1">
                        <i class="ph ph-warning"></i>" "(dep.clone())" is not mirrored yet"
                    </h3>
                    <p class="color-fg-muted mb-0">
                        "The commit this repo declares is not on disk. The "
                        <a href=(format!("/{repo}/stack"))>"stack"</a>
                        " page says what the last fetch made of it."
                    </p>
                </div>
            </div>)
    }
}

// ---- urls ------------------------------------------------------------------------

/// Where a dep's tree opens. `query` carries a `?rev=` through, when there is one.
fn dep_url(repo: &str, dep: &str, query: Option<&str>) -> String {
    format!("/{repo}/stack/{}{}", render::encode_path(dep), query.unwrap_or_default())
}

/// Where a listing row in a dep's tree points. A submodule has no content in this
/// mirror, so it has no page here; [`submodule_links`] is what may still give it one.
fn dep_entry_url(repo: &str, dep: &str, entry: &TreeEntry, query: &str) -> Option<String> {
    let dep = render::encode_path(dep);
    let path = render::encode_path(&entry.path);
    match entry.kind {
        EntryKind::Dir => Some(format!("/{repo}/stack/{dep}/tree/{path}{query}")),
        EntryKind::File | EntryKind::Symlink => {
            Some(format!("/{repo}/stack/{dep}/blob/{path}{query}"))
        }
        EntryKind::Submodule => None,
    }
}

/// The first eight of a commit id, the length every other page shows.
fn short(commit: &str) -> String {
    commit.chars().take(8).collect()
}

// ---- gitlinks that point at a mirrored dep ---------------------------------------

/// Where the submodule entries of one tree point, when their `.gitmodules` URL is one
/// of this repo's mirrored deps: `entry path -> href`.
///
/// A gitlink records a commit and nothing else — the URL lives in `.gitmodules`, which
/// is read at the same commit as the tree being rendered, because a submodule's URL can
/// change between commits like any other file. The URL is normalized by [`upstream::locate`],
/// the one normalizer, so every spelling of an upstream lands on the mirror directory
/// the column keyed. A gitlink that matches becomes a link to that dep at the gitlink's
/// own commit; every other one stays the inert label it is.
///
/// Costs nothing on a tree with no submodules, which is nearly every tree.
pub(super) async fn submodule_links(
    cx: &Cx,
    repo: &str,
    tree: &crate::git::Repo,
    rev: &str,
    entries: &[TreeEntry],
) -> HashMap<String, String> {
    let mut links = HashMap::new();
    if !entries.iter().any(|entry| entry.kind == EntryKind::Submodule) {
        return links;
    }
    let Ok(Some(raw)) = tree.show_file(rev, ".gitmodules").await else {
        return links;
    };
    let declared = gitmodules(&String::from_utf8_lossy(&raw));
    if declared.is_empty() {
        return links;
    }

    let app = app(cx);
    let column = app.upstreams.column(&app.mirrors.repo(repo)).await;
    if column.is_empty() {
        return links;
    }

    for entry in entries.iter().filter(|entry| entry.kind == EntryKind::Submodule) {
        let Some(url) = declared.get(&entry.path) else { continue };
        let Ok(location) = upstream::locate(&app.config.mirrors, url) else { continue };
        let Some(dep) = column.iter().find(|dep| dep.path == location.path) else { continue };
        // The commit the gitlink records, read out of the tree itself.
        let Ok(commit) = tree.rev_parse(&format!("{rev}:{}", entry.path)).await else {
            continue;
        };
        if !upstream::is_commit_id(&commit) {
            continue;
        }
        links.insert(
            entry.path.clone(),
            dep_url(repo, &dep.name, Some(&format!("?rev={commit}"))),
        );
    }
    links
}

/// `path -> url`, out of a `.gitmodules` file.
///
/// git's config grammar, cut to the shape this file always has: a `[submodule "name"]`
/// header starts an entry, `path` and `url` fill it in. Anything else is skipped rather
/// than guessed at — a submodule this cannot read simply keeps the inert label.
///
/// Where this diverges from git's own quoting and comment rules it fails closed, by
/// construction: the worst a misread line can do is yield a value that is not the URL,
/// which then fails [`upstream::locate`] or matches no dep, and the gitlink stays the
/// label it already was. Nothing here can invent a link to the wrong upstream.
fn gitmodules(text: &str) -> HashMap<String, String> {
    let mut found = HashMap::new();
    let mut path: Option<String> = None;
    let mut url: Option<String> = None;
    let mut flush = |path: &mut Option<String>, url: &mut Option<String>| {
        if let (Some(path), Some(url)) = (path.take(), url.take()) {
            found.insert(path, url);
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            flush(&mut path, &mut url);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = unquote(value.trim());
        match key.trim().to_ascii_lowercase().as_str() {
            "path" if !value.is_empty() => path = Some(value),
            "url" if !value.is_empty() => url = Some(value),
            _ => {}
        }
    }
    flush(&mut path, &mut url);
    found
}

/// git writes a config value that needs it in double quotes. Nothing else about the
/// quoting grammar matters here: a URL with an escape in it is not a URL [`upstream::locate`]
/// would accept anyway.
fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value)
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gitmodules_file_gives_up_its_paths_and_urls() {
        let found = gitmodules(
            "# a comment\n\
             [submodule \"dgit\"]\n\
             \tpath = vendor/dgit\n\
             \turl = https://github.com/littledivy/dgit.git\n\
             \tbranch = main\n\
             [submodule \"quoted\"]\n\
             \tpath = \"vendor/celld\"\n\
             \turl = \"https://github.com/denoland/celld\"\n",
        );
        assert_eq!(
            found.get("vendor/dgit").map(String::as_str),
            Some("https://github.com/littledivy/dgit.git")
        );
        assert_eq!(
            found.get("vendor/celld").map(String::as_str),
            Some("https://github.com/denoland/celld")
        );
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn half_an_entry_is_no_entry() {
        // A section with a path and no url, and a stray key outside any section: both
        // leave the gitlink inert rather than guessing at a mirror.
        let found = gitmodules(
            "url = https://example.invalid/loose\n\
             [submodule \"nourl\"]\n\
             path = vendor/nourl\n",
        );
        assert!(found.is_empty(), "{found:?}");
    }
}
