//! `hc run` works like a bash tool: it waits until the program is not
//! working (it exits, or says it is idle, done, blocked or error) and hands
//! back the output, the exit code as its own. A timeout leaves it running.

#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;

fn hc(s: &Session, args: &[&str]) -> (i32, String) {
    let out = s.hc(args);
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn initial_size_is_optional_and_applies_before_the_program_starts() {
    use serde_json::{Value, json};
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = s.connect();
    for (id, size, expected) in [(1, Value::Null, "120 200"), (2, json!({"cols": 73, "rows": 19}), "19 73")] {
        let mut params = json!({"argv": ["sh", "-c", "stty size"], "wait": true});
        if !size.is_null() {
            params["size"] = size;
        }
        let reply = conn.call(id, "terminal.run", params);
        assert_eq!(reply["result"]["output"].as_str().unwrap().trim(), expected, "{reply}");
    }
    for size in [json!({"cols": 0, "rows": 19}), json!({"cols": 73, "rows": 0}), json!({"cols": 73})] {
        let reply = conn.call(3, "terminal.run", json!({"argv": ["sh", "-c", "stty size"], "size": size}));
        assert_eq!(reply["error"]["data"]["kind"], "invalid_params", "{reply}");
    }
}

#[test]
fn initial_display_answers_the_programs_first_color_query() {
    use serde_json::json;
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = s.connect();
    // Asks for the background at once, like a TUI at startup, before any viewer attaches.
    let probe = "stty raw -echo min 0 time 10; printf '\\033]11;?\\033\\\\'; r=$(dd bs=256 count=1 2>/dev/null); \
                 stty sane; printf '%s' \"$r\" | od -An -c | tr -d ' \\n'";
    let params = json!({"argv": ["sh", "-c", probe], "wait": true, "display": {"bg": "#fbfcfb"}});
    let reply = conn.call(1, "terminal.run", params);
    assert!(reply["result"]["output"].as_str().unwrap().contains("fbfb/fcfc/fbfb"), "{reply}");
}

#[test]
fn callers_use_the_cores_terminal_identity() {
    let mut s = Session::new();
    s.env.extend(
        [("TERM", "dumb"), ("COLORTERM", ""), ("TERM_PROGRAM", "agent"), ("TERM_PROGRAM_VERSION", "1")]
            .map(|(k, v)| (k.into(), v.into())),
    );
    let probe =
        r#"sh -c 'printf "identity=%s:%s:%s:%s\n" "$TERM" "$COLORTERM" "$TERM_PROGRAM" "${TERM_PROGRAM_VERSION-}"'"#;
    let (code, out) = hc(&s, &["run", probe]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("identity=xterm-256color:truecolor:ghostty:"), "{out:?}");

    // The outer pane supplies a real PTY to the nested CLI.
    let command =
        format!("TERM=xterm-kitty COLORTERM=24bit TERM_PROGRAM=kitty TERM_PROGRAM_VERSION=0.40 {BIN} run {probe}");
    let (code, out) = hc(&s, &["run", "sh", "-c", &command]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("identity=xterm-256color:truecolor:ghostty:"), "{out:?}");

    let reply = s.connect().call(
        1,
        "terminal.run",
        serde_json::json!({
            "argv": ["sh", "-c", "printf '%s:%s:%s:%s' \"$TERM\" \"$COLORTERM\" \"$TERM_PROGRAM\" \"$TERM_PROGRAM_VERSION\""],
            "env": {"TERM": "vt100", "COLORTERM": "24bit", "TERM_PROGRAM": "client", "TERM_PROGRAM_VERSION": "2"},
            "wait": true
        }),
    );
    assert_eq!(reply["result"]["output"].as_str().unwrap().trim(), "vt100:24bit:client:2", "{reply}");
}

#[test]
fn a_quick_command_returns_its_output_and_exit_code_and_leaves_no_note() {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "0".into()));
    let parent = run(&s, &["sh", "-c", &script(&s, "r state=idle; cat")]);
    let out = s.cmd().env("HC_TERMINAL_ID", &parent).args(["run", "sh", "-c", "echo hi; exit 3"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3), "the program's exit code");
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!text(&s, &parent).contains("<hc>"), "it saw the exit itself: no note\n{}", text(&s, &parent));

    let (code, out) = hc(&s, &["run", "echo a && echo b | tr b c"]);
    assert_eq!(code, 0);
    assert!(out.contains("a\nc"), "one quoted string is a shell line: {out:?}");
}

#[test]
fn pagers_never_hold_a_run_and_huge_output_keeps_its_tail() {
    let s = Session::new();
    let repo = s.root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let r = repo.display().to_string();
    let setup = format!(
        "cd {r} && git init -q && for i in $(seq 1 150); do git -c user.name=t -c user.email=t@t commit -q --allow-empty -m c$i; done"
    );
    assert!(std::process::Command::new("sh").args(["-c", &setup]).status().unwrap().success());
    // Longer than the 120-row screen: git would open less and wait.
    let (code, out) = hc(&s, &["run", "--timeout", "10", "--cwd", &r, "git", "log", "--oneline"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(" c1\n") || out.trim_end().ends_with(" c1"), "all of it, no pager");

    let (code, out) = hc(&s, &["run", "awk 'BEGIN{for(i=1;i<=1000;i++) printf \"%0180d\\n\", i}'"]);
    assert_eq!(code, 0);
    assert!(out.len() < 60_000, "capped: {} bytes", out.len());
    assert!(out.starts_with("[hc] earlier output cut: hc read t"), "{}", &out[..100]);
    assert!(out.trim_end().ends_with(&format!("{:0180}", 1000)), "the tail is kept");
}

#[test]
fn a_program_that_says_it_stopped_working_returns_and_a_slow_one_keeps_running() {
    let s = Session::new();
    let (code, out) = hc(
        &s,
        &["run", "sh", "-c", &script(&s, "r state=working; echo busy; sleep 0.4; echo ready; r state=idle; cat")],
    );
    assert_eq!(code, 0);
    assert!(out.contains("ready") && out.contains("idle"), "back once idle, not at working: {out:?}");

    let (code, out) = hc(&s, &["run", "--timeout", "0.5", "sh", "-c", "echo started; sleep 100"]);
    assert_eq!(code, 124, "timed out, like timeout(1)");
    assert!(out.contains("started") && out.contains("still running as t2"), "{out:?}");
    assert_eq!(get(&s, "t2")["state"], "live");

    let started = s.json(&["run", "--bg", "sleep", "100"]);
    assert_eq!((started["address"].as_str(), started["state"].as_str()), (Some("t3"), Some("live")), "--bg: at once");
    let full = s.json(&["run", "sh", "-c", "echo json"]);
    assert_eq!(
        (full["wait"]["exit_code"].as_i64(), full["output"].as_str().map(str::trim)),
        (Some(0), Some("json")),
        "{full}"
    );
}

#[test]
fn create_then_start_supports_pre_attach_put_and_idempotent_retry() {
    use serde_json::json;
    let s = Session::new();
    let created = s.json(&["create", "--name", "worker", "--label", "role=yolo"]);
    assert_eq!((created["state"].as_str(), created["launch"].as_str()), (Some("live"), Some("empty")));
    let info = get(&s, "worker");
    assert_eq!((info["launch"].as_str(), &info["argv"]), (Some("empty"), &json!([])));
    assert_eq!(info["labels"]["role"], "yolo");

    assert_eq!(s.err(&["--json", "send", "worker", "hi"]).1["error"]["kind"], "not_started");
    assert_eq!(s.err(&["--json", "keys", "worker", "enter"]).1["error"]["kind"], "not_started");
    assert_eq!(s.err(&["--json", "interrupt", "worker"]).1["error"]["kind"], "not_started");

    let note = s.root.join("note.txt");
    std::fs::write(&note, "staged-ok").unwrap();
    let landed = s.json(&["put", "worker", note.to_str().unwrap()])["path"].as_str().unwrap().to_string();

    let _v = framed::Viewer::attach(&s, "worker", 93, 27, json!({}));
    let resized = get(&s, "worker");
    assert_eq!((resized["cols"].as_u64(), resized["rows"].as_u64()), (Some(93), Some(27)));

    let body = format!("printf 'grid=%s;file=%s\\n' \"$(stty size)\" \"$(cat '{landed}')\"; sleep 100");
    let r1 = s.json(&["start", "worker", "--bg", "sh", "-c", &body]);
    assert_eq!(r1["address"], created["address"]);
    wait_until("grid and staged file", || text(&s, "worker").contains("grid=27 93;file=staged-ok"));
    let started = get(&s, "worker");
    assert_eq!(started["launch"], "started");
    let pid = started["pid"].as_u64().unwrap();

    let r2 = s.json(&["start", "worker", "--bg", "sh", "-c", &body]);
    assert_eq!(r2["address"], created["address"]);
    assert_eq!(get(&s, "worker")["pid"].as_u64(), Some(pid));
    assert_eq!(s.json(&["ls"])["terminals"].as_array().unwrap().len(), 1);

    assert_eq!(s.err(&["--json", "start", "worker", "--bg", "sleep", "50"]).1["error"]["kind"], "already_started");

    let quick = s.json(&["create", "--name", "quick"]);
    let (code, out) = hc(&s, &["start", "quick", "sh", "-c", "echo done-once"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("done-once"), "{out:?}");
    wait_until("quick finished", || get(&s, "quick")["state"] == "finished");
    let replay = s.json(&["start", "quick", "--bg", "sh", "-c", "echo done-once"]);
    assert_eq!(replay["address"], quick["address"]);
    assert_eq!(s.err(&["--json", "start", "quick", "--bg", "true"]).1["error"]["kind"], "already_started");
}

#[test]
fn create_handles_failed_spawn_kill_keep_and_restart_before_start() {
    let s = Session::new();
    s.json(&["create", "--keep", "--name", "retryable"]);
    assert_eq!(
        s.err(&["--json", "start", "retryable", "--bg", "/nonexistent-hc-bin-xyz"]).1["error"]["kind"],
        "spawn_failed"
    );
    let after_fail = get(&s, "retryable");
    assert_eq!((after_fail["state"].as_str(), after_fail["launch"].as_str()), (Some("live"), Some("empty")));

    let (code, out) = hc(&s, &["start", "retryable", "sh", "-c", "echo recovered"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("recovered"), "{out:?}");

    s.json(&["create", "--keep", "--name", "doomed"]);
    s.json(&["server", "restart"]);
    let after_restart = get(&s, "doomed");
    assert_eq!((after_restart["state"].as_str(), after_restart["launch"].as_str()), (Some("live"), Some("empty")));

    s.json(&["kill", "doomed"]);
    let killed = get(&s, "doomed");
    assert_eq!((killed["state"].as_str(), killed["reason"].as_str()), (Some("finished"), Some("killed")));
    assert_eq!(s.err(&["--json", "start", "doomed", "--bg", "true"]).1["error"]["kind"], "finished");
}

#[test]
fn concurrent_starts_with_template_send_first_message_once() {
    use serde_json::json;
    let s = Session::new();
    let prog = executable(
        &s,
        &format!(
            "#!/bin/sh\nstty -echo\n{}",
            script(
                &s,
                "r state=idle; while IFS= read -r line; do printf 'MSG:%s\\n' \"$line\"; r state=working; sleep 0.05; r state=done; done"
            )
        ),
    );
    let tdir = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&tdir).unwrap();
    std::fs::write(tdir.join("worker.md"), format!("---\ndescription: test worker\ncommand: {prog}\n---\nPrefix."))
        .unwrap();

    s.json(&["create", "--name", "tmpl"]);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut c1 = s.connect();
    let mut c2 = s.connect();
    let b1 = barrier.clone();
    let t1 = std::thread::spawn(move || {
        b1.wait();
        c1.call(1, "terminal.start", json!({"terminal": "tmpl", "argv": ["worker", "only-once"]}))
    });
    let t2 = std::thread::spawn(move || {
        barrier.wait();
        c2.call(1, "terminal.start", json!({"terminal": "tmpl", "argv": ["worker", "only-once"]}))
    });
    let r1 = t1.join().unwrap();
    let r2 = t2.join().unwrap();
    assert!(r1["error"].is_null() && r2["error"].is_null(), "r1={r1} r2={r2}");
    assert_eq!(r1["result"]["address"], r2["result"]["address"]);
    assert_eq!(r1["result"]["pid"], r2["result"]["pid"]);
    assert_eq!(
        (&r1["result"]["first_message"]["sent"], &r2["result"]["first_message"]["sent"]),
        (&json!(true), &json!(true))
    );

    wait_until("first message received", || text(&s, "tmpl").contains("MSG:only-once"));
    std::thread::sleep(std::time::Duration::from_millis(150));
    let screen = text(&s, "tmpl");
    assert_eq!(screen.matches("only-once").count(), 1, "{screen}");
}
