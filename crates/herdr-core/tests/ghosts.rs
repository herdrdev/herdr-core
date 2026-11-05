//! Normal terminal ends spare detached programs; losing the holder still
//! cleans up its abandoned programs, including those ignoring hangup.

#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use std::time::Duration;

/// A Python process that ignores hangup (and with `term`, terminate too),
/// writing its pid to `file`; with `own_session`, it leaves the terminal's
/// session first, like agents' tool shells. Python, not perl or sh: macOS
/// hides the environment of its own binaries, ours or not.
fn python(s: &Session, file: &str, own_session: bool, term: bool) -> String {
    let setsid = if own_session { "os.setsid(); " } else { "" };
    let ignore_term = if term { "signal.signal(signal.SIGTERM, signal.SIG_IGN); " } else { "" };
    format!(
        "python3 -c 'import os, signal, time; {setsid}signal.signal(signal.SIGHUP, signal.SIG_IGN); {ignore_term}open(\"{}\", \"w\").write(str(os.getpid())); time.sleep(300)'",
        s.root.join(file).display()
    )
}

fn stubborn(s: &Session, file: &str, own_session: bool) -> String {
    format!("{} &", python(s, file, own_session, true))
}

fn pid_in(s: &Session, file: &str) -> Option<i64> {
    std::fs::read_to_string(s.root.join(file)).ok()?.trim().parse().ok()
}

/// Alive and not a zombie.
fn running(pid: i64) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
        Err(_) => cfg!(target_os = "macos") && pid_alive(pid),
    }
}

fn started(s: &Session, files: &[&str]) -> Vec<i64> {
    wait_until("helpers started", || files.iter().all(|f| pid_in(s, f).is_some()));
    files.iter().map(|f| pid_in(s, f).unwrap()).collect()
}

fn gone(pids: &[i64]) {
    wait_until("every process it started ended", || !pids.iter().any(|p| running(*p)));
}

fn run(s: &Session, script: &str) -> String {
    s.json(&["run", "--bg", "sh", "-c", script])["address"].as_str().unwrap().to_string()
}

#[test]
fn kill_spares_programs_that_ignore_hangup() {
    let s = Session::new();
    let script = format!("{} {} sleep 300", stubborn(&s, "same", false), stubborn(&s, "detached", true));
    let t = run(&s, &script);
    let pids = started(&s, &["same", "detached"]);
    let survivors = Survivors(pids);
    s.json(&["kill", &t]);
    assert!(survivors.0.iter().all(|p| running(*p)), "a detached program was killed");
}

#[test]
fn detached_programs_survive_a_normal_exit() {
    let s = Session::new();
    let script = format!("{} {} sleep 0.3", stubborn(&s, "same", false), stubborn(&s, "detached", true));
    let t = run(&s, &script);
    let pids = started(&s, &["same", "detached"]);
    let survivors = Survivors(pids);
    wait_until("finished", || s.json(&["get", &t])["state"] == "finished");
    assert!(survivors.0.iter().all(|p| running(*p)), "a detached program was killed");
}

struct Survivors(Vec<i64>);

impl Drop for Survivors {
    fn drop(&mut self) {
        for &pid in &self.0 {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
        gone(&self.0);
    }
}

#[test]
fn kept_recovery_does_not_sweep_after_a_normal_stop() {
    let s = Session::new();
    let script = format!("{} sleep 300", stubborn(&s, "detached", true));
    s.json(&["run", "--bg", "--keep", "sh", "-c", &script]);
    let survivors = Survivors(started(&s, &["detached"]));
    s.json(&["server", "stop"]);
    assert!(survivors.0.iter().all(|p| running(*p)), "stop killed a detached program");
    s.json(&["server", "status"]);
    s.json(&["server", "restart"]);
    assert!(survivors.0.iter().all(|p| running(*p)), "recovery killed a detached program");
}

#[test]
#[cfg(target_os = "linux")]
fn closing_detached_terminals_releases_the_holders_pty_masters() {
    let s = Session::new();
    let warm = run(&s, "true");
    wait_until("warm terminal ended", || s.json(&["get", &warm])["state"] == "finished");
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_u64().unwrap();
    let masters = || {
        std::fs::read_dir(format!("/proc/{holder}/fd"))
            .unwrap()
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .filter(|path| path.ends_with("ptmx"))
            .count()
    };
    wait_until("warm PTY released", || masters() == 0);
    eprintln!("holder {holder}: before closing detached terminals, {} PTY masters", masters());
    let mut survivors = Survivors(Vec::new());
    for i in 0..3 {
        let file = format!("detached-{i}");
        let t = run(&s, &format!("{} exec sleep 300", stubborn(&s, &file, true)));
        survivors.0.extend(started(&s, &[&file]));
        s.json(&["kill", &t]);
    }
    assert!(survivors.0.iter().all(|p| running(*p)), "a detached program was killed");
    wait_until("closed terminals released every PTY master", || masters() == 0);
    eprintln!("holder {holder}: after closing three detached terminals, {} PTY masters", masters());
}

#[test]
fn a_replacement_whose_holder_was_lost_is_recovered_and_cleaned_up() {
    let s = Session::new();
    let t = s.json(&["run", "--bg", "--keep", "sleep", "300"])["address"].as_str().unwrap().to_string();
    s.json(&["server", "stop"]);
    // What a crash leaves: a holder had claimed the replacement program but
    // never reported it started, and something it launched is still running.
    let path = s.root.join("state/herdr-core/test/terminals.json");
    let mut reg: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let token = format!("0-replacement-{}", std::process::id());
    let rec = reg["terminals"].as_object_mut().unwrap().values_mut().next().unwrap();
    rec["pid"] = serde_json::Value::Null;
    rec["reason"] = "server_stop".into();
    rec["holder"] = "a-holder-that-is-gone".into();
    rec["scope"] = token.clone().into();
    std::fs::write(&path, serde_json::to_vec(&reg).unwrap()).unwrap();
    let launched = std::process::Command::new("sh")
        .args(["-c", &format!("{} &", python(&s, "left", false, false))])
        .env("HC_SCOPE", &token)
        .status()
        .unwrap();
    assert!(launched.success());
    let left = started(&s, &["left"]);
    s.json(&["server", "status"]);
    gone(&left);
    wait_until("the kept terminal got a new shell", || get(&s, &t)["pid"].is_number());
}

#[test]
fn a_lost_holder_takes_its_programs_with_it() {
    let s = Session::new();
    let t = run(&s, &format!("{} exec {}", stubborn(&s, "detached", true), python(&s, "leader", false, false)));
    let pids = started(&s, &["leader", "detached"]);
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
    unsafe { libc::kill(holder as i32, libc::SIGKILL) };
    wait_until("holder lost", || s.json(&["get", &t])["reason"] == "holder_lost");
    gone(&pids);
}

#[test]
fn a_stop_right_after_a_lost_holder_still_ends_its_programs() {
    let s = Session::new();
    run(&s, &format!("{} exec {}", stubborn(&s, "detached", true), python(&s, "leader", false, true)));
    let pids = started(&s, &["leader", "detached"]);
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
    unsafe { libc::kill(holder as i32, libc::SIGKILL) };
    wait_until("holder lost", || s.json(&["server", "status"])["holders"].as_array().unwrap().is_empty());
    s.json(&["server", "stop"]);
    assert!(!pids.iter().any(|p| running(*p)), "stop returned with the lost holder's programs alive");
}

#[test]
fn a_holder_dying_during_stop_still_has_its_programs_ended_before_stop_returns() {
    let s = Session::new();
    run(&s, &format!("{} sleep 300", stubborn(&s, "detached", true)));
    let pids = started(&s, &["detached"]);
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap() as i32;
    unsafe { libc::kill(holder, libc::SIGSTOP) }; // frozen: stop gets past killing into its goodbye
    let stop = s.cmd().args(["server", "stop"]).spawn().unwrap();
    std::thread::sleep(Duration::from_secs(4));
    unsafe { libc::kill(holder, libc::SIGKILL) };
    let out = stop.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(!pids.iter().any(|p| running(*p)), "stop returned with the dead holder's programs alive");
}

#[test]
fn a_terminal_that_started_another_hc_session_does_not_take_it_down() {
    let s = Session::new();
    // Ending a terminal's own job must not take down the core that job started.
    let inner = format!("HC_SESSION=inner {} events", s.bin.display());
    let t = run(&s, &format!("({inner}) & exec sleep 300"));
    let mut status = s.cmd();
    status.env("HC_SESSION", "inner").args(["server", "status"]);
    wait_until("inner core", || status.output().unwrap().status.success());
    let inner_core =
        serde_json::from_slice::<serde_json::Value>(&status.output().unwrap().stdout).unwrap()["pid"].as_i64().unwrap();
    s.json(&["kill", &t]);
    std::thread::sleep(Duration::from_millis(1500));
    let alive = running(inner_core);
    let mut stop = s.cmd();
    stop.env("HC_SESSION", "inner").args(["server", "stop"]);
    stop.output().unwrap();
    assert!(alive, "ending the terminal killed the other session's core");
}

#[test]
fn a_run_while_stopping_is_refused() {
    let s = Session::new();
    // Ends only on KILL, so the stop takes a moment.
    let t = run(&s, "trap '' HUP TERM; while :; do sleep 1; done");
    wait_until("running", || s.json(&["get", &t])["pid"].is_number());
    let stop = {
        let mut cmd = s.cmd();
        cmd.args(["server", "stop"]);
        std::thread::spawn(move || cmd.output().unwrap())
    };
    std::thread::sleep(Duration::from_millis(300));
    let (_, err) = s.err(&["run", "--bg", "sleep", "300"]);
    assert_eq!(err["error"]["kind"], "unavailable", "{err}");
    assert!(stop.join().unwrap().status.success());
}

#[test]
fn a_program_that_starts_after_its_run_gave_up_is_ended() {
    let mut s = Session::new();
    s.env.push(("HC_SPAWN_MS".into(), "500".into()));
    s.json(&["run", "--bg", "true"]); // a holder now runs
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
    unsafe { libc::kill(holder as i32, libc::SIGSTOP) };
    let (_, err) =
        s.err(&["run", "--bg", "sh", "-c", &format!("echo $$ > {}; sleep 300", s.root.join("late").display())]);
    unsafe { libc::kill(holder as i32, libc::SIGCONT) };
    assert_eq!(err["error"]["kind"], "spawn_failed", "{err}");
    // It may die before it even writes its pid; either way it must not live on.
    std::thread::sleep(Duration::from_millis(1000));
    if let Some(pid) = pid_in(&s, "late") {
        gone(&[pid]);
    }
}
