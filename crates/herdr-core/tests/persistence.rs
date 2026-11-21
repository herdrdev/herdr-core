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
    let snapshot = state.join("terminals.json");
    fs::remove_file(&snapshot).unwrap();
    // Atomic replacement cannot replace a directory. The error diagnostic also fails on /dev/full.
    fs::create_dir(&snapshot).unwrap();
    let during = s.hc(&["run", "--bg", "sleep", "600"]);
    fs::remove_dir(&snapshot).unwrap();
    assert!(during.status.success(), "{}", String::from_utf8_lossy(&during.stderr));
    let during: serde_json::Value = serde_json::from_slice(&during.stdout).unwrap();
    let during = during["address"].as_str().unwrap();
    let after = run(&s, &["sleep", "600"]);
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(state.join("terminals.json")).unwrap()).unwrap();
    assert_eq!(saved["terminals"].as_object().unwrap().len(), 3);
    let terminals = [before.as_str(), during, after.as_str()];
    let pids: Vec<_> = terminals.iter().map(|id| get(&s, id)["pid"].clone()).collect();
    s.json(&["server", "restart"]);
    for (id, pid) in terminals.iter().zip(pids) {
        assert_eq!(get(&s, id)["pid"], pid, "{id} survives restart");
    }
}

#[test]
fn server_stop_replies_when_its_registry_snapshot_cannot_be_replaced() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    fs::create_dir_all(&state).unwrap();
    symlink("/dev/full", state.join("core.log")).unwrap();
    run(&s, &["sleep", "600"]);
    let snapshot = state.join("terminals.json");
    fs::remove_file(&snapshot).unwrap();
    fs::create_dir(&snapshot).unwrap();
    // The runtime test injects a stalled writer to cover the distinct flush-timeout case.
    let stop = s.json(&["server", "stop"]);
    assert_eq!(stop["stopped"], true);
}
