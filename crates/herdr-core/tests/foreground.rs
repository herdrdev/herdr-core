#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::{Value, json};
use std::io::BufRead;

#[test]
fn what_runs_in_a_terminal_and_where_is_on_its_record_and_evented() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let sub = s.root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let t = run(&s, &["sh", "-c", &script(&s, &format!("w go; cd {}; exec sleep 100", sub.display()))]);
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap();
    // By its argv: the OS names macOS's /bin/sh `bash`, and that's the fact we pass on.
    wait_until("the shell in front", || get(&s, &t)["foreground"]["argv"][0] == "sh");
    let fg = get(&s, &t)["foreground"].clone();
    assert_eq!(canonical(fg["cwd"].as_str().unwrap()), canonical(get(&s, &t)["cwd"].as_str().unwrap()), "{fg}");
    assert!(fg["pid"].as_i64().unwrap() > 0 && !fg["name"].as_str().unwrap().is_empty(), "{fg}");

    let mut conn = s.connect();
    conn.call(1, "events.subscribe", json!({"terminal": t}));
    touch(&s, "go");
    let event = loop {
        let mut line = String::new();
        conn.reader.read_line(&mut line).expect("an event within 10 s");
        let msg: Value = serde_json::from_str(&line).unwrap();
        if msg["params"]["type"] == "foreground" && msg["params"]["data"]["name"] == "sleep" {
            break msg["params"]["data"].clone();
        }
    };
    assert_eq!(event["argv"], json!(["sleep", "100"]));
    assert_eq!(canonical(event["cwd"].as_str().unwrap()), canonical(sub.to_str().unwrap()));
    assert_eq!(get(&s, &t)["foreground"], event, "the record says the same");
}

#[cfg(target_os = "linux")]
#[test]
fn reported_cwd_fills_in_when_process_cwd_is_unreadable() {
    const FOLDERS: [(&str, &str); 2] = [("first folder", "first%20folder"), ("second folder", "second%20folder")];
    if let Some(root) = std::env::var_os("HC_TEST_CWD_REPORT_ROOT") {
        let root = std::path::PathBuf::from(root);
        let next = || assert!(std::io::stdin().read_line(&mut String::new()).unwrap() > 0);
        println!("\x1b]7;file://localhost{}/first%20folder\x07ready to change folder", root.display());
        next();
        std::env::set_current_dir(root.join("readable")).unwrap();
        for (dir, encoded) in FOLDERS {
            next();
            // Reproduce denied /proc/<pid>/cwd without requiring a second group or privileges.
            assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }, 0);
            std::env::set_current_dir(root.join(dir)).unwrap();
            println!("\x1b]7;file://localhost{}/{encoded}\x1b\\reported {encoded}", root.display());
        }
        next();
        return;
    }

    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let root = s.root.canonicalize().unwrap();
    let readable = root.join("readable");
    std::fs::create_dir(&readable).unwrap();
    for (dir, _) in FOLDERS {
        std::fs::create_dir(root.join(dir)).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let t = run(
        &s,
        &[
            "env",
            &format!("HC_TEST_CWD_REPORT_ROOT={}", root.display()),
            exe.to_str().unwrap(),
            "--nocapture",
            "--exact",
            "reported_cwd_fills_in_when_process_cwd_is_unreadable",
        ],
    );
    wait_until("initial cwd report", || text(&s, &t).contains("ready to change folder"));
    let pid = get(&s, &t)["pid"].as_u64().unwrap();
    s.json(&["send", &t, "change readable folder"]);
    wait_until("readable process cwd takes precedence over the report", || {
        get(&s, &t)["foreground"]["cwd"] == readable.to_str().unwrap()
    });

    let mut conn = s.connect();
    conn.call(1, "events.subscribe", json!({"terminal": t}));
    for (dir, encoded) in FOLDERS {
        s.json(&["send", &t, "change folder"]);
        wait_until("the process reported its new folder", || text(&s, &t).contains(&format!("reported {encoded}")));
        match std::fs::read_link(format!("/proc/{pid}/cwd")) {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied),
            Ok(_) => {
                eprintln!("skipping inaccessible cwd check: this test can inspect non-dumpable processes");
                return;
            }
        }
        let path = root.join(dir);
        let event = next_event(&mut conn, |m| {
            m["params"]["type"] == "foreground" && m["params"]["data"]["cwd"] == path.to_str().unwrap()
        });
        assert_eq!(event["params"]["data"]["pid"], pid);
        assert_eq!(get(&s, &t)["foreground"], event["params"]["data"]);
    }
}

#[test]
fn root_agent_keeps_status_when_a_foreground_tool_returns() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let tool = executable(&s, &format!("#!/bin/sh\n{}", script(&s, "w finish")));
    let agent = executable(
        &s,
        &format!(
            "#!/bin/bash\nset -m\n{}",
            script(
                &s,
                &format!("r state=working; w tool; {tool}; touch {}/returned; read -r action", s.root.display())
            )
        ),
    );
    let t = run(&s, &["bash", &agent]);
    wait_until("root status", || get(&s, &t)["status"]["state"] == "working");
    touch(&s, "tool");
    wait_until("tool foreground", || {
        get(&s, &t)["foreground"]["argv"].as_array().is_some_and(|argv| argv.contains(&json!(tool)))
    });
    assert_eq!(get(&s, &t)["status"]["state"], "working");
    touch(&s, "finish");
    wait_until("tool returned", || s.root.join("returned").exists());
    wait_until("root foreground", || get(&s, &t)["foreground"]["pid"] == get(&s, &t)["pid"]);
    assert_eq!(get(&s, &t)["status"]["state"], "working");
}

#[test]
fn a_shell_that_execs_an_agent_keeps_its_status() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let agent = executable(&s, &format!("#!/bin/sh\n{}", script(&s, "r state=working; read -r action")));
    let t = run(&s, &["sh", "-c", &format!("exec {agent}")]);
    // Its argv is in front from the exec on, before it reports: wait for both.
    wait_until("agent status", || get(&s, &t)["status"]["state"] == "working");
    wait_until("agent is the root program", || {
        get(&s, &t)["foreground"]["argv"].as_array().is_some_and(|argv| argv.contains(&json!(agent)))
    });
    still_working_after_samples(&s, &t);
}

#[test]
fn a_template_command_wrapper_keeps_live_status() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let templates = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(
        templates.join("agent.md"),
        "---\ncommand: printf '\\033]7501;state=idle\\033\\\\'; read -r action\n---\n",
    )
    .unwrap();
    let t = run(&s, &["agent"]);
    wait_until("wrapper is foreground", || get(&s, &t)["foreground"]["pid"] == get(&s, &t)["pid"]);
    assert_eq!(get(&s, &t)["status"]["state"], "idle");
}

#[test]
fn child_exit_before_a_foreground_sample_still_clears_status() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "1000".into()));
    let t = run(&s, &["sh", "-m"]);
    wait_until("root foreground", || !get(&s, &t)["foreground"].is_null());
    let path = s.root.join("quick.sh");
    std::fs::write(&path, script(&s, "r state=working; sleep 0.05")).unwrap();
    s.json(&["send", &t, &format!("sh {}", path.display())]);
    wait_until("reported", || get(&s, &t)["reported"] == true);
    wait_until("status cleared", || get(&s, &t)["status"].is_null());
}

#[test]
fn a_shell_script_root_keeps_its_agents_status() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let agent = executable(&s, &format!("#!/bin/sh\n{}", script(&s, "r state=working; read -r action")));
    let wrapper = executable(&s, &format!("#!/bin/bash\n{agent}\n"));
    let t = run(&s, &["bash", &wrapper]);
    wait_until("agent status", || get(&s, &t)["status"]["state"] == "working");
    wait_until("script root is foreground", || get(&s, &t)["foreground"]["pid"] == get(&s, &t)["pid"]);
    still_working_after_samples(&s, &t);
}

/// About four foreground samples (`HC_FOREGROUND_MS` 50) after the agent reported, its status stands.
fn still_working_after_samples(s: &Session, t: &str) {
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(get(s, t)["status"]["state"], "working");
}

#[test]
fn an_app_command_wrapper_clears_status_after_execing_its_interactive_shell() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let agent = executable(&s, &format!("#!/bin/sh\n{}", script(&s, "r state=working; w done")));
    let t = run(&s, &["bash", "-lic", "\"$@\"; exec \"$0\" -li", "bash", &agent]);
    wait_until("agent status", || get(&s, &t)["status"]["state"] == "working");
    touch(&s, "done");
    wait_until("interactive shell returned", || get(&s, &t)["foreground"]["argv"] == json!(["bash", "-li"]));
    wait_until("status cleared", || get(&s, &t)["status"].is_null());
}
