//! Full-screen history reconstruction regression tests against a fake app on a real terminal.
#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::Viewer;
use common::*;
use serde_json::{Value, json};

/// A short read time limit makes the "never quiet" case fast; others get a real one.
fn session_with_limit(ms: &str) -> Session {
    let mut s = Session::new();
    s.env.push(("HC_STITCH_MS".into(), ms.into()));
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "headless = \"40x12\"\n").unwrap();
    s
}

fn session() -> Session {
    session_with_limit("8000")
}

/// Starts the fake app and waits until it has drawn.
fn app(s: &Session, flags: &[&str]) -> String {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_tui.py");
    let mut args = vec!["run", "--bg", "python3", script];
    args.extend_from_slice(flags);
    let t = s.json(&args)["address"].as_str().unwrap().to_string();
    wait_until("app drawn", || s.json(&["read", &t])["text"].as_str().unwrap().contains("status"));
    t
}

fn read(s: &Session, t: &str) -> Value {
    s.json(&["read", t, "--lines", "200"])
}

fn screen(s: &Session, t: &str) -> String {
    s.json(&["read", t])["text"].as_str().unwrap().to_string()
}

/// Every transcript line exactly once, in order.
fn whole(text: &str, n: usize) -> bool {
    let got: Vec<&str> = text.lines().filter(|l| l.starts_with("line ")).map(str::trim_end).collect();
    let want: Vec<String> = (1..=n).map(|i| format!("line {i:03}")).collect();
    got == want
}

#[test]
fn a_full_screen_agent_is_read_whole_and_left_where_it_was() {
    let s = session();
    let t = app(&s, &["--lines", "50"]);
    let before = screen(&s, &t);
    assert!(!before.contains("line 001"), "the start is off screen");
    let r = read(&s, &t);
    assert_eq!((r["how"].as_str(), r["complete"].as_bool()), (Some("stitched"), Some(true)), "{r}");
    assert!(whole(r["text"].as_str().unwrap(), 50), "{}", r["text"]);
    wait_until("view restored", || screen(&s, &t).contains("line 050"));

    // Nothing printed since: the same read comes from memory, without scrolling again.
    let wheels = |text: &str| text.split("wheels=").nth(1).and_then(|w| w.split_whitespace().next()).map(String::from);
    let after = wheels(&screen(&s, &t));
    let again = read(&s, &t);
    assert_eq!(again["text"], r["text"]);
    assert_eq!(wheels(&screen(&s, &t)), after);
}

#[test]
fn after_a_full_screen_agent_exits_its_history_is_never_called_complete() {
    let s = session();
    let t = app(&s, &["--lines", "50"]);
    s.json(&["keys", &t, "q"]);
    wait_until("exited", || s.json(&["get", &t])["state"] == "finished");
    let r = read(&s, &t);
    assert_eq!(r["complete"], false, "its own history went with it: {r}");
}

#[test]
fn pinned_headers_scrollbars_loading_frames_and_synchronized_output() {
    let s = session();
    let t = app(&s, &["--lines", "40", "--header", "--scrollbar", "--loading", "--sync"]);
    let r = read(&s, &t);
    assert_eq!((r["how"].as_str(), r["complete"].as_bool()), (Some("stitched"), Some(true)), "{r}");
    let text = r["text"].as_str().unwrap();
    assert!(whole(&text.lines().map(|l| l.trim_end_matches(['│', '┃'])).collect::<Vec<_>>().join("\n"), 40), "{text}");
    assert!(text.matches("pinned header").count() <= 1, "{text}");
    assert!(!text.contains("loading"), "{text}");
}

#[test]
fn repainting_blocked_and_error_prompts_are_read_without_scrolling() {
    let s = session_with_limit("15000");
    std::fs::write(s.root.join("config/herdr-core/config.toml"), "headless = \"200x120\"\n").unwrap();
    for state in ["blocked", "error"] {
        let flag = format!("--{state}");
        let t = app(&s, &["--lines", "10", "--blink", "600", "--fixed", &flag]);
        assert_eq!(s.json(&["get", &t])["status"]["state"], state);
        let started = std::time::Instant::now();
        let r = s.json(&["read", &t, "--lines", "30"]);
        let elapsed = started.elapsed();
        eprintln!("{state} prompt: {elapsed:?}, how={}, complete={}", r["how"], r["complete"]);
        assert_eq!((r["how"].as_str(), r["complete"].as_bool()), (Some("screen"), Some(false)), "{r}");
        assert_eq!(r["why"], state, "{r}");
        assert!(elapsed < std::time::Duration::from_secs(1), "took {elapsed:?}");
        assert!(whole(r["text"].as_str().unwrap(), 10), "{r}");
        let plain = String::from_utf8(s.hc(&["read", &t, "--lines", "30"]).stdout).unwrap();
        assert!(plain.contains(&format!("only its screen: it is {state}")), "{plain}");
        let after = screen(&s, &t);
        assert!(whole(&after, 10));
        assert!(after.contains("wheels=0 keys="), "a read must not send input: {after}");
        assert_eq!(s.json(&["get", &t])["status"]["state"], state);
    }
}

#[test]
fn slow_redraws_still_stitch() {
    let s = session();
    let t = app(&s, &["--lines", "30", "--slow", "60"]);
    let r = read(&s, &t);
    assert_eq!(r["complete"], true, "{r}");
    assert!(whole(r["text"].as_str().unwrap(), 30));
}

#[test]
fn a_very_slow_app_is_never_called_complete_early_and_is_put_back() {
    let s = session();
    // Slower than a scroll step (120 ms): the first steps show nothing yet.
    let t = app(&s, &["--lines", "20", "--slow", "200"]);
    let r = read(&s, &t);
    if r["complete"] == true {
        assert!(whole(r["text"].as_str().unwrap(), 20), "complete means all of it: {r}");
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(screen(&s, &t).contains("line 020"), "left scrolled up:\n{}", screen(&s, &t));
}

#[test]
fn a_short_read_does_not_answer_a_longer_one() {
    let s = session();
    let t = app(&s, &["--lines", "50"]);
    let short = s.json(&["read", &t, "--lines", "1"]);
    assert_eq!(short["complete"], true);
    assert!(!short["text"].as_str().unwrap().contains("line 001"));
    let r = read(&s, &t);
    assert_eq!(r["complete"], true, "{r}");
    assert!(whole(r["text"].as_str().unwrap(), 50), "{}", r["text"]);
}

#[test]
fn a_key_during_the_first_probe_still_puts_the_view_back() {
    let s = session();
    let t = app(&s, &["--lines", "50", "--slow", "60"]);
    let mut v = Viewer::attach(&s, &t, 40, 12, json!({}));
    v.shows("line 050");
    v.typed(&b"\x1b[<64;5;5M".repeat(6)); // a person scrolls up six lines
    // All six steps landed (a slow app plays them back one by one).
    wait_until("scrolled", || screen(&s, &t).starts_with("line 034"));
    let reader = {
        let mut cmd = s.cmd();
        cmd.args(["--json", "read", &t, "--lines", "200"]);
        std::thread::spawn(move || cmd.output().unwrap())
    };
    // Only once the probe has moved the view: a key before it tests nothing.
    wait_until("probing", || !screen(&s, &t).starts_with("line 034"));
    v.typed(b"x");
    reader.join().unwrap();
    wait_until("the key went in", || screen(&s, &t).contains("keys=x"));
    std::thread::sleep(std::time::Duration::from_millis(500));
    let now = screen(&s, &t);
    assert!(now.starts_with("line 034") && now.contains("line 044\n"), "not put back:\n{now}");
}

#[test]
fn any_doubt_returns_the_plain_screen_marked_incomplete() {
    let s = session_with_limit("1500");
    for flags in [&["--chatty"][..], &["--no-mouse"], &["--working"]] {
        let mut all = vec!["--lines", "50"];
        all.extend_from_slice(flags);
        let t = app(&s, &all);
        let r = read(&s, &t);
        assert_eq!((r["how"].as_str(), r["complete"].as_bool()), (Some("screen"), Some(false)), "{flags:?}: {r}");
        assert!(r["text"].as_str().unwrap().contains("line 050"), "{flags:?}");
        // Only "it's working" is worth waiting for: say so, and when.
        let working = flags == ["--working"];
        assert_eq!(r["why"].as_str(), working.then_some("working"), "{flags:?}: {r}");
        let plain = String::from_utf8(s.hc(&["read", &t, "--lines", "200"]).stdout).unwrap();
        assert_eq!(plain.contains("read again once it stops"), working, "{flags:?}: {plain}");
        wait_until("still at the bottom", || screen(&s, &t).contains("line 050"));
    }
}

#[test]
fn a_view_someone_scrolled_up_is_left_alone() {
    let s = session();
    let lines = |text: &str| text.lines().filter(|l| l.starts_with("line ")).map(String::from).collect::<Vec<_>>();
    // Six lines up, and one: less than the probe would move.
    for (notches, last) in [(6, "line 044"), (1, "line 049")] {
        let t = app(&s, &["--lines", "50"]);
        let mut v = Viewer::attach(&s, &t, 40, 12, json!({}));
        v.shows("line 050");
        v.typed(&b"\x1b[<64;5;5M".repeat(notches));
        wait_until("every wheel notch rendered", || lines(&screen(&s, &t)).last().is_some_and(|l| l == last));
        let before = lines(&screen(&s, &t));
        assert!(before.last().is_some_and(|l| l == last), "{before:?}");
        let started = std::time::Instant::now();
        let r = read(&s, &t);
        assert_eq!((r["how"].as_str(), r["complete"].as_bool()), (Some("screen"), Some(false)), "{r}");
        assert!(started.elapsed() < std::time::Duration::from_secs(4), "{notches}: took {:?}", started.elapsed());
        wait_until("left where the person put it", || lines(&screen(&s, &t)) == before);
    }
}

#[test]
fn viewers_never_see_the_scrolling_and_a_persons_key_wins() {
    let s = session();
    // A whole read scrolls to the top and back; the viewer only gets the repaint.
    let fast = app(&s, &["--lines", "50"]);
    let mut v = Viewer::attach(&s, &fast, 40, 12, json!({}));
    v.shows("line 050");
    let raw_before = v.raw.len();
    assert_eq!(read(&s, &fast)["complete"], true);
    v.drain(std::time::Duration::from_millis(300));
    let seen = String::from_utf8_lossy(&v.raw[raw_before..]).to_string();
    assert!(!seen.contains("line 001"), "the viewer saw the scrolling");
    assert!(v.text().contains("line 050"));

    // A key someone presses mid-read stops it; the view is restored first.
    let t = app(&s, &["--lines", "50", "--slow", "60"]);
    let mut v = Viewer::attach(&s, &t, 40, 12, json!({}));
    v.shows("line 050");
    let reader = {
        let mut cmd = s.cmd();
        cmd.args(["--json", "read", &t, "--lines", "200"]);
        std::thread::spawn(move || serde_json::from_slice::<Value>(&cmd.output().unwrap().stdout).unwrap())
    };
    std::thread::sleep(std::time::Duration::from_millis(250));
    v.typed(b"x");
    let r = reader.join().unwrap();
    assert_eq!(r["complete"], false, "a key aborts the stitch: {r}");

    v.shows("keys=x");
    assert!(v.text().contains("line 050"), "the view was restored first:\n{}", v.text());
}
