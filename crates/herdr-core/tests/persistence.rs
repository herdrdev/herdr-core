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
    // Both the registry snapshot and its error diagnostic return ENOSPC, without root or a full disk.
    symlink("/dev/full", &tmp).unwrap();
    let during = s.hc(&["run", "--bg", "sleep", "600"]);
    fs::remove_file(&tmp).unwrap();
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
