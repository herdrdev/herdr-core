#![cfg(unix)]

mod common;

use common::framed::Viewer;
use common::*;
use herdr_core::wire;
use serde_json::json;

/// What the app says when the person's window gains or loses the OS focus.
fn focus(v: &mut Viewer, focused: bool) {
    v.conn.send(v.channel, wire::FOCUS, &[focused as u8]);
}

fn resize(v: &mut Viewer, cols: u16, rows: u16) {
    let size = [cols.to_le_bytes(), rows.to_le_bytes()].concat();
    v.conn.send(v.channel, wire::RESIZE, &size);
}

fn size(s: &Session, t: &str) -> (u16, u16) {
    let got = s.json(&["get", t]);
    (
        got["cols"].as_u64().unwrap_or_else(|| panic!("{got}")) as u16,
        got["rows"].as_u64().unwrap_or_else(|| panic!("{got}")) as u16,
    )
}

#[test]
fn a_viewer_the_person_isnt_in_never_resizes_the_program() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let mut in_window = Viewer::attach(&s, &t, 80, 24, json!({}));
    focus(&mut in_window, true);
    let mut background = Viewer::attach(&s, &t, 120, 40, json!({}));
    focus(&mut background, false);

    // The background window was laid out, so it reports its pane's grid.
    resize(&mut background, 100, 30);
    assert_eq!(size(&s, &t), (80, 24), "a viewer the person isn't in must not resize the program");

    // Focusing it is what moves the size, and it moves to the size it asked for.
    focus(&mut background, true);
    assert_eq!(size(&s, &t), (100, 30), "the focused viewer's size is the program's");

    // Typing in the other window is the person speaking: it takes authority
    // back, with the size that window holds.
    in_window.typed(b"");
    assert_eq!(size(&s, &t), (80, 24), "typing keeps the size of the window typed in");
}

#[test]
fn a_viewer_that_never_reports_focus_keeps_its_authority() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let mut cli = Viewer::attach(&s, &t, 80, 24, json!({}));

    // `hc attach` reports the outer terminal's size and never says a word
    // about focus: for the one viewer, every size still applies as before.
    resize(&mut cli, 100, 30);
    assert_eq!(size(&s, &t), (100, 30), "a viewer that never reports focus is the person's own");

    let mut watcher = Viewer::attach(&s, &t, 40, 12, json!({}));
    focus(&mut watcher, false);
    resize(&mut cli, 90, 36);
    assert_eq!(size(&s, &t), (90, 36), "an unfocused viewer must not take the size from a CLI viewer");
}

#[test]
fn a_background_window_joining_leaves_the_program_the_size_it_was() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let mut in_window = Viewer::attach(&s, &t, 80, 24, json!({}));
    focus(&mut in_window, true);

    // A second app instance starts and attaches its panes; its window is in
    // the background from its first frame.
    let mut late = Viewer::attach(&s, &t, 120, 40, json!({}));
    focus(&mut late, false);
    assert_eq!(size(&s, &t), (80, 24), "a background window joining must not re-wrap the program");
}

#[test]
fn a_window_reconnecting_in_the_background_hands_the_size_back() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);

    // The person's app window reconnected to the core, so it has not claimed
    // activity yet: a resumed attach says nothing about focus.
    let mut app = Viewer::attach(&s, &t, 282, 65, json!({"resumed": true}));

    // A second instance starts and attaches the same program, its window in
    // the background from its first frame.
    let mut late = Viewer::attach(&s, &t, 132, 54, json!({}));
    focus(&mut late, false);
    assert_eq!(size(&s, &t), (282, 65), "a reconnecting window keeps the size it was showing");

    resize(&mut late, 90, 40);
    assert_eq!(size(&s, &t), (282, 65), "the background window's layout must not re-wrap the program");

    // The person comes back to their own window and it gets laid out: the
    // size of the window they are in counts again.
    resize(&mut app, 200, 60);
    assert_eq!(size(&s, &t), (200, 60), "the window the person is in sizes the program");
}
