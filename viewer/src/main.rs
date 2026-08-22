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
use nashcode::upstream::Upstreams;
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
                        args.iter().any(|arg| arg == "--replace"),
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
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Configuration before logging, because the self-DSN decides what logging does.
    // Anything `Config::from_env` needs to complain about goes to stderr for the same
    // reason: there is no subscriber yet to hear it.
    let config = Arc::new(Config::from_env());
    // Held for the life of the process. Dropping it shuts the transport down, and the
    // last events — the ones from whatever is killing us — never leave.
    // The release is read at run time, not baked in. It was `option_env!` first, which
    // meant nothing ever set it and the one project this feature dogfoods was the one
    // project whose snippets always said "tip, not release". Set it to the deployed
    // commit and context capture works on nashcode's own issues too.
    let _reporting = nashcode::bugs::selfreport::init(
        config.bugs_self_dsn.as_deref(),
        std::env::var("NASHCODE_RELEASE")
            .ok()
            .map(|release| release.trim().to_owned())
            .filter(|release| !release.is_empty()),
    );
    // `Option<Layer>` is itself a `Layer`, so the wiring is the same either way.
    let reporting_layer = _reporting.as_ref().map(|_| nashcode::bugs::selfreport::layer());
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nashcode=info,warn".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(reporting_layer)
        .init();
    if _reporting.is_some() {
        tracing::info!("bugs: nashcode is reporting its own errors to NASHCODE_BUGS_SELF_DSN");
    }

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

    // The mirror poll. Its first cycle is immediate, so the first page load is instant;
    // every later cycle is what discovers a repo pushed to a name nobody configured.
    tokio::spawn(mirrors.clone().watch());

    // The upstream column has a clock of its own: a `track` dep moves in a repo nobody
    // here pushes to, so no tip observer and no page load would ever notice.
    let upstreams = Upstreams::new(config.clone());
    tokio::spawn(upstreams.clone().watch());

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
                // Check-ins are the other unbounded table: one a minute is half a
                // million rows a year. Same retention, same nightly pass.
                match bugs.prune_checkins() {
                    Ok(0) => {}
                    Ok(deleted) => tracing::info!(deleted, "bugs: pruned check-ins past retention"),
                    Err(error) => tracing::warn!(%error, "bugs: cannot prune the check-in history"),
                }
            }
        });
    }

    // The cron sweep, with the eviction pass riding along on the same tick. A minute is
    // the finest resolution a cron schedule has, so it is the finest "late" can mean;
    // eviction shares the tick because a project under its cap costs one indexed count
    // and a project over it must not stay over for an hour.
    if bugs.enabled() {
        let bugs = bugs.clone();
        tokio::spawn(nashcode::bugs::selfreport::quietly(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(
                nashcode::bugs::crons::SWEEP_INTERVAL_SECS,
            ));
            loop {
                every.tick().await;
                match bugs.sweep_crons() {
                    Ok(swept) if swept.total() == 0 => {}
                    Ok(swept) => tracing::info!(
                        missed = swept.missed,
                        timed_out = swept.timed_out,
                        "bugs: cron monitors went late"
                    ),
                    Err(error) => tracing::warn!(%error, "bugs: cannot sweep the cron monitors"),
                }
                bugs.evict().await;
            }
        }));
    }

    // The one task that talks to Pushover. Off with no credentials, and off with no
    // bucket too: with error tracking off there is no state change to report.
    if let Some(pushover) = config.pushover.clone()
        && bugs.enabled()
    {
        let sender = nashcode::bugs::pushover::Sender::new(
            db.clone(),
            pushover,
            &config.public_url,
        );
        tracing::info!("bugs: notifications go to Pushover");
        tokio::spawn(nashcode::bugs::selfreport::quietly(
            sender.run(std::time::Duration::from_secs(5)),
        ));
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
                // Supervised, because a panicked drainer is indistinguishable from a
                // quiet one: the viewer serves every page exactly as before and the
                // buffer on the edge fills until it starts refusing envelopes. The
                // watcher costs one task and turns a silence into a line.
                let task =
                    tokio::spawn(nashcode::bugs::selfreport::quietly(drainer.run(drain.interval)));
                tokio::spawn(async move {
                    match task.await {
                        Ok(()) => tracing::error!("bugs: the drain task ended; nothing is being pulled from the ingester"),
                        Err(error) if error.is_cancelled() => {}
                        Err(error) => tracing::error!(%error, "bugs: the drain task panicked; nothing is being pulled from the ingester"),
                    }
                });
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
        brain: brain::Brain::new(upstreams.clone()),
        upstreams,
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
        eprintln!(
            "doctor: no repos seeded from NASHCODE_REPOS; discovery runs a cycle after \
             startup and the index page fills in from DGIT_URL"
        );
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
    if config.pushover.is_none() {
        eprintln!(
            "doctor: NASHCODE_PUSHOVER_TOKEN and NASHCODE_PUSHOVER_USER are unset; a new \
             issue or a regression is recorded and shown, and nothing leaves the box"
        );
    } else if std::env::var("NASHCODE_URL").is_err() {
        // Every notification carries a link, and with no NASHCODE_URL that link is the
        // bind address — which resolves to the phone that is reading it. Silent until
        // somebody taps one, which is the worst moment to find out.
        eprintln!(
            "doctor: Pushover is on and NASHCODE_URL is unset, so every notification \
             will link to {}, which is this box and not a URL a phone can open. Set \
             NASHCODE_URL to the tailnet address.",
            config.public_url
        );
    }
    if config.bugs_self_dsn.is_some() && std::env::var("NASHCODE_RELEASE").is_err() {
        eprintln!(
            "doctor: NASHCODE_BUGS_SELF_DSN is set and NASHCODE_RELEASE is not, so \
             nashcode's own issues carry no release and their source snippets read the \
             default-branch tip rather than the deployed commit"
        );
    }
    if config.bugs_self_dsn.is_none() {
        eprintln!(
            "doctor: NASHCODE_BUGS_SELF_DSN is unset; nashcode does not report its own \
             errors anywhere"
        );
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
