#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::Duration;

fn session() -> Session {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "300".into()));
    s.env.push(("HC_STARTED_MS".into(), "1000".into()));
    s
}

/// `r <report>` sends OSC 7501, `w <name>` waits for a file in the session root.
fn run(s: &Session, extra: &[&str], body: &str) -> String {
    let script = script(s, body);
    let mut args = vec!["run", "--bg"];
    args.extend_from_slice(extra);
    args.extend(["sh", "-c", script.as_str()]);
    s.json(&args)["address"].as_str().unwrap().to_string()
}

/// `hc` as if typed inside terminal `t` (its HC_TERMINAL_ID).
fn hc_in(s: &Session, t: &str, args: &[&str]) -> Value {
    let out = s.cmd().env("HC_TERMINAL_ID", t).args(args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn invalid_config_keeps_pending_parent_notes_responsive_and_uses_the_default_hold() {
    for setting in ["draft_wait", "notes_quiet"] {
        let mut s = session();
        s.env.push(("HC_DRAFT_MS".into(), "2500".into()));
        let p = run(&s, &[], "stty -echo; r state=idle; while read -r l; do echo \"parent-got:$l\"; done");
        wait_until("parent ready", || get(&s, &p)["reported"] == true);
        let mut person = common::framed::Viewer::attach(&s, &p, 80, 24, json!({}));
        let kid = run(&s, &["--parent", &p], "w news");
        let config = s.root.join("config/herdr-core/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, format!("{setting} = inf\n")).unwrap();
        person.typed(b"my draft");
        let start = std::time::Instant::now();
        touch(&s, "news");
        // Fix the config even if the responsiveness check fails, so cleanup can stop the core.
        let mut conn = s.connect();
        conn.reader.get_ref().set_read_timeout(Some(Duration::from_millis(1000))).unwrap();
        use std::io::{BufRead, Write};
        std::thread::sleep(Duration::from_millis(400));
        writeln!(
            conn.writer,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "terminal.get", "params": {"terminal": p}})
        )
        .unwrap();
        let mut response = String::new();
        let result = conn.reader.read_line(&mut response);
        if result.is_err() {
            std::fs::write(&config, "").unwrap();
        }
        assert!(result.is_ok(), "pending note stalled the core with {setting} = inf: {result:?}");
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["result"]["reported"], true, "{response}");
        assert!(!text(&s, &p).contains("parent-got:"), "note submitted the draft early");
        wait_until("note with invalid config", || text(&s, &p).contains(&format!("<hc>{kid} exited")));
        assert!(start.elapsed() >= Duration::from_millis(2400), "default draft hold was skipped");
        let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
        assert_eq!(log.matches("using delivery defaults").count(), 1, "{log}");
        assert!(log.contains(setting), "{log}");
    }
}

#[test]
fn a_parent_note_waits_for_a_draft_and_survives_a_dialog_while_waiting() {
    let mut s = session();
    s.env.push(("HC_DRAFT_MS".into(), "3000".into()));
    let p = run(
        &s,
        &[],
        "stty -echo; r state=idle; (w busy; r state=blocked; w free; r state=idle) & while read -r l; do echo \"parent-got:$l\"; done",
    );
    wait_until("parent ready", || get(&s, &p)["reported"] == true);
    let mut person = common::framed::Viewer::attach(&s, &p, 80, 24, json!({}));
    person.typed(b"my draft");
    let kid = run(&s, &["--parent", &p], "r state=working; w news; r state=blocked; sleep 100");
    touch(&s, "news");
    wait_until("child blocked", || get(&s, &kid)["status"]["state"] == "blocked");
    std::thread::sleep(Duration::from_millis(700));
    assert!(!text(&s, &p).contains("parent-got:"), "note submitted the draft");
    touch(&s, "busy");
    wait_until("parent dialog", || get(&s, &p)["status"]["state"] == "blocked");
    person.typed(b"\r");
    std::thread::sleep(Duration::from_millis(400));
    assert!(!text(&s, &p).contains("<hc>"), "note answered the dialog");
    touch(&s, "free");
    wait_until("note after the dialog", || text(&s, &p).contains(&format!("parent-got:<hc>{kid} is blocked")));
    let got = text(&s, &p);
    assert!(got.contains("parent-got:my draft"), "{got}");
    assert_eq!(got.matches("is blocked").count(), 1, "{got}");
}

#[test]
fn a_terminal_is_nobodys_caller_in_another_session() {
    let s = session();
    let other = |args: &[&str]| s.cmd().env("HC_SESSION", "other").args(args).output().unwrap();
    let stranger = String::from_utf8(other(&["run", "--bg", "sleep", "100"]).stdout).unwrap();
    assert!(stranger.contains("\"t1\""), "the other session has its own t1: {stranger}");
    let bin = s.bin.display().to_string();
    let p = run(
        &s,
        &[],
        &format!(
            "{bin} run --bg --name same sleep 100; HC_SESSION=other {bin} run --bg --name across sleep 100; sleep 100"
        ),
    );
    assert_eq!(p, "t1");
    wait_until("both started", || {
        other(&["get", "across"]).status.success() && s.hc(&["get", "same"]).status.success()
    });
    assert_eq!(get(&s, "same")["parent"], "t1");
    let across: Value = serde_json::from_slice(&other(&["get", "across"]).stdout).unwrap();
    assert!(across["parent"].is_null(), "not the other session's t1: {across}");
    other(&["server", "stop"]);
}

#[test]
fn the_parent_comes_from_the_caller_or_flags() {
    let s = session();
    let p = run(&s, &[], "sleep 100");
    let kid = hc_in(&s, &p, &["run", "--bg", "sleep", "100"]);
    assert_eq!(kid["parent"], p.as_str());
    let orphan = hc_in(&s, &p, &["run", "--bg", "--no-parent", "sleep", "100"]);
    assert!(orphan["parent"].is_null());
    let adopted = s.json(&["run", "--bg", "--parent", &p, "sleep", "100"]);
    assert_eq!(adopted["parent"], p.as_str());
    let (_, err) = s.err(&["run", "--bg", "--parent", "t99", "true"]);
    assert_eq!(err["error"]["kind"], "not_found");
    assert!(get(&s, &p)["parent"].is_null(), "started from outside any terminal");
}

#[test]
fn children_end_with_their_parent_unless_they_asked_to_outlive_it() {
    let s = session();
    let p = run(&s, &[], "sleep 100");
    let goes = s.json(&["run", "--bg", "--parent", &p, "sleep", "100"])["address"].as_str().unwrap().to_string();
    let stays = s.json(&["run", "--bg", "--parent", &p, "--outlive-parent", "sleep", "100"])["address"]
        .as_str()
        .unwrap()
        .to_string();
    s.json(&["kill", &p]);
    wait_until("child ends", || get(&s, &goes)["state"] == "finished");
    assert_eq!(get(&s, &stays)["state"], "live");
}

#[test]
fn only_a_blocked_errored_or_exited_child_is_a_note() {
    let s = session();
    let p = run(&s, &[], "stty -echo; r state=idle; while read -r l; do echo \"parent-got:$l\"; done");
    let kid = run(
        &s,
        &["--parent", &p, "--name", "reviewer"],
        "r state=working; w a; r state=done; w b; r state=blocked:kind=permission; w e; r state=working; r state=error:msg=Ym9vbQ==; w c; exit 3",
    );
    touch(&s, "a");
    wait_until("done", || get(&s, &kid)["status"]["state"] == "done");
    std::thread::sleep(Duration::from_millis(800));
    assert!(!text(&s, &p).contains("<hc>"), "a finished turn is the child's to report");
    touch(&s, "b");
    wait_until("blocked note", || {
        text(&s, &p)
            .contains(&format!("parent-got:<hc>{kid} reviewer is blocked (permission). See: hc read {kid}</hc>"))
    });
    touch(&s, "e");
    wait_until("error note", || {
        text(&s, &p).contains(&format!("parent-got:<hc>{kid} reviewer hit an error: boom. See: hc read {kid}</hc>"))
    });
    touch(&s, "c");
    wait_until("exit note", || {
        text(&s, &p).contains(&format!("parent-got:<hc>{kid} reviewer exited (exit 3). Read: hc read {kid}</hc>"))
    });
    assert_eq!(text(&s, &p).matches("is blocked").count(), 1, "each piece of news once");
}

#[test]
fn a_plain_programs_end_is_a_note_with_its_last_lines_unless_the_parent_saw_it() {
    let s = session();
    let p = run(&s, &[], "stty -echo; r state=idle; while read -r l; do echo \"parent-got:$l\"; done");
    // Plain: never reports a status. 25 lines, blanks between: the last 20 that say something.
    let ok = run(&s, &["--parent", &p, "--name", "build"], "w a; for i in $(seq 25); do echo line$i; echo; done");
    let silent = run(&s, &["--parent", &p], "w b; exit 4");
    touch(&s, "a");
    wait_until("success note", || {
        text(&s, &p).contains(&format!("parent-got:<hc>{ok} build exited (exit 0). Last lines:"))
            && text(&s, &p).contains(&format!("parent-got:Read: hc read {ok}</hc>"))
    });
    let got: Vec<String> = text(&s, &p).lines().map(String::from).collect();
    let line = |n: u32| got.contains(&format!("parent-got:line{n}"));
    assert!(line(6) && line(25) && !line(5), "{got:?}");
    assert!(!got.contains(&"parent-got:".to_string()), "no blank lines: {got:?}");

    touch(&s, "b");
    wait_until("failure note, nothing to show", || {
        text(&s, &p).contains(&format!("parent-got:<hc>{silent} exited (exit 4). Read: hc read {silent}</hc>"))
    });
    // A program it waited for is no note: it has the result already.
    hc_in(&s, &p, &["--json", "run", "sh", "-c", "echo waited-for"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!text(&s, &p).contains("parent-got:waited-for"), "{}", text(&s, &p));
}

#[test]
fn notes_reach_a_working_parent_but_never_a_blocked_one() {
    let s = session();
    // Echo stays on while it is blocked, so anything typed early shows at once.
    let p = run(
        &s,
        &[],
        "r state=blocked; w free; r state=working; stty -echo; while read -r l; do echo \"parent-got:$l\"; done",
    );
    let a = run(&s, &["--parent", &p], "r state=working; w a; exit 0");
    let b = run(&s, &["--parent", &p], "r state=working; w b; r state=blocked:kind=permission; sleep 100");
    touch(&s, "a");
    touch(&s, "b");
    std::thread::sleep(Duration::from_millis(800));
    assert!(!text(&s, &p).contains("<hc>"), "never typed into a blocked parent");
    touch(&s, "free");
    wait_until("both notes, while it works", || {
        let t = text(&s, &p);
        t.contains(&format!("<hc>{a} exited (exit 0)")) && t.contains(&format!("<hc>{b} is blocked (permission)"))
    });
}

#[test]
fn only_news_the_parent_has_not_seen_becomes_a_note() {
    let s = session();
    let p = run(
        &s,
        &[],
        "stty -echo; r state=blocked; w free; r state=idle; while read -r l; do echo \"parent-got:$l\"; done",
    );
    let read_it = run(&s, &["--parent", &p], "r state=working; w a; r state=blocked; sleep 100");
    let waited = run(&s, &["--parent", &p], "r state=working; w b; r state=blocked; sleep 100");
    let quiet = run(&s, &["--parent", &p, "--no-notify"], "r state=working; w c; r state=blocked; sleep 100");
    touch(&s, "a");
    wait_until("blocked", || get(&s, &read_it)["status"]["state"] == "blocked");
    hc_in(&s, &p, &["--json", "read", &read_it]); // the parent looked: no note needed

    let waiting = {
        let mut cmd = s.cmd();
        cmd.env("HC_TERMINAL_ID", &p).args(["wait", &waited, "--timeout", "10"]);
        std::thread::spawn(move || cmd.output().unwrap())
    };
    std::thread::sleep(Duration::from_millis(300));
    touch(&s, "b");
    let out = waiting.join().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"blocked\""));
    touch(&s, "c");
    wait_until("quiet child blocked", || get(&s, &quiet)["status"]["state"] == "blocked");

    touch(&s, "free");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!text(&s, &p).contains("<hc>"), "nothing new to tell: {}", text(&s, &p));
}

#[test]
fn wait_for_any_child_also_wakes_when_someone_messages_you() {
    let s = session();
    let p = run(&s, &[], "r state=working; w idle; r state=idle; sleep 100");
    let kid = run(&s, &["--parent", &p, "--name", "helper"], "r state=working; sleep 100");
    wait_until("working", || get(&s, &kid)["status"]["state"] == "working" && get(&s, &p)["reported"] == true);

    let any = wait_children(&s, &p, "any");
    std::thread::sleep(Duration::from_millis(300));
    hc_in(&s, &kid, &["send", &p, "found it"]);
    let r = any.join().unwrap();
    assert_eq!(
        (r["reason"].as_str(), r["from"].as_str()),
        (Some("message"), Some(kid.as_str())),
        "an address to reply to: {r}"
    );
    assert_eq!(r["children"], json!([]), "always present");

    // Typed while the parent was busy between two waits: the next wait hears it at once.
    hc_in(&s, &kid, &["send", &p, "and this"]);
    assert_eq!(hc_in(&s, &p, &["wait", "--children", "any", "--timeout", "5"])["reason"], "message");
    assert_eq!(
        hc_in(&s, &p, &["wait", "--children", "any", "--timeout", "0.5"])["reason"],
        "timeout",
        "each message wakes one wait"
    );

    // Typed into an idle parent it starts a turn there: nothing left pending.
    touch(&s, "idle");
    wait_until("idle", || get(&s, &p)["status"]["state"] == "idle");
    hc_in(&s, &kid, &["send", &p, "hello"]);
    assert_eq!(hc_in(&s, &p, &["wait", "--children", "any", "--timeout", "0.5"])["reason"], "timeout");
}

#[test]
fn every_wait_wakes_when_someone_messages_the_waiter() {
    let s = session();
    let p = run(&s, &[], "r state=working; sleep 100");
    let kid = run(&s, &["--parent", &p], "r state=working; sleep 100");
    wait_until("working", || get(&s, &kid)["status"]["state"] == "working" && get(&s, &p)["reported"] == true);

    // The child keeps working but reports in: the parent can't read it until its wait ends.
    let one = {
        let mut cmd = s.cmd();
        cmd.env("HC_TERMINAL_ID", &p).args(["wait", &kid, "--timeout", "10"]);
        std::thread::spawn(move || serde_json::from_slice::<Value>(&cmd.output().unwrap().stdout).unwrap())
    };
    std::thread::sleep(Duration::from_millis(300));
    hc_in(&s, &kid, &["send", &p, "picked option a, continuing"]);
    let r = one.join().unwrap();
    assert_eq!((r["reason"].as_str(), r["from"].as_str()), (Some("message"), Some(kid.as_str())), "{r}");
    assert_eq!((r["ended"].as_bool(), r["status"].as_str()), (Some(false), Some("working")), "it still runs: {r}");

    let all = wait_children(&s, &p, "all");
    std::thread::sleep(Duration::from_millis(300));
    hc_in(&s, &kid, &["send", &p, "still going"]);
    let r = all.join().unwrap();
    assert_eq!((r["reason"].as_str(), r["from"].as_str()), (Some("message"), Some(kid.as_str())), "{r}");
    assert_eq!(r["children"][0]["reason"], "running", "{r}");
}

#[test]
fn a_turn_that_already_reported_to_the_parent_is_not_news_again() {
    let s = session();
    let p = run(&s, &[], "r state=working; sleep 100");
    let talker = run(&s, &["--parent", &p], "r state=working; w a; r state=done; sleep 100");
    let silent = run(&s, &["--parent", &p], "r state=working; w b; r state=done; sleep 100");
    wait_until("working", || get(&s, &silent)["status"]["state"] == "working" && get(&s, &p)["reported"] == true);

    hc_in(&s, &talker, &["send", &p, "my report"]);
    assert_eq!(hc_in(&s, &p, &["wait", "--children", "any", "--timeout", "5"])["reason"], "message");
    touch(&s, "a");
    wait_until("talker done", || get(&s, &talker)["status"]["state"] == "done");
    let r = hc_in(&s, &p, &["wait", "--children", "any", "--timeout", "0.5"]);
    assert_eq!(r["reason"], "timeout", "its report already said it: {r}");

    touch(&s, "b");
    let r = hc_in(&s, &p, &["wait", "--children", "any", "--timeout", "5"]);
    assert_eq!(ended(&r), [silent], "a silent turn end is still news");
}

#[test]
fn messages_between_agents_say_who_sent_them() {
    let s = session();
    let echo = "stty -echo; r state=idle; while read -r l; do echo \"got:$l\"; done";
    let sender = run(&s, &["--name", "boss"], echo);
    let agent = run(&s, &[], echo);
    let shell = run(&s, &[], "stty -echo; while read -r l; do echo \"got:$l\"; done");
    wait_until("reported", || get(&s, &sender)["reported"] == true && get(&s, &agent)["reported"] == true);

    hc_in(&s, &sender, &["send", &agent, "review it"]);
    wait_until("tagged", || text(&s, &agent).contains(&format!("got:<hc from=\"{sender} boss\">review it</hc>")));
    s.json(&["send", &agent, "from a person"]);
    let line = |t: &str, want: &str| text(&s, t).lines().any(|l| l.trim_end() == want);
    wait_until("a person's send is untouched", || line(&agent, "got:from a person"));
    hc_in(&s, &sender, &["send", &shell, "ls"]);
    wait_until("a shell gets the command as is", || line(&shell, "got:ls"));
}

#[test]
fn notes_off_means_plain_messages_too() {
    // `notes = false`: nothing hc adds on its own, tags included.
    let s = session();
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "notes = false\n").unwrap();
    let echo = "stty -echo; r state=idle; while read -r l; do echo \"got:$l\"; done";
    let sender = run(&s, &["--name", "boss"], echo);
    let agent = run(&s, &[], echo);
    wait_until("reported", || get(&s, &sender)["reported"] == true && get(&s, &agent)["reported"] == true);
    hc_in(&s, &sender, &["send", &agent, "review it"]);
    wait_until("arrived", || text(&s, &agent).contains("review it"));
    assert!(text(&s, &agent).lines().any(|l| l.trim_end() == "got:review it"), "{}", text(&s, &agent));
    // A child of an agent gets stuck: no note either.
    let kid = run(&s, &["--parent", &sender], "r state=working; w a; r state=blocked; sleep 100");
    touch(&s, "a");
    wait_until("blocked", || get(&s, &kid)["status"]["state"] == "blocked");
    std::thread::sleep(Duration::from_millis(1000));
    assert!(!text(&s, &sender).contains("<hc>"), "{}", text(&s, &sender));
}

#[test]
fn no_notes_into_programs_that_are_not_agents() {
    let s = session();
    let p = run(&s, &[], "stty -echo; while read -r l; do echo \"parent-got:$l\"; done");
    let kid = run(&s, &["--parent", &p], "r state=working; w a; r state=blocked; sleep 100");
    touch(&s, "a");
    wait_until("blocked", || get(&s, &kid)["status"]["state"] == "blocked");
    std::thread::sleep(Duration::from_millis(1000));
    assert!(!text(&s, &p).contains("<hc>"));
}

/// `hc wait --children <mode>` inside `p`, in the background.
fn wait_children(s: &Session, p: &str, mode: &str) -> std::thread::JoinHandle<Value> {
    let mut cmd = s.cmd();
    cmd.env("HC_TERMINAL_ID", p).args(["wait", "--children", mode, "--timeout", "10"]);
    std::thread::spawn(move || serde_json::from_slice::<Value>(&cmd.output().unwrap().stdout).unwrap())
}

fn ended(r: &Value) -> Vec<String> {
    let kids = r["children"].as_array().unwrap().iter();
    kids.filter(|c| c["ended"] == true).map(|c| c["terminal"].as_str().unwrap().to_string()).collect()
}

#[test]
fn wait_for_all_children_means_every_one_of_them() {
    let s = session();
    let p = run(&s, &[], "sleep 100");
    let idle = run(&s, &["--parent", &p], "r state=idle; sleep 100");
    let busy = run(&s, &["--parent", &p], "r state=working; w a; r state=done; sleep 100");
    let starting = run(&s, &["--parent", &p], "w b; r state=working; r state=done; sleep 100");
    wait_until("ready", || {
        get(&s, &busy)["status"]["state"] == "working" && get(&s, &idle)["status"]["state"] == "idle"
    });

    let all = wait_children(&s, &p, "all");
    std::thread::sleep(Duration::from_millis(300));
    touch(&s, "a");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!all.is_finished(), "a child that hasn't reported yet is still starting");
    touch(&s, "b");
    let r = all.join().unwrap();
    assert_eq!(r["ended"], true);
    assert_eq!(ended(&r), [idle, busy, starting], "an idle child counts as stopped");
}

#[test]
fn all_children_means_none_working_at_the_end_and_lists_the_finished_too() {
    let s = session();
    let p = run(&s, &[], "sleep 100");
    let gone = run(&s, &["--parent", &p], "exit 7");
    let late = run(&s, &["--parent", &p], "r state=idle; w a; r state=working; w b; r state=done; sleep 100");
    let busy = run(&s, &["--parent", &p], "r state=working; w c; r state=done; sleep 100");
    wait_until("ready", || {
        get(&s, &late)["status"]["state"] == "idle" && get(&s, &busy)["status"]["state"] == "working"
    });
    wait_until("gone", || get(&s, &gone)["state"] == "finished");
    let all = wait_children(&s, &p, "all");
    std::thread::sleep(Duration::from_millis(300));
    touch(&s, "a"); // the idle one starts working while we wait
    wait_until("late working", || get(&s, &late)["status"]["state"] == "working");
    touch(&s, "c");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!all.is_finished(), "one is still working");
    touch(&s, "b");
    let r = all.join().unwrap();
    let gone_result = r["children"].as_array().unwrap().iter().find(|c| c["terminal"] == gone.as_str()).cloned();
    assert_eq!(gone_result.map(|c| c["exit_code"].clone()), Some(json!(7)), "{r}");
}

#[test]
fn any_child_waits_even_when_all_are_idle() {
    let s = session();
    let p = run(&s, &[], "sleep 100");
    let kid = run(&s, &["--parent", &p], "r state=idle; w a; r state=working; r state=done; sleep 100");
    wait_until("idle", || get(&s, &kid)["status"]["state"] == "idle");
    let any = wait_children(&s, &p, "any");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!any.is_finished(), "nothing happened yet");
    touch(&s, "a");
    assert_eq!(ended(&any.join().unwrap()), [kid]);
    let (_, err) = s.err(&["wait", "--children", "any", &p, "--output", "x"]);
    assert_eq!(err["error"]["kind"], "invalid_params", "not silently ignored");
}

#[test]
fn wait_for_any_child_returns_news_not_yet_seen() {
    let s = session();
    let p = run(&s, &[], "sleep 100");
    let a = run(&s, &["--parent", &p], "r state=working; w a; r state=done; sleep 100");
    let b = run(&s, &["--parent", &p], "r state=working; w b; r state=done; sleep 100");
    wait_until("working", || get(&s, &b)["status"]["state"] == "working");
    touch(&s, "a");
    wait_until("a done", || get(&s, &a)["status"]["state"] == "done");

    let r = hc_in(&s, &p, &["wait", "--children", "any", "--timeout", "10"]);
    assert_eq!(ended(&r), std::slice::from_ref(&a), "news from before the wait counts");
    let any = wait_children(&s, &p, "any");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!any.is_finished(), "a's news was seen");
    touch(&s, "b");
    assert_eq!(ended(&any.join().unwrap()), [b]);
}

#[test]
fn rm_children_go_once_a_waiting_parent_has_the_exit_code() {
    let s = session();
    let p = run(&s, &[], "sleep 100");
    let gone = s.json(&["run", "--bg", "--rm", "true"])["address"].as_str().unwrap().to_string();
    wait_until("removed", || s.hc(&["get", &gone]).status.code() == Some(3));

    let kid = run(&s, &["--parent", &p, "--rm"], "w a; exit 5");
    let waiting = {
        let mut cmd = s.cmd();
        cmd.env("HC_TERMINAL_ID", &p).args(["wait", &kid, "--timeout", "10"]);
        std::thread::spawn(move || serde_json::from_slice::<Value>(&cmd.output().unwrap().stdout).unwrap())
    };
    std::thread::sleep(Duration::from_millis(300));
    touch(&s, "a");
    assert_eq!(waiting.join().unwrap()["exit_code"], 5);
    wait_until("removed after the wait", || s.hc(&["get", &kid]).status.code() == Some(3));
}
