//! The binary. Everything it does is in `cli::build`.
//!
//! `run_env` parses the process arguments, dispatches, and hands back an
//! `Execution`; `finish` prints the envelope on stdout — or nothing at all, when
//! a raw command such as `grep` already wrote its own — and exits with the typed
//! code.

#[tokio::main]
async fn main() {
    nashcode_cli::cli::build().run_env().await.finish()
}
