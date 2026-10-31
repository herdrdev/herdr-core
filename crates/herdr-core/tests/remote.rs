#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::Viewer;
use common::*;
use serde_json::{Value, json};

/// A local core with `rohan` added (reconnects within 200 ms in tests).
fn with_rohan() -> (Session, Session) {
    with_rohan_env(&[])
}

fn with_rohan_env(env: &[(&str, &str)]) -> (Session, Session) {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.env.push(("HC_RECONNECT_MS".into(), "200".into()));
    s.env.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    s.json(&["machine", "add", "rohan"]);
    (rohan, s)
}

#[test]
fn a_frozen_machine_never_freezes_hc() {
    // rohan answers heartbeats every 200 ms; here, 2 s is all a machine gets.
    let mut rohan = Session::new();
    rohan.env.push(("HC_HEARTBEAT_MS".into(), "200".into()));
    let pid = rohan.json(&["server", "status"])["pid"].as_i64().unwrap() as i32;
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.env.extend([("HC_RECONNECT_MS", "200"), ("HC_HEARTBEAT_MS", "200")].map(|(k, v)| (k.into(), v.into())));
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "machine_timeout = 2\n").unwrap();
    s.json(&["machine", "add", "rohan"]);
    let state = |s: &Session| s.json(&["machine", "ls"])["machines"][0]["state"].clone();
    wait_until("connected", || state(&s) == "connected");

    unsafe { libc::kill(pid, libc::SIGSTOP) }; // alive, connected, answering nothing
    let started = std::time::Instant::now();
    // Not a wait: that one rides out the lost line until its own timeout.
    let (_, err) = s.err(&["run", "--bg", "--on", "rohan", "true"]);
    let took = started.elapsed();
    let thawed = || unsafe { libc::kill(pid, libc::SIGCONT) };
    if took > std::time::Duration::from_secs(6) {
        thawed();
        panic!("a call to a frozen machine took {took:?}: {err}");
    }
    assert_eq!(err["error"]["kind"], "unavailable", "{err}");
    let quiet = std::time::Instant::now();
    while state(&s) == "connected" && quiet.elapsed() < std::time::Duration::from_secs(8) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let after = state(&s);
    // Redialing a frozen machine doesn't hold calls past their deadline either.
    let started = std::time::Instant::now();
    let (_, err) = s.err(&["run", "--bg", "--on", "rohan", "true"]);
    let took = started.elapsed();
    thawed();
    assert_ne!(after, "connected", "a silent line is given up");
    assert!(took < std::time::Duration::from_secs(6), "a call while it redials took {took:?}: {err}");
    wait_until("back once it answers again", || state(&s) == "connected");
}

#[test]
fn a_big_answer_on_a_slow_link_is_not_mistaken_for_a_frozen_machine() {
    let mut rohan = Session::new();
    rohan.env.push(("HC_HEARTBEAT_MS".into(), "200".into()));
    let mut s = Session::new();
    let (var, fake) = fake_ssh(&s, &[("rohan", &rohan)]);
    // Everything rohan sends comes at ~320 KB/s; 0.7 s of silence is all a machine gets.
    let slow = s.root.join("slow-ssh");
    let throttle = "import sys, time\nwhile b := sys.stdin.buffer.read1(32768):\n    sys.stdout.buffer.write(b); sys.stdout.buffer.flush(); time.sleep(0.1)";
    std::fs::write(&slow, format!("#!/bin/sh\n{fake} \"$@\" | python3 -c '{throttle}'\n")).unwrap();
    std::fs::set_permissions(&slow, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push((var, slow.display().to_string()));
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "machine_timeout = 0.5\n").unwrap();
    s.env.push(("HC_HEARTBEAT_MS".into(), "200".into()));
    rohan.json(&["server", "start"]);
    s.json(&["machine", "add", "rohan"]);

    std::thread::sleep(std::time::Duration::from_secs(1)); // idle, heartbeats keep it up
    let r = s.json(&["exec", "--on", "rohan", "head", "-c", "750000", "/dev/zero"]);
    assert_eq!(r["stdout"].as_str().unwrap().len(), 1_000_000, "took ~3 s to arrive, still flowing");
}

fn addresses(list: &Value) -> Vec<String> {
    list["terminals"].as_array().unwrap().iter().map(|t| t["address"].as_str().unwrap().to_string()).collect()
}

#[test]
fn two_machines_that_added_each_other_list_at_once() {
    let (mut rohan, mut s) = (Session::new(), Session::new()); // s stops first: it would dial rohan back up
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    rohan.env.push(fake_ssh(&rohan, &[("desk", &s)]));
    // Each core started with its own way to the other.
    let (here, there) = (run(&s, &["sleep", "100"]), run(&rohan, &["sleep", "100"]));
    s.json(&["machine", "add", "rohan"]);
    rohan.json(&["machine", "add", "desk"]);
    let wants = [[format!("rohan/{there}"), here.clone()], [format!("desk/{here}"), there]];
    for (s, want) in [&s, &rohan].into_iter().zip(wants) {
        let started = std::time::Instant::now();
        let list = s.json(&["ls"]);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "took {:?}", started.elapsed());
        let mut got = addresses(&list);
        got.sort();
        assert_eq!(got, want, "each lists its own and the other's, nothing relayed back");
    }
    rohan.json(&["machine", "rm", "desk"]); // else it dials a stopped desk back up
}

#[test]
fn two_machines_that_added_each_other_never_echo_events_back() {
    let (mut rohan, mut s) = (Session::new(), Session::new()); // s stops first: it would dial rohan back up
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    rohan.env.push(fake_ssh(&rohan, &[("desk", &s)]));
    run(&s, &["sleep", "100"]);
    run(&rohan, &["sleep", "100"]);
    s.json(&["machine", "add", "rohan"]);
    rohan.json(&["machine", "add", "desk"]);
    wait_until("both connected", || {
        [&s, &rohan].iter().all(|s| s.json(&["machine", "ls"])["machines"][0]["state"] == "connected")
    });
    let mut c = s.connect();
    c.call(1, "events.subscribe", json!({}));
    c.call(2, "doc.set", json!({"name": "app", "value": 1, "version": 0}));
    // Two sleeping terminals and one document change: a handful of events, not a storm.
    c.reader.get_ref().set_read_timeout(Some(std::time::Duration::from_millis(100))).unwrap();
    let (started, mut events) = (std::time::Instant::now(), 0);
    while started.elapsed() < std::time::Duration::from_secs(2) {
        let mut line = String::new();
        events += usize::from(std::io::BufRead::read_line(&mut c.reader, &mut line).is_ok_and(|n| n > 0));
    }
    assert!(events < 20, "{events} events in 2 s");
    rohan.json(&["machine", "rm", "desk"]); // else it dials a stopped desk back up
}

#[test]
fn terminals_on_another_machine_work_like_local_ones() {
    let (rohan, s) = with_rohan_env(&[("HC_TEST_SSH_DELAY", "0.4")]); // a login as slow as a real one
    let local = run(&s, &["sh", "-c", "sleep 100"]);
    let t = run(&s, &["--on", "rohan", "--name", "echo", "sh", "-c", "pwd; while read l; do echo got-$l; done"]);
    assert_eq!(t, "rohan/t1");
    assert_eq!(addresses(&rohan.json(&["ls"])), ["t1"], "on rohan it is its own");
    assert_eq!(addresses(&s.json(&["ls"])), [local.as_str(), "rohan/t1"]);

    wait_until("its output", || text(&s, &t).contains(&rohan.root.display().to_string()));
    s.json(&["send", "rohan/echo", "hello"]);
    wait_until("the reply", || text(&s, "rohan/echo").contains("got-hello"));
    assert_eq!(get(&s, &t)["name"], "echo");

    let done = run(&s, &["--on", "rohan", "sh", "-c", "sleep 0.3; exit 3"]);
    let w = s.json(&["wait", &done]);
    assert_eq!((w["terminal"].as_str(), w["exit_code"].as_i64()), (Some(done.as_str()), Some(3)), "{w}");
    // A restart there closes the line; a call right then waits for the redial.
    rohan.hc(&["server", "restart"]);
    assert!(text(&s, &t).contains("got-hello"), "no 'unreachable' in between");
    assert_eq!(s.json(&["kill", &t])["state"], "finished");
    s.json(&["rm", &t]);
    assert_eq!(s.err(&["--json", "read", "nope/t1"]).1["error"]["kind"], "machine_unknown");
}

/// A local agent (it reports status; what is typed into it shows on its
/// screen) that starts `child` on rohan; returns both addresses.
fn parent_with_child_on_rohan(s: &Session, child: &str) -> (String, String) {
    let parent = run(s, &["sh", "-c", &script(s, "r state=idle; cat")]);
    wait_until("the parent reports", || get(s, &parent)["reported"] == true);
    let out = s
        .cmd()
        .env("HC_TERMINAL_ID", &parent)
        .args(["--json", "run", "--bg", "--on", "rohan", "sh", "-c", child])
        .output()
        .unwrap();
    let started: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(started["parent"], parent.as_str(), "{started}");
    (parent, started["address"].as_str().unwrap().into())
}

#[test]
fn a_child_on_another_machine_reports_back_to_its_parent() {
    let (rohan, s) = with_rohan_env(&[("HC_QUIET_MS", "0")]);
    let (parent, child) = parent_with_child_on_rohan(&s, &script(&s, "r state=working; w go; exit 3"));
    let parent_there = get(&rohan, "t1")["parent"].as_str().unwrap().to_string();
    assert!(parent_there.ends_with(&format!("/{parent}")), "{parent_there}");

    let said = rohan.cmd().env("HC_TERMINAL_ID", "t1").args(["send", &parent_there, "hi-parent"]).output().unwrap();
    assert!(said.status.success(), "{}", String::from_utf8_lossy(&said.stderr));
    wait_until("the child's message", || text(&s, &parent).contains(&format!("<hc from=\"{child}\">hi-parent</hc>")));

    let other = run(&s, &["sh", "-c", &script(&s, "r state=idle; cat")]);
    let prefix = parent_there.split('/').next().unwrap();
    let (_, refused) = rohan.err(&["--json", "send", &format!("{prefix}/{other}"), "x"]);
    assert_eq!(refused["error"]["kind"], "denied", "only its parents: {refused}");

    touch(&s, "go");
    wait_until("the exit note", || {
        text(&s, &parent).contains(&format!("<hc>{child} exited (exit 3). Read: hc read {child}</hc>"))
    });
}

#[test]
fn waiting_on_children_counts_the_ones_on_other_machines() {
    let (_rohan, s) = with_rohan();
    let (parent, child) = parent_with_child_on_rohan(&s, &script(&s, "r state=working; w go; r state=idle; sleep 100"));
    let wait = |mode: &str, timeout: &str| {
        let out = s
            .cmd()
            .env("HC_TERMINAL_ID", &parent)
            .args(["--json", "wait", "--children", mode, "--timeout", timeout])
            .output()
            .unwrap();
        serde_json::from_slice::<Value>(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)))
    };
    let still = wait("all", "0.5");
    assert_eq!((&still["ended"], &still["children"][0]["terminal"]), (&json!(false), &json!(child)), "{still}");

    touch(&s, "go");
    let any = wait("any", "10");
    assert_eq!(
        (&any["ended"], &any["terminal"], &any["children"][0]["terminal"]),
        (&json!(true), &json!(parent), &json!(child)),
        "{any}"
    );
    let all = wait("all", "10");
    assert_eq!((&all["ended"], &all["children"][0]["status"]), (&json!(true), &json!("idle")), "{all}");
}

#[test]
fn a_child_ends_with_its_parent_on_another_machine_too() {
    let (rohan, s) = with_rohan();
    // `--rm`: the parent is forgotten at once, but its children there still hear.
    let parent = run(&s, &["--rm", "sh", "-c", &script(&s, "r state=idle; w go")]);
    let out =
        s.cmd().env("HC_TERMINAL_ID", &parent).args(["run", "--bg", "--on", "rohan", "sleep", "100"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    touch(&s, "go");
    wait_until("the child there ended", || get(&rohan, "t1")["state"] == "finished");
}

#[test]
fn a_note_made_while_the_line_is_down_arrives_when_it_is_back() {
    let (rohan, s) = with_rohan_env(&[("HC_QUIET_MS", "0")]);
    let (parent, child) = parent_with_child_on_rohan(&s, &script(&s, "r state=working; w go; exit 4"));
    std::fs::write(s.root.join("down"), "").unwrap();
    rohan.hc(&["server", "restart"]);
    wait_until("rohan unreachable", || s.json(&["machine", "ls"])["machines"][0]["state"] == "unreachable");
    touch(&s, "go");
    wait_until("it exited there", || get(&rohan, "t1")["state"] == "finished");
    rohan.hc(&["server", "restart"]); // the note is kept across a restart there too
    std::fs::remove_file(s.root.join("down")).unwrap();
    wait_until("the exit note", || text(&s, &parent).contains(&format!("{child} exited (exit 4)")));
}

/// The next event's params for which `pick` holds.
fn next_params(conn: &mut Conn, pick: impl Fn(&Value) -> bool) -> Value {
    next_event(conn, |m| m["method"] == "event" && pick(&m["params"]))["params"].clone()
}

#[test]
fn events_cover_terminals_on_other_machines() {
    let (_rohan, s) = with_rohan();
    let mut all = s.connect();
    all.call(1, "events.subscribe", json!({}));
    let t = run(&s, &["--on", "rohan", "sh", "-c", &script(&s, "r state=working; w go; r state=idle; sleep 100")]);
    let created = next_params(&mut all, |e| e["type"] == "created" && e["terminal"] == t.as_str());
    assert_eq!(created["data"]["address"], t.as_str(), "its record speaks as seen from here: {created}");

    touch(&s, "go");
    let idle = next_params(&mut all, |e| {
        e["type"] == "status" && e["terminal"] == t.as_str() && e["data"]["status"]["state"] == "idle"
    });
    assert!(idle["seq"].as_u64().unwrap() > created["seq"].as_u64().unwrap(), "one ordered stream");

    let mut one = s.connect();
    let snap = one.call(1, "events.subscribe", json!({"terminal": t}));
    let terminals = snap["result"]["terminals"].as_array().unwrap().clone();
    assert_eq!((terminals.len(), &terminals[0]["status"]["state"]), (1, &json!("idle")), "{snap}");
    s.json(&["kill", &t]);
    next_params(&mut one, |e| e["type"] == "finished" && e["terminal"] == t.as_str());
}

#[test]
fn the_picker_starts_a_shell_on_another_machine() {
    let (rohan, s) = with_rohan();
    std::fs::write(rohan.root.join(".zshrc"), "").unwrap(); // a login zsh there skips its first-run setup
    let outer = run(&s, &[&s.bin.display().to_string()]);
    let mut v = Viewer::attach(&s, &outer, 100, 30, json!({}));
    v.shows("new on rohan");
    v.typed(b"on rohan\r");
    wait_until("a shell there", || addresses(&s.json(&["ls"])).contains(&"rohan/t1".to_string()));
    v.typed(b"echo hi-from-$HC_TERMINAL_ID\r");
    v.shows("hi-from-t1"); // rohan's t1: here it would be t2
}

#[test]
fn a_shell_or_a_shell_line_there_uses_that_machines_own_shell() {
    let mut rohan = Session::new();
    let shell = rohan.root.join("rohans-shell");
    std::fs::write(&shell, "#!/bin/sh\necho rohans own shell\nexec sleep 100\n").unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    rohan.env.push(("SHELL".into(), shell.display().to_string()));
    rohan.json(&["server", "start"]);
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    let mut conn = s.connect();

    let t = conn.call(1, "terminal.run", json!({"on": "rohan"}))["result"]["address"].as_str().unwrap().to_string();
    wait_until("its shell", || text(&s, &t).contains("rohans own shell"));
    let mut own = vec![shell.display().to_string()];
    if cfg!(target_os = "macos") {
        own.push("-l".into()); // macOS starts its own shells as login shells
    }
    assert_eq!(get(&s, &t)["argv"], json!(own));

    let line = "echo $((6 * 7))";
    let r = conn.call(2, "terminal.run", json!({"on": "rohan", "line": line, "wait": true}))["result"].clone();
    assert!(r["output"].as_str().unwrap().contains("42"), "{r}");
    assert_eq!(r["argv"], json!(["sh", "-c", line]));
    let both = conn.call(3, "terminal.run", json!({"argv": ["true"], "line": "true"}));
    assert_eq!(both["error"]["data"]["kind"], "invalid_params", "{both}");
}

#[test]
fn attaching_to_a_terminal_on_another_machine() {
    let (rohan, s) = with_rohan();
    let t = run(&s, &["--on", "rohan", "sh", "-c", "echo ready; read l; echo got-$l; read l"]);
    let size = |s: &Session, t: &str| (get(s, t)["cols"].as_u64(), get(s, t)["rows"].as_u64());
    let headless = size(&rohan, "t1");

    let mut v = Viewer::attach(&s, &t, 100, 30, json!({}));
    v.shows("ready");
    assert_eq!(size(&rohan, "t1"), (Some(100), Some(30)), "the viewer's size reaches the program there");
    v.typed(b"hi\r");
    v.shows("got-hi");
    drop(v);
    wait_until("rohan's viewer gone", || size(&rohan, "t1") == headless);

    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.typed(b"bye\r");
    v.until("the end", |v| v.closed.is_some());
}

#[test]
fn a_viewer_there_keeps_up_past_one_window_of_output() {
    let (_rohan, s) = with_rohan();
    let t = run(&s, &["--on", "rohan", "sh", "-c", "read l; seq 1 100000; echo all-done; sleep 100"]);
    let mut v = Viewer::attach(&s, &t, 100, 30, json!({}));
    v.typed(b"go\r");
    v.shows("all-done");
    assert!(v.received > herdr_core::wire::WINDOW, "more than a stream starts with");
}

#[test]
fn a_viewer_rides_out_a_dropped_line_without_reattaching() {
    let (rohan, s) = with_rohan();
    let t = run(&s, &["--on", "rohan", "sh", "-c", "echo ready; while read l; do echo got-$l; done"]);
    let mut v = Viewer::attach(&s, &t, 100, 30, json!({}));
    v.shows("ready");
    std::fs::write(s.root.join("down"), "").unwrap();
    rohan.hc(&["server", "restart"]); // the line drops; the program keeps running there
    wait_until("rohan unreachable", || s.json(&["machine", "ls"])["machines"][0]["state"] == "unreachable");
    v.shows("lost"); // the viewer hears it, and stays open
    assert!(v.closed.is_none(), "the stream stays open while the line is down");
    std::fs::remove_file(s.root.join("down")).unwrap();
    // A live machine line does not mean this viewer has been reattached yet.
    // Probe until input reaches the program through this viewer's stream.
    wait_until("the viewer's input back", || {
        v.typed(b"probe\r");
        text(&rohan, "t1").contains("got-probe")
    });
    v.typed(b"after\r");
    v.shows("got-after"); // same viewer, typing and output both ways again
}

#[test]
fn a_viewer_back_after_a_dropped_line_does_not_take_the_size() {
    let (rohan, s) = with_rohan();
    let mark = rohan.root.join("mark");
    let shows_mark = format!("while :; do cat {} 2>/dev/null; sleep 0.05; done", mark.display());
    let t = run(&s, &["--on", "rohan", "sh", "-c", &shows_mark]);
    let size = |t: &str| (get(&rohan, t)["cols"].as_u64().unwrap(), get(&rohan, t)["rows"].as_u64().unwrap());
    let mut away = Viewer::attach(&s, &t, 100, 30, json!({}));
    away.conn.send(away.channel, herdr_core::wire::RESIZE, &[120, 0, 40, 0]);
    wait_until("its latest size there", || size("t1") == (120, 40));

    std::fs::write(s.root.join("down"), "").unwrap();
    rohan.hc(&["server", "restart"]);
    wait_until("rohan unreachable", || s.json(&["machine", "ls"])["machines"][0]["state"] == "unreachable");
    let here = Viewer::attach(&rohan, "t1", 80, 24, json!({}));
    assert_eq!(size("t1"), (80, 24));
    std::fs::remove_file(s.root.join("down")).unwrap();
    wait_until("rohan back", || s.json(&["machine", "ls"])["machines"][0]["state"] == "connected");
    std::fs::write(&mark, "MARK-back\n").unwrap();
    away.shows("MARK-back"); // reattached there
    assert_eq!(size("t1"), (80, 24), "coming back is not activity");

    drop(here);
    wait_until("the one left leads, at its latest size", || size("t1") == (120, 40));
}

#[test]
fn a_run_or_wait_there_rides_out_a_dropped_line() {
    let (rohan, s) = with_rohan();
    let state = |s: &Session| s.json(&["machine", "ls"])["machines"][0]["state"].clone();
    let drop_line = || {
        std::fs::write(s.root.join("down"), "").unwrap();
        rohan.hc(&["server", "restart"]);
        wait_until("rohan unreachable", || state(&s) == "unreachable");
        std::fs::remove_file(s.root.join("down")).unwrap();
    };
    // A waited run: the line drops while it runs; it still ends with its output and code.
    let body = script(&s, "echo started; w go; echo finished-fine; exit 3");
    let waited = {
        let mut cmd = s.cmd();
        cmd.args(["run", "--on", "rohan", "--timeout", "30", "sh", "-c", &body]);
        std::thread::spawn(move || cmd.output().unwrap())
    };
    wait_until("started there", || rohan.json(&["ls"])["terminals"].as_array().is_some_and(|t| !t.is_empty()));
    drop_line();
    touch(&s, "go");
    let out = waited.join().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("finished-fine"), "{text} / {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(3), "its own exit code");

    // A wait on a terminal there, the same.
    let t = run(&s, &["--on", "rohan", "sh", "-c", &script(&s, "w go2; exit 0")]);
    let waiting = {
        let mut cmd = s.cmd();
        cmd.args(["wait", &t, "--timeout", "30"]);
        std::thread::spawn(move || cmd.output().unwrap())
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    drop_line();
    touch(&s, "go2");
    let out = waiting.join().unwrap();
    let r: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    assert_eq!(r["reason"], "exit", "{r} / {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_machine_that_drops_off_is_shown_not_waited_on_and_comes_back() {
    let (rohan, s) = with_rohan();
    run(&s, &["--on", "rohan", "sh", "-c", "sleep 100"]);
    assert_eq!(s.json(&["machine", "ls"])["machines"][0]["state"], "connected");

    std::fs::write(s.root.join("down"), "").unwrap();
    rohan.hc(&["server", "restart"]); // drops the line; the terminal keeps running there
    wait_until("rohan unreachable", || s.json(&["machine", "ls"])["machines"][0]["state"] == "unreachable");
    let list = s.json(&["ls"]);
    assert_eq!(list["machines"][0]["state"], "unreachable", "{list}");
    assert!(addresses(&list).is_empty());
    assert_eq!(s.err(&["--json", "read", "rohan/t1"]).1["error"]["kind"], "unavailable");

    std::fs::remove_file(s.root.join("down")).unwrap();
    wait_until("rohan back", || s.json(&["machine", "ls"])["machines"][0]["state"] == "connected");
    assert_eq!(addresses(&s.json(&["ls"])), ["rohan/t1"]);
}
