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
use crate::upstream::Upstreams;

/// Builds the deterministic aggregate, caching the git-derived part of each repo
/// against its branch tips. SQLite-derived activity is cheap and always fresh.
#[derive(Clone)]
pub struct Brain {
    git_cache: Arc<Mutex<HashMap<String, (String, serde_json::Value)>>>,
    /// The upstream column. Held on the brain rather than passed to `aggregate`,
    /// because `aggregate`'s signature is shared with a route this work does not own.
    upstreams: Upstreams,
}

impl std::fmt::Debug for Brain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Brain")
    }
}

impl Brain {
    pub fn new(upstreams: Upstreams) -> Self {
        Self { git_cache: Arc::new(Mutex::new(HashMap::new())), upstreams }
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
        for name in config.repos.names() {
            if let Some(filter) = repo_filter
                && filter != name
            {
                continue;
            }
            repos.push(self.repo_json(config, db, mirrors, docs, &name, since).await);
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
            // Whether this repo has a drawn design, and how stale it is, in one look.
            if let Some(architecture) = architecture_json(db, name) {
                object.insert("architecture".to_owned(), architecture);
            }
            // How much the repo is queryable as code, and how old that answer is.
            object.insert("code".to_owned(), crate::code::brain_stanza(db, name));
            // The upstream column, for a repo that declares one. Outside the tip cache
            // above on purpose: a `track` dep moves without any branch of ours moving,
            // and a mirror that failed to fetch has to be able to say so today. The
            // key is absent for a repo with no manifest, the way `architecture` is
            // absent for a repo nobody has drawn.
            if let Some(stack) = self.upstreams.stack(&repo).await
                && let Ok(stack) = serde_json::to_value(&stack)
            {
                object.insert("stack".to_owned(), stack);
            }
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

/// How many architecture diagrams a repo has, and who drew the newest one.
///
/// `None` for a repo nobody has submitted a diagram for, so the key is absent
/// rather than a stanza of nulls: "has a drawn design" is answered by whether the
/// key is there at all. `architecture_history` is already ordered newest-first, so
/// the count and the latest come out of one query.
fn architecture_json(db: &Db, repo: &str) -> Option<serde_json::Value> {
    let history = db.architecture_history(repo).unwrap_or_default();
    let latest = history.first()?;
    Some(serde_json::json!({
        "submissions": history.len(),
        "latest_at": latest.created_at,
        "latest_author": latest.author,
    }))
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
    /// Which code-intelligence tools the model reached for, in order. Visible so an
    /// agent reading the reply can see what it was actually grounded in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_used: Vec<String>,
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
    name branches, plans, and cards by their real identifiers. The STATE JSON \
    describes plans, cards, branches, and activity, but not the source code. To answer \
    anything about the code itself, call the tools: code_text for an exact string, \
    code_similar for a description of behaviour, and code_def, code_refs, and \
    code_callers for a named symbol. Cite the file and line you got an answer from. A \
    tool that answers with an empty list has answered: say so rather than guessing.";

/// How many tool round trips one question may take before the loop gives up and asks
/// for a final answer. Enough for "find it, then find who calls it, then read it";
/// short enough that a model stuck in a loop still returns.
const MAX_TOOL_TURNS: usize = 6;

/// The whole question's budget, tool calls included.
///
/// SPEC gives `/brain/ask` five minutes. Before the tool loop that was also the
/// per-request timeout and the two were the same number; now seven requests could sit
/// behind it, so the deadline has to be around the loop rather than around each hop.
const ASK_DEADLINE: Duration = Duration::from_secs(300);

/// Everything the code-intelligence tools need. Held by the route, passed in whole so
/// `ask` has one parameter for "the repos you may look at" rather than four.
#[derive(Clone)]
pub struct Tools {
    pub db: Db,
    pub mirrors: Mirrors,
    pub embeddings: crate::code::Embeddings,
    /// The repos the question was scoped to. A tool call naming anything else is
    /// refused: `/brain/ask?repo=x` must not become a way to read repo `y`.
    pub repos: Vec<String>,
}

impl std::fmt::Debug for Tools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tools")
    }
}

impl Tools {
    /// The tool definitions, as the Claude API wants them.
    fn definitions(&self) -> serde_json::Value {
        let repo = serde_json::json!({
            "type": "string",
            "description": format!("The repository to search. One of: {}.", self.repos.join(", ")),
        });
        let limit = serde_json::json!({
            "type": "integer",
            "description": "How many results to return. Defaults to 10.",
        });
        serde_json::json!([
            {
                "name": "code_text",
                "description": "Exact full-text search over a repository's default \
                    branch, the way `git grep` does it. Use it when you know the \
                    literal string: an error message, a config key, an identifier you \
                    are unsure is a symbol.",
                "input_schema": {
                    "type": "object",
                    "properties": { "repo": repo, "q": {
                        "type": "string",
                        "description": "The literal text to find. Not a regular expression.",
                    }, "limit": limit },
                    "required": ["repo", "q"],
                },
            },
            {
                "name": "code_similar",
                "description": "Semantic search: find the code that is *about* \
                    something, by meaning rather than by spelling. Use it when you can \
                    describe the behaviour but not name it — 'where retries are backed \
                    off', 'the code that parses the config file'.",
                "input_schema": {
                    "type": "object",
                    "properties": { "repo": repo, "q": {
                        "type": "string",
                        "description": "A description of the behaviour you are looking for.",
                    }, "limit": limit },
                    "required": ["repo", "q"],
                },
            },
            {
                "name": "code_def",
                "description": "Where a named symbol is defined: its file, its line \
                    range, and what kind of thing it is.",
                "input_schema": {
                    "type": "object",
                    "properties": { "repo": repo, "symbol": {
                        "type": "string",
                        "description": "The bare name, without a module path: `retry`, not `net::retry`.",
                    } },
                    "required": ["repo", "symbol"],
                },
            },
            {
                "name": "code_refs",
                "description": "Every place a named symbol is used, with the function \
                    each use sits inside.",
                "input_schema": {
                    "type": "object",
                    "properties": { "repo": repo, "symbol": { "type": "string" }, "limit": limit },
                    "required": ["repo", "symbol"],
                },
            },
            {
                "name": "code_callers",
                "description": "The functions that call a named symbol. This is the \
                    'who depends on this' question.",
                "input_schema": {
                    "type": "object",
                    "properties": { "repo": repo, "symbol": { "type": "string" }, "limit": limit },
                    "required": ["repo", "symbol"],
                },
            },
        ])
    }

    /// Run one tool call. Every failure is a result the model can read and recover
    /// from, never an error that ends the conversation.
    async fn call(&self, name: &str, input: &serde_json::Value) -> serde_json::Value {
        let repo = input["repo"].as_str().unwrap_or_default().to_owned();
        if !self.repos.iter().any(|known| known == &repo) {
            return serde_json::json!({
                "error": format!(
                    "no repository named {repo} is in scope; ask about one of: {}",
                    self.repos.join(", ")
                ),
            });
        }
        let limit = input["limit"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(crate::code::DEFAULT_LIMIT)
            .clamp(1, crate::code::MAX_LIMIT);
        let symbol = input["symbol"].as_str().unwrap_or_default().trim().to_owned();
        let query = input["q"].as_str().unwrap_or_default().trim().to_owned();

        match name {
            "code_text" => {
                let handle = self.mirrors.repo(&repo);
                let rev = handle.default_branch().await.unwrap_or_else(|_| "HEAD".to_owned());
                match crate::code::grep(&handle, &rev, &query, limit).await {
                    Ok(found) => serde_json::json!({
                        "hits": found.hits,
                        "truncated": found.truncated,
                    }),
                    Err(error) => serde_json::json!({ "error": error.to_string() }),
                }
            }
            "code_similar" => match self.embeddings.ready() {
                None => serde_json::json!({
                    "error": format!(
                        "semantic search is unavailable: {}. Use code_text instead.",
                        self.embeddings.why_not()
                    ),
                }),
                Some(embedder) => {
                    let one = vec![query];
                    match tokio::task::spawn_blocking(move || embedder.embed(&one)).await {
                        Ok(Ok(mut vectors)) if !vectors.is_empty() => {
                            let found =
                                crate::code::similar(&self.db, &repo, vectors.remove(0), limit)
                                    .await;
                            serde_json::json!({ "hits": found.hits })
                        }
                        Ok(Ok(_)) => serde_json::json!({ "error": "the model returned no vector" }),
                        Ok(Err(error)) => serde_json::json!({ "error": error.to_string() }),
                        Err(error) => serde_json::json!({ "error": error.to_string() }),
                    }
                }
            },
            "code_def" => serde_json::json!({
                "definitions": crate::code::definitions(&self.db, &repo, &symbol),
            }),
            "code_refs" => serde_json::json!({
                "references": crate::code::references(&self.db, &repo, &symbol)
                    .into_iter().take(limit).collect::<Vec<_>>(),
            }),
            "code_callers" => serde_json::json!({
                "callers": crate::code::callers(&self.db, &repo, &symbol)
                    .into_iter().take(limit).collect::<Vec<_>>(),
            }),
            other => serde_json::json!({ "error": format!("no tool named {other}") }),
        }
    }
}

/// Ask the Claude API, letting it call the code-intelligence tools until it has what
/// it needs.
///
/// The deterministic state and the plan documents go in the first message, as before.
/// What is new is that the source itself does not: a repo's code is far too large to
/// paste, and the tools are how the model reaches the part of it that matters.
pub async fn ask(
    config: &Config,
    state: serde_json::Value,
    documents: BTreeMap<String, String>,
    question: &str,
    tools: Option<&Tools>,
) -> Result<Answer, AskError> {
    // One deadline for the question, not one per hop: a slow model that keeps calling
    // tools must not be able to hold a request open for seven timeouts in a row.
    match tokio::time::timeout(ASK_DEADLINE, ask_within(config, state, documents, question, tools))
        .await
    {
        Ok(answer) => answer,
        Err(_elapsed) => Err(AskError::Upstream {
            status: 504,
            message: format!(
                "the question was still running after {}s; ask something narrower, or \
                 query the JSON endpoints directly",
                ASK_DEADLINE.as_secs()
            ),
        }),
    }
}

async fn ask_within(
    config: &Config,
    state: serde_json::Value,
    documents: BTreeMap<String, String>,
    question: &str,
    tools: Option<&Tools>,
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("reqwest client builds");

    // The opening message stays a plain string. Only the tool-result turns need the
    // block form, and keeping the first one as it always was means the request an
    // operator sees on the wire has not changed shape.
    let mut messages = vec![serde_json::json!({ "role": "user", "content": user })];
    let mut tools_used: Vec<String> = Vec::new();
    let mut model = config.brain_model.clone();

    for turn in 0..=MAX_TOOL_TURNS {
        let mut body = serde_json::json!({
            "model": config.brain_model,
            "max_tokens": 16000,
            "system": SYSTEM_PROMPT,
            "messages": messages,
        });
        // The last turn goes without tools, so the model has to answer with what it
        // already found instead of asking for one more thing forever.
        if let Some(tools) = tools
            && turn < MAX_TOOL_TURNS
        {
            body["tools"] = tools.definitions();
        }

        let parsed = send(&client, config, key, &body).await?;
        model = parsed["model"].as_str().unwrap_or(&model).to_owned();

        let blocks: Vec<serde_json::Value> =
            parsed["content"].as_array().cloned().unwrap_or_default();
        let answer: String = blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect();

        match parsed["stop_reason"].as_str() {
            Some("refusal") => {
                return Err(AskError::Upstream {
                    status: 502,
                    message: format!("the model refused: {answer}"),
                });
            }
            Some("max_tokens") => {
                return Ok(Answer {
                    answer: format!("{answer}\n\n[truncated: the reply hit max_tokens]"),
                    model,
                    tools_used,
                });
            }
            Some("tool_use") => {}
            _ => return Ok(Answer { answer, model, tools_used }),
        }

        let Some(tools) = tools else {
            // Asked for a tool in a build that offered none. Nothing to run, so the
            // text it did produce is the answer.
            return Ok(Answer { answer, model, tools_used });
        };

        let calls: Vec<&serde_json::Value> =
            blocks.iter().filter(|block| block["type"] == "tool_use").collect();
        if calls.is_empty() {
            return Ok(Answer { answer, model, tools_used });
        }

        let mut results = Vec::with_capacity(calls.len());
        for call in &calls {
            let name = call["name"].as_str().unwrap_or_default();
            let output = tools.call(name, &call["input"]).await;
            tools_used.push(format!("{name}({})", call["input"]["repo"].as_str().unwrap_or("?")));
            results.push(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": call["id"],
                "content": output.to_string(),
            }));
        }
        messages.push(serde_json::json!({ "role": "assistant", "content": blocks }));
        messages.push(serde_json::json!({ "role": "user", "content": results }));
    }

    Ok(Answer {
        answer: format!("[gave up after {MAX_TOOL_TURNS} tool round trips]"),
        model,
        tools_used,
    })
}

/// One request to the Claude API, with the status mapping the spec asks for: a 429
/// passes through as actionable rate-limit information; everything else is a 502
/// carrying the API's own message.
async fn send(
    client: &reqwest::Client,
    config: &Config,
    key: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, AskError> {
    let response = client
        .post(format!("{}/v1/messages", config.anthropic_url))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(body)
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
        let ours = if status == 429 { 429 } else { 502 };
        return Err(AskError::Upstream { status: ours, message });
    }

    serde_json::from_str(&text).map_err(|error| AskError::Upstream {
        status: 502,
        message: format!("unparseable API reply: {error}"),
    })
}
