//! `nashcode use`, `nashcode profiles`, `nashcode token`.

use super::Ctx;
use crate::profile::Store;
use anyhow::{Result, bail};
use serde_json::json;

pub fn use_profile(ctx: &Ctx, name: &str) -> Result<()> {
    let mut store = Store::load()?;
    store.set_active(name)?;
    store.save()?;
    let p = &store.profiles[name];
    ctx.out.emit(
        json!({ "active": name, "url": p.url, "ssh": p.ssh }),
        || ctx.out.line(format!("active profile: {name}  {}", p.url)),
    );
    Ok(())
}

pub fn list(ctx: &Ctx) -> Result<()> {
    let store = Store::load()?;
    let rows: Vec<_> = store
        .profiles
        .iter()
        .map(|(name, p)| {
            json!({
                "name": name,
                "active": store.active.as_deref() == Some(name.as_str()),
                "url": p.url,
                "ssh": p.ssh,
                "viewer_url": p.viewer_url,
                "bucket": p.bucket,
            })
        })
        .collect();

    ctx.out.emit(
        json!({ "active": store.active, "profiles": rows }),
        || {
            if store.profiles.is_empty() {
                ctx.out.line("no profiles yet. Run `nashcode setup`.");
                return;
            }
            let width = store.profiles.keys().map(|k| k.len()).max().unwrap_or(4);
            for (name, p) in &store.profiles {
                let flag = if store.active.as_deref() == Some(name.as_str()) {
                    "*"
                } else {
                    " "
                };
                ctx.out
                    .line(format!("{flag} {name:width$}  {}", p.url, width = width));
            }
        },
    );
    Ok(())
}

pub fn token(ctx: &Ctx) -> Result<()> {
    let (name, p) = ctx.profile()?;
    if p.token.is_empty() {
        bail!("profile `{name}` holds no token");
    }
    ctx.out
        .emit(json!({ "profile": name, "token": p.token }), || {
            ctx.out.line(&p.token)
        });
    Ok(())
}
