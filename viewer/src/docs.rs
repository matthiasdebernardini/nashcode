//! Plans and cards: markdown files in the repo with YAML front matter.
//!
//! Everything here is file-native. A plan is a markdown file under `plans/`, a card is
//! one under `tasks/`, and the links between them and the branches live in front
//! matter. Nothing about a document or a link is stored in SQLite, so an agent that
//! edits a file and pushes has changed the board and the link graph by doing so.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::git::{GitResult, Repo};
use crate::render::is_markdown_path;

pub const PLANS_DIR: &str = "plans";
pub const TASKS_DIR: &str = "tasks";

/// The wiki's home page when the repo has one.
pub const WIKI_INDEX: &str = "docs/index.md";

/// The contract agents load before they touch anything. When a repo has one it sits at
/// the top of the wiki sidebar, because it is the page a person re-reads most.
pub const WIKI_PINNED: &str = "lat.md";

/// The column a card with unreadable front matter falls into. Never a real status.
pub const NEEDS_ATTENTION: &str = "needs-attention";

/// The canonical columns, in order. Anything else sorts after them alphabetically.
pub const CANONICAL_STATUSES: [&str; 3] = ["todo", "doing", "done"];

/// The status alphabet, one rule for both doors: a git push (via [`parse_document`])
/// and the board's move endpoint. The caller trims and lowercases first;
/// [`NEEDS_ATTENTION`] is reserved for the parser, so a card can never claim it.
pub fn valid_status(status: &str) -> bool {
    !status.is_empty()
        && status.len() <= 40
        && status != NEEDS_ATTENTION
        && status.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The refs a document declares about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Refs {
    /// `branch: <name>` — the branch this document is about.
    pub branch: Option<String>,
    /// `plan: plans/x.md` — the plan this card implements.
    pub plan: Option<String>,
    /// `tasks: [tasks/a.md, ...]` — the cards this plan is broken into.
    pub tasks: Vec<String>,
}

/// A declared ref whose target is not there: a `branch:` naming no branch, or a
/// `plan:`/`tasks:` path that is in no tree.
///
/// The link site already renders one of these as "missing", but only if somebody opens
/// that page. Collecting them per repo makes the whole set answerable at once.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Dangling {
    /// The document that declares the ref.
    pub from: String,
    /// Which front-matter key it came from: `branch`, `plan`, or `tasks`.
    pub key: String,
    /// The branch name or path that resolves to nothing.
    pub target: String,
}

/// One plan or card, as read from the mirror.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Document {
    /// Repo-relative path, e.g. `plans/api.md`.
    pub path: String,
    pub title: String,
    /// `None` for a plan; `Some` for a card (possibly [`NEEDS_ATTENTION`]).
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub refs: Refs,
    /// The markdown body with the front matter removed.
    pub body: String,
    /// The first paragraph of the body, for list and API summaries.
    pub summary: String,
    /// Set when the front matter was present but unreadable.
    pub front_matter_error: Option<String>,
}

impl Document {
    pub fn is_card(&self) -> bool {
        self.status.is_some()
    }

    /// The card's column. Cards whose front matter would not parse are quarantined
    /// rather than dropped, so a bad file is visible instead of silently missing.
    pub fn column(&self) -> &str {
        match (&self.front_matter_error, &self.status) {
            (Some(_), _) => NEEDS_ATTENTION,
            (None, Some(status)) => status,
            (None, None) => NEEDS_ATTENTION,
        }
    }
}

/// The front-matter fields we understand. Everything else in the block is preserved on
/// disk and ignored here.
#[derive(Debug, Default, serde::Deserialize)]
struct FrontMatter {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    tasks: Option<Tasks>,
}

/// `tasks:` accepts one path or a list of them.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum Tasks {
    One(String),
    Many(Vec<String>),
}

impl Tasks {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(one) => vec![one],
            Self::Many(many) => many,
        }
    }
}

/// Byte bounds of the front-matter block in `source`:
/// `(front_start, front_end, body_start)`, where `front_start..front_end` is the text
/// between the fences (possibly empty) and `body_start..` is the body.
///
/// Rewrites must work on these offsets — searching for the block's *content* finds the
/// wrong place whenever that content also appears earlier (or is empty).
fn front_matter_bounds(source: &str) -> Option<(usize, usize, usize)> {
    // Tolerate a UTF-8 byte-order mark, which editors on other platforms leave behind.
    let bom = if source.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
    let text = &source[bom..];
    let open = if text.starts_with("---\n") {
        4
    } else if text.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let front_start = bom + open;

    // Find the closing fence at the start of a line.
    let rest = &source[front_start..];
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" || trimmed == "..." {
            let front_end = front_start + offset;
            return Some((front_start, front_end, front_end + line.len()));
        }
        offset += line.len();
    }
    // An unterminated fence is not front matter.
    None
}

/// Split a document into its front-matter block and its body.
///
/// The block is the text between a leading `---` line and the next `---` line. A file
/// without one is all body.
pub fn split_front_matter(source: &str) -> (Option<&str>, &str) {
    match front_matter_bounds(source) {
        Some((front_start, front_end, body_start)) => {
            (Some(&source[front_start..front_end]), &source[body_start..])
        }
        None => (None, source.strip_prefix('\u{feff}').unwrap_or(source)),
    }
}

/// Parse one markdown file into a document.
pub fn parse_document(path: &str, source: &str) -> Document {
    let (front, body) = split_front_matter(source);

    let (matter, error) = match front {
        None => (FrontMatter::default(), None),
        Some(block) if block.trim().is_empty() => (FrontMatter::default(), None),
        Some(block) => match serde_yaml_bw::from_str::<FrontMatter>(block) {
            Ok(matter) => (matter, None),
            Err(problem) => (FrontMatter::default(), Some(problem.to_string())),
        },
    };

    let title = matter
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| first_heading(body))
        .unwrap_or_else(|| file_stem(path));

    // A card is any file under `tasks/`. One without a status is as broken as one with
    // unreadable front matter, so it lands in the same column.
    let is_card = path.starts_with(&format!("{TASKS_DIR}/"));
    let status = if is_card {
        Some(
            matter
                .status
                .clone()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| NEEDS_ATTENTION.to_owned()),
        )
    } else {
        None
    };

    // A status outside the alphabet is as broken as unreadable front matter: quarantine
    // it rather than let a pushed file invent a column or squat on `needs-attention`.
    let error = match (&error, is_card, &matter.status) {
        (Some(_), _, _) => error,
        (None, true, None) => Some("card has no `status` in its front matter".to_owned()),
        (None, true, Some(_)) if !status.as_deref().is_some_and(valid_status) => {
            Some("invalid status".to_owned())
        }
        _ => None,
    };

    Document {
        path: path.to_owned(),
        title,
        status,
        assignee: matter.assignee.clone(),
        refs: Refs {
            branch: matter.branch.clone().filter(|b| !b.trim().is_empty()),
            plan: matter.plan.clone().filter(|p| !p.trim().is_empty()),
            tasks: matter.tasks.map(Tasks::into_vec).unwrap_or_default(),
        },
        summary: first_paragraph(body),
        body: body.to_owned(),
        front_matter_error: error,
    }
}

fn file_stem(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".md")
        .trim_end_matches(".markdown")
        .to_owned()
}

fn first_heading(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| line.starts_with('#'))
        .map(|line| line.trim_start_matches('#').trim().to_owned())
        .filter(|title| !title.is_empty())
}

fn first_paragraph(body: &str) -> String {
    let mut paragraph = String::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if paragraph.is_empty() {
                continue;
            }
            break;
        }
        if trimmed.starts_with('#') || trimmed.starts_with("```") {
            if paragraph.is_empty() {
                continue;
            }
            break;
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(trimmed);
    }
    paragraph
}

/// Rewrite only the `status:` line inside a document's front matter.
///
/// Everything else in the file, byte for byte, is left alone. That matters: a card is a
/// human-edited document, and a move must not reformat it.
pub fn rewrite_status(source: &str, new_status: &str) -> Option<String> {
    let (start, end, _) = front_matter_bounds(source)?;
    let front = &source[start..end];

    let mut rewritten = String::with_capacity(front.len() + new_status.len());
    let mut replaced = false;
    for line in front.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        if !replaced && body.trim_start().starts_with("status:") {
            let indent = &body[..body.len() - body.trim_start().len()];
            let newline = &line[body.len()..];
            rewritten.push_str(&format!("{indent}status: {new_status}{newline}"));
            replaced = true;
        } else {
            rewritten.push_str(line);
        }
    }

    if !replaced {
        // No status line to replace: add one, keeping the block otherwise intact.
        if !rewritten.is_empty() && !rewritten.ends_with('\n') {
            rewritten.push('\n');
        }
        rewritten.push_str(&format!("status: {new_status}\n"));
    }

    Some(format!("{}{}{}", &source[..start], rewritten, &source[end..]))
}

/// Order the board's columns: the canonical three first, then anything else
/// alphabetically, with the quarantine column last.
pub fn order_columns(statuses: impl IntoIterator<Item = String>) -> Vec<String> {
    let present: BTreeSet<String> = statuses.into_iter().collect();
    let mut columns: Vec<String> = CANONICAL_STATUSES
        .iter()
        .map(|s| (*s).to_owned())
        .filter(|s| present.contains(s))
        .collect();
    columns.extend(
        present
            .iter()
            .filter(|s| !CANONICAL_STATUSES.contains(&s.as_str()) && *s != NEEDS_ATTENTION)
            .cloned(),
    );
    if present.contains(NEEDS_ATTENTION) {
        columns.push(NEEDS_ATTENTION.to_owned());
    }
    columns
}

// ---- the link index --------------------------------------------------------------

/// Every document in a repo at one commit, plus the back-links derived from them.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DocIndex {
    /// The commit this index describes.
    pub commit: String,
    /// Plans and cards, keyed by path.
    pub documents: BTreeMap<String, Document>,
    /// Every path in the repo, for autolinking.
    pub all_paths: BTreeSet<String>,
    /// Branch name -> documents declaring `branch: <name>`.
    pub by_branch: BTreeMap<String, Vec<String>>,
    /// Plan path -> documents referencing it through `plan:` or `tasks:`.
    pub referencing_plan: BTreeMap<String, Vec<String>>,
    /// Branches claimed by more than one card, or by more than one plan, as
    /// `(branch, the claiming paths)`. A branch is allowed one card and one plan; a
    /// second claimant makes every lookup a coin toss, so it is recorded here rather
    /// than settled silently by whichever path sorts first.
    pub conflicts: Vec<(String, Vec<String>)>,
    /// Every declared ref that resolves to nothing, in document order.
    pub dangling: Vec<Dangling>,
}

impl DocIndex {
    pub fn plans(&self) -> Vec<&Document> {
        let prefix = format!("{PLANS_DIR}/");
        self.documents.values().filter(|d| d.path.starts_with(&prefix)).collect()
    }

    pub fn cards(&self) -> Vec<&Document> {
        let prefix = format!("{TASKS_DIR}/");
        self.documents.values().filter(|d| d.path.starts_with(&prefix)).collect()
    }

    pub fn get(&self, path: &str) -> Option<&Document> {
        self.documents.get(path)
    }

    /// Does this path exist in the repo at all? Drives the "missing" marker.
    pub fn exists(&self, path: &str) -> bool {
        self.all_paths.contains(path)
    }

    /// Documents that declare `branch: <branch>`.
    pub fn documents_for_branch(&self, branch: &str) -> Vec<&Document> {
        self.by_branch
            .get(branch)
            .map(|paths| paths.iter().filter_map(|p| self.documents.get(p)).collect())
            .unwrap_or_default()
    }

    /// Every card declaring `branch: <branch>`. More than one is the conflict.
    ///
    /// By directory, the way the merge flip picks the files it rewrites — not by
    /// [`Document::is_card`], which a plan carrying a `status:` also answers yes to.
    pub fn cards_for_branch(&self, branch: &str) -> Vec<&Document> {
        let prefix = format!("{TASKS_DIR}/");
        self.documents_for_branch(branch)
            .into_iter()
            .filter(|d| d.path.starts_with(&prefix))
            .collect()
    }

    /// The single card that owns a branch, if exactly one claims it.
    pub fn card_for_branch(&self, branch: &str) -> Option<&Document> {
        self.documents_for_branch(branch).into_iter().find(|d| d.is_card())
    }

    pub fn plan_for_branch(&self, branch: &str) -> Option<&Document> {
        self.documents_for_branch(branch).into_iter().find(|d| !d.is_card())
    }

    /// The claimant groups fighting over `branch`, empty when nothing conflicts.
    pub fn conflicts_for_branch(&self, branch: &str) -> Vec<&Vec<String>> {
        self.conflicts.iter().filter(|(b, _)| b == branch).map(|(_, paths)| paths).collect()
    }

    /// Every markdown file in the repo: the wiki's whole surface, in path order.
    ///
    /// There is no separate wiki store — the pages are the files already in git, so
    /// this is the tree filtered by extension and nothing more.
    pub fn wiki_pages(&self) -> Vec<&str> {
        self.all_paths
            .iter()
            .map(String::as_str)
            .filter(|path| is_markdown_path(path))
            .collect()
    }

    /// The wiki's home: `docs/index.md` when it exists, else the root README.
    ///
    /// README casing varies by repo and by author; the match ignores it. A README in
    /// a subdirectory is that directory's, not the wiki's.
    pub fn wiki_home(&self) -> Option<&str> {
        if let Some(index) = self.all_paths.get(WIKI_INDEX) {
            return Some(index.as_str());
        }
        self.all_paths
            .iter()
            .map(String::as_str)
            .find(|path| {
                !path.contains('/')
                    && is_markdown_path(path)
                    && path.rsplit_once('.').is_some_and(|(stem, _)| stem.eq_ignore_ascii_case("readme"))
            })
    }

    /// Cards and plans that point at this plan, in either direction.
    pub fn backlinks_to(&self, path: &str) -> Vec<&Document> {
        self.referencing_plan
            .get(path)
            .map(|paths| paths.iter().filter_map(|p| self.documents.get(p)).collect())
            .unwrap_or_default()
    }

    /// Cards grouped into board columns, each column ordered newest first by the order
    /// the caller supplied.
    pub fn board(&self, order: &[String]) -> Vec<(String, Vec<&Document>)> {
        let mut columns: BTreeMap<&str, Vec<&Document>> = BTreeMap::new();
        for card in self.cards() {
            columns.entry(card.column()).or_default().push(card);
        }
        let mut ordered: Vec<(String, Vec<&Document>)> = Vec::new();
        for status in order {
            let mut cards = columns.remove(status.as_str()).unwrap_or_default();
            // Preserve the caller's ordering hint (mtime, newest first).
            cards.sort_by_key(|card| {
                order.iter().position(|s| s == card.column()).unwrap_or(usize::MAX)
            });
            ordered.push((status.clone(), cards));
        }
        ordered
    }
}

/// Builds and caches a [`DocIndex`] per commit.
///
/// The scan reads every plan and card in the tree, so it is cached against the commit
/// it describes and thrown away the moment the tip moves.
#[derive(Clone, Default)]
pub struct DocIndexCache {
    entries: Arc<Mutex<HashMap<String, Arc<DocIndex>>>>,
}

impl std::fmt::Debug for DocIndexCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DocIndexCache")
    }
}

impl DocIndexCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The index for a repo at a commit, scanning only on a cache miss.
    pub async fn get(&self, repo_name: &str, repo: &Repo, commit: &str) -> Arc<DocIndex> {
        let key = format!("{repo_name}@{commit}");
        if let Some(hit) = self.entries.lock().await.get(&key) {
            return hit.clone();
        }
        let index = Arc::new(scan(repo, commit).await.unwrap_or_else(|error| {
            tracing::warn!(repo_name, commit, %error, "document scan failed");
            DocIndex { commit: commit.to_owned(), ..Default::default() }
        }));
        let mut entries = self.entries.lock().await;
        // Keep the map from growing without bound as branches move.
        if entries.len() > 64 {
            entries.clear();
        }
        entries.insert(key, index.clone());
        index
    }
}

/// What a conflict group is made of, for the sentence that reports it.
pub fn conflict_kind(paths: &[String]) -> &'static str {
    match paths.first() {
        Some(path) if path.starts_with(&format!("{TASKS_DIR}/")) => "cards",
        _ => "plans",
    }
}

/// Read every plan and card in the tree at `commit` and derive the back-links.
pub async fn scan(repo: &Repo, commit: &str) -> GitResult<DocIndex> {
    let all_paths: BTreeSet<String> = repo.list_files(commit, "").await?.into_iter().collect();

    let mut documents = BTreeMap::new();
    for path in &all_paths {
        let in_plans = path.starts_with(&format!("{PLANS_DIR}/"));
        let in_tasks = path.starts_with(&format!("{TASKS_DIR}/"));
        if !(in_plans || in_tasks) {
            continue;
        }
        if !(path.ends_with(".md") || path.ends_with(".markdown")) {
            continue;
        }
        let Some(bytes) = repo.show_file(commit, path).await? else { continue };
        let source = String::from_utf8_lossy(&bytes);
        documents.insert(path.clone(), parse_document(path, &source));
    }

    let mut by_branch: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut referencing_plan: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for document in documents.values() {
        if let Some(branch) = &document.refs.branch {
            by_branch.entry(branch.clone()).or_default().push(document.path.clone());
        }
        // A card naming its plan, and a plan naming its cards, are the same edge seen
        // from the two ends. Record both so either page can show the other.
        if let Some(plan) = &document.refs.plan {
            referencing_plan.entry(plan.clone()).or_default().push(document.path.clone());
        }
        for task in &document.refs.tasks {
            referencing_plan.entry(task.clone()).or_default().push(document.path.clone());
        }
    }
    for paths in by_branch.values_mut().chain(referencing_plan.values_mut()) {
        paths.sort();
        paths.dedup();
    }

    // One card and one plan per branch. The two kinds are counted apart because a
    // branch is allowed one of each; two of either kind is a conflict nobody can
    // resolve for the reader, so it is named instead of hidden.
    let tasks_prefix = format!("{TASKS_DIR}/");
    let mut conflicts: Vec<(String, Vec<String>)> = Vec::new();
    for (branch, paths) in &by_branch {
        for cards in [true, false] {
            let claimants: Vec<String> = paths
                .iter()
                .filter(|path| path.starts_with(&tasks_prefix) == cards)
                .cloned()
                .collect();
            if claimants.len() > 1 {
                conflicts.push((branch.clone(), claimants));
            }
        }
    }

    // Refs that point at nothing. Branches come from the mirror's ref list rather than
    // the tree, because a branch is not a file; a doc path is dangling when the tree at
    // this commit does not have it. A git failure aborts the scan rather than reporting
    // every branch ref as dangling.
    let branches: BTreeSet<String> = repo.branches().await?.into_iter().collect();
    let mut dangling: Vec<Dangling> = Vec::new();
    for document in documents.values() {
        let mut note = |key: &str, target: &str| {
            dangling.push(Dangling {
                from: document.path.clone(),
                key: key.to_owned(),
                target: target.to_owned(),
            });
        };
        if let Some(branch) = &document.refs.branch
            && !branches.contains(branch)
        {
            note("branch", branch);
        }
        if let Some(plan) = &document.refs.plan
            && !all_paths.contains(plan)
        {
            note("plan", plan);
        }
        for task in &document.refs.tasks {
            if !all_paths.contains(task) {
                note("tasks", task);
            }
        }
    }

    Ok(DocIndex {
        commit: commit.to_owned(),
        documents,
        all_paths,
        by_branch,
        referencing_plan,
        conflicts,
        dangling,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "---\nstatus: doing\ntitle: Ship the API\nbranch: feat/api\nplan: plans/api.md\n---\n\n# Ship it\n\nFirst paragraph here.\n\nSecond.\n";

    #[test]
    fn front_matter_splits_cleanly() {
        let (front, body) = split_front_matter(CARD);
        assert!(front.unwrap().contains("status: doing"));
        assert!(body.starts_with("\n# Ship it"));
    }

    #[test]
    fn a_file_without_front_matter_is_all_body() {
        let (front, body) = split_front_matter("# Just a doc\n\ntext\n");
        assert!(front.is_none());
        assert_eq!(body, "# Just a doc\n\ntext\n");
    }

    #[test]
    fn a_card_parses_its_fields_and_refs() {
        let card = parse_document("tasks/api.md", CARD);
        assert_eq!(card.title, "Ship the API");
        assert_eq!(card.status.as_deref(), Some("doing"));
        assert_eq!(card.refs.branch.as_deref(), Some("feat/api"));
        assert_eq!(card.refs.plan.as_deref(), Some("plans/api.md"));
        assert_eq!(card.summary, "First paragraph here.");
        assert!(card.front_matter_error.is_none());
        assert_eq!(card.column(), "doing");
    }

    #[test]
    fn a_title_falls_back_to_the_first_heading_then_the_filename() {
        let heading = parse_document("plans/x.md", "---\nstatus: todo\n---\n# From heading\n");
        assert_eq!(heading.title, "From heading");
        let stem = parse_document("plans/my-plan.md", "no heading here\n");
        assert_eq!(stem.title, "my-plan");
    }

    #[test]
    fn malformed_front_matter_quarantines_the_card_instead_of_failing() {
        let broken = parse_document("tasks/bad.md", "---\nstatus: [unclosed\n---\nbody\n");
        assert!(broken.front_matter_error.is_some());
        assert_eq!(broken.column(), NEEDS_ATTENTION);
        // The body is still readable, so the card can be opened and fixed.
        assert!(broken.body.contains("body"));
    }

    #[test]
    fn a_card_with_no_status_is_quarantined_too() {
        let card = parse_document("tasks/x.md", "---\ntitle: No status\n---\nbody\n");
        assert_eq!(card.column(), NEEDS_ATTENTION);
        assert!(card.front_matter_error.is_some());
    }

    #[test]
    fn tasks_accepts_one_path_or_a_list() {
        let one = parse_document("plans/p.md", "---\ntasks: tasks/a.md\n---\n");
        assert_eq!(one.refs.tasks, vec!["tasks/a.md"]);
        let many = parse_document("plans/p.md", "---\ntasks: [tasks/a.md, tasks/b.md]\n---\n");
        assert_eq!(many.refs.tasks, vec!["tasks/a.md", "tasks/b.md"]);
    }

    #[test]
    fn rewriting_the_status_leaves_every_other_byte_alone() {
        let moved = rewrite_status(CARD, "done").unwrap();
        assert!(moved.contains("status: done"));
        assert!(!moved.contains("status: doing"));
        // The body must be byte-identical.
        let (_, original_body) = split_front_matter(CARD);
        let (_, new_body) = split_front_matter(&moved);
        assert_eq!(original_body, new_body);
        // And every other front-matter line survives.
        assert!(moved.contains("title: Ship the API"));
        assert!(moved.contains("branch: feat/api"));
        assert!(moved.contains("plan: plans/api.md"));
    }

    #[test]
    fn rewriting_adds_a_status_line_when_there_is_none() {
        let source = "---\ntitle: X\n---\nbody\n";
        let moved = rewrite_status(source, "todo").unwrap();
        assert!(moved.contains("status: todo"));
        assert!(moved.contains("title: X"));
        assert!(moved.ends_with("body\n"));
    }

    #[test]
    fn rewriting_a_file_without_front_matter_is_refused() {
        assert!(rewrite_status("# no front matter\n", "done").is_none());
    }

    #[test]
    fn an_empty_front_matter_block_gains_a_status_inside_the_fences() {
        // The empty block used to trip `source.find("")` == 0 and prepend the status
        // before the opening fence. Byte-for-byte expectation:
        let moved = rewrite_status("---\n---\nbody\n", "todo").unwrap();
        assert_eq!(moved, "---\nstatus: todo\n---\nbody\n");
    }

    #[test]
    fn front_matter_that_repeats_earlier_bytes_rewrites_in_place() {
        // A block of dashes also occurs inside the opening fence; offsets, not a
        // substring search, must locate the block.
        let moved = rewrite_status("---\n-\n---\nbody\n", "todo").unwrap();
        assert_eq!(moved, "---\n-\nstatus: todo\n---\nbody\n");
    }

    #[test]
    fn columns_run_todo_doing_done_then_extras_then_quarantine() {
        let order = order_columns([
            "done".to_owned(),
            "zeta".to_owned(),
            "todo".to_owned(),
            NEEDS_ATTENTION.to_owned(),
            "blocked".to_owned(),
            "doing".to_owned(),
        ]);
        assert_eq!(
            order,
            vec!["todo", "doing", "done", "blocked", "zeta", NEEDS_ATTENTION]
        );
    }

    #[test]
    fn missing_canonical_columns_are_not_invented() {
        assert_eq!(order_columns(["doing".to_owned()]), vec!["doing"]);
    }
}
