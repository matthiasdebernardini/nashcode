# nashgit

Run your own git host on your own box, behind your own tailnet. One binary
builds the server from nothing and then works it day to day.

You need three things: a Linux box you can SSH to, a Tailscale account, and an
S3-compatible bucket. `nashgit setup` does the rest.

## The stack

- **dgit** — a git server written for Cloudflare Workers. Each repository is a
  Durable Object with its own SQLite database. Stock `git` talks to it over
  HTTPS.
- **celld** — runs that Worker code on your box, no Cloudflare account
  involved. Every repository replicates to your bucket, so the box is
  disposable: rebuild it and the same repositories come back.
- **your bucket** — the store and the coordinator. celld needs conditional
  writes, so only Amazon S3, Cloudflare R2, and Tigris are offered. MinIO
  community, Backblaze B2, Hetzner, and DigitalOcean Spaces silently lack
  them; see <https://celld.dev/docs/fencing>.
- **Tailscale** — the perimeter. celld listens on 127.0.0.1 only and
  `tailscale serve` fronts it with HTTPS on your tailnet. The server has no
  public port.

## Install

```sh
cargo install --path .
```

## Set up a server

```sh
nashgit setup
```

The wizard asks for the SSH destination and the bucket, installs tailscale,
celld, node, and git over SSH, deploys dgit, writes a systemd unit, joins the
tailnet, and proves the result with a real push and clone. Re-running is safe;
every step skips what is already done.

Every prompt has a flag, so it also runs unattended:

```sh
AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... \
nashgit setup --yes --host me@your-box --provider tigris --bucket your-bucket
```

`--dry-run` prints every remote script and touches nothing.

The result is saved as a **profile** in `~/.config/nashgit/config.toml`
(mode 0600 — it holds the push token). Keep several servers and switch:

```sh
nashgit profiles
nashgit use work
nashgit ls --profile personal
```

## Day to day

```sh
nashgit init             # version the current folder: create, wire, push
nashgit new api --desc "the api server"
nashgit ls
nashgit clone api
nashgit remote           # point this repo's origin at the server
nashgit rm scratch
nashgit gc api
nashgit desc api --section services --private
nashgit token            # print the push token, for CI
nashgit doctor           # one line per check, ✓/✗/-
```

The push token never goes into a remote URL. `nashgit` hands it to git's
credential helper (`git credential approve`), so `git remote -v`, your config
files, and your shell history stay clean.

## Using with jj

nashgit speaks [Jujutsu](https://jj-vcs.github.io/jj/) natively. It detects
the working copy from the directory layout — plain git (`.git`), colocated jj
(`.jj` and `.git`), or jj-only (`.jj`) — and uses the right tool: `jj git
remote add` in a jj repository, `git remote add` otherwise.

- `nashgit init` prefers jj: with jj on PATH it creates the working copy with
  `jj git init --colocate`, so git tooling keeps working alongside. `--git`
  forces plain git; `--jj` forces jj.
- `nashgit new` and `nashgit clone` take `--jj` to colocate jj on top of the
  git working copy. Set `NASHGIT_JJ=1` to make that the default.
- Credentials work unchanged: jj asks git's credential helpers, so the token
  stored by `nashgit` authenticates `jj git push` too.

No server-side change is needed — jj pushes to git remotes natively.

## Plans and comments

A plan is a markdown file under `plans/` in a repository. The viewer (an
optional companion service, `nashgit setup --viewer`) renders them, and people
comment on the rendered page.

```sh
nashgit plan new "replace the parser"   # plans/replace-the-parser.md
nashgit annotate plans/replace-the-parser.md   # open locally in plannotator
nashgit comments plans/replace-the-parser.md   # read the replies
nashgit comments plans/replace-the-parser.md --branch main --since 2026-08-01T00:00:00Z
```

This is a loop an agent can drive: write a plan, wait for a human to comment
in the viewer, read the comments back with `--json`.

## JSON output

Every command takes `--json` and then prints exactly one JSON value on
stdout. Progress goes to stderr. On failure, stdout still carries one value:
`{"error": "..."}` — a pipeline never parses air. `nashgit comments --json`
passes the viewer's answer through untouched.

## Notes on secrets

- The profile file is written 0600, its directory 0700.
- Remote scripts travel to the host on stdin (`ssh dest bash -s`), so tokens
  and bucket credentials never appear in either machine's process list.
- Nothing prints a secret except `nashgit token`, which exists to do exactly
  that.
- SSH is the system `ssh`: your config, agent, jump hosts, and hardware keys
  keep working, and nashgit never holds a key.

## Development

```sh
cargo nextest run
```

No test touches a network or a real host. The seams are environment
variables: `NASHGIT_SSH_BIN`, `NASHGIT_GIT_BIN`, `NASHGIT_JJ_BIN` point the
CLI at recording shims, `NASHGIT_CONFIG` relocates the profile store, and the
`comments` tests serve canned JSON from a listener on 127.0.0.1.
