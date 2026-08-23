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
use people_core::{Candidate, PeopleFile};

use crate::board::{self, Board, CardId, Lane};
use crate::edit::{self, Edit, display};
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

/// How long a project has to stay selected before its inboxes are read.
///
/// A lookup shells out to `imsg` and to Gmail, so arrowing down a lane of thirty-five
/// projects must not fire thirty-five of them. A third of a second is below the
/// threshold at which a person waiting on an answer notices a wait, and well above the
/// speed at which the same person walks past a card on the way to another.
const SETTLE: Duration = Duration::from_millis(350);

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
    PersonSignal,
    /// The directory `Sync folders…` reads. It belongs to the toolbar's panel, not to
    /// a card, which is why it is on neither tab ring.
    ClientsDir,
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
    pub(crate) const PERSON: [Field; 4] =
        [Field::PersonName, Field::PersonPhones, Field::PersonEmails, Field::PersonSignal];

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
            Field::PersonSignal => "person.signal",
            Field::ClientsDir => "sync.dir",
        }
    }

    /// A switch, not a text field: Space and Enter flip it, and it holds no caret.
    pub fn is_toggle(self) -> bool {
        matches!(self, Field::ProjectEnrich | Field::ProjectMediaOnly | Field::PersonSignal)
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

/// What the two suggestion sources have said about one project.
///
/// Kept per project id for the life of the window. A lookup shells out to `imsg` and
/// `gws`, which is seconds, not milliseconds; re-running it every time the operator
/// clicked back onto a card would make the inspector feel broken and would ask Gmail
/// the same question ten times.
#[derive(Debug, Clone)]
pub enum Suggested {
    /// The sources are being asked. Drawn, not skipped.
    Looking,
    /// What they answered, minus everybody accepted or skipped since.
    Found(Vec<Candidate>),
    /// Neither source could answer, in one line.
    Failed(String),
}

/// What a renamed project keeps of what it had been told.
///
/// An answer is about the project, and the project is the same project under a new
/// name, so `Found` and `Failed` travel with it.
///
/// `Looking` is not an answer. It is a question the **old** id asked, and the task
/// that asked it checks the selection when it lands: the selection is the new id by
/// then, so the task drops its own work and returns. Carried across, that leaves
/// `Looking…` on the new id with nothing coming, no Look button — `Looking` draws no
/// command — and `look_for_suggestions` refusing to re-ask because the key is already
/// in the map. So a rename hands a question back as no question at all, which is the
/// state that has a Look button in it.
pub fn carry_suggestion(had: Option<Suggested>) -> Option<Suggested> {
    match had {
        Some(Suggested::Looking) | None => None,
        answered => answered,
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

    /// What the sources have said about each project, by project id. Session only:
    /// suggestions are a reading of two inboxes, not a thing the file remembers.
    pub suggestions: HashMap<String, Suggested>,
    /// The folder `Sync folders…` reads, and whether its panel is open. A text field
    /// and not a native open-dialog in v1: the answer is one path the operator already
    /// knows, and a file picker is a platform surface with its own focus contract.
    pub sync_panel: bool,
    pub clients_dir: String,
    /// `clients_dir` still holds what `NASHCODE_CLIENTS` said, decided when the panel
    /// opened. A bool and not a second read of the environment: see
    /// [`PeopleApp::open_sync_panel`].
    pub clients_dir_from_env: bool,

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
            suggestions: HashMap::new(),
            sync_panel: false,
            clients_dir: String::new(),
            clients_dir_from_env: false,
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
        // The answers were about the projects in the file that has just been replaced.
        // A project that kept its id through the reload is not the same question — its
        // name, its query and its people may all have moved under it — and one that
        // did not keep its id would sit in this map forever.
        self.suggestions.clear();
        // A fresh file is a fresh sweep, so Gmail's hundred-message budget starts
        // again. The budget exists to stop one `suggest` run from making eight hundred
        // round trips; a window that is open all day would otherwise spend it once and
        // then return nothing for the rest of the day, with nothing on screen saying
        // why. A reload is the one moment that is a new run and is not a keystroke.
        people_core::suggest::reset_gmail_budget();
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

    // -- suggestions --------------------------------------------------------

    /// Ask the two inboxes who else writes about this project, unless they have
    /// already been asked this session.
    ///
    /// The lookup is [`people_core::candidates_for`], the same call
    /// `nashcode people suggest` makes, on a background task: it shells out to `imsg`
    /// and to Gmail, and neither belongs on the frame the click arrived in.
    ///
    /// **What leaves the machine**: the project's *name*, as a Gmail search, and
    /// nothing else. No phone number and no address out of `people.json` is ever sent.
    pub fn look_for_suggestions(&mut self, project: String, cx: &mut Context<Self>) {
        if self.suggestions.contains_key(&project) || self.stage != Stage::Editing {
            return;
        }
        let Some(subject) =
            self.edit.to_file().projects.into_iter().find(|it| it.id == project)
        else {
            return;
        };
        let file = self.edit.to_file();
        self.suggestions.insert(project.clone(), Suggested::Looking);
        cx.notify();

        cx.spawn(async move |this, cx| {
            // Let the selection settle first. A card the operator arrowed past is not
            // a card they asked a question about, and it costs two processes to ask.
            cx.background_executor().timer(SETTLE).await;
            let still = {
                let project = project.clone();
                this.update(cx, |this: &mut PeopleApp, cx| {
                    let still = this.selected == Some(CardId::Project(project.clone()));
                    if !still {
                        // Forget the `Looking`, so coming back asks rather than
                        // showing a question that was never put.
                        this.suggestions.remove(&project);
                        cx.notify();
                    }
                    still
                })
            };
            if !matches!(still, Ok(true)) {
                return;
            }

            let found = cx
                .background_executor()
                .spawn(async move {
                    let found = people_core::candidates_for(&subject, &file);
                    // `notes_now`, not `take_notes`: the window's lookups are cached
                    // per project, so a drained note would be spent on whichever
                    // project was asked first and the second would say "nobody new"
                    // about a source that is still not installed.
                    (found, people_core::suggest::notes_now())
                })
                .await;
            // Err means the window closed while the sources were being read.
            let _ = this.update(cx, |this: &mut PeopleApp, cx| {
                let (found, notes) = found;
                // Nothing found *and* a source that could not answer is a failure the
                // operator can act on — install the tool, sign in. Nothing found with
                // both sources answering is simply nobody new.
                let state = match (found.is_empty(), notes.is_empty()) {
                    (true, false) => Suggested::Failed(notes.join(" · ")),
                    _ => Suggested::Found(found),
                };
                this.suggestions.insert(project, state);
                cx.notify();
            });
        })
        .detach();
    }

    /// Ask again after a source could not answer. The one way out of the error state.
    pub fn look_again(&mut self, project: String, cx: &mut Context<Self>) {
        self.suggestions.remove(&project);
        self.look_for_suggestions(project, cx);
    }

    /// Take a suggestion: the person is created, put on the project, and both ends are
    /// counted as having matched.
    ///
    /// It does not save. Accepting is a claim about who somebody is, and the operator
    /// gets to look at the picture the claim makes before it reaches the file every
    /// router reads.
    pub fn accept_suggestion(&mut self, project: &str, address: &str, cx: &mut Context<Self>) {
        // The whole sequence is one function over the model, in `edit.rs`, so what
        // accepting does to the file is provable without a window. What is left here
        // is the frame: find the list the row is in, and say what came back.
        let Some(Suggested::Found(found)) = self.suggestions.get_mut(project) else {
            return;
        };
        let Some(done) = edit::accept(&mut self.edit, found, project, address, &board::now())
        else {
            return;
        };
        self.say(done.notice, false);
        cx.notify();
    }

    /// Wave a suggestion away for this session. Nothing is written, so the next window
    /// offers them again — which is right: the answer was "not now", not "never".
    pub fn skip_suggestion(&mut self, project: &str, address: &str, cx: &mut Context<Self>) {
        self.drop_suggestion(project, address);
        cx.notify();
    }

    fn drop_suggestion(&mut self, project: &str, address: &str) {
        if let Some(Suggested::Found(found)) = self.suggestions.get_mut(project) {
            found.retain(|candidate| candidate.address() != address);
        }
    }

    // -- sync folders -------------------------------------------------------

    /// Open the panel that asks which folder the clients live in.
    ///
    /// It opens even when `NASHCODE_CLIENTS` answers, filled in with that answer. A
    /// command that added thirty-five projects to the file the instant it was clicked,
    /// from a path nobody on screen had seen, would be the one command in this window
    /// the operator could not predict.
    pub fn open_sync_panel(&mut self, cx: &mut Context<Self>) {
        let from_env = std::env::var("NASHCODE_CLIENTS").unwrap_or_default();
        if self.clients_dir.trim().is_empty() {
            self.clients_dir = from_env.clone();
        }
        // Decided here, once, rather than read again from the environment on every
        // frame: a render is a picture of state, and a process-wide variable read
        // from inside one is a fact no field owns. It says "prefilled" only while the
        // field still holds what the variable said, so a path typed over the top
        // stops claiming to have come from there.
        self.clients_dir_from_env = !from_env.trim().is_empty() && self.clients_dir == from_env;
        self.sync_panel = true;
        self.focus_field(Field::ClientsDir);
        cx.notify();
    }

    pub fn close_sync_panel(&mut self, cx: &mut Context<Self>) {
        self.sync_panel = false;
        if self.field == Some(Field::ClientsDir) {
            self.field = None;
        }
        cx.notify();
    }

    /// One project per client folder, into the edits rather than onto the disk.
    ///
    /// The report is applied to the working copy and left unsaved, like every other
    /// edit in this window: a command that wrote thirty-five projects straight to the
    /// file every consumer reads would be a command with no way back from a typo in
    /// the path.
    pub fn sync_folders(&mut self, cx: &mut Context<Self>) {
        if self.stage != Stage::Editing {
            self.say("There is no file to add projects to yet", true);
            cx.notify();
            return;
        }
        let dir = store::expand_home(&self.clients_dir);
        if dir.as_os_str().is_empty() {
            self.say("Name the folder your client folders are in", true);
            cx.notify();
            return;
        }

        let mut file = self.edit.to_file();
        match file.sync_folders(&dir) {
            Err(why) => self.say(why, true),
            Ok(report) => {
                // `Edit::from_file` of this window's own `to_file` is the round trip
                // the whole app rests on, so nothing already typed is lost by it.
                self.edit = Edit::from_file(&file);
                self.sync_panel = false;
                self.field = None;
                self.caret = 0;
                let board = self.board();
                self.selected = self.selected.take().filter(|id| board.holds(id));
                self.hover_link = None;
                self.say(store::sync_summary(&report), false);
            }
        }
        cx.notify();
    }

    // -- the selection ------------------------------------------------------

    pub fn select_card(&mut self, id: CardId, cx: &mut Context<Self>) {
        self.lane = Board::lane_of(&id);
        self.selected = Some(id);
        self.field = None;
        self.ask_about_selection(cx);
        cx.notify();
    }

    /// A project that has just become the selection asks the two inboxes who else
    /// writes about it.
    ///
    /// Every way of landing on a card calls this — the click, the arrow keys, Tab
    /// between lanes, and a project that was just created — so the section is never
    /// the one part of the inspector that a keyboard user cannot reach. A rename does
    /// not: it is the same project, and its answer travels with it in `resync_id`.
    fn ask_about_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(CardId::Project(project)) = self.selected.clone() {
            self.look_for_suggestions(project, cx);
        }
    }

    pub fn new_person(&mut self, cx: &mut Context<Self>) {
        let id = self.edit.add_person();
        self.selected = Some(CardId::Person(id));
        self.lane = Lane::People;
        self.clear_new_name(Field::PersonName);
        cx.notify();
    }

    pub fn new_project(&mut self, cx: &mut Context<Self>) {
        let id = self.edit.add_project();
        self.selected = Some(CardId::Project(id));
        self.lane = Lane::Projects;
        self.clear_new_name(Field::ProjectName);
        self.ask_about_selection(cx);
        cx.notify();
    }

    /// A card that has just been added opens with its name field empty.
    ///
    /// The field has no selection, so a caret parked at the front of "New project"
    /// would make the first keystroke read `AcmeNew project` and the id
    /// `acmenew-project`. Emptying it is what a selected placeholder would have done,
    /// with none of the machinery: the card keeps a title either way, because
    /// [`display`] falls back to the id.
    fn clear_new_name(&mut self, field: Field) {
        if let Some(text) = self.text_mut(field) {
            text.clear();
        }
        self.focus_field(field);
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
                // Nothing points at the old id any more, so an answer left in the map
                // would be handed to whoever next slugs to the same name.
                self.suggestions.remove(&id);
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
        // The panel's field belongs to the window, not to whatever card is selected.
        if field == Field::ClientsDir {
            return Some(&self.clients_dir);
        }
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
            Field::ProjectEnrich | Field::ProjectMediaOnly | Field::PersonSignal => {
                return None;
            }
            Field::ClientsDir => unreachable!("answered above"),
        })
    }

    fn text_mut(&mut self, field: Field) -> Option<&mut String> {
        if field == Field::ClientsDir {
            return Some(&mut self.clients_dir);
        }
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
            Field::ProjectEnrich
            | Field::ProjectMediaOnly
            | Field::PersonSignal
            | Field::ClientsDir => None,
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
                    // The same project under a new name, so an *answer* travels with
                    // it: asking the two inboxes again on every keystroke of a rename
                    // would be two processes per character. A question in flight does
                    // not — see `carry_suggestion`.
                    let carried = carry_suggestion(self.suggestions.remove(&id));
                    if let Some(found) = carried {
                        self.suggestions.insert(fresh.clone(), found);
                    }
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
        match field {
            Field::PersonSignal => {
                self.person_id().and_then(|id| self.edit.person(id)).is_some_and(|it| it.signal)
            }
            Field::ProjectEnrich => self
                .project_id()
                .and_then(|id| self.edit.project(id))
                .is_some_and(|it| it.enrich),
            Field::ProjectMediaOnly => self
                .project_id()
                .and_then(|id| self.edit.project(id))
                .is_some_and(|it| it.media_only),
            _ => false,
        }
    }

    pub fn flip(&mut self, field: Field, cx: &mut Context<Self>) {
        if field == Field::PersonSignal {
            if let Some(id) = self.person_id().map(str::to_owned)
                && let Some(person) = self.edit.person_mut(&id)
            {
                person.signal = !person.signal;
            }
            self.field = Some(field);
            cx.notify();
            return;
        }
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
    ///
    /// The sync panel's field is on no ring: it is one field in a panel that is either
    /// open or closed, and stepping out of it into the inspector behind it would move
    /// the keyboard somewhere the eye is not.
    fn step_field(&mut self, back: bool) {
        if self.field == Some(Field::ClientsDir) {
            return;
        }
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
    fn enter_lane(&mut self, lane: Lane, cx: &mut Context<Self>) {
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
        self.ask_about_selection(cx);
    }

    /// Move the selection up or down the lane the keyboard is in.
    fn step_card(&mut self, back: bool, cx: &mut Context<Self>) {
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
        self.ask_about_selection(cx);
    }

    /// One step left or right through the lanes.
    fn step_lane(&mut self, back: bool, cx: &mut Context<Self>) {
        let at = Lane::ALL.iter().position(|lane| *lane == self.lane).unwrap_or(0);
        let next = if back {
            (at + Lane::ALL.len() - 1) % Lane::ALL.len()
        } else {
            (at + 1) % Lane::ALL.len()
        };
        self.enter_lane(Lane::ALL[next], cx);
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
            // Escape leaves the innermost thing first: the panel, then the field,
            // then the selection.
            "escape" => {
                if self.sync_panel {
                    self.close_sync_panel(cx);
                } else if self.field.take().is_none() {
                    self.selected = None;
                }
                self.notice = None;
            }
            "tab" if self.field.is_some() => self.step_field(modifiers.shift),
            "tab" => self.step_lane(modifiers.shift, cx),
            _ => match self.field {
                Some(field) => self.field_key(field, key, event, cx),
                None => match key {
                    "up" => self.step_card(true, cx),
                    "down" => self.step_card(false, cx),
                    "left" => self.step_lane(true, cx),
                    "right" => self.step_lane(false, cx),
                    "enter" => self.step_field(false),
                    _ => return,
                },
            },
        }
        cx.notify();
    }

    /// A key inside a focused inspector field.
    fn field_key(&mut self, field: Field, key: &str, event: &KeyDownEvent, cx: &mut Context<Self>) {
        // The panel holds one field and one command, so Enter in it is that command.
        if field == Field::ClientsDir && key == "enter" {
            self.sync_folders(cx);
            return;
        }
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
        // The one command in the toolbar that opens something rather than committing
        // something, so it says so with an ellipsis and stays quiet beside Save.
        let sync_enabled = self.stage == Stage::Editing && self.busy.is_none();
        let sync_button =
            widgets::button("sync", "Sync folders…", Tone::Ghost, sync_enabled, theme);
        actions = actions.child(if sync_enabled {
            sync_button.on_click(cx.listener(|this, _, _, cx| this.open_sync_panel(cx)))
        } else {
            sync_button
        });

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

    /// The panel `Sync folders…` opens: one field, one command, one way out.
    ///
    /// Inline rather than a native open-dialog: v1 draws no platform surfaces, and the
    /// answer is a path the operator can already type. It is not a focus trap — Escape
    /// closes it, and the field it opens on is on no tab ring, so the keyboard cannot
    /// wander out of it into the inspector behind.
    fn render_sync_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let from_env = self.clients_dir_from_env;
        v().gap(space::TIGHT)
            .mx(space::SECTION)
            .mt(space::GROUP)
            .p(space::GROUP)
            .rounded(theme.radius)
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .child(section_title("One project per folder under this directory", theme))
            .child(self.field(
                Field::ClientsDir,
                "Client folders",
                "~/NashvilleAutomation",
                cx,
            ))
            .child(muted(
                if from_env {
                    "From NASHCODE_CLIENTS. Nothing is removed, and nothing already written is changed."
                } else {
                    "Nothing is removed, and nothing already written is changed."
                },
                theme,
            )
            .text_xs())
            .child(
                h().gap(space::TIGHT)
                    .child(
                        widgets::button("sync-run", "Add the folders", Tone::Primary, true, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.sync_folders(cx))),
                    )
                    .child(
                        widgets::button("sync-cancel", "Cancel", Tone::Ghost, true, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.close_sync_panel(cx))),
                    ),
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
        if self.sync_panel {
            root = root.child(self.render_sync_panel(cx));
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
    /// Every field the inspector draws, and the one field it does not.
    const ALL: [Field; 14] = [
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
        Field::PersonSignal,
        Field::ClientsDir,
    ];

    #[test]
    fn every_field_is_on_exactly_one_tab_ring() {
        for field in ALL {
            let times =
                Field::PROJECT.iter().chain(Field::PERSON.iter()).filter(|f| **f == field).count();
            let wanted = usize::from(field != Field::ClientsDir);
            assert_eq!(times, wanted, "{field:?} is on {times} rings, not {wanted}");
        }
        assert_eq!(Field::PROJECT.len() + Field::PERSON.len(), ALL.len() - 1);
    }

    /// The sync panel's field is on no ring on purpose, so it needs the other half of
    /// the accessibility contract: something must put the keyboard in it, and Escape
    /// must take the keyboard back out.
    ///
    /// `open_sync_panel` focuses it and `close_sync_panel` releases it — the two are
    /// the only ways in and out, and they are what the button and the key both call.
    #[test]
    fn the_panels_field_is_reached_by_opening_the_panel_and_left_by_closing_it() {
        assert!(!Field::PROJECT.contains(&Field::ClientsDir));
        assert!(!Field::PERSON.contains(&Field::ClientsDir));
        assert!(!Field::ClientsDir.is_toggle());
        assert!(!Field::ClientsDir.is_multiline(), "one directory, one line");
        assert!(!Field::ClientsDir.renames(), "it moves no card");
    }

    #[test]
    fn a_toggle_is_never_a_text_field_and_every_field_has_its_own_id() {
        let mut keys: Vec<&str> = ALL.iter().map(|f| f.key()).collect();
        keys.sort_unstable();
        let total = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), total, "two fields share an element id");

        for field in ALL {
            assert!(!(field.is_toggle() && field.is_multiline()), "{field:?}");
        }
    }

    /// The wedge a rename used to leave: "Looking…" for the rest of the session.
    ///
    /// The answer to a question the old id asked lands under the old key and is
    /// dropped, and a key already in the map is a key `look_for_suggestions` will not
    /// ask about again — so a carried `Looking` is a state with no way out of it.
    #[test]
    fn a_rename_carries_an_answer_across_and_never_an_unanswered_question() {
        let found = Suggested::Found(vec![Candidate {
            name: "Dana Reyes".to_owned(),
            email: Some("dana@example.com".to_owned()),
            phone: None,
            where_seen: "Gmail".to_owned(),
            last: None,
        }]);
        assert!(
            matches!(carry_suggestion(Some(found)), Some(Suggested::Found(found)) if found.len() == 1),
            "an answer is about the project, and it is the same project"
        );
        assert!(matches!(
            carry_suggestion(Some(Suggested::Failed("gws is not on PATH".to_owned()))),
            Some(Suggested::Failed(_))
        ));

        // The two that leave the new id in the not-asked state, which is the one with
        // a Look button on it.
        assert!(carry_suggestion(Some(Suggested::Looking)).is_none());
        assert!(carry_suggestion(None).is_none());
    }

    #[test]
    fn only_the_two_name_fields_move_a_card() {
        // Editing a name re-slugs the id and moves the selection with it. Any other
        // field doing that would move the selection while somebody typed a phone.
        for field in &ALL {
            assert_eq!(
                field.renames(),
                matches!(field, Field::PersonName | Field::ProjectName),
                "{field:?}"
            );
        }
    }
}
