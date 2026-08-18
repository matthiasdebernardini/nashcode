//! `/brain`: the whole tailnet's work state as one queryable JSON surface, plus the
//! optional subjective layer that asks the Claude API about it.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::db::Db;
use crate::docs::DocIndexCache;
use crate::mirror::Mirrors;
use crate::stack::StackGraph;

/// Builds the deterministic aggregate, caching the git-derived part of each repo
/// against its branch tips. SQLite-derived activity is cheap and always fresh.
#[derive(Clone, Default)]
pub struct Brain {
    git_cache: Arc<Mutex<HashMap<String, (String, serde_json::Value)>>>,
}

impl std::fmt::Debug for Brain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Brain")
    }
}

impl Brain {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `/brain` JSON. No model in the loop; every field is derived from the
    /// mirrors and SQLite.
    pub async fn aggregate(
        &self,
        config: &Config,
        db: &Db,
        mirrors: &Mirrors,
        docs: &DocIndexCache,
        repo_filter: Option<&str>,
        since: Option<&str>,
    ) -> serde_json::Value {
        let mut repos = Vec::new();
        for name in &config.repos {
            if let Some(filter) = repo_filter
                && filter != name
            {
                continue;
            }
            repos.push(self.repo_json(config, db, mirrors, docs, name, since).await);
        }
        serde_json::json!({
            "generated_at": crate::db::now(),
            "repos": repos,
        })
    }

    async fn repo_json(
        &self,
        _config: &Config,
        db: &Db,
        mirrors: &Mirrors,
        docs: &DocIndexCache,
        name: &str,
        since: Option<&str>,
    ) -> serde_json::Value {
        let status = mirrors.refresh(name).await;
        if !status.available {
            return serde_json::json!({
                "name": name,
                "available": false,
                "error": status.message,
            });
        }

        let repo = mirrors.repo(name);
        let git_part = match StackGraph::infer(&repo).await {
            Err(error) => serde_json::json!({ "error": error.to_string() }),
            Ok(graph) => {
                let tips_key: String = graph
                    .nodes
                    .values()
                    .map(|n| format!("{}={};", n.branch, n.tip))
                    .collect();
                let cached = self.git_cache.lock().await.get(name).and_then(|(key, value)| {
                    (key == &tips_key).then(|| value.clone())
                });
                match cached {
                    Some(value) => value,
                    None => {
                        let value = git_repo_json(db, mirrors, docs, name, &graph).await;
                        let mut cache = self.git_cache.lock().await;
                        if cache.len() > 32 {
                            cache.clear();
                        }
                        cache.insert(name.to_owned(), (tips_key, value.clone()));
                        value
                    }
                }
            }
        };

        // Branch CI status and activity come from SQLite, always fresh.
        let mut value = git_part;
        if let Some(object) = value.as_object_mut() {
            if let Some(branches) = object.get_mut("branches").and_then(|b| b.as_array_mut()) {
                for branch in branches {
                    let tip = branch.get("tip").and_then(|t| t.as_str()).unwrap_or_default();
                    let ci = db
                        .latest_run(name, tip)
                        .ok()
                        .flatten()
                        .map(|run| run.status)
                        .unwrap_or_else(|| "none".to_owned());
                    branch["ci"] = serde_json::Value::String(ci);
                }
            }
            object.insert("name".to_owned(), serde_json::json!(name));
            object.insert("available".to_owned(), serde_json::json!(true));
            object.insert("stale".to_owned(), serde_json::json!(status.stale));
            object.insert("activity".to_owned(), activity_json(db, name, since));
            object.insert("open_comment_counts".to_owned(), comment_counts(db, name));
        }
        value
    }
}

/// The tip-cached part: branches with stack shape, plans, cards.
async fn git_repo_json(
    _db: &Db,
    mirrors: &Mirrors,
    docs: &DocIndexCache,
    name: &str,
    graph: &StackGraph,
) -> serde_json::Value {
    let repo = mirrors.repo(name);
    let branches: Vec<serde_json::Value> = graph
        .nodes
        .values()
        .map(|node| {
            serde_json::json!({
                "branch": node.branch,
                "tip": node.tip,
                "parent": node.parent,
                "ahead": node.ahead,
            })
        })
        .collect();

    let default_tip = graph
        .get(&graph.default_branch)
        .map(|node| node.tip.clone())
        .unwrap_or_default();
    let index = docs.get(name, &repo, &default_tip).await;

    let plans: Vec<serde_json::Value> = index
        .plans()
        .iter()
        .map(|plan| {
            serde_json::json!({
                "path": plan.path,
                "title": plan.title,
                "refs": plan.refs,
                "summary": plan.summary,
            })
        })
        .collect();

    let mut cards: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for card in index.cards() {
        cards.entry(card.column().to_owned()).or_default().push(serde_json::json!({
            "path": card.path,
            "title": card.title,
            "assignee": card.assignee,
            "refs": card.refs,
        }));
    }

    serde_json::json!({
        "default_branch": graph.default_branch,
        "branches": branches,
        "plans": plans,
        "cards": cards,
    })
}

/// Merges, restacks, comments, and CI runs — each with an author and an RFC3339
/// timestamp — bounded by `since`.
fn activity_json(db: &Db, repo: &str, since: Option<&str>) -> serde_json::Value {
    let since = since.and_then(crate::db::normalize_timestamp);
    let keep = |at: &str| since.as_deref().is_none_or(|bound| at > bound);

    let mut events: Vec<serde_json::Value> = Vec::new();
    for entry in db.audit(repo, 100).unwrap_or_default() {
        if keep(&entry.created_at) {
            events.push(serde_json::json!({
                "type": entry.action,
                "author": entry.actor,
                "at": entry.created_at,
                "branch": entry.branch,
                "detail": entry.detail,
            }));
        }
    }
    let filter = crate::db::CommentFilter {
        repo: repo.to_owned(),
        since: since.clone(),
        ..Default::default()
    };
    for comment in db.comments(&filter).unwrap_or_default() {
        events.push(serde_json::json!({
            "type": "comment",
            "author": comment.author,
            "at": comment.created_at,
            "branch": comment.branch,
            "file": comment.file,
            "line": comment.line,
        }));
    }
    for run in db.recent_runs(repo, 100).unwrap_or_default() {
        if keep(&run.created_at) {
            events.push(serde_json::json!({
                "type": "ci_run",
                "author": "ci",
                "at": run.created_at,
                "branch": run.branch,
                "commit": run.commit,
                "status": run.status,
            }));
        }
    }
    events.sort_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()));
    serde_json::Value::Array(events)
}

fn comment_counts(db: &Db, repo: &str) -> serde_json::Value {
    let filter = crate::db::CommentFilter { repo: repo.to_owned(), ..Default::default() };
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for comment in db.comments(&filter).unwrap_or_default() {
        if let Some(file) = comment.file {
            *counts.entry(file).or_default() += 1;
        }
    }
    serde_json::json!(counts)
}

// ---- the subjective layer --------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct Answer {
    pub answer: String,
    pub model: String,
}

/// Why an ask failed, mapped onto responses by the route.
#[derive(Debug)]
pub enum AskError {
    /// The upstream API said no. `status` is what we respond with: the API's own 429
    /// passes through; everything else is a 502 carrying the API's message.
    Upstream { status: u16, message: String },
}

const SYSTEM_PROMPT: &str = "You are the work-state brain for a small personal git \
    forge. Answer from the STATE JSON and the documents given; be terse and concrete; \
    name branches, plans, and cards by their real identifiers.";

/// Send exactly one request to the Claude API and shape the reply.
pub async fn ask(
    config: &Config,
    state: serde_json::Value,
    documents: BTreeMap<String, String>,
    question: &str,
) -> Result<Answer, AskError> {
    let key = config.anthropic_key.as_deref().unwrap_or_default();

    let mut user = format!(
        "STATE:\n{}\n\n",
        serde_json::to_string_pretty(&state).unwrap_or_default()
    );
    if !documents.is_empty() {
        user.push_str("DOCUMENTS:\n");
        for (path, text) in &documents {
            user.push_str(&format!("--- {path} ---\n{text}\n"));
        }
        user.push('\n');
    }
    user.push_str(&format!("QUESTION: {question}"));

    let body = serde_json::json!({
        "model": config.brain_model,
        "max_tokens": 16000,
        "system": SYSTEM_PROMPT,
        "messages": [{ "role": "user", "content": user }],
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("reqwest client builds");

    let response = client
        .post(format!("{}/v1/messages", config.anthropic_url))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| AskError::Upstream { status: 502, message: error.to_string() })?;

    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();

    if !(200..300).contains(&status) {
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or(text);
        // 429 passes through untouched; anything else is a bad gateway.
        let ours = if status == 429 { 429 } else { 502 };
        return Err(AskError::Upstream { status: ours, message });
    }

    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        AskError::Upstream { status: 502, message: format!("unparseable API reply: {error}") }
    })?;

    let mut answer = String::new();
    for block in parsed["content"].as_array().into_iter().flatten() {
        if block["type"] == "text"
            && let Some(chunk) = block["text"].as_str()
        {
            answer.push_str(chunk);
        }
    }
    let model = parsed["model"].as_str().unwrap_or(&config.brain_model).to_owned();

    match parsed["stop_reason"].as_str() {
        Some("refusal") => Err(AskError::Upstream {
            status: 502,
            message: format!("the model refused: {answer}"),
        }),
        Some("max_tokens") => Ok(Answer {
            answer: format!("{answer}\n\n[truncated: the reply hit max_tokens]"),
            model,
        }),
        _ => Ok(Answer { answer, model }),
    }
}
