#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::{Framed, Viewer};
use common::*;
use herdr_core::wire;
use serde_json::{Value, json};
use std::io::BufRead;
use std::time::Duration;

fn wait_status(s: &Session, t: &str, state: &str) -> Value {
    let mut last = Value::Null;
    wait_until(&format!("{t} {state}"), || {
        last = get(s, t);
        last["status"]["state"] == state
    });
    last
}

#[test]
fn status_words_and_turns_are_kept_exactly_and_reset_on_exit() {
    let s = Session::new();
    let body = "r state=idle; w a; r state=working:app=fake:progress=40; w b; r state=done:msg=QWxsIGRvbmU=; w c; r state=working; w d; exit 3";
    let t = run(&s, &["sh", "-c", &script(&s, body)]);
    let rec = wait_status(&s, &t, "idle");
    assert_eq!((rec["turn"].as_u64(), rec["reported"].as_bool()), (Some(0), Some(true)));

    touch(&s, "a");
    let rec = wait_status(&s, &t, "working");
    assert_eq!((rec["status"]["app"].as_str(), rec["status"]["progress"].as_u64()), (Some("fake"), Some(40)));
    touch(&s, "b");
    let rec = wait_status(&s, &t, "done");
    assert_eq!((rec["status"]["msg"].as_str(), rec["turn"].as_u64()), (Some("All done"), Some(1)));
    assert!(rec["status"].get("app").is_none(), "a report replaces its record completely");

    touch(&s, "c");
    wait_status(&s, &t, "working");
    touch(&s, "d");
    wait_until("exit", || get(&s, &t)["state"] == "finished");
    let rec = get(&s, &t);
    assert!(rec["status"].is_null(), "exit resets status: {rec}");
    assert_eq!((rec["turn"].as_u64(), rec["exit_code"].as_i64()), (Some(2), Some(3)));
}

#[test]
fn events_arrive_in_order_after_a_snapshot() {
    let s = Session::new();
    let before = run(&s, &["sleep", "100"]);
    let mut conn = {
        s.json(&["server", "start"]);
        s.connect()
    };
    let sub = conn.call(1, "events.subscribe", json!({}));
    assert_eq!(sub["result"]["terminals"][0]["address"], before.as_str());
    let mut seq = sub["result"]["seq"].as_u64().unwrap();

    let t = run(&s, &["sh", "-c", &script(&s, "w a; r state=working; w b; r state=done; w c")]);
    s.json(&["label", &t, "role=x"]);
    touch(&s, "a");
    wait_status(&s, &t, "working");
    touch(&s, "b");
    wait_status(&s, &t, "done");
    touch(&s, "c");
    wait_until("finished", || get(&s, &t)["state"] == "finished");
    s.json(&["rm", &t]);

    let mut seen = Vec::new();
    while seen.last().map(String::as_str) != Some("removed") {
        // Foreground facts come about twice a second, for every terminal: tested on their own.
        let e = next_event(&mut conn, |m| m["method"] == "event" && m["params"]["type"] != "foreground");
        let p = &e["params"];
        assert_eq!(p["terminal"], t.as_str());
        assert!(p["seq"].as_u64().unwrap() > seq, "seq goes up");
        seq = p["seq"].as_u64().unwrap();
        seen.push(p["type"].as_str().unwrap().to_string());
    }
    // The last status is the reset at exit.
    assert_eq!(seen, ["created", "labels", "status", "status", "turn", "status", "finished", "removed"]);
}

#[test]
fn filters_pick_terminals_by_address_or_label() {
    let s = Session::new();
    let a = run(&s, &["--label", "team=red", "sh", "-c", &script(&s, "w a; r state=working; sleep 100")]);
    let b = run(&s, &["sh", "-c", &script(&s, "w b; r state=working; sleep 100")]);
    s.json(&["server", "start"]);
    let mut by_addr = s.connect();
    by_addr.call(1, "events.subscribe", json!({"terminal": b}));
    let mut by_label = s.connect();
    let sub = by_label.call(1, "events.subscribe", json!({"labels": {"team": "red"}}));
    assert_eq!(sub["result"]["terminals"].as_array().unwrap().len(), 1);

    touch(&s, "a");
    touch(&s, "b");
    let e = next_event(&mut by_addr, |m| m["method"] == "event");
    assert_eq!(e["params"]["terminal"], b.as_str());
    let e = next_event(&mut by_label, |m| m["method"] == "event");
    assert_eq!(e["params"]["terminal"], a.as_str());

    let bad = s.connect().call(1, "events.subscribe", json!({"terminal": "t99"}));
    assert_eq!(bad["error"]["data"]["kind"], "not_found");
}

#[test]
fn a_subscriber_that_falls_behind_gets_missed_and_a_snapshot() {
    let mut s = Session::new();
    s.env.push(("HC_HEARTBEAT_MS".into(), "100".into()));
    s.json(&["server", "start"]);
    let mut slow = s.connect();
    slow.call(1, "events.subscribe", json!({}));
    // Flood events, not durable turn boundaries: each new status word must be synced to disk.
    let flood = script(
        &s,
        "i=0; while [ $i -lt 3000 ]; do r state=working:progress=0; r state=working:progress=1; i=$((i+1)); done; r state=done; sleep 100",
    );
    let t = run(&s, &["sh", "-c", &flood]);
    wait_status(&s, &t, "done");

    // It gets `missed` (only once its queue filled), and ends up with the true final state.
    let finished_here = |x: &Value| x["address"] == t.as_str() && x["status"]["state"] == "done";
    let mut missed_at = None;
    let done = next_event(&mut slow, |m| {
        if m["method"] == "missed" {
            missed_at.get_or_insert(m["params"]["seq"].as_u64().unwrap());
        }
        (m["method"] == "event" && m["params"]["data"]["status"]["state"] == "done")
            || (m["method"] == "missed" && m["params"]["terminals"].as_array().unwrap().iter().any(finished_here))
    });
    let missed_at = missed_at.expect("never told it missed events");
    assert!(missed_at > herdr_core::events::PLAIN_QUEUE as u64, "behind too early: {missed_at}");
    let beat = next_event(&mut slow, |m| m["method"] == "heartbeat");
    assert!(beat["params"]["seq"].as_u64().unwrap() >= done["params"]["seq"].as_u64().unwrap());
}

#[test]
fn hc_events_prints_the_snapshot_then_one_line_per_event() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut child = s.cmd().arg("events").stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let snapshot: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert!(snapshot["terminals"].as_array().unwrap().is_empty());
    let t = run(&s, &["true"]);
    let created: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(
        (created["params"]["type"].as_str(), created["params"]["terminal"].as_str()),
        (Some("created"), Some(t.as_str()))
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn framed_subscribers_get_events_on_their_own_channel() {
    let s = Session::new();
    let mut f = Framed::open(&s);
    let sub = f.call("events.subscribe", json!({}));
    let channel = sub["result"]["channel"].as_u64().unwrap() as u32;
    let t = run(&s, &["true"]);
    let mut text = String::new();
    while !text.contains("\"finished\"") {
        let (ch, kind, payload) = f.read(Duration::from_secs(10)).expect("event frame");
        if ch == channel && kind == wire::DATA {
            text.push_str(&String::from_utf8_lossy(&payload));
        }
    }
    assert!(text.contains(&format!("\"terminal\":\"{t}\"")) && text.contains("\"created\""));
}

#[test]
fn a_new_viewer_sees_the_status_and_7501_questions_are_answered_once() {
    let s = Session::new();
    let probe = script(
        &s,
        "r state=working:app=fake:id=job:msg=aGk=; r state=blocked:kind=question; w a; \
         stty raw -echo min 0 time 2; printf '\\033]7501;?\\033\\\\'; x=$(dd bs=64 count=1 2>/dev/null); \
         stty sane; printf '%s' \"$x\" | od -An -c | tr -d ' \\n'; echo; echo END; sleep 100",
    );
    let t = run(&s, &["sh", "-c", &probe]);
    wait_status(&s, &t, "blocked");
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    touch(&s, "a");
    v.shows("END");
    let reports = v.screen.take_status_reports();
    assert!(
        reports.iter().any(|r| r.id.is_empty() && r.state == "blocked" && r.kind == Some("question")),
        "{reports:?}"
    );
    assert!(reports.iter().any(|r| r.id == "job" && r.app == "fake" && r.message == "hi"), "{reports:?}");
    assert_eq!(v.text().matches("]7501;?").count(), 1, "answered once: {}", v.text());
    assert!(!String::from_utf8_lossy(&v.raw).contains("\x1b]7501;?\x1b\\"), "query reached the viewer");
}

#[test]
fn a_core_crash_does_not_count_old_turns_twice() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", &script(&s, "r state=working; w a; r state=done; sleep 100")]);
    wait_status(&s, &t, "working");
    touch(&s, "a");
    let done = wait_status(&s, &t, "done");
    assert_eq!(done["turn"], 1);
    assert!(done["status_ms"].as_u64().unwrap() >= done["created_ms"].as_u64().unwrap());
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    unsafe { libc::kill(core as i32, libc::SIGKILL) };
    wait_until("core gone", || !pid_alive(core));

    let rec = get(&s, &t);
    assert_eq!((rec["status"]["state"].as_str(), rec["turn"].as_u64()), (Some("done"), Some(1)));
    assert_eq!(rec["status_ms"], done["status_ms"], "nor move the time its status changed");
}
