//! The fourth column: whatever card is selected, in full, and editable.
//!
//! The canvas answers "where does this route?". The inspector answers "and what is
//! it?" — so nothing is edited on the canvas and nothing is drawn twice. It has one
//! field ring per kind of card, which is also the keyboard's ring, and it owns the
//! one destructive command in the window.

use gpui::{Context, div, prelude::*, rems};
use people_core::ContactKind;

use crate::app::{Field, PeopleApp, Suggested};
use crate::board::{Board, CardId};
use crate::edit::display;
use crate::theme::{ThemeExt, space};
use crate::widgets::{self, TextField, Tone, h, muted, section_title, v};

impl PeopleApp {
    pub(crate) fn render_inspector(&self, board: &Board, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let body: gpui::AnyElement = match self.selected.clone() {
            None => self.nothing_selected(cx).into_any_element(),
            Some(CardId::Person(id)) => self.person_inspector(board, &id, cx).into_any_element(),
            Some(CardId::Project(id)) => {
                self.project_inspector(board, &id, cx).into_any_element()
            }
            Some(id @ CardId::Contact { .. }) => {
                self.contact_inspector(board, &id, cx).into_any_element()
            }
        };

        v().id("inspector")
            .w(rems(21.))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .p(space::SECTION)
            .gap(space::SECTION)
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius)
            .child(body)
    }

    /// The empty inspector says what to do and offers the two things there are to do.
    fn nothing_selected(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v().gap(space::GROUP)
            .child(muted("Click a card to see where it routes.", theme))
            .child(
                h().gap(space::TIGHT)
                    .child(
                        widgets::button("new-person", "New person", Tone::Default, true, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.new_person(cx))),
                    )
                    .child(
                        widgets::button("new-project", "New project", Tone::Default, true, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.new_project(cx))),
                    ),
            )
    }

    fn person_inspector(&self, board: &Board, id: &str, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(person) = self.edit.person(id) else {
            return v().child(muted("That person is gone.", theme));
        };
        let key = id.to_owned();

        // Every project, filled when this person is on it. One row, so membership is
        // a thing you see rather than a list you maintain. Warmest first, the same
        // order the lane behind it draws — a chip row sorted differently from the lane
        // it names would be a second answer to "which project matters".
        let mut chips = h().gap(space::TIGHT).flex_wrap();
        if board.projects.is_empty() {
            chips = chips.child(muted("No projects yet.", theme).text_xs());
        }
        for card in &board.projects {
            let Some(project) = self.edit.project(&card.project) else {
                continue;
            };
            let on = project.people.iter().any(|listed| listed == id);
            let name = display(&project.id, &project.name);
            let project_id = project.id.clone();
            let person_id = key.clone();
            let mut chip = widgets::pill(theme)
                .id(widgets::eid("member", &project.id))
                .cursor_pointer()
                .child(div().child(name))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_membership(&project_id, &person_id, !on, cx);
                }));
            chip = if on {
                chip.bg(theme.accent_soft).border_color(theme.accent)
            } else {
                chip.text_color(theme.muted)
            };
            chips = chips.child(chip);
        }

        v().gap(space::SECTION)
            .child(
                h().justify_between()
                    .items_start()
                    .child(
                        v().gap(space::HAIR)
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .text_lg()
                                    .truncate()
                                    .child(display(&person.id, &person.name)),
                            )
                            .child(section_title(format!("id {}", person.id), theme)),
                    )
                    .child(
                        widgets::button(
                            widgets::eid("delete-person", id),
                            "Delete",
                            Tone::Danger,
                            true,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_selected(cx))),
                    ),
            )
            .child(
                v().gap(space::GROUP)
                    .child(self.field(Field::PersonName, "Name", "What to call them", cx))
                    .child(self.field(
                        Field::PersonPhones,
                        "Phones",
                        "One E.164 number per line, e.g. +15550001111",
                        cx,
                    ))
                    .child(self.field(Field::PersonEmails, "Emails", "One address per line", cx))
                    .child(self.switch(Field::PersonSignal, "Signal", cx))
                    .child(
                        muted("The number above is also their Signal number.", theme).text_xs(),
                    ),
            )
            .child(v().gap(space::TIGHT).child(section_title("On these projects", theme)).child(chips))
            .child(
                muted("The id is the name in slug form. Renaming them carries every project with it.", theme)
                    .text_xs(),
            )
    }

    fn project_inspector(&self, board: &Board, id: &str, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(project) = self.edit.project(id) else {
            return v().child(muted("That project is gone.", theme));
        };
        // Warmest first, like the lane. A dangling id has no card to take a place
        // from, so it is named last, where a fault belongs.
        let mut members: Vec<String> = board
            .people
            .iter()
            .filter(|card| project.people.iter().any(|listed| listed == &card.person))
            .map(|card| card.name.clone())
            .collect();
        members.extend(
            project
                .people
                .iter()
                .filter(|person| self.edit.person(person).is_none())
                .map(|person| format!("{person} (no such person)")),
        );

        v().gap(space::SECTION)
            .child(
                h().justify_between()
                    .items_start()
                    .child(
                        v().gap(space::HAIR)
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .text_lg()
                                    .truncate()
                                    .child(display(&project.id, &project.name)),
                            )
                            .child(section_title(format!("id {}", project.id), theme))
                            .child(section_title(
                                if members.is_empty() {
                                    "nobody on it".to_owned()
                                } else {
                                    members.join(", ")
                                },
                                theme,
                            )),
                    )
                    .child(
                        widgets::button(
                            widgets::eid("delete-project", id),
                            "Delete",
                            Tone::Danger,
                            true,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_selected(cx))),
                    ),
            )
            .child(
                v().gap(space::GROUP)
                    .child(self.field(Field::ProjectName, "Name", "What to call it", cx))
                    .child(self.field(
                        Field::ProjectFolder,
                        "Folder",
                        "~/Projects/…, where work files",
                        cx,
                    ))
                    .child(self.field(
                        Field::ProjectRepo,
                        "Repo",
                        "The nashcode repo, or empty for none",
                        cx,
                    )),
            )
            .child(
                v().gap(space::GROUP)
                    .child(section_title("iMessage", theme))
                    .child(self.field(
                        Field::ProjectChatIds,
                        "Chat ids",
                        "One group chat id per line",
                        cx,
                    ))
                    .child(self.field(
                        Field::ProjectPrompt,
                        "Prompt",
                        "What the router should do with a message",
                        cx,
                    ))
                    .child(
                        h().gap(space::SECTION)
                            .child(self.switch(Field::ProjectEnrich, "Enrich", cx))
                            .child(self.switch(Field::ProjectMediaOnly, "Media only", cx)),
                    ),
            )
            .child(
                v().gap(space::GROUP)
                    .child(section_title("Email", theme))
                    .child(self.field(
                        Field::ProjectAccount,
                        "Account",
                        "The mailbox to search; it never scores",
                        cx,
                    ))
                    .child(self.field(
                        Field::ProjectQuery,
                        "Query",
                        "A Gmail query that replaces the built one",
                        cx,
                    )),
            )
            .child(self.suggestions(id, cx))
    }

    /// Who else writes about this project, and the two things to do about each of
    /// them.
    ///
    /// All four states are drawn: asked and waiting, nobody new, a source that could
    /// not answer, and a list. The section is always here while a project is selected,
    /// because a section that appeared only when it had something would read as a
    /// section that had gone wrong.
    fn suggestions(&self, id: &str, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut section = v()
            .gap(space::GROUP)
            .child(section_title("Suggested", theme))
            .child(
                muted(
                    "From Messages and Gmail. Only this project's name is searched; nothing out of the file is sent.",
                    theme,
                )
                .text_xs(),
            );

        match self.suggestions.get(id) {
            // Nothing has been asked. Only a project reloaded from disk under an old
            // selection lands here, and it is a state with a way out rather than a
            // spinner that never turns.
            None => {
                let project = id.to_owned();
                return section
                    .child(muted("Not looked yet.", theme).text_sm())
                    .child(
                        widgets::button("look", "Look for people", Tone::Default, true, theme)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.look_again(project.clone(), cx);
                            })),
                    );
            }
            Some(Suggested::Looking) => {
                return section.child(muted("Looking…", theme).text_sm());
            }
            Some(Suggested::Failed(why)) => {
                let project = id.to_owned();
                return section
                    .child(div().text_sm().text_color(theme.warning).child(why.clone()))
                    .child(
                        widgets::button("look-again", "Try again", Tone::Ghost, true, theme)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.look_again(project.clone(), cx);
                            })),
                    );
            }
            Some(Suggested::Found(found)) if found.is_empty() => {
                return section.child(muted("Nobody new.", theme).text_sm());
            }
            Some(Suggested::Found(found)) => {
                for candidate in found {
                    section = section.child(self.candidate_row(id, candidate, cx));
                }
            }
        }
        section
    }

    /// One candidate: who, where to write to them, where they were seen, and the two
    /// commands. Accept is the primary of this row and it does not save.
    fn candidate_row(
        &self,
        project: &str,
        candidate: &people_core::Candidate,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        // The address, not a row number: the list shortens under every accept, and a
        // position would name whoever moved up into it.
        let address = candidate.address().to_owned();
        let (accept_to, skip_to) = (project.to_owned(), project.to_owned());
        let (accept_at, skip_at) = (address.clone(), address.clone());

        v().gap(space::PART)
            .p(space::TIGHT)
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .child(div().text_sm().truncate().child(candidate.name.clone()))
            .child(section_title(address, theme))
            .child(section_title(candidate.where_seen.clone(), theme))
            .child(
                h().gap(space::TIGHT)
                    .child(
                        widgets::button(
                            widgets::eid("accept", &accept_at),
                            "Accept",
                            Tone::Primary,
                            true,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.accept_suggestion(&accept_to, &accept_at, cx);
                        })),
                    )
                    .child(
                        widgets::button(
                            widgets::eid("skip", &skip_at),
                            "Skip",
                            Tone::Ghost,
                            true,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.skip_suggestion(&skip_to, &skip_at, cx);
                        })),
                    ),
            )
    }

    /// A contact is read-only here: it is a line inside a person, and the person is
    /// where it is edited. The one command is the way there.
    fn contact_inspector(
        &self,
        board: &Board,
        id: &CardId,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let CardId::Contact { owner, value, .. } = id else {
            return v().child(muted("That address is gone.", theme));
        };
        let (owner, value) = (owner.as_deref(), value.as_str());
        // By id, not by address: an address one person wrote down twice is two cards,
        // and the inspector answers about the one that was clicked.
        let card = board.contacts.iter().find(|card| &card.id == id);
        let kind = match card.map(|card| card.kind) {
            Some(ContactKind::Phone) => "phone",
            Some(ContactKind::Email) => "email",
            None => "address",
        };
        let routes: Vec<String> = owner
            .map(|owner| {
                self.edit
                    .projects
                    .iter()
                    .filter(|project| project.people.iter().any(|listed| listed == owner))
                    .map(|project| display(&project.id, &project.name))
                    .collect()
            })
            .unwrap_or_default();

        let mut body = v()
            .gap(space::SECTION)
            .child(
                v().gap(space::HAIR)
                    .child(div().text_lg().child(value.to_owned()))
                    .child(section_title(kind, theme)),
            )
            .children(card.and_then(|card| card.warning.clone()).map(|warning| {
                div().text_sm().text_color(theme.warning).child(warning)
            }));

        match owner {
            None => {
                body = body.child(muted(
                    "One of your own addresses. It is excluded before anything scores, which is why nothing routes by it.",
                    theme,
                ));
            }
            Some(owner) => {
                let who = self
                    .edit
                    .person(owner)
                    .map_or_else(|| owner.to_owned(), |person| display(&person.id, &person.name));
                let target = owner.to_owned();
                body = body
                    .child(
                        v().gap(space::PART)
                            .child(section_title("Belongs to", theme))
                            .child(div().text_sm().child(who))
                            .child(section_title(
                                if routes.is_empty() {
                                    "routes nowhere — they are on no project".to_owned()
                                } else {
                                    format!("routes to {}", routes.join(", "))
                                },
                                theme,
                            )),
                    )
                    .child(
                        widgets::button("go-to-person", "Go to person", Tone::Default, true, theme)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_card(CardId::Person(target.clone()), cx);
                            })),
                    );
            }
        }
        body
    }

    /// One labelled text field, wired to the inspector's focus and caret.
    pub(crate) fn field(
        &self,
        field: Field,
        label: &'static str,
        placeholder: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let focused = self.field == Some(field);
        div()
            .id(widgets::eid("field", field.key()))
            .child(widgets::text_field(
                TextField {
                    label,
                    value: self.text(field).unwrap_or(""),
                    placeholder,
                    focused,
                    caret: self.caret,
                    multiline: field.is_multiline(),
                },
                theme,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.focus_field(field);
                cx.notify();
            }))
    }

    /// One toggle, wired the same way.
    fn switch(&self, field: Field, label: &'static str, cx: &Context<Self>) -> impl IntoElement {
        widgets::toggle(
            widgets::eid("toggle", field.key()),
            label,
            self.toggle_value(field),
            self.field == Some(field),
            cx.theme(),
        )
        .on_click(cx.listener(move |this, _, _, cx| this.flip(field, cx)))
    }
}
