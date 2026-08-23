//! The one place a colour, a radius, or a gap is a number.
//!
//! `gpui-component` would supply `cx.theme()`; it pins Zed's `gpui`, a different
//! package from the `gpui-ce` this app is built on, and the two cannot both be in one
//! build. So the token layer is here, and the rest of the app reads it the same way
//! the guides say to read the ecosystem's: by semantic role, never by palette
//! position, and never as a literal at a call site.
//!
//! [`Theme`] is a GPUI global, set once at startup. That is the extension point a
//! second theme would use; it is not a per-view field, because a colour is not view
//! state.
//!
//! Spacing and radius are [`Rems`], not pixels, so type, whitespace, and control
//! frames share the window's one zoom axis. The steps are the design guide's own
//! scale — 2, 4, 8, 12, 16, 24, 32 px at a 16 px base — named by the relationship
//! they express rather than by the number they currently resolve to.

use gpui::{App, Global, Hsla, Rems, rems, rgb, rgba};

/// The semantic colour roles this app draws with.
///
/// Fields are public and record-like on purpose: a theme is configuration, and the
/// compatibility cost of adding a role is a compile error at the one place that
/// builds one.
pub struct Theme {
    /// The window itself.
    pub background: Hsla,
    /// Panels, list bodies, and input frames that sit on the background.
    pub surface: Hsla,
    /// Hairlines and control boundaries.
    pub border: Hsla,
    /// Body text.
    pub foreground: Hsla,
    /// Secondary metadata, help, and placeholders.
    pub muted: Hsla,
    /// The selection and the keyboard focus ring. One accent, spent sparingly.
    pub accent: Hsla,
    /// Text on top of `accent`.
    pub accent_foreground: Hsla,
    /// The backing of a selected row: the accent at low alpha, so the row reads as
    /// chosen without becoming the loudest thing on screen.
    pub accent_soft: Hsla,
    /// A refusal, and the one destructive command.
    pub danger: Hsla,
    /// Something loaded and will not work. Never a refusal.
    pub warning: Hsla,
    /// The corner radius of a control. Circles and pills use `rounded_full`.
    pub radius: Rems,
}

impl Global for Theme {}

impl Theme {
    /// The only theme there is today. A quiet dark desktop surface: the content
    /// carries the screen, and the accent appears only on selection and focus.
    pub fn dark() -> Self {
        Self {
            background: rgb(0x17181b).into(),
            surface: rgb(0x1f2125).into(),
            border: rgb(0x32353b).into(),
            foreground: rgb(0xe7e8ec).into(),
            muted: rgb(0x8c919b).into(),
            accent: rgb(0x6ea8fe).into(),
            accent_foreground: rgb(0x0d1117).into(),
            accent_soft: rgba(0x6ea8fe33).into(),
            danger: rgb(0xe5646d).into(),
            warning: rgb(0xd8a03c).into(),
            radius: rems(0.375),
        }
    }
}

/// `cx.theme()`, spelled the way the guides spell it.
pub trait ThemeExt {
    fn theme(&self) -> &Theme;
}

impl ThemeExt for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// The spacing scale, named by relationship. Picking a gap means answering what the
/// two things mean to each other, not how far apart they should look.
pub mod space {
    use gpui::{Rems, rems};

    /// Optical correction: a glyph baseline, a compact separator. 2 px.
    pub const HAIR: Rems = rems(0.125);
    /// Parts of one control: a label and its caret, a chip and its remove glyph. 4 px.
    pub const PART: Rems = rems(0.25);
    /// Closely related controls: a button's label and its neighbour. 8 px.
    pub const TIGHT: Rems = rems(0.5);
    /// One content group: a form row, a list row's columns. 12 px.
    pub const GROUP: Rems = rems(0.75);
    /// Separate groups in one section: panel padding, form groups. 16 px.
    pub const SECTION: Rems = rems(1.0);
    /// A major region boundary: the breathing room an empty state needs. 32 px.
    pub const REGION: Rems = rems(2.0);
}
