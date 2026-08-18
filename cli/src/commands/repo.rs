//! The day-to-day repository commands: init, new, ls, clone, rm, gc, desc, remote.

use super::Ctx;
use crate::api::{Client, RepoConfig, valid_repo_name};
use crate::cli::{CloneArgs, DescArgs, GcArgs, InitArgs, NewArgs, RemoteArgs, RmArgs};
use crate::profile::Profile;
use crate::vcs::{self, Workspace};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::path::Path;

/// Wire `origin` and hand the push token to git's credential store.
///
/// The token never goes into the remote URL: `git remote -v`, the config file,
/// and anything the user pastes into a bug report stay clean. jj takes the same
/// path, because jj asks git's credential helpers for HTTPS passwords.
fn wire_remote(ctx: &Ctx, p: &Profile, ws: &Workspace, name: &str) -> Result<String> {
    let url = p.repo_url(name);
    let cmd = ws.set_origin(&url)?;
    ctx.out.step(&cmd);

    if p.token.is_empty() {
        ctx.out
            .warn("this profile holds no token, so pushes will ask for a password");
        return Ok(url);
    }
    match vcs::credential_helper()? {
        Some(helper) => {
            vcs::credential_approve(&p.url_host()?, "x", &p.token)?;
            ctx.out.step(format!("token stored via credential.{helper}"));
        }
        None => {
            let suggested = vcs::suggested_helper();
            ctx.out.warn(format!(
                "git has no credential helper, so the push token was not saved.\n         \
                 Set one, then run `nashgit remote` again:\n         \
                 git config --global credential.helper {suggested}"
            ));
        }
    }
    Ok(url)
}

/// jj was asked for. An explicit `--jj` with no jj installed is an error; the
/// `$NASHGIT_JJ=1` ambient default degrades to a warning and plain git.
fn require_jj(explicit_flag: bool, out: &crate::output::Out) -> Result<()> {
    if vcs::jj_available() {
        return Ok(());
    }
    if explicit_flag {
        bail!("--jj was given but jj is not on PATH. Install Jujutsu: https://jj-vcs.github.io/jj/");
    }
    out.warn("NASHGIT_JJ=1 is set but jj is not on PATH; continuing with git only");
    Ok(())
}

fn create(client: &Client, name: &str, cfg: &RepoConfig) -> Result<crate::api::ConfigEcho> {
    if !valid_repo_name(name) {
        bail!(
            "`{name}` is not a valid repository name. dgit allows letters, digits, dot, dash, \
             and underscore, starting with a letter or digit."
        );
    }
    client.put_config(name, cfg)
}

pub fn new(ctx: &Ctx, args: &NewArgs) -> Result<()> {
    let (_, p, client) = ctx.client()?;
    let cfg = RepoConfig {
        description: args.desc.clone(),
        section: args.section.clone(),
        owner: args.owner.clone().or_else(|| p.site_owner.clone()),
        private: args.private.then_some(true),
    };
    let echo = create(&client, &args.name, &cfg)?;

    let mut remote = None;
    let mut colocated = false;
    if !args.no_remote {
        if let Some(ws) = vcs::detect_cwd()? {
            remote = Some(wire_remote(ctx, &p, &ws, &args.name)?);
            if vcs::jj_requested(args.jj) && !ws.kind.is_jj() {
                require_jj(args.jj, &ctx.out)?;
                if vcs::jj_available() {
                    vcs::jj_init_colocate(&ws.root)?;
                    colocated = true;
                    ctx.out.step("jj git init --colocate");
                }
            }
        }
    }

    let url = p.repo_url(&args.name);
    ctx.out.emit(
        json!({
            "name": args.name,
            "url": url,
            "web": format!("{}/{}/", p.url.trim_end_matches('/'), args.name),
            "description": echo.description,
            "section": echo.section,
            "private": echo.private,
            "remote": remote,
            "jj_colocated": colocated,
        }),
        || {
            ctx.out.line(format!("created {}", args.name));
            ctx.out
                .line(format!("  {}/{}/", p.url.trim_end_matches('/'), args.name));
            if remote.is_some() {
                ctx.out.line(format!("  origin -> {url}"));
            }
        },
    );
    Ok(())
}

/// `nashgit init` — turn the current directory into a versioned repository.
pub fn init(ctx: &Ctx, args: &InitArgs) -> Result<()> {
    let (_, p, client) = ctx.client()?;
    let cwd = std::env::current_dir().context("read current directory")?;
    let name = match &args.name {
        Some(n) => n.clone(),
        None => cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow::anyhow!("cannot name the repository after `{}`; pass a name", cwd.display()))?,
    };

    let cfg = RepoConfig {
        description: args.desc.clone(),
        section: args.section.clone(),
        owner: p.site_owner.clone(),
        private: args.private.then_some(true),
    };
    let echo = create(&client, &name, &cfg)?;
    ctx.out.step(format!("repository `{name}` exists on {}", p.url));

    // Working copy: reuse whatever is here, otherwise create one.
    let ws = match vcs::detect(&cwd) {
        Some(ws) => {
            ctx.out
                .step(format!("reusing the {} working copy at {}", ws.kind.as_str(), ws.root.display()));
            ws
        }
        None => {
            if vcs::prefer_jj(args.jj, args.git) {
                vcs::jj_init_colocate(&cwd)?;
                ctx.out.step("jj git init --colocate");
            } else {
                vcs::git_init(&cwd, "main")?;
                ctx.out.step("git init -b main");
            }
            vcs::detect(&cwd)
                .ok_or_else(|| anyhow::anyhow!("created a working copy but cannot find it again"))?
        }
    };

    let url = wire_remote(ctx, &p, &ws, &name)?;

    let mut pushed = false;
    if !args.no_push {
        pushed = commit_and_push(ctx, &ws)?;
    }

    ctx.out.emit(
        json!({
            "name": name,
            "url": url,
            "web": format!("{}/{}/", p.url.trim_end_matches('/'), name),
            "vcs": ws.kind.as_str(),
            "root": ws.root,
            "private": echo.private,
            "pushed": pushed,
        }),
        || {
            ctx.out.line(format!("{} is now versioned as `{name}`", ws.root.display()));
            ctx.out
                .line(format!("  {}/{}/", p.url.trim_end_matches('/'), name));
            if !pushed {
                ctx.out.line("  nothing pushed yet");
            }
        },
    );
    Ok(())
}

/// Commit whatever is uncommitted and push it. Returns false when there was
/// nothing to send.
fn commit_and_push(ctx: &Ctx, ws: &Workspace) -> Result<bool> {
    const MSG: &str = "Initial commit (nashgit init)";
    if ws.kind.is_jj() {
        // jj tracks the working copy already; `jj commit` closes the current
        // change, and a bookmark is what `jj git push` can actually send.
        let c = vcs::jj(&ws.root, &["commit", "-m", MSG])?;
        if !c.ok() && !c.stderr.contains("No changes") {
            ctx.out.step(format!("jj commit: {}", c.stderr.trim()));
        }
        let mut set = vcs::jj(&ws.root, &["bookmark", "set", "main", "-r", "@-"])?;
        if !set.ok() {
            // jj before 0.21 called them branches.
            set = vcs::jj(&ws.root, &["branch", "set", "main", "-r", "@-"])?;
        }
        if !set.ok() {
            bail!("could not point the `main` bookmark at the new commit: {}", set.stderr.trim());
        }
        let push = vcs::jj(
            &ws.root,
            &["git", "push", "--allow-new", "--remote", "origin", "--bookmark", "main"],
        )?;
        if !push.ok() {
            bail!("jj git push failed: {}", push.stderr.trim());
        }
        ctx.out.step("jj git push --allow-new --bookmark main");
        return Ok(true);
    }

    let dirty = vcs::git(&ws.root, &["status", "--porcelain"])?;
    if !dirty.stdout.trim().is_empty() {
        let add = vcs::git(&ws.root, &["add", "-A"])?;
        if !add.ok() {
            bail!("git add failed: {}", add.stderr.trim());
        }
        let commit = vcs::git(&ws.root, &["commit", "--quiet", "-m", MSG])?;
        if !commit.ok() {
            bail!("git commit failed: {}", commit.stderr.trim());
        }
        ctx.out.step("git commit -m \"Initial commit (nashgit init)\"");
    }
    let head = vcs::git(&ws.root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = head.stdout.trim();
    let branch = if !head.ok() || branch.is_empty() || branch == "HEAD" {
        "main"
    } else {
        branch
    };
    if vcs::git(&ws.root, &["rev-parse", "--verify", "--quiet", "HEAD"])?
        .stdout
        .trim()
        .is_empty()
    {
        ctx.out.step("no commits yet, nothing to push");
        return Ok(false);
    }
    let push = vcs::git(&ws.root, &["push", "--quiet", "-u", "origin", branch])?;
    if !push.ok() {
        bail!("git push failed: {}", push.stderr.trim());
    }
    ctx.out.step(format!("git push -u origin {branch}"));
    Ok(true)
}

pub fn ls(ctx: &Ctx) -> Result<()> {
    let (_, p, client) = ctx.client()?;
    let repos = client.list()?;
    ctx.out.emit(
        json!({ "url": p.url, "count": repos.len(), "repos": repos }),
        || {
            if repos.is_empty() {
                ctx.out.line("no repositories");
                return;
            }
            let width = repos.iter().map(|r| r.name.len()).max().unwrap_or(4);
            for r in &repos {
                let desc = if r.description.is_empty() {
                    "-"
                } else {
                    &r.description
                };
                ctx.out.line(format!(
                    "{:width$}  {:<10}  {}",
                    r.name,
                    r.idle,
                    desc,
                    width = width
                ));
            }
        },
    );
    Ok(())
}

pub fn clone(ctx: &Ctx, args: &CloneArgs) -> Result<()> {
    let (_, p, _) = ctx.client()?;
    let url = p.repo_url(&args.name);
    let dir = args.dir.clone().unwrap_or_else(|| args.name.clone());

    if Path::new(&dir).exists() {
        bail!("`{dir}` already exists");
    }
    // Store the token first, so a private repository clones without a prompt.
    if !p.token.is_empty() && vcs::credential_helper()?.is_some() {
        vcs::credential_approve(&p.url_host()?, "x", &p.token)?;
    }
    let cwd = std::env::current_dir().context("read current directory")?;
    let r = vcs::git(&cwd, &["clone", &url, &dir])?;
    if !r.ok() {
        bail!("git clone failed: {}", r.stderr.trim());
    }

    let mut colocated = false;
    if vcs::jj_requested(args.jj) {
        require_jj(args.jj, &ctx.out)?;
        if vcs::jj_available() {
            vcs::jj_init_colocate(&cwd.join(&dir))?;
            colocated = true;
            ctx.out.step("jj git init --colocate");
        }
    }

    ctx.out.emit(
        json!({ "name": args.name, "url": url, "dir": dir, "jj_colocated": colocated }),
        || ctx.out.line(format!("cloned {} into {dir}", args.name)),
    );
    Ok(())
}

pub fn rm(ctx: &Ctx, args: &RmArgs) -> Result<()> {
    let (_, p, client) = ctx.client()?;
    if !args.yes {
        if !ctx.out.interactive() {
            bail!("refusing to delete `{}` without --yes", args.name);
        }
        let prompt = format!(
            "Delete `{}` from {}? Every commit in it goes with it.",
            args.name, p.url
        );
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .context("read confirmation")?;
        if !confirmed {
            ctx.out.emit(json!({ "name": args.name, "deleted": false }), || {
                ctx.out.line("cancelled")
            });
            return Ok(());
        }
    }
    client.delete(&args.name)?;
    ctx.out
        .emit(json!({ "name": args.name, "deleted": true }), || {
            ctx.out.line(format!("deleted {}", args.name))
        });
    Ok(())
}

pub fn gc(ctx: &Ctx, args: &GcArgs) -> Result<()> {
    let (_, _, client) = ctx.client()?;
    let r = client.gc(&args.name)?;
    ctx.out.emit(
        json!({ "name": args.name, "removed": r.removed, "kept": r.kept, "skipped": r.skipped }),
        || {
            if r.skipped {
                ctx.out.line(format!(
                    "{}: skipped, too large to sweep ({} objects kept)",
                    args.name, r.kept
                ));
            } else {
                ctx.out.line(format!(
                    "{}: removed {}, kept {}",
                    args.name, r.removed, r.kept
                ));
            }
        },
    );
    Ok(())
}

pub fn desc(ctx: &Ctx, args: &DescArgs) -> Result<()> {
    let (_, _, client) = ctx.client()?;
    let cfg = RepoConfig {
        description: args.desc.clone(),
        section: args.section.clone(),
        owner: args.owner.clone(),
        private: if args.private {
            Some(true)
        } else if args.public {
            Some(false)
        } else {
            None
        },
    };
    if cfg.is_empty() {
        bail!("nothing to change. Pass --desc, --section, --owner, --private, or --public.");
    }
    let echo = client.put_config(&args.name, &cfg)?;
    ctx.out.emit(
        json!({
            "name": args.name,
            "description": echo.description,
            "owner": echo.owner,
            "section": echo.section,
            "private": echo.private,
        }),
        || {
            ctx.out.line(format!(
                "{}: {}{}{}",
                args.name,
                if echo.description.is_empty() {
                    "[no description]".to_string()
                } else {
                    echo.description.clone()
                },
                if echo.section.is_empty() {
                    String::new()
                } else {
                    format!("  section={}", echo.section)
                },
                if echo.private { "  private" } else { "" },
            ))
        },
    );
    Ok(())
}

pub fn remote(ctx: &Ctx, args: &RemoteArgs) -> Result<()> {
    let (_, p) = ctx.profile()?;
    let ws = vcs::require_cwd()?;
    let name = match &args.name {
        Some(n) => n.clone(),
        None => ws
            .default_repo_name()
            .ok_or_else(|| anyhow::anyhow!("cannot name the repository after the directory; pass a name"))?,
    };
    if !valid_repo_name(&name) {
        bail!("`{name}` is not a valid dgit repository name");
    }
    let url = wire_remote(ctx, &p, &ws, &name)?;
    ctx.out.emit(
        json!({ "name": name, "url": url, "vcs": ws.kind.as_str(), "root": ws.root }),
        || ctx.out.line(format!("origin -> {url}")),
    );
    Ok(())
}
