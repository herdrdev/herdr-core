#![cfg(unix)] // Windows: tests/windows.rs

//! Kitty images: what a viewer gets however and whenever it arrives.

mod common;

use common::framed::Viewer;
use common::*;
use ghostty_vt::testing::{chunked, noise, placed, placeholder_cells, png_filled, same_image};
use herdr_core::wire;
use serde_json::json;
use std::time::Duration;

/// A program that waits for `go` before doing `then`.
fn after_go(s: &Session, then: &str) -> String {
    format!("while [ ! -e {} ]; do sleep 0.02; done; {then}", s.root.join("go").display())
}

fn go(s: &Session) {
    std::fs::write(s.root.join("go"), "").unwrap();
}

fn ids(v: &Viewer) -> Vec<u32> {
    placed(&v.screen).iter().map(|p| p.0).collect()
}
/// A program that prints `bytes` once `go` exists, then DONE, then waits.
fn printer(s: &Session, bytes: &[u8]) -> String {
    let file = s.root.join("emit");
    std::fs::write(&file, bytes).unwrap();
    let flood = after_go(s, &format!("cat {}; printf 'DONE\\r\\n'; sleep 100", file.display()));
    run(s, &["sh", "-c", &flood])
}
/// A program that sends `first`, then `then` once `go` exists, and leaves the terminal raw.
fn script(s: &Session, first: &[u8], then: &[u8]) -> String {
    let (first_file, second_file) = (s.root.join("first"), s.root.join("second"));
    std::fs::write(&first_file, first).unwrap();
    std::fs::write(&second_file, then).unwrap();
    let then = format!("cat {}", second_file.display());
    let program = format!("stty raw -echo; cat {}; {}; cat >/dev/null", first_file.display(), after_go(s, &then));
    run(s, &["sh", "-c", &program])
}
fn printed(s: &Session, terminal: &str, needle: &str) {
    wait_until(needle, || s.json(&["read", terminal])["text"].as_str().unwrap_or("").contains(needle));
}
fn watch(s: &Session, terminal: &str) -> Viewer {
    Viewer::attach(s, terminal, 80, 24, json!({}))
}
fn observe(s: &Session, terminal: &str) -> Viewer {
    Viewer::attach(s, terminal, 80, 24, json!({"observe": true}))
}
/// Gives back all the credit a viewer was holding.
fn grant(v: &mut Viewer) {
    v.credit = true;
    v.conn.send(v.channel, wire::CREDIT, &(64u32 << 20).to_le_bytes());
}
fn mixed() -> Vec<u8> {
    let mut b = vec![];
    b.extend(b"direct:\r\n");
    b.extend(chunked("a=T,f=100,i=1,c=8,r=2,q=2", &png_filled(16, 8, [255, 0, 0, 255])));
    b.extend(b"\r\nunsized:\r\n");
    b.extend(chunked("a=T,f=100,i=4,q=2", &png_filled(100, 80, [9, 9, 200, 255])));
    b.extend(b"\r\nimplicit: ");
    b.extend(chunked("a=T,f=100,c=2,r=1,C=1,q=2", &png_filled(4, 2, [0, 0, 255, 255])));
    b.extend(b"\r\nplaceholder:\r\n");
    b.extend(chunked("a=T,f=100,U=1,i=2,c=4,r=1,q=2", &png_filled(8, 2, [0, 255, 0, 255])));
    b.extend(placeholder_cells().as_bytes());
    b.extend(b"\r\nrgb:\r\n");
    b.extend(chunked("a=T,f=24,s=2,v=2,i=3,c=2,r=2,q=2", &[9u8; 12]));
    b.extend(b"\r\n");
    b
}

#[test]
fn a_late_viewer_sees_what_an_early_viewer_saw() {
    let s = Session::new();
    let t = printer(&s, &mixed());
    let mut early = watch(&s, &t);
    early.drain(Duration::from_millis(100));
    go(&s);
    early.shows("DONE");
    let mut late = observe(&s, &t);
    late.shows("DONE");
    late.until("its images", |v| placed(&v.screen).len() == 5);
    assert_eq!(placed(&early.screen), placed(&late.screen));
    assert_eq!(early.screen.screen_rows(), late.screen.screen_rows(), "text in the same places");
    for id in [1, 2, 3, 4] {
        let (shown, replayed) = (early.screen.image_pixels(id).unwrap(), late.screen.image_pixels(id).unwrap());
        assert!(same_image(&shown, &replayed), "pixels of {id}");
    }
}

#[test]
fn a_viewer_attached_after_the_program_printed_alone_gets_its_images() {
    let s = Session::new();
    let t = printer(&s, &mixed());
    go(&s);
    printed(&s, &t, "DONE");
    let mut late = watch(&s, &t);
    late.shows("DONE");
    assert_eq!(ids(&late).iter().filter(|id| [1, 2, 3, 4].contains(id)).count(), 4, "{:?}", placed(&late.screen));
}

#[test]
fn a_large_first_paint_outlives_chatty_output() {
    let s = Session::new();
    // Over the backlog a viewer may fall behind by, so only the paint's own allowance gets it through.
    let mut b = chunked("a=T,f=32,s=1000,v=825,i=1,c=8,r=2,q=2", &noise(3_300_000));
    b.extend(b"\r\n");
    let file = s.root.join("emit");
    std::fs::write(&file, &b).unwrap();
    // On one line: scrolling would rightly take the image off the screen.
    let program = format!("cat {}; while :; do printf 'tick\\r'; sleep 0.002; done", file.display());
    let t = run(&s, &["sh", "-c", &program]);
    printed(&s, &t, "tick");
    let mut v = watch(&s, &t);
    v.until("the image", |v| !v.screen.image_placements().is_empty());
    assert_eq!(v.screen.image_pixels(1).map(|p| p.len()), Some(3_300_000));
    assert!(v.received < 6 << 20, "painted more than once: {}", v.received);
}

#[test]
fn a_viewer_gets_the_text_before_a_large_direct_image_arrives() {
    let s = Session::new();
    let mut b = b"HELLO\r\n".to_vec();
    b.extend(chunked("a=T,f=32,s=800,v=750,i=1,c=8,r=2,q=2", &noise(2_400_000)));
    b.extend(b"\r\n");
    let t = printer(&s, &b);
    go(&s);
    printed(&s, &t, "DONE");
    // Never gives credit back: all it ever gets is the first window of the paint.
    let mut v = watch(&s, &t);
    v.credit = false;
    v.shows("HELLO");
    assert!(v.received <= wire::WINDOW);
    assert!(ids(&v).is_empty(), "the image comes after");
    grant(&mut v);
    v.until("the image", |v| !v.screen.image_placements().is_empty());
}

#[test]
fn an_image_too_big_for_a_paint_leaves_a_viewer_its_text_and_the_others_that_fit() {
    let s = Session::new();
    let mut b = b"HELLO\r\n".to_vec();
    b.extend(chunked("a=T,f=32,s=1000,v=1500,i=1,c=8,r=2,q=2", &noise(6_000_000)));
    b.extend(b"\r\n");
    b.extend(chunked("a=T,f=100,i=2,c=4,r=1,q=2", &png_filled(8, 2, [0, 255, 0, 255])));
    b.extend(b"\r\n");
    let t = printer(&s, &b);
    let mut live = observe(&s, &t);
    go(&s);
    // Under load it can fall behind too, and then it's in the late viewer's position.
    live.shows("DONE");
    let mut late = watch(&s, &t);
    late.shows("HELLO");
    late.until("the small image", |v| ids(v) == [2]);
}

#[test]
fn a_viewer_told_only_the_last_size_of_a_drag_has_no_image_the_screen_lost_on_the_way() {
    let s = Session::new();
    // A full window with a picture near its top, as after `icat x.png; ls`.
    let mut first = b"$ a\r\n$ b\r\n$ icat\r\n".to_vec();
    first.extend(chunked("a=T,f=100,i=1,c=4,r=1,q=2", &png_filled(8, 4, [255, 0, 0, 255])));
    first.extend(b"\r\n");
    first.extend((0..19).flat_map(|i| format!("below {i}\r\n").into_bytes()));
    first.extend(b"FIRST");
    let t = script(&s, &first, b"");
    let mut leader = watch(&s, &t);
    let mut observer = Viewer::attach(&s, &t, 80, 24, json!({"observe": true, "resize_notifications": true}));
    leader.shows("FIRST");
    observer.shows("FIRST");
    observer.until("the image", |v| v.screen.image_placements().len() == 1);
    observer.drain(Duration::from_millis(100));
    for (w, h) in [(78u16, 18u16), (76, 22)] {
        leader.conn.send(leader.channel, wire::RESIZE, &[w.to_le_bytes(), h.to_le_bytes()].concat());
    }
    observer.drain(Duration::from_millis(600));
    let visible = |v: &Viewer| v.screen.image_placements().iter().filter(|p| p.visible).count();
    let mut late = Viewer::attach(&s, &t, 76, 22, json!({"observe": true}));
    late.drain(Duration::from_millis(300));
    assert_eq!(visible(&late), 0, "or this isn't the case it's about");
    assert_eq!(visible(&observer), 0);
}
