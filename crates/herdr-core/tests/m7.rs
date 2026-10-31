#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::Value;
use std::time::Duration;

fn session() -> Session {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "0".into()));
    s.env.push(("HC_STARTED_MS".into(), "1000".into()));
    s.env.push(("HC_READY_MS".into(), "1500".into()));
    s
}

fn template(s: &Session, name: &str, body: &str) {
    let dir = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.md")), body).unwrap();
}

/// An agent that says idle, then echoes each line it reads, working while it does.
const ECHO_AGENT: &str = r#"sh -c 'printf "\033]7501;state=idle\033\\"; stty -echo; while read -r l; do printf "\033]7501;state=working\033\\"; echo "got:$l"; printf "\033]7501;state=done\033\\"; done'"#;

#[test]
fn a_template_starts_its_command_and_sends_body_and_prompt_first() {
    let s = session();
    template(
        &s,
        "reviewer",
        &format!(
            "---\nname: reviewer\ndescription: Read-only code review.\ncommand: {ECHO_AGENT}\n---\nYou review code.\nBe brief.\n"
        ),
    );
    let r = s.json(&["run", "--bg", "reviewer", "review src/relay.rs"]);
    let t = r["address"].as_str().unwrap().to_string();
    assert_eq!(r["template"], "reviewer");
    assert_eq!(s.json(&["get", &t])["argv"], serde_json::json!(["sh", "-c", ECHO_AGENT]));
    assert_eq!(
        (r["first_message"]["sent"].as_bool(), r["first_message"]["started"].as_bool()),
        (Some(true), Some(true))
    );
    wait_until("first message", || {
        let t = text(&s, &t);
        t.contains("got:You review code.") && t.contains("got:Be brief.") && t.contains("got:review src/relay.rs")
    });
    assert_eq!(s.json(&["get", &t])["template"], "reviewer");

    let list = s.json(&["templates"]);
    assert_eq!(list["templates"][0]["name"], "reviewer");
    assert_eq!(list["templates"][0]["description"], "Read-only code review.");
}

#[test]
fn an_agent_starting_a_worker_tells_it_who_asked_and_how_to_answer() {
    let s = session();
    template(&s, "reviewer", &format!("---\ndescription: r\ncommand: {ECHO_AGENT}\n---\nYou review code.\n"));
    let boss =
        s.json(&["run", "--bg", "--name", "boss", "sh", "-c", r#"printf '\033]7501;state=idle\033\\'; sleep 100"#]);
    let boss = boss["address"].as_str().unwrap().to_string();
    wait_until("boss reported", || s.json(&["get", &boss])["reported"] == true);
    let out = s.cmd().env("HC_TERMINAL_ID", &boss).args(["run", "--bg", "reviewer", "review x"]).output().unwrap();
    let t: Value = serde_json::from_slice(&out.stdout).unwrap();
    let t = t["address"].as_str().unwrap().to_string();
    let open = format!("got:<hc from=\"{boss} boss\" reach=\"hc send {boss} when done or stuck\">You review code.");
    wait_until("tagged first message", || {
        let text = text(&s, &t);
        text.contains(&open) && text.contains("got:review x</hc>")
    });
}

#[test]
fn hc_flags_after_a_template_task_are_still_hcs() {
    let s = session();
    template(&s, "reviewer", &format!("---\ndescription: r\ncommand: {ECHO_AGENT}\n---\n"));
    let cwd = s.root.display().to_string();
    let r =
        s.json(&["run", "--bg", "--cwd", &cwd, "reviewer", "review x", "--name", "r1", "--label=k=v", "--no-notify"]);
    assert_eq!((r["name"].as_str(), r["first_message"]["sent"].as_bool()), (Some("r1"), Some(true)));
    let got = s.json(&["get", "r1"]);
    assert_eq!((got["labels"]["k"].as_str(), got["notify"].as_bool()), (Some("v"), Some(false)));
    assert_eq!(got["cwd"], cwd.as_str(), "flags before the template stay too");
    wait_until("task", || text(&s, "r1").contains("got:review x"));
    assert!(!text(&s, "r1").contains("--name"), "{}", text(&s, "r1"));
    // A flag after the task without its value is a mistake, not a program to run.
    let (code, err) = s.err(&["run", "--bg", "reviewer", "review x", "--name"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (2, Some("usage")), "{err}");
    let (code, _) = s.err(&["run", "--bg", "--no-parent", "reviewer", "review x", "--parent", "t1"]);
    assert_eq!(code, 2, "conflicting flags on both sides");
    // A program's own arguments stay its own.
    let p = s.json(&["run", "--bg", "sh", "-c", "echo args:$*; sleep 100", "sh", "--name", "x"])["address"]
        .as_str()
        .unwrap()
        .to_string();
    wait_until("program args", || text(&s, &p).contains("args:--name x"));
    // --json after the task, in a terminal (where output is for people by default).
    let line = format!("{} run --bg reviewer 'review y' --name r2 --json; sleep 100", s.bin.display());
    let caller = s.json(&["run", "--bg", "sh", "-c", &line])["address"].as_str().unwrap().to_string();
    wait_until("json printed", || text(&s, &caller).contains("\"address\""));
}

#[test]
fn templates_are_read_fresh_and_programs_still_run_by_name() {
    let s = session();
    // Commands are shell lines: leading VAR=value assignments work.
    template(&s, "echo", "---\ndescription: one\ncommand: V=first-version sh -c 'echo got-$V; sleep 100'\n---\n");
    let a = s.json(&["run", "--bg", "echo"])["address"].as_str().unwrap().to_string();
    template(&s, "echo", "---\ndescription: two\ncommand: sh -c 'echo second-version; sleep 100'\n---\n");
    let b = s.json(&["run", "--bg", "echo"])["address"].as_str().unwrap().to_string();
    wait_until("versions", || text(&s, &a).contains("got-first-version") && text(&s, &b).contains("second-version"));
    assert_eq!(s.json(&["templates"])["templates"][0]["description"], "two");

    // Not a template: a plain program, as before.
    let plain = s.json(&["run", "--bg", "sh", "-c", "echo plain; sleep 100"]);
    assert!(plain["template"].is_null());
}

#[test]
fn the_first_message_waits_for_the_agent_to_be_ready() {
    let s = session();
    // Never reports status: by default nothing is typed (it could be a dialog).
    template(
        &s,
        "mute",
        "---\ncommand: sh -c 'stty -echo; echo Trust this folder?; read -r l; echo got:$l; sleep 100'\n---\nhello\n",
    );
    let r = s.json(&["run", "--bg", "mute", "go"]);
    assert_eq!(r["first_message"]["sent"], false);
    assert!(r["first_message"]["reason"].as_str().unwrap().contains("never reported"));
    let t = r["address"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(300));
    assert!(!text(&s, &t).contains("got:"));
    assert_eq!(s.json(&["get", &t])["state"], "live");

    // An agent without status can opt into "ready when output goes quiet".
    template(
        &s,
        "quiet",
        "---\nready: quiet\ncommand: sh -c 'stty -echo; echo banner; while read -r l; do echo got:$l; done'\n---\nhello\n",
    );
    let r = s.json(&["run", "--bg", "quiet", "go"]);
    assert_eq!(r["first_message"]["sent"], true);
    let t = r["address"].as_str().unwrap().to_string();
    wait_until("sent after quiet", || text(&s, &t).contains("got:go"));

    // A program that exits before it is ready, even one removed at exit, says so.
    template(&s, "broken", "---\ncommand: sh -c 'exit 3'\n---\nhello\n");
    let r = s.json(&["run", "--bg", "--rm", "broken", "go"]);
    assert!(r["first_message"]["reason"].as_str().unwrap_or("").contains("exited"), "{r}");

    // Output that never goes quiet: not ready, nothing typed.
    template(
        &s,
        "noisy",
        "---\nready: quiet\ncommand: sh -c 'stty -echo; while :; do echo tick; sleep 0.1; done'\n---\nhello\n",
    );
    let r = s.json(&["run", "--bg", "noisy", "go"]);
    assert_eq!(r["first_message"]["sent"], false, "{r}");
}

#[test]
fn a_template_can_override_how_messages_are_submitted() {
    let s = session();
    template(
        &s,
        "tabby",
        &format!(
            "---\nsubmit: tab\nstop: ctrl+g\npaste_gap_ms: 50\ncommand: printf '\\033]7501;state=idle\\033\\\\'; {BYTES}\n---\n"
        ),
    );
    let r = s.json(&["run", "--bg", "tabby", "x"]);
    let t = r["address"].as_str().unwrap().to_string();
    wait_until("submitted with tab", || {
        let bytes: Vec<String> = text(&s, &t).split_whitespace().map(String::from).collect();
        bytes.ends_with(&["78".to_string(), "09".to_string()])
    });
    s.json(&["interrupt", &t]);
    wait_until("stopped with its own key", || text(&s, &t).split_whitespace().last() == Some("07"));
    let (code, err) = s.err(&["run", "--bg", "nosuch-template-or-program"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (1, Some("spawn_failed")));
    let _: Value = s.json(&["templates"]);
}
