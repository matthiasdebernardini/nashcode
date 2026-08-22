//! Shared markup: the page shell, the repo header with its tab nav, CI status icons,
//! stale banners, and comment blocks. All Primer classes; Phosphor supplies the icons.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::view::{NodeViewParts, PartsWriter, View, component, view};

use crate::db::{Comment, status};
use crate::mirror::MirrorStatus;
use crate::render;
use crate::web;

/// Trusted, already-escaped HTML (rendered markdown, server-built annotations).
pub struct Raw(pub String);

impl NodeViewParts for Raw {
    fn into_view_parts(self, _cx: &Cx, parts: &mut PartsWriter<'_>) {
        parts.push_string_unescaped(self.0);
    }
}

/// The document chrome every page renders into.
#[component]
pub async fn shell(
    #[into] title: String,
    /// The repo whose header and tabs to show; empty for repo-less pages.
    #[default(String::new())]
    #[into]
    repo: String,
    #[default] active: &'static str,
    #[default] status: Option<MirrorStatus>,
    child: View,
) -> Result {
    let repo = (!repo.is_empty()).then_some(repo);
    view! {
        <!DOCTYPE html>
        <html lang="en" data-color-mode="auto" data-light-theme="light" data-dark-theme="dark">
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <title>(format!("{title} · nashcode"))</title>
                <link rel="icon" type="image/svg+xml" href="/favicon.svg">
                <link rel="stylesheet" href=(web::css_url())>
                <script type="module" src=(web::js_url())></script>
            </head>
            <body>
                <header class="color-bg-inset border-bottom">
                    <div class="container-lg px-3 py-2 d-flex flex-items-center gap-2">
                        <a href="/" class="Link--primary no-underline h4 nashcode-display">
                            <i class="ph ph-git-branch"></i>
                            " nashcode"
                        </a>
                        if let Some(repo) = &repo {
                            <span class="color-fg-muted">"/"</span>
                            <a href=(format!("/{repo}")) class="Link--primary no-underline h4 nashcode-display">
                                (repo)
                            </a>
                        }
                    </div>
                    if let Some(repo) = &repo {
                        repo_tabs(repo: repo.clone(), active: active)
                    }
                </header>
                <main class="container-lg px-3 py-3">
                    if let Some(status) = &status {
                        stale_banner(status: status.clone())
                    }
                    (child)
                </main>
            </body>
        </html>
    }
}

/// GitHub-style tab nav for a repo: Code / Docs / Stacks / Stack / Plans / Board / CI /
/// Agent / Architecture.
#[component]
pub async fn repo_tabs(#[into] repo: String, active: &'static str) -> Result {
    let tabs: [(&str, &str, &str, String); 9] = [
        ("code", "Code", "ph-code", format!("/{repo}")),
        // The wiki reads the repo's own markdown, so it sits beside the code it
        // describes rather than off with the planning tabs.
        ("docs", "Docs", "ph-book-open", format!("/{repo}/docs")),
        ("stacks", "Stacks", "ph-stack", format!("/{repo}/stacks")),
        // Two tabs, one word, nothing in common: "Stacks" is branch stacks, "Stack" is
        // the upstream dependency column. They sit side by side and each page says in
        // one line what the other one is.
        ("stack", "Stack", "ph-cube", format!("/{repo}/stack")),
        ("plans", "Plans", "ph-file-text", format!("/{repo}/plans")),
        ("board", "Board", "ph-kanban", format!("/{repo}/board")),
        ("ci", "CI", "ph-play", format!("/{repo}/ci")),
        // Traces and Prompts were two tabs over the same sessions. One tab reads them
        // as the conversation they were.
        ("agent", "Agent", "ph-robot", format!("/{repo}/agent")),
        ("architecture", "Architecture", "ph-graph", format!("/{repo}/architecture")),
    ];
    view! {
        <nav class="UnderlineNav container-lg px-3" aria-label="Repository">
            <div class="UnderlineNav-body">
                for (key, label, icon, href) in tabs {
                    <a
                        class="UnderlineNav-item"
                        href=(href)
                        aria-current=((key == active).then_some("page"))
                    >
                        <i class=(format!("ph {icon}"))></i>
                        " " (label)
                    </a>
                }
            </div>
        </nav>
    }
}

/// The degrade-don't-fail banner: content is real but possibly behind.
#[component]
pub async fn stale_banner(status: MirrorStatus) -> Result {
    view! {
        if status.stale && status.available {
            <div class="flash flash-warn mb-3">
                <i class="ph ph-warning"></i>
                " Showing the last mirrored state — the git server is unreachable."
                if let Some(fetched) = &status.last_fetched {
                    <span class="color-fg-muted">(format!(" Last fetched {fetched}."))</span>
                }
                if let Some(message) = &status.message {
                    <p class="mb-0 text-small color-fg-muted">(message)</p>
                }
            </div>
        }
    }
}

/// The error card for a repo that has no mirror yet. Never a 500.
#[component]
pub async fn unavailable_card(#[into] repo: String, status: MirrorStatus) -> Result {
    view! {
        <div class="Box color-border-danger-emphasis">
            <div class="Box-body">
                <h3 class="mb-1"><i class="ph ph-warning"></i>" " (repo) " has no mirror yet"</h3>
                <p class="color-fg-muted mb-0">
                    "The first clone from the git server has not succeeded. "
                    "Pages will fill in once it is reachable."
                </p>
                if let Some(message) = &status.message {
                    <pre class="mt-2 text-small color-fg-danger">(message)</pre>
                }
            </div>
        </div>
    }
}

/// One Phosphor icon per CI status, colored with Primer tokens.
#[component]
pub async fn ci_icon(#[default] run_status: Option<String>) -> Result {
    let (icon, color, label) = match run_status.as_deref() {
        Some(status::PASSED) => ("ph-check-circle", "color-fg-success", "passed"),
        Some(status::FAILED) | Some(status::ERROR) => ("ph-x-circle", "color-fg-danger", "failed"),
        Some(status::TIMEOUT) => ("ph-x-circle", "color-fg-danger", "timed out"),
        Some(status::RUNNING) => ("ph-hourglass", "color-fg-attention", "running"),
        // Not a result: nothing is executing this run, and nothing will.
        Some(status::STUCK) => ("ph-plugs", "color-fg-attention", "stuck, no heartbeat"),
        Some(status::QUEUED) => ("ph-clock", "color-fg-attention", "queued"),
        Some(status::SKIPPED) => ("ph-minus-circle", "color-fg-muted", "skipped"),
        _ => ("ph-minus-circle", "color-fg-subtle", "never ran"),
    };
    view! {
        <i class=(format!("ph {icon} {color}")) title=(format!("CI: {label}")) aria-label=(format!("CI: {label}"))></i>
    }
}

/// A branch name pill linking to its PR view.
#[component]
pub async fn branch_label(#[into] repo: String, #[into] branch: String) -> Result {
    view! {
        <a class="branch-name no-underline" href=(render::branch_url(&repo, &branch))>(branch)</a>
    }
}

/// One rendered comment, with a delete button when the viewer wrote it.
#[component]
pub async fn comment_block(
    cx: &Cx,
    #[into] repo: String,
    comment: Comment,
    #[default] outdated: bool,
) -> Result {
    let viewer = web::actor(cx);
    let body = render::markdown(&comment.body, &repo, None, &[]);
    view! {
        <div class="Box-row" id=(format!("comment-{}", comment.id))>
            <div class="d-flex flex-items-center gap-2 mb-1">
                <i class="ph ph-chat-circle color-fg-muted"></i>
                <strong class="nashcode-display">(comment.display_author())</strong>
                <span class="color-fg-muted text-small">(comment.created_at.clone())</span>
                if let Some(line) = comment.line {
                    <span class="Label Label--secondary">(format!("line {line}"))</span>
                }
                if outdated {
                    <span class="Label Label--attention">"outdated"</span>
                }
                if viewer.login == comment.author {
                    <form
                        method="post"
                        action=(format!("/{repo}/comments/{}/delete", comment.id))
                        class="ml-auto"
                    >
                        <button type="submit" class="btn-octicon btn-octicon-danger" aria-label="Delete comment">
                            <i class="ph ph-trash"></i>
                        </button>
                    </form>
                }
            </div>
            <div class="markdown-body nashcode-annotation-body">(Raw(body))</div>
        </div>
    }
}

/// The composer that posts a comment. No `file` means a PR-level comment; with a
/// `file` it is the whole-file composer. Line anchors come from clicking a diff line,
/// never from typing a number.
#[component]
pub async fn comment_composer(
    #[into] repo: String,
    #[into] branch: String,
    #[default] file: Option<String>,
) -> Result {
    view! {
        <form method="post" action=(format!("/{repo}/comments"))
              class="nashcode-composer Box-row">
            <input type="hidden" name="branch" value=(branch)>
            if let Some(file) = &file {
                <input type="hidden" name="file" value=(file)>
            }
            <textarea name="body" class="form-control input-block" placeholder="Leave a comment (markdown)" required=""></textarea>
            <div class="d-flex flex-items-center gap-2">
                <button type="submit" class="btn btn-primary ml-auto">"Comment"</button>
            </div>
        </form>
    }
}

/// The composer the browser clones when a diff line is clicked, kept inert in a
/// `<template>` until then. Same action and same fields as the file-level composer,
/// plus the hidden `line` the click fills in — so the server needs no new endpoint.
#[component]
pub async fn inline_comment_composer(
    #[into] repo: String,
    #[into] branch: String,
    #[into] file: String,
) -> Result {
    view! {
        <template class="nashcode-inline-composer-template">
            <form method="post" action=(format!("/{repo}/comments"))
                  class="nashcode-composer nashcode-inline-composer">
                <input type="hidden" name="branch" value=(branch)>
                <input type="hidden" name="file" value=(file)>
                <input type="hidden" name="line" value="">
                <div class="d-flex flex-items-center gap-2 text-small color-fg-muted">
                    <i class="ph ph-chat-circle"></i>
                    <span class="nashcode-inline-composer-target"></span>
                </div>
                <textarea name="body" class="form-control input-block" placeholder="Leave a comment (markdown)" required=""></textarea>
                <div class="d-flex flex-items-center gap-2">
                    <button type="button" class="btn nashcode-inline-composer-cancel">"Cancel"</button>
                    <button type="submit" class="btn btn-primary ml-auto">"Comment"</button>
                </div>
            </form>
        </template>
    }
}

/// A Graphite-style stack column: default branch at the top, one row per branch.
#[component]
pub async fn stack_column(
    #[into] repo: String,
    chain: Vec<StackRow>,
    #[default] current: Option<String>,
) -> Result {
    view! {
        <div class="nashcode-stack-graph Box p-2">
            let repo_ref = &repo;
            let current_ref = &current;
            for row in chain {
                <div class=(format!(
                    "nashcode-stack-row py-1 d-flex flex-items-center gap-2{}",
                    if current_ref.as_deref() == Some(row.branch.as_str()) { " is-current" } else { "" }
                ))>
                    branch_label(key: row.branch.clone(), repo: repo_ref.clone(), branch: row.branch.clone())
                    if row.ahead > 0 {
                        <span class="Counter">(format!("+{}", row.ahead))</span>
                    }
                    ci_icon(key: row.branch.clone(), run_status: row.ci.clone())
                </div>
            }
        </div>
    }
}

/// What one stack row shows. Assembled by pages from the graph + SQLite.
#[derive(Debug, Clone)]
pub struct StackRow {
    pub branch: String,
    pub ahead: usize,
    pub ci: Option<String>,
}
