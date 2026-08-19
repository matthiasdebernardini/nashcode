use clap::Parser;
use nashcode_cli::cli::{Cli, Command, PlanCommand};
use nashcode_cli::commands::{Ctx, brain, code, doctor, grep, invite, plan, profiles, repo, setup};
use nashcode_cli::output::Out;

fn main() {
    let cli = Cli::parse();
    // `grep` takes its whole argument list unparsed, so a `--json` typed after the
    // subcommand — which is where an agent types it — lands in grep's list rather than
    // in clap's. Grep's own parse is what decides the mode in that case.
    let json = cli.json || matches!(&cli.command, Command::Grep(a) if grep::wants_json(a));
    let out = Out::new(json, cli.quiet);
    let ctx = Ctx {
        out,
        profile_name: cli.profile.clone(),
    };

    let result = match &cli.command {
        Command::Setup(a) => setup::run(&ctx, a),
        Command::Use(a) => profiles::use_profile(&ctx, &a.name),
        Command::Profiles => profiles::list(&ctx),
        Command::Token => profiles::token(&ctx),
        Command::Init(a) => repo::init(&ctx, a),
        Command::New(a) => repo::new(&ctx, a),
        Command::Ls => repo::ls(&ctx),
        Command::Clone(a) => repo::clone(&ctx, a),
        Command::Rm(a) => repo::rm(&ctx, a),
        Command::Gc(a) => repo::gc(&ctx, a),
        Command::Desc(a) => repo::desc(&ctx, a),
        Command::Remote(a) => repo::remote(&ctx, a),
        Command::Invite(a) => invite::run(&ctx, a),
        Command::Doctor => doctor::run(&ctx),
        Command::Plan(PlanCommand::New(a)) => plan::new(&ctx, a),
        Command::Annotate(a) => plan::annotate(&ctx, a),
        Command::Comments(a) => plan::comments(&ctx, a),
        Command::Index(a) => code::run(&ctx, a),
        Command::Brain(a) => brain::run(&ctx, a),
        Command::Grep(a) => grep::run(&ctx, a),
    };

    if let Err(e) = result {
        // Human diagnosis goes to stderr in both modes. In --json mode stdout
        // still gets exactly one JSON value, so a pipeline never parses air.
        eprintln!("error: {e:#}");
        if out.is_json() {
            out.json(&serde_json::json!({ "error": format!("{e:#}") }));
        }
        std::process::exit(1);
    }
}
