//! Progress, on stderr, and nothing else.
//!
//! Stdout belongs to the agcli envelope: exactly one JSON value per run, so a
//! caller can pipe it straight into a parser. Everything a command wants to say
//! while it works — the step it is on, a warning it cannot fold into the
//! result — goes to stderr, where an agent still reads it and no parser trips
//! over it. `--quiet` silences the steps; a warning is never silenced, because
//! a warning is the one thing a caller cannot infer from the result.

use std::io::Write;

#[derive(Debug, Clone, Copy, Default)]
pub struct Out {
    /// Suppress progress notes on stderr (set by the reserved `--quiet`).
    pub quiet: bool,
}

impl Out {
    pub fn new(quiet: bool) -> Self {
        Self { quiet }
    }

    /// Progress chatter. Always stderr, dropped when quiet.
    ///
    /// Write errors are ignored on purpose: a reader that closes the pipe early
    /// is the reader's business, not a panic.
    pub fn step(&self, s: impl AsRef<str>) {
        if !self.quiet {
            let _ = writeln!(std::io::stderr(), "  {}", s.as_ref());
        }
    }

    /// A warning. Always stderr, never suppressed.
    pub fn warn(&self, s: impl AsRef<str>) {
        let _ = writeln!(std::io::stderr(), "warning: {}", s.as_ref());
    }
}
