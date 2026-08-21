//! `nashcode ls` parses dgit's index page. The fixture is a page rendered the
//! way dgit's `indexPage()` renders it (sections, entities, an idle-less row),
//! saved so the parser is tested against the real markup, not a sketch of it.

use dgit_index::parse;

const FIXTURE: &str = include_str!("fixtures/index.html");

#[test]
fn the_saved_dgit_page_parses_to_the_four_repos() {
    let repos = parse(FIXTURE);
    let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        ["deploy-scripts", "dotfiles", "widget.js", "plan-viewer"]
    );
}

#[test]
fn sections_carry_down_onto_their_rows() {
    let repos = parse(FIXTURE);
    assert_eq!(repos[0].section, "infra");
    assert_eq!(repos[1].section, "infra");
    assert_eq!(repos[2].section, "projects");
    assert_eq!(repos[3].section, "projects");
}

#[test]
fn entities_decode_and_no_description_becomes_empty() {
    let repos = parse(FIXTURE);
    assert_eq!(repos[0].description, "the box & its services");
    assert_eq!(repos[1].description, ""); // "[no description]" placeholder
    assert_eq!(repos[2].description, "an embeddable widget — drop-in");
    assert_eq!(repos[3].description, "renders plans/ <md> files");
}

#[test]
fn owners_and_idle_read_positionally_and_tolerate_blanks() {
    let repos = parse(FIXTURE);
    assert_eq!(repos[0].owner, "ops");
    assert_eq!(repos[0].idle, "3 days");
    assert_eq!(repos[3].idle, ""); // never pushed
}

#[test]
fn navigation_and_header_rows_never_become_repos() {
    let repos = parse(FIXTURE);
    assert!(repos.iter().all(|r| r.name != "cgit.css"));
    assert!(repos.iter().all(|r| !r.name.is_empty()));
}
