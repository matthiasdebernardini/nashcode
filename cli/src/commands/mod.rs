//! Command implementations, plus the small amount of state they share.

pub mod doctor;
pub mod plan;
pub mod profiles;
pub mod repo;
pub mod setup;

use crate::api::Client;
use crate::output::Out;
use crate::profile::{Profile, Store};
use anyhow::{Result, bail};

/// What every command gets: the resolved profile and the output mode.
pub struct Ctx {
    pub out: Out,
    pub profile_name: Option<String>,
}

impl Ctx {
    /// Load the store and pick the profile this invocation acts on.
    pub fn profile(&self) -> Result<(String, Profile)> {
        let store = Store::load()?;
        let (name, p) = store.resolve(self.profile_name.as_deref())?;
        Ok((name, p.clone()))
    }

    /// The profile plus an HTTP client aimed at its dgit server.
    pub fn client(&self) -> Result<(String, Profile, Client)> {
        let (name, p) = self.profile()?;
        if p.url.is_empty() {
            bail!("profile `{name}` has no server URL");
        }
        let client = Client::new(&p.url, &p.token);
        Ok((name, p, client))
    }
}
