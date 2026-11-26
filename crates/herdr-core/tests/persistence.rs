#![cfg(target_os = "linux")]

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::symlink;

#[test]
fn failed_registry_and_stderr_writes_do_not_stop_persistence() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    fs::create_dir_all(&state).unwrap();
    symlink("/dev/full", state.join("core.log")).unwrap();
    let before = run(&s, &["sleep", "600"]);
    let tmp = state.join("terminals.tmp");
    // A preexisting link blocks the private registry temp file. The error diagnostic returns ENOSPC.
    symlink("/dev/full", &tmp).unwrap();
    let (_, error) = s.err(&["--json", "run", "--bg", "--name", "during", "sleep", "600"]);
    fs::remove_file(&tmp).unwrap();
    assert_eq!(error["error"]["kind"], "unavailable", "{error}");
    assert!(error["error"]["message"].as_str().unwrap().contains("may already have completed"), "{error}");
    wait_until("failed registry save retried without another mutation", || {
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(state.join("terminals.json")).unwrap()).unwrap();
        saved["terminals"].as_object().unwrap().len() == 2
    });
    let terminals = [before.as_str(), "during"];
    let pids: Vec<_> = terminals.iter().map(|id| get(&s, id)["pid"].clone()).collect();
    s.json(&["server", "restart"]);
    for (id, pid) in terminals.iter().zip(pids) {
        assert_eq!(get(&s, id)["pid"], pid, "{id} survives restart");
    }
}

#[test]
fn failed_environment_and_stderr_writes_do_not_block_restart() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    fs::create_dir_all(&state).unwrap();
    symlink("/dev/full", state.join("core.log")).unwrap();
    let t = run(&s, &["sleep", "600"]);
    let pid = get(&s, &t)["pid"].clone();
    fs::create_dir(state.join("restart-env.json")).unwrap();
    let first = s.json(&["server", "status"])["pid"].clone();
    assert_ne!(s.json(&["server", "restart"])["pid"], first);
    assert_eq!(get(&s, &t)["pid"], pid);
}
