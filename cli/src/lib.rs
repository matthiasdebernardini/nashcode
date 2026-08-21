//! nashcode: deploy and operate a self-hosted dgit git server on celld,
//! behind your own tailnet.
//!
//! The library exists so integration tests can reach the pure parts (profile
//! store, script generators, jj detection) without going through the binary.
//! The binary in `main.rs` is a thin dispatcher over [`commands`].

pub mod api;
pub mod cli;
pub mod commands;
pub mod exit;
pub mod output;
pub mod profile;
pub mod remote;
pub mod ssh;
pub mod timefmt;
pub mod vcs;
