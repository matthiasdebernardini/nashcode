//! `nashcode use`, `nashcode profiles`, `nashcode token`.

use super::Ctx;
use crate::profile::Store;
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub fn use_profile(_ctx: &Ctx, name: &str) -> Result<Value> {
    let mut store = Store::load()?;
    store.set_active(name)?;
    store.save()?;
    let p = &store.profiles[name];
    Ok(json!({ "active": name, "url": p.url, "ssh": p.ssh }))
}

pub fn list(_ctx: &Ctx) -> Result<Value> {
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

    Ok(json!({ "active": store.active, "profiles": rows }))
}

pub fn token(ctx: &Ctx) -> Result<Value> {
    let (name, p) = ctx.profile()?;
    if p.token.is_empty() {
        bail!("profile `{name}` holds no token");
    }
    Ok(json!({ "profile": name, "token": p.token }))
}
