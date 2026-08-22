//! Every HTML page. Data is assembled first, then rendered with Primer markup.

use std::collections::BTreeMap;

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::response::{IntoResponse, Response};
use topcoat::router::{page, path_param, query_params};
use topcoat::view::{View, component, view};

use crate::ci::strip_ansi;
use crate::db::{CiRun, Comment, CommentFilter, status};
use crate::docs::{self, DocIndex, Document};
use crate::git::{EntryKind, TreeEntry};
use crate::mirror::MirrorStatus;
use crate::render;
use crate::stack::StackGraph;
use crate::web::components::{
    Raw, StackRow, branch_label, ci_icon, comment_block, comment_composer,
    inline_comment_composer, shell, stack_column, unavailable_card,
};
use crate::web::{app, repo_ctx};

path_param!(repo);
path_param!(*rest);

/// Action suffixes parsed off the branch catch-all. A branch name may not end with one.
const ACTION_SUFFIXES: [&str; 6] =
    ["ci/rerun", "ci/requeue", "ci", "merge", "restack", "delete"];

/// Split `rest` into `(branch, action)`.
pub fn split_action(rest: &str) -> (&str, Option<&str>) {
    for action in ACTION_SUFFIXES {
        if let Some(branch) = rest.strip_suffix(action)
            && let Some(branch) = branch.strip_suffix('/')
            && !branch.is_empty()
        {
            return (branch, Some(action));
        }
    }
    (rest, None)
}

fn join_rest(cx: &Cx) -> String {
    path_param::<Rest>(cx).collect::<Vec<_>>().join("/")
}

async fn ci_for(cx: &Cx, repo: &str, tip: &str) -> Option<String> {
    app(cx)
        .db
        .latest_run(repo, tip)
        .ok()
        .flatten()
        .map(|run| run.effective_status().to_owned())
}

// ---- / ---------------------------------------------------------------------------

#[page("/")]
async fn home(cx: &Cx) -> Result {
    let app = app(cx);
    let mut sections: Vec<View> = Vec::new();
    for name in app.config.repos.names() {
        let status = app.mirrors.refresh(&name).await;
        let section = if status.available {
            let graph = StackGraph::infer(&app.mirrors.repo(&name)).await?;
            let mut chains: Vec<Vec<StackRow>> = Vec::new();
            for chain in graph.chains() {
                let mut rows = Vec::new();
                for branch in chain {
                    let node = graph.get(&branch).expect("chain branches exist");
                    rows.push(StackRow {
                        branch: branch.clone(),
                        ahead: node.ahead,
                        ci: ci_for(cx, &name, &node.tip).await,
                    });
                }
                chains.push(rows);
            }
            let stale = status.stale;
            let name_view = name.clone();
            view! { cx =>
                <div class="Box mb-3">
                    <div class="Box-header d-flex flex-items-center gap-2">
                        <i class="ph ph-git-branch"></i>
                        <a class="Link--primary no-underline h4 nashcode-display" href=(format!("/{name_view}"))>
                            (name_view.clone())
                        </a>
                        if stale {
                            <span class="Label Label--attention">"stale"</span>
                        }
                    </div>
                    <div class="Box-body d-flex flex-wrap gap-3">
                        if chains.is_empty() {
                            <span class="color-fg-muted">"no branches"</span>
                        }
                        let nv = &name_view;
                        for (i, chain) in chains.into_iter().enumerate() {
                            stack_column(key: i, repo: nv.clone(), chain: chain)
                        }
                    </div>
                </div>
            }?
        } else {
            let name_view = name.clone();
            view! { cx =>
                <div class="mb-3">
                    unavailable_card(repo: name_view, status: status.clone())
                </div>
            }?
        };
        sections.push(section);
    }
    let empty = app.config.repos.is_empty();
    view! {
        shell(
            title: "repos",
            <h2 class="mb-3">"Repositories"</h2>
            if empty {
                <div class="Box"><div class="Box-body color-fg-muted">
                    "No repos yet. Discovery has not found one on the git server; push a repo and it appears here."
                </div></div>
            }
            for (i, section) in sections.into_iter().enumerate() {
                <div key=(i)>(section)</div>
            }
        )
    }
}

// ---- /{repo} and /{repo}/tree/{*path} — the Code tab -----------------------------

#[page("/{repo}")]
async fn repo_code(cx: &Cx) -> Result {
    tree_page(cx, String::new()).await
}

#[page("/{repo}/tree/{*rest}")]
async fn repo_tree(cx: &Cx) -> Result {
    let dir = join_rest(cx);
    tree_page(cx, dir).await
}

/// A directory listing on the default branch, with that directory's README below it.
async fn tree_page(cx: &Cx, dir: String) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "code",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let repo = app(cx).mirrors.repo(&name);
    // A mirror of a repo nobody has pushed to yet has no default branch. That is a
    // state, not a failure.
    let Ok(branch) = repo.default_branch().await else {
        return view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "code", status: Some(ctx.status.clone()),
                <div class="Box"><div class="Box-body color-fg-muted">
                    "Nothing pushed here yet."
                </div></div>)
        };
    };
    let tip = repo.tip(&branch).await?;
    let dir = dir.trim_matches('/').to_owned();
    let Some(mut entries) = repo.ls_tree(&tip, &dir).await? else {
        return Err(topcoat::router::error::not_found().into());
    };
    // Directories first, then files, each alphabetical — the GitHub order.
    entries.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.cmp(&b.name)));

    // A gitlink whose `.gitmodules` URL is one of this repo's mirrored deps opens that
    // dep at the gitlink's commit; every other one keeps the inert label below. Free on
    // a tree with no submodules, which is nearly every tree.
    let links = crate::web::stack::submodule_links(cx, &name, &repo, &tip, &entries).await;

    let readme = entries
        .iter()
        .find(|entry| {
            entry.kind == EntryKind::File && entry.name.eq_ignore_ascii_case("README.md")
        })
        .cloned();
    let readme_html = match &readme {
        Some(entry) => render_markdown_at(cx, &name, &repo, &tip, &entry.path).await?,
        None => None,
    };

    let title =
        if dir.is_empty() { name.clone() } else { format!("{name} · {dir}") };
    let new_file_url = new_file_url(&name, &dir);
    view! { cx =>
        shell(title: title, repo: name.clone(), active: "code", status: Some(ctx.status.clone()),
            path_crumbs(repo: name.clone(), path: dir.clone())
            <div class="Box mb-3">
                <div class="Box-header d-flex flex-items-center gap-2">
                    <i class="ph ph-git-branch"></i>
                    branch_label(repo: name.clone(), branch: branch.clone())
                    <span class="Counter">(entries.len())</span>
                    <a class="ml-auto btn btn-sm" href=(new_file_url)>
                        <i class="ph ph-file-plus"></i>" New file"
                    </a>
                </div>
                if entries.is_empty() {
                    <div class="Box-row color-fg-muted">"This directory is empty."</div>
                }
                let n = &name;
                let links_ref = &links;
                for entry in entries {
                    <div key=(entry.path.clone()) class="Box-row d-flex flex-items-center gap-2">
                        <i class=(entry_icon(&entry))></i>
                        match entry_url(n, &entry).or_else(|| links_ref.get(&entry.path).cloned()) {
                            Some(url) => <a class="Link--primary" href=(url)>(entry.name.clone())</a>,
                            None => <span>(entry.name.clone()) <span class="Label">"submodule"</span></span>,
                        }
                        if let Some(size) = entry.size {
                            <span class="ml-auto color-fg-muted text-small">(human_size(size))</span>
                        }
                    </div>
                }
            </div>
            if let (Some(entry), Some(html)) = (&readme, readme_html) {
                <div class="Box">
                    <div class="Box-header d-flex flex-items-center gap-2">
                        <i class="ph ph-book-open"></i>
                        <strong>(entry.name.clone())</strong>
                    </div>
                    <div class="Box-body markdown-body">(Raw(html))</div>
                </div>
            }
        )
    }
}

// ---- /{repo}/blob/{*path} --------------------------------------------------------

/// What a blob page has to show. Markdown renders, other text goes in a numbered
/// `<pre>`, and anything that is not UTF-8 is offered as a download instead. Both text
/// arms carry ready-made HTML.
enum Blob {
    Markdown(String),
    Text(String),
    Binary,
}

#[page("/{repo}/blob/{*rest}")]
async fn repo_blob(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "code",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let path = join_rest(cx).trim_matches('/').to_owned();
    let (parent, file_name) = match path.rsplit_once('/') {
        Some((dir, file)) => (dir.to_owned(), file.to_owned()),
        None => (String::new(), path.clone()),
    };
    if file_name.is_empty() {
        return Err(topcoat::router::error::not_found().into());
    }

    let repo = app(cx).mirrors.repo(&name);
    let Ok(branch) = repo.default_branch().await else {
        return Err(topcoat::router::error::not_found().into());
    };
    let tip = repo.tip(&branch).await?;

    // The parent listing answers two questions at once: does this path exist, and is
    // it a blob? `git show <rev>:<dir>` succeeds on a directory and prints its
    // entries, so asking for the bytes first would render a tree as a file.
    let entry = repo
        .ls_tree(&tip, &parent)
        .await?
        .unwrap_or_default()
        .into_iter()
        .find(|entry| entry.name == file_name);
    let Some(entry) = entry.filter(|entry| entry.kind != EntryKind::Dir) else {
        return Err(topcoat::router::error::not_found().into());
    };
    let Some(bytes) = repo.show_file(&tip, &path).await? else {
        return Err(topcoat::router::error::not_found().into());
    };

    let size = bytes.len() as u64;
    let body = match String::from_utf8(bytes) {
        Ok(text) if is_markdown(&file_name) => Blob::Markdown(
            render_markdown_source(cx, &name, &repo, &tip, &text).await,
        ),
        Ok(text) => Blob::Text(numbered_code(&text, shiki_lang(&file_name))),
        Err(_) => Blob::Binary,
    };
    let raw_url = raw_url(&name, &branch, &path);
    // Binaries have no pencil: a textarea cannot hold them, and git is the tool for
    // replacing one.
    let edit_url = (!matches!(body, Blob::Binary)).then(|| edit_url(&name, &path));
    let wiki_url = is_markdown(&file_name).then(|| render::docs_url(&name, &path));

    view! { cx =>
        shell(title: format!("{name} · {path}"), repo: name.clone(), active: "code", status: Some(ctx.status.clone()),
            path_crumbs(repo: name.clone(), path: path.clone())
            <div class="Box">
                <div class="Box-header d-flex flex-items-center gap-2">
                    <i class=(entry_icon(&entry))></i>
                    <strong>(file_name.clone())</strong>
                    <span class="color-fg-muted text-small">(human_size(size))</span>
                    <span class="ml-auto d-flex flex-items-center gap-2 text-small">
                        if let Some(url) = &wiki_url {
                            <a class="Link--secondary" href=(url.clone())>
                                <i class="ph ph-book-open"></i>"wiki"
                            </a>
                        }
                        if let Some(url) = &edit_url {
                            <a class="Link--secondary" href=(url.clone())
                               aria-label="Edit this file" title="Edit this file">
                                <i class="ph ph-pencil-simple"></i>"edit"
                            </a>
                        }
                        <a class="Link--secondary" href=(raw_url.clone())>"raw"</a>
                    </span>
                </div>
                match &body {
                    Blob::Markdown(html) => <div class="Box-body markdown-body">(Raw(html.clone()))</div>,
                    Blob::Text(html) => (Raw(html.clone())),
                    Blob::Binary => <div class="Box-body color-fg-muted">
                        <i class="ph ph-file-x"></i>
                        (format!(" Binary file, {size} bytes. "))
                        <a href=(raw_url.clone())>"Download"</a>
                    </div>,
                }
            </div>
        )
    }
}

// ---- code-tab helpers ------------------------------------------------------------

/// `repo / dir / file` — every step but the last links back up the tree.
#[component]
async fn path_crumbs(#[into] repo: String, #[into] path: String) -> Result {
    let mut crumbs: Vec<(String, Option<String>)> = Vec::new();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    crumbs.push((
        repo.clone(),
        (!segments.is_empty()).then(|| format!("/{repo}")),
    ));
    let mut walked = String::new();
    for (i, segment) in segments.iter().enumerate() {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(segment);
        let last = i + 1 == segments.len();
        crumbs.push((
            (*segment).to_owned(),
            (!last).then(|| format!("/{repo}/tree/{}", render::encode_path(&walked))),
        ));
    }
    view! {
        <div class="d-flex flex-items-center gap-1 mb-2 h4 nashcode-display">
            for (i, (label, href)) in crumbs.into_iter().enumerate() {
                if i > 0 {
                    <span key=(format!("sep-{i}")) class="color-fg-muted">"/"</span>
                }
                match href {
                    Some(href) => <a key=(format!("crumb-{i}")) class="Link--primary no-underline" href=(href)>(label)</a>,
                    None => <strong key=(format!("crumb-{i}"))>(label)</strong>,
                }
            }
        </div>
    }
}

/// Where a listing row points. Submodules have no content here, so they have no page.
fn entry_url(repo: &str, entry: &TreeEntry) -> Option<String> {
    match entry.kind {
        EntryKind::Dir => Some(format!("/{repo}/tree/{}", render::encode_path(&entry.path))),
        EntryKind::File | EntryKind::Symlink => Some(render::blob_url(repo, &entry.path)),
        EntryKind::Submodule => None,
    }
}

pub(super) fn entry_icon(entry: &TreeEntry) -> &'static str {
    match entry.kind {
        EntryKind::Dir => "ph ph-folder color-fg-accent",
        EntryKind::Symlink => "ph ph-link color-fg-muted",
        EntryKind::Submodule => "ph ph-git-commit color-fg-muted",
        EntryKind::File => match extension(&entry.name).as_deref() {
            Some("md" | "markdown" | "txt" | "rst") => "ph ph-file-text color-fg-muted",
            Some(
                "rs" | "js" | "ts" | "tsx" | "jsx" | "py" | "go" | "rb" | "sh" | "c" | "h"
                | "cpp" | "toml" | "json" | "yaml" | "yml" | "css" | "html" | "sql",
            ) => "ph ph-file-code color-fg-muted",
            _ => "ph ph-file color-fg-muted",
        },
    }
}

fn extension(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase())
}

fn is_markdown(name: &str) -> bool {
    matches!(extension(name).as_deref(), Some("md" | "markdown"))
}

/// A file above this many lines is served without a language tag: shiki would spend
/// longer tokenizing it than a person will spend reading it. Numbering still happens —
/// that is the part a link depends on.
const HIGHLIGHT_LINE_LIMIT: usize = 5000;

/// Above this many lines the gutter itself is dropped and the file is served as one
/// plain `<pre>`.
///
/// The gutter costs about 145 bytes a line, so a 500k-line generated file would be a
/// 70 MB page — the viewer would be the thing that fell over, not the browser. Ten
/// times the highlight limit is well past any file a person reads and well short of
/// any page size that hurts. The raw link is on the header either way, and it is the
/// right tool for a file this size.
const GUTTER_LINE_LIMIT: usize = 50_000;

/// The shiki grammar for a file, by extension first and whole name second.
///
/// The name is what the server knows; the browser turns it into one dynamic
/// `import()`, so an id nobody's repo uses costs nothing. `None` means the plain
/// `<pre>` stands, which is also what happens with JS off.
pub(super) fn shiki_lang(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let by_name = match lower.rsplit('/').next().unwrap_or(&lower) {
        "dockerfile" | "containerfile" => Some("dockerfile"),
        "makefile" | "gnumakefile" => Some("make"),
        "justfile" | ".justfile" => Some("just"),
        "cmakelists.txt" => Some("cmake"),
        // shiki ships no grammar for ignore files; they stay a plain <pre>.
        ".env" | ".editorconfig" => Some("ini"),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    Some(match extension(&lower)?.as_str() {
        "rs" => "rust",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "py" | "pyi" => "python",
        "go" => "go",
        "rb" | "rake" | "gemspec" => "ruby",
        "sh" | "bash" | "zsh" | "ksh" => "shellscript",
        "fish" => "fish",
        "ps1" | "psm1" => "powershell",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "m" | "mm" => "objective-c",
        "cs" => "csharp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "scala" | "sbt" => "scala",
        "php" => "php",
        "pl" | "pm" => "perl",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "hs" => "haskell",
        "clj" | "cljs" | "edn" => "clojure",
        "ml" | "mli" => "ocaml",
        "zig" => "zig",
        "nix" => "nix",
        "vim" => "viml",
        "awk" => "awk",
        "groovy" | "gradle" => "groovy",
        "toml" => "toml",
        "json" => "json",
        "jsonc" => "jsonc",
        "json5" => "json5",
        "yaml" | "yml" => "yaml",
        "xml" | "plist" | "svg" | "xsd" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "sql" => "sql",
        "graphql" | "gql" => "graphql",
        "proto" => "proto",
        "tf" | "tfvars" => "terraform",
        "hcl" => "hcl",
        "ini" | "cfg" | "conf" | "properties" => "ini",
        "diff" | "patch" => "diff",
        "tex" | "sty" => "latex",
        "md" | "markdown" => "markdown",
        "bat" | "cmd" => "bat",
        _ => return None,
    })
}

/// A file's text as a `<pre>` with a numbered gutter.
///
/// Every line is its own block carrying an `L{n}` id and a clickable number, so
/// `#L10` and `#L10-L20` work with JS off and survive highlighting: `app.js` swaps the
/// *contents* of each line, never the gutter around it. `data-lang`, when present,
/// names the shiki grammar the browser should fetch.
pub(super) fn numbered_code(text: &str, lang: Option<&str>) -> String {
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A trailing newline ends the last line; it does not start another one.
    if lines.len() > 1 && lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.len() > GUTTER_LINE_LIMIT {
        return format!(
            "<pre class=\"Box-body nashcode-code text-small\" data-lines=\"{}\">{}</pre>",
            lines.len(),
            render::escape_text(text)
        );
    }
    let mut html = String::with_capacity(text.len() + lines.len() * 96);
    html.push_str("<pre class=\"Box-body nashcode-code nashcode-blob text-small\"");
    if let Some(lang) = lang.filter(|_| lines.len() <= HIGHLIGHT_LINE_LIMIT) {
        html.push_str(&format!(" data-lang=\"{}\"", render::escape_attr(lang)));
    }
    html.push_str(&format!(" data-lines=\"{}\">", lines.len()));
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        html.push_str(&format!(
            "<span class=\"nashcode-line\" id=\"L{number}\">\
             <a class=\"nashcode-lineno\" href=\"#L{number}\" data-line=\"{number}\" \
             aria-label=\"Line {number}\"></a>\
             <span class=\"nashcode-line-code\">{}</span></span>",
            render::escape_text(line.trim_end_matches('\r'))
        ));
    }
    html.push_str("</pre>");
    html
}

pub(super) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{size:.1} {}", UNITS[unit]) }
}

/// Render one file in the mirror as markdown, autolinked against the repo's document
/// index and branch list. `None` when the file cannot be read.
async fn render_markdown_at(
    cx: &Cx,
    name: &str,
    repo: &crate::git::Repo,
    tip: &str,
    path: &str,
) -> Result<Option<String>> {
    let Some(bytes) = repo.show_file(tip, path).await? else {
        return Ok(None);
    };
    let source = String::from_utf8_lossy(&bytes).into_owned();
    Ok(Some(render_markdown_source(cx, name, repo, tip, &source).await))
}

async fn render_markdown_source(
    cx: &Cx,
    name: &str,
    repo: &crate::git::Repo,
    tip: &str,
    source: &str,
) -> String {
    let index = app(cx).docs.get(name, repo, tip).await;
    let branches = repo.branches().await.unwrap_or_default();
    render::markdown(source, name, Some(&index), &branches)
}

// ---- /{repo}/edit and /{repo}/edit/{*path} — one file, one commit -----------------

/// A repo-relative path the write path may touch, or `None`.
///
/// Paths arrive from a URL and from a text field, so both are normalized here and
/// nowhere else: no empty or dotted segments, no `.git`, no backslashes, no control
/// characters. Refusing beats sanitizing — a path that had to be repaired is a path
/// the person did not mean.
fn safe_repo_path(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed.len() > 512 {
        return None;
    }
    for segment in trimmed.split('/') {
        let bad = segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.eq_ignore_ascii_case(".git")
            || segment.contains('\\')
            || segment.chars().any(char::is_control);
        if bad {
            return None;
        }
    }
    Some(trimmed.to_owned())
}

/// The "New file" button's target: the empty form, with the directory prefilled.
fn new_file_url(repo: &str, dir: &str) -> String {
    if dir.is_empty() {
        return format!("/{repo}/edit");
    }
    let query = serde_urlencoded::to_string([("dir", dir)]).unwrap_or_default();
    format!("/{repo}/edit?{query}")
}

/// Where the pencil points.
fn edit_url(repo: &str, path: &str) -> String {
    format!("/{repo}/edit/{}", render::encode_path(path))
}

/// Where the raw bytes of a file on a branch are served.
fn raw_url(repo: &str, branch: &str, path: &str) -> String {
    format!(
        "/{repo}/raw/{}/{}",
        render::encode_path(branch),
        render::encode_path(path)
    )
}

/// What the form has to say about the attempt that just failed to commit.
enum EditNote {
    /// Nothing was committed, and that is a problem to fix.
    Refused(String),
    /// Nothing was committed, and nothing needed to be.
    Unchanged(String),
}

/// What the edit form is showing right now — the file as typed, not as stored, so a
/// rejected commit comes back with the person's own text still in it.
struct EditState {
    path: String,
    content: String,
    message: String,
    /// A path the repo does not have yet: the field is editable and the form says "Create".
    creating: bool,
    /// The blob this form was opened against, empty for a file that did not exist.
    /// Posted back so the commit can refuse to overwrite someone else's push.
    base: String,
    /// The loaded file ended without a newline. A round trip must not add one.
    no_trailing_newline: bool,
    note: Option<EditNote>,
}

#[topcoat::router::query_params(error = bad_request)]
struct NewFileQuery {
    dir: Option<String>,
}

/// `GET /{repo}/edit` — the empty form. `?dir=` prefills the directory the person
/// pressed "New file" in.
#[page("/{repo}/edit")]
async fn edit_new(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "code",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let dir = query_params::<NewFileQuery>(cx)?
        .dir
        .clone()
        .and_then(|dir| safe_repo_path(&dir))
        .map(|dir| format!("{dir}/"))
        .unwrap_or_default();
    edit_view(
        cx,
        &name,
        &ctx.status,
        EditState {
            path: dir,
            content: String::new(),
            message: String::new(),
            creating: true,
            base: String::new(),
            no_trailing_newline: false,
            note: None,
        },
    )
    .await
}

/// The file as the default branch has it right now: its blob id and its bytes.
///
/// The blob id is git's own content hash, so "did this change" needs no comparison of
/// the bytes and no timestamp anyone could disagree about.
async fn current_blob(
    repo: &crate::git::Repo,
    path: &str,
) -> Result<Option<(String, Vec<u8>)>> {
    let Ok(branch) = repo.default_branch().await else {
        return Ok(None);
    };
    let Ok(tip) = repo.tip(&branch).await else {
        return Ok(None);
    };
    let Some(bytes) = repo.show_file(&tip, path).await? else {
        return Ok(None);
    };
    let id = repo
        .rev_parse(&format!("{tip}:{path}"))
        .await
        .unwrap_or_default()
        .trim()
        .to_owned();
    Ok(Some((id, bytes)))
}

/// `GET /{repo}/edit/{*path}` — the same form, holding the file as the default branch
/// has it.
#[page("/{repo}/edit/{*rest}")]
async fn edit_existing(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "code",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let Some(path) = safe_repo_path(&join_rest(cx)) else {
        return Err(topcoat::router::error::not_found().into());
    };

    let repo = app(cx).mirrors.repo(&name);
    let Some((base, bytes)) = current_blob(&repo, &path).await? else {
        return Err(topcoat::router::error::not_found().into());
    };
    let no_trailing_newline = !bytes.is_empty() && !bytes.ends_with(b"\n");
    // A textarea holds text. Anything else keeps its download link and its pencil-less
    // header; arriving here by hand gets the reason, not a 404.
    let Ok(content) = String::from_utf8(bytes) else {
        return view! { cx =>
            shell(title: format!("{name} · {path}"), repo: name.clone(), active: "code", status: Some(ctx.status.clone()),
                <div class="Box color-border-danger-emphasis">
                    <div class="Box-body">
                        <h3 class="mb-1"><i class="ph ph-warning"></i>" Not a text file"</h3>
                        <p class="color-fg-muted mb-0">
                            (path.clone()) " is not UTF-8, so there is nothing a textarea can hold. "
                            "Push a replacement with git."
                        </p>
                    </div>
                </div>)
        };
    };

    edit_view(
        cx,
        &name,
        &ctx.status,
        EditState {
            path,
            content,
            message: String::new(),
            creating: false,
            base,
            no_trailing_newline,
            note: None,
        },
    )
    .await
}

/// The form itself. One textarea, one commit message, one button — the shape the board
/// already writes through, with a path field only when the file does not exist yet.
async fn edit_view(cx: &Cx, name: &str, status: &MirrorStatus, state: EditState) -> Result {
    let repo = app(cx).mirrors.repo(name);
    let branch = repo.default_branch().await.unwrap_or_else(|_| "the default branch".to_owned());
    let title = if state.creating {
        format!("{name} · new file")
    } else {
        format!("{name} · edit {}", state.path)
    };
    let heading = if state.creating { "New file" } else { "Edit file" };
    let cancel_url = if state.creating {
        format!("/{name}")
    } else {
        render::blob_url(name, &state.path)
    };
    let placeholder = if state.creating {
        format!("Create {}", if state.path.is_empty() { "a file".to_owned() } else { state.path.clone() })
    } else {
        format!("Update {}", state.path)
    };
    let name = name.to_owned();
    view! { cx =>
        shell(title: title, repo: name.clone(), active: "code", status: Some(status.clone()),
            <h3 class="mb-2">
                <i class=(if state.creating { "ph ph-file-plus" } else { "ph ph-pencil-simple" })></i>
                " " (heading)
            </h3>
            if let Some(EditNote::Refused(why)) = &state.note {
                <div class="flash flash-error mb-3">
                    <i class="ph ph-warning"></i>" Nothing was committed: " (why.clone())
                </div>
            }
            if let Some(EditNote::Unchanged(what)) = &state.note {
                <div class="flash mb-3">
                    <i class="ph ph-check-circle"></i>" " (what.clone())
                </div>
            }
            <form method="post" action=(format!("/{name}/edit")) class="Box">
                // The blob the form was opened against, and whether it ended without a
                // newline. Both travel with the edit so the commit can tell a stale
                // form from a fresh one and leave the file's own shape alone.
                <input type="hidden" name="base" value=(state.base.clone())>
                <input type="hidden" name="eof" value=(if state.no_trailing_newline { "none" } else { "newline" })>
                <div class="Box-header d-flex flex-items-center gap-2">
                    <i class="ph ph-file"></i>
                    if state.creating {
                        <input
                            type="text"
                            name="path"
                            class="form-control input-block"
                            placeholder="path/to/file.md"
                            value=(state.path.clone())
                            required=""
                            autofocus=""
                        >
                    } else {
                        <strong>(state.path.clone())</strong>
                        <input type="hidden" name="path" value=(state.path.clone())>
                    }
                </div>
                <div class="Box-body">
                    <textarea
                        name="content"
                        class="form-control input-block nashcode-editor"
                        rows="24"
                        spellcheck="false"
                        aria-label="File contents"
                    >(state.content.clone())</textarea>
                </div>
                <div class="Box-footer d-flex flex-items-center gap-2">
                    <input
                        type="text"
                        name="message"
                        class="form-control input-block"
                        placeholder=(placeholder)
                        value=(state.message.clone())
                        aria-label="Commit message"
                    >
                    <a class="btn" href=(cancel_url)>"Cancel"</a>
                    <button type="submit" class="btn btn-primary">
                        (format!("Commit to {branch}"))
                    </button>
                </div>
            </form>
        )
    }
}

/// The form's body. `path` travels in the body, not the URL, so one endpoint serves
/// both a new file and an edit.
#[derive(Debug, Default, serde::Deserialize)]
struct EditIn {
    #[serde(default)]
    path: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    message: String,
    /// The blob the form was opened against. Absent for a client that never loaded a
    /// form — a script posting straight at the endpoint gets no staleness check,
    /// because it never had a version to be stale against.
    base: Option<String>,
    /// `none` when the loaded file ended without a newline.
    eof: Option<String>,
}

/// `POST /{repo}/edit` — one commit on the default branch, pushed through the same
/// write path the board uses. The push must succeed before this redirects; a failure
/// re-renders the form with the person's text and the reason.
#[topcoat::router::route(POST "/{repo}/edit")]
async fn edit_post(cx: &Cx, body: topcoat::router::request::Bytes) -> Result<Response> {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        let page = view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "code",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        }?;
        return page.into_response(cx);
    }
    let input: EditIn = serde_urlencoded::from_bytes(&body)
        .or_else(|_| serde_json::from_slice(&body))
        .unwrap_or_default();

    // A textarea posts CRLF whatever the file held; a repo does not want that. The
    // trailing newline is restored unless the file the form loaded did without one:
    // adding one would show up as a diff nobody asked for.
    let wants_newline = input.eof.as_deref() != Some("none");
    let mut content = input.content.replace("\r\n", "\n");
    if wants_newline && !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }

    // Re-render the form as it stands, carrying whatever the person typed.
    let form = |path: String, creating: bool, base: String, note: EditNote| EditState {
        path,
        content: content.clone(),
        message: input.message.clone(),
        creating,
        base,
        no_trailing_newline: !wants_newline,
        note: Some(note),
    };

    let Some(path) = safe_repo_path(&input.path) else {
        let page = edit_view(
            cx,
            &name,
            &ctx.status,
            form(
                input.path.clone(),
                true,
                String::new(),
                EditNote::Refused(
                    "a path must be repo-relative, with no empty, dotted, or .git segments"
                        .to_owned(),
                ),
            ),
        )
        .await?;
        return page.into_response(cx);
    };

    let repo = app(cx).mirrors.repo(&name);
    let head = current_blob(&repo, &path).await?;
    let head_id = head.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
    let existed = head.is_some();

    // Someone pushed between the form loading and this submit. Overwriting their work
    // silently is the one outcome nobody can undo from here, so the edit comes back
    // instead — the person's text is still in the box and the file is still theirs.
    if let Some(base) = &input.base
        && base.trim() != head_id
    {
        let why = if existed {
            format!("{path} changed since this form was opened. Open it again and re-apply the edit.")
        } else {
            format!("{path} was deleted since this form was opened.")
        };
        let page =
            edit_view(cx, &name, &ctx.status, form(path, !existed, head_id, EditNote::Refused(why)))
                .await?;
        return page.into_response(cx);
    }

    // Nothing to commit is not a failure, and it is not git's error message either.
    if head.as_ref().is_some_and(|(_, bytes)| bytes == content.as_bytes()) {
        let page = edit_view(
            cx,
            &name,
            &ctx.status,
            form(
                path.clone(),
                false,
                head_id,
                EditNote::Unchanged(format!("{path} is already exactly this. Nothing to commit.")),
            ),
        )
        .await?;
        return page.into_response(cx);
    }

    let message = match input.message.trim() {
        "" if existed => format!("Update {path}"),
        "" => format!("Create {path}"),
        typed => typed.to_owned(),
    };

    let who = crate::web::actor(cx);
    match app(cx).ops.commit_file(&name, &path, &content, &message, &who).await {
        Ok(_) => crate::web::see_other(&render::blob_url(&name, &path)),
        Err(error) => {
            let page = edit_view(
                cx,
                &name,
                &ctx.status,
                form(path, !existed, head_id, EditNote::Refused(error.to_string())),
            )
            .await?;
            page.into_response(cx)
        }
    }
}

// ---- /{repo}/stacks --------------------------------------------------------------

#[page("/{repo}/stacks")]
async fn repo_stacks(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! {
            shell(title: name.clone(), repo: name.clone(), active: "stacks",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let graph = StackGraph::infer(&app(cx).mirrors.repo(&name)).await?;
    let mut chains: Vec<Vec<StackRow>> = Vec::new();
    for chain in graph.chains() {
        let mut rows = Vec::new();
        for branch in chain {
            let node = graph.get(&branch).expect("chain branches exist");
            rows.push(StackRow {
                branch: branch.clone(),
                ahead: node.ahead,
                ci: ci_for(cx, &name, &node.tip).await,
            });
        }
        chains.push(rows);
    }

    // The branch list, Forgejo's fields: branch, stack parent, ahead count, last
    // commit, CI dot.
    struct Row {
        branch: String,
        parent: Option<String>,
        ahead: usize,
        last: Option<(String, String, String)>, // short, subject, date
        ci: Option<String>,
        is_default: bool,
    }
    let mut rows = Vec::new();
    for node in graph.nodes.values() {
        rows.push(Row {
            branch: node.branch.clone(),
            parent: node.parent.clone(),
            ahead: node.ahead,
            last: node
                .last_commit
                .as_ref()
                .map(|c| (c.short.clone(), c.subject.clone(), c.date.clone())),
            ci: ci_for(cx, &name, &node.tip).await,
            is_default: node.branch == graph.default_branch,
        });
    }
    rows.sort_by_key(|row| (!row.is_default, row.branch.clone()));

    let audit = app(cx).db.audit(&name, 50).unwrap_or_default();

    view! {
        shell(title: format!("{name} · stacks"), repo: name.clone(), active: "stacks", status: Some(ctx.status.clone()),
            <h3 class="mb-1"><i class="ph ph-stack"></i>" Stacks"</h3>
            <p class="color-fg-muted text-small mb-3">
                "Branches stacked on branches. The upstream dependency column is the "
                <a href=(format!("/{name}/stack"))>"Stack"</a>" tab."
            </p>
            <div class="d-flex flex-wrap gap-3 mb-4">
                let n = &name;
                for (i, chain) in chains.into_iter().enumerate() {
                    stack_column(key: i, repo: n.clone(), chain: chain)
                }
            </div>
            <div class="Box mb-4">
                <div class="Box-header d-flex flex-items-center gap-2">
                    <i class="ph ph-git-branch"></i>
                    <strong>"Branches"</strong>
                    <span class="Counter">(rows.len())</span>
                </div>
                let n = &name;
                for row in rows {
                    <div key=(row.branch.clone()) class="Box-row d-flex flex-items-center gap-2">
                        ci_icon(run_status: row.ci.clone())
                        branch_label(repo: n.clone(), branch: row.branch.clone())
                        if row.is_default {
                            <span class="Label">"default"</span>
                        }
                        if let Some(parent) = &row.parent {
                            <span class="color-fg-muted text-small">
                                <i class="ph ph-caret-right"></i>
                                " on "
                            </span>
                            branch_label(repo: n.clone(), branch: parent.clone())
                        }
                        if row.ahead > 0 {
                            <span class="Counter">(format!("+{}", row.ahead))</span>
                        }
                        if let Some((short, subject, date)) = &row.last {
                            <span class="ml-auto color-fg-muted text-small">
                                <code class="commit-sha">(short.clone())</code>
                                " " (subject.clone())
                                " · " (date.clone())
                            </span>
                        }
                    </div>
                }
            </div>
            <h3 class="mb-2"><i class="ph ph-git-merge"></i>" Merge and restack log"</h3>
            <div class="Box">
                if audit.is_empty() {
                    <div class="Box-body color-fg-muted">"Nothing merged or restacked yet."</div>
                }
                for entry in audit {
                    <div key=(entry.id) class="Box-row d-flex flex-items-center gap-2">
                        <i class=(if entry.action == "merge" { "ph ph-git-merge" } else { "ph ph-arrows-clockwise" })></i>
                        <strong class="nashcode-display">(entry.actor.clone())</strong>
                        <span>(entry.detail.clone())</span>
                        <span class="color-fg-muted text-small ml-auto">
                            <code class="commit-sha">(entry.old_tip.chars().take(8).collect::<String>())</code>
                            " → "
                            <code class="commit-sha">(entry.new_tip.chars().take(8).collect::<String>())</code>
                            " · " (entry.created_at.clone())
                        </span>
                    </div>
                }
            </div>
        )
    }
}

// ---- /{repo}/ci ------------------------------------------------------------------

#[page("/{repo}/ci")]
async fn repo_ci(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    let runs = app(cx).db.recent_runs(&name, 50).unwrap_or_default();
    view! {
        shell(title: format!("{name} · ci"), repo: name.clone(), active: "ci", status: Some(ctx.status.clone()),
            <h3 class="mb-2"><i class="ph ph-play"></i>" Recent CI runs"</h3>
            <div class="Box">
                if runs.is_empty() {
                    <div class="Box-body color-fg-muted">"No runs yet. New branch tips queue automatically."</div>
                }
                let n = &name;
                for run in runs {
                    ci_run_row(key: run.id, repo: n.clone(), run: run)
                }
            </div>
        )
    }
}

#[component]
async fn ci_run_row(#[into] repo: String, run: CiRun) -> Result {
    let effective = run.effective_status().to_owned();
    view! {
        <div class="Box-row d-flex flex-items-center gap-2">
            ci_icon(run_status: Some(effective))
            branch_label(repo: repo.clone(), branch: run.branch.clone())
            <code class="commit-sha">(run.commit.chars().take(8).collect::<String>())</code>
            <a href=(format!("/{repo}/{}/ci?run={}", run.branch, run.id)) class="Link--secondary text-small">
                "log"
            </a>
            <span class="ml-auto color-fg-muted text-small">
                (format!("{} · {}ms", run.created_at, run.duration_ms))
            </span>
        </div>
    }
}

// ---- /{repo}/plans ---------------------------------------------------------------

#[topcoat::router::query_params(error = bad_request)]
struct PlansQuery {
    branch: Option<String>,
}

/// The repo's default branch and the doc index at the requested (or default) branch.
async fn doc_view(cx: &Cx, name: &str, branch: Option<String>) -> Result<(String, String, std::sync::Arc<DocIndex>)> {
    let app = app(cx);
    let repo = app.mirrors.repo(name);
    let branch = match branch {
        Some(branch) => branch,
        None => repo.default_branch().await?,
    };
    let tip = match repo.tip(&branch).await {
        Ok(tip) => tip,
        Err(_) => return Err(topcoat::router::error::not_found().into()),
    };
    let index = app.docs.get(name, &repo, &tip).await;
    Ok((branch, tip, index))
}

#[page("/{repo}/plans")]
async fn repo_plans(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! {
            shell(title: name.clone(), repo: name.clone(), active: "plans",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let query_branch = query_params::<PlansQuery>(cx)?.branch.clone();
    let (branch, _tip, index) = doc_view(cx, &name, query_branch).await?;

    let plans: Vec<Document> = index.plans().into_iter().cloned().collect();
    let comment_counts: BTreeMap<String, usize> = {
        let filter = CommentFilter { repo: name.clone(), ..Default::default() };
        let mut counts = BTreeMap::new();
        for comment in app(cx).db.comments(&filter).unwrap_or_default() {
            if let Some(file) = comment.file {
                *counts.entry(file).or_insert(0) += 1;
            }
        }
        counts
    };

    view! {
        shell(title: format!("{name} · plans"), repo: name.clone(), active: "plans", status: Some(ctx.status.clone()),
            <h3 class="mb-2"><i class="ph ph-file-text"></i>" Plans on "<code>(branch.clone())</code></h3>
            <div class="Box">
                if plans.is_empty() {
                    <div class="Box-body color-fg-muted">"No plans/*.md on this branch."</div>
                }
                let n = &name;
                let b = &branch;
                let counts = &comment_counts;
                for plan in plans {
                    <div key=(plan.path.clone()) class="Box-row d-flex flex-items-center gap-2">
                        <i class="ph ph-file-text color-fg-muted"></i>
                        <a class="Link--primary" href=(format!("/{n}/{}?branch={b}", plan.path))>
                            (plan.title.clone())
                        </a>
                        if let Some(refbranch) = &plan.refs.branch {
                            branch_label(repo: n.clone(), branch: refbranch.clone())
                        }
                        if let Some(count) = counts.get(&plan.path) {
                            <span class="Counter"><i class="ph ph-chat-circle"></i>" "(*count)</span>
                        }
                        <span class="ml-auto color-fg-muted text-small">(plan.summary.chars().take(90).collect::<String>())</span>
                    </div>
                }
            </div>
        )
    }
}

// ---- /{repo}/plans/{*path} and /{repo}/tasks/{*path} ------------------------------

#[page("/{repo}/plans/{*rest}")]
async fn plan_page(cx: &Cx) -> Result {
    document_page(cx, docs::PLANS_DIR).await
}

#[page("/{repo}/tasks/{*rest}")]
async fn card_page(cx: &Cx) -> Result {
    document_page(cx, docs::TASKS_DIR).await
}

async fn document_page(cx: &Cx, root: &'static str) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "plans",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let tail = join_rest(cx);
    let path = if tail.starts_with(&format!("{root}/")) { tail } else { format!("{root}/{tail}") };

    let query_branch = query_params::<PlansQuery>(cx)?.branch.clone();
    let (branch, tip, index) = doc_view(cx, &name, query_branch).await?;
    let repo = app(cx).mirrors.repo(&name);
    let Some(bytes) = repo.show_file(&tip, &path).await? else {
        return Err(topcoat::router::error::not_found().into());
    };
    let source = String::from_utf8_lossy(&bytes).into_owned();
    let document = docs::parse_document(&path, &source);
    let branches = repo.branches().await.unwrap_or_default();
    let body_html = render::markdown(&document.body, &name, Some(&index), &branches);

    // The comment thread for this file, split into current and outdated.
    let filter = CommentFilter {
        repo: name.clone(),
        branch: Some(branch.clone()),
        file: Some(path.clone()),
        ..Default::default()
    };
    let comments = app(cx).db.comments(&filter).unwrap_or_default();
    let mut current: Vec<Comment> = Vec::new();
    let mut outdated: Vec<Comment> = Vec::new();
    for comment in comments {
        if comment.line.is_some()
            && comment.commit != tip
            && repo.path_changed(&comment.commit, &tip, &path).await.unwrap_or(false)
        {
            outdated.push(comment);
        } else {
            current.push(comment);
        }
    }

    // Back-links: what points here, and what this points at.
    let backlinks: Vec<Document> = index.backlinks_to(&path).into_iter().cloned().collect();
    let doc_branch = document.refs.branch.clone();
    let branch_ci = match &doc_branch {
        Some(b) => match repo.tip(b).await {
            Ok(tip) => Some((b.clone(), ci_for(cx, &name, &tip).await, true)),
            Err(_) => Some((b.clone(), None, false)),
        },
        None => None,
    };

    let raw_url = raw_url(&name, &branch, &path);
    let active = if root == docs::PLANS_DIR { "plans" } else { "board" };

    view! { cx =>
        shell(title: format!("{name} · {}", document.title), repo: name.clone(), active: active, status: Some(ctx.status.clone()),
            <div class="d-flex flex-items-center gap-2 mb-2">
                <h3 class="mb-0">
                    <i class=(if document.is_card() { "ph ph-kanban" } else { "ph ph-file-text" })></i>
                    " " (document.title.clone())
                </h3>
                if let Some(status_label) = &document.status {
                    <span class="Label Label--accent">(status_label.clone())</span>
                }
                if let Some(assignee) = &document.assignee {
                    <span class="Label">(format!("@{assignee}"))</span>
                }
                <a class="ml-auto Link--secondary text-small" href=(raw_url)>"raw"</a>
            </div>
            if let Some(error) = &document.front_matter_error {
                <div class="flash flash-error mb-2">
                    <i class="ph ph-warning"></i>" Front matter problem: " (error.clone())
                </div>
            }
            <div class="d-flex flex-items-center gap-2 mb-3 text-small">
                if let Some((b, ci, exists)) = &branch_ci {
                    <span class="color-fg-muted">"branch:"</span>
                    if *exists {
                        branch_label(repo: name.clone(), branch: b.clone())
                        ci_icon(run_status: ci.clone())
                    } else {
                        <span class="color-fg-muted">(b.clone()) <span class="Label">"missing"</span></span>
                    }
                }
                if let Some(plan_ref) = &document.refs.plan {
                    <span class="color-fg-muted">"plan:"</span>
                    doc_ref(repo: name.clone(), index: index.clone(), path: plan_ref.clone())
                }
                let n = &name;
                let idx = &index;
                for task in document.refs.tasks.clone() {
                    <span key=(task.clone()) class="color-fg-muted">"task:"</span>
                    doc_ref(key: format!("ref-{task}"), repo: n.clone(), index: idx.clone(), path: task.clone())
                }
            </div>
            <div class="Box mb-3">
                <div class="Box-body markdown-body">(Raw(body_html))</div>
            </div>
            if !backlinks.is_empty() {
                <h4 class="mb-2">"Referenced by"</h4>
                <div class="Box mb-3">
                    let n = &name;
                    for doc in backlinks {
                        <div key=(doc.path.clone()) class="Box-row d-flex flex-items-center gap-2">
                            <i class=(if doc.is_card() { "ph ph-kanban" } else { "ph ph-file-text" })></i>
                            <a href=(format!("/{n}/{}", doc.path))>(doc.title.clone())</a>
                            if let Some(status_label) = &doc.status {
                                <span class="Label">(status_label.clone())</span>
                            }
                        </div>
                    }
                </div>
            }
            <h4 class="mb-2"><i class="ph ph-chat-circle"></i>" Comments"</h4>
            <div class="Box">
                if current.is_empty() {
                    <div class="Box-row color-fg-muted">"No comments yet."</div>
                }
                let n = &name;
                for comment in current {
                    comment_block(key: comment.id, repo: n.clone(), comment: comment)
                }
                comment_composer(repo: name.clone(), branch: branch.clone(), file: Some(path.clone()))
            </div>
            if !outdated.is_empty() {
                <details class="mt-3">
                    <summary class="color-fg-muted">(format!("{} outdated comment(s)", outdated.len()))</summary>
                    <div class="Box mt-2">
                        let n2 = &name;
                        for comment in outdated {
                            comment_block(key: comment.id, repo: n2.clone(), comment: comment, outdated: true)
                        }
                    </div>
                </details>
            }
        )
    }
}

/// A declared ref: a link when the target exists, plain text with a "missing" marker
/// when it does not. A dangling ref never breaks the page.
#[component]
async fn doc_ref(
    #[into] repo: String,
    index: std::sync::Arc<DocIndex>,
    #[into] path: String,
) -> Result {
    let exists = index.exists(&path);
    let title = index.get(&path).map(|d| d.title.clone()).unwrap_or_else(|| path.clone());
    view! {
        if exists {
            <a href=(format!("/{repo}/{path}"))>(title)</a>
        } else {
            <span class="color-fg-muted">(path.clone()) " " <span class="Label">"missing"</span></span>
        }
    }
}

// ---- /{repo}/docs and /{repo}/docs/{*path} — the wiki ----------------------------

/// One level of the wiki sidebar. Directories nest; files are leaves.
#[derive(Default)]
struct WikiNode {
    dirs: BTreeMap<String, WikiNode>,
    /// `(display name, repo-relative path)`, alphabetical.
    files: Vec<(String, String)>,
}

impl WikiNode {
    /// Build the tree from every markdown path in the repo.
    fn of(paths: &[&str]) -> Self {
        let mut root = Self::default();
        for path in paths {
            let mut node = &mut root;
            let segments: Vec<&str> = path.split('/').collect();
            let (name, dirs) = segments.split_last().expect("a path has a last segment");
            for dir in dirs {
                node = node.dirs.entry((*dir).to_owned()).or_default();
            }
            node.files.push(((*name).to_owned(), (*path).to_owned()));
        }
        root
    }

    /// Does this subtree hold the page being read? Answers whether a `<details>` opens.
    fn holds(&self, path: &str) -> bool {
        self.files.iter().any(|(_, file)| file == path)
            || self.dirs.values().any(|dir| dir.holds(path))
    }
}

/// The sidebar, as HTML.
///
/// Recursion, not a component: a `#[component]` cannot call itself without boxing its
/// own future, and the markup is a plain nested list. Every value here is escaped on
/// the way in — the paths come from git, and git will carry any byte a filename can.
fn wiki_sidebar(repo: &str, node: &WikiNode, current: &str, pinned: Option<&str>) -> String {
    let mut html = String::new();
    if let Some(pinned) = pinned {
        html.push_str(&wiki_link(repo, pinned, pinned, current, "ph-push-pin"));
    }
    for (name, dir) in &node.dirs {
        html.push_str(&format!(
            "<details class=\"nashcode-wiki-dir\"{}><summary>{}</summary><div class=\"nashcode-wiki-children\">",
            if dir.holds(current) { " open=\"\"" } else { "" },
            render::escape_text(name)
        ));
        html.push_str(&wiki_sidebar(repo, dir, current, None));
        html.push_str("</div></details>");
    }
    for (name, path) in &node.files {
        if Some(path.as_str()) == pinned {
            continue;
        }
        html.push_str(&wiki_link(repo, name, path, current, "ph-file-text"));
    }
    html
}

fn wiki_link(repo: &str, label: &str, path: &str, current: &str, icon: &str) -> String {
    format!(
        "<a class=\"nashcode-wiki-link{}\" href=\"{}\"{}><i class=\"ph {icon}\"></i>{}</a>",
        if path == current { " is-current" } else { "" },
        render::escape_attr(&render::docs_url(repo, path)),
        if path == current { " aria-current=\"page\"" } else { "" },
        render::escape_text(label)
    )
}

#[page("/{repo}/docs")]
async fn repo_docs_home(cx: &Cx) -> Result {
    wiki_page(cx, None).await
}

#[page("/{repo}/docs/{*rest}")]
async fn repo_docs_page(cx: &Cx) -> Result {
    let path = join_rest(cx);
    wiki_page(cx, Some(path)).await
}

/// One wiki page, in the frame the whole wiki shares.
///
/// `None` asks for the home page: `docs/index.md` if the repo has one, else the root
/// README. The renderer is the plans renderer, so escaping and autolinking are
/// identical; only the relative-link pass is extra.
async fn wiki_page(cx: &Cx, requested: Option<String>) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "docs",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let repo = app(cx).mirrors.repo(&name);
    let Ok(branch) = repo.default_branch().await else {
        return view! { cx =>
            shell(title: format!("{name} · wiki"), repo: name.clone(), active: "docs", status: Some(ctx.status.clone()),
                <div class="Box"><div class="Box-body color-fg-muted">
                    "Nothing pushed here yet."
                </div></div>)
        };
    };
    let tip = repo.tip(&branch).await?;
    let index = app(cx).docs.get(&name, &repo, &tip).await;

    let pages = index.wiki_pages();
    // A path from the URL only reaches a markdown file that exists. Everything else
    // belongs to /blob/, which is a 404 away from here on purpose — except that
    // `docs/...` is also a perfectly ordinary branch name. Reserving `docs` was meant
    // to cost one branch name, not a whole namespace, so a request that names no wiki
    // page but does name a real branch falls through to that branch's PR view.
    let current = match &requested {
        Some(path) => {
            let known = safe_repo_path(path).filter(|p| pages.contains(&p.as_str()));
            match known {
                Some(path) => Some(path),
                None => {
                    let branch = format!("docs/{path}");
                    if repo.tip(&branch).await.is_ok() {
                        return branch_page(cx, &name, &branch).await;
                    }
                    return Err(topcoat::router::error::not_found().into());
                }
            }
        }
        None => index.wiki_home().map(str::to_owned),
    };

    let tree = WikiNode::of(&pages);
    let pinned = pages.contains(&docs::WIKI_PINNED).then_some(docs::WIKI_PINNED);
    let sidebar = wiki_sidebar(&name, &tree, current.as_deref().unwrap_or_default(), pinned);

    let article = match &current {
        Some(path) => {
            let source = repo
                .show_file(&tip, path)
                .await?
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            let document = docs::parse_document(path, &source);
            let branches = repo.branches().await.unwrap_or_default();
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            let body = render::markdown_in_docs(
                &document.body,
                &name,
                Some(&index),
                &branches,
                dir,
                &branch,
            );
            Some((path.clone(), document.title, body))
        }
        None => None,
    };

    let title = match &article {
        Some((_, doc_title, _)) => format!("{name} · {doc_title}"),
        None => format!("{name} · wiki"),
    };
    let empty_message = if pages.is_empty() {
        "No markdown in this repo yet. Every .md file it gains becomes a wiki page."
    } else {
        "No docs/index.md and no README at the root. Pick a page from the sidebar."
    };

    view! { cx =>
        shell(title: title, repo: name.clone(), active: "docs", status: Some(ctx.status.clone()),
            <div class="nashcode-wiki">
                <nav class="nashcode-wiki-nav Box" aria-label="Wiki pages">
                    <div class="Box-header d-flex flex-items-center gap-2">
                        <i class="ph ph-book-open"></i>
                        <a class="Link--primary no-underline nashcode-display" href=(format!("/{name}/docs"))>"Wiki"</a>
                        <span class="Counter">(pages.len())</span>
                    </div>
                    <div class="Box-body">
                        if pages.is_empty() {
                            <span class="color-fg-muted text-small">"No pages."</span>
                        }
                        (Raw(sidebar))
                    </div>
                </nav>
                <article class="nashcode-wiki-page">
                    match &article {
                        Some((path, doc_title, body)) => <div class="Box">
                            <div class="Box-header d-flex flex-items-center gap-2">
                                <i class="ph ph-file-text"></i>
                                <strong>(doc_title.clone())</strong>
                                <span class="ml-auto d-flex flex-items-center gap-2 text-small">
                                    <a class="Link--secondary" href=(render::blob_url(&name, path))>"source"</a>
                                    <a class="Link--secondary" href=(raw_url(&name, &branch, path))>"raw"</a>
                                </span>
                            </div>
                            <div class="Box-body markdown-body">(Raw(body.clone()))</div>
                        </div>,
                        None => <div class="Box"><div class="Box-body color-fg-muted">
                            (empty_message)
                        </div></div>,
                    }
                </article>
            </div>
        )
    }
}

// ---- /{repo}/board ---------------------------------------------------------------

#[page("/{repo}/board")]
async fn repo_board(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! {
            shell(title: name.clone(), repo: name.clone(), active: "board",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let (_branch, tip, index) = doc_view(cx, &name, None).await?;
    let repo = app(cx).mirrors.repo(&name);

    // Canonical columns always render (they are the drop targets); extras appear
    // when a card uses them.
    let mut statuses: Vec<String> =
        docs::CANONICAL_STATUSES.iter().map(|s| (*s).to_owned()).collect();
    for card in index.cards() {
        let column = card.column().to_owned();
        if !statuses.contains(&column) {
            statuses.push(column);
        }
    }
    let order = docs::order_columns(statuses);

    // Cards ordered newest-first by the last commit that touched them.
    type Column = (String, Vec<(Document, Option<String>)>);
    let mut columns: Vec<Column> = Vec::new();
    for column_name in &order {
        let mut cards: Vec<(String, Document)> = Vec::new();
        for card in index.cards() {
            if card.column() == column_name {
                let touched = repo
                    .last_touched(&tip, &card.path)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                cards.push((touched, (*card).clone()));
            }
        }
        cards.sort_by(|a, b| b.0.cmp(&a.0));
        let mut with_ci = Vec::new();
        for (_, card) in cards {
            let ci = match &card.refs.branch {
                Some(branch) => match repo.tip(branch).await {
                    Ok(branch_tip) => ci_for(cx, &name, &branch_tip).await,
                    Err(_) => None,
                },
                None => None,
            };
            with_ci.push((card, ci));
        }
        columns.push((column_name.clone(), with_ci));
    }

    view! {
        shell(title: format!("{name} · board"), repo: name.clone(), active: "board", status: Some(ctx.status.clone()),
            <h3 class="mb-2"><i class="ph ph-kanban"></i>" Board"</h3>
            <div class="nashcode-board" data-repo=(name.clone())>
                let n = &name;
                for (column_name, cards) in columns {
                    <div
                        key=(column_name.clone())
                        class="nashcode-board-column"
                        data-status=(column_name.clone())
                        data-nodrop=((column_name == docs::NEEDS_ATTENTION).then_some("true"))
                    >
                        <div class="nashcode-board-column-header">
                            <i class=(if column_name == docs::NEEDS_ATTENTION { "ph ph-warning" } else { "ph ph-kanban" })></i>
                            <strong>(column_name.clone())</strong>
                            <span class="Counter">(cards.len())</span>
                        </div>
                        <div class="nashcode-board-column-body">
                            for (card, ci) in cards {
                                <a
                                    key=(card.path.clone())
                                    class="nashcode-board-card"
                                    href=(format!("/{n}/{}", card.path))
                                    data-file=(card.path.clone())
                                >
                                    <div class="d-flex flex-items-center gap-2">
                                        <strong>(card.title.clone())</strong>
                                        if card.refs.branch.is_some() {
                                            ci_icon(run_status: ci.clone())
                                        }
                                    </div>
                                    <div class="color-fg-muted text-small">(card.path.clone())</div>
                                    if let Some(assignee) = &card.assignee {
                                        <span class="Label mt-1">(format!("@{assignee}"))</span>
                                    }
                                    if let Some(error) = &card.front_matter_error {
                                        <div class="color-fg-danger text-small mt-1">(error.clone())</div>
                                    }
                                </a>
                            }
                        </div>
                    </div>
                }
            </div>
        )
    }
}

// ---- /{repo}/{*rest}: branch PR view and CI log ----------------------------------

#[topcoat::router::query_params(error = bad_request)]
struct BranchQuery {
    run: Option<i64>,
    error: Option<String>,
}

#[page("/{repo}/{*rest}")]
async fn branch_catch_all(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let rest = join_rest(cx);
    let (branch, action) = split_action(&rest);
    match action {
        None => branch_page(cx, &name, &rest).await,
        Some("ci") => ci_log_page(cx, &name, branch).await,
        // Actions are POST; a GET just goes to the branch.
        Some(_) => Err(topcoat::router::error::redirect(format!("/{name}/{branch}")).into()),
    }
}

async fn branch_page(cx: &Cx, name: &str, branch: &str) -> Result {
    let ctx = repo_ctx(cx, name).await?;
    let name = name.to_owned();
    if !ctx.status.available {
        return view! { cx =>
            shell(title: name.clone(), repo: name.clone(), active: "code",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }
    let repo = app(cx).mirrors.repo(&name);
    let graph = StackGraph::infer(&repo).await?;
    let Some(node) = graph.get(branch).cloned() else {
        return Err(topcoat::router::error::not_found().into());
    };
    let branch = branch.to_owned();
    let is_default = branch == graph.default_branch;
    let error_banner = query_params::<BranchQuery>(cx)?.error.clone();

    // Commits unique to the branch (or the recent history for the default branch).
    let commits = match &node.parent {
        Some(parent) => {
            let parent_tip = graph.get(parent).map(|p| p.tip.clone()).unwrap_or_default();
            repo.commits(&parent_tip, &node.tip).await?
        }
        None => repo.recent_commits(&node.tip, 20).await.unwrap_or_default(),
    };

    // A commit whose trace is known links straight to the conversation that wrote it.
    let commits: Vec<(crate::git::Commit, Option<String>)> = {
        let mut out = Vec::new();
        for commit in commits {
            let session = app(cx)
                .db
                .trace_sessions_for_commit(&name, &commit.id)
                .unwrap_or_default()
                .into_iter()
                .next();
            out.push((commit, session));
        }
        out
    };

    // Per-file three-dot diffs against the merge base with the parent.
    let mut files: Vec<(String, String, String)> = Vec::new(); // (path, status, patch)
    if let Some(parent) = &node.parent {
        let parent_tip = graph.get(parent).map(|p| p.tip.clone()).unwrap_or_default();
        for changed in repo.changed_files(&parent_tip, &node.tip).await? {
            let patch = repo.file_diff(&parent_tip, &node.tip, &changed.path).await?;
            files.push((changed.path, changed.status, patch));
        }
    }

    // Comments: PR-level, per-file inline, outdated, and not-in-this-diff.
    let filter = CommentFilter {
        repo: name.clone(),
        branch: Some(branch.clone()),
        ..Default::default()
    };
    let all_comments = app(cx).db.comments(&filter).unwrap_or_default();
    let mut pr_level: Vec<Comment> = Vec::new();
    let mut by_file: BTreeMap<String, Vec<Comment>> = BTreeMap::new();
    let mut outdated: Vec<Comment> = Vec::new();
    let diff_paths: Vec<String> = files.iter().map(|(p, _, _)| p.clone()).collect();
    for comment in all_comments {
        match &comment.file {
            None => pr_level.push(comment),
            Some(file) => {
                let stale_anchor = comment.line.is_some()
                    && comment.commit != node.tip
                    && repo.path_changed(&comment.commit, &node.tip, file).await.unwrap_or(false);
                if stale_anchor {
                    outdated.push(comment);
                } else {
                    by_file.entry(file.clone()).or_default().push(comment);
                }
            }
        }
    }
    let mut other_files: Vec<(String, Vec<Comment>)> = Vec::new();
    for (file, list) in &by_file {
        if !diff_paths.contains(file) {
            other_files.push((file.clone(), list.clone()));
        }
    }

    // Diff payloads for @pierre/diffs, annotations included.
    let mut diff_blobs: Vec<(String, String, String, String)> = Vec::new(); // (mount, path, status, json)
    for (i, (path, file_status, patch)) in files.iter().enumerate() {
        let mount = format!("diff-{i}");
        let annotations: Vec<serde_json::Value> = by_file
            .get(path)
            .map(|list| annotation_payload(&name, list))
            .unwrap_or_default();
        let json = serde_json::json!({
            "mount": mount,
            "file": path,
            "patch": patch,
            "annotations": annotations,
        })
        .to_string();
        diff_blobs.push((mount, path.clone(), file_status.clone(), json));
    }

    // Stack banner and linked documents.
    let chain = graph.path_to(&branch);
    let children = node.children.clone();
    let parent = node.parent.clone();
    let ci = ci_for(cx, &name, &node.tip).await;
    let ci_blocks = status::blocks_merge(ci.as_deref());
    // Stuck is not failed: nothing ran this to a red result, so the escape is a
    // requeue, not the "merge despite CI" override.
    let ci_stuck = ci.as_deref() == Some(status::STUCK);

    let default_tip = graph
        .get(&graph.default_branch)
        .map(|n| n.tip.clone())
        .unwrap_or_default();
    let index = app(cx).docs.get(&name, &repo, &default_tip).await;
    let linked_docs: Vec<Document> =
        index.documents_for_branch(&branch).into_iter().cloned().collect();

    let title = format!("{name} · {branch}");
    view! { cx =>
        shell(title: title, repo: name.clone(), active: "code", status: Some(ctx.status.clone()),
            if let Some(error) = &error_banner {
                <div class="flash flash-error mb-3"><i class="ph ph-warning"></i>" "(error.clone())</div>
            }
            <div class="Box mb-3">
                <div class="Box-header d-flex flex-items-center gap-2 flex-wrap">
                    <i class="ph ph-git-branch"></i>
                    let n = &name;
                    for (i, step) in chain.iter().enumerate() {
                        if i > 0 {
                            <i key=(format!("sep-{i}")) class="ph ph-caret-right color-fg-muted"></i>
                        }
                        branch_label(key: step.clone(), repo: n.clone(), branch: step.clone())
                    }
                    ci_icon(run_status: ci.clone())
                    <a class="Link--secondary text-small" href=(format!("/{name}/{branch}/ci"))>"ci log"</a>
                    if ci_stuck {
                        <span class="text-small color-fg-attention">"CI stopped reporting"</span>
                        <form method="post" action=(format!("/{name}/{branch}/ci/requeue"))>
                            <button type="submit" class="btn btn-sm">
                                <i class="ph ph-arrow-counter-clockwise"></i>" Requeue"
                            </button>
                        </form>
                    }
                    if !is_default {
                        <form method="post" action=(format!("/{name}/{branch}/merge")) class="ml-auto d-flex flex-items-center gap-2">
                            <label class="text-small color-fg-muted">
                                <input type="checkbox" name="delete_branch" value="1"> " delete branch"
                            </label>
                            if ci_blocks {
                                <input type="hidden" name="allow_red" value="1">
                                <button type="submit" class="btn btn-danger"
                                        onclick="return confirm('CI is not green. Merge anyway?')">
                                    <i class="ph ph-git-merge"></i>
                                    (format!(" Merge into {} despite CI", parent.clone().unwrap_or_default()))
                                </button>
                            } else {
                                <button type="submit" class="btn btn-primary">
                                    <i class="ph ph-git-merge"></i>
                                    (format!(" Merge into {}", parent.clone().unwrap_or_default()))
                                </button>
                            }
                        </form>
                    }
                    if !children.is_empty() {
                        <form method="post" action=(format!("/{name}/{branch}/restack"))
                              class=(if is_default { "ml-auto" } else { "" })>
                            <button type="submit" class="btn">
                                <i class="ph ph-arrows-clockwise"></i>" Restack descendants"
                            </button>
                        </form>
                    }
                    if !is_default && children.is_empty() {
                        <form method="post" action=(format!("/{name}/{branch}/delete"))>
                            <button type="submit" class="btn btn-danger"
                                    // Static text only: a branch name interpolated into
                                    // inline JS could escape the string (apostrophes
                                    // survive attribute escaping).
                                    onclick="return confirm('Delete this branch on the git server?')">
                                <i class="ph ph-trash"></i>" Delete branch"
                            </button>
                        </form>
                    }
                </div>
                if !children.is_empty() {
                    <div class="Box-row text-small color-fg-muted d-flex gap-2 flex-items-center">
                        "Stacked on this branch:"
                        let n = &name;
                        for child in children {
                            branch_label(key: child.clone(), repo: n.clone(), branch: child.clone())
                        }
                    </div>
                }
                if !linked_docs.is_empty() {
                    <div class="Box-row text-small d-flex gap-2 flex-items-center">
                        <span class="color-fg-muted">"Linked:"</span>
                        let n = &name;
                        for doc in linked_docs {
                            <a key=(doc.path.clone()) href=(format!("/{n}/{}", doc.path))>
                                <i class=(if doc.is_card() { "ph ph-kanban" } else { "ph ph-file-text" })></i>
                                " " (doc.title.clone())
                            </a>
                            if let Some(status_label) = &doc.status {
                                <span key=(format!("st-{}", doc.path)) class="Label">(status_label.clone())</span>
                            }
                        }
                    </div>
                }
            </div>

            <h4 class="mb-2"><i class="ph ph-git-commit"></i>" Commits "<span class="Counter">(commits.len())</span></h4>
            <div class="Box mb-3">
                if commits.is_empty() {
                    <div class="Box-row color-fg-muted">"No commits beyond the parent."</div>
                }
                let n = &name;
                for (commit, trace_session) in commits {
                    <div key=(commit.id.clone()) class="Box-row d-flex flex-items-center gap-2">
                        <code class="commit-sha">(commit.short.clone())</code>
                        <span>(commit.subject.clone())</span>
                        if let Some(session) = &trace_session {
                            <a class="Link--secondary text-small" href=(format!("/{n}/agent/{session}"))>
                                <i class="ph ph-robot"></i>" trace"
                            </a>
                        }
                        <span class="ml-auto color-fg-muted text-small">
                            (format!("{} · {}", commit.author, commit.date))
                        </span>
                    </div>
                }
            </div>

            if !diff_blobs.is_empty() {
                <h4 class="mb-2"><i class="ph ph-file-text"></i>" Files changed "<span class="Counter">(diff_blobs.len())</span></h4>
                let n = &name;
                let b = &branch;
                for (mount, path, file_status, json) in diff_blobs {
                    <div key=(mount.clone()) class="Box mb-3">
                        <div class="Box-header d-flex flex-items-center gap-2">
                            <code class="commit-sha">(path.clone())</code>
                            <span class="Label">(file_status.clone())</span>
                        </div>
                        <div class="nashcode-diff-mount" id=(mount.clone())>
                            <pre class="p-3 text-small nashcode-code nashcode-diff-fallback">(diff_fallback(&json))</pre>
                        </div>
                        <script type="application/json" class="nashcode-diff-data">(Raw(escape_json_for_script(&json)))</script>
                        inline_comment_composer(key: format!("inline-{mount}"), repo: n.clone(), branch: b.clone(), file: path.clone())
                        comment_composer(key: format!("composer-{mount}"), repo: n.clone(), branch: b.clone(), file: Some(path.clone()))
                    </div>
                }
            }

            if !other_files.is_empty() {
                <h4 class="mb-2">"Comments on other files"</h4>
                <div class="Box mb-3">
                    let n = &name;
                    for (file, list) in other_files {
                        <div key=(file.clone()) class="Box-row color-fg-muted text-small">(file.clone())</div>
                        for comment in list {
                            comment_block(key: format!("c-{}", comment.id), repo: n.clone(), comment: comment)
                        }
                    }
                </div>
            }

            <h4 class="mb-2"><i class="ph ph-chat-circle"></i>" Discussion"</h4>
            <div class="Box mb-3">
                if pr_level.is_empty() {
                    <div class="Box-row color-fg-muted">"No comments yet."</div>
                }
                let n = &name;
                for comment in pr_level {
                    comment_block(key: comment.id, repo: n.clone(), comment: comment)
                }
                comment_composer(repo: name.clone(), branch: branch.clone())
            </div>

            if !outdated.is_empty() {
                <details>
                    <summary class="color-fg-muted">(format!("{} outdated comment(s)", outdated.len()))</summary>
                    <div class="Box mt-2">
                        let n2 = &name;
                        for comment in outdated {
                            comment_block(key: comment.id, repo: n2.clone(), comment: comment, outdated: true)
                        }
                    </div>
                </details>
            }
        )
    }
}

/// The `<pre>` fallback body: the raw patch, shown until (or without) JS.
fn diff_fallback(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v["patch"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// `</script>` inside a JSON blob would end the tag early; escape the slash.
fn escape_json_for_script(json: &str) -> String {
    json.replace('<', "\\u003c")
}

/// Group line comments into @pierre/diffs `DiffLineAnnotation`s (new side).
fn annotation_payload(repo: &str, comments: &[Comment]) -> Vec<serde_json::Value> {
    let mut by_line: BTreeMap<i64, String> = BTreeMap::new();
    for comment in comments {
        let Some(line) = comment.line else { continue };
        let body = render::markdown(&comment.body, repo, None, &[]);
        let block = format!(
            "<div class=\"nashcode-annotation-comment\">\
             <strong>{}</strong> <span class=\"color-fg-muted\">{}</span>\
             <div class=\"markdown-body\">{}</div></div>",
            render::escape_text(&comment.author),
            render::escape_text(&comment.created_at),
            body,
        );
        by_line.entry(line).or_default().push_str(&block);
    }
    by_line
        .into_iter()
        .map(|(line, html)| {
            serde_json::json!({
                "side": "additions",
                "lineNumber": line,
                "metadata": { "html": html },
            })
        })
        .collect()
}

async fn ci_log_page(cx: &Cx, name: &str, branch: &str) -> Result {
    let ctx = repo_ctx(cx, name).await?;
    let name = name.to_owned();
    let branch = branch.to_owned();
    let app = app(cx);

    let requested = query_params::<BranchQuery>(cx)?.run;
    let runs = app.db.recent_runs(&name, 100).unwrap_or_default();
    let run: Option<CiRun> = match requested {
        Some(id) => runs.iter().find(|r| r.id == id).cloned(),
        None => runs.iter().find(|r| r.branch == branch).cloned(),
    };

    let log = run
        .as_ref()
        .and_then(|r| r.log_path.as_ref())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|raw| strip_ansi(&raw));
    let effective = run.as_ref().map(|r| r.effective_status().to_owned());
    let stuck = effective.as_deref() == Some(status::STUCK);
    // A run that ended without producing a log still owes the reader a reason.
    let no_log = run
        .as_ref()
        .map(|r| {
            if r.note.is_empty() { "This run produced no log.".to_owned() } else { r.note.clone() }
        })
        .unwrap_or_default();

    view! { cx =>
        shell(title: format!("{name} · {branch} · ci"), repo: name.clone(), active: "ci", status: Some(ctx.status.clone()),
            <div class="d-flex flex-items-center gap-2 mb-3">
                <h3 class="mb-0"><i class="ph ph-play"></i>" CI · "</h3>
                branch_label(repo: name.clone(), branch: branch.clone())
                if let Some(run) = &run {
                    ci_icon(run_status: effective.clone())
                    <code class="commit-sha">(run.commit.chars().take(8).collect::<String>())</code>
                    <span class="color-fg-muted text-small">(format!("{} · {}ms", run.created_at, run.duration_ms))</span>
                }
                <form method="post" action=(format!("/{name}/{branch}/ci/rerun")) class="ml-auto">
                    <button type="submit" class="btn">
                        <i class="ph ph-arrow-counter-clockwise"></i>" Re-run"
                    </button>
                </form>
                if stuck {
                    <form method="post" action=(format!("/{name}/{branch}/ci/requeue"))>
                        <button type="submit" class="btn btn-primary">
                            <i class="ph ph-arrow-counter-clockwise"></i>" Requeue this run"
                        </button>
                    </form>
                }
            </div>
            <div class="Box">
                match (&run, &log) {
                    (None, _) => <div class="Box-body color-fg-muted">"No CI run recorded for this branch yet."</div>,
                    (Some(_), None) => <div class="Box-body color-fg-muted">(no_log.clone())</div>,
                    (Some(_), Some(log)) => <pre class="Box-body nashcode-ci-log text-small">(log.clone())</pre>,
                }
            </div>
        )
    }
}
