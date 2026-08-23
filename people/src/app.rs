//! The window: one canvas, one file, one selection.
//!
//! The retained identity here is the file at [`PeopleApp::path`], not a screen. The
//! canvas, the wires, and the inspector are three readings of one [`Edit`], so there
//! is one behavior owner and the rest is presentation. Splitting them into entities
//! would mean keeping copies of one document in step, which is the failure the guides
//! call a second hidden source of truth.
//!
//! Everything that is not "what does this frame look like" lives elsewhere: the model
//! and its list edits in [`crate::edit`], the picture in [`crate::board`], the wire
//! arithmetic in [`crate::links`], the disk, the viewer, and every may-I decision in
//! [`crate::store`], the caret in [`crate::widgets`]. What is left here is the shell,
//! the commands, the selection, and the keyboard.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use gpui::{
    Context, FocusHandle, KeyDownEvent, MouseMoveEvent, Render, Task, Window, div, prelude::*,
};
use people_core::PeopleFile;

use crate::board::{Board, CardId, Lane};
use crate::edit::{Edit, display};
use crate::links::{self, CardBounds, Endpoints};
use crate::store::{self, Command, Disk, DiskChange, Stage, Viewer};
use crate::theme::{ThemeExt, space};
use crate::widgets::{self, Tone, body, h, muted, section_title, v};

/// How often the file's modification time is read.
///
/// A poll rather than a filesystem watcher: two seconds is faster than a person can
/// switch windows, one `stat` of one small file costs nothing, and it needs no
/// platform-specific event API.
const POLL: Duration = Duration::from_secs(2);

/// The height of the macOS titlebar the window draws under.
///
/// `main.rs` asks for a transparent titlebar, which is what makes the strip at the
/// top of the window the same colour as the window; the cost is that the traffic
/// lights are drawn over the content. The toolbar keeps that strip clear. A raw pixel
/// because it is a platform boundary, fixed by AppKit, not product spacing that the
/// base font should scale.
pub(crate) const TITLEBAR: gpui::Pixels = gpui::px(28.);

/// A field, named by what it edits rather than by where it sits, so the name survives
/// every layout change and never depends on a list position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    ProjectName,
    ProjectFolder,
    ProjectRepo,
    ProjectChatIds,
    ProjectPrompt,
    ProjectEnrich,
    ProjectMediaOnly,
    ProjectAccount,
    ProjectQuery,
    PersonName,
    PersonPhones,
    PersonEmails,
}

impl Field {
    /// Tab order for the project inspector, top to bottom as it is drawn.
    pub(crate) const PROJECT: [Field; 9] = [
        Field::ProjectName,
        Field::ProjectFolder,
        Field::ProjectRepo,
        Field::ProjectChatIds,
        Field::ProjectPrompt,
        Field::ProjectEnrich,
        Field::ProjectMediaOnly,
        Field::ProjectAccount,
        Field::ProjectQuery,
    ];
    pub(crate) const PERSON: [Field; 3] =
        [Field::PersonName, Field::PersonPhones, Field::PersonEmails];

    /// A stable name for the field, for its element id. Domain words, so it does not
    /// change when the inspector is reordered.
    pub fn key(self) -> &'static str {
        match self {
            Field::ProjectName => "project.name",
            Field::ProjectFolder => "project.folder",
            Field::ProjectRepo => "project.repo",
            Field::ProjectChatIds => "project.chat_ids",
            Field::ProjectPrompt => "project.imsg.prompt",
            Field::ProjectEnrich => "project.imsg.enrich",
            Field::ProjectMediaOnly => "project.imsg.media_only",
            Field::ProjectAccount => "project.email.account",
            Field::ProjectQuery => "project.email.query",
            Field::PersonName => "person.name",
            Field::PersonPhones => "person.phones",
            Field::PersonEmails => "person.emails",
        }
    }

    /// A switch, not a text field: Space and Enter flip it, and it holds no caret.
    pub fn is_toggle(self) -> bool {
        matches!(self, Field::ProjectEnrich | Field::ProjectMediaOnly)
    }

    /// One entry per line, and Enter adds a line.
    pub fn is_multiline(self) -> bool {
        matches!(
            self,
            Field::ProjectChatIds | Field::ProjectPrompt | Field::PersonPhones | Field::PersonEmails
        )
    }

    /// Editing this field re-derives the card's id from the new name.
    fn renames(self) -> bool {
        matches!(self, Field::PersonName | Field::ProjectName)
    }
}

/// The one line in the status bar that says what just happened.
pub struct Notice {
    pub text: String,
    /// A refusal or a failure, rather than a result.
    pub bad: bool,
}

pub struct PeopleApp {
    focus: FocusHandle,
    pub path: PathBuf,
    pub stage: Stage,
    /// Why the file will not parse, when `stage` is [`Stage::Broken`].
    pub error: Option<String>,
    pub edit: Edit,
    /// The file as it is on disk. `edit != saved` is the whole of "unsaved".
    saved: Edit,
    disk_mtime: Option<SystemTime>,
    /// The file moved on disk while there were unsaved edits, and nobody has chosen
    /// yet. Nothing is discarded until they do.
    pub disk_changed: bool,

    /// The card the canvas is answering about.
    pub selected: Option<CardId>,
    /// Which lane the keyboard is in.
    pub lane: Lane,
    /// The inspector field the keyboard is in, when it is in one.
    pub field: Option<Field>,
    /// A byte offset into the focused field's text.
    pub caret: usize,
    /// The wire under the pointer, as an index into this frame's `Board::links`.
    pub hover_link: Option<usize>,
    /// Where every card was on the last frame. Written in prepaint, read in paint.
    pub card_bounds: CardBounds,
    /// The wires the last frame drew, so the pointer can be tested against them.
    pub link_geometry: Rc<RefCell<Vec<(usize, Endpoints)>>>,

    /// A command is in flight, and what to call it.
    busy: Option<&'static str>,
    pub notice: Option<Notice>,
    last_saved: Option<DateTime<Local>>,
    pushed_at: Option<String>,
    pub viewer: Result<Viewer, String>,
    _watch: Task<()>,
}

impl PeopleApp {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let watch_path = path.clone();
        let viewer = store::viewer();
        let asked = viewer.as_ref().ok().cloned();

        let watch = cx.spawn(async move |this, cx| {
            // The first read, off the main thread, so the window can draw its loading
            // state instead of opening on a frozen frame.
            let first = {
                let path = watch_path.clone();
                cx.background_executor()
                    .spawn(async move { (store::read(&path), store::mtime(&path)) })
                    .await
            };
            if this
                .update(cx, |this: &mut PeopleApp, cx| {
                    this.apply(first.0, first.1);
                    cx.notify();
                })
                .is_err()
            {
                return;
            }

            // What the viewer already holds. It is a status line, not a blocker, so a
            // viewer that is not there costs one failed request and nothing else.
            if let Some(viewer) = asked {
                let stamp =
                    cx.background_executor().spawn(async move { store::pushed_at(&viewer) }).await;
                let _ = this.update(cx, |this: &mut PeopleApp, cx| {
                    if let Ok(Some(stamp)) = stamp {
                        this.pushed_at = Some(stamp);
                        cx.notify();
                    }
                });
            }

            loop {
                cx.background_executor().timer(POLL).await;
                let path = watch_path.clone();
                let found =
                    cx.background_executor().spawn(async move { store::mtime(&path) }).await;
                let path = watch_path.clone();
                // Err means the window is gone, and so is the reason to keep polling.
                let Ok(reread) = this.update(cx, |this: &mut PeopleApp, cx| {
                    match store::disk_change(this.disk_mtime, found, this.unsaved()) {
                        DiskChange::Nothing => false,
                        DiskChange::Ask => {
                            let already = this.disk_changed;
                            this.disk_changed = true;
                            if !already {
                                cx.notify();
                            }
                            false
                        }
                        DiskChange::Reload => true,
                    }
                }) else {
                    return;
                };
                if !reread {
                    continue;
                }
                let fresh = cx
                    .background_executor()
                    .spawn(async move { (store::read(&path), store::mtime(&path)) })
                    .await;
                let _ = this.update(cx, |this: &mut PeopleApp, cx| {
                    // The decision is taken again, on the facts of the frame the read
                    // landed in. Keys typed while it was in flight are unsaved edits
                    // now, and the first decision was made when there were none:
                    // applying the read here would discard work nobody agreed to lose.
                    match store::disk_change(this.disk_mtime, fresh.1, this.unsaved()) {
                        DiskChange::Reload => {
                            this.apply(fresh.0, fresh.1);
                            cx.notify();
                        }
                        DiskChange::Ask => {
                            this.disk_changed = true;
                            cx.notify();
                        }
                        // The file went back to where the window left it — a save of
                        // our own, most often — so this read is stale and says nothing.
                        DiskChange::Nothing => {}
                    }
                });
            }
        });

        Self {
            focus: cx.focus_handle(),
            path,
            stage: Stage::Loading,
            error: None,
            edit: Edit::default(),
            saved: Edit::default(),
            disk_mtime: None,
            disk_changed: false,
            selected: None,
            lane: Lane::People,
            field: None,
            caret: 0,
            hover_link: None,
            card_bounds: Rc::new(RefCell::new(HashMap::new())),
            link_geometry: Rc::new(RefCell::new(Vec::new())),
            busy: None,
            notice: None,
            last_saved: None,
            pushed_at: None,
            viewer,
            _watch: watch,
        }
    }

    /// Take the keyboard. Called once, from the bootstrap: the window owns ⌘S from its
    /// first frame, and `render` never asks for focus on its own.
    pub fn take_focus(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.focus(&self.focus);
    }

    /// The picture, derived from the edits rather than stored beside them. A stored
    /// board would go stale between keystrokes, and the file is small enough that
    /// deriving it costs less than keeping it right.
    pub fn board(&self) -> Board {
        if self.stage == Stage::Editing { Board::from_file(&self.edit.to_file()) } else { Board::default() }
    }

    /// There are edits the file on disk does not have.
    pub fn unsaved(&self) -> bool {
        self.stage == Stage::Editing && self.edit != self.saved
    }

    /// Take a read of the file. Nothing here asks whether it may: the two callers —
    /// the first load and a reload the operator agreed to — have already decided.
    fn apply(&mut self, disk: Disk, mtime: Option<SystemTime>) {
        match disk {
            Disk::Missing => {
                self.stage = Stage::Missing;
                self.error = None;
                self.edit = Edit::default();
            }
            Disk::Broken(why) => {
                self.stage = Stage::Broken;
                self.error = Some(why);
            }
            Disk::Loaded(file) => {
                self.stage = Stage::Editing;
                self.error = None;
                self.edit = Edit::from_file(&file);
            }
        }
        self.saved = self.edit.clone();
        self.disk_mtime = mtime;
        self.disk_changed = false;
        self.field = None;
        self.caret = 0;
        self.hover_link = None;
        // A selection is a card id, so it survives a reload that left the card there.
        let board = self.board();
        self.selected = self.selected.take().filter(|id| board.holds(id));
    }

    pub fn save_command(&self) -> Command {
        store::save_command(self.stage, self.unsaved(), self.busy.is_some())
    }

    pub fn push_command(&self) -> Command {
        store::push_command(
            self.stage,
            self.unsaved(),
            self.busy.is_some(),
            self.viewer.as_ref().map_err(String::as_str),
        )
    }

    fn say(&mut self, text: impl Into<String>, bad: bool) {
        self.notice = Some(Notice { text: text.into(), bad });
    }

    // -- commands -----------------------------------------------------------

    /// Write the file. Fatal findings stop it; warnings do not.
    ///
    /// One method, called by the button and by ⌘S alike, so the two cannot disagree
    /// about when a save is allowed or what it does.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        let command = self.save_command();
        if !command.enabled {
            if let Some(why) = command.why {
                self.say(why, true);
            }
            cx.notify();
            return;
        }

        let file = self.edit.to_file();
        let refused = store::refusals(&file);
        if !refused.is_empty() {
            self.say(format!("Not saved. {}", refused.join(" ")), true);
            cx.notify();
            return;
        }

        match file.save(&self.path) {
            Ok(()) => {
                self.saved = self.edit.clone();
                self.disk_mtime = store::mtime(&self.path);
                self.disk_changed = false;
                self.last_saved = Some(Local::now());
                self.notice = None;
            }
            Err(why) => self.say(why, true),
        }
        cx.notify();
    }

    /// Send the file on disk to the viewer.
    pub fn push(&mut self, cx: &mut Context<Self>) {
        let command = self.push_command();
        if !command.enabled {
            if let Some(why) = command.why {
                self.say(why, true);
            }
            cx.notify();
            return;
        }
        let Ok(viewer) = self.viewer.clone() else {
            return;
        };

        let file = self.saved.to_file();
        self.busy = Some("Pushing…");
        self.notice = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor().spawn(async move { store::push(&viewer, &file) }).await;
            // Err means the window closed while the request was out; nothing to tell.
            let _ = this.update(cx, |this: &mut PeopleApp, cx| {
                this.busy = None;
                match result {
                    Ok(reply) => {
                        this.pushed_at = Some(reply.pushed_at.clone());
                        this.say(
                            format!("Pushed {} people and {} projects", reply.people, reply.projects),
                            false,
                        );
                    }
                    Err(why) => this.say(why, true),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Write an empty file, so the path exists and the window can edit it.
    pub fn create_file(&mut self, cx: &mut Context<Self>) {
        match PeopleFile::default().save(&self.path) {
            Ok(()) => {
                self.apply(store::read(&self.path), store::mtime(&self.path));
                self.notice = None;
            }
            Err(why) => self.say(why, true),
        }
        cx.notify();
    }

    /// Take the file that arrived on disk, losing the edits in the window.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.apply(store::read(&self.path), store::mtime(&self.path));
        self.say("Reloaded from disk", false);
        cx.notify();
    }

    /// Keep the edits in the window and stop asking. The next save writes over
    /// whatever arrived, which is what "keep" means.
    pub fn keep_edits(&mut self, cx: &mut Context<Self>) {
        self.disk_mtime = store::mtime(&self.path);
        self.disk_changed = false;
        cx.notify();
    }

    pub fn open_repo(&mut self, repo: &str) {
        let Ok(viewer) = &self.viewer else {
            return;
        };
        let url = format!("{}/{repo}", viewer.base);
        // The browser owns a URL; a failure to launch it is not something this window
        // can fix, and it is not worth a modal.
        let _ = std::process::Command::new("open").arg(url).spawn();
    }

    // -- the selection ------------------------------------------------------

    pub fn select_card(&mut self, id: CardId, cx: &mut Context<Self>) {
        self.lane = Board::lane_of(&id);
        self.selected = Some(id);
        self.field = None;
        cx.notify();
    }

    pub fn new_person(&mut self, cx: &mut Context<Self>) {
        let id = self.edit.add_person();
        self.selected = Some(CardId::Person(id));
        self.lane = Lane::People;
        self.focus_field(Field::PersonName);
        // The name is the id, so it opens selected: the first keystroke replaces it
        // rather than appending to a placeholder nobody wants.
        self.caret = 0;
        cx.notify();
    }

    pub fn new_project(&mut self, cx: &mut Context<Self>) {
        let id = self.edit.add_project();
        self.selected = Some(CardId::Project(id));
        self.lane = Lane::Projects;
        self.focus_field(Field::ProjectName);
        self.caret = 0;
        cx.notify();
    }

    /// Delete whatever is selected. A person a project still lists is refused, and the
    /// refusal names the projects, because taking them off there is the fix.
    ///
    /// A card that goes quietly is a card the operator has to go looking for, so the
    /// status line names what left. The name is read before the delete, because
    /// afterwards there is nobody to ask.
    pub fn delete_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected.clone() {
            Some(CardId::Person(id)) => {
                let who = self
                    .edit
                    .person(&id)
                    .map_or_else(|| id.clone(), |person| display(&person.id, &person.name));
                match self.edit.delete_person(&id) {
                    Ok(()) => {
                        self.forget_card();
                        self.say(format!("Deleted {who}"), false);
                    }
                    Err(why) => self.say(why, true),
                }
            }
            Some(CardId::Project(id)) => {
                let what = self
                    .edit
                    .project(&id)
                    .map_or_else(|| id.clone(), |project| display(&project.id, &project.name));
                self.edit.delete_project(&id);
                self.forget_card();
                self.say(format!("Deleted {what}"), false);
            }
            _ => {}
        }
        cx.notify();
    }

    /// Everything that pointed at the card that has just gone.
    fn forget_card(&mut self) {
        self.selected = None;
        self.field = None;
        // The hover is an index into the link list of the frame that drew it, and that
        // list is one card shorter now.
        self.hover_link = None;
    }

    /// The pointer left the canvas, so it is on no wire. A delete and a rename clear
    /// the same field on their own way past, because they are notifying anyway.
    pub(crate) fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.hover_link.take().is_some() {
            cx.notify();
        }
    }

    /// Put a person on a project, or take them off it.
    pub fn set_membership(
        &mut self,
        project: &str,
        person: &str,
        on: bool,
        cx: &mut Context<Self>,
    ) {
        if on {
            self.edit.add_to_project(project, person);
        } else {
            self.edit.remove_from_project(project, person);
        }
        cx.notify();
    }

    // -- inspector fields ---------------------------------------------------

    /// The tab ring for the selected card, or nothing when a card has no fields.
    fn ring(&self) -> &'static [Field] {
        match &self.selected {
            Some(CardId::Person(_)) => &Field::PERSON,
            Some(CardId::Project(_)) => &Field::PROJECT,
            _ => &[],
        }
    }

    fn person_id(&self) -> Option<&str> {
        match &self.selected {
            Some(CardId::Person(id)) => Some(id),
            _ => None,
        }
    }

    fn project_id(&self) -> Option<&str> {
        match &self.selected {
            Some(CardId::Project(id)) => Some(id),
            _ => None,
        }
    }

    pub fn focus_field(&mut self, field: Field) {
        self.field = Some(field);
        self.caret = self.text(field).map_or(0, str::len);
    }

    /// The field's text, for drawing.
    pub fn text(&self, field: Field) -> Option<&str> {
        let project = self.project_id().and_then(|id| self.edit.project(id));
        let person = self.person_id().and_then(|id| self.edit.person(id));
        Some(match field {
            Field::ProjectName => project?.name.as_str(),
            Field::ProjectFolder => project?.folder.as_str(),
            Field::ProjectRepo => project?.repo.as_str(),
            Field::ProjectChatIds => project?.chat_ids.as_str(),
            Field::ProjectPrompt => project?.prompt.as_str(),
            Field::ProjectAccount => project?.account.as_str(),
            Field::ProjectQuery => project?.query.as_str(),
            Field::PersonName => person?.name.as_str(),
            Field::PersonPhones => person?.phones.as_str(),
            Field::PersonEmails => person?.emails.as_str(),
            Field::ProjectEnrich | Field::ProjectMediaOnly => return None,
        })
    }

    fn text_mut(&mut self, field: Field) -> Option<&mut String> {
        let project_id = self.project_id().map(str::to_owned);
        let person_id = self.person_id().map(str::to_owned);
        match field {
            Field::ProjectName
            | Field::ProjectFolder
            | Field::ProjectRepo
            | Field::ProjectChatIds
            | Field::ProjectPrompt
            | Field::ProjectAccount
            | Field::ProjectQuery => {
                let project = self.edit.project_mut(&project_id?)?;
                Some(match field {
                    Field::ProjectName => &mut project.name,
                    Field::ProjectFolder => &mut project.folder,
                    Field::ProjectRepo => &mut project.repo,
                    Field::ProjectChatIds => &mut project.chat_ids,
                    Field::ProjectPrompt => &mut project.prompt,
                    Field::ProjectAccount => &mut project.account,
                    _ => &mut project.query,
                })
            }
            Field::PersonName | Field::PersonPhones | Field::PersonEmails => {
                let person = self.edit.person_mut(&person_id?)?;
                Some(match field {
                    Field::PersonName => &mut person.name,
                    Field::PersonPhones => &mut person.phones,
                    _ => &mut person.emails,
                })
            }
            Field::ProjectEnrich | Field::ProjectMediaOnly => None,
        }
    }

    /// The id is the name in slug form, so a name that changed moves its card. The
    /// selection has to move with it, or the next keystroke edits nothing.
    fn resync_id(&mut self) {
        let moved = match self.selected.clone() {
            Some(CardId::Person(id)) => {
                let fresh = self.edit.reslug_person(&id);
                if fresh != id {
                    self.selected = Some(CardId::Person(fresh));
                    true
                } else {
                    false
                }
            }
            Some(CardId::Project(id)) => {
                let fresh = self.edit.reslug_project(&id);
                if fresh != id {
                    self.selected = Some(CardId::Project(fresh));
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if moved {
            // A renamed card is a new card id, so every wire that touched it is a new
            // link. The hovered index would name one of somebody else's.
            self.hover_link = None;
        }
    }

    pub fn toggle_value(&self, field: Field) -> bool {
        let Some(project) = self.project_id().and_then(|id| self.edit.project(id)) else {
            return false;
        };
        match field {
            Field::ProjectEnrich => project.enrich,
            Field::ProjectMediaOnly => project.media_only,
            _ => false,
        }
    }

    pub fn flip(&mut self, field: Field, cx: &mut Context<Self>) {
        let Some(id) = self.project_id().map(str::to_owned) else {
            return;
        };
        if let Some(project) = self.edit.project_mut(&id) {
            match field {
                Field::ProjectEnrich => project.enrich = !project.enrich,
                Field::ProjectMediaOnly => project.media_only = !project.media_only,
                _ => return,
            }
        }
        self.field = Some(field);
        cx.notify();
    }

    // -- keyboard -----------------------------------------------------------

    /// Move one step around the inspector's tab ring.
    fn step_field(&mut self, back: bool) {
        let ring = self.ring();
        if ring.is_empty() {
            return;
        }
        let next = match self.field.and_then(|field| ring.iter().position(|f| *f == field)) {
            None if back => ring.len() - 1,
            None => 0,
            Some(at) if back => (at + ring.len() - 1) % ring.len(),
            Some(at) => (at + 1) % ring.len(),
        };
        self.focus_field(ring[next]);
    }

    /// Move to another lane and land on a card in it.
    ///
    /// It lands on a card the current selection reaches when there is one, so Tab
    /// walks the wire rather than jumping to the top of the next column.
    fn enter_lane(&mut self, lane: Lane) {
        let board = self.board();
        let cards = board.lane(lane);
        self.lane = lane;
        self.field = None;
        if cards.is_empty() {
            self.selected = None;
            return;
        }
        let reached = self.selected.as_ref().map(|id| board.connected(id));
        let landing = reached
            .and_then(|set| cards.iter().find(|id| set.contains(id)).cloned())
            .unwrap_or_else(|| cards[0].clone());
        self.selected = Some(landing);
    }

    /// Move the selection up or down the lane the keyboard is in.
    fn step_card(&mut self, back: bool) {
        let board = self.board();
        let cards = board.lane(self.lane);
        if cards.is_empty() {
            return;
        }
        let at = self.selected.as_ref().and_then(|id| cards.iter().position(|card| card == id));
        let next = match at {
            None if back => cards.len() - 1,
            None => 0,
            Some(at) if back => at.saturating_sub(1),
            Some(at) => (at + 1).min(cards.len() - 1),
        };
        self.selected = Some(cards[next].clone());
    }

    /// One step left or right through the lanes.
    fn step_lane(&mut self, back: bool) {
        let at = Lane::ALL.iter().position(|lane| *lane == self.lane).unwrap_or(0);
        let next = if back {
            (at + Lane::ALL.len() - 1) % Lane::ALL.len()
        } else {
            (at + 1) % Lane::ALL.len()
        };
        self.enter_lane(Lane::ALL[next]);
    }

    /// One handler for the window.
    ///
    /// GPUI Actions would be the ecosystem's way to bind these; this app dispatches
    /// from the focused root instead, exactly as the reference application on this
    /// crate version does. What the Action rule protects is preserved by hand: every
    /// command below calls the same method its button calls, so the two entry points
    /// cannot drift.
    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;

        if modifiers.platform {
            match key {
                "s" => self.save(cx),
                "p" if modifiers.shift => self.push(cx),
                "n" if modifiers.shift => self.new_project(cx),
                "n" => self.new_person(cx),
                _ => {}
            }
            return;
        }

        match key {
            // Escape leaves the innermost thing first: the field, then the selection.
            "escape" => {
                if self.field.take().is_none() {
                    self.selected = None;
                }
                self.notice = None;
            }
            "tab" if self.field.is_some() => self.step_field(modifiers.shift),
            "tab" => self.step_lane(modifiers.shift),
            _ => match self.field {
                Some(field) => self.field_key(field, key, event, cx),
                None => match key {
                    "up" => self.step_card(true),
                    "down" => self.step_card(false),
                    "left" => self.step_lane(true),
                    "right" => self.step_lane(false),
                    "enter" => self.step_field(false),
                    _ => return,
                },
            },
        }
        cx.notify();
    }

    /// A key inside a focused inspector field.
    fn field_key(&mut self, field: Field, key: &str, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if field.is_toggle() {
            match key {
                "space" | "enter" => self.flip(field, cx),
                "up" => self.step_field(true),
                "down" => self.step_field(false),
                _ => {}
            }
            return;
        }

        let caret = self.caret;
        let multiline = field.is_multiline();
        let typed = event.keystroke.key_char.clone();
        let Some(text) = self.text_mut(field) else {
            return;
        };

        self.caret = match key {
            "left" => widgets::left(text, caret),
            "right" => widgets::right(text, caret),
            "home" => widgets::line_start(text, caret),
            "end" => widgets::line_end(text, caret),
            "up" if multiline => widgets::up(text, caret),
            "down" if multiline => widgets::down(text, caret),
            "backspace" => widgets::backspace(text, caret),
            "delete" => widgets::delete(text, caret),
            "enter" if multiline => widgets::insert(text, caret, "\n"),
            // A single-line field takes Enter as "done with this one".
            "enter" | "up" | "down" => {
                self.step_field(key == "up");
                return;
            }
            _ => match typed.as_deref() {
                Some(character)
                    if !character.is_empty() && !character.chars().any(char::is_control) =>
                {
                    widgets::insert(text, caret, character)
                }
                _ => return,
            },
        };
        if field.renames() {
            self.resync_id();
        }
    }

    /// The pointer moved over the canvas. Only a change of wire is worth a frame.
    fn on_pointer(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let wires = self.link_geometry.borrow().clone();
        let found = links::nearest(&wires, event.position, Self::wire_reach());
        if found != self.hover_link {
            self.hover_link = found;
            cx.notify();
        }
    }

    /// The canvas gives the pointer to the links layer.
    pub(crate) fn pointer_listener(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&MouseMoveEvent, &mut Window, &mut gpui::App) + 'static {
        cx.listener(Self::on_pointer)
    }

    // -- the shell ----------------------------------------------------------

    fn render_toolbar(&self, board: &Board, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let save = self.save_command();
        let push = self.push_command();

        let mut actions = h().gap(space::TIGHT);
        let save_button = widgets::button("save", "Save", Tone::Primary, save.enabled, theme);
        actions = actions.child(if save.enabled {
            save_button.on_click(cx.listener(|this, _, _, cx| this.save(cx)))
        } else {
            save_button
        });
        let push_button =
            widgets::button("push", self.busy.unwrap_or("Push"), Tone::Default, push.enabled, theme);
        actions = actions.child(if push.enabled {
            push_button.on_click(cx.listener(|this, _, _, cx| this.push(cx)))
        } else {
            push_button
        });

        h().justify_between()
            .px(space::SECTION)
            .py(space::GROUP)
            // The traffic lights float over the top-left of the window, so the first
            // line of the toolbar starts below them.
            .pt(TITLEBAR)
            .border_b_1()
            .border_color(theme.border)
            .child(section_title(
                format!(
                    "{} contacts · {} people · {} projects",
                    board.contacts.len(),
                    board.people.len(),
                    board.projects.len()
                ),
                theme,
            ))
            .child(actions)
    }

    /// The banner that appears only while a choice is owed.
    fn render_disk_banner(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h().gap(space::GROUP)
            .mx(space::SECTION)
            .mt(space::GROUP)
            .px(space::GROUP)
            .py(space::TIGHT)
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.warning)
            // The sentence takes the surplus; the two choices keep the trailing edge.
            .child(div().flex_1().min_w_0().text_sm().text_color(theme.warning).child(
                "The file changed on disk while you were editing. Reloading discards your edits.",
            ))
            .child(
                widgets::button("reload", "Reload", Tone::Default, true, theme)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .child(
                widgets::button("keep", "Keep my edits", Tone::Ghost, true, theme)
                    .on_click(cx.listener(|this, _, _, cx| this.keep_edits(cx))),
            )
    }

    fn render_status(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut line = h()
            .gap(space::GROUP)
            .px(space::SECTION)
            .py(space::TIGHT)
            .border_t_1()
            .border_color(theme.border)
            .child(muted(self.path.display().to_string(), theme).text_xs());

        if self.unsaved() {
            line = line.child(div().text_xs().text_color(theme.warning).child("unsaved"));
        }
        if let Some(at) = self.last_saved {
            line = line.child(section_title(format!("saved {}", store::short_time(at)), theme));
        }
        line = line.child(section_title(
            match &self.pushed_at {
                Some(stamp) => format!("pushed {}", store::short_stamp(stamp)),
                None => "never pushed".to_owned(),
            },
            theme,
        ));

        // The count only, and only of the findings the word covers: a fatal one is not
        // a warning, it is a refused save, and it is said in full where the save is
        // asked for. Each warning proper is written on the card it is about, which is
        // where it can be acted on.
        let warnings = if self.stage == Stage::Editing {
            self.edit.to_file().validate().iter().filter(|finding| !finding.fatal).count()
        } else {
            0
        };
        if warnings > 0 {
            line = line.child(div().text_xs().text_color(theme.warning).child(match warnings {
                1 => "1 warning".to_owned(),
                n => format!("{n} warnings"),
            }));
        }

        // A viewer that could not be resolved states its reason here rather than
        // behind a button that cannot be pressed.
        line = match &self.viewer {
            Ok(viewer) => line.child(section_title(format!("viewer: {}", viewer.source), theme)),
            Err(why) => line
                .child(div().text_xs().text_color(theme.warning).truncate().child(why.clone())),
        };

        if let Some(notice) = &self.notice {
            line = line.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(if notice.bad { theme.danger } else { theme.muted })
                    .truncate()
                    .child(notice.text.clone()),
            );
        }
        line
    }

    /// The four states in which there is no canvas to draw.
    fn render_placeholder(&self, board: &Board, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let theme = cx.theme();
        let frame = || v().flex_1().items_center().justify_center().gap(space::GROUP).p(space::REGION);
        match self.stage {
            Stage::Loading => {
                Some(frame().child(muted("Reading the file…", theme)).into_any_element())
            }
            Stage::Missing => Some(
                frame()
                    .child(body("There is no people file yet.", theme))
                    .child(muted(self.path.display().to_string(), theme).text_xs())
                    .child(
                        widgets::button("create", "Create empty file", Tone::Primary, true, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.create_file(cx))),
                    )
                    .into_any_element(),
            ),
            Stage::Broken => Some(
                frame()
                    .items_start()
                    .justify_start()
                    .child(div().text_sm().text_color(theme.danger).child(
                        "The file will not parse, so nothing here is editable — an editor that guessed would save the guess over your file.",
                    ))
                    .child(muted(self.path.display().to_string(), theme).text_xs())
                    .child(
                        div()
                            .w_full()
                            .p(space::GROUP)
                            .rounded(theme.radius)
                            .bg(theme.surface)
                            .border_1()
                            .border_color(theme.border)
                            .text_sm()
                            .text_color(theme.foreground)
                            .children(
                                self.error
                                    .iter()
                                    .flat_map(|why| why.lines())
                                    .map(|line| div().child(line.to_owned())),
                            ),
                    )
                    .into_any_element(),
            ),
            Stage::Editing if board.is_empty() => Some(
                frame()
                    .child(body("Nobody and nothing yet.", theme))
                    .child(muted("Add a person and a project, then join them.", theme))
                    .child(
                        h().gap(space::TIGHT)
                            .child(
                                widgets::button("first-person", "New person", Tone::Primary, true, theme)
                                    .on_click(cx.listener(|this, _, _, cx| this.new_person(cx))),
                            )
                            .child(
                                widgets::button("first-project", "New project", Tone::Default, true, theme)
                                    .on_click(cx.listener(|this, _, _, cx| this.new_project(cx))),
                            ),
                    )
                    .into_any_element(),
            ),
            Stage::Editing => None,
        }
    }
}

impl Render for PeopleApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Last frame's geometry, cleared before this frame's prepaint refills it, so a
        // card that was deleted or renamed leaves no ghost behind for a wire to hang
        // from.
        self.card_bounds.borrow_mut().clear();

        let theme = cx.theme();
        let board = self.board();
        let mut root = v()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_toolbar(&board, cx));

        if self.disk_changed {
            root = root.child(self.render_disk_banner(cx));
        }

        let body = match self.render_placeholder(&board, cx) {
            Some(placeholder) => placeholder,
            None => h()
                .items_start()
                .flex_1()
                .min_h_0()
                .gap(space::SECTION)
                .child(self.render_canvas(&board, cx))
                .child(self.render_inspector(&board, cx))
                .into_any_element(),
        };

        // The body is the one region allowed to shrink, so a tall canvas scrolls
        // inside it instead of pushing the status line off the window.
        root.child(v().flex_1().min_h_0().p(space::SECTION).child(body))
            .child(self.render_status(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keyboard contract for the inspector: Tab reaches every field, once.
    ///
    /// A field added to a form but forgotten in its ring would be editable by mouse
    /// and unreachable by keyboard, which is the one accessibility rule this window
    /// cannot afford to break silently.
    #[test]
    fn every_field_is_on_exactly_one_tab_ring() {
        const ALL: [Field; 12] = [
            Field::ProjectName,
            Field::ProjectFolder,
            Field::ProjectRepo,
            Field::ProjectChatIds,
            Field::ProjectPrompt,
            Field::ProjectEnrich,
            Field::ProjectMediaOnly,
            Field::ProjectAccount,
            Field::ProjectQuery,
            Field::PersonName,
            Field::PersonPhones,
            Field::PersonEmails,
        ];
        for field in ALL {
            let times =
                Field::PROJECT.iter().chain(Field::PERSON.iter()).filter(|f| **f == field).count();
            assert_eq!(times, 1, "{field:?} is on {times} rings, not one");
        }
        assert_eq!(Field::PROJECT.len() + Field::PERSON.len(), ALL.len());
    }

    #[test]
    fn a_toggle_is_never_a_text_field_and_every_field_has_its_own_id() {
        let mut keys: Vec<&str> =
            Field::PROJECT.iter().chain(Field::PERSON.iter()).map(|f| f.key()).collect();
        keys.sort_unstable();
        let total = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), total, "two fields share an element id");

        for field in Field::PROJECT.iter().chain(Field::PERSON.iter()) {
            assert!(!(field.is_toggle() && field.is_multiline()), "{field:?}");
        }
    }

    #[test]
    fn only_the_two_name_fields_move_a_card() {
        // Editing a name re-slugs the id and moves the selection with it. Any other
        // field doing that would move the selection while somebody typed a phone.
        for field in Field::PROJECT.iter().chain(Field::PERSON.iter()) {
            assert_eq!(
                field.renames(),
                matches!(field, Field::PersonName | Field::ProjectName),
                "{field:?}"
            );
        }
    }
}
