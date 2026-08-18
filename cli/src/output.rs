//! Two output modes, one call site.
//!
//! Human mode is terse: short lines on stdout, no banners. `--json` mode emits
//! exactly one JSON value on stdout and nothing else, so an agent can pipe the
//! output straight into a parser. Progress and prompts always go to stderr, so
//! `--json` stdout stays clean even during `setup`.

use serde_json::Value;
use std::io::{IsTerminal, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub struct Out {
    pub mode: Mode,
    /// Suppress progress chatter on stderr (set by `--quiet`).
    pub quiet: bool,
}

impl Out {
    pub fn new(json: bool, quiet: bool) -> Self {
        Self {
            mode: if json { Mode::Json } else { Mode::Human },
            quiet,
        }
    }

    pub fn is_json(&self) -> bool {
        self.mode == Mode::Json
    }

    /// True when a prompt can be shown: interactive terminal and not `--json`.
    pub fn interactive(&self) -> bool {
        self.mode == Mode::Human && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    /// A line of human output. Dropped in `--json` mode.
    ///
    /// Write errors are ignored on purpose: `nashcode ls | head -1` closes the
    /// pipe early, and that is the reader's business, not a panic.
    pub fn line(&self, s: impl AsRef<str>) {
        if self.mode == Mode::Human {
            let _ = writeln!(std::io::stdout(), "{}", s.as_ref());
        }
    }

    /// Progress chatter. Always stderr, dropped when quiet.
    pub fn step(&self, s: impl AsRef<str>) {
        if !self.quiet {
            let _ = writeln!(std::io::stderr(), "  {}", s.as_ref());
        }
    }

    /// A warning. Always stderr in both modes, so `--json` still surfaces it.
    pub fn warn(&self, s: impl AsRef<str>) {
        let _ = writeln!(std::io::stderr(), "warning: {}", s.as_ref());
    }

    /// The single JSON value for this command. Dropped in human mode.
    pub fn json(&self, v: &Value) {
        if self.mode == Mode::Json {
            let _ = writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(v).unwrap_or_default()
            );
        }
    }

    /// The common shape: print human lines from a closure, or the JSON value.
    pub fn emit(&self, v: Value, human: impl FnOnce()) {
        match self.mode {
            Mode::Json => self.json(&v),
            Mode::Human => human(),
        }
    }
}

/// `✓` for ok, `✗` for fail, `-` for anything else (skip).
pub fn mark(status: &str) -> &'static str {
    match status {
        "ok" => "✓",
        "fail" => "✗",
        _ => "-",
    }
}
