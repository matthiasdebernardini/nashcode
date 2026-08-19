//! nashcode viewer library: everything the binary wires together, exposed so the
//! integration tests can drive the real router end to end.

pub mod brain;
pub mod bugs;
pub mod ci;
pub mod cli;
pub mod code;
pub mod config;
pub mod db;
pub mod docs;
pub mod git;
pub mod hooks;
pub mod mirror;
pub mod ops;
pub mod render;
pub mod stack;
pub mod traces;
pub mod web;
