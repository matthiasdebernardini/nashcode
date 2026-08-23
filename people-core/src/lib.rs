//! People: who belongs to which project, and the one rule that answers "which
//! project is this message about".
//!
//! The file — `~/.nashcode/people.json`, `NASHCODE_PEOPLE` overrides it — is the only
//! source of truth. It is hand-editable and it is not in git: the iMessage router and
//! the desktop app read it without a checkout, and the numbers in it belong on no
//! mirror. Every consumer asks the same question of the same file, so a client who
//! texts, mails, and joins a Meet lands in one project rather than three.
//!
//! Three binaries share this crate: the viewer serves the routes, the CLI edits and
//! pushes the file, and the desktop app shows it. One matching rule, one definition of
//! the file, and nothing here depends on any of the three. The HTTP helpers live
//! behind the `client` feature, because the server that answers those requests never
//! makes them.
//!
//! ## Refused, and merely reported
//!
//! [`PeopleFile::validate`] returns every finding it has. A *fatal* one breaks the
//! join key — a duplicate id, a project naming an id no person has, an id that is
//! blank — so [`PeopleFile::parse`] refuses the file outright and `PUT /people`
//! answers 400: routing over a broken join silently files work in the wrong place. A
//! warning — a project with no people, a phone that is not E.164, a person nobody can
//! match — means the file loads and something in it will never do its job.
//! `nashcode people check` prints both and exits non-zero for either.

pub mod folders;
pub mod frecency;
pub mod io;
pub mod map;
pub mod model;
pub mod route;

#[cfg(feature = "client")]
pub mod client;

pub use folders::{SyncReport, slug, unique_id};
pub use frecency::{HALF_LIFE_DAYS, by_frecency, frecency};
pub use io::{Pushed, default_path};
pub use map::{ContactKind, ContactRow, contact_map};
pub use model::{Email, Finding, Imsg, PeopleFile, Person, Project, Seen, is_e164};
pub use route::{Contact, Match, Routing, normalize};

#[cfg(feature = "client")]
pub use client::{PushReply, push, pushed_at};
