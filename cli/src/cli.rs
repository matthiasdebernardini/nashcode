//! The command surface.
//!
//! Built on `agcli`: every command answers with one JSON envelope on stdout and
//! a typed exit code, and the reserved agent flags (`--select`, `--compact`,
//! `--quiet`, `--yes`, `--dry-run`, and a `--json` that is accepted and ignored)
//! come free. `grep` is the one exception — it owes callers ripgrep's
//! `path:line:content` and ripgrep's exit codes, so it is a raw passthrough
//! command that owns its own argv and its own stdout.
//!
//! The descriptions here are the CLI's documentation. They assume the reader
//! has never heard of celld, so the root tree explains the architecture before
//! it lists commands.

use crate::commands::{
    Ctx, brain, card, code, context, doctor, grep, invite, people, plan, profiles, repo,
    setup,
};
use crate::exit::{Class, class_of};
use crate::output::Out;
use agcli::{AgentCli, Command, CommandError, CommandOutput, CommandRequest, ExitCode, NextAction};

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

nashcode builds that from nothing. `nashcode setup` asks for an SSH destination
and a bucket, installs the pieces over SSH, deploys dgit, writes the systemd
unit, joins the tailnet, and proves the result with a real push and clone.
After that it is a small client: create repositories, list them, wire remotes,
run garbage collection, and check the deployment's health.

Everything server-side runs through the system `ssh`, so your SSH config,
your agent, and your jump hosts keep working, and nashcode never holds a key.

Nobody types this CLI: an agent drives it. Every command answers with one JSON
envelope on stdout and a typed exit code — 0 success, 2 usage, 3 not found,
4 auth, 5 upstream API — and every error carries a `fix` you can run. Progress
notes go to stderr. `grep` is the exception: it speaks ripgrep's output and
ripgrep's exit codes, because that is what makes it usable on reflex.

Getting started:
  nashcode setup                    build a deployment and save it as a profile
  nashcode init                     version the current folder on that server
  nashcode doctor                   check that everything still works";

// --- shared help prose ------------------------------------------------------

const SETUP_DOC: &str = "\
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

Nothing is ever asked interactively: every answer is a flag, and a missing one
is a usage error naming the flag. Bucket credentials are read from the
environment when the flags are absent, so they need never appear in a shell
history: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY. A Tailscale auth key can
come from TS_AUTHKEY the same way.

`--dry-run` returns every remote script it would have run and touches nothing —
no host, no profile file. Those scripts are the real ones, so they carry the
generated push token and any bucket credentials in them. Read the preview; do
not paste it anywhere.

The flags, where the name does not say it:
  --host             SSH destination, e.g. me@build-box.
  --name             what to save the deployment as. Defaults to the host's
                     short name (me@build-box.ts.net -> build-box).
  --provider         aws-s3, r2, or tigris. Nothing else is offered; see
                     https://celld.dev/docs/fencing for why.
  --bucket           a name, or a full celld bucket URL: s3://my-cells/prefix.
  --region           `auto` for R2 and Tigris; a real region for Amazon S3.
  --endpoint         the S3 API URL. Unset for Amazon S3, https://t3.storage.dev
                     for Tigris, https://<account-id>.r2.cloudflarestorage.com
                     for R2.
  --access-key-id    falls back to $AWS_ACCESS_KEY_ID.
  --secret-access-key falls back to $AWS_SECRET_ACCESS_KEY. Prefer the
                     environment: a flag lands in your shell history.
  --creds-on-host    the host already has credentials (instance role, ~/.aws,
                     ADC). Conflicts with the two above.
  --site-name        shown in the web interface. Defaults to the tailnet
                     hostname.
  --site-desc        shown under the title.
  --site-owner       shown against repositories by default.
  --token            use this push token instead of generating one. Careful: a
                     flag lands in your shell history; a generated one does not.
  --tailscale-authkey so `tailscale up` needs no browser. Prefer $TS_AUTHKEY.
  --viewer           also publish the nashcode viewer on HTTPS :8443.
  --viewer-port      loopback port the viewer listens on. Default 8090.
  --listen-port      loopback port celld listens on. Default 8080.
  --skip-verify      skip the end-to-end push/clone check (step 6).";

const TOKEN_DOC: &str = "\
Print the push token for a profile.

This is the command whose whole purpose is to write a secret into the envelope.
The token is dgit's GIT_TOKEN: HTTP Basic with any username and this as the
password authorises a push, and it also authorises the admin calls behind
`nashcode rm`, `gc`, and `desc`. Treat it like a deploy key.

One other command can emit secrets, and it is worth knowing which: `setup
--dry-run` returns the remote scripts it would have run, and those scripts carry
the generated GIT_TOKEN and any bucket credentials you passed, because that is
what they would have written on the host. A dry run is for reading, not for
pasting into an issue.";

const INIT_DOC: &str = "\
Version the current directory on the active profile's server.

Point it at a folder of text files and the folder becomes a repository:

  1. create the repository on the server (PUT /<name>/config);
  2. initialise a working copy if the folder has none — jj when jj is on
     PATH (`jj git init --colocate`), otherwise git;
  3. wire `origin` and store the push token in git's credential helper;
  4. commit anything uncommitted and push.

The default name is the directory's name; letters, digits, dot, dash and
underscore, starting with a letter or digit, is what dgit allows.

  --private   hide it from the index and require the token to read it.
  --desc      a one-line description.
  --section   the heading to file it under on the index page.
  --git       create the working copy with git even when jj is on PATH.
  --jj        create it with jj, colocated. The default when jj is on PATH.
  --no-push   create and wire the repository, but do not commit or push.

Re-running is safe: an existing repository is described, not replaced, and an
existing working copy is left as it is.";

const NEW_DOC: &str = "\
Create an empty repository on the server.

dgit creates a repository on first push, but that leaves it with no
description and no section, so this instead calls PUT /<name>/config, which
both creates and describes it. When run inside a working copy, it also wires
`origin` and hands the token to git's credential helper, so the token stays
out of the remote URL and out of your shell history.

  <name>        letters, digits, dot, dash, underscore; starting with a letter
                or digit. Anything else is refused before it becomes a URL path.
  --private     hide it from the index and require the token to read it.
  --desc        a one-line description.
  --section     the heading to file it under on the index page.
  --owner       shown against the repository. Defaults to the profile's owner.
  --no-remote   do not touch the current directory's working copy.
  --jj          after wiring the remote, make the working copy a colocated jj
                repo. On by default when $NASHCODE_JJ=1; an explicit --jj with
                no jj on PATH is an error, the env-var default only warns.";

const LS_DOC: &str = "\
List the repositories on the server.

Public repositories only: dgit's index page filters private ones out before
rendering, even for an authenticated request, and the index page is the only
list dgit has. A private repository is still there — clone it by name.

dgit publishes no JSON list endpoint, so this reads the index page and parses
the listing table. The parse is deliberately forgiving of markup changes; if a
future dgit breaks it, the result is an empty list rather than wrong data.";

const GC_DOC: &str = "\
Prune unreachable objects in a repository (POST /<name>/gc).

dgit also runs this by itself, from a timer, after a forced update or a ref
deletion, so it is rarely needed by hand. Objects inside stored packfiles are
kept: dgit cannot delete from the middle of a pack without repacking it.";

const REMOTE_DOC: &str = "\
Point this working copy's `origin` at the active profile's server.

Uses `jj git remote` in a jj repository and `git remote` in a git one. The
token goes to git's credential helper, never into the remote URL, so `git
remote -v` and any file you commit stay free of secrets.";

const INVITE_DOC: &str = "\
Give someone their own push token, list the invited, or take a token back.

dgit accepts extra push tokens from its GIT_TOKENS var, comma-separated,
alongside the main GIT_TOKEN. dgit only sees that flat list, so nashcode keeps
a name → token mapping on the host (~/git-invites.toml, mode 0600) and
regenerates GIT_TOKENS from it on every change, then redeploys the Worker and
restarts celld — the same mechanism `setup` uses. Each change ends with an
auth probe: an invite proves the new token is accepted, a revoke proves the
old one no longer is.

`nashcode invite <name>` returns the token once, with the remote URL and the
one-liner that stores it via `git credential approve`. A name is letters,
digits, dash and underscore, starting with a letter or digit — the alphabet is
small because the name reaches a shell pattern on the host. Re-inviting a name
rotates that person's token. `--list` returns names only, never tokens.
`--revoke <name>` removes that person's token, redeploys, and verifies the old
one now fails.

A token is push access, not network access. Reaching the server at all is
Tailscale's job: add the person to your tailnet or share the node with them.
nashcode does not touch Tailscale ACLs.";

const PLAN_DOC: &str = "\
Work with plan files under plans/.

A plan is a markdown file in `plans/` at the root of a repository. It is a
convention, not a format: the viewer renders the directory, and humans comment
on the rendered page. `nashcode annotate` opens one locally, and
`nashcode comments` pulls the replies back down.";

const ANNOTATE_DOC: &str = "\
Open a plan file in plannotator, a local annotation tool, and carry the answer
back to the viewer.

plannotator is a separate program. When it is not installed, this reports where
to get it and does nothing else. When the active profile has a viewer URL, the
plan's URL on the viewer comes back too, which is the link to send to someone
who should comment on it.

The agent launches plannotator FOR the human, so launching is the default. What
the human writes is posted back to the viewer as one comment on the plan, so the
agent polling `nashcode comments` hears it. Approving posts too: `Approved.`,
plus the notes when there are any. Dismissing posts nothing. If the comment
cannot be sent, the feedback comes back in the error rather than being lost.

`--no-launch` is the inspect-only form: it launches nothing and only reports the
file, where plannotator is (null when absent), and the viewer URL.";

const INDEX_DOC: &str = "\
Ask the viewer to rebuild a repository's code index, and report what it holds.

The viewer indexes by itself, every time a merge lands on the default branch, so
this is for the two cases it does not cover: the first index of a repository
pushed before the viewer knew how to index one, and a rebuild after a change to
how indexing works.

It queues and returns. Indexing runs on the viewer's own job queue, never on a
request, so the answer is `queued` rather than `done`. Run it again with --status
to report what the index holds without queueing another run, or read
GET /<repo>/code yourself. The repository defaults to the name `origin` points
at.

The first index run on a fresh box downloads the embedding model, which is a few
hundred megabytes. Until that lands, semantic search reports itself unavailable
and text search carries on working.

Three indexes come out of it: `git grep` for exact text (no stored index at all),
vectors for `what is this about`, and a symbol graph for `who calls this`. Query
them at GET /<repo>/code/text, /code/similar, /code/def, /code/refs, and
/code/callers, or ask POST /brain/ask, which can call all five.";

const COMMENTS_DOC: &str = "\
Read the human comments left on a plan in the viewer.

This is the return path for an agent that writes plans: the agent creates a
plan, a person comments on it in the viewer's web interface, and this command
brings those comments back as a bounded list of rows.

It reads GET /<repo>/comments from the viewer, which is a different service
from dgit and has its own URL. The profile must therefore have a viewer URL;
`nashcode setup --viewer` records one.";

const CONTEXT_DOC: &str = "\
File and read what the work is about.

A meeting, an email, a pasted chat, or a note becomes one committed markdown
file in the repository it concerns, at context/<kind>/YYYY/MM/<id>.md on the
default branch. Four kinds: meeting, email, chat, note. The server accepts files
and indexes them; it never fetches email, chat, or audio.

  put   file one item. The text is the named file, or standard input.
  ls    walk what is filed, oldest ingest first.
  get   read one item back: front matter and body.

Put is safe to re-run. With --source — the provider's stable id, a Gmail message
id or a chat thread plus day or a URL — the same item always names the same
file, so a pusher that died before it wrote its marker gets `existing: true`
next run instead of a second copy.

A digest on the operator's machine turns these files into brain/entities/, the
memory `nashcode brain` prints before a session searches. Nothing here runs it.

Context lives in the viewer, so the profile needs a viewer URL:
`nashcode setup --viewer` records one.";

const PEOPLE_DOC: &str = "\
Say who belongs to which project, so every inbox routes by who wrote.

One file — ~/.nashcode/people.json, or $NASHCODE_PEOPLE, or --file — lists people
and projects. A person has an id, a name, phones in E.164, and emails. A project
has an id, a folder, an optional nashcode repo, and the ids of the people who ask
about it. Nothing else joins a phone number to a project.

  ls      every project, who is in it, and who is in no project.
  route   which project these contacts are about, best first.
  push    give the viewer a copy, so the meeting extension can ask too.
  check   everything wrong with the file. Non-zero when there is anything.
  import  build the file once from the old per-inbox lists. One-shot.

Routing is one rule: a project scores one point per distinct person any contact
matches, by email or by phone. Equal scores keep file order and come back with
tie: true, which means nothing here decides — ask a human. Your own addresses
never score.

The file stays on this machine. `push` sends the viewer a copy so a browser can
ask the same question; the viewer has no route that hands it back.";

const BRAIN_DOC: &str = "\
Read the viewer's work state and return the short version.

GET /brain is an aggregate: every branch, every plan, every card and a hundred
activity rows per repository. This command is the digest of it — branches with
their tip and CI state, what the code index holds and how old it is, the plan
files and how many comments wait on them, the latest architecture submission,
and the last five things that happened. The result is that digest, not the raw
stanza; `curl /brain` is still there when you want the whole thing.

The repository defaults to the name `origin` points at, the way `comments`
resolves it. Run it outside a repository and it digests every repository the
viewer knows, up to twenty, saying how many it left out.

It always exits 0. This is meant for a session-start hook, so a viewer that is
down, unreachable, or not yet configured comes back as `status: unavailable`
with the reason, and gets out of the way.";

/// `doctor` is agcli's own command: it owns the description, the report shape,
/// and the exit code a failing check drives. Its prose therefore has nowhere to
/// live on the command itself, so it rides in the root tree as a `doctor` field,
/// where an introspecting agent still finds it.
const DOCTOR_DOC: &str = "\
Check a deployment, one entry per check.

Local checks always run: the profile exists, the server answers, its TLS
certificate is trusted, and the token is accepted. The token check is a
request dgit rejects on grounds of a malformed body, so it proves the
credential without creating or changing anything.

Host checks run over SSH when the profile has an ssh destination: the celld
service is active, it answers on loopback, the node is on the tailnet, the
`tailscale serve` proxy that injects the identity headers is configured, and
celld can reach the bucket. Checks that cannot run are reported as skipped,
never as passing.";

/// `grep`'s own help. It is a raw command, so the framework never prints help
/// for it: `nashcode grep --help` reaches the handler and this is what it
/// writes.
pub const GREP_DOC: &str = "\
Search the code. The surface is ripgrep's, the answers come from the index.

  nashcode grep retry
  nashcode grep -i -C2 -t rust 'fn connect' src/

Flags mirror rg where they matter: -i, -n (always on), -l, -C/-A/-B, -t <type>,
-g <glob>, plus --json and --repo. Any other flag you type out of rg habit is
ignored rather than refused, so a command that works in rg works here.

Output is grep's: one hit per line as path:line:content, context lines as
path-line-content. Everything the index adds rides in `#` comment lines. A
header names the indexed commit and its age. A `# definitions:` block comes
first — each definition with its kind and its reference and caller counts —
then the text hits, then, only when the text pass found nothing, a
`# semantic (no exact match):` block from the embeddings.

Freshness is hybrid. Text hits come from a real rg run over your working tree
whenever you are inside a checkout with rg on PATH, because the tree you are
editing is always fresher than the index; definitions, counts and semantic hits
come from GET /<repo>/code/find. Outside a checkout, or with no rg, the text
pass falls back to the index.

Exit codes are grep's: 0 when something matched, 1 when nothing did. A viewer
that cannot be reached degrades to plain local rg with one `#` line saying so.
Only having neither a checkout nor an index is an error, and that exits 2.

This command bypasses the JSON envelope on purpose — grep's output IS its
contract. `nashcode grep --json` gives the same facts as one JSON value.";

// --- the provider list ------------------------------------------------------

/// Object stores celld can use safely.
///
/// The list is short on purpose. celld's ownership records need conditional
/// writes and read-after-write consistency; a store without them lets two
/// nodes own one repository at the same time, and it fails silently rather
/// than loudly. See https://celld.dev/docs/fencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Amazon S3. No endpoint needed; set a real region.
    AwsS3,
    /// Cloudflare R2. Region is `auto`; endpoint is your account's S3 API URL.
    R2,
    /// Tigris. Region is `auto`; endpoint is https://t3.storage.dev.
    Tigris,
}

impl Provider {
    pub fn from_id(id: &str) -> Option<Provider> {
        match id {
            "aws-s3" | "aws" | "s3" => Some(Provider::AwsS3),
            "r2" => Some(Provider::R2),
            "tigris" => Some(Provider::Tigris),
            _ => None,
        }
    }

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

    /// The region celld should use when the caller does not name one.
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
            Provider::AwsS3 => "leave unset for Amazon S3",
            Provider::R2 => "https://<account-id>.r2.cloudflarestorage.com",
            Provider::Tigris => "https://t3.storage.dev",
        }
    }

    pub fn all() -> &'static [Provider] {
        &[Provider::AwsS3, Provider::R2, Provider::Tigris]
    }
}

// --- argument records -------------------------------------------------------
//
// These stay plain structs. The command bodies read them, the handlers below
// fill them from the parsed request, and neither half has to know how the other
// works.

#[derive(Debug, Default)]
pub struct SetupArgs {
    pub host: Option<String>,
    pub name: Option<String>,
    pub provider: Option<Provider>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub creds_on_host: bool,
    pub site_name: Option<String>,
    pub site_desc: Option<String>,
    pub site_owner: Option<String>,
    pub token: Option<String>,
    pub tailscale_authkey: Option<String>,
    pub viewer: bool,
    pub viewer_port: u16,
    pub listen_port: u16,
    pub skip_verify: bool,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct InitArgs {
    pub name: Option<String>,
    pub private: bool,
    pub desc: Option<String>,
    pub section: Option<String>,
    pub git: bool,
    pub jj: bool,
    pub no_push: bool,
}

#[derive(Debug, Default)]
pub struct NewArgs {
    pub name: String,
    pub private: bool,
    pub desc: Option<String>,
    pub section: Option<String>,
    pub owner: Option<String>,
    pub no_remote: bool,
    pub jj: bool,
}

#[derive(Debug, Default)]
pub struct CloneArgs {
    pub name: String,
    pub dir: Option<String>,
    pub jj: bool,
}

#[derive(Debug, Default)]
pub struct RmArgs {
    pub name: String,
}

#[derive(Debug, Default)]
pub struct GcArgs {
    pub name: String,
}

#[derive(Debug, Default)]
pub struct DescArgs {
    pub name: String,
    pub desc: Option<String>,
    pub section: Option<String>,
    pub owner: Option<String>,
    pub private: bool,
    pub public: bool,
}

#[derive(Debug, Default)]
pub struct RemoteArgs {
    pub name: Option<String>,
}

#[derive(Debug, Default)]
pub struct InviteArgs {
    pub name: Option<String>,
    pub list: bool,
    pub revoke: Option<String>,
}

#[derive(Debug, Default)]
pub struct PlanNewArgs {
    pub title: Vec<String>,
}

#[derive(Debug, Default)]
pub struct AnnotateArgs {
    pub file: String,
    /// Report where things are without launching plannotator.
    pub no_launch: bool,
}

#[derive(Debug, Default)]
pub struct IndexArgs {
    pub repo: Option<String>,
    pub status: bool,
}

#[derive(Debug, Default)]
pub struct BrainArgs {
    pub repo: Option<String>,
}

#[derive(Debug, Default)]
pub struct ReadyArgs {
    pub repo: Option<String>,
}

#[derive(Debug, Default)]
pub struct ClaimArgs {
    pub file: String,
}

#[derive(Debug, Default)]
pub struct CommentsArgs {
    pub file: String,
    pub branch: Option<String>,
    pub since: Option<String>,
    pub repo: Option<String>,
}

#[derive(Debug, Default)]
pub struct ContextPutArgs {
    pub kind: String,
    /// The file to file. Standard input when absent.
    pub file: Option<String>,
    pub title: Option<String>,
    pub at: Option<String>,
    pub source: Option<String>,
    pub repo: Option<String>,
}

#[derive(Debug, Default)]
pub struct ContextLsArgs {
    pub kind: Option<String>,
    pub since: Option<String>,
    pub repo: Option<String>,
}

#[derive(Debug, Default)]
pub struct ContextGetArgs {
    pub kind: String,
    pub id: String,
    pub repo: Option<String>,
}

#[derive(Debug, Default)]
pub struct PeopleLsArgs {
    pub file: Option<String>,
}

#[derive(Debug, Default)]
pub struct PeopleRouteArgs {
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub file: Option<String>,
}

#[derive(Debug, Default)]
pub struct PeoplePushArgs {
    pub file: Option<String>,
}

#[derive(Debug, Default)]
pub struct PeopleCheckArgs {
    pub file: Option<String>,
}

#[derive(Debug, Default)]
pub struct PeopleImportArgs {
    pub routes: Option<String>,
    pub context: Option<String>,
}

// --- the handler boundary ---------------------------------------------------

/// What every command gets from the request: the profile it acts on and whether
/// progress chatter is wanted.
fn context(req: &CommandRequest<'_>) -> Ctx {
    Ctx {
        out: Out::new(req.quiet()),
        profile_name: req.flag("profile").map(str::to_string),
    }
}

fn text(req: &CommandRequest<'_>, key: &str) -> Option<String> {
    req.flag(key).map(str::to_string).filter(|v| !v.is_empty())
}

/// Every value a repeated flag was given, in the order it was given.
///
/// agcli keeps flags in a map, so `--email a --email b` would arrive as `b` and
/// nothing else. One meeting has many attendees, so this reads the raw argv
/// instead. Both spellings count: `--email a` and `--email=a`.
///
/// A candidate that starts with `--` is the next flag, not this one's value:
/// `--email --json` has no address in it. Swallowing the flag would turn a typo into a
/// search for an attendee called "--json", so it is left where it is and agcli reports
/// the missing value.
fn repeated(req: &CommandRequest<'_>, key: &str) -> Vec<String> {
    let long = format!("--{key}");
    let assigned = format!("--{key}=");
    let mut values = Vec::new();
    let mut args = req.invocation().raw_args().iter().peekable();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix(&assigned) {
            values.push(value.to_owned());
        } else if arg == &long
            && args.peek().is_some_and(|next| !next.starts_with("--"))
            && let Some(value) = args.next()
        {
            values.push(value.clone());
        }
    }
    values.into_iter().map(|value| value.trim().to_owned()).filter(|value| !value.is_empty()).collect()
}

/// A declared boolean flag. agcli stores `--flag` as the string `"true"`, and an
/// explicit `--flag=false` has to keep meaning false.
fn on(req: &CommandRequest<'_>, key: &str) -> bool {
    req.flag(key).is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "false" | "0" | "no" | "off" | ""
        )
    })
}

fn port(req: &CommandRequest<'_>, key: &str, default: u16) -> u16 {
    req.flag(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The failure class of an error from a command body.
///
/// It is read from the error, not from its text. Every site that knows what kind
/// of failure it is raising says so there (see `crate::exit`), which is the only
/// place that can know: a 500 from the viewer is an upstream failure whatever
/// the body says, and a reviewer who writes "does not exist" in their notes is
/// not reporting a missing file.
fn oops(error: anyhow::Error, fix: impl Into<String>) -> CommandError {
    let message = format!("{error:#}");
    match class_of(&error) {
        Some(class) => {
            CommandError::new(message, class.code(), fix_for(class, fix)).exit_code(class.exit_code())
        }
        None => CommandError::new(message, "ERROR", fix).exit_code(ExitCode::ERROR),
    }
}

/// The command to run next. Two classes know better than the call site does.
///
/// A rejected credential is not fixed by listing repositories — `nashcode ls`
/// reads anonymously and succeeds while the push token is dead, which teaches an
/// agent exactly the wrong thing. And a deployment that is failing upstream is
/// what `doctor` exists to diagnose, whichever command tripped over it.
fn fix_for(class: Class, given: impl Into<String>) -> String {
    match class {
        Class::Auth => doctor::fix_for("token").to_string(),
        Class::Api => "nashcode doctor".to_string(),
        Class::Usage | Class::NotFound => given.into(),
    }
}

/// A bad invocation. Not a failure of the deployment, so it never carries one of
/// the other codes.
fn misuse(message: impl Into<String>, fix: impl Into<String>) -> CommandError {
    CommandError::new(message, "USAGE", fix).exit_code(ExitCode::USAGE)
}

// --- the tree ---------------------------------------------------------------

/// Build the CLI. `main` runs it; the tests audit it.
pub fn build() -> AgentCli {
    AgentCli::new("nashcode", LONG_ABOUT)
        .version(env!("CARGO_PKG_VERSION"))
        .command(setup_command())
        .command(use_command())
        .command(profiles_command())
        .command(token_command())
        .command(init_command())
        .command(new_command())
        .command(ls_command())
        .command(clone_command())
        .command(rm_command())
        .command(gc_command())
        .command(desc_command())
        .command(remote_command())
        .command(invite_command())
        .command(plan_command())
        .command(annotate_command())
        .command(index_command())
        .command(comments_command())
        .command(brain_command())
        .command(context_command())
        .command(people_command())
        .command(ready_command())
        .command(claim_command())
        .command(grep_command())
        .skill()
        .doctor_with(
            Command::new("doctor", DOCTOR_DOC).usage("nashcode doctor [--profile <name>]"),
            doctor::checks(),
        )
}

fn setup_command() -> Command {
    Command::new("setup", SETUP_DOC)
        .usage(
            "nashcode setup [--host <user@host>] [--name <name>] [--provider <id>] \
             [--bucket <bucket>] [--region <region>] [--endpoint <url>] \
             [--access-key-id <id>] [--secret-access-key <secret>] [--creds-on-host] \
             [--site-name <name>] [--site-desc <text>] [--site-owner <name>] \
             [--token <token>] [--tailscale-authkey <key>] [--viewer] \
             [--viewer-port <port>] [--listen-port <port>] [--skip-verify]",
        )
        .handles_dry_run()
        .handler(|req, _ctx| {
            let ctx = context(req);
            let provider_id = text(req, "provider");
            let args = SetupArgs {
                host: text(req, "host"),
                name: text(req, "name"),
                provider: provider_id.as_deref().and_then(Provider::from_id),
                bucket: text(req, "bucket"),
                region: text(req, "region"),
                endpoint: text(req, "endpoint"),
                access_key_id: text(req, "access-key-id"),
                secret_access_key: text(req, "secret-access-key"),
                creds_on_host: on(req, "creds-on-host"),
                site_name: text(req, "site-name"),
                site_desc: text(req, "site-desc"),
                site_owner: text(req, "site-owner"),
                token: text(req, "token"),
                tailscale_authkey: text(req, "tailscale-authkey"),
                viewer: on(req, "viewer"),
                viewer_port: port(req, "viewer-port", 8090),
                listen_port: port(req, "listen-port", 8080),
                skip_verify: on(req, "skip-verify"),
                dry_run: req.dry_run(),
            };
            Box::pin(async move {
                if let Some(id) = &provider_id
                    && args.provider.is_none()
                {
                    let known: Vec<&str> = Provider::all().iter().map(|p| p.id()).collect();
                    return Err(misuse(
                        format!("`{id}` is not a store nashcode offers"),
                        // Runnable as written: an unquoted `|` here would have
                        // been a shell pipe into a program called `r2`.
                        format!(
                            "nashcode setup --provider {}   # one of: {}",
                            known.first().copied().unwrap_or("aws-s3"),
                            known.join(", ")
                        ),
                    ));
                }
                let value = setup::run(&ctx, &args).map_err(|e| {
                    let fix = setup_fix(&format!("{e:#}"));
                    oops(e, fix)
                })?;
                Ok(CommandOutput::new(value)
                    .next_action(NextAction::new(
                        "nashcode init [<name>]",
                        "Version a folder on the deployment you just built",
                    ))
                    .next_action(NextAction::new(
                        "nashcode doctor",
                        "Verify the deployment end to end",
                    )))
            })
        })
}

/// `setup` fails in the middle of a seven-step wizard, so the fix depends on
/// which step gave up. Anything unrecognised gets the re-run, which is safe:
/// every step is idempotent.
fn setup_fix(message: &str) -> String {
    let m = message.to_ascii_lowercase();
    if m.contains("--provider") || m.contains("--bucket") || m.contains("--host") {
        return "nashcode setup --host <user@host> --provider tigris --bucket <name>".to_string();
    }
    if m.contains("credentials") {
        return "nashcode setup --creds-on-host …   # or export AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY first"
            .to_string();
    }
    "nashcode setup --dry-run   # then re-run without it: every step is idempotent".to_string()
}

fn use_command() -> Command {
    Command::new(
        "use",
        "Make a saved profile the active one.\n\n\
         Every other command then acts on it, unless it is given --profile.",
    )
    .usage("nashcode use <name>")
    .handler(|req, _ctx| {
        let ctx = context(req);
        let name = req.arg(0).unwrap_or_default().to_string();
        Box::pin(async move {
            if name.is_empty() {
                return Err(misuse(
                    "no profile named: `nashcode use` needs the name of a saved profile",
                    "nashcode profiles   # then: nashcode use <name>",
                ));
            }
            let value = profiles::use_profile(&ctx, &name)
                .map_err(|e| oops(e, "nashcode profiles"))?;
            Ok(CommandOutput::new(value))
        })
    })
}

fn profiles_command() -> Command {
    Command::new(
        "profiles",
        "List the saved profiles, and say which one is active.",
    )
    .usage("nashcode profiles")
    .handler(|req, _ctx| {
        let ctx = context(req);
        Box::pin(async move {
            let value = profiles::list(&ctx).map_err(|e| oops(e, "nashcode setup"))?;
            Ok(CommandOutput::new(value))
        })
    })
}

fn token_command() -> Command {
    Command::new("token", TOKEN_DOC)
        .usage("nashcode token [--profile <name>]")
        .handler(|req, _ctx| {
            let ctx = context(req);
            Box::pin(async move {
                let value = profiles::token(&ctx)
                    .map_err(|e| oops(e, "nashcode profiles   # then: nashcode use <name>"))?;
                Ok(CommandOutput::new(value))
            })
        })
}

/// What an agent does next once a repository exists.
fn after_repo() -> Vec<NextAction> {
    vec![
        NextAction::new(
            "nashcode plan new <title>",
            "Start a plan a human can comment on",
        ),
        NextAction::new(
            "nashcode index [<repo>]",
            "Build the viewer's code index for it",
        ),
        NextAction::new("nashcode brain [<repo>]", "Read the repository's state"),
    ]
}

fn init_command() -> Command {
    Command::new("init", INIT_DOC)
        .usage(
            "nashcode init [<name>] [--private] [--desc <text>] [--section <name>] \
             [--git] [--jj] [--no-push] [--profile <name>]",
        )
        .handler(|req, _ctx| {
            let ctx = context(req);
            let args = InitArgs {
                name: req.arg(0).map(str::to_string),
                private: on(req, "private"),
                desc: text(req, "desc"),
                section: text(req, "section"),
                git: on(req, "git"),
                jj: on(req, "jj"),
                no_push: on(req, "no-push"),
            };
            Box::pin(async move {
                if args.git && args.jj {
                    return Err(misuse(
                        "--git and --jj ask for different working copies",
                        "nashcode init --jj   # pick one; --git is the other",
                    ));
                }
                let value = repo::init(&ctx, &args).map_err(|e| oops(e, "nashcode doctor"))?;
                Ok(CommandOutput::new(value).next_actions(after_repo()))
            })
        })
}

fn new_command() -> Command {
    Command::new("new", NEW_DOC)
        .usage(
            "nashcode new <name> [--private] [--desc <text>] [--section <name>] \
             [--owner <name>] [--no-remote] [--jj] [--profile <name>]",
        )
        .handler(|req, _ctx| {
            let ctx = context(req);
            let name = req.arg(0).map(str::to_string);
            let args = NewArgs {
                name: name.clone().unwrap_or_default(),
                private: on(req, "private"),
                desc: text(req, "desc"),
                section: text(req, "section"),
                owner: text(req, "owner"),
                no_remote: on(req, "no-remote"),
                jj: on(req, "jj"),
            };
            Box::pin(async move {
                if name.is_none() {
                    return Err(misuse(
                        "no repository name",
                        "nashcode new <name>",
                    ));
                }
                let value = repo::new(&ctx, &args).map_err(|e| oops(e, "nashcode doctor"))?;
                Ok(CommandOutput::new(value).next_actions(after_repo()))
            })
        })
}

fn ls_command() -> Command {
    Command::new("ls", LS_DOC)
        .usage("nashcode ls [--profile <name>]")
        .handler(|req, _ctx| {
            let ctx = context(req);
            Box::pin(async move {
                let rows = repo::ls(&ctx).map_err(|e| oops(e, "nashcode doctor"))?;
                Ok(CommandOutput::list(rows))
            })
        })
}

fn clone_command() -> Command {
    Command::new(
        "clone",
        "Clone a repository from the server into a new directory.\n\n\
         The push token is handed to git's credential helper first, so a private \
         repository clones without asking for a password.\n\n\
         <dir> defaults to the repository name, and must not already exist.\n\
         --jj makes the clone a colocated jj repo; on by default when \
         $NASHCODE_JJ=1.",
    )
    .usage("nashcode clone <name> [<dir>] [--jj] [--profile <name>]")
    .handler(|req, _ctx| {
        let ctx = context(req);
        let name = req.arg(0).map(str::to_string);
        let args = CloneArgs {
            name: name.clone().unwrap_or_default(),
            dir: req.arg(1).map(str::to_string),
            jj: on(req, "jj"),
        };
        Box::pin(async move {
            if name.is_none() {
                return Err(misuse("no repository name", "nashcode ls"));
            }
            let value = repo::clone(&ctx, &args).map_err(|e| oops(e, "nashcode ls"))?;
            Ok(CommandOutput::new(value))
        })
    })
}

fn rm_command() -> Command {
    Command::new(
        "rm",
        "Delete a repository from the server, and every commit in it.\n\n\
         There is no confirmation prompt — nobody is at a terminal — so this \
         refuses to run without --yes.",
    )
    .usage("nashcode rm <name> [--profile <name>]")
    .handler(|req, _ctx| {
        let ctx = context(req);
        let name = req.arg(0).map(str::to_string);
        let confirmed = req.assume_yes();
        Box::pin(async move {
            let Some(name) = name else {
                return Err(misuse("no repository name", "nashcode rm <name> --yes"));
            };
            if !confirmed {
                return Err(misuse(
                    format!("refusing to delete `{name}` without --yes"),
                    format!("nashcode rm {name} --yes"),
                ));
            }
            let args = RmArgs { name };
            let value = repo::rm(&ctx, &args).map_err(|e| oops(e, "nashcode ls"))?;
            Ok(CommandOutput::new(value))
        })
    })
}

fn gc_command() -> Command {
    Command::new("gc", GC_DOC)
        .usage("nashcode gc <name> [--profile <name>]")
        .handler(|req, _ctx| {
            let ctx = context(req);
            let name = req.arg(0).map(str::to_string);
            Box::pin(async move {
                let Some(name) = name else {
                    return Err(misuse("no repository name", "nashcode gc <name>"));
                };
                let value =
                    repo::gc(&ctx, &GcArgs { name }).map_err(|e| oops(e, "nashcode ls"))?;
                Ok(CommandOutput::new(value))
            })
        })
}

fn desc_command() -> Command {
    Command::new(
        "desc",
        "Set a repository's description, owner, section, or private flag.\n\n\
         One PUT /<name>/config carrying only the fields you named; anything you \
         leave out is left alone, so this is safe to run for one field.\n\n\
         --private hides it from the index and gates reads behind the token; \
         --public shows it on the index and allows anonymous reads. Naming \
         neither leaves the flag as it is; naming both is refused.",
    )
    .usage(
        "nashcode desc <name> [--desc <text>] [--section <name>] [--owner <name>] \
         [--private] [--public] [--profile <name>]",
    )
    .handler(|req, _ctx| {
        let ctx = context(req);
        let name = req.arg(0).map(str::to_string);
        let args = DescArgs {
            name: name.clone().unwrap_or_default(),
            desc: text(req, "desc"),
            section: text(req, "section"),
            owner: text(req, "owner"),
            private: on(req, "private"),
            public: on(req, "public"),
        };
        Box::pin(async move {
            if name.is_none() {
                return Err(misuse("no repository name", "nashcode ls"));
            }
            if args.private && args.public {
                return Err(misuse(
                    "--private and --public ask for opposite things",
                    "nashcode desc <name> --private   # pick one; --public is the other",
                ));
            }
            let value = repo::desc(&ctx, &args).map_err(|e| oops(e, "nashcode ls"))?;
            Ok(CommandOutput::new(value))
        })
    })
}

fn remote_command() -> Command {
    Command::new("remote", REMOTE_DOC)
        .usage("nashcode remote [<name>] [--profile <name>]")
        .handler(|req, _ctx| {
            let ctx = context(req);
            let args = RemoteArgs {
                name: req.arg(0).map(str::to_string),
            };
            Box::pin(async move {
                let value = repo::remote(&ctx, &args).map_err(|e| oops(e, "nashcode doctor"))?;
                Ok(CommandOutput::new(value))
            })
        })
}

fn invite_command() -> Command {
    Command::new("invite", INVITE_DOC)
        .usage("nashcode invite [<name>] [--list] [--revoke <name>] [--profile <name>]")
        .handler(|req, _ctx| {
            let ctx = context(req);
            let args = InviteArgs {
                name: req.arg(0).map(str::to_string),
                list: on(req, "list"),
                revoke: text(req, "revoke"),
            };
            Box::pin(async move {
                let asked = usize::from(args.name.is_some())
                    + usize::from(args.list)
                    + usize::from(args.revoke.is_some());
                if asked > 1 {
                    return Err(misuse(
                        "say one thing at a time: a name to invite, --list, or --revoke <name>",
                        "nashcode invite --list",
                    ));
                }
                if asked == 0 {
                    return Err(misuse(
                        "say what to do: `nashcode invite <name>`, `--list`, or `--revoke <name>`",
                        "nashcode invite --list",
                    ));
                }
                let value = invite::run(&ctx, &args).map_err(|e| oops(e, "nashcode doctor"))?;
                Ok(CommandOutput::new(value))
            })
        })
}

fn plan_command() -> Command {
    Command::new("plan", PLAN_DOC).subcommand(
        Command::new(
            "new",
            "Create plans/<slug>.md from a template.\n\n\
             The filename is a slug of the title. It refuses to clobber a plan \
             that already exists.",
        )
        .usage("nashcode plan new <title...>")
        .handler(|req, _ctx| {
            let ctx = context(req);
            let args = PlanNewArgs {
                title: req.positionals().to_vec(),
            };
            Box::pin(async move {
                let value = plan::new(&ctx, &args)
                    .map_err(|e| oops(e, "nashcode plan new \"replace the parser\""))?;
                let file = value["relative"].as_str().unwrap_or_default().to_string();
                Ok(CommandOutput::new(value)
                    .next_action(NextAction::new(
                        format!("nashcode annotate {file}"),
                        "Hand the plan to a human to review",
                    ))
                    .next_action(NextAction::new(
                        format!("nashcode comments {file}"),
                        "Read what they said",
                    )))
            })
        }),
    )
}

fn annotate_command() -> Command {
    Command::new("annotate", ANNOTATE_DOC)
        .usage("nashcode annotate <file> [--no-launch] [--profile <name>]")
        .handler(|req, _ctx| {
            let ctx = context(req);
            let file = req.arg(0).map(str::to_string);
            let args = AnnotateArgs {
                file: file.clone().unwrap_or_default(),
                no_launch: on(req, "no-launch"),
            };
            Box::pin(async move {
                let Some(file) = file else {
                    return Err(misuse("no plan file", "nashcode annotate plans/<file>.md"));
                };
                let value = plan::annotate(&ctx, &args)
                    .map_err(|e| oops(e, format!("nashcode comments {file}")))?;
                let since = crate::timefmt::now_rfc3339();
                Ok(CommandOutput::new(value).next_action(NextAction::new(
                    format!("nashcode comments {file} --since={since}"),
                    "Read anything left on the plan from here on",
                )))
            })
        })
}

fn index_command() -> Command {
    Command::new("index", INDEX_DOC)
        .usage("nashcode index [<repo>] [--status] [--profile <name>]")
        .handler(|req, _ctx| {
            let ctx = context(req);
            let args = IndexArgs {
                repo: req.arg(0).map(str::to_string),
                status: on(req, "status"),
            };
            let named = args.repo.clone().unwrap_or_default();
            Box::pin(async move {
                let value = code::run(&ctx, &args)
                    .map_err(|e| oops(e, "nashcode setup --viewer   # the index lives in the viewer"))?;
                let suffix = if named.is_empty() {
                    String::new()
                } else {
                    format!(" {named}")
                };
                Ok(CommandOutput::new(value)
                    .next_action(NextAction::new(
                        format!("nashcode index{suffix} --status"),
                        "Check whether the queued run finished",
                    ))
                    .next_action(NextAction::new(
                        format!("nashcode brain{suffix}"),
                        "Read the repository's state, index included",
                    )))
            })
        })
}

fn comments_command() -> Command {
    Command::new("comments", COMMENTS_DOC)
        .usage(
            "nashcode comments <file> [--branch <branch>] [--since <rfc3339>] \
             [--repo <name>] [--profile <name>]",
        )
        .handler(|req, _ctx| {
            let ctx = context(req);
            let file = req.arg(0).map(str::to_string);
            let args = CommentsArgs {
                file: file.clone().unwrap_or_default(),
                branch: text(req, "branch"),
                since: text(req, "since"),
                repo: text(req, "repo"),
            };
            let echo = (args.branch.clone(), args.repo.clone());
            Box::pin(async move {
                let Some(file) = file else {
                    return Err(misuse("no plan file", "nashcode comments plans/<file>.md"));
                };
                let rows = plan::comments(&ctx, &args).map_err(|e| {
                    oops(e, "nashcode setup --viewer   # comments live in the viewer")
                })?;
                let newest = plan::newest_timestamp(&rows);
                let mut poll = format!("nashcode comments {file}");
                if let Some(repo) = &echo.1 {
                    poll.push_str(&format!(" --repo={repo}"));
                }
                if let Some(branch) = &echo.0 {
                    poll.push_str(&format!(" --branch={branch}"));
                }
                if let Some(newest) = &newest {
                    poll.push_str(&format!(" --since={newest}"));
                }
                Ok(CommandOutput::list(rows).next_action(NextAction::new(
                    poll,
                    match newest {
                        Some(_) => "Poll again: --since is exclusive, so this returns only \
                                    comments written after the newest one above",
                        None => "Poll again for the first comment",
                    },
                )))
            })
        })
}

fn brain_command() -> Command {
    Command::new("brain", BRAIN_DOC)
        .usage("nashcode brain [<repo>] [--profile <name>]")
        .handler(|req, _ctx| {
            let ctx = context(req);
            let args = BrainArgs {
                repo: req.arg(0).map(str::to_string),
            };
            Box::pin(async move {
                // Never an Err. A session-start hook runs this, and the cost of
                // a wrong answer is one unhelpful field; the cost of a nonzero
                // exit is the session.
                Ok(CommandOutput::new(brain::run(&ctx, &args)))
            })
        })
}

fn context_command() -> Command {
    Command::new("context", CONTEXT_DOC)
        .subcommand(
            Command::new(
                "put",
                "File one item into the repository it is about.\n\n\
                 The text comes from the named file, or from standard input when no \
                 file is named. `--at` defaults to now. A `meeting` is the browser \
                 extension's transcript JSON and carries its own title and times, so \
                 the flags are ignored for it.\n\n\
                 `--source` is the provider's stable id — a Gmail message id, a chat \
                 thread plus day, a URL. It makes the put idempotent: the same source \
                 always names the same file, and a second put of it commits nothing \
                 and answers `existing: true`.",
            )
            .usage(
                "nashcode context put <meeting|email|chat|note> [<file>] --title <title> \
                 [--at <rfc3339>] [--source <id>] [--repo <name>] [--profile <name>]",
            )
            .handler(|req, _ctx| {
                let ctx = context(req);
                let args = ContextPutArgs {
                    kind: req.arg(0).unwrap_or_default().to_string(),
                    file: req.arg(1).map(str::to_string),
                    title: text(req, "title"),
                    at: text(req, "at"),
                    source: text(req, "source"),
                    repo: text(req, "repo"),
                };
                Box::pin(async move {
                    if args.kind.is_empty() {
                        return Err(misuse(
                            "no kind named",
                            "nashcode context put email --title \"Re: invoice\" < msg.txt",
                        ));
                    }
                    let kind = args.kind.clone();
                    let value = context::put(&ctx, &args)
                        .map_err(|e| oops(e, "nashcode setup --viewer   # context lives in the viewer"))?;
                    let id = value["id"].as_str().unwrap_or_default().to_string();
                    Ok(CommandOutput::new(value).next_action(NextAction::new(
                        format!("nashcode context get {kind} {id}"),
                        "Read the filed item back",
                    )))
                })
            }),
        )
        .subcommand(
            Command::new(
                "ls",
                "List what is filed, oldest ingest first.\n\n\
                 `--since` takes the `next_since` of a previous answer and is strictly \
                 exclusive, so handing it back is the whole polling loop: nothing \
                 repeats, and a backfilled item — one whose `at` is older than \
                 everything around it — still arrives.",
            )
            .usage(
                "nashcode context ls [--kind <kind>] [--since <cursor>] [--repo <name>] \
                 [--profile <name>]",
            )
            .handler(|req, _ctx| {
                let ctx = context(req);
                let args = ContextLsArgs {
                    kind: text(req, "kind"),
                    since: text(req, "since"),
                    repo: text(req, "repo"),
                };
                let echo = args.kind.clone();
                Box::pin(async move {
                    let value = context::ls(&ctx, &args)
                        .map_err(|e| oops(e, "nashcode setup --viewer   # context lives in the viewer"))?;
                    let mut poll = "nashcode context ls".to_string();
                    if let Some(kind) = &echo {
                        poll.push_str(&format!(" --kind={kind}"));
                    }
                    if let Some(next) = value["next_since"].as_str().filter(|s| !s.is_empty()) {
                        poll.push_str(&format!(" --since={next}"));
                    }
                    Ok(CommandOutput::new(value).next_action(NextAction::new(
                        poll,
                        "Poll again: --since is exclusive, so this returns only what was \
                         filed after the items above",
                    )))
                })
            }),
        )
        .subcommand(
            Command::new("get", "Read one filed item: its front matter and its body.")
                .usage(
                    "nashcode context get <meeting|email|chat|note> <id> [--repo <name>] \
                     [--profile <name>]",
                )
                .handler(|req, _ctx| {
                    let ctx = context(req);
                    let args = ContextGetArgs {
                        kind: req.arg(0).unwrap_or_default().to_string(),
                        id: req.arg(1).unwrap_or_default().to_string(),
                        repo: text(req, "repo"),
                    };
                    Box::pin(async move {
                        if args.kind.is_empty() || args.id.is_empty() {
                            return Err(misuse(
                                "name a kind and an id",
                                "nashcode context get email 2026-06-13-0905-re-invoice-18f2a0b1",
                            ));
                        }
                        let value = context::get(&ctx, &args)
                            .map_err(|e| oops(e, "nashcode context ls"))?;
                        Ok(CommandOutput::new(value))
                    })
                }),
        )
}

fn people_command() -> Command {
    Command::new("people", PEOPLE_DOC)
        .subcommand(
            Command::new(
                "ls",
                "Every project, the people in it, and the people in no project.\n\n\
                 A project shows its nashcode repo and its folder; a person shows how \
                 many phones and emails they have, because a person with neither can \
                 never be matched. `pushed_at` is when the viewer last got a copy.",
            )
            .usage("nashcode people ls [--file <path>] [--profile <name>]")
            .handler(|req, _ctx| {
                let ctx = context(req);
                let args = PeopleLsArgs { file: text(req, "file") };
                Box::pin(async move {
                    let value = people::ls(&ctx, &args)
                        .map_err(|e| oops(e, "nashcode people import   # build the file once"))?;
                    Ok(CommandOutput::new(value).next_action(NextAction::new(
                        "nashcode people check",
                        "Check the file before pushing it",
                    )))
                })
            }),
        )
        .subcommand(
            Command::new(
                "route",
                "Which project these contacts are about, best first.\n\n\
                 Both flags repeat: pass every attendee. A project scores one point \
                 per distinct person matched, so the winner is the project the most \
                 of these people belong to. `tie: true` means the top two score the \
                 same and nothing here decides — ask a person. Your own addresses \
                 never score.",
            )
            .usage(
                "nashcode people route [--email <address>]... [--phone <e164>]... \
                 [--file <path>]",
            )
            .handler(|req, _ctx| {
                let ctx = context(req);
                let args = PeopleRouteArgs {
                    emails: repeated(req, "email"),
                    phones: repeated(req, "phone"),
                    file: text(req, "file"),
                };
                Box::pin(async move {
                    if args.emails.is_empty() && args.phones.is_empty() {
                        return Err(misuse(
                            "ask about somebody",
                            "nashcode people route --email rob@example.com",
                        ));
                    }
                    let value = people::route(&ctx, &args)
                        .map_err(|e| oops(e, "nashcode people ls"))?;
                    Ok(CommandOutput::new(value))
                })
            }),
        )
        .subcommand(
            Command::new(
                "push",
                "Give the viewer a copy of the file.\n\n\
                 The viewer answers GET /people/route with it, which is how the \
                 meeting extension fills the repo box. It has no route that hands the \
                 copy back: phones and emails stay on this machine. A file the viewer \
                 refuses comes back with the reason and nothing is stored.",
            )
            .usage("nashcode people push [--file <path>] [--profile <name>]")
            .handler(|req, _ctx| {
                let ctx = context(req);
                let args = PeoplePushArgs { file: text(req, "file") };
                Box::pin(async move {
                    let value = people::push(&ctx, &args).map_err(|e| {
                        oops(e, "nashcode setup --viewer   # people live in the viewer")
                    })?;
                    Ok(CommandOutput::new(value))
                })
            }),
        )
        .subcommand(
            Command::new(
                "check",
                "Everything wrong with the file, and a non-zero exit when there is \
                 anything.\n\n\
                 Refused: a duplicate id, a project naming an id no person has, an id \
                 that is blank. Those break the join key, so the file does not load at \
                 all. Warned: a project with nobody in it, a phone that is not E.164, \
                 a person with neither a phone nor an email. Those load and never do \
                 their job.",
            )
            .usage("nashcode people check [--file <path>]")
            .handler(|req, _ctx| {
                let ctx = context(req);
                let args = PeopleCheckArgs { file: text(req, "file") };
                Box::pin(async move {
                    let value = people::check(&ctx, &args)
                        .map_err(|e| oops(e, "nashcode people ls"))?;
                    Ok(CommandOutput::new(value).next_action(NextAction::new(
                        "nashcode people push",
                        "Give the viewer the file it will answer from",
                    )))
                })
            }),
        )
        .subcommand(
            Command::new(
                "import",
                "Build the file once from the old per-inbox lists. One-shot.\n\n\
                 Reads ~/.imsg-router/routes.json and, when it is there, \
                 ~/.nashcode/context.toml, and returns the people.json they add up \
                 to. Nothing is written and nothing is deleted: read the result, fix \
                 the names, then save it yourself:\n\n\
                 \x20 nashcode people import | jq .result.file > ~/.nashcode/people.json\n\n\
                 routes.json knows numbers, not names, so every person arrives as \
                 <project>-<n> with an empty name and stderr lists them. This \
                 subcommand is deleted once it has run.",
            )
            .usage("nashcode people import [--routes <path>] [--context <path>]")
            .handler(|req, _ctx| {
                let ctx = context(req);
                let args = PeopleImportArgs {
                    routes: text(req, "routes"),
                    context: text(req, "context"),
                };
                Box::pin(async move {
                    let value = people::import(&ctx, &args).map_err(|e| {
                        oops(e, "nashcode people import --routes ~/.imsg-router/routes.json")
                    })?;
                    Ok(CommandOutput::new(value).next_action(NextAction::new(
                        "nashcode people check",
                        "Save the file, fill in the empty names, then check it",
                    )))
                })
            }),
        )
}

fn ready_command() -> Command {
    Command::new(
        "ready",
        "List the cards you may start now.\n\n\
         A card under `tasks/` is ready when it is `todo` and every card that names it \
         in `blocks:` is `done`. The viewer derives that from the tree it mirrors, so \
         this reads `/brain` rather than your working copy: a blocker somebody else \
         finished and pushed counts the moment it lands.\n\n\
         With no argument it asks about the repository `origin` points at, or about \
         every repository when you are not in one.",
    )
    .usage("nashcode ready [<repo>] [--profile <name>]")
    .handler(|req, _ctx| {
        let ctx = context(req);
        let args = ReadyArgs {
            repo: req.arg(0).map(str::to_string),
        };
        Box::pin(async move {
            let rows = card::ready(&ctx, &args)
                .map_err(|e| oops(e, "nashcode setup --viewer   # ready lives in the viewer"))?;
            let first = rows
                .first()
                .and_then(|row| row["path"].as_str())
                .unwrap_or("tasks/<card>.md")
                .to_string();
            Ok(CommandOutput::list(rows).next_action(NextAction::new(
                format!("nashcode claim {first}"),
                "Take the card: assignee and `doing` in one commit",
            )))
        })
    })
}

fn claim_command() -> Command {
    Command::new(
        "claim",
        "Take a card: `assignee: <you>` and `status: doing`, committed and pushed.\n\n\
         One write, one commit of that one file, one push, so two agents reading the \
         same ready list race on the push instead of on the file. The assignee is \
         whatever this working copy commits as (`user.name`, then $USER). Nothing else \
         in the card is touched.",
    )
    .usage("nashcode claim <tasks/x.md> [--profile <name>]")
    .handler(|req, _ctx| {
        let ctx = context(req);
        let file = req.arg(0).map(str::to_string);
        let args = ClaimArgs {
            file: file.clone().unwrap_or_default(),
        };
        Box::pin(async move {
            let Some(file) = file else {
                return Err(misuse("no card named", "nashcode claim tasks/<card>.md"));
            };
            let value = card::claim(&ctx, &args).map_err(|e| oops(e, "nashcode ready"))?;
            Ok(CommandOutput::new(value).next_action(NextAction::new(
                format!("nashcode comments {file}"),
                "Read anything left on the card",
            )))
        })
    })
}

fn grep_command() -> Command {
    Command::new("grep", GREP_DOC)
        .usage("nashcode grep [rg-flags...] <pattern> [path...]")
        .raw_handler(|args, _ctx| {
            let args = args.to_vec();
            Box::pin(async move { grep::run(&args) })
        })
}
