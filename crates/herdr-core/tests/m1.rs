#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::Value;
use std::time::Duration;

fn wait_finished(s: &Session, t: &str) -> Value {
    let mut last = Value::Null;
    wait_until(&format!("{t} finished"), || {
        last = get(s, t);
        last["state"] == "finished"
    });
    last
}

#[cfg(target_os = "macos")]
#[test]
fn default_shells_load_the_login_environment_but_explicit_programs_do_not() {
    let mut s = Session::new();
    s.env.push(("SHELL".into(), "/bin/zsh".into()));
    std::fs::write(s.root.join(".zprofile"), "export HC_LOGIN_TEST=loaded\n").unwrap();
    let t = run(&s, &[]);
    s.json(&["send", &t, "print -r -- login=$options[login] profile=$HC_LOGIN_TEST"]);
    wait_text(&s, &t, "login=on profile=loaded");

    let t = run(&s, &["/bin/zsh", "-c", "print -r -- login=$options[login] profile=$HC_LOGIN_TEST"]);
    wait_finished(&s, &t);
    assert!(text(&s, &t).contains("login=off profile="), "explicit argv stays unchanged");
}

#[cfg(target_os = "macos")]
#[test]
fn programs_get_a_utf8_language_when_the_core_has_none() {
    // A core the app starts from Finder has no LANG: shells would run in "C".
    let lang = |s: &Session| {
        let t = run(s, &["sh", "-c", "echo lang=[$LANG]"]);
        wait_finished(s, &t);
        s.json(&["read", &t])["text"].as_str().unwrap().to_string()
    };
    let s = Session::new();
    assert!(lang(&s).contains(".UTF-8]"), "{}", lang(&s));
    let mut s = Session::new();
    s.env.push(("LANG".into(), "C".into()));
    assert!(lang(&s).contains("lang=[C]"), "the user's choice stays");
}

#[test]
fn programs_never_get_the_ssh_login_that_started_the_core() {
    let mut s = Session::new();
    s.env.push(("SSH_CONNECTION".into(), "100.1.1.1 1 100.2.2.2 22".into()));
    let t = run(&s, &["sh", "-c", "echo ssh=[$SSH_CONNECTION]"]);
    wait_finished(&s, &t);
    let text = s.json(&["read", &t])["text"].as_str().unwrap().to_string();
    assert!(text.contains("ssh=[]"), "{text}");
}

#[test]
fn piped_ls_says_what_runs_where_so_agents_find_each_other() {
    let s = Session::new();
    run(&s, &["--label", "where=durable / main", "--cwd", "/", "sleep", "100"]);
    run(&s, &["--cwd", "/", "sh", "-c", "sleep 100"]);
    let ls = s.json(&["ls"]);
    let (a, b) = (&ls["terminals"][0], &ls["terminals"][1]);
    assert_eq!(
        (a["agent"].as_str(), a["where"].as_str(), a["cwd"].as_str()),
        (Some("sleep"), Some("durable / main"), Some("/")),
        "{a}"
    );
    assert_eq!((b["agent"].as_str(), b.get("where")), (Some("sh"), None), "{b}");
}

#[test]
fn finished_terminals_expire_even_when_nothing_else_happens() {
    let mut s = Session::new();
    s.env.push(("HC_RETAIN_MS".into(), "300".into()));
    let t = run(&s, &["true"]);
    wait_finished(&s, &t);
    std::thread::sleep(Duration::from_millis(600));
    let all = s.json(&["ls", "--all"]);
    assert!(!all.to_string().contains(&format!("\"{t}\"")), "kept past its time: {all}");
}

#[test]
fn the_public_record_shows_facts_not_bookkeeping() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "echo hi; sleep 100"]);
    wait_text(&s, &t, "hi");
    for record in [get(&s, &t), s.json(&["label", &t, "k=v"])] {
        let text = record.to_string();
        for private in ["\"news\"", "\"seen\"", "\"scope\"", "\"holder\"", "\"printed\""] {
            assert!(!text.contains(private), "{private} in {text}");
        }
    }
    assert_eq!(get(&s, &t)["address"], t.as_str());
}

#[test]
fn a_read_missing_history_says_so() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "seq 1 20000; sleep 100"]);
    wait_text(&s, &t, "20000");
    let full = |lines: &str| s.json(&["read", &t, "--lines", lines]);
    assert_eq!(full("10")["complete"], true, "the last lines are all there");
    let r = full("30000");
    assert!(!r["text"].as_str().unwrap().starts_with("1\n"), "older lines were dropped");
    assert_eq!(r["complete"], false, "and the read says so");
    s.json(&["kill", &t]);
    wait_finished(&s, &t);
    assert_eq!(full("30000")["complete"], false, "finished too");
}

#[test]
fn read_prints_plain_text_even_when_piped() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "echo 'say \"hi\"'; echo second; sleep 100"]);
    wait_text(&s, &t, "second");
    let out = String::from_utf8(s.hc(&["read", &t]).stdout).unwrap();
    assert!(out.starts_with("say \"hi\"\nsecond\n"), "{out:?}");
}

fn wait_text(s: &Session, t: &str, needle: &str) {
    wait_until(&format!("{needle:?} in {t}"), || text(s, t).contains(needle));
}

fn status(s: &Session) -> Value {
    s.json(&["server", "status"])
}

fn kill9(pid: i64) {
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    wait_until("process gone", || !pid_alive(pid));
}

#[test]
fn runs_a_program_and_keeps_its_result_after_exit() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "echo hello; exit 3"]);
    assert_eq!(t, "t1");
    let rec = wait_finished(&s, &t);
    assert_eq!(rec["exit_code"], 3);
    assert_eq!(rec["reason"], "exited");
    assert!(text(&s, &t).contains("hello"));

    let ls = s.json(&["ls"]);
    assert_eq!(ls["terminals"].as_array().unwrap().len(), 0);
    assert_eq!(ls["hidden_finished"], 1);
    let all = s.json(&["ls", "--all"]);
    assert_eq!(all["terminals"][0]["address"], "t1");
    // Piped run and ls keep it short for agents; get has the full record.
    let row = all["terminals"][0].as_object().unwrap();
    assert!(row.contains_key("exit_code") && !row.contains_key("seen_ms") && !row.contains_key("argv"));
    assert!(s.json(&["get", "t1"]).get("argv").is_some());
    let started = s.json(&["run", "--bg", "true"]);
    assert!(started.get("address").is_some() && started.get("created_ms").is_none());
}

#[test]
fn child_gets_core_env_caller_identity_cwd_and_its_own_address() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let out = s
        .cmd()
        .args(["run", "--bg", "sh", "-c", "echo id=$HC_TERMINAL_ID foo=$FOO tp=$TERM_PROGRAM pwd=$PWD; sleep 100"])
        .env("FOO", "bar")
        .env("TERM_PROGRAM", "WezTerm")
        .current_dir("/var/tmp")
        .env("PWD", "/var/tmp") // as a shell sets it (on macOS the real path is /private/var/tmp)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_text(&s, "t1", "id=t1 foo= tp=WezTerm pwd=/var/tmp");

    // Never herdr: a program started from inside Herdr sees plain ghostty.
    let out = s
        .cmd()
        .args(["run", "--bg", "sh", "-c", "echo tp=$TERM_PROGRAM v=$TERM_PROGRAM_VERSION.; sleep 100"])
        .env("TERM_PROGRAM", "herdr")
        .env("TERM_PROGRAM_VERSION", "0.9.3")
        .output()
        .unwrap();
    assert!(out.status.success());
    wait_text(&s, "t2", "tp=ghostty v=.");
    assert_eq!(get(&s, "t2")["identity"], "ghostty");
}

#[test]
fn spawn_failure_is_reported_and_leaves_nothing_running() {
    let s = Session::new();
    let (code, err) = s.err(&["run", "--bg", "/no/such/program"]);
    assert_eq!(err["error"]["kind"], "spawn_failed");
    assert_eq!(code, 1);
    assert_eq!(s.json(&["ls", "--all"])["terminals"].as_array().unwrap().len(), 0);
}

#[test]
fn names_are_unique_among_live_terminals() {
    let s = Session::new();
    let a = run(&s, &["--name", "worker", "sleep", "100"]);
    let (_, err) = s.err(&["run", "--bg", "--name", "worker", "sleep", "100"]);
    assert_eq!(err["error"]["kind"], "name_taken");
    for bad in ["a/b", "t9", ""] {
        let (code, err) = s.err(&["run", "--bg", "--name", bad, "true"]);
        assert_eq!((code, err["error"]["kind"].as_str()), (6, Some("invalid_params")), "{bad}");
    }
    assert_eq!(get(&s, "worker")["address"], a.as_str());

    s.json(&["kill", "worker"]);
    let b = run(&s, &["--name", "worker", "sleep", "100"]);
    assert_ne!(a, b);
    assert_eq!(get(&s, "worker")["address"], b.as_str());
    assert_eq!(get(&s, &a)["name"], "worker");
}

#[test]
fn labels_are_stored_filtered_and_removed() {
    let s = Session::new();
    let a = run(&s, &["--label", "rex.role=reviewer", "--label", "empty=", "sleep", "100"]);
    assert!(get(&s, &a)["labels"].get("empty").is_none(), "key= means no label, at run too");
    let _b = run(&s, &["sleep", "100"]);
    s.json(&["label", &a, "herdr.pane=3"]);
    let only = s.json(&["ls", "--label", "herdr.pane=3", "--label", "rex.role=reviewer"]);
    let only = only["terminals"].as_array().unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0]["address"], a.as_str());
    s.json(&["label", &a, "rex.role="]);
    assert!(get(&s, &a)["labels"].get("rex.role").is_none());
}

#[test]
fn kill_hangs_up_then_escalates() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let rec = s.json(&["kill", &t]);
    assert_eq!(rec["state"], "finished");
    assert_eq!(rec["reason"], "killed");
    assert_eq!(rec["signal"], libc::SIGHUP);

    let stubborn = run(&s, &["sh", "-c", "trap '' HUP TERM; echo ready; while :; do sleep 1; done"]);
    wait_text(&s, &stubborn, "ready");
    let rec = s.json(&["kill", &stubborn]);
    assert_eq!(rec["state"], "finished");
    assert_eq!(rec["signal"], libc::SIGKILL);
}

#[test]
fn rm_removes_only_finished_terminals() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let (_, err) = s.err(&["rm", &t]);
    assert_eq!(err["error"]["kind"], "live");
    s.json(&["kill", &t]);
    s.json(&["rm", &t]);
    let (code, err) = s.err(&["get", &t]);
    assert_eq!((code, err["error"]["kind"].as_str()), (3, Some("not_found")));
}

#[test]
fn remote_addresses_are_rejected_for_now() {
    let s = Session::new();
    let (code, err) = s.err(&["get", "otherbox/t1"]);
    assert_eq!((code, err["error"]["kind"].as_str()), (4, Some("machine_unknown")));
    let name = status(&s)["core"]["name"].as_str().unwrap().to_string();
    let (_, err) = s.err(&["get", &format!("{name}/t1")]);
    assert_eq!(err["error"]["kind"], "not_found");
}

#[test]
fn core_crash_adopts_terminals_and_recovers_the_screen() {
    let s = Session::new();
    let go = s.root.join("go");
    let script = format!("echo before; while [ ! -e {} ]; do sleep 0.02; done; echo during; sleep 100", go.display());
    let t = run(&s, &["sh", "-c", &script]);
    wait_text(&s, &t, "before");
    let pid = get(&s, &t)["pid"].as_i64().unwrap();
    let core = status(&s)["pid"].as_i64().unwrap();

    kill9(core);
    // Output produced while no core is running must not be lost.
    std::fs::write(&go, "").unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let rec = get(&s, &t);
    assert_eq!(rec["address"], t.as_str());
    assert_eq!(rec["state"], "live");
    assert_eq!(rec["pid"].as_i64().unwrap(), pid);
    assert_eq!(rec["screen"], "inexact");
    let text = text(&s, &t);
    assert!(text.contains("before") && text.contains("during"), "{text:?}");
    assert!(pid_alive(pid));
}

#[test]
fn planned_restart_restores_the_exact_screen() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "printf 'one\\ntwo\\n'; sleep 0.5; echo three; sleep 100"]);
    wait_text(&s, &t, "two");
    let pid = get(&s, &t)["pid"].clone();
    let old = status(&s)["pid"].as_i64().unwrap();
    let new = s.json(&["server", "restart"])["pid"].as_i64().unwrap();
    assert!(!pid_alive(old) && new != old);
    std::thread::sleep(Duration::from_millis(700));

    let rec = get(&s, &t);
    assert_eq!((rec["state"].as_str(), rec["screen"].as_str()), (Some("live"), Some("exact")));
    assert_eq!(rec["pid"], pid);
    let text = text(&s, &t);
    assert!(text.contains("one\ntwo\nthree"), "{text:?}");
    assert_eq!(text.matches("one").count(), 1, "replayed twice: {text:?}");

    // An unsavable handoff refuses SIGTERM without losing the core or its screen.
    let blocked = s.root.join("state/herdr-core/test/restart-env.json");
    std::fs::create_dir(&blocked).unwrap();
    unsafe { libc::kill(new as i32, libc::SIGTERM) };
    wait_until("SIGTERM handoff refused", || {
        std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log"))
            .is_ok_and(|log| log.contains("core: cannot restart:"))
    });
    assert_eq!(status(&s)["pid"].as_i64(), Some(new));
    assert_eq!(get(&s, &t)["screen"], "exact");
    std::fs::remove_dir(blocked).unwrap();

    // SIGTERM (e.g. a service manager) can be retried after the failure.
    unsafe { libc::kill(new as i32, libc::SIGTERM) };
    wait_until("core exit", || !pid_alive(new));
    assert_eq!(get(&s, &t)["screen"], "exact");
}

#[test]
fn finished_terminals_and_ids_survive_server_stop() {
    let s = Session::new();
    let done = run(&s, &["sh", "-c", "echo bye"]);
    wait_finished(&s, &done);
    let live = run(&s, &["sleep", "100"]);
    let pid = get(&s, &live)["pid"].as_i64().unwrap();
    s.json(&["server", "stop"]);
    assert!(!pid_alive(pid));

    assert!(text(&s, &done).contains("bye"));
    let rec = get(&s, &live);
    assert_eq!(rec["state"], "finished");
    assert_eq!(rec["reason"], "server_stop");
    assert_eq!(run(&s, &["true"]), "t3");
}

#[test]
fn server_stop_typed_inside_a_terminal_still_stops_the_core() {
    let s = Session::new();
    let core = status(&s)["pid"].as_i64().unwrap();
    // Stopping ends this terminal, and with it the hc waiting for the reply.
    run(&s, &["sh", "-c", &format!("{} server stop; sleep 100", s.bin.display())]);
    wait_until("core exits", || !pid_alive(core));
    let t = run(&s, &["sh", "-c", "echo again"]);
    wait_until("a fresh core runs programs again", || text(&s, &t).contains("again"));
}

#[test]
fn a_command_cut_off_by_a_restart_says_so() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let waiting = {
        let mut cmd = s.cmd();
        cmd.args(["wait", &t, "--timeout", "30"]);
        std::thread::spawn(move || cmd.output().unwrap())
    };
    std::thread::sleep(Duration::from_millis(300));
    s.json(&["server", "restart"]);
    let out = waiting.join().unwrap();
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(err["error"]["message"].as_str().unwrap().contains("restarted"), "{err}");
    assert_eq!(get(&s, &t)["state"], "live", "and the program kept running");
}

#[test]
fn losing_the_holder_finishes_everything() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", r"printf '\033]7501;state=working\033\\'; sleep 100"]);
    wait_until("working", || get(&s, &t)["status"]["state"] == "working");
    let core = status(&s)["pid"].as_i64().unwrap();
    let holder = holder_pid(&s);
    kill9(core);
    kill9(holder);

    let rec = get(&s, &t);
    assert_eq!(rec["state"], "finished");
    assert_eq!(rec["reason"], "stopped_by_restart");
    assert_eq!((rec["status"].clone(), rec["turn"].as_u64()), (Value::Null, Some(1)), "ended like any exit: {rec}");
    assert_eq!(run(&s, &["true"]), "t2");
}

#[test]
fn a_turn_that_starts_and_ends_in_one_burst_survives_losing_the_holder() {
    let s = Session::new();
    // Idle before and after: only the reports in between show the turn.
    let burst = r"printf '\033]7501;state=idle\033\\'; sleep 0.3
        printf '\033]7501;state=working\033\\\033]7501;state=done\033\\\033]7501;state=idle\033\\'; sleep 100";
    let t = run(&s, &["sh", "-c", burst]);
    wait_until("the turn", || get(&s, &t)["turn"] == 1);
    let (core, holder) = (status(&s)["pid"].as_i64().unwrap(), holder_pid(&s));
    kill9(core);
    kill9(holder);
    assert_eq!(get(&s, &t)["turn"], 1);
}

#[test]
fn holder_restarts_a_crashed_core_even_after_an_update() {
    let mut s = Session::new();
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "100".into()));
    s.bin = s.root.join("herdr-core");
    std::fs::copy(BIN, &s.bin).unwrap();
    run(&s, &["sleep", "100"]);
    let core = status(&s)["pid"].as_i64().unwrap();
    // An update swaps the binary under the running holder.
    let staged = s.root.join("herdr-core.new");
    write_new_build(&staged);
    std::fs::rename(&staged, &s.bin).unwrap();
    kill9(core);
    // No CLI call here: only the holder can bring a core back.
    wait_until("holder-started core", || s.socket().exists() && s.connect_ok());
    assert_ne!(status(&s)["pid"].as_i64().unwrap(), core);
    assert_eq!(get(&s, "t1")["state"], "live");
}

#[test]
fn an_update_rolls_new_terminals_onto_a_new_holder_while_the_old_one_drains() {
    let mut s = Session::new();
    s.bin = s.root.join("herdr-core");
    std::fs::copy(BIN, &s.bin).unwrap();
    let old = run(&s, &["sleep", "100"]);
    let old_holder = holder_pid(&s);

    // Install a new build, then restart the planned way.
    let staged = s.root.join("herdr-core.new");
    write_new_build(&staged);
    std::fs::rename(&staged, &s.bin).unwrap();
    s.json(&["server", "restart"]);

    let new = run(&s, &["sleep", "100"]);
    let new_holder = holder_pid(&s);
    assert_ne!(new_holder, old_holder, "new terminals go to a holder of the new build");
    assert_eq!(status(&s)["holders"].as_array().unwrap().len(), 2);
    assert!(pid_alive(old_holder), "the old holder keeps the old terminal");

    // Still one core for both: a crash adopts terminals from both holders.
    kill9(status(&s)["pid"].as_i64().unwrap());
    assert_eq!((get(&s, &old)["state"].as_str(), get(&s, &new)["state"].as_str()), (Some("live"), Some("live")));

    s.json(&["kill", &old]);
    wait_until("old holder drains and exits", || !pid_alive(old_holder));
    assert_eq!(get(&s, &new)["state"], "live");
    assert_eq!(status(&s)["holders"].as_array().unwrap().len(), 1);
}

fn holder_pid(s: &Session) -> i64 {
    status(s)["holder"]["pid"].as_i64().unwrap()
}

#[test]
fn title_changes_are_saved_shortly_after_not_right_away() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "for i in 1 2 3 4 5 6 7 8 9; do printf '\\033]2;spin %s\\007' $i; done; sleep 100"]);
    wait_until("last title", || get(&s, &t)["title"] == "spin 9");
    let saved = || {
        let file = std::fs::read(s.root.join("state/herdr-core/test/terminals.json")).unwrap();
        serde_json::from_slice::<Value>(&file).unwrap()["terminals"]["1"]["title"].clone()
    };
    assert_eq!(saved(), "", "saved on a title change");
    wait_until("title saved", || saved() == "spin 9");
}
