//! The hand-written control set: a row, a button, a link, a toggle, a text field,
//! and the caret arithmetic the field runs on.
//!
//! `gpui-component` supplies these; it cannot be in this build (see `theme.rs`), so
//! they are here. Each one is value-like — everything it draws comes from its
//! arguments, it retains nothing between frames, and it takes no callback. The view
//! that owns the behavior wraps it in `div().id(...).on_click(...)`, so identity and
//! side effects stay with the entity that can be held responsible for them.
//!
//! The caret functions at the bottom are the text field's behavior, kept beside its
//! presentation because they change together. They are pure and they are tested:
//! every one of them is an invariant about a string and a byte offset into it.

use gpui::{Div, ElementId, Stateful, div, prelude::*, px, rems};

use crate::theme::{Theme, space};

/// A stable element id built from a domain key rather than a list position, so a
/// row keeps its identity when the list above it grows.
pub fn eid(prefix: &str, key: &str) -> ElementId {
    ElementId::Name(format!("{prefix}:{key}").into())
}

/// A column. `gpui-component`'s `v_flex`, spelled for one file.
pub fn v() -> Div {
    div().flex().flex_col()
}

/// A row, centred on its own baseline so mixed text sizes share one line.
pub fn h() -> Div {
    div().flex().flex_row().items_center()
}

/// The label above a region. Short, quiet, and never a sentence.
pub fn section_title(text: impl Into<gpui::SharedString>, theme: &Theme) -> Div {
    div().text_xs().text_color(theme.muted).child(text.into())
}

/// Ordinary body text.
pub fn body(text: impl Into<gpui::SharedString>, theme: &Theme) -> Div {
    div().text_sm().text_color(theme.foreground).child(text.into())
}

/// Secondary text: metadata, help, a count.
pub fn muted(text: impl Into<gpui::SharedString>, theme: &Theme) -> Div {
    div().text_sm().text_color(theme.muted).child(text.into())
}

/// How a card reads while a selection is up.
///
/// The canvas answers one question at a time — "where does this route?" — so the
/// three states are about that answer, not about decoration: this is the answer, this
/// is part of it, this is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    /// Nothing is selected. Every card reads the same.
    Plain,
    /// The selected card itself.
    Selected,
    /// Connected to the selection.
    Lit,
    /// Not connected. Still legible, and deliberately quiet.
    Dim,
}

impl CardState {
    /// Body text on a card in this state.
    pub fn ink(self, theme: &Theme) -> gpui::Hsla {
        match self {
            CardState::Dim => theme.muted,
            _ => theme.foreground,
        }
    }

    /// Secondary text on a card in this state. Dimming both would flatten the card,
    /// so the quiet line stays where it is and only the loud one steps back.
    pub fn wash(self, theme: &Theme) -> gpui::Hsla {
        match self {
            CardState::Dim => theme.border,
            _ => theme.muted,
        }
    }
}

/// One card on the canvas. The caller adds `on_click`; the card owns no behavior.
pub fn card(id: impl Into<ElementId>, state: CardState, theme: &Theme) -> Stateful<Div> {
    let base = v()
        .id(id)
        // The links layer reads this card's resolved bounds out of a canvas child,
        // which has to position against the card itself.
        .relative()
        .gap(space::HAIR)
        .px(space::TIGHT)
        .py(space::PART)
        .rounded(theme.radius)
        .border_1()
        .text_sm();

    match state {
        CardState::Selected => base.bg(theme.accent_soft).border_color(theme.accent),
        CardState::Lit => base
            .bg(theme.surface)
            .border_color(theme.accent)
            .hover(|style| style.bg(theme.accent_soft)),
        CardState::Plain => base
            .bg(theme.surface)
            .border_color(theme.border)
            .hover(|style| style.border_color(theme.accent)),
        CardState::Dim => base
            .bg(theme.background)
            .border_color(theme.border)
            .opacity(0.55)
            .hover(|style| style.opacity(1.0)),
    }
}

/// What a button means here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The default commit of a decision area. One per area, or none.
    Primary,
    /// An ordinary visible command.
    Default,
    /// A quiet inline command.
    Ghost,
    /// Destructive commitment.
    Danger,
}

/// A command. The caller attaches `on_click` only when `enabled`, so a disabled
/// button cannot be pressed and does not pretend it can.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<gpui::SharedString>,
    tone: Tone,
    enabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let base = h()
        .id(id)
        .justify_center()
        .px(space::GROUP)
        .py(space::PART)
        .rounded(theme.radius)
        .text_sm()
        .border_1()
        .child(label.into());

    if !enabled {
        // Lower emphasis and no hover response: the control must not look like it
        // would do something.
        return base.border_color(theme.border).text_color(theme.muted).opacity(0.6);
    }

    match tone {
        Tone::Primary => base
            .bg(theme.accent)
            .border_color(theme.accent)
            .text_color(theme.accent_foreground)
            .hover(|style| style.opacity(0.88)),
        Tone::Default => base
            .bg(theme.background)
            .border_color(theme.border)
            .text_color(theme.foreground)
            .hover(|style| style.bg(theme.accent_soft)),
        Tone::Ghost => base
            .border_color(gpui::transparent_black())
            .text_color(theme.muted)
            .hover(|style| style.text_color(theme.foreground).bg(theme.accent_soft)),
        Tone::Danger => base
            .border_color(theme.danger)
            .text_color(theme.danger)
            .hover(|style| style.bg(theme.danger).text_color(theme.accent_foreground)),
    }
}

/// A URL, opened by the browser. Underlined and pointing-hand, because its contract
/// is leaving this application. Never a command.
pub fn link(
    id: impl Into<ElementId>,
    label: impl Into<gpui::SharedString>,
    theme: &Theme,
) -> Stateful<Div> {
    h().id(id)
        .cursor_pointer()
        .text_sm()
        .text_color(theme.accent)
        .underline()
        .child(label.into())
}

/// An independent on/off setting. A glyph rather than an icon font: v1 draws no
/// images, and the state must not be carried by colour alone.
pub fn toggle(
    id: impl Into<ElementId>,
    label: impl Into<gpui::SharedString>,
    checked: bool,
    focused: bool,
    theme: &Theme,
) -> Stateful<Div> {
    h().id(id)
        .gap(space::TIGHT)
        .px(space::PART)
        .py(space::HAIR)
        .rounded(theme.radius)
        .border_1()
        .border_color(if focused { theme.accent } else { gpui::transparent_black() })
        .text_sm()
        .text_color(theme.foreground)
        .child(div().text_color(theme.muted).child(if checked { "[x]" } else { "[ ]" }))
        .child(label.into())
}

/// A pill: a person on a project, a count, a short state.
pub fn pill(theme: &Theme) -> Div {
    h().gap(space::PART)
        .px(space::TIGHT)
        .py(space::HAIR)
        .rounded_full()
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .text_sm()
        .text_color(theme.foreground)
}

/// Everything the text field draws.
pub struct TextField<'a> {
    pub label: &'a str,
    pub value: &'a str,
    /// What to show instead of nothing. Muted, and never a value.
    pub placeholder: &'a str,
    pub focused: bool,
    /// A byte offset into `value`; only read when `focused`.
    pub caret: usize,
    /// One entry per line, and Enter adds a line. A single-line field ignores Enter.
    pub multiline: bool,
}

/// A labelled text field.
///
/// The caller owns the click: wrap this in `div().id(...).on_click(...)` so the
/// element's identity belongs to the view that knows which field it is.
pub fn text_field(field: TextField<'_>, theme: &Theme) -> Div {
    let mut frame = v()
        .px(space::TIGHT)
        .py(space::PART)
        .bg(theme.background)
        .border_1()
        .border_color(if field.focused { theme.accent } else { theme.border })
        .rounded(theme.radius)
        .text_sm()
        .text_color(theme.foreground)
        .overflow_hidden();
    if field.multiline {
        frame = frame.min_h(rems(4.5));
    }

    if field.value.is_empty() && !field.focused {
        frame = frame.child(div().text_color(theme.muted).child(field.placeholder.to_owned()));
        return labelled(field.label, frame, theme);
    }

    let caret = clamp(field.value, field.caret);
    let mut offset = 0usize;
    for line in field.value.split('\n') {
        let start = offset;
        let end = start + line.len();
        offset = end + 1; // past the '\n' this split removed

        // One line's height, held even when the line is empty: a blank line in a
        // one-entry-per-line field is a place the operator is about to type into,
        // and a row that collapsed to nothing would hide it.
        let row = h().min_h(rems(1.15));
        let has_caret = field.focused && caret >= start && caret <= end;
        frame = frame.child(if has_caret {
            let split = caret - start;
            row.child(div().child(line[..split].to_owned()))
                .child(caret_bar(theme))
                .child(div().child(line[split..].to_owned()))
        } else {
            row.child(div().child(line.to_owned()))
        });
    }
    labelled(field.label, frame, theme)
}

/// The caret. Its width is one device hairline — the one place in this app where a
/// pixel is the right unit, because it is a physical boundary and not product
/// geometry. Its height rides the text, so it scales with the base font.
fn caret_bar(theme: &Theme) -> Div {
    div().w(px(1.5)).h(rems(1.15)).bg(theme.accent)
}

/// Label above control, closer to it than to whatever comes next.
fn labelled(label: &str, control: Div, theme: &Theme) -> Div {
    v().gap(space::PART).child(section_title(label.to_owned(), theme)).child(control)
}

// ---------------------------------------------------------------------------
// Caret arithmetic. Byte offsets, always on a character boundary.
// ---------------------------------------------------------------------------

/// The nearest legal caret position at or before `caret`. Every function here
/// starts with this, so a caret can never land inside a multi-byte character and
/// panic a slice.
pub fn clamp(text: &str, caret: usize) -> usize {
    let mut at = caret.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Type. Answers the caret's new home.
pub fn insert(text: &mut String, caret: usize, typed: &str) -> usize {
    let at = clamp(text, caret);
    text.insert_str(at, typed);
    at + typed.len()
}

/// Backspace: remove the character before the caret.
pub fn backspace(text: &mut String, caret: usize) -> usize {
    let at = clamp(text, caret);
    let from = left(text, at);
    text.replace_range(from..at, "");
    from
}

/// Delete: remove the character after the caret.
pub fn delete(text: &mut String, caret: usize) -> usize {
    let at = clamp(text, caret);
    let to = right(text, at);
    text.replace_range(at..to, "");
    at
}

pub fn left(text: &str, caret: usize) -> usize {
    let at = clamp(text, caret);
    text[..at].chars().next_back().map_or(0, |ch| at - ch.len_utf8())
}

pub fn right(text: &str, caret: usize) -> usize {
    let at = clamp(text, caret);
    text[at..].chars().next().map_or(at, |ch| at + ch.len_utf8())
}

pub fn line_start(text: &str, caret: usize) -> usize {
    let at = clamp(text, caret);
    text[..at].rfind('\n').map_or(0, |newline| newline + 1)
}

pub fn line_end(text: &str, caret: usize) -> usize {
    let at = clamp(text, caret);
    text[at..].find('\n').map_or(text.len(), |newline| at + newline)
}

/// Up a line, keeping the column where it can. A short line above catches the caret
/// at its end rather than losing it.
pub fn up(text: &str, caret: usize) -> usize {
    let start = line_start(text, caret);
    if start == 0 {
        return 0;
    }
    let column = text[start..clamp(text, caret)].chars().count();
    let above_end = start - 1;
    advance(text, line_start(text, above_end), column, above_end)
}

/// Down a line, by the same rule.
pub fn down(text: &str, caret: usize) -> usize {
    let end = line_end(text, caret);
    if end == text.len() {
        return text.len();
    }
    let column = text[line_start(text, caret)..clamp(text, caret)].chars().count();
    let below_start = end + 1;
    advance(text, below_start, column, line_end(text, below_start))
}

/// `column` characters past `from`, stopping at `limit`.
fn advance(text: &str, from: usize, column: usize, limit: usize) -> usize {
    let mut at = from;
    for _ in 0..column {
        if at >= limit {
            break;
        }
        at = right(text, at);
    }
    at.min(limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_erasing_land_the_caret_where_the_next_key_expects_it() {
        let mut text = String::new();
        let mut caret = insert(&mut text, 0, "ab");
        assert_eq!((text.as_str(), caret), ("ab", 2));

        caret = left(&text, caret);
        caret = insert(&mut text, caret, "X");
        assert_eq!((text.as_str(), caret), ("aXb", 2));

        caret = backspace(&mut text, caret);
        assert_eq!((text.as_str(), caret), ("ab", 1));

        caret = delete(&mut text, caret);
        assert_eq!((text.as_str(), caret), ("a", 1));

        // Both erase keys at the ends of the string do nothing rather than panic.
        let end = text.len();
        assert_eq!(backspace(&mut text, 0), 0);
        assert_eq!(delete(&mut text, end), end);
        assert_eq!(text, "a");
    }

    #[test]
    fn a_caret_never_lands_inside_a_character() {
        // A phone list can hold a name; a name can hold anything.
        let text = "é🙂x".to_owned();
        assert_eq!(clamp(&text, 1), 0, "half of the é is not a place");
        assert_eq!(clamp(&text, 99), text.len());

        let mut at = 0;
        for _ in 0..3 {
            at = right(&text, at);
            assert!(text.is_char_boundary(at));
        }
        assert_eq!(at, text.len());
        for _ in 0..3 {
            at = left(&text, at);
            assert!(text.is_char_boundary(at));
        }
        assert_eq!(at, 0);
    }

    #[test]
    fn the_line_keys_work_on_the_line_the_caret_is_on() {
        let text = "+15550001111\n+15550002222\n";
        let second = 13; // the first character of the second line

        assert_eq!(line_start(text, second + 4), second);
        assert_eq!(line_end(text, second + 4), 25);
        assert_eq!(line_start(text, 0), 0);
        assert_eq!(line_end(text, text.len()), text.len(), "the trailing line is empty");
    }

    #[test]
    fn up_and_down_keep_the_column_and_stop_at_the_ends() {
        let text = "abcdef\nxy\nlonger line";
        // Column 4 on the first line; the short middle line catches it at its end.
        assert_eq!(down(text, 4), 9, "'xy' has no column 4");
        assert_eq!(down(text, 9), 12, "back out to column 2 of the last line");
        assert_eq!(up(text, 12), 9, "and back up to where it came from");
        assert_eq!(up(text, 2), 0, "the first line goes to the start, not nowhere");
        assert_eq!(down(text, text.len()), text.len(), "and the last line stays put");
    }
}
