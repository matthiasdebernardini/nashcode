//! Every HTML page. Data is assembled first, then rendered with Primer markup.

use std::collections::BTreeMap;

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{page, path_param, query_params};
use topcoat::view::{View, component, view};

use crate::ci::strip_ansi;
use crate::db::{CiRun, Comment, CommentFilter, status};
use crate::docs::{self, DocIndex, Document};
use crate::render;
use crate::stack::StackGraph;
use crate::web::components::{
    Raw, StackRow, branch_label, ci_icon, comment_block, comment_composer, shell, stack_column,
    unavailable_card,
};
use crate::web::{app, repo_ctx};

path_param!(repo);
path_param!(*rest);

/// Action suffixes parsed off the branch catch-all. A branch name may not end with one.
const ACTION_SUFFIXES: [&str; 5] = ["ci/rerun", "ci", "merge", "restack", "delete"];

/// Split `rest` into `(branch, action)`.
pub fn split_action(rest: &str) -> (&str, Option<&str>) {
    for action in ACTION_SUFFIXES {
        if let Some(branch) = rest.strip_suffix(action) {
            if let Some(branch) = branch.strip_suffix('/')
                && !branch.is_empty()
            {
                return (branch, Some(action));
            }
        }
    }
    (rest, None)
}

fn join_rest(cx: &Cx) -> String {
    path_param::<Rest>(cx).collect::<Vec<_>>().join("/")
}

async fn ci_for(cx: &Cx, repo: &str, tip: &str) -> Option<String> {
    app(cx).db.latest_run(repo, tip).ok().flatten().map(|run| run.status)
}

// ---- / ---------------------------------------------------------------------------

#[page("/")]
async fn home(cx: &Cx) -> Result {
    let app = app(cx);
    let mut sections: Vec<View> = Vec::new();
    for name in app.config.repos.clone() {
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
                        <a class="Link--primary no-underline h4 nashgit-display" href=(format!("/{name_view}"))>
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
                    "No repos configured. Set NASHGIT_REPOS."
                </div></div>
            }
            for (i, section) in sections.into_iter().enumerate() {
                <div key=(i)>(section)</div>
            }
        )
    }
}

// ---- /{repo} — the Code tab ------------------------------------------------------

#[page("/{repo}")]
async fn repo_code(cx: &Cx) -> Result {
    let name = path_param::<Repo>(cx).to_owned();
    let ctx = repo_ctx(cx, &name).await?;
    if !ctx.status.available {
        return view! {
            shell(title: name.clone(), repo: name.clone(), active: "code",
                unavailable_card(repo: name.clone(), status: ctx.status.clone()))
        };
    }

    let graph = StackGraph::infer(&app(cx).mirrors.repo(&name)).await?;
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

    view! {
        shell(title: name.clone(), repo: name.clone(), active: "code", status: Some(ctx.status.clone()),
            <div class="Box">
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
        )
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
    let audit = app(cx).db.audit(&name, 50).unwrap_or_default();

    view! {
        shell(title: format!("{name} · stacks"), repo: name.clone(), active: "stacks", status: Some(ctx.status.clone()),
            <h3 class="mb-2"><i class="ph ph-stack"></i>" Stacks"</h3>
            <div class="d-flex flex-wrap gap-3 mb-4">
                let n = &name;
                for (i, chain) in chains.into_iter().enumerate() {
                    stack_column(key: i, repo: n.clone(), chain: chain)
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
                        <strong class="nashgit-display">(entry.actor.clone())</strong>
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
    view! {
        <div class="Box-row d-flex flex-items-center gap-2">
            ci_icon(run_status: Some(run.status.clone()))
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

    let raw_url = format!("/{name}/raw/{branch}/{path}");
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
                comment_composer(repo: name.clone(), branch: branch.clone(), file: Some(path.clone()), with_line: true)
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
    let mut columns: Vec<(String, Vec<(Document, Option<String>)>)> = Vec::new();
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
            <div class="nashgit-board" data-repo=(name.clone())>
                let n = &name;
                for (column_name, cards) in columns {
                    <div
                        key=(column_name.clone())
                        class="nashgit-board-column"
                        data-status=(column_name.clone())
                        data-nodrop=((column_name == docs::NEEDS_ATTENTION).then_some("true"))
                    >
                        <div class="nashgit-board-column-header">
                            <i class=(if column_name == docs::NEEDS_ATTENTION { "ph ph-warning" } else { "ph ph-kanban" })></i>
                            <strong>(column_name.clone())</strong>
                            <span class="Counter">(cards.len())</span>
                        </div>
                        <div class="nashgit-board-column-body">
                            for (card, ci) in cards {
                                <a
                                    key=(card.path.clone())
                                    class="nashgit-board-card"
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
                            <a class="Link--secondary text-small" href=(format!("/{n}/traces/{session}"))>
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
                        <div class="nashgit-diff-mount" id=(mount.clone())>
                            <pre class="p-3 text-small nashgit-code nashgit-diff-fallback">(diff_fallback(&json))</pre>
                        </div>
                        <script type="application/json" class="nashgit-diff-data">(Raw(escape_json_for_script(&json)))</script>
                        comment_composer(key: format!("composer-{mount}"), repo: n.clone(), branch: b.clone(), file: Some(path.clone()), with_line: true)
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
            "<div class=\"nashgit-annotation-comment\">\
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

    view! { cx =>
        shell(title: format!("{name} · {branch} · ci"), repo: name.clone(), active: "ci", status: Some(ctx.status.clone()),
            <div class="d-flex flex-items-center gap-2 mb-3">
                <h3 class="mb-0"><i class="ph ph-play"></i>" CI · "</h3>
                branch_label(repo: name.clone(), branch: branch.clone())
                if let Some(run) = &run {
                    ci_icon(run_status: Some(run.status.clone()))
                    <code class="commit-sha">(run.commit.chars().take(8).collect::<String>())</code>
                    <span class="color-fg-muted text-small">(format!("{} · {}ms", run.created_at, run.duration_ms))</span>
                }
                <form method="post" action=(format!("/{name}/{branch}/ci/rerun")) class="ml-auto">
                    <button type="submit" class="btn">
                        <i class="ph ph-arrow-counter-clockwise"></i>" Re-run"
                    </button>
                </form>
            </div>
            <div class="Box">
                match (&run, &log) {
                    (None, _) => <div class="Box-body color-fg-muted">"No CI run recorded for this branch yet."</div>,
                    (Some(_), None) => <div class="Box-body color-fg-muted">"This run produced no log."</div>,
                    (Some(_), Some(log)) => <pre class="Box-body nashgit-ci-log text-small">(log.clone())</pre>,
                }
            </div>
        )
    }
}
