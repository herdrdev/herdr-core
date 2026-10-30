#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::Viewer;
use common::*;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
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
fn send_can_follow_up_on_done_through_the_cli_and_api() {
    let s = session();
    let agent =
        run(&s, "stty -echo; r state=idle; while read -r l; do r state=working; echo got:$l; r state=done; done");
    wait_until("idle", || status(&s, &agent) == "idle");
    s.json(&["send", "--if", "idle", &agent, "first"]);
    wait_until("done", || status(&s, &agent) == "done");

    let sent = s.json(&["send", "--if", "done", &agent, "second"]);
    assert_eq!(
        (sent["status"].as_str(), sent["turn"].as_u64(), sent["started"].as_bool()),
        (Some("done"), Some(1), Some(true))
    );
    shows(&s, &agent, "got:second");
    wait_until("turn 2", || s.json(&["get", &agent])["turn"] == 2);

    let sent = s.connect().call(1, "terminal.send", json!({"terminal": agent, "text": "third", "if": "done"}));
    assert_eq!(sent["result"]["status"], "done", "{sent}");
    assert_eq!(sent["result"]["started"], true, "{sent}");
    shows(&s, &agent, "got:third");
    wait_until("turn 3", || s.json(&["get", &agent])["turn"] == 3);
}

#[test]
fn a_done_guard_refuses_other_states_without_typing() {
    let s = session();
    for state in ["idle", "working", "blocked", "error", "silent"] {
        let report = if state == "silent" { String::new() } else { format!("r state={state};") };
        let agent = run(&s, &format!("stty -echo; {report} echo ready; while read -r l; do echo got:$l; done"));
        shows(&s, &agent, "ready");
        if state != "silent" {
            wait_until(state, || status(&s, &agent) == state);
        }
        let (code, err) = s.err(&["send", "--if", "done", &agent, "refused"]);
        let expected = if state == "blocked" { (9, "blocked") } else { (10, "status_mismatch") };
        assert_eq!((code, err["error"]["kind"].as_str().unwrap()), expected, "{state}: {err}");

        // A later allowed send proves the refused text was not buffered as input.
        s.json(&["send", "--force", &agent, "after"]);
        shows(&s, &agent, "got:after");
        assert!(!text(&s, &agent).contains("refused"), "{state}: nothing typed for a refused send");
    }
    let exited = run(&s, "r state=done; exit 0");
    wait_until("finished", || s.json(&["get", &exited])["state"] == "finished");
    let (code, err) = s.err(&["send", "--if", "done", &exited, "refused"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (11, Some("finished")));
}

#[test]
fn a_done_guard_is_rechecked_after_human_typing_pauses() {
    let mut s = session();
    s.env.push(("HC_QUIET_MS".into(), "3000".into()));
    for state in ["working", "blocked"] {
        let gate = s.root.join(format!("switch-{state}"));
        assert!(s.program("mkfifo").arg(&gate).status().unwrap().success());
        let agent = run(
            &s,
            &format!(
                "stty raw -echo; r state=done; dd bs=1 count=1 >/dev/null 2>&1; echo human; \
                 stty sane -echo; read -r state < '{}'; r state=$state; \
                 while read -r l; do echo got:$l; done",
                gate.display()
            ),
        );
        wait_until("done", || status(&s, &agent) == "done");
        let mut person = Viewer::attach(&s, &agent, 80, 24, json!({}));
        person.typed(b"h");
        shows(&s, &agent, "human");

        let mut sender = s.connect();
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "terminal.send", "params": {"terminal": agent, "text": "refused", "if": "done"}});
        writeln!(sender.writer, "{request}").unwrap();
        std::fs::write(&gate, format!("{state}\n")).unwrap();
        wait_until(state, || status(&s, &agent) == state);

        let mut line = String::new();
        sender.reader.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        let expected = if state == "blocked" { "blocked" } else { "status_mismatch" };
        assert_eq!(reply["error"]["data"]["kind"], expected, "{reply}");
        s.json(&["send", "--force", &agent, "after"]);
        shows(&s, &agent, "got:after");
        assert!(!text(&s, &agent).contains("refused"), "{state}: nothing typed after the status changed");
    }
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
fn agents_wait_for_a_person_to_pause_and_never_mix_with_their_keys() {
    let s = session();
    let t = s.json(&["run", "--bg", "sh", "-c", BYTES])["address"].as_str().unwrap().to_string();
    let mut person = Viewer::attach(&s, &t, 80, 24, json!({}));

    // Someone is typing: the send waits for the quiet time.
    person.typed(b"q");
    let started = Instant::now();
    s.json(&["send", &t, "x"]);
    assert!(started.elapsed() >= Duration::from_millis(500), "sent after {:?}", started.elapsed());

    // Keys pressed while a paste is going in are held until it's done.
    std::thread::sleep(Duration::from_millis(700));
    let sender = {
        let mut cmd = s.cmd();
        cmd.args(["send", &t, "abc"]);
        std::thread::spawn(move || cmd.output().unwrap())
    };
    // Inside the paste: "abc" is in, its Enter (the second 0d) is not.
    let tokens = || text(&s, &t).split_whitespace().map(String::from).collect::<Vec<_>>();
    wait_until("paste in", || tokens().contains(&"63".to_string()));
    assert_eq!(tokens().iter().filter(|b| *b == "0d").count(), 1, "the paste ended already: tests nothing");
    person.typed(b"Z");
    assert!(sender.join().unwrap().status.success());
    wait_until("Z delivered", || text(&s, &t).contains("5a"));
    let bytes: Vec<String> = text(&s, &t).split_whitespace().map(String::from).collect();
    let tail: Vec<&str> = bytes.iter().rev().take(5).rev().map(String::as_str).collect();
    assert_eq!(tail, ["61", "62", "63", "0d", "5a"], "all: {bytes:?}");
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
    let t = s.json(&["run", "--bg", "sh", "-c", "printf '\\033[?1h'; stty raw -echo; od -An -c"])["address"]
        .as_str()
        .unwrap()
        .to_string();
    shows(&s, &t, ""); // started
    std::thread::sleep(Duration::from_millis(200));
    s.json(&["keys", &t, "up", "ctrl+c", "a", "a", "a", "a", "a", "a", "a", "a", "a", "a", "a", "a"]);
    shows(&s, &t, "033   O   A 003   a");
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
