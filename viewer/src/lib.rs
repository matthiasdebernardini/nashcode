//! nashcode viewer library: everything the binary wires together, exposed so the
//! integration tests can drive the real router end to end.

pub mod brain;
pub mod bugs;
pub mod ci;
pub mod cli;
pub mod code;
pub mod config;
pub mod context;
pub mod db;
pub mod docs;
pub mod git;
pub mod hooks;
pub mod mirror;
pub mod ops;
/// Who belongs to which project. The model, the validation, and the routing rule are
/// `people-core`, so the CLI and the desktop app run the same code without building a
/// server; the routes over it are in [`web::api`].
pub use people_core as people;
pub mod render;
pub mod stack;
pub mod traces;
pub mod upstream;
pub mod web;
