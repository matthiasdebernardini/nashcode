//! nashcode viewer: stacked-branch review, plans, board, CI, and brain for a dgit
//! server. See SPEC.md for the contract and README.md for setup.

use std::sync::Arc;

use nashcode::ci::{CiQueue, CiWorker, DEFAULT_TIMEOUT};
use nashcode::code::{Embeddings, Indexer};
use nashcode::config::Config;
use nashcode::db::Db;
use nashcode::docs::DocIndexCache;
use nashcode::hooks::Webhooks;
use nashcode::mirror::{Mirrors, NewTip, TipObserver};
use nashcode::ops::Ops;
use nashcode::bugs::Bugs;
use nashcode::{brain, hooks, web};

#[tokio::main]
async fn main() {
    // The same binary is the server and the agent-side client.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("serve") => {
            serve().await;
            0
        }
        Some("hook") => nashcode::cli::hook().await,
        Some("trace") => match args.get(1).map(String::as_str) {
            Some("push") => match args.get(2) {
                Some(file) => {
                    nashcode::cli::trace_push(
                        file,
                        nashcode::cli::flag_value(&args, "--session"),
                        nashcode::cli::flag_value(&args, "--repo"),
                    )
                    .await
                }
                None => {
                    eprintln!("{}", nashcode::cli::USAGE);
                    2
                }
            },
            Some("list") => nashcode::cli::trace_list(nashcode::cli::flag_value(&args, "--repo")).await,
            Some("show") => match args.get(2) {
                Some(session) => {
                    nashcode::cli::trace_show(session, nashcode::cli::flag_value(&args, "--repo"))
                        .await
                }
                None => {
                    eprintln!("{}", nashcode::cli::USAGE);
                    2
                }
            },
            _ => {
                eprintln!("{}", nashcode::cli::USAGE);
                2
            }
        },
        Some("doctor") => nashcode::cli::doctor().await,
        Some("--help") | Some("-h") | Some("help") => {
            println!("{}", nashcode::cli::USAGE);
            0
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{}", nashcode::cli::USAGE);
            2
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

async fn serve() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nashcode=info,warn".into()),
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

    // Every newly seen branch tip queues a CI job, asks for a code index run, and
    // fires the push webhook. Routing indexing through the same observer means a merge
    // made outside the viewer indexes too, which a call inside `merge` would miss; the
    // queue coalesces the duplicates a multi-branch push produces.
    let observer = {
        let ci = ci_queue.clone();
        let hooks = hooks.clone();
        Arc::new(move |tip: NewTip| {
            ci.enqueue(&tip.repo, &tip.branch, &tip.commit);
            ci.enqueue_index(&tip.repo);
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

    let embeddings = Embeddings::new();
    tokio::spawn(
        CiWorker {
            config: config.clone(),
            db: db.clone(),
            hooks: hooks.clone(),
            timeout: DEFAULT_TIMEOUT,
            indexer: Some(Indexer {
                config: config.clone(),
                db: db.clone(),
                mirrors: mirrors.clone(),
                embeddings: embeddings.clone(),
            }),
            queue: Some(ci_queue.clone()),
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

    let bugs = match Bugs::new(&config, db.clone()) {
        Ok(bugs) => bugs,
        Err(error) => {
            eprintln!("cannot apply the bugs schema: {error}");
            std::process::exit(1);
        }
    };

    // Anything the digest never finished — a crash between the bucket write and the
    // index write — is picked up here rather than sitting in the bucket forever.
    {
        let bugs = bugs.clone();
        tokio::spawn(async move {
            bugs.sweep(false).await;
        });
    }

    // The nightly log prune. A day is the interval, not the alignment: a viewer that
    // restarts at noon prunes at noon, which is a property nobody has to think about.
    // Only hot rows go; the NDJSON archive in the bucket stays.
    if bugs.enabled() {
        let bugs = bugs.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
            loop {
                every.tick().await;
                match bugs.prune_logs() {
                    Ok(0) => {}
                    Ok(deleted) => tracing::info!(deleted, "bugs: pruned log rows past retention"),
                    Err(error) => tracing::warn!(%error, "bugs: cannot prune the log window"),
                }
            }
        });
    }

    // The drain. Off unless an ingester is configured, and a refusal to start rather
    // than a warning when it is configured with no bucket: a drainer that acked rows
    // into nowhere durable would delete them off the edge and lose them for good.
    if let Some(drain) = config.bugs_drain.clone() {
        if !bugs.enabled() {
            eprintln!(
                "NASHCODE_BUGS_DRAIN is set and NASHCODE_BUGS_BUCKET is not. The drainer \
                 acks rows off the ingester once they are durable here, and with no \
                 bucket nothing here is durable. Set the bucket, or unset the drain."
            );
            std::process::exit(1);
        }
        match nashcode::bugs::drain::transport_for(&drain).await {
            Ok(transport) => {
                let drainer =
                    nashcode::bugs::drain::Drainer::new(bugs.clone(), db.clone(), transport);
                tracing::info!(
                    target = %drain.target,
                    interval = drain.interval.as_secs(),
                    "bugs: draining the public ingester"
                );
                tokio::spawn(drainer.run(drain.interval));
            }
            Err(error) => {
                eprintln!("cannot dial NASHCODE_BUGS_DRAIN: {error}");
                std::process::exit(1);
            }
        }
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
        bugs,
        embeddings,
    };

    let router = web::router(app);
    let listener = match tokio::net::TcpListener::bind(&config.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot bind {}: {error}", config.bind);
            std::process::exit(1);
        }
    };
    tracing::info!(bind = %config.bind, "nashcode listening");
    if let Err(error) = topcoat::serve(listener, router).await {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}

/// One line per thing an operator would otherwise discover the hard way.
fn doctor(config: &Config) {
    if config.repos.is_empty() {
        eprintln!("doctor: NASHCODE_REPOS is empty; the index page will show nothing");
    }
    if config.dgit_url.is_empty() {
        eprintln!("doctor: DGIT_URL is unset; mirrors cannot clone or fetch");
    }
    if config.anthropic_key.is_none() {
        eprintln!("doctor: ANTHROPIC_API_KEY is unset; POST /brain/ask answers 404");
    }
    if config.bugs_bucket.is_none() {
        eprintln!(
            "doctor: NASHCODE_BUGS_BUCKET is unset; error tracking is off and /bugs \
             plus /api/:project/envelope/ answer 404"
        );
    }
    match &config.bugs_drain {
        None => eprintln!(
            "doctor: NASHCODE_BUGS_DRAIN is unset; nothing is pulled from the public \
             ingester and only projects that post straight at the tailnet are tracked"
        ),
        Some(drain) if !drain.is_url() && cfg!(not(feature = "drain-iroh")) => eprintln!(
            "doctor: NASHCODE_BUGS_DRAIN looks like an iroh EndpointId and this binary \
             was built without the `drain-iroh` feature; the drainer will refuse to start"
        ),
        Some(_) => {}
    }
    if config.git_token.is_empty() {
        eprintln!("doctor: GIT_TOKEN is empty; pushes to dgit will be anonymous");
    }
    if cfg!(not(feature = "embeddings")) {
        eprintln!(
            "doctor: built without the `embeddings` feature; GET /:repo/code/similar \
             will report itself unavailable. Text search and the symbol graph are \
             unaffected."
        );
    } else {
        eprintln!(
            "doctor: semantic search needs the ONNX Runtime shared library at run time \
             (set ORT_DYLIB_PATH, or install libonnxruntime), and downloads the {} \
             model on the first index run. Missing either one degrades \
             /:repo/code/similar and nothing else.",
            nashcode::code::embed::configured_model()
        );
    }
}
