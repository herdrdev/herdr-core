#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use std::io::BufRead;

fn config(s: &Session, toml: &str) {
    let path = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, toml).unwrap();
}

#[test]
fn headless_size_and_history_come_from_config() {
    let s = Session::new();
    config(&s, "headless = \"100x30\"\nhistory_mb = 0.05\n");
    let t = s.json(&[
        "run",
        "--bg",
        "sh",
        "-c",
        "i=0; while [ $i -lt 6000 ]; do echo line-$i-padding; i=$((i+1)); done; sleep 100",
    ]);
    let t = t["address"].as_str().unwrap().to_string();
    let rec = s.json(&["get", &t]);
    assert_eq!((rec["cols"].as_u64(), rec["rows"].as_u64()), (Some(100), Some(30)));
    wait_until("output", || s.json(&["read", &t])["text"].as_str().unwrap().contains("line-5999-"));
    let text = s.json(&["read", &t, "--lines", "10000"])["text"].as_str().unwrap().to_string();
    assert!(!text.contains("line-0-padding"), "history kept past its budget");

    config(&s, "headless = \"wide\"\n");
    let (code, err) = s.err(&["run", "--bg", "true"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (8, Some("config")));
}

#[test]
fn long_session_names_fit_in_a_socket_path_or_say_so() {
    // macOS allows 104 bytes for a socket path; the session name is in it twice.
    let mut s = Session::new();
    s.env.push(("HC_SESSION".into(), "a".repeat(40)));
    let t = run(&s, &["sleep", "100"]);
    assert_eq!(s.json(&["get", &t])["state"], "live");
    s.hc(&["server", "stop"]);

    let mut s = Session::new();
    s.env.push(("HC_SESSION".into(), "a".repeat(70)));
    let (code, err) = s.err(&["--json", "ls"]);
    assert_ne!(code, 0);
    assert!(err.to_string().contains("too long"), "{err}");
}

#[test]
fn the_socket_can_live_elsewhere() {
    let mut s = Session::new();
    let custom = s.root.join("elsewhere/core.sock");
    s.env.push(("HC_SOCKET".into(), custom.display().to_string()));
    s.json(&["server", "start"]);
    assert!(custom.exists() && !s.socket().exists());

    let t = Session::new();
    let configured = t.root.join("configured.sock");
    config(&t, &format!("socket = \"{}\"\n", configured.display()));
    t.json(&["server", "start"]);
    assert!(configured.exists());
}

#[test]
fn a_shell_looking_at_the_wrong_socket_is_told_where_the_core_is() {
    let s = Session::new();
    s.json(&["server", "start"]);
    // A login (ssh, su) with a different runtime dir looks somewhere else.
    let started = std::time::Instant::now();
    let out = s.cmd().env("XDG_RUNTIME_DIR", s.root.join("other")).arg("ls").output().unwrap();
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    let message = err["error"]["message"].as_str().unwrap();
    assert!(message.contains(&s.socket().display().to_string()), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "answered without waiting for a core");
}

#[test]
fn a_label_filtered_subscriber_hears_a_terminal_leave_its_filter() {
    let s = Session::new();
    let t = s.json(&["run", "--bg", "--label", "team=red", "sleep", "100"])["address"].as_str().unwrap().to_string();
    let mut events =
        s.cmd().args(["events", "--label", "team=red"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut lines = std::io::BufReader::new(events.stdout.take().unwrap()).lines();
    let snapshot: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(snapshot["terminals"][0]["address"], t.as_str());
    s.json(&["label", &t, "team=blue"]);
    let e: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(
        (e["params"]["type"].as_str(), e["params"]["data"]["labels"]["team"].as_str()),
        (Some("labels"), Some("blue")),
        "{e}"
    );
    assert!(e["params"]["data"].get("record").is_none(), "leaving needs no record");
    // Entering the filter: the event brings the whole record, so the view can add it.
    let u = s.json(&["run", "--bg", "--name", "newcomer", "sleep", "100"])["address"].as_str().unwrap().to_string();
    s.json(&["label", &u, "team=red"]);
    let e: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    events.kill().unwrap();
    events.wait().unwrap();
    assert_eq!(e["params"]["terminal"], u.as_str());
    assert_eq!(e["params"]["data"]["record"]["name"], "newcomer", "{e}");
}

#[test]
fn events_carry_every_changing_fact() {
    let s = Session::new();
    let mut events = s.cmd().args(["events"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut lines = std::io::BufReader::new(events.stdout.take().unwrap()).lines();
    lines.next().unwrap().unwrap(); // the snapshot
    let t = s.json(&["run", "--bg", "sh", "-c", r"printf '\033]7501;state=working\033\\'; sleep 100"])["address"]
        .as_str()
        .unwrap()
        .to_string();
    let mut next = |kind: &str| loop {
        let e: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        if e["params"]["type"] == kind {
            break e["params"]["data"].clone();
        }
    };
    assert_eq!(next("status")["reported"], true);
    s.json(&["kill", &t]);
    let finished = next("finished");
    events.kill().unwrap();
    events.wait().unwrap();
    assert!(finished["finished_ms"].as_u64().is_some(), "{finished}");
}

#[test]
fn events_can_follow_one_parents_children() {
    let s = Session::new();
    let p = s.json(&["run", "--bg", "sleep", "100"])["address"].as_str().unwrap().to_string();
    let mut events =
        s.cmd().args(["events", "--children-of", &p]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut lines = std::io::BufReader::new(events.stdout.take().unwrap()).lines();
    lines.next().unwrap().unwrap(); // the snapshot
    s.json(&["run", "--bg", "true"]); // not a child: no event
    let kid = s.json(&["run", "--bg", "--parent", &p, "true"])["address"].as_str().unwrap().to_string();
    let e: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    events.kill().unwrap();
    events.wait().unwrap();
    assert_eq!(e["params"]["terminal"], kid.as_str());
}

#[test]
fn configured_shell_runs_its_arguments_and_leaves_explicit_programs_alone() {
    let mut s = Session::new();
    s.env.push(("SHELL".into(), "/bin/sh".into()));
    let t = run(&s, &[]);
    s.json(&["send", &t, "echo default-shell-works"]);
    wait_until("default shell", || text(&s, &t).contains("default-shell-works"));
    config(&s, r#"shell = ["/bin/sh", "-c", "echo configured-shell; exec /bin/sh"]"#);
    let t = run(&s, &[]);
    wait_until("configured shell", || text(&s, &t).contains("configured-shell"));
    let t = run(&s, &["sh", "-c", "echo explicit-program"]);
    wait_until("explicit program", || text(&s, &t).contains("explicit-program"));
    assert!(!text(&s, &t).contains("configured-shell"));
    for (value, kind) in [("[]", "config"), (r#"[""]"#, "config"), (r#"["hc-missing-shell-24"]"#, "spawn_failed")] {
        config(&s, &format!("shell = {value}"));
        assert_eq!(s.err(&["run", "--bg"]).1["error"]["kind"], kind);
    }
}
