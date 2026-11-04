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

fn wait_children(s: &Session, parent: &str, mode: &str, timeout: &str) -> Value {
    let out = s
        .cmd()
        .env("HC_TERMINAL_ID", parent)
        .args(["--json", "wait", "--children", mode, "--timeout", timeout])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn template(s: &Session, name: &str, body: &str) {
    let dir = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.md")), body).unwrap();
}

/// An agent that says idle, then echoes each line it reads, working while it does.
const ECHO_AGENT: &str = r#"sh -c 'printf "\033]7501;state=idle\033\\"; stty -echo; while read -r l; do printf "\033]7501;state=working\033\\"; echo "got:$l"; printf "\033]7501;state=done\033\\"; done'"#;

#[test]
fn first_task_can_finish_without_a_working_report() {
    for mode in ["all", "any"] {
        for state in ["done", "blocked", "error", "exit"] {
            let mut s = session();
            s.env.push(("HC_STARTED_MS".into(), "100".into()));
            let gate = s.root.join("finish");
            let end = if state == "exit" {
                "exit 7".to_string()
            } else {
                format!(r#"printf "\033]7501;state={state}\033\\"; sleep 100"#)
            };
            let command = format!(
                r#"sh -c 'stty -echo; printf "\033]7501;state=idle\033\\"; read -r task; while [ ! -f "{}" ]; do sleep 0.01; done; {end}'"#,
                gate.display()
            );
            template(&s, "worker", &format!("---\ncommand: {command}\n---\n"));
            let parent = s.json(&["run", "--bg", "sh", "-c", "sleep 100"])["address"].as_str().unwrap().to_string();
            let r = s.json(&["run", "--bg", "--parent", &parent, "worker", "first task"]);
            assert_eq!(r["first_message"]["sent"], true, "{r}");
            std::fs::write(gate, "").unwrap();
            let r = wait_children(&s, &parent, mode, "5");
            assert_eq!(r["reason"], "children", "{mode} {state}: {r}");
            assert_eq!(r["children"][0]["ended"], true, "{r}");
            if state == "exit" {
                assert_eq!(r["children"][0]["exit_code"], 7, "{r}");
            } else {
                assert_eq!(r["children"][0]["status"], state, "{r}");
            }
        }
    }
}

#[test]
fn first_task_completion_inside_started_window_is_not_forgotten() {
    for mode in ["all", "any"] {
        let s = session();
        template(
            &s,
            "worker",
            r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=idle\033\\"; read -r task; printf "\033]7501;state=done\033\\"; printf "\033]7501;state=idle\033\\"; sleep 100'
---
"#,
        );
        let parent = s.json(&["run", "--bg", "sleep", "100"])["address"].as_str().unwrap().to_string();
        let r = s.json(&["run", "--bg", "--parent", &parent, "worker", "first task"]);
        assert_eq!(r["first_message"]["sent"], true, "{r}");
        let r = wait_children(&s, &parent, mode, "0.1");
        assert_eq!(r["reason"], "children", "{mode}: {r}");
        assert_eq!(r["children"][0]["ended"], true, "{r}");
    }
}

#[test]
fn first_task_does_not_consume_a_pre_delivery_done_word() {
    let mut s = session();
    s.env.push(("HC_STARTED_MS".into(), "100".into()));
    let gate = s.root.join("finish");
    template(
        &s,
        "worker",
        &format!(
            r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=done\033\\"; read -r task; while [ ! -f "{}" ]; do sleep 0.01; done; printf "\033]7501;state=done\033\\"; sleep 100'
---
"#,
            gate.display()
        ),
    );
    let parent = s.json(&["run", "--bg", "sleep", "100"])["address"].as_str().unwrap().to_string();
    let r = s.json(&["run", "--bg", "--parent", &parent, "worker", "first task"]);
    assert_eq!(r["first_message"]["sent"], true, "{r}");
    let wait = || wait_children(&s, &parent, "all", "0.1");
    let r = wait();
    assert_eq!(r["reason"], "timeout", "startup done is not task completion: {r}");
    std::fs::write(gate, "").unwrap();
    wait_until("delivered task completed", || wait()["reason"] == "children");
}

#[test]
fn first_task_startup_done_is_not_completion_after_recovery() {
    for recovery in ["restart", "crash"] {
        startup_done_stays_pending(Some(recovery));
    }
}

#[test]
fn first_task_startup_done_is_not_completion_after_a_child_report() {
    startup_done_stays_pending(None);
}

fn startup_done_stays_pending(recovery: Option<&str>) {
    for mode in ["all", "any"] {
        let mut s = session();
        s.env.push(("HC_STARTED_MS".into(), "100".into()));
        s.env.push(("HC_HOLDER_GRACE_MS".into(), "60000".into()));
        let child_gate = s.root.join("child-report");
        let finish = s.root.join("finish");
        template(
            &s,
            "worker",
            &format!(
                r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=done\033\\"; read -r task; while [ ! -f "{}" ]; do sleep 0.01; done; printf "\033]7501;state=done:id=sub\033\\"; while [ ! -f "{}" ]; do sleep 0.01; done; printf "\033]7501;state=done\033\\"; sleep 100'
---
"#,
                child_gate.display(),
                finish.display()
            ),
        );
        let parent = s.json(&["run", "--bg", "sleep", "100"])["address"].as_str().unwrap().to_string();
        let r = s.json(&["run", "--bg", "--parent", &parent, "worker", "first task"]);
        assert_eq!(r["first_message"]["sent"], true, "{r}");
        let child = r["address"].as_str().unwrap();
        if recovery == Some("restart") {
            s.json(&["server", "restart"]);
        } else if recovery == Some("crash") {
            let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap() as i32;
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
            wait_until("core absent", || !s.connect_ok());
        } else {
            std::fs::write(&child_gate, "").unwrap();
            wait_until("child report", || s.json(&["get", child])["records"]["sub"]["state"] == "done");
        }
        let wait = |timeout: &str| wait_children(&s, &parent, mode, timeout);
        let r = wait("0.1");
        assert_eq!(r["reason"], "timeout", "{mode}: startup done is not task completion: {r}");
        assert_eq!(r["children"][0]["ended"], false, "{r}");
        std::fs::write(child_gate, "").unwrap();
        std::fs::write(finish, "").unwrap();
        let r = wait("5");
        assert_eq!(r["reason"], "children", "{mode}: fresh root done completes the task: {r}");
        assert_eq!(r["children"][0]["ended"], true, "{r}");
    }
}

#[test]
fn first_task_completed_while_core_was_absent_is_reconciled() {
    for mode in ["all", "any"] {
        let mut s = session();
        s.env.push(("HC_HOLDER_GRACE_MS".into(), "60000".into()));
        s.env.push(("HC_STARTED_MS".into(), "100".into()));
        let gate = s.root.join("finish");
        let finished = s.root.join("finished");
        template(
            &s,
            "worker",
            &format!(
                r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=idle\033\\"; read -r task; while [ ! -f "{}" ]; do sleep 0.01; done; printf "\033]7501;state=done\033\\"; touch "{}"; sleep 100'
---
"#,
                gate.display(),
                finished.display()
            ),
        );
        let parent = s.json(&["run", "--bg", "sleep", "100"])["address"].as_str().unwrap().to_string();
        let r = s.json(&["run", "--bg", "--parent", &parent, "worker", "first task"]);
        assert_eq!(r["first_message"]["sent"], true, "{r}");
        let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap() as i32;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        wait_until("core absent", || !s.connect_ok());
        std::fs::write(&gate, "").unwrap();
        wait_until("task finished without core", || finished.exists());
        let r = wait_children(&s, &parent, mode, "0.1");
        assert_eq!(r["reason"], "children", "{mode}: {r}");
        assert_eq!(r["children"][0]["ended"], true, "{r}");
        if mode == "any" {
            s.json(&["server", "restart"]);
            let r = wait_children(&s, &parent, "any", "0.1");
            assert_eq!(r["reason"], "timeout", "completion news must only be emitted once: {r}");
        }
    }
}

#[test]
fn first_task_done_then_idle_during_handover_is_not_forgotten() {
    for mode in ["all", "any"] {
        let mut s = session();
        s.env.push(("HC_HOLDER_GRACE_MS".into(), "60000".into()));
        s.env.push(("HC_STARTED_MS".into(), "100".into()));
        let gate = s.root.join("finish");
        let finished = s.root.join("finished");
        template(
            &s,
            "worker",
            &format!(
                r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=idle\033\\"; read -r task; while [ ! -f "{}" ]; do sleep 0.01; done; printf "\033]7501;state=done\033\\"; printf "\033]7501;state=idle\033\\"; touch "{}"; sleep 100'
---
"#,
                gate.display(),
                finished.display()
            ),
        );
        let parent = s.json(&["run", "--bg", "sleep", "100"])["address"].as_str().unwrap().to_string();
        let r = s.json(&["run", "--bg", "--parent", &parent, "worker", "first task"]);
        assert_eq!(r["first_message"]["sent"], true, "{r}");
        let child = r["address"].as_str().unwrap();
        let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap();
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
        wait_until("core exited gracefully", || !pid_alive(pid));
        std::fs::write(gate, "").unwrap();
        wait_until("task finished without core", || finished.exists());
        let r = wait_children(&s, &parent, mode, "0.1");
        assert_eq!(r["reason"], "children", "{mode}: {r}");
        assert_eq!(r["children"][0]["ended"], true, "{r}");
        assert_eq!(r["children"][0]["status"], "idle", "{r}");
        assert_eq!(s.json(&["get", child])["screen"], "exact");
    }
}

#[test]
fn children_wait_for_a_sent_first_task_not_startup_idle() {
    for mode in ["all", "any"] {
        let s = session();
        let gate = s.root.join("start");
        let finish = s.root.join("finish");
        let command = format!(
            r#"sh -c 'stty -echo; printf "\033]7501;state=idle\033\\"; read -r task; while [ ! -f "{}" ]; do sleep 0.01; done; printf "\033]7501;state=working\033\\"; while [ ! -f "{}" ]; do sleep 0.01; done; printf "\033]7501;state=idle\033\\"; sleep 100'"#,
            gate.display(),
            finish.display()
        );
        template(&s, "worker", &format!("---\ncommand: {command}\n---\n"));
        let parent = s.json(&["run", "--bg", "sh", "-c", "sleep 100"])["address"].as_str().unwrap().to_string();
        let r = s.json(&["run", "--bg", "--parent", &parent, "worker", "first task"]);
        assert_eq!(r["first_message"]["sent"], true, "{r}");
        assert_eq!(r["first_message"]["started"], false, "{r}");
        let child = r["address"].as_str().unwrap();
        let wait = |timeout: &str| wait_children(&s, &parent, mode, timeout);
        let r = wait("0.1");
        assert_eq!(r["reason"], "timeout", "{mode}: startup idle is not task completion: {r}");
        assert_eq!(r["children"][0]["ended"], false, "{r}");
        std::fs::write(gate, "").unwrap();
        wait_until("first task working", || s.json(&["get", child])["status"]["state"] == "working");
        assert_eq!(wait("0.1")["reason"], "timeout");
        std::fs::write(finish, "").unwrap();
        let r = wait("5");
        assert_eq!(r["reason"], "children", "{r}");
        assert_eq!(r["children"][0]["turn"], 1, "{r}");
        assert_eq!(r["children"][0]["status"], "idle", "{r}");
    }
}

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
