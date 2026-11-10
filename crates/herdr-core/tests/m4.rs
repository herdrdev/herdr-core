#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::Viewer;
use common::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn session() -> Session {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "600".into()));
    s.env.push(("HC_STARTED_MS".into(), "1500".into()));
    s
}

fn run(s: &Session, script: &str) -> String {
    let body = format!("r(){{ printf '\\033]7501;%s\\033\\\\' \"$1\"; }}; {script}");
    s.json(&["run", "--bg", "sh", "-c", &body])["address"].as_str().unwrap().to_string()
}

fn shows(s: &Session, t: &str, needle: &str) {
    wait_until(&format!("{needle:?} in {t}"), || text(s, t).contains(needle));
}

fn status(s: &Session, t: &str) -> Value {
    s.json(&["get", t])["status"]["state"].clone()
}

#[test]
fn a_send_into_a_starting_agent_waits_for_its_screen_to_settle() {
    let mut s = session();
    s.env.push(("HC_SETTLE_MS".into(), "800".into()));
    // Like Pi: it reports at once, then keeps setting up and loses what is typed meanwhile.
    let t = s.json(&[
        "run",
        "--bg",
        "bash",
        "-c",
        "stty -echo; printf '\\033]7501;state=idle\\033\\\\'; \
        for i in 1 2 3 4 5 6; do echo booting; sleep 0.1; while read -r -t 0.01 l; do :; done; done; \
        while read -r l; do echo \"got:$l\"; done",
    ]);
    let t = t["address"].as_str().unwrap().to_string();
    wait_until("reported", || s.json(&["get", &t])["reported"] == true);
    s.json(&["send", &t, "hello"]);
    shows(&s, &t, "got:hello");
}

#[test]
fn an_immediate_send_waits_even_before_the_first_output() {
    let mut s = session();
    s.env.push(("HC_SETTLE_MS".into(), "800".into()));
    let t = run(
        &s,
        "sleep 0.4; stty -echo; while read -r -t 0.01 l; do :; done; \
         r state=idle; while read -r l; do echo got:$l; r state=working; r state=done; done",
    );
    let sent = s.json(&["send", &t, "hello"]);
    assert_eq!(sent["started"], true);
    shows(&s, &t, "got:hello");
}

#[test]
fn a_send_refuses_a_starting_program_that_never_settles() {
    let mut s = session();
    s.env.push(("HC_SETTLE_MS".into(), "800".into()));
    let t = run(&s, "stty -echo; while :; do echo booting; sleep 0.1; done");
    shows(&s, &t, "booting");
    let (_, err) = s.err(&["send", &t, "hello"]);
    assert_eq!(err["error"]["kind"], "unavailable");
    assert!(err["error"]["hint"].as_str().unwrap().contains("nothing was typed"));
}

#[test]
fn send_pastes_submits_and_reports_whether_work_started() {
    let s = session();
    let agent = run(
        &s,
        "printf '\\033[?2004h'; stty -echo; r state=idle; \
         while read -r l; do r state=working; printf '%s' \"$l\" | od -An -c | tr -d ' \\n'; echo; r state=done; done",
    );
    wait_until("idle", || status(&s, &agent) == "idle");
    let r = s.json(&["send", &agent, "hello-world"]);
    assert_eq!((r["status"].as_str(), r["turn"].as_u64(), r["started"].as_bool()), (Some("idle"), Some(0), Some(true)));
    shows(&s, &agent, "033[200~hello-world033[201~");
    wait_until("turn 1", || s.json(&["get", &agent])["turn"] == 1);

    // Reports status but never starts working: false. Never reports: unknown (null).
    let lazy = run(&s, "stty -echo; r state=idle; while read -r l; do echo got; done");
    wait_until("idle", || status(&s, &lazy) == "idle");
    assert_eq!(s.json(&["send", &lazy, "x"])["started"], false);
    let silent = s.json(&["run", "--bg", "cat"])["address"].as_str().unwrap().to_string();
    assert!(s.json(&["send", &silent, "x"])["started"].is_null());
}

#[test]
fn heavy_traffic_both_ways_freezes_nothing() {
    let s = session();
    // Floods cursor-position queries (the core answers each one into its input,
    // which it never reads), while a 2 MB message goes in too.
    let flood =
        s.json(&["run", "--bg", "sh", "-c", "yes \"$(printf '\\033[6n')\""])["address"].as_str().unwrap().to_string();
    let mut conn = s.connect();
    let big = "x".repeat(2 << 20);
    let sending = std::thread::spawn(move || conn.call(1, "terminal.send", json!({"terminal": flood, "text": big})));
    std::thread::sleep(Duration::from_millis(1500));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut status = s.cmd();
    status.args(["server", "status"]);
    std::thread::spawn(move || tx.send(status.output().unwrap()));
    let answered = rx.recv_timeout(Duration::from_secs(5)).expect("the core stopped answering: frozen");
    assert!(answered.status.success());
    let _ = sending.join();
}

#[test]
fn a_bad_timeout_is_refused_before_anything_is_typed() {
    let s = session();
    let t = run(&s, "stty -echo; r state=idle; while read -r l; do echo \"got:$l\"; done");
    wait_until("idle", || status(&s, &t) == "idle");
    for bad in ["--timeout=-1", "--timeout=inf", "--timeout=nan"] {
        let (code, err) = s.err(&["send", &t, "hello", "--wait", bad]);
        assert_eq!((code, err["error"]["kind"].as_str()), (2, Some("usage")), "{bad}: {err}");
        assert_eq!(s.err(&["interrupt", &t, bad]).0, 2);
    }
    // Through the API too, where nothing stops a negative number earlier.
    let mut conn = s.connect();
    let r = conn.call(1, "terminal.send", json!({"terminal": t, "text": "hello", "wait": true, "timeout": -1}));
    assert_eq!(r["error"]["data"]["kind"], "invalid_params", "{r}");
    let r = conn.call(2, "terminal.interrupt", json!({"terminal": t, "timeout": -1}));
    assert_eq!(r["error"]["data"]["kind"], "invalid_params", "{r}");
    s.json(&["send", &t, "after"]);
    shows(&s, &t, "got:after");
    assert!(!text(&s, &t).contains("got:hello"), "nothing typed for a refused send");
}

#[test]
fn send_refuses_a_blocked_program_and_checks_status_atomically() {
    let s = session();
    let agent =
        run(&s, "r state=blocked:kind=permission; stty -echo; read -r l; echo answered:$l; r state=idle; sleep 100");
    wait_until("blocked", || status(&s, &agent) == "blocked");
    let (code, err) = s.err(&["send", &agent, "y"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (9, Some("blocked")));
    assert!(err["error"]["message"].as_str().unwrap().contains("permission"));
    s.json(&["send", "--force", &agent, "y"]);
    shows(&s, &agent, "answered:y");

    wait_until("idle", || status(&s, &agent) == "idle");
    let (code, err) = s.err(&["send", "--if", "working", &agent, "nope"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (10, Some("status_mismatch")));
    assert!(!text(&s, &agent).contains("nope"));
}

#[test]
fn agents_wait_for_a_person_to_pause() {
    let s = session();
    let t = s.json(&["run", "--bg", "sh", "-c", BYTES])["address"].as_str().unwrap().to_string();
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));

    // Arrow keys still wait for quiet, without creating a draft.
    person.typed(b"\x1b[A");
    let started = Instant::now();
    s.json(&["send", &t, "x"]);
    assert!(started.elapsed() >= Duration::from_millis(500), "sent after {:?}", started.elapsed());
}

#[test]
fn a_send_waits_for_the_person_to_send_or_clear_their_draft() {
    let mut s = session();
    s.env.push(("HC_DRAFT_MS".into(), "2000".into()));
    let t = s.json(&["run", "--bg", "sh", "-c", BYTES])["address"].as_str().unwrap().to_string();
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    for clear in [b"\r".as_slice(), b"\x7f\x7f\x7f"] {
        person.typed(b"hey");
        let sender = {
            let mut cmd = s.cmd();
            cmd.args(["send", &t, "x"]);
            std::thread::spawn(move || cmd.output().unwrap())
        };
        std::thread::sleep(Duration::from_millis(900));
        assert!(!sender.is_finished(), "submitted the unfinished draft");
        let cleared = Instant::now();
        person.typed(clear);
        assert!(sender.join().unwrap().status.success());
        if clear == b"\x7f\x7f\x7f" {
            assert!(cleared.elapsed() >= Duration::from_millis(500), "backspace keeps the quiet wait");
        } else {
            assert!(
                cleared.elapsed() < Duration::from_millis(750),
                "did not release immediately: {:?}",
                cleared.elapsed()
            );
        }
    }
    person.typed(b"walk away");
    let start = Instant::now();
    s.json(&["send", &t, "after cap"]);
    assert!(start.elapsed() >= Duration::from_millis(1900));
}

#[test]
fn planned_restart_keeps_the_draft_and_its_original_deadline() {
    let mut s = session();
    s.env.push(("HC_DRAFT_MS".into(), "4000".into()));
    let t = s.json(&["run", "--bg", "sh", "-c", BYTES])["address"].as_str().unwrap().to_string();
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    person.typed(b"draft");
    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(1800));
    s.json(&["server", "restart"]);
    let sending = Instant::now();
    s.json(&["send", &t, "x"]);
    assert!(sending.elapsed() >= Duration::from_millis(1200), "forgot the draft");
    assert!(start.elapsed() >= Duration::from_millis(3900), "released before its deadline");
    assert!(start.elapsed() < Duration::from_millis(5200), "restarted the draft clock");
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    person.typed(b"next");
    s.json(&["server", "restart"]);
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    person.typed(b"\r");
    let start = Instant::now();
    s.json(&["send", &t, "y"]);
    assert!(start.elapsed() < Duration::from_millis(1200), "could not submit the restored draft");
}

#[test]
fn invalid_delivery_durations_use_defaults_without_losing_the_terminal() {
    let s = session();
    let t = s.json(&["run", "--bg", "cat"])["address"].as_str().unwrap().to_string();
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    for setting in ["draft_wait", "notes_quiet"] {
        for value in ["inf", "-inf", "nan", "1e100"] {
            std::fs::write(&config, format!("{setting} = {value}\n")).unwrap();
            s.json(&["send", &t, "delivered"]);
        }
    }
    std::fs::write(config, "draft_wait = 0\nnotes_quiet = 0\n").unwrap();
    s.json(&["send", &t, "recovered"]);
    wait_until("delivery after fixing config", || text(&s, &t).contains("recovered"));
}

#[test]
fn draft_wait_config_releases_a_send_and_zero_keeps_only_the_quiet_pause() {
    let mut s = session();
    s.env.push(("HC_DRAFT_MS".into(), "5000".into()));
    let t = s.json(&["run", "--bg", "sh", "-c", BYTES])["address"].as_str().unwrap().to_string();
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    for (seconds, minimum, maximum) in [(2.0, 1900, 4000), (0.0, 500, 1600)] {
        std::fs::write(&config, format!("draft_wait = {seconds}\n")).unwrap();
        person.typed(b"draft");
        let start = Instant::now();
        s.json(&["send", &t, "x"]);
        assert!(start.elapsed() >= Duration::from_millis(minimum), "released too early: {:?}", start.elapsed());
        assert!(start.elapsed() < Duration::from_millis(maximum), "held too long: {:?}", start.elapsed());
    }
}

#[test]
fn an_abandoned_draft_send_never_types_and_does_not_hold_up_interrupt() {
    let mut s = session();
    s.env.push(("HC_DRAFT_MS".into(), "2000".into()));
    let t = run(&s, &format!("r state=idle; {BYTES}"));
    wait_until("reported", || get(&s, &t)["reported"] == true);
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    person.typed(b"draft");
    let mut sender = s.cmd().args(["send", &t, "^abandoned"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(sender.try_wait().unwrap().is_none());
    let start = Instant::now();
    s.json(&["interrupt", &t]);
    assert!(start.elapsed() < Duration::from_millis(500), "interrupt queued behind the draft");
    sender.kill().unwrap();
    sender.wait().unwrap();
    let mut caller = common::framed::Framed::open(&s);
    caller.send(
        0,
        herdr_core::wire::DATA,
        json!({"jsonrpc": "2.0", "id": 42, "method": "terminal.send",
            "params": {"terminal": t, "text": "^cancelled"}})
        .to_string()
        .as_bytes(),
    );
    std::thread::sleep(Duration::from_millis(100));
    caller.send(
        0,
        herdr_core::wire::DATA,
        json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": 42}}).to_string().as_bytes(),
    );
    std::thread::sleep(Duration::from_millis(100));
    person.typed(b"\r");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!text(&s, &t).contains("5e"), "abandoned send typed its first byte");
    s.json(&["send", &t, "z"]);
    shows(&s, &t, "7a");
}

#[test]
fn half_closed_callers_keep_their_sends() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    let s = session();
    let t = run(&s, BYTES);
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    for draft in [false, true] {
        if draft {
            person.typed(b"draft");
        }
        let mut caller = UnixStream::connect(s.socket()).unwrap();
        writeln!(
            caller,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "terminal.send",
            "params": {"terminal": t, "text": "z"}})
        )
        .unwrap();
        caller.shutdown(Shutdown::Write).unwrap();
        if draft {
            std::thread::sleep(Duration::from_millis(300));
            person.typed(b"\r");
        }
        caller.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reply = String::new();
        BufReader::new(caller).read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
    }
    shows(&s, &t, "7a");
}

#[test]
fn held_sends_keep_order_and_leave_interrupt_free_during_the_next_draft() {
    let mut s = session();
    s.env.push(("HC_DRAFT_MS".into(), "5000".into()));
    let t = run(&s, &format!("r state=idle; {BYTES}"));
    wait_until("reported", || get(&s, &t)["reported"] == true);
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    person.typed(b"draft");
    let mut first = s.cmd().args(["send", &t, "X"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let mut second = s.cmd().args(["send", &t, "Y"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    person.typed(b"\r");
    shows(&s, &t, "58");
    person.typed(b"next draft");
    std::thread::sleep(Duration::from_millis(300));
    assert!(second.try_wait().unwrap().is_none(), "second send submitted the next draft");
    let start = Instant::now();
    s.json(&["interrupt", &t]);
    assert!(start.elapsed() < Duration::from_millis(500), "interrupt queued behind second draft");
    person.typed(b"\r");
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    let output = text(&s, &t);
    assert!(output.find("58").unwrap() < output.find("59").unwrap(), "{output}");
}

#[test]
fn agent_clear_keys_end_the_tracked_draft() {
    let mut s = session();
    s.env.push(("HC_DRAFT_MS".into(), "5000".into()));
    let t = run(&s, BYTES);
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));
    for clear in ["enter", "ctrl+c", "ctrl+u"] {
        person.typed(b"draft");
        s.json(&["keys", &t, clear]);
        person.typed(b"\x1b[A");
        let start = Instant::now();
        s.json(&["send", &t, "x"]);
        assert!(start.elapsed() < Duration::from_secs(2), "{clear}: stale draft held send");
    }
}

#[test]
fn a_program_that_never_reads_its_input_stalls_nobody() {
    let s = session();
    // Raw mode: the terminal driver blocks writers instead of dropping input.
    let deaf = s.json(&["run", "--bg", "sh", "-c", "stty raw -echo; echo deaf; sleep 100"])["address"]
        .as_str()
        .unwrap()
        .to_string();
    shows(&s, &deaf, "deaf");
    // Far more than a terminal's input buffer holds.
    let flood = "x".repeat(256 * 1024);
    let mut conn = s.connect();
    let target = deaf.clone();
    let sender = std::thread::spawn(move || conn.call(1, "terminal.send", json!({"terminal": target, "text": flood})));
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let other =
        s.json(&["run", "--bg", "sh", "-c", "echo still-fine; sleep 100"])["address"].as_str().unwrap().to_string();
    shows(&s, &other, "still-fine");
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    s.json(&["kill", &deaf]);
    sender.join().unwrap();
}

#[test]
fn keys_follow_the_programs_keyboard_modes() {
    let s = session();
    // One byte at a time: uutils od (Ubuntu 26.04) holds its output until EOF.
    // Unquoted $c drops the padding BSD od (macOS) puts before each byte.
    let probe = "stty raw -echo; printf '\\033[?1hready\\n'; \
                 while c=$(dd bs=1 count=1 2>/dev/null | od -An -tx1) && [ -n \"$c\" ]; do printf ' %s' $c; done";
    let t = s.json(&["run", "--bg", "sh", "-c", probe])["address"].as_str().unwrap().to_string();
    shows(&s, &t, "ready");
    s.json(&["keys", &t, "up", "ctrl+c", "a", "a", "a", "a", "a", "a", "a", "a", "a", "a", "a", "a"]);
    shows(&s, &t, "1b 4f 41 03 61");
    let (code, err) = s.err(&["keys", &t, "hyper+q"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (6, Some("invalid_params")));
}

#[test]
fn interrupt_reports_what_it_observed() {
    let s = session();
    let interrupt = |t: &str| s.json(&["interrupt", t, "--timeout", "1"]);
    let stops = run(&s, "stty raw -echo; r state=working; dd bs=1 count=1 >/dev/null 2>&1; r state=idle; sleep 100");
    let stubborn = run(&s, "stty raw -echo; r state=working; sleep 100");
    let quits = run(&s, "stty raw -echo; r state=working; dd bs=1 count=1 >/dev/null 2>&1; exit 0");
    for t in [&stops, &stubborn, &quits] {
        wait_until("working", || status(&s, t) == "working");
    }
    let r = interrupt(&stops);
    assert_eq!((r["outcome"].as_str(), r["key"].as_str()), (Some("stopped"), Some("esc")));
    assert_eq!(interrupt(&stubborn)["outcome"], "still_working");
    assert_eq!(interrupt(&quits)["outcome"], "exited");

    // No status reports: a shell that gets its prompt back has stopped its job.
    let shell = s.json(&["run", "--bg", "sh", "-i"])["address"].as_str().unwrap().to_string();
    s.json(&["send", &shell, "sleep 100"]);
    std::thread::sleep(Duration::from_millis(300));
    let r = interrupt(&shell);
    assert_eq!((r["outcome"].as_str(), r["key"].as_str()), (Some("stopped"), Some("ctrl+c")));
    let deaf = s.json(&["run", "--bg", "sh", "-c", "trap '' INT; echo deaf; while :; do sleep 0.1; done"])["address"]
        .as_str()
        .unwrap()
        .to_string();
    shows(&s, &deaf, "deaf"); // the trap is in place
    assert_eq!(interrupt(&deaf)["outcome"], "unknown");

    let (code, err) = s.err(&["send", &quits, "x"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (11, Some("finished")));
}
