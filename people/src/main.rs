//! `nashcode-people`: the desktop editor for `~/.nashcode/people.json`.
//!
//! One window, one file, one picture: three lanes of cards — every address, every
//! person, every project — and a wire wherever the file says one belongs to the next.
//! The file is the only source of truth — the router, the email pusher, and the CLI
//! all read the same path — so this app writes it whole or not at all and never keeps
//! a second copy.
//!
//! The bootstrap is the reference application's: `Application::new().run`, one window
//! at a documented default size, one entity in it. `gpui-component` would supply
//! `Root`, a theme, and the controls; it pins Zed's `gpui`, which cannot coexist with
//! the `gpui-ce` this builds on, so the theme is in `theme.rs` and the controls are
//! in `widgets.rs`.

mod app;
mod board;
mod edit;
mod inspector;
mod lanes;
mod links;
mod store;
mod theme;
mod widgets;

use app::PeopleApp;
use gpui::{
    Application, Bounds, Pixels, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*,
    px, size,
};
use theme::Theme;

/// 1280×780. The window is four columns wide: three lanes of cards — 232, 200 and 240
/// points — the gaps the wires are drawn in, and an inspector that needs about 336
/// points before a label and its field stop sharing a line. 1040×620 is the smallest
/// window in which all four still work; below that the inspector would win its space
/// from the lanes, and the picture is the point of the window.
const WINDOW: (f32, f32) = (1280., 780.);
const MINIMUM: (f32, f32) = (1040., 620.);

fn main() {
    let path = people_core::default_path();

    Application::new().run(move |cx| {
        // Before any view: a view reads `cx.theme()` while it is being built.
        cx.set_global(Theme::dark());

        let bounds = centred(cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // A transparent titlebar lets the window's own background run to the
                // top of the frame, so the strip above the toolbar is the same dark
                // surface as everything under it rather than a system-grey band with
                // a dark picture below it. The traffic lights then sit over the
                // toolbar, which is why the toolbar reserves `app::TITLEBAR` at its
                // top. The title itself is hidden by AppKit in this mode; the window
                // still carries it, for the menu bar and the window list.
                titlebar: Some(TitlebarOptions {
                    title: Some("People".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_min_size: Some(size(px(MINIMUM.0), px(MINIMUM.1))),
                focus: true,
                show: true,
                ..Default::default()
            },
            |_, cx| cx.new(|cx| PeopleApp::new(path.clone(), cx)),
        );

        match window {
            // The window owns the keyboard from the first frame, so ⌘S works before
            // anything has been clicked.
            Ok(window) => {
                let _ = window.update(cx, |view, window, cx| {
                    view.take_focus(window, cx);
                });
            }
            Err(error) => {
                eprintln!("nashcode-people: the window did not open: {error}");
                cx.quit();
            }
        }
    });
}

/// The window's opening rectangle, centred on the primary display. A display that
/// cannot be read is not a reason not to open: a legal rectangle is enough.
fn centred(cx: &gpui::App) -> Bounds<Pixels> {
    let (w, h) = (px(WINDOW.0), px(WINDOW.1));
    let screen = cx.primary_display().map(|display| display.bounds()).unwrap_or(Bounds::new(
        point(px(0.), px(0.)),
        size(px(1440.), px(900.)),
    ));
    Bounds::new(
        point(
            // Pixels multiplies by f32 but does not divide, hence * 0.5.
            screen.origin.x + (screen.size.width - w) * 0.5,
            screen.origin.y + (screen.size.height - h) * 0.5,
        ),
        size(w, h),
    )
}
