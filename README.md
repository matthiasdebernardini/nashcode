# nashcode

Your own git host on your own box, behind your own tailnet — plus the review UI that
makes it a place to work, not just a place to push.

This is a **companion to GitHub, not a replacement**. GitHub keeps the public repos, the
issues, the world. nashcode is where you and your coding agents do private, fast,
stacked-branch work on infrastructure you own end to end.

## Three pieces

1. **The server** — [dgit](https://github.com/littledivy/dgit), a git server built for
   Cloudflare Workers, run on your own Linux box via celld. Every repo replicates to
   your S3-compatible bucket, so the box is disposable. Tailscale is the perimeter:
   loopback bind, `tailscale serve` in front, no public port. There is no server code in
   this repo — the CLI deploys and operates stock dgit.

2. **[`cli/`](cli/README.md)** — the `nashcode` binary. `nashcode setup` builds the whole
   server from an SSH destination and a bucket name; then it is the daily driver:
   `init`, `new`, `clone`, `ls`, `invite`, `doctor`. Works with git and jj.

3. **[`viewer/`](viewer/README.md)** — the `nashcode-viewer` binary, a web app for what
   dgit lacks: Pierre-quality diffs, stacked-branch review with merge/restack buttons, a
   built-in CI runner, comments with a public JSON API, plans and a kanban board that
   live *in* the repo, agent traces linking every commit to the conversation that wrote
   it, and `/brain` — the whole tailnet's work state as one JSON document.

## Build

```sh
cargo build --release            # both binaries; the viewer needs node+npm at build time
cargo install --path cli         # the `nashcode` CLI
cargo install --path viewer      # the `nashcode-viewer` server
```

## Layout

```
cli/       the nashcode CLI: server setup and day-to-day git operations
viewer/    the nashcode-viewer web app: review, CI, board, traces, brain
uat/       the manual user-acceptance harness
AGENTS.md  how coding agents use a nashcode deployment (plans, comments, cards, traces)
```

Each crate has its own README. `viewer/SPEC.md` is the viewer's contract;
`viewer/NOTES.md` records where its implementation had to choose.

## Development

```sh
cargo nextest run --workspace    # the whole suite; fixture repos use real git
```

## License

Apache-2.0 (see [LICENSE](LICENSE)). Vendored fonts under `viewer/vendor/` and the
fonts pulled from npm keep their own licenses (SIL OFL), shipped alongside the files.
