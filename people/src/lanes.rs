//! The canvas: three lanes of cards and the wires between them.
//!
//! Presentation only. Every card reads its state from [`PeopleApp`] and hands every
//! click straight back to it.
//!
//! Two things here are not ordinary layout. Each card carries a zero-cost `canvas`
//! child whose only job is to write the card's resolved bounds into a shared map
//! during prepaint. The links layer is another `canvas`, first in the tree so it
//! paints under the cards, which reads that map during paint. GPUI prepaints the
//! whole tree before it paints any of it, so the wires a frame draws are measured
//! from the same frame's cards — through a scroll, a resize, or a change of base font.

use gpui::{Context, canvas, div, prelude::*, px, rems};
use people_core::ContactKind;

use crate::app::PeopleApp;
use crate::board::{Band, Board, CardId, Lane};
use crate::links::{self, Endpoints};
use crate::theme::{Theme, ThemeExt, space};
use crate::widgets::{self, CardState, Tone, h, section_title, v};

/// Lane widths. The contacts and projects lanes hold addresses and paths, which are
/// long; the people lane holds names, which are not.
const CONTACTS: f32 = 14.5;
const PEOPLE: f32 = 12.5;
const PROJECTS: f32 = 15.0;

/// How wide a wire is, and how close the pointer has to come to claim one. Both are
/// physical: a stroke width and a hit tolerance, neither of them product spacing.
const WIRE: f32 = 1.5;
const REACH: f32 = 7.0;

impl PeopleApp {
    pub(crate) fn render_canvas(&self, board: &Board, cx: &Context<Self>) -> impl IntoElement {
        let lit = self.selected.as_ref().map(|id| board.connected(id));

        // One scroll owner for the whole canvas, not one per lane: three lanes that
        // scrolled apart would tear every wire that crosses between them.
        div()
            .id("canvas")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            // The wires have no elements of their own, so the pointer is tested
            // against their geometry here, where the canvas can see it.
            .on_mouse_move(self.pointer_listener(cx))
            // A pointer that left the canvas is on no wire. The hover is an index into
            // one frame's link list, so a stale one would light two cards that the
            // pointer is nowhere near.
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.clear_hover(cx);
                }
            }))
            .child(self.render_links(board, cx))
            .child(
                h().items_start()
                    .gap(space::REGION)
                    .child(self.contacts_lane(board, lit.as_ref(), cx))
                    .child(self.people_lane(board, lit.as_ref(), cx))
                    .child(self.projects_lane(board, lit.as_ref(), cx)),
            )
    }

    /// The wires. First child, so it paints beneath the cards it joins.
    fn render_links(&self, board: &Board, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (plain, accent) = (theme.border, theme.accent);
        let bounds = self.card_bounds.clone();
        let geometry = self.link_geometry.clone();
        let hovered = self.hover_link;
        let lit = self.selected.as_ref().map(|id| board.connected(id));
        let links = board.links.clone();

        canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                let places = bounds.borrow();
                let mut drawn: Vec<(usize, Endpoints)> = Vec::new();

                for (index, link) in links.iter().enumerate() {
                    let (Some(from), Some(to)) = (places.get(&link.from), places.get(&link.to))
                    else {
                        // A card that has not been laid out yet has no wire. It will
                        // have one on the next frame.
                        continue;
                    };
                    let ends = links::endpoints(*from, *to);
                    drawn.push((index, ends));

                    let highlighted = hovered == Some(index)
                        || lit
                            .as_ref()
                            .is_some_and(|set| set.contains(&link.from) && set.contains(&link.to));
                    // A selection that does not include this wire pushes it back to
                    // the surface it sits on rather than hiding it: the picture keeps
                    // its shape while one route is being read.
                    let dimmed = lit.is_some() && !highlighted;
                    let colour = if highlighted { accent } else { plain };
                    let width = if highlighted { px(WIRE + 0.75) } else { px(WIRE) };

                    if let Some(path) = links::path(&ends, width) {
                        window.paint_path(path, if dimmed { colour.opacity(0.35) } else { colour });
                    }
                }
                *geometry.borrow_mut() = drawn;
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    fn contacts_lane(
        &self,
        board: &Board,
        lit: Option<&std::collections::BTreeSet<CardId>>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        // Addresses collapse on the same rule as the other two lanes. Thirty-five
        // client folders is seventy addresses, and a lane that drew every one of them
        // in full while the two beside it collapsed would be the tallest column on a
        // canvas that scrolls as one piece — it would push the people and the projects
        // off the fold to say "phone" seventy times.
        let open = board.expanded(Lane::Contacts, self.selected.as_ref());
        let mut lane = self
            .lane_head(Lane::Contacts, board.contacts.len(), open.len(), None, cx)
            .w(rems(CONTACTS));

        let mut band = Band::Routes;
        for card in &board.contacts {
            if card.band != band {
                band = card.band;
                lane = lane.children(band_title(band, theme));
            }
            let id = card.id.clone();
            let state = self.card_state(board, &id, lit);
            let kind = match card.kind {
                ContactKind::Phone => "phone",
                ContactKind::Email => "email",
            };
            // The address alone does not name a card: two people may hold it, and one
            // person may hold it twice. The id carries the owner and the occurrence
            // with it, so the three never collide.
            let mut face = widgets::card(widgets::eid("contact", &card.id.key()), state, theme)
                .child(div().truncate().text_color(state.ink(theme)).child(card.value.clone()));
            // Collapsed, a contact is its address and its wire. The kind is already in
            // the address — a `+` or an `@` — and a warning about an address nobody is
            // looking at is a warning read on the card that was clicked.
            if open.contains(&id) {
                face = face
                    .child(div().text_xs().text_color(state.wash(theme)).child(kind))
                    .children(warning(card.warning.as_deref(), theme));
            }
            lane = lane.child(
                face.child(self.measure(id.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_card(id.clone(), cx))),
            );
        }
        lane
    }

    fn people_lane(
        &self,
        board: &Board,
        lit: Option<&std::collections::BTreeSet<CardId>>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let open = board.expanded(Lane::People, self.selected.as_ref());
        let mut lane = self
            .lane_head(Lane::People, board.people.len(), open.len(), Some("New person"), cx)
            .w(rems(PEOPLE));

        let mut band = Band::Routes;
        for card in &board.people {
            if card.band != band {
                band = card.band;
                lane = lane.children(band_title(band, theme));
            }
            let id = card.id.clone();
            let state = self.card_state(board, &id, lit);
            let mut face = widgets::card(widgets::eid("person", &card.person), state, theme)
                .child(div().truncate().text_color(state.ink(theme)).child(card.name.clone()));
            if open.contains(&id) {
                face = face
                    .child(
                        div()
                            .text_xs()
                            .text_color(state.wash(theme))
                            .child(format!("{} ph · {} em", card.phones, card.emails)),
                    )
                    .children(warmth(card.warmth.as_deref(), state, theme))
                    .children(warning(card.warning.as_deref(), theme));
            }
            lane = lane.child(
                face.child(self.measure(id.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_card(id.clone(), cx))),
            );
        }
        lane
    }

    fn projects_lane(
        &self,
        board: &Board,
        lit: Option<&std::collections::BTreeSet<CardId>>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let open = board.expanded(Lane::Projects, self.selected.as_ref());
        let mut lane = self
            .lane_head(Lane::Projects, board.projects.len(), open.len(), Some("New project"), cx)
            .w(rems(PROJECTS));

        for card in &board.projects {
            let id = card.id.clone();
            let state = self.card_state(board, &id, lit);
            // A collapsed project is its name and its wire, and nothing else. Thirty
            // full cards would be a lane nobody reads; thirty names is a list somebody
            // finds one in.
            if !open.contains(&id) {
                lane = lane.child(
                    widgets::card(widgets::eid("project", &card.project), state, theme)
                        .child(
                            div().truncate().text_color(state.ink(theme)).child(card.name.clone()),
                        )
                        .child(self.measure(id.clone()))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.select_card(id.clone(), cx)),
                        ),
                );
                continue;
            }
            // The repo is a page on the viewer, so it is a Link and it opens a
            // browser. Every other word on the card is a fact, not a destination.
            let repo: gpui::AnyElement = match (&card.repo, &self.viewer) {
                (None, _) => div()
                    .text_xs()
                    .text_color(state.wash(theme))
                    .child("no nashcode repo")
                    .into_any_element(),
                (Some(repo), Ok(_)) => {
                    let target = repo.clone();
                    widgets::link(widgets::eid("repo", repo), repo.clone(), theme)
                        .text_xs()
                        .on_click(cx.listener(move |this, _, _, _| this.open_repo(&target)))
                        .into_any_element()
                }
                (Some(repo), Err(_)) => div()
                    .text_xs()
                    .text_color(state.wash(theme))
                    .child(format!("{repo} — no viewer to open it on"))
                    .into_any_element(),
            };
            lane = lane.child(
                widgets::card(widgets::eid("project", &card.project), state, theme)
                    .child(div().truncate().text_color(state.ink(theme)).child(card.name.clone()))
                    .child(repo)
                    .child(
                        div()
                            .text_xs()
                            .text_color(state.wash(theme))
                            .truncate()
                            .child(card.folder.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(state.wash(theme))
                            .child(people_count(card.people)),
                    )
                    .children(warmth(card.warmth.as_deref(), state, theme))
                    .children(warning(card.warning.as_deref(), theme))
                    .child(self.measure(id.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_card(id.clone(), cx))),
            );
        }
        lane
    }

    /// A lane's title, its count, and — for the two lanes that own a collection —
    /// the command that adds to it.
    ///
    /// `shown` is how many of `count` are drawn in full. When it is fewer, the header
    /// says so: a lane that quietly stopped drawing cards would read as a lane that
    /// had lost them.
    fn lane_head(
        &self,
        lane: Lane,
        count: usize,
        shown: usize,
        add: Option<&'static str>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let focused = self.lane == lane && self.field.is_none();
        let mut head = h()
            .justify_between()
            .pb(space::PART)
            .mb(space::PART)
            .border_b_1()
            // The focused lane keeps a visible keyboard mark even while the selection
            // is what the eye is on.
            .border_color(if focused { theme.accent } else { theme.border })
            .child(
                h().gap(space::TIGHT)
                    .child(div().text_sm().text_color(theme.foreground).child(lane.title()))
                    .child(section_title(lane_count(count, shown), theme)),
            );
        if let Some(label) = add {
            let is_person = lane == Lane::People;
            head = head.child(
                widgets::button(widgets::eid("add", label), label, Tone::Ghost, true, theme)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if is_person {
                            this.new_person(cx);
                        } else {
                            this.new_project(cx);
                        }
                    })),
            );
        }
        v().gap(space::TIGHT).flex_shrink_0().child(head)
    }

    /// The zero-size element that reports a card's resolved bounds to the links layer.
    fn measure(&self, id: CardId) -> impl IntoElement {
        let places = self.card_bounds.clone();
        canvas(
            move |bounds, _, _| {
                places.borrow_mut().insert(id, bounds);
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// How a card reads under the current selection.
    pub(crate) fn card_state(
        &self,
        board: &Board,
        id: &CardId,
        lit: Option<&std::collections::BTreeSet<CardId>>,
    ) -> CardState {
        if self.selected.as_ref() == Some(id) {
            return CardState::Selected;
        }
        // A hovered wire lights its two ends even when nothing is selected: that is
        // what makes a wire answer "which two?" before it is clicked.
        if let Some(index) = self.hover_link
            && let Some(link) = board.links.get(index)
            && (&link.from == id || &link.to == id)
        {
            return CardState::Lit;
        }
        match lit {
            None => CardState::Plain,
            Some(set) if set.contains(id) => CardState::Lit,
            Some(_) => CardState::Dim,
        }
    }

    /// How close the pointer has to come to a wire to claim it.
    pub(crate) fn wire_reach() -> gpui::Pixels {
        px(REACH)
    }
}

/// The line that opens a band. The routing band is the lane itself and needs no
/// title; the other two are the two ways an address can lead nowhere, and each says
/// which.
fn band_title(band: Band, theme: &Theme) -> Option<gpui::Div> {
    let (words, colour) = match band {
        Band::Routes => return None,
        Band::Nowhere => ("Routes nowhere", theme.warning),
        Band::Mine => ("Yours, never scores", theme.muted),
    };
    Some(div().pt(space::GROUP).pb(space::PART).text_xs().text_color(colour).child(words))
}

/// The one short line about what is wrong with this card, if anything is.
fn warning(text: Option<&str>, theme: &Theme) -> Option<gpui::Div> {
    text.map(|text| {
        div().pt(space::HAIR).text_xs().text_color(theme.warning).child(text.to_owned())
    })
}

/// `35 · 10 shown`, or just `8` when the whole lane is drawn.
fn lane_count(count: usize, shown: usize) -> String {
    if shown >= count { count.to_string() } else { format!("{count} · {shown} shown") }
}

/// The warmth the lane's order was decided by, under the card it decided.
fn warmth(text: Option<&str>, state: CardState, theme: &Theme) -> Option<gpui::Div> {
    text.map(|text| div().text_xs().text_color(state.wash(theme)).child(text.to_owned()))
}

fn people_count(people: usize) -> String {
    match people {
        1 => "1 person".to_owned(),
        n => format!("{n} people"),
    }
}

#[cfg(test)]
mod tests {
    use super::{lane_count, people_count};

    #[test]
    fn a_project_with_one_person_on_it_does_not_say_1_people() {
        assert_eq!(people_count(0), "0 people");
        assert_eq!(people_count(1), "1 person");
        assert_eq!(people_count(4), "4 people");
    }

    #[test]
    fn a_lane_header_says_how_much_of_itself_it_is_drawing() {
        assert_eq!(lane_count(35, 10), "35 · 10 shown");
        // A selection opens its chain, and the header counts what is on screen.
        assert_eq!(lane_count(35, 12), "35 · 12 shown");
        // A lane that fits says its size and nothing more: "8 · 8 shown" is noise.
        assert_eq!(lane_count(8, 8), "8");
        assert_eq!(lane_count(0, 0), "0");
    }
}
