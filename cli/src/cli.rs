//! The command surface.
//!
//! Help text here is the CLI's documentation. It assumes the reader has never
//! heard of celld, so the top-level `--help` explains the architecture before
//! it lists commands.

use clap::{Args, Parser, Subcommand, ValueEnum};

pub const ABOUT: &str = "Run your own git host on your own box, behind your own tailnet.";

pub const LONG_ABOUT: &str = "\
Run your own git host on your own box, behind your own tailnet.

The stack has four parts. dgit is a git server written for Cloudflare Workers:
each repository is a Durable Object, a small isolated server with its own
SQLite database, and it speaks ordinary git-over-HTTPS to a stock git client.
celld runs that same Worker code on a machine you own, so no Cloudflare account
is involved. celld keeps no local database of record: every repository's SQLite
data replicates to an S3-compatible bucket you own, which is both the storage
and the coordinator, so the box is disposable and a rebuilt box comes back with
the same repositories. Tailscale is the perimeter: celld listens only on
127.0.0.1, and `tailscale serve` fronts it with HTTPS on your tailnet, so the
server has no public port at all and only your devices can reach it.

nashgit builds that from nothing. `nashgit setup` asks for an SSH destination
and a bucket, installs the pieces over SSH, deploys dgit, writes the systemd
unit, joins the tailnet, and proves the result with a real push and clone.
After that it is a small client: create repositories, list them, wire remotes,
run garbage collection, and check the deployment's health.

Everything server-side runs through the system `ssh`, so your SSH config,
your agent, and your jump hosts keep working, and nashgit never holds a key.
Every command takes --json for scripts and agents.

Getting started:
  nashgit setup                    build a deployment and save it as a profile
  nashgit init                     version the current folder on that server
  nashgit doctor                   check that everything still works";

#[derive(Debug, Parser)]
#[command(
    name = "nashgit",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    propagate_version = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Emit one JSON value on stdout instead of human text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Act on this profile instead of the active one.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Suppress progress notes on stderr.
    #[arg(long, short, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build a deployment on a host you can SSH to.
    #[command(long_about = "\
Build a deployment on a host you can SSH to.

Seven steps, each of which can be re-run safely:

  1. Host     check the SSH destination answers, has systemd, and can reach
              root through sudo without a password.
  2. Bucket   choose the S3-compatible bucket celld will use as its store.
              Only stores with working conditional writes are offered; see
              https://celld.dev/docs/fencing.
  3. Install  tailscale, celld, node + npm + esbuild, and git. Anything
              already present is left alone.
  4. Deploy   clone dgit, npm install, generate a push token, patch
              wrangler.celld.jsonc, `celld deploy`, then write and start a
              systemd unit that runs celld on 127.0.0.1:8080.
  5. Tailnet  `tailscale up` (a login URL is printed if one is needed), then
              `tailscale serve` to publish HTTPS on the tailnet.
  6. Verify   push a temporary repository with the token, clone it back with
              no credentials, delete it.
  7. Profile  save the result locally so every other command can find it.

Every prompt has a flag, so the whole wizard runs unattended. Bucket
credentials are read from the environment when the flags are absent, so they
need never appear in a shell history: AWS_ACCESS_KEY_ID and
AWS_SECRET_ACCESS_KEY.")]
    Setup(SetupArgs),

    /// Make a saved profile the active one.
    Use(UseArgs),

    /// List saved profiles.
    Profiles,

    /// Print the push token for a profile, for CI or a credential helper.
    #[command(long_about = "\
Print the push token for a profile.

This is the one command that writes a secret to stdout. The token is dgit's
GIT_TOKEN: HTTP Basic with any username and this as the password authorises a
push, and it also authorises the admin calls behind `nashgit rm`, `gc`, and
`desc`. Treat it like a deploy key.")]
    Token,

    /// Version the current directory: create the repository and push it.
    #[command(long_about = "\
Version the current directory on the active profile's server.

Point it at a folder of text files and the folder becomes a repository:

  1. create the repository on the server (PUT /<name>/config);
  2. initialise a working copy if the folder has none — jj when jj is on
     PATH (`jj git init --colocate`), otherwise git;
  3. wire `origin` and store the push token in git's credential helper;
  4. commit anything uncommitted and push.

The default name is the directory's name. Re-running is safe: an existing
repository is described, not replaced, and an existing working copy is left
as it is.")]
    Init(InitArgs),

    /// Create an empty repository on the server.
    #[command(long_about = "\
Create an empty repository on the server.

dgit creates a repository on first push, but that leaves it with no
description and no section, so this instead calls PUT /<name>/config, which
both creates and describes it. When run inside a working copy, it also wires
`origin` and hands the token to git's credential helper, so the token stays
out of the remote URL and out of your shell history.")]
    New(NewArgs),

    /// List the repositories on the server.
    #[command(long_about = "\
List the repositories on the server.

Public repositories only: dgit's index page filters private ones out before
rendering, even for an authenticated request, and the index page is the only
list dgit has. A private repository is still there — clone it by name.

dgit publishes no JSON list endpoint, so this reads the index page and parses
the listing table. The parse is deliberately forgiving of markup changes; if a
future dgit breaks it, `--json` will return an empty list rather than wrong
data.")]
    Ls,

    /// Clone a repository from the server.
    Clone(CloneArgs),

    /// Delete a repository from the server.
    Rm(RmArgs),

    /// Prune unreachable objects in a repository.
    #[command(long_about = "\
Prune unreachable objects in a repository (POST /<name>/gc).

dgit also runs this by itself, from a timer, after a forced update or a ref
deletion, so it is rarely needed by hand. Objects inside stored packfiles are
kept: dgit cannot delete from the middle of a pack without repacking it.")]
    Gc(GcArgs),

    /// Set a repository's description, owner, section, or private flag.
    Desc(DescArgs),

    /// Point this working copy's `origin` at the server.
    #[command(long_about = "\
Point this working copy's `origin` at the active profile's server.

Uses `jj git remote` in a jj repository and `git remote` in a git one. The
token goes to git's credential helper, never into the remote URL, so `git
remote -v` and any file you commit stay free of secrets.")]
    Remote(RemoteArgs),

    /// Give someone their own push token, list them, or take one back.
    #[command(long_about = "\
Give someone their own push token, list the invited, or take a token back.

dgit accepts extra push tokens from its GIT_TOKENS var, comma-separated,
alongside the main GIT_TOKEN. dgit only sees that flat list, so nashgit keeps
a name → token mapping on the host (~/git-invites.toml, mode 0600) and
regenerates GIT_TOKENS from it on every change, then redeploys the Worker and
restarts celld — the same mechanism `setup` uses. Each change ends with an
auth probe: an invite proves the new token is accepted, a revoke proves the
old one no longer is.

`nashgit invite <name>` prints the token once, with the remote URL and the
one-liner that stores it via `git credential approve`. Re-inviting a name
rotates that person's token. `--list` prints names only, never tokens.

A token is push access, not network access. Reaching the server at all is
Tailscale's job: add the person to your tailnet or share the node with them.
nashgit does not touch Tailscale ACLs.")]
    Invite(InviteArgs),

    /// Check that the deployment still works, one line per check.
    #[command(long_about = "\
Check a deployment, one line per check.

Local checks always run: the profile exists, the server answers, its TLS
certificate is trusted, and the token is accepted. The token check is a
request dgit rejects on grounds of a malformed body, so it proves the
credential without creating or changing anything.

Host checks run over SSH when the profile has an ssh destination: the celld
service is active, it answers on loopback, the node is on the tailnet, the
`tailscale serve` proxy that injects the identity headers is configured, and
celld can reach the bucket. Checks that cannot run are reported as skipped,
never as passing.")]
    Doctor,

    /// Work with plan files under plans/.
    #[command(subcommand, long_about = "\
Work with plan files under plans/.

A plan is a markdown file in `plans/` at the root of a repository. It is a
convention, not a format: the viewer renders the directory, and humans comment
on the rendered page. `nashgit annotate` opens one locally, and
`nashgit comments` pulls the replies back down.")]
    Plan(PlanCommand),

    /// Open a plan in plannotator for local review.
    #[command(long_about = "\
Open a plan file in plannotator, a local annotation tool.

plannotator is a separate program. When it is not installed, this prints where
to get it and does nothing else. When the active profile has a viewer URL, the
plan's URL on the viewer is printed too, which is the link to send to someone
who should comment on it.")]
    Annotate(AnnotateArgs),

    /// Read the human comments left on a plan in the viewer.
    #[command(long_about = "\
Read the human comments left on a plan in the viewer.

This is the return path for an agent that writes plans: the agent creates a
plan, a person comments on it in the viewer's web interface, and this command
brings those comments back as text or JSON.

It reads GET /<repo>/comments from the viewer, which is a different service
from dgit and has its own URL. The profile must therefore have a viewer URL;
`nashgit setup --viewer` records one.")]
    Comments(CommentsArgs),
}

#[derive(Debug, Subcommand)]
pub enum PlanCommand {
    /// Create plans/<slug>.md from a template.
    New(PlanNewArgs),
}

/// Object stores celld can use safely.
///
/// The list is short on purpose. celld's ownership records need conditional
/// writes and read-after-write consistency; a store without them lets two
/// nodes own one repository at the same time, and it fails silently rather
/// than loudly. See https://celld.dev/docs/fencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Provider {
    /// Amazon S3. No endpoint needed; set a real region.
    #[value(name = "aws-s3")]
    AwsS3,
    /// Cloudflare R2. Region is `auto`; endpoint is your account's S3 API URL.
    #[value(name = "r2")]
    R2,
    /// Tigris. Region is `auto`; endpoint is https://t3.storage.dev.
    #[value(name = "tigris")]
    Tigris,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::AwsS3 => "Amazon S3",
            Provider::R2 => "Cloudflare R2",
            Provider::Tigris => "Tigris",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Provider::AwsS3 => "aws-s3",
            Provider::R2 => "r2",
            Provider::Tigris => "tigris",
        }
    }

    /// The region celld should use when the user does not name one.
    pub fn default_region(self) -> &'static str {
        match self {
            Provider::AwsS3 => "us-east-1",
            Provider::R2 | Provider::Tigris => "auto",
        }
    }

    /// The endpoint, where it is fixed or has an obvious shape.
    pub fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Provider::AwsS3 => None,
            Provider::R2 => None, // https://<account-id>.r2.cloudflarestorage.com
            Provider::Tigris => Some("https://t3.storage.dev"),
        }
    }

    pub fn endpoint_hint(self) -> &'static str {
        match self {
            Provider::AwsS3 => "leave blank for Amazon S3",
            Provider::R2 => "https://<account-id>.r2.cloudflarestorage.com",
            Provider::Tigris => "https://t3.storage.dev",
        }
    }

    pub fn all() -> &'static [Provider] {
        &[Provider::AwsS3, Provider::R2, Provider::Tigris]
    }
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// SSH destination of the host, e.g. me@build-box.
    #[arg(long, value_name = "USER@HOST")]
    pub host: Option<String>,

    /// Name to save the deployment under. Defaults to the host's short name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Object-store provider.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,

    /// Bucket name, or a full celld bucket URL such as s3://my-cells/prefix.
    #[arg(long, value_name = "BUCKET")]
    pub bucket: Option<String>,

    /// Storage region. `auto` for R2 and Tigris.
    #[arg(long, value_name = "REGION")]
    pub region: Option<String>,

    /// S3 endpoint URL. Leave unset for Amazon S3.
    #[arg(long, value_name = "URL")]
    pub endpoint: Option<String>,

    /// Access key id. Falls back to $AWS_ACCESS_KEY_ID.
    #[arg(long, value_name = "ID")]
    pub access_key_id: Option<String>,

    /// Secret access key. Prefer $AWS_SECRET_ACCESS_KEY: a flag lands in your
    /// shell history.
    #[arg(long, value_name = "SECRET")]
    pub secret_access_key: Option<String>,

    /// The host already has bucket credentials (instance role, ~/.aws, ADC).
    #[arg(long, conflicts_with_all = ["access_key_id", "secret_access_key"])]
    pub creds_on_host: bool,

    /// Site name shown in the web interface. Defaults to the tailnet hostname.
    #[arg(long, value_name = "NAME")]
    pub site_name: Option<String>,

    /// Site description shown under the title.
    #[arg(long, value_name = "TEXT")]
    pub site_desc: Option<String>,

    /// Default owner shown against repositories.
    #[arg(long, value_name = "NAME")]
    pub site_owner: Option<String>,

    /// Use this push token instead of generating one.
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,

    /// Tailscale auth key, so `tailscale up` needs no browser.
    /// Prefer $TS_AUTHKEY.
    #[arg(long, value_name = "KEY")]
    pub tailscale_authkey: Option<String>,

    /// Also publish the nashgit viewer on HTTPS :8443.
    #[arg(long)]
    pub viewer: bool,

    /// Loopback port the viewer listens on.
    #[arg(long, default_value_t = 8090, value_name = "PORT")]
    pub viewer_port: u16,

    /// Loopback port celld listens on.
    #[arg(long, default_value_t = 8080, value_name = "PORT")]
    pub listen_port: u16,

    /// Skip the end-to-end push/clone check.
    #[arg(long)]
    pub skip_verify: bool,

    /// Print every remote script instead of running it. Touches nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Never prompt. Missing answers become errors instead of questions.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct UseArgs {
    /// Profile to activate.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Repository name. Defaults to the current directory's name.
    pub name: Option<String>,

    /// Hide the repository from the index and require the token to read it.
    #[arg(long)]
    pub private: bool,

    /// One-line description.
    #[arg(long, value_name = "TEXT")]
    pub desc: Option<String>,

    /// Section heading to file it under on the index page.
    #[arg(long, value_name = "NAME")]
    pub section: Option<String>,

    /// Create the working copy with git even when jj is available.
    #[arg(long, conflicts_with = "jj")]
    pub git: bool,

    /// Create the working copy with jj (colocated). Default when jj is on PATH.
    #[arg(long)]
    pub jj: bool,

    /// Create and wire the repository, but do not commit or push.
    #[arg(long)]
    pub no_push: bool,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    /// Repository name. Letters, digits, dot, dash, underscore.
    pub name: String,

    /// Hide the repository from the index and require the token to read it.
    #[arg(long)]
    pub private: bool,

    /// One-line description.
    #[arg(long, value_name = "TEXT")]
    pub desc: Option<String>,

    /// Section heading to file it under on the index page.
    #[arg(long, value_name = "NAME")]
    pub section: Option<String>,

    /// Owner shown against the repository.
    #[arg(long, value_name = "NAME")]
    pub owner: Option<String>,

    /// Do not touch the current directory's working copy.
    #[arg(long)]
    pub no_remote: bool,

    /// After wiring the remote, make the working copy a colocated jj repo.
    /// On by default when $NASHGIT_JJ=1.
    #[arg(long)]
    pub jj: bool,
}

#[derive(Debug, Args)]
pub struct CloneArgs {
    /// Repository name on the server.
    pub name: String,

    /// Directory to clone into. Defaults to the repository name.
    pub dir: Option<String>,

    /// Make the clone a colocated jj repo. On by default when $NASHGIT_JJ=1.
    #[arg(long)]
    pub jj: bool,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Repository to delete.
    pub name: String,

    /// Do not ask for confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Repository to sweep.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct DescArgs {
    /// Repository to change.
    pub name: String,

    /// New description.
    #[arg(long, value_name = "TEXT")]
    pub desc: Option<String>,

    /// New section heading.
    #[arg(long, value_name = "NAME")]
    pub section: Option<String>,

    /// New owner.
    #[arg(long, value_name = "NAME")]
    pub owner: Option<String>,

    /// Hide it from the index and gate reads behind the token.
    #[arg(long, conflicts_with = "public")]
    pub private: bool,

    /// Show it on the index and allow anonymous reads.
    #[arg(long)]
    pub public: bool,
}

#[derive(Debug, Args)]
pub struct RemoteArgs {
    /// Repository name on the server. Defaults to the directory's name.
    pub name: Option<String>,
}

#[derive(Debug, Args)]
pub struct InviteArgs {
    /// Person to invite. Letters, digits, dash, underscore.
    /// Re-inviting a name rotates their token.
    #[arg(conflicts_with_all = ["list", "revoke"])]
    pub name: Option<String>,

    /// List the invited names. Never prints tokens.
    #[arg(long, conflicts_with = "revoke")]
    pub list: bool,

    /// Remove this person's token, redeploy, and verify it now fails.
    #[arg(long, value_name = "NAME")]
    pub revoke: Option<String>,
}

#[derive(Debug, Args)]
pub struct PlanNewArgs {
    /// Plan title. The filename is a slug of it.
    pub title: Vec<String>,
}

#[derive(Debug, Args)]
pub struct AnnotateArgs {
    /// Path to the plan, e.g. plans/rewrite-the-parser.md.
    pub file: String,
}

#[derive(Debug, Args)]
pub struct CommentsArgs {
    /// Plan file the comments are attached to, e.g. plans/rewrite.md.
    pub file: String,

    /// Only comments left against this branch.
    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Only comments newer than this RFC 3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub since: Option<String>,

    /// Repository on the viewer. Defaults to the name `origin` points at.
    #[arg(long, value_name = "NAME")]
    pub repo: Option<String>,
}
