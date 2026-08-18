//! nashgit viewer: stacked-branch review, plans, board, CI, and brain for a dgit
//! server. See SPEC.md for the contract and README.md for setup.

use std::sync::Arc;

use nashgit::ci::{CiQueue, CiWorker, DEFAULT_TIMEOUT};
use nashgit::config::Config;
use nashgit::db::Db;
use nashgit::docs::DocIndexCache;
use nashgit::hooks::Webhooks;
use nashgit::mirror::{Mirrors, NewTip, TipObserver};
use nashgit::ops::Ops;
use nashgit::{brain, hooks, web};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nashgit=info,warn".into()),
        )
        .init();

    let config = Arc::new(Config::from_env());
    doctor(&config);

    let db = match Db::open(&config.db_path) {
        Ok(db) => db,
        Err(error) => {
            eprintln!("cannot open {}: {error}", config.db_path.display());
            std::process::exit(1);
        }
    };

    let hooks = Webhooks::new(config.webhooks.clone());
    let (ci_queue, ci_rx) = CiQueue::new(db.clone());

    // Every newly seen branch tip queues a CI job and fires the push webhook.
    let observer = {
        let ci = ci_queue.clone();
        let hooks = hooks.clone();
        Arc::new(move |tip: NewTip| {
            ci.enqueue(&tip.repo, &tip.branch, &tip.commit);
            hooks.send(
                hooks::PUSH,
                serde_json::json!({
                    "repo": tip.repo,
                    "branch": tip.branch,
                    "commit": tip.commit,
                }),
            );
        }) as TipObserver
    };
    let mirrors = Mirrors::new(config.clone(), db.clone()).with_observer(observer);

    tokio::spawn(
        CiWorker {
            config: config.clone(),
            db: db.clone(),
            hooks: hooks.clone(),
            timeout: DEFAULT_TIMEOUT,
        }
        .run(ci_rx),
    );

    // Warm every mirror once so the first page load is instant.
    {
        let mirrors = mirrors.clone();
        tokio::spawn(async move {
            mirrors.refresh_all().await;
        });
    }

    let app = web::App {
        ops: Ops {
            config: config.clone(),
            db: db.clone(),
            mirrors: mirrors.clone(),
            hooks: hooks.clone(),
        },
        config: config.clone(),
        db,
        mirrors,
        docs: DocIndexCache::new(),
        ci: ci_queue,
        brain: brain::Brain::new(),
    };

    let router = web::router(app);
    let listener = match tokio::net::TcpListener::bind(&config.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot bind {}: {error}", config.bind);
            std::process::exit(1);
        }
    };
    tracing::info!(bind = %config.bind, "nashgit listening");
    if let Err(error) = topcoat::serve(listener, router).await {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}

/// One line per thing an operator would otherwise discover the hard way.
fn doctor(config: &Config) {
    if config.repos.is_empty() {
        eprintln!("doctor: NASHGIT_REPOS is empty; the index page will show nothing");
    }
    if config.dgit_url.is_empty() {
        eprintln!("doctor: DGIT_URL is unset; mirrors cannot clone or fetch");
    }
    if config.anthropic_key.is_none() {
        eprintln!("doctor: ANTHROPIC_API_KEY is unset; POST /brain/ask answers 404");
    }
    if config.git_token.is_empty() {
        eprintln!("doctor: GIT_TOKEN is empty; pushes to dgit will be anonymous");
    }
}
