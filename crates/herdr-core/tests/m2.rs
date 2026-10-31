#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::{Framed, Viewer};
use common::*;
use herdr_core::wire;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn size(s: &Session, t: &str) -> (u64, u64) {
    let r = s.json(&["get", t]);
    (r["cols"].as_u64().unwrap(), r["rows"].as_u64().unwrap())
}

/// A program that waits for `go` before doing `then`.
fn after_go(s: &Session, then: &str) -> String {
    format!("while [ ! -e {} ]; do sleep 0.02; done; {then}", s.root.join("go").display())
}

fn go(s: &Session) {
    std::fs::write(s.root.join("go"), "").unwrap();
}

#[test]
fn first_paint_then_live_bytes_and_typing() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "echo first; while read l; do echo got-$l; done"]);
    wait_until("first", || s.json(&["read", &t])["text"].as_str().unwrap().contains("first"));

    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.shows("first");
    v.typed(b"hello\r");
    v.shows("got-hello");
}

#[test]
fn a_late_viewer_gets_the_win32_input_request_the_program_made_at_start() {
    // ConPTY asks its viewer for win32-input-mode once, at start; the screen can't hold that.
    let s = Session::new();
    let off = s.root.join("off");
    let program = after_go(
        &s,
        &format!(
            "printf '\\033[?9001h'; echo asked; while [ ! -e {} ]; do sleep 0.02; done; \
             printf '\\033[?9001l'; echo released; sleep 100",
            off.display()
        ),
    );
    let t = run(&s, &["sh", "-c", &program]);
    let mode = |seen: &str| {
        let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
        v.shows(seen);
        let raw = String::from_utf8_lossy(&v.raw).into_owned();
        raw.rfind("\x1b[?9001").and_then(|at| raw[at + 7..].chars().next())
    };
    go(&s);
    wait_until("asked", || s.json(&["read", &t])["text"].as_str().unwrap().contains("asked"));
    assert_eq!(mode("asked"), Some('h'));
    s.json(&["server", "restart"]);
    assert_eq!(mode("asked"), Some('h'), "a restart keeps it");
    // Turned off while no core runs: the next one learns it from what it missed.
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    unsafe {
        libc::kill(core as i32, libc::SIGKILL);
    }
    wait_until("core gone", || !s.connect_ok());
    std::fs::write(&off, "").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    wait_until("released", || s.json(&["read", &t])["text"].as_str().unwrap().contains("released"));
    assert_eq!(mode("released"), Some('l'));
}

#[test]
fn queries_are_answered_once_by_the_core_with_the_viewers_colors() {
    let s = Session::new();
    // Prints the raw replies it got to DA1 and a background color query.
    let probe = after_go(
        &s,
        "stty raw -echo min 0 time 2; printf '\\033[c\\033]11;?\\033\\\\'; r=''; \
         for i in 1 2 3 4 5; do r=\"$r$(dd bs=256 count=1 2>/dev/null)\"; done; \
         stty sane; printf '%s' \"$r\" | od -An -c | tr -d ' \\n'; echo; echo END; sleep 100",
    );
    let t = run(&s, &["sh", "-c", &probe]);
    let mut v = Viewer::attach(&s, &t, 100, 30, json!({"display": {"bg": "#ffffff"}}));
    go(&s);
    v.shows("END");
    let text = v.text();
    assert_eq!(text.matches("[?62;22c").count(), 1, "{text}");
    assert_eq!(text.matches("ffff/ffff/ffff").count(), 1, "{text}");
    let raw = String::from_utf8_lossy(&v.raw);
    assert!(!raw.contains("\x1b[c") && !raw.contains("]11;?"), "queries reached the viewer: {raw:?}");

    // Nobody attached: Ghostty's default background.
    let t2 = run(&s, &["sh", "-c", &probe.replace("/go", "/go2")]);
    std::fs::write(s.root.join("go2"), "").unwrap();
    wait_until("headless probe", || s.json(&["read", &t2])["text"].as_str().unwrap().contains("END"));
    assert!(s.json(&["read", &t2])["text"].as_str().unwrap().contains("2828/2c2c/3434"));
}

#[test]
fn display_changes_notify_an_idle_subscribed_program() {
    let s = Session::new();
    let received = s.root.join("replies");
    let probe = script(
        &s,
        &format!(
            "w go; stty raw -echo; (w query; printf '\\033]10;?\\007\\033]11;?\\007\\033[?996n') & \
             printf '\\033[?2031h'; echo READY; cat > {}",
            received.display()
        ),
    );
    let t = run(&s, &["sh", "-c", &probe]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({"display": {"fg": "#eeeeee", "bg": "#000000", "cell": [9, 18]}}));
    go(&s);
    v.shows("READY");
    let replies = || std::fs::read(&received).unwrap_or_default();
    // Do not print anything from the program to flush the notification.
    v.conn.send(v.channel, wire::FACTS, br##"{"fg":"#010203","bg":"#ffffff"}"##);
    wait_until("light notification at the PTY", || replies() == b"\x1b[?997;2n");
    v.conn.send(v.channel, wire::FACTS, br##"{"bg":"#000000"}"##);
    wait_until("dark notification at the PTY", || replies() == b"\x1b[?997;2n\x1b[?997;1n");

    v.conn.send(v.channel, wire::FACTS, br##"{"bg":"#000000"}"##);
    v.conn.send(v.channel, wire::FACTS, b"{}");
    v.conn.call("terminal.get", json!({"terminal": t}));

    // Omitted foreground facts survive both the update and a leadership change.
    let other = Viewer::attach(&s, &t, 80, 24, json!({"display": {"fg": "#abcdef"}}));
    v.conn.send(v.channel, wire::FOCUS, &[1]);
    v.conn.call("terminal.get", json!({"terminal": t}));
    touch(&s, "query");
    let expected = b"\x1b[?997;2n\x1b[?997;1n\x1b[?997;1n\x1b[?997;1n\x1b]10;rgb:0101/0202/0303\x07\x1b]11;rgb:0000/0000/0000\x07\x1b[?997;1n";
    wait_until("queries answered at the PTY without extra notifications", || replies() == expected);
    drop(other);
}

#[test]
fn in_band_resize_reports_belong_to_the_viewer_not_the_core() {
    let s = Session::new();
    let probe = script(
        &s,
        "stty raw -echo min 0 time 2; printf '\\033[?2048h'; echo READY; w resized; \
         echo OUTPUT; r=''; for i in 1 2 3; do r=\"$r$(dd bs=256 count=1 2>/dev/null)\"; done; \
         printf '%s' \"$r\" | od -An -tx1; echo END; sleep 100",
    );
    let t = run(&s, &["sh", "-c", &probe]);
    wait_until("2048 enabled", || text(&s, &t).contains("READY"));
    let mut v = Viewer::attach(&s, &t, 100, 30, json!({}));
    v.conn.send(v.channel, wire::RESIZE, &[90, 0, 20, 0]);
    wait_until("resized", || size(&s, &t) == (90, 20));
    touch(&s, "resized");
    v.shows("END");
    assert!(!v.text().contains("1b"), "core injected a resize report: {}", v.text());
}

#[test]
fn size_follows_the_last_active_typing_viewer() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "trap 'stty size' WINCH; while :; do sleep 0.05; done"]);
    assert_eq!(size(&s, &t), (200, 120));

    let mut a = Viewer::attach(&s, &t, 100, 30, json!({}));
    assert_eq!(size(&s, &t), (100, 30));
    a.shows("30 100");
    let mut b = Viewer::attach(&s, &t, 80, 24, json!({}));
    assert_eq!(size(&s, &t), (80, 24));
    a.conn.send(a.channel, wire::FOCUS, &[1]);
    wait_until("a leads again", || size(&s, &t) == (100, 30));

    let observer = Viewer::attach(&s, &t, 50, 10, json!({"observe": true}));
    assert_eq!(size(&s, &t), (100, 30));
    b.conn.send(b.channel, wire::RESIZE, &[90, 0, 20, 0]);
    wait_until("b resized", || size(&s, &t) == (90, 20));

    drop(a);
    drop(b);
    wait_until("headless again", || size(&s, &t) == (200, 120));
    drop(observer);
}

#[test]
fn a_stalled_viewer_blocks_nobody_and_catches_up_with_a_repaint() {
    let s = Session::new();
    let flood = after_go(&s, "head -c 8000000 /dev/zero | tr '\\0' x; echo; echo DONE; sleep 100");
    let t = run(&s, &["sh", "-c", &flood]);
    let mut stalled = Viewer::attach(&s, &t, 80, 24, json!({}));
    stalled.credit = false;
    let mut live = Viewer::attach(&s, &t, 80, 24, json!({"observe": true}));
    go(&s);
    live.shows("DONE");

    // It never gets more than the credit it gave.
    stalled.drain(Duration::from_millis(300));
    assert!(stalled.received <= wire::WINDOW, "sent past credit: {}", stalled.received);

    // Its stream is stuck, but calls on its own connection still answer at once.
    let started = Instant::now();
    assert!(stalled.conn.call("server.status", json!({}))["result"]["pid"].is_number());
    assert!(started.elapsed() < Duration::from_secs(1));

    stalled.credit = true;
    stalled.conn.send(stalled.channel, wire::CREDIT, &(64u32 << 20).to_le_bytes());
    stalled.shows("DONE");
    assert!(stalled.received < 6 << 20, "replayed the whole backlog: {} bytes", stalled.received);
}

#[test]
fn viewers_learn_when_the_program_ends() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "read x; exit 7"]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.typed(b"\r");
    v.until("close", |v| v.closed.is_some());
    assert_eq!(v.closed.as_ref().unwrap()["exit_code"], 7);

    let reply = Framed::open(&s).call("terminal.attach", json!({"terminal": t, "cols": 80, "rows": 24}));
    assert_eq!(reply["error"]["data"]["kind"], "finished");
    let plain = s.connect().call(1, "terminal.attach", json!({"terminal": t, "cols": 80, "rows": 24}));
    assert_eq!(plain["error"]["data"]["kind"], "needs_framed");
}

#[test]
fn rm_without_a_target_offers_what_can_be_removed() {
    let s = Session::new();
    run(&s, &["--name", "alive", "sleep", "100"]);
    let done = run(&s, &["--name", "over", "true"]);
    wait_until("finished", || s.json(&["get", &done])["state"] == "finished");
    let (_, err) = s.err(&["rm"]);
    let message = err["error"]["message"].as_str().unwrap();
    assert!(message.contains("over") && !message.contains("alive"), "{message}");
}

#[test]
fn without_a_target_pipes_get_candidates_and_terminals_get_a_picker() {
    let s = Session::new();
    let a = run(&s, &["--name", "alpha", "sh", "-c", "echo i-am-alpha; sleep 100"]);
    let b = run(&s, &["--name", "beta", "sh", "-c", "echo i-am-beta; sleep 100"]);
    let (code, err) = s.err(&["get"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (7, Some("needs_target")));
    let message = err["error"]["message"].as_str().unwrap();
    assert!(message.contains(&a) && message.contains("beta"), "{message}");
    assert_eq!(s.json(&[]).get("terminals").unwrap().as_array().unwrap().len(), 2);

    let bin = s.bin.display().to_string();
    let picker = run(&s, &[&bin, "attach"]);
    let mut v = Viewer::attach(&s, &picker, 80, 24, json!({}));
    v.shows("alpha");
    v.typed(b"bet");
    v.until("filtered", |v| !v.text().contains("alpha"));
    v.typed(b"\r");
    v.until("attached", |v| !v.text().contains("which terminal?"));
    v.shows("i-am-beta");
    v.typed(b"\x1c");
    v.until("client exit", |v| v.closed.is_some());
    assert!(s.json(&["read", &picker])["text"].as_str().unwrap().contains(&format!("detached from {b}")));

    let cancelled = run(&s, &[&bin, "kill"]);
    let mut v = Viewer::attach(&s, &cancelled, 80, 24, json!({}));
    v.shows("alpha");
    v.typed(b"\x1b");
    v.until("cancel", |v| v.closed.is_some());
    assert_eq!(v.closed.as_ref().unwrap()["exit_code"], 130);
    assert_eq!(s.json(&["get", "alpha"])["state"], "live");
}

#[test]
fn bare_hc_in_a_terminal_can_start_a_new_shell() {
    let s = Session::new();
    let bin = s.bin.display().to_string();
    let outer = run(&s, &[&bin]);
    let mut v = Viewer::attach(&s, &outer, 80, 24, json!({}));
    v.shows("new");
    v.typed(b"\r");
    wait_until("new shell", || s.json(&["ls"])["terminals"].as_array().unwrap().len() == 2);
    v.typed(b"echo hi-from-$HC_TERMINAL_ID\r");
    v.shows("hi-from-t2");
}

#[test]
fn the_detach_key_comes_from_the_config_file() {
    let s = Session::new();
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "detach_key = \"ctrl+]\"\n").unwrap();
    let inner = run(&s, &["sh", "-c", BYTES]);
    let bin = s.bin.display().to_string();
    let outer = run(&s, &[&bin, "attach", &inner]);
    let mut v = Viewer::attach(&s, &outer, 80, 24, json!({}));
    v.typed(b"\x1c");
    v.shows("1c");
    v.typed(b"\x1d");
    v.until("client exit", |v| v.closed.is_some());

    std::fs::write(&config, "detach_key = \"alt+x\"\n").unwrap();
    let (code, err) = s.err(&["attach", &inner]);
    assert_eq!((code, err["error"]["kind"].as_str()), (8, Some("config")));
    assert!(err["error"]["message"].as_str().unwrap().contains("detach_key"));
}

#[test]
fn a_detach_key_split_across_reads_still_detaches() {
    if slow_runner() {
        return eprintln!("skipped: the 50 ms to finish a key needs a runner that keeps time");
    }
    let s = Session::new();
    let inner = run(&s, &["sh", "-c", "stty raw -echo; echo inner-ready; cat > /dev/null"]);
    let bin = s.bin.display().to_string();
    let outer = run(&s, &[&bin, "attach", &inner]);
    let mut v = Viewer::attach(&s, &outer, 90, 25, json!({}));
    v.shows("inner-ready");
    // Kitty's ctrl+\ in two reads, the way a slow link can deliver it. The gap
    // stays well inside the 50 ms the rest is waited for: busy CI runners add
    // their own delay on top.
    v.typed(b"\x1b[92;");
    std::thread::sleep(Duration::from_millis(10));
    v.typed(b"5u");
    v.until("client exit", |v| v.closed.is_some());
    assert!(s.json(&["read", &outer])["text"].as_str().unwrap().contains("detached"));
}

#[test]
fn a_terminal_cannot_attach_to_itself() {
    // It would show its own output inside itself, forever.
    let s = Session::new();
    let bin = s.bin.display().to_string();
    let t = run(&s, &["sh", "-c", &format!("{bin} attach $HC_TERMINAL_ID; echo code-$?; sleep 100")]);
    wait_until("refused", || s.json(&["read", &t])["text"].as_str().unwrap().contains("code-"));
    let text = s.json(&["read", &t])["text"].as_str().unwrap().to_string();
    assert!(text.contains("itself") && !text.contains("code-0"), "{text}");
}

#[test]
fn a_late_reply_from_the_viewers_terminal_never_reaches_the_program() {
    let s = Session::new();
    let got = s.root.join("got");
    let inner = run(&s, &["sh", "-c", &format!("stty raw -echo; echo inner-ready; cat > {}", got.display())]);
    let bin = s.bin.display().to_string();
    let outer = run(&s, &[&bin, "attach", &inner]);
    let mut v = Viewer::attach(&s, &outer, 90, 25, json!({}));
    v.shows("inner-ready");
    // A Windows console answers DA1 itself, so the cell size comes after it.
    v.typed(b"\x1b[6;20;10tx");
    wait_until("typed", || std::fs::read(&got).is_ok_and(|g| !g.is_empty()));
    v.typed(b"\x1c");
    v.until("client exit", |v| v.closed.is_some());
    assert_eq!(std::fs::read(&got).unwrap(), b"x");
}

#[test]
fn a_terminated_attach_gives_the_terminal_back() {
    let s = Session::new();
    let inner = run(&s, &["sleep", "100"]);
    let (bin, root) = (s.bin.display().to_string(), s.root.display().to_string());
    let script = format!("sh -c 'echo $$ > {root}/pid; exec {bin} attach {inner}'; stty -a > {root}/after; sleep 100");
    run(&s, &["sh", "-c", &script]);
    wait_until("attached", || std::fs::read_to_string(s.root.join("pid")).is_ok_and(|p| !p.is_empty()));
    std::thread::sleep(Duration::from_millis(500));
    let pid: i32 = std::fs::read_to_string(s.root.join("pid")).unwrap().trim().parse().unwrap();
    unsafe { libc::kill(pid, libc::SIGTERM) };
    wait_until("its shell resumed", || std::fs::read_to_string(s.root.join("after")).is_ok_and(|a| !a.is_empty()));
    let after = std::fs::read_to_string(s.root.join("after")).unwrap();
    assert!(!after.contains("-icanon") && !after.contains("-echo "), "left raw:\n{after}");
}

#[test]
fn the_picker_takes_text_and_enter_in_one_read() {
    let s = Session::new();
    run(&s, &["--name", "alpha", "sleep", "100"]);
    run(&s, &["--name", "beta", "sleep", "100"]);
    let bin = s.bin.display().to_string();
    let picker = run(&s, &[&bin, "get"]);
    let mut v = Viewer::attach(&s, &picker, 80, 24, json!({}));
    v.shows("alpha");
    v.typed(b"beta\r");
    v.until("picked and printed", |v| v.closed.is_some());
    assert!(s.json(&["read", &picker])["text"].as_str().unwrap().contains("name: beta"));
}

#[test]
fn hc_attach_relays_both_ways_and_detaches_on_ctrl_backslash() {
    let s = Session::new();
    let inner = run(&s, &["sh", "-c", "echo inner-ready; while read l; do echo inner-got-$l; done"]);
    // The attach client runs in a terminal of its own, which our viewer drives like a person.
    let bin = s.bin.display().to_string();
    let outer = run(&s, &[&bin, "attach", &inner]);
    let mut v = Viewer::attach(&s, &outer, 90, 25, json!({}));
    v.shows("inner-ready");
    wait_until("inner follows the client's size", || size(&s, &inner) == (90, 25));
    v.typed(b"abc\r");
    v.shows("inner-got-abc");

    v.typed(b"\x1c");
    v.until("client exit", |v| v.closed.is_some());
    assert_eq!(v.closed.as_ref().unwrap()["exit_code"], 0);
    assert!(s.json(&["read", &outer])["text"].as_str().unwrap().contains("detached"));
    let rec: Value = s.json(&["get", &inner]);
    assert_eq!(rec["state"], "live");
    wait_until("headless after detach", || size(&s, &inner) == (200, 120));
}
