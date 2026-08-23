# nashcode-people — where the implementation had to choose

`viewer/SPEC.md` §People is the contract. This file records what the code decided
where the spec left room, in the form the GPUI coding guide asks a change to be able
to state.

The window is one picture, not a set of screens. Three lanes — every address, every
person, every project — and a wire wherever the file says one belongs to the next.
Click a card and the route it is part of lights up; everything else goes quiet. The
inspector on the right is where that card is edited.

One sentence covers the architecture: the retained identity is **the file**, not a
screen. `PeopleApp` is the only entity. The canvas, the wires, and the inspector are
three readings of one `Edit`, because they describe one document and three entities
would mean keeping three copies of it in step.

---

## Shell and toolbar

**Behavior owner** — `PeopleApp` (`app.rs`). It owns the stage, the edit model, the
selection, the focused lane, the inspector field, the caret, the hovered wire, the
busy flag, and the two commands. **Presentation owner** — `PeopleApp::render_toolbar`,
`render_status`, `render_disk_banner`, `render_placeholder`, drawing with `widgets.rs`
and `theme.rs`.

**Retained identity and lifecycle** — the file path, fixed at launch from
`people_core::default_path()`. `stage` walks `Loading → Missing | Broken | Editing`.
`edit` is the working copy, `saved` is the copy on disk, and `edit != saved` is the
whole of "unsaved". One background task lives as long as the window: it does the first
read, asks the viewer for `pushed_at`, then polls the file's modification time every
two seconds.

**Pointer, keyboard, focus** — one `FocusHandle`, on the root, taken once from the
bootstrap. ⌘S saves, ⇧⌘P pushes, ⌘N adds a person, ⇧⌘N adds a project. A disabled Save
or Push carries no click handler at all; pressing its key says why in the status line,
and a viewer that could not be resolved states its reason there permanently. Every
command has one method; the button and the key call the same one.

**Layout and overflow** — the root is a column: toolbar, optional disk banner, body,
status line. Only the body may shrink (`flex_1().min_h_0()`), so a tall canvas never
pushes the status line off the window. The titlebar is transparent, so the window's own
dark background runs to the top of the frame instead of a system-grey band sitting over
a dark picture; the cost is that macOS draws the traffic lights over the content, and
the toolbar reserves `app::TITLEBAR` at its top for them.

**Theme tokens** — `background`, `border`, `danger`, `warning`, `muted`; spacing
`SECTION` for the frame and `TIGHT`/`GROUP` inside it. No exceptions.

**What a save refuses** — `store::refusals` is the whole rule, and it is a pure
function of the file: a fatal finding stops the write, a warning does not. Fatal means
the file would not parse again, and this window is the only way back into it.

**The test that fails if it regresses** —
`store::tests::save_is_offered_only_when_there_is_a_loaded_file_with_a_change_in_it`,
`store::tests::push_waits_for_the_save_and_for_a_viewer_to_push_to`,
`store::tests::a_fatal_finding_stops_the_save_and_a_warning_does_not`,
`store::tests::a_read_that_lands_after_a_keystroke_asks_instead_of_overwriting_it`, and
the three `disk_change` tests.

---

## The canvas: three lanes

**Behavior owner** — `board.rs` decides which cards exist, which band each sits in,
which warning it carries, and which cards are joined. `PeopleApp` owns the selection
and the lane the keyboard is in. **Presentation owner** — `lanes.rs`.

**Retained identity and lifecycle** — `board::CardId`: an address, plus the person who
holds it and which of that person's copies it is; a person id; or a project id. Never a
lane position: an id is the name in slug form, so a card moves when its name changes,
and a selection that meant "row 3" would follow the wrong card. The occurrence is part
of a contact's identity because an address is not one: two people may write the same
number down, and one person may write it down twice. `CardId::key` is what an element
id is built from, so cards that look alike are still three separate things to the
bounds map, the wires, and the selection. The board itself is derived on every frame from
`edit.to_file()`; a stored board would go stale between keystrokes, and the file is
small enough that deriving it costs less than keeping it right.

**Order** — `people_core::by_frecency`, never alphabetical. A `seen` count halved
every **fourteen days** (`people_core::HALF_LIFE_DAYS`): a client untouched for a
fortnight is worth half what they were, one untouched for a quarter about a fiftieth.
The clock is read once per `Board::from_file` and handed to every lane below it, so
three lanes can never disagree about the same file; `Board::at(file, now)` is the same
function with the instant stated, which is what makes the order testable. Projects and
people sort by their own `seen`; **contacts have none of their own and follow their
holder's place**, because an address is warm when the person holding it is. A card that
has ever matched carries `3× · 2d ago` under it — the number the order was made of,
said out loud, in `people_core::seen_label`, which is the same spelling
`nashcode people ls` prints.

**Collapse at scale** — with thirty-five client folders the picture stops being a
picture. A lane past `board::EXPANDED` (ten) draws its warm head in full and the tail
as one line each: the name, still clickable, still on the arrow keys, still carrying
its wire. A selection expands **its whole connected chain**, whatever its rank, which
is the exception the rule exists for — the project the operator just clicked is the one
they want in full, and a chain that collapsed halfway would answer "where does this
route?" with half a wire. The header says what it is doing: `Projects 35 · 10 shown`,
and just `Projects 8` when the whole lane is drawn, because "8 · 8 shown" is noise. Ten
is what fits above the fold beside the inspector at the window's opening height.
**All three lanes, addresses included.** The canvas scrolls as one piece, so a
contacts lane that drew all seventy addresses of a thirty-five-client file in full
would push the people and the projects off the fold to say "phone" seventy times. A
collapsed address is its value alone: the kind is already the `+` or the `@` in it,
and a warning about an address nobody has clicked is read on the card that was.

**Bands** — a lane is sorted into runs, not annotated in place: `Band::Routes`, then
`Band::Nowhere` under a warning-coloured "Routes nowhere", then `Band::Mine` under
"Yours, never scores". `me` entries are not a fault — never scoring is their job — so
their band is muted and they are not counted as routing nowhere.

**One name rule** — a card's title, a project chip, a refusal, and the inspector's
heading all call `edit::display`: the name when there is one, else the id. `board.rs`
kept a second copy of that rule; two rules that agree today are one rename apart from
disagreeing, in a window where the id *is* the name.

**Warnings** — on the card they concern, one short line, never a banner. A phone that
is not E.164 sits on that address. A person with no phone and no email sits on that
person. A project with nobody on it, or listing an id nobody has, sits on that project.
The status line carries only the count, and only of the warnings: a fatal finding is
not a warning, it is a refused save, and it is said in full where the save was asked
for.

**Pointer, keyboard, focus** — a click on a card selects it and moves the keyboard to
its lane. Up and Down move the selection within the focused lane; Left, Right and Tab
move between lanes; Enter drops into the inspector; Esc clears the selection. Moving
to a lane lands on a card the current selection reaches when there is one, so Tab
walks the wire rather than jumping to the top of the next column. The focused lane
keeps an accent rule under its title even while the eye is on the selected card.

**Layout and overflow** — one scroll owner for the whole canvas, not one per lane.
Three lanes that scrolled apart would tear every wire that crosses between them, and
a wire drawn from a card that had scrolled out of its own lane would cross a
neighbour's. Lane widths are fixed (14.5, 12.5 and 15 rem); the gaps between them are
`space::REGION`, which is where the wires are drawn.

**Theme tokens** — `surface` for a card, `accent`/`accent_soft` for the selected and
lit states, `border` for a plain card, `muted` for secondary text, `warning` for the
band title and the card warnings. A dimmed card drops to `opacity(0.55)` and comes
back on hover, so nothing is ever unreadable.

**The test that fails if it regresses** —
`board::tests::the_file_becomes_three_lanes_with_the_dead_ends_below_the_fold`,
`board::tests::a_warning_sits_on_the_card_it_is_about`,
`board::tests::a_project_with_nobody_on_it_says_so_on_its_own_card`,
`board::tests::one_number_on_two_people_is_two_cards_two_wires_and_two_selections`,
`board::tests::every_lane_is_warmest_first_and_never_alphabetical`,
`board::tests::a_card_says_the_warmth_its_place_was_decided_by`,
`board::tests::a_long_lane_draws_its_warm_head_and_collapses_the_rest`,
`board::tests::a_long_contacts_lane_collapses_like_the_other_two`,
`board::tests::a_selection_expands_its_whole_chain_however_cold_it_is`,
`board::tests::the_rule_itself_is_about_an_order_and_a_selection_and_nothing_else`,
`lanes::tests::a_lane_header_says_how_much_of_itself_it_is_drawing`, and
`lanes::tests::a_project_with_one_person_on_it_does_not_say_1_people`.

---

## The links layer

**Behavior owner** — `links.rs`: where a wire starts and ends, the curve between, and
which one the pointer is on. **Presentation owner** —
`PeopleApp::render_links` in `lanes.rs`.

**Retained identity and lifecycle** — none, on purpose. Endpoints are not stored.
Each card carries a zero-size `canvas` child whose only job is to write its resolved
bounds into a shared `HashMap<CardId, Bounds<Pixels>>` during prepaint; the links
canvas reads that map during paint of the same frame. GPUI prepaints the whole tree
before it paints any of it, so a wire is always measured from the frame it is drawn
in — through a scroll, a resize, or a change of base font. `render` clears the map
first, so a deleted or renamed card leaves no ghost to hang a wire from.

The links canvas is the **first** child of the canvas region, so it paints under the
cards it joins; its paint still sees the whole frame's measurements, because paint
comes after every prepaint.

**Curves, not polylines.** `gpui-ce 0.3.3` has `PathBuilder::stroke`,
`cubic_bezier_to` and `Window::paint_path`, so the wires are cubic Béziers whose
control points sit level with each end, half the horizontal gap in. They therefore
leave and arrive horizontally, which is what makes a column of them read as wires
rather than as a scribble.

**Hover.** A path is not an element and cannot be hit-tested, so the canvas takes
`on_mouse_move`, and `links::nearest` walks the same sixteen samples the painter
draws and answers with the wire within seven points of the pointer. Both its ends
then light up. The geometry it tests is the previous frame's, which only matters
while a scroll is in flight.

The hovered wire is an index into one frame's link list, so it is dropped whenever that
list is about to be rebuilt under it: a delete, a rename that moves a card, and the
pointer leaving the canvas. An index that outlived its list would light two cards the
pointer is nowhere near.

**Colour** — `border` for a plain wire, `accent` for a highlighted one (and three
quarters of a point thicker). A selection pushes the wires it does not include to 35%
alpha rather than hiding them, so the picture keeps its shape while one route is read.

**Theme tokens** — `border` and `accent`. The two exceptions are the stroke width
(1.5 px) and the hit tolerance (7 px): both are physical, neither is product spacing.

**The test that fails if it regresses** —
`links::tests::a_link_leaves_the_right_edge_and_arrives_at_the_left_edge_of_its_two_cards`,
`links::tests::an_endpoint_moves_exactly_as_far_as_its_card_scrolls` (the scrolling
contract, stated as arithmetic),
`links::tests::the_curve_starts_and_ends_on_its_endpoints_and_leaves_them_level`,
`links::tests::the_pointer_finds_the_link_it_is_on_and_no_other`,
`links::tests::a_pointer_between_two_wires_takes_the_nearer_one`, and
`links::tests::two_cards_on_top_of_one_another_draw_nothing_rather_than_panicking`.

---

## The selection

**Behavior owner** — `Board::connected`. **Presentation owner** —
`PeopleApp::card_state` and the wire colouring in `render_links`.

The traversal follows the picture rather than the graph. From a contact the eye runs
right — to the person, then to that person's projects. From a project it runs left.
From a person it runs both ways. A plain graph walk would keep going, and selecting
one phone number would light up half the board through a project's other members. An
address in `me` belongs to nobody and lights up alone, which is the whole point of it.

**The test that fails if it regresses** —
`board::tests::selecting_a_contact_lights_its_person_and_that_persons_projects_and_stops`,
`board::tests::selecting_a_person_lights_both_sides_of_them`,
`board::tests::selecting_a_project_lights_its_people_and_their_addresses`, and
`board::tests::one_of_the_operators_own_addresses_lights_up_alone`.

---

## The inspector

**Behavior owner** — `PeopleApp`: `focus_field`, `text_mut`, `flip`,
`set_membership`, `delete_selected`, `resync_id`. **Presentation owner** —
`inspector.rs`.

**Retained identity and lifecycle** — whatever `selected` names. A person shows name,
phones, emails, a **Signal** switch, and a row of chips holding **every** project,
filled when the person is on it: membership is a thing you see, not a list you
maintain. The chips are in the lane's order — warmest first — because a chip row sorted
differently from the lane it names would be a second answer to "which project matters".
A project shows its nine fields, its members in the same warm order, and its Suggested
section.

**Signal** — one switch, bound to `Person.signal`. Not a third list: the number is
already in `phones`, and the flag says what else that number reaches. Off is the
absence of a flag, so a file that never had `"signal"` in it never gains one. A contact is read-only — it is a line inside a person, and the
person is where it is edited — with one command, "Go to person". Nothing selected
shows one sentence and the two New buttons.

**Ids are the name in slug form.** Editing a name re-slugs the id on the keystroke and
carries every project that lists that person across to the new one. The rename is one
operation over the whole file and the new id is uniquified before anything moves, so
it can never collide; the selection is re-pointed at the new card in the same step.
The id is shown and is not typed.

**Pointer, keyboard, focus** — Tab and ⇧Tab walk the ring for the selected kind of
card: nine fields for a project, three for a person, none for a contact. The two
toggles are on the ring and Space or Enter flips them. Enter in a single-line field
means "done with this one" and steps on. Esc leaves the field and returns the keyboard
to the lane. Delete is the one `Danger` button in the window; on a person it refuses
itself when a project still lists them and names those projects, because taking them
off there is the fix. A delete that goes through says what left — "Deleted Rob Castro"
— because a card that vanishes silently is one the operator goes looking for. **Gap, on purpose:** the project chips, the toolbar buttons and
the lane New buttons are pointer-only.

**Layout and overflow** — a fixed 21 rem column with its own scroll, so the lanes keep
their width whatever the inspector holds.

**Theme tokens** — `surface` for the panel, `accent`/`accent_soft` for a filled chip
and a focused field, `danger` for Delete, `muted` for labels and help.

**The test that fails if it regresses** —
`edit::tests::the_signal_flag_is_a_field_of_the_form_rather_than_a_key_carried_past_it`,
`edit::tests::an_id_is_the_name_in_slug_form_and_a_rename_carries_every_project_with_it`,
`edit::tests::a_rename_onto_a_taken_id_is_suffixed_rather_than_a_collision`,
`edit::tests::a_name_with_nothing_to_slug_still_gets_an_id`,
`edit::tests::deleting_a_person_a_project_still_lists_is_refused_and_says_where`,
`edit::tests::membership_is_a_switch_rather_than_a_list_that_can_hold_a_name_twice`,
`edit::tests::the_file_survives_the_round_trip_through_the_fields`,
`app::tests::every_field_is_on_exactly_one_tab_ring`, and
`app::tests::only_the_two_name_fields_move_a_card`.

---

## The Suggested section

**Behavior owner** — `PeopleApp`: `look_for_suggestions`, `look_again`,
`accept_suggestion`, `skip_suggestion`, `carry_suggestion`, and `edit::accept`, which
is the whole of an accept over the model. The discovery itself is
`people_core::suggest::candidates_for`, which is also what `nashcode people suggest`
calls — one implementation, so a name the terminal offers and a name the window offers
are the same name found the same way. **Presentation owner** —
`PeopleApp::suggestions` and `candidate_row` in `inspector.rs`.

**What leaves the machine.** The Gmail search sends the selected project's **name** as
the query, and nothing else. No phone number, no address, no person id, nothing else
out of `people.json` is ever sent. The Messages side sends nothing at all: `imsg` reads
the local database. Everything the two answer with is compared against the file here,
on this Mac. The section says so on screen, under its title, rather than only here.

**Retained identity and lifecycle** — `HashMap<project id, Suggested>`, for the life of
the window and no longer. A lookup runs two processes, so re-running it every time the
operator clicked back onto a card would make the inspector feel broken and would ask
Gmail the same question ten times. A candidate is identified by its **address**, never
by its row: the list shortens under every accept, and a position would name whoever
moved up into it.

**A rename carries an answer and never an unanswered question.** `Found` and `Failed`
are about the project, and it is the same project under a new name, so `resync_id`
moves them to the new id. `Looking` is not an answer — it is a question the *old* id
asked, and the task that asked it drops its own work when it sees the selection has
moved. Carried across, that left `Looking…` on the new id with nothing coming, no Look
button, and a key already in the map that `look_for_suggestions` would not ask about
again: "Looking…" for the rest of the session, one keystroke into naming a project. So
a rename hands the question back as no question at all, which is the state the Look
button is in. `carry_suggestion` is that one decision, and it is pure.

**A project that goes takes its answer with it**, and a reload drops every answer:
they were about the file that has just been replaced, and a project that kept its id
through it is not the same question — its name, its query and its people may all have
moved under it. The same reload gives Gmail's hundred-message budget back
(`people_core::suggest::reset_gmail_budget`). The budget stops one CLI sweep from
making eight hundred round trips; a window open all day would otherwise spend it once
and then draw "nobody new" for the rest of the day with nothing on screen saying why.
A reload is the one moment that is a new run and is not a keystroke.

**The settle delay.** Every way of landing on a project asks — the click, the arrow
keys, Tab between lanes, a project just created — so the section is not the one part of
the inspector a keyboard cannot reach. Arrowing down a lane of thirty-five would then
be thirty-five pairs of processes, so the task waits `app::SETTLE` (350 ms) and then
checks that the project is still the selection; if it is not, it forgets its own
`Looking` and returns, and coming back asks properly. A third of a second is below the
threshold at which a person waiting notices a wait and well above the speed at which
the same person walks past a card.

**Accepting never saves.** `edit::accept` is the whole sequence in one function over
the model: it creates the person (`Edit::accept_person`, id from
`people_core::unique_id` over the name's slug), puts them on the project, counts one
match on both ends, drops the row that offered them, and answers with the sentence the
status line says. Half of it would leave a person on no project, or a project warmed
for somebody it does not list. What is left in the window is the frame. It leaves the
file unsaved. Accepting is a claim about who
somebody is, and the operator gets to look at the picture that claim makes before it
reaches the file every router reads. Skipping writes nothing at all, so the next window
offers them again — the answer was "not now", not "never".

**Pointer, keyboard, focus** — the rows are pointer-only, like the project chips and
the toolbar. Same documented gap, same v2 fix.

**Layout and overflow** — inside the inspector's own scroll column, so a project with
nine candidates does not widen the lanes.

**Theme tokens** — `border` for a row, `muted` for the address and the sighting,
`warning` for the source that could not answer, `accent` through `Tone::Primary` on
Accept.

**All four states are drawn** — not asked (with a Look button, which is where a
project selected by a reload lands), looking, nobody new, and a source that could not
answer, that last one with the reason and a **Try again**. A section that appeared only
when it had something would read as a section that had gone wrong.

**The test that fails if it regresses** —
`edit::tests::an_accepted_suggestion_arrives_with_the_id_its_name_gives_it`,
`edit::tests::accepting_warms_both_ends_of_what_was_accepted`,
`edit::tests::accepting_a_suggestion_is_one_step_and_says_what_it_did`,
`app::tests::a_rename_carries_an_answer_across_and_never_an_unanswered_question`,
and, in the shared
crate, `people_core::suggest::tests::*` — the chat parser, the `From:` parser, the
dedupe, and `a_person_on_another_project_is_not_a_candidate_here_either`.

---

## The Sync folders panel

**Behavior owner** — `PeopleApp::open_sync_panel`, `close_sync_panel`, `sync_folders`;
the two pure parts are `store::expand_home` and `store::sync_summary`. The sync itself
is `PeopleFile::sync_folders`, in the shared crate, so the CLI and the window add the
same projects from the same directory. **Presentation owner** —
`PeopleApp::render_sync_panel`.

**Retained identity and lifecycle** — `clients_dir`, a string on the window;
`sync_panel` and `clients_dir_from_env`, two bools. The panel opens filled from
`NASHCODE_CLIENTS` when that is set, and whether it did is decided **at open time**
and kept: a render is a picture of state, and a process-wide variable read from inside
one is a fact no field owns. The flag says "prefilled" only while the field still
holds what the variable said, so a path typed over the top stops claiming to have come
from there. The panel **opens** rather than running: a command that added thirty-five projects the
instant it was clicked, from a path nobody on screen had seen, would be the one command
in this window the operator could not predict.

**No native dialog in v1.** The answer is one path the operator can type, and a file
picker is a platform surface with its own focus contract, its own sandbox questions,
and no test.

**The report goes into the edits, not onto the disk.** `Edit::from_file` of the
window's own `to_file` is the round trip the whole app rests on, so nothing already
typed is lost by it, and the status line says what arrived —
`Added 2: orchard-hill, stone-bakery. 0 already there, skipped deploy-web,
tin-roof-backups. Not saved yet.` It names what the file's `skip` list turned away,
because a client folder that did not appear is the question the command raises. It
names a folder whose name slugs to nothing (`---`) apart from those, under `no project
name in …`: the operator asked for a skip and did not ask for this, and the folder has
to be renamed or put in `skip` — neither of which is possible while nothing on screen
says which folder it was.

**Pointer, keyboard, focus** — the panel opens with the keyboard in its field, Enter
runs the sync, Escape closes it. `Field::ClientsDir` is on **no** tab ring: it is one
field in a panel that is either open or closed, and Tab stepping out of it into the
inspector behind would move the keyboard somewhere the eye is not. It is not a focus
trap either — Escape is always the way out.

**Layout and overflow** — a band under the toolbar, above the body, exactly where the
changed-on-disk banner sits, so the two never fight for the same strip.

**Theme tokens** — `surface`, `border`, `muted`; `Tone::Primary` on the one command and
`Tone::Ghost` on Cancel.

**The test that fails if it regresses** —
`store::tests::a_leading_tilde_is_the_home_directory_and_an_empty_field_is_no_path`,
`store::tests::a_sync_says_what_arrived_what_was_already_there_and_what_was_turned_away`,
and `app::tests::the_panels_field_is_reached_by_opening_the_panel_and_left_by_closing_it`.

---

## Implementation checklist

- **State and side-effect ownership are explicit** — one entity owns the document; the
  picture, the geometry, the disk, the network, and every may-I decision are pure
  functions in `board.rs`, `links.rs`, `edit.rs` and `store.rs`.
- **`RenderOnce` versus `Entity<T>`** — one `Entity<PeopleApp>`, because the document
  spans frames. Every widget is a value-like function: it takes what it draws and
  retains nothing.
- **Repeated elements have stable domain-based IDs** — `widgets::eid(prefix, key)`
  builds every id from an address, a person id, a project id, or a field name. No list
  index is an id anywhere.
- **Theme tokens and sizes replace visual literals** — every colour, radius, and gap
  comes from `theme.rs`. The raw pixels that remain are all physical boundaries, not
  product geometry, and each is commented where it sits: the caret's 1.5 px width, the
  wire's 1.5 px stroke, the 7 px hover tolerance, the 28 px macOS titlebar the toolbar
  keeps clear (`app::TITLEBAR`), and the window's own geometry in `main.rs` — the
  1280×780 opening size, the 1040×620 minimum, and the 1440×900 fallback rectangle for
  a display that will not answer. A window rectangle is in screen points by definition;
  it is the one measurement the base font must not scale.
- **Keyboard, focus, disabled state, and overlays work together** — one focused root
  dispatches every key; a disabled command carries no handler and says why; there are
  no overlays, so there is no focus trap to get wrong.
- **Loading, empty, error, and cancellation paths are represented** — loading, file
  missing (with Create empty file), parse error (nothing editable, error text shown),
  empty board (with the two New buttons), unsaved, changed-on-disk (Reload or Keep),
  push in flight, push failed.
- **Long data sets use a virtualized component** — they do not, and they now collapse
  instead. The file holds tens of people, not thousands; the canvas is one scrolling
  column, and a lane past ten draws the rest as one line each (`board::expanded`), so
  thirty-five projects cost thirty-five short rows rather than thirty-five full cards.
  Virtualization proper would break the links layer, which measures every card each
  frame: a virtualized canvas would have to derive the bounds of cards it did not lay
  out. That is still the v2 change if the file ever reaches thousands.
- **Public API additions preserve dependency direction** — the only new public API is
  `people_core::contact_map`. It went into the shared crate, not the app, so the join
  exists once. `people-core` still knows nothing about gpui.
- **Tests prove behavior at the appropriate layer** — pure tests for the board, the
  selection rule, the wire endpoints and the hover test, the round trip, the slug ids,
  the list edits, the caret, the two toolbar decisions, and the watcher decision. No
  window is needed for any of them.
- **Formatting, Clippy, and tests pass** — `cargo build -p nashcode-people`,
  `cargo nextest run -p nashcode-people -p people-core`, and
  `cargo clippy -p nashcode-people -p people-core --all-targets -- -D warnings`.

---

## The choices

**`gpui-ce` without `gpui-component`.** `gpui-component` depends on Zed's `gpui 0.2.2`.
That is a different package from `gpui-ce`, and two `gpui`s cannot be in one build.
`gpui-ce 0.3.3` is what the reference application on this Mac builds against, so it is
what this builds against. The cost is that `cx.theme()` and the control set do not
exist and had to be written.

**The hand-written widgets.** `widgets.rs` holds a card, a button, a link, a toggle, a
pill, and a text field, plus the caret arithmetic. Each is a function of its arguments
and takes no callback: the view that owns the behavior wraps it in
`div().id(…).on_click(…)`, so identity and side effects stay with the entity that can
be held responsible for them. The text field is the one with real behavior — insert,
backspace, delete, left, right, home, end, up, down — and all of it is pure and
tested. It has **no selection, no mouse caret placement, and no clipboard**: a click
focuses the field and puts the caret at the end. That is v1's honest limit, and it is
why a new card opens on an **empty** name field rather than on a selected placeholder:
a caret parked in front of "New project" made the first keystroke read
`AcmeNew project` and the id `acmenew-project`. The card keeps a title either way,
because `edit::display` falls back to the id.

**GPUI Actions.** The commands are dispatched from one `on_key_down` on the focused
root, as the reference application does on this crate version, rather than through
registered Actions and key bindings. What the Action rule protects is preserved by
hand: every command has one method, and the button and the key call it. Actions are a
v2 change with no behavior difference the operator can see.

**The modification-time poll.** The watcher reads `stat` on the file every two
seconds, on a background executor, and compares the answer with what the window last
wrote. Two seconds is faster than a person can switch windows, one `stat` of one small
file costs nothing, and it needs no platform-specific event API. Unsaved edits are
never discarded by it: a file that moved while there were edits raises a banner, and
Reload or Keep is the operator's choice.

**The watcher decides twice.** Once on the poll, and again when the background read
lands, because the read is not instant and a person can type during it. The second
decision runs the same `store::disk_change` on the frame it lands in: unsaved now means
the banner, not the reload, and a stamp that went back to where the window left it —
the window's own save, usually — means the read is stale and is dropped. This is the
guide's rule about work that arrives after the state it was asked about moved.

**The file carries more than the window models.** `people-core` allows keys nothing
models — `skip`, a person's `seen`, a hand-written `"schema_version"` — and keeps them
through a load and a save. `Edit` does the same: each person, each project, and the
file itself parks its remainder, and `to_file` writes it back untouched. The modelled
fields are blanked out of the parked copy, so the same value is never held twice and
`edit != saved` still answers "unsaved" exactly. A save that dropped a stray key would
teach the operator not to write in their own file.

**Push waits for Save.** Push sends the file on disk, not the edits in the window, so
the viewer's copy is always a copy of something that exists. Pushing the window's
state would let the viewer answer "which project" from a file nobody could inspect.

**The push time after a push** comes from the push receipt (`PushReply::pushed_at`)
rather than a second `people_core::pushed_at` call. It is the same clock answering the
same question, one request fewer. The start-up read still goes through `pushed_at`,
because there is no receipt yet.

**No drag and drop.** Membership is a chip you click, and the wires are drawn from the
file rather than pulled by hand. Dragging a wire would be a second way to say the same
thing, with its own hit-testing and its own undo.

**One implementation of the discovery.** `Candidate` and the two sources moved out of
`cli/src/commands/people.rs` into `people_core::suggest`, behind the `client` feature,
and the CLI now calls `candidates_for`. Two copies of "who else writes about this
project" would have drifted the first time one of them learned a new source. The
Messages chat list is read **once per process** (`suggest::chats`): `imsg chats` takes
no search, so the alternative was thirty-five reads of one answer. That is right for
the CLI, which exits after a run, and right for the window, whose suggestions are
cached per project anyway — a chat list from earlier in the session is exactly as fresh
as the suggestions drawn from it.

**A source that could not answer speaks once — to a process that then exits.**
`people_core::suggest` keeps one note per tool per process. The CLI takes them with
`take_notes`, which drains, and exits. The window reads them with `notes_now`, which
does not: its lookups are cached per project, so a drained note would be spent on
whichever project was asked first, and the second project would report "Nobody new."
about a source that is still not installed. `gws` is still not on `PATH` on the
eleventh lookup, so the eleventh lookup may still say so.

**What v1 does not do.** The lane does not scroll to the selection: arrowing onto the
fourteenth project expands it, and the operator still has to scroll to it. It cannot
edit `me` — those addresses are shown in their own
band and carried through the round trip untouched. It has no search, no images, and no
icons beyond text glyphs.

**What v2 would add.** Scroll-to-selection, so a keyboard walk down a collapsed lane
brings the card it lands on into view; an editable `me`; selection, mouse caret
placement and clipboard in the text field; registered Actions with a visible shortcut
list; a filesystem watcher; keyboard reach for the chips, the suggestion rows and the
toolbar; a native folder picker behind `Sync folders…`; a virtualized canvas with
derived card bounds if the file ever grows past thousands.
