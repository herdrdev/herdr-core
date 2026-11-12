#![cfg(unix)]

mod common;
use common::*;

fn config(s: &Session, setting: &str) {
    let path = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, setting).unwrap();
}

fn state(s: &Session) -> String {
    s.json(&["server", "status"])["keep_awake"].as_str().unwrap().into()
}

fn assertion(pid: u64) -> Option<String> {
    let output = std::process::Command::new("/usr/bin/pmset").args(["-g", "assertions"]).output().unwrap();
    assert!(output.status.success());
    let owner = format!("pid {pid}(");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find(|line| line.contains(&owner) && line.contains("PreventUserIdleSystemSleep"))
        .map(String::from)
}

#[test]
fn only_local_work_holds_idle_sleep_and_the_last_exit_releases_it() {
    let s = Session::new();
    config(&s, "keep_awake = 'on'");
    let pid = s.json(&["server", "start"])["pid"].as_u64().unwrap();
    if !cfg!(target_os = "macos") {
        assert_eq!(state(&s), "unsupported");
        return;
    }
    assert_eq!(state(&s), "idle");
    let work = ["sh", "-c", r"printf '\033]7501;state=working\033\\'; sleep 100"];
    let a = run(&s, &work);
    let b = run(&s, &work);
    wait_until("both terminals working", || assertion(pid).is_some_and(|line| line.contains("2 terminals working")));
    s.json(&["kill", &a]);
    assert_eq!(state(&s), "active");
    assert!(assertion(pid).unwrap().contains("1 terminals working"));
    s.json(&["kill", &b]);
    wait_until("sleep assertion released", || assertion(pid).is_none());
    assert_eq!(state(&s), "idle");
}

#[test]
fn a_continuous_work_period_expires_without_more_output_and_rearms_after_idle() {
    let s = Session::new();
    config(&s, "keep_awake = 'on'\nkeep_awake_max = 1");
    let pid = s.json(&["server", "start"])["pid"].as_u64().unwrap();
    if !cfg!(target_os = "macos") {
        assert_eq!(state(&s), "unsupported");
        return;
    }
    let t = run(&s, &["sh", "-c", r#"while read state; do printf '\033]7501;state=%s\033\\' "$state"; done"#]);
    s.json(&["send", "--force", &t, "working"]);
    wait_until("sleep assertion", || state(&s) == "active");
    // No status request or output drives expiration.
    std::thread::sleep(std::time::Duration::from_millis(1300));
    assert!(assertion(pid).is_none(), "quiet work expired without a status request");
    assert_eq!(state(&s), "limited");
    s.json(&["send", "--force", &t, "working"]);
    assert_eq!(state(&s), "limited", "repeated working does not renew the limit");
    s.json(&["send", &t, "done"]);
    wait_until("idle", || state(&s) == "idle");
    s.json(&["send", "--force", &t, "working"]);
    wait_until("new work period", || state(&s) == "active");
    config(&s, "keep_awake = 'off'");
    wait_until("disabled", || state(&s) == "off");
}

#[test]
fn only_root_working_reports_count_and_all_other_states_release_sleep() {
    let s = Session::new();
    config(&s, "keep_awake = 'on'");
    let pid = s.json(&["server", "start"])["pid"].as_u64().unwrap();
    if !cfg!(target_os = "macos") {
        assert_eq!(state(&s), "unsupported");
        return;
    }
    let t = run(&s, &["sh", "-c", r#"while read state; do printf '\033]7501;state=%s\033\\' "$state"; done"#]);
    s.json(&["send", &t, "working:id=child"]);
    wait_until("child report", || s.json(&["get", &t])["records"]["child"]["state"] == "working");
    assert_eq!(state(&s), "idle");
    for next in ["done", "blocked", "error", "idle", "clear"] {
        s.json(&["send", "--force", &t, "working"]);
        wait_until("root working", || state(&s) == "active");
        s.json(&["send", &t, next]);
        wait_until("root stopped working", || state(&s) == "idle");
    }
    // Config edits release a quiet assertion without a status request.
    s.json(&["send", "--force", &t, "working"]);
    wait_until("root working", || state(&s) == "active");
    config(&s, "keep_awake = 'off'");
    wait_until("disabled without requests", || assertion(pid).is_none());
    config(&s, "keep_awake = 'on'");
    wait_until("enabled without requests", || assertion(pid).is_some());
    s.json(&["server", "stop"]);
    assert!(assertion(pid).is_none());
}
