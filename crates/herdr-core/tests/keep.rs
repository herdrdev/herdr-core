#![cfg(unix)]
mod common;
use common::*;

#[test]
fn kept_terminals_close_only_on_exit_or_kill() {
    let s = Session::new();
    let t = run(&s, &["--keep", "sh", "-c", "exit 3"]);
    wait_until("exit", || get(&s, &t)["state"] == "finished");
    assert_eq!(get(&s, &t)["reason"], "exited");
    let t = run(&s, &["sleep", "100"]);
    assert_eq!(s.json(&["keep", &t])["keep"], true);
    assert_eq!(s.json(&["keep", &t, "--off"])["keep"], false);
    s.json(&["keep", &t]);
    assert_eq!(s.json(&["kill", &t])["reason"], "killed");
    let shell = run(&s, &["--keep"]);
    assert_eq!(get(&s, &shell)["keep"], true);
    assert!(get(&s, &shell)["pid"].is_number());
    s.json(&["kill", &shell]);
}

#[test]
fn lost_kept_terminal_restarts_in_place_in_its_last_folder() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "30".into()));
    s.env.push(("TERM".into(), "vt100".into()));
    s.env.push(("TERM_PROGRAM".into(), "keep-test".into()));
    s.env.push(("HC_KEEP_TEST".into(), "preserved".into()));
    let dir = s.root.join("last");
    std::fs::create_dir(&dir).unwrap();
    let canonical_dir = std::fs::canonicalize(&dir).unwrap();
    let t = run(&s, &["--keep", "--name", "kept", "sh", "-c", &format!("cd {}; exec sleep 100", dir.display())]);
    wait_until("folder observed", || get(&s, &t)["foreground"]["cwd"] == canonical_dir.display().to_string());
    let mut events = s.connect();
    assert!(events.call(1, "events.subscribe", serde_json::json!({"terminal": t}))["result"].is_object());
    let mut viewer = framed::Viewer::attach(&s, &t, 91, 29, serde_json::json!({}));
    let old = get(&s, &t)["pid"].as_i64().unwrap();
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
    unsafe {
        libc::kill(holder as i32, libc::SIGKILL);
    }
    wait_until("replacement shell", || {
        let r = get(&s, &t);
        r["state"] == "live" && r["pid"].as_i64().is_some_and(|p| p != old)
    });
    viewer.until("loss closes the stream", |v| v.closed.is_some());
    assert_eq!(viewer.closed.as_ref().unwrap()["reason"], "holder_lost");
    let event = next_event(&mut events, |e| e["params"]["type"] == "restarted");
    assert_eq!(event["params"]["data"]["address"], t);
    let r = get(&s, &t);
    assert_eq!(r["cwd"], canonical_dir.display().to_string());
    assert_eq!(r["name"], "kept");
    assert_eq!(r["identity"], "keep-test");
    assert_eq!((r["cols"].as_u64(), r["rows"].as_u64()), (Some(91), Some(29)));
    let mut viewer = framed::Viewer::attach(&s, &t, 91, 29, serde_json::json!({}));
    s.json(&["send", &t, "printf 'kept-env:%s:%s\\n' \"$TERM\" \"$HC_KEEP_TEST\""]);
    viewer.shows("kept-env:vt100:preserved");
    assert_eq!(s.json(&["ls"])["terminals"].as_array().unwrap().len(), 1);
    assert!(!pid_alive(old));
}

#[test]
fn kept_terminal_survives_core_crash_and_recovers_if_its_holder_is_also_lost() {
    let s = Session::new();
    let t = run(&s, &["--keep", "sleep", "100"]);
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, r#"shell = ["/bin/sh", "-c", "echo configured-restore; exec /bin/sh"]"#).unwrap();
    let old = get(&s, &t)["pid"].as_i64().unwrap();
    let status = s.json(&["server", "status"]);
    unsafe {
        libc::kill(status["pid"].as_i64().unwrap() as i32, libc::SIGKILL);
    }
    wait_until("core gone", || !s.connect_ok());
    assert_eq!(get(&s, &t)["pid"], old, "surviving holder is adopted, not replaced");
    let status = s.json(&["server", "status"]);
    unsafe {
        libc::kill(status["pid"].as_i64().unwrap() as i32, libc::SIGKILL);
        libc::kill(status["holder"]["pid"].as_i64().unwrap() as i32, libc::SIGKILL);
    }
    wait_until("core gone again", || !s.connect_ok());
    wait_until("replacement on start", || get(&s, &t)["pid"].as_i64().is_some_and(|p| p != old));
    assert_eq!(get(&s, &t)["state"], "live");
    wait_until("configured restored shell", || text(&s, &t).contains("configured-restore"));
    assert!(!pid_alive(old));
}

#[test]
fn failed_shell_restart_stays_live_and_retries() {
    use std::os::unix::fs::PermissionsExt;
    let mut s = Session::new();
    let shell = s.root.join("shell");
    s.env.push(("SHELL".into(), shell.display().to_string()));
    let t = run(&s, &["--keep", "--rm", "sleep", "100"]);
    s.json(&["server", "stop"]);
    let r = get(&s, &t);
    assert_eq!(r["state"], "live");
    assert!(r["finished_ms"].is_null());
    std::fs::write(&shell, "#!/bin/sh\nexec /bin/sh\n").unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    wait_until("retry succeeds", || get(&s, &t)["pid"].is_number());
    assert_eq!(s.json(&["ls"])["terminals"].as_array().unwrap().len(), 1);
    let r = get(&s, &t);
    assert!(r["reason"].is_null());
    s.err(&["kill", &t]); // --rm removes it before kill can return its record.
    assert_eq!(s.err(&["get", &t]).1["error"]["kind"], "not_found");
}

#[test]
fn recovery_saves_only_client_overrides_and_refreshes_the_core_environment() {
    let mut s = Session::new();
    s.env.push(("HC_KEEP_BASE".into(), "old-secret".into()));
    s.json(&["server", "start"]);
    let mut conn = s.connect();
    let started = conn.call(
        1,
        "terminal.run",
        serde_json::json!({
            "argv": ["sleep", "100"], "keep": true,
            "env": {"HC_KEEP_OVERRIDE": "saved", "TERM": "vt100"}
        }),
    );
    let t = started["result"]["address"].as_str().unwrap();
    let state = std::fs::read_to_string(s.root.join("state/herdr-core/test/terminals.json")).unwrap();
    assert!(!state.contains("HC_KEEP_BASE") && !state.contains("old-secret"), "{state}");
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(
        state["terminals"]["1"]["restart_env"],
        serde_json::json!({"HC_KEEP_OVERRIDE": "saved", "TERM": "vt100"})
    );
    s.json(&["server", "stop"]);
    s.env.retain(|(k, _)| k != "HC_KEEP_BASE");
    s.env.push(("HC_KEEP_BASE".into(), "fresh".into()));
    wait_until("recovery with fresh environment", || get(&s, t)["pid"].is_number());
    s.json(&["send", t, "printf 'recovery:%s:%s:%s\\n' \"$HC_KEEP_BASE\" \"$HC_KEEP_OVERRIDE\" \"$TERM\""]);
    wait_until("fresh base and saved overrides", || text(&s, t).contains("recovery:fresh:saved:vt100"));
}

#[test]
fn signal_death_is_lost_only_when_kept() {
    let s = Session::new();
    let t = run(&s, &["--keep", "sleep", "100"]);
    let old = get(&s, &t)["pid"].as_i64().unwrap();
    let mut events = s.connect();
    events.call(1, "events.subscribe", serde_json::json!({"terminal": t}));
    unsafe {
        libc::kill(old as i32, libc::SIGTERM);
    }
    let event = next_event(&mut events, |e| e["params"]["type"] == "lost");
    assert_eq!(event["params"]["data"]["reason"], "signaled");
    assert_pending_input(&s, &t);
    wait_until("fresh shell after signal", || get(&s, &t)["pid"].as_i64().is_some_and(|p| p != old));
    let t = run(&s, &["sleep", "100"]);
    unsafe {
        libc::kill(get(&s, &t)["pid"].as_i64().unwrap() as i32, libc::SIGTERM);
    }
    wait_until("non-kept signal stays final", || get(&s, &t)["state"] == "finished");
    assert_eq!(get(&s, &t)["reason"], "exited");
}

struct PausedHolder(i32);
impl Drop for PausedHolder {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0, libc::SIGCONT);
        }
    }
}

fn paused_replacement() -> (Session, String, PausedHolder) {
    let mut s = Session::new();
    s.env.push(("SHELL".into(), s.root.join("missing-shell").display().to_string()));
    let t = run(&s, &["--keep", "sleep", "100"]);
    s.json(&["server", "stop"]);
    get(&s, &t); // Startup's first replacement fails; pause its holder before the retry.
    let mut holder = 0;
    wait_until("replacement holder", || {
        holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap_or(0) as i32;
        holder != 0
    });
    unsafe {
        libc::kill(holder, libc::SIGSTOP);
    }
    let holder = PausedHolder(holder);
    // The claim comes after two failed tries' backoff (1.5 s + 3 s) and a 1 s tick:
    // about 5 s here, past wait_until's 10 s on a slow runner.
    let claimed = || {
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(s.root.join("state/herdr-core/test/terminals.json")).unwrap())
                .unwrap();
        state["terminals"]["1"]["holder"].is_string()
    };
    let start = std::time::Instant::now();
    while !claimed() {
        assert!(start.elapsed() < std::time::Duration::from_secs(30), "timed out waiting for replacement claimed");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    (s, t, holder)
}

fn assert_pending_input(s: &Session, t: &str) {
    let mut conn = framed::Framed::open(s);
    for (method, extra) in [
        ("terminal.send", serde_json::json!({"text": "hello"})),
        ("terminal.keys", serde_json::json!({"keys": ["enter"]})),
        ("terminal.interrupt", serde_json::json!({})),
        ("terminal.attach", serde_json::json!({"cols": 80, "rows": 24})),
    ] {
        let mut p = extra;
        p["terminal"] = t.into();
        let reply = conn.call(method, p);
        assert_eq!(reply["error"]["data"]["kind"], "unavailable", "{method}: {reply}");
        assert!(reply["error"]["message"].as_str().unwrap().contains("lost, waiting for its new shell"));
    }
}

#[test]
fn pending_programs_refuse_input_and_attach_and_can_be_killed() {
    let (s, t, holder) = paused_replacement();
    assert_pending_input(&s, &t);
    // The queued replacement can actually spawn after cancellation; it must not revive the record.
    use std::os::unix::fs::PermissionsExt;
    let shell = s.root.join("missing-shell");
    std::fs::write(&shell, "#!/bin/sh\nexec /bin/sh\n").unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    let r = s.json(&["kill", &t]);
    unsafe {
        libc::kill(holder.0, libc::SIGCONT);
    }
    assert_eq!(r["state"], "finished");
    assert_eq!(r["reason"], "killed");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(get(&s, &t)["reason"], "killed");
}

#[test]
fn disabling_keep_during_a_failed_spawn_finishes_the_record() {
    let (s, t, holder) = paused_replacement();
    s.json(&["keep", &t, "--off"]);
    unsafe {
        libc::kill(holder.0, libc::SIGCONT);
    }
    wait_until("failed spawn closes after keep off", || get(&s, &t)["state"] == "finished");
}

#[test]
fn kill_intent_wins_over_holder_loss() {
    use std::process::Stdio;
    let s = Session::new();
    let t = run(&s, &["--keep", "sleep", "100"]);
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap() as i32;
    unsafe {
        libc::kill(holder, libc::SIGSTOP);
    }
    let kill = s.cmd().args(["--json", "kill", &t]).stdout(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    unsafe {
        libc::kill(holder, libc::SIGKILL);
    }
    let out = kill.wait_with_output().unwrap();
    assert!(out.status.success());
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["state"], "finished");
    assert_eq!(r["reason"], "killed");
}

#[test]
fn waits_end_when_a_kept_program_is_lost() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = s.connect();
    let reply = conn.call(
        1,
        "terminal.run",
        serde_json::json!({
            "argv": ["sh", "-c", "sleep 0.1; kill -9 $$"], "keep": true, "wait": true, "timeout": 1.0
        }),
    );
    assert_eq!(reply["result"]["wait"]["ended"], true, "{reply}");
    assert_eq!(reply["result"]["wait"]["reason"], "exit");
    assert_eq!(reply["result"]["wait"]["signal"], 9);
    let t = reply["result"]["address"].as_str().unwrap();
    let waited = s.json(&["wait", t, "--timeout", "1"]);
    assert_eq!(waited["ended"], true, "{waited}");
    assert_eq!(waited["reason"], "exit");
}

#[test]
fn a_kept_run_timeout_retains_the_claim_until_the_holder_answers() {
    let mut s = Session::new();
    s.env.push(("HC_SPAWN_MS".into(), "50".into()));
    run(&s, &["sleep", "100"]);
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap() as i32;
    unsafe {
        libc::kill(holder, libc::SIGSTOP);
    }
    let paused = PausedHolder(holder);
    s.err(&["run", "--bg", "--keep", "sleep", "100"]);
    assert_eq!(get(&s, "t2")["state"], "live");
    assert!(get(&s, "t2")["pid"].is_null());
    assert_eq!(get(&s, "t2")["reason"], "spawn_pending");
    assert_pending_input(&s, "t2");
    drop(paused);
    wait_until("late spawn is still addressable", || get(&s, "t2")["pid"].is_number());
    s.json(&["kill", "t2"]);
}

#[test]
fn children_waits_also_end_on_program_loss() {
    let s = Session::new();
    let parent = run(&s, &["sleep", "100"]);
    let child = run(&s, &["--keep", "--parent", &parent, "sleep", "100"]);
    let mut events = s.connect();
    events.call(1, "events.subscribe", serde_json::json!({"terminal": child}));
    unsafe {
        libc::kill(get(&s, &child)["pid"].as_i64().unwrap() as i32, libc::SIGTERM);
    }
    next_event(&mut events, |e| e["params"]["type"] == "lost");
    let mut conn = s.connect();
    for (id, mode) in [(1, "any"), (2, "all")] {
        let reply = conn.call(
            id,
            "terminal.wait",
            serde_json::json!({
                "terminal": parent, "caller": parent, "children": mode, "timeout": 0.1
            }),
        );
        let r = &reply["result"];
        assert_eq!(r["ended"], true, "{reply}");
        assert_eq!(r["children"][0]["reason"], "exit", "{reply}");
    }
}

#[test]
fn late_initial_spawn_failure_is_handled_after_timeout_even_after_keep_off() {
    for keep in [false, true] {
        let mut s = Session::new();
        s.env.push(("HC_SPAWN_MS".into(), "50".into()));
        run(&s, &["sleep", "100"]);
        let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap() as i32;
        unsafe {
            libc::kill(holder, libc::SIGSTOP);
        }
        let paused = PausedHolder(holder);
        s.err(&["run", "--bg", "--keep", "nonexistent-keep-program"]);
        if !keep {
            s.json(&["keep", "t2", "--off"]);
        }
        drop(paused);
        wait_until("late failed spawn reconciled", || {
            let r = get(&s, "t2");
            if keep { r["pid"].is_number() } else { r["state"] == "finished" }
        });
        assert_eq!(get(&s, "t2")["keep"], keep);
    }
}

#[test]
fn ordinary_exit_stays_finished_across_a_core_restart_in_the_same_boot() {
    let s = Session::new();
    let t = run(&s, &["--keep", "sh", "-c", "exit 1"]);
    wait_until("ordinary exit", || get(&s, &t)["state"] == "finished");
    s.json(&["server", "stop"]);
    assert_eq!(get(&s, &t)["state"], "finished");
    assert_eq!(get(&s, &t)["exit_code"], 1);
}

#[test]
fn stopped_kept_terminal_returns_at_next_start() {
    let s = Session::new();
    let t = run(&s, &["--keep", "sleep", "100"]);
    s.json(&["server", "stop"]);
    let r = get(&s, &t);
    assert_eq!(r["state"], "live");
    wait_until("shell after start", || get(&s, &t)["pid"].is_number());
}
