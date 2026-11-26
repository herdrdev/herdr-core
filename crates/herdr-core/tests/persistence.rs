#![cfg(target_os = "linux")]

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn the_registry_is_private_even_with_a_leftover_temp_file() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    run(&s, &["sleep", "600"]);
    let registry = state.join("terminals.json");
    assert_eq!(fs::metadata(&registry).unwrap().permissions().mode() & 0o777, 0o600);
    let tmp = state.join("terminals.tmp");
    fs::write(&tmp, b"leftover").unwrap();
    fs::set_permissions(tmp, fs::Permissions::from_mode(0o644)).unwrap();
    run(&s, &["sleep", "600"]);
    assert_eq!(fs::metadata(registry).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn a_linked_registry_temp_file_is_replaced_without_changing_its_target() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    let tmp = state.join("terminals.tmp");
    let other = s.root.join("other");
    run(&s, &["sleep", "600"]);
    fs::write(&other, b"keep this file").unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o640)).unwrap();
    for link in [fs::hard_link::<&std::path::Path, &std::path::Path>, symlink] {
        link(other.as_path(), tmp.as_path()).unwrap();
        run(&s, &["sleep", "600"]);
        assert!(fs::read(&other).unwrap() == b"keep this file", "linked target contents changed");
        assert_eq!(fs::metadata(&other).unwrap().permissions().mode() & 0o777, 0o640);
        let registry = state.join("terminals.json");
        assert!(fs::symlink_metadata(&registry).unwrap().is_file());
        assert_eq!(fs::metadata(registry).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn failed_registry_and_stderr_writes_do_not_stop_persistence() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    fs::create_dir_all(&state).unwrap();
    symlink("/dev/full", state.join("core.log")).unwrap();
    let before = run(&s, &["sleep", "600"]);
    let tmp = state.join("terminals.tmp");
    // Both the registry snapshot and its error diagnostic fail without needing a full disk.
    fs::create_dir(&tmp).unwrap();
    let (_, error) = s.err(&["--json", "run", "--bg", "--name", "during", "sleep", "600"]);
    fs::remove_dir(&tmp).unwrap();
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

#[test]
fn server_stop_replies_when_its_registry_flush_times_out() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    fs::create_dir_all(&state).unwrap();
    symlink("/dev/full", state.join("core.log")).unwrap();
    s.json(&["server", "status"]);
    while !state.join("terminals.json").exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let tmp = state.join("terminals.tmp");
    // Opening a fifo for writing waits for a reader: a stalled disk.
    assert!(std::process::Command::new("mkfifo").arg(&tmp).status().unwrap().success());
    let mut stop = s.cmd();
    stop.args(["server", "stop"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut stop = stop.spawn().unwrap();
    // Past the stop's 10 s flush timeout, before the final flush's.
    std::thread::sleep(std::time::Duration::from_secs(15));
    assert!(stop.try_wait().unwrap().is_none(), "the registry write must still be stalled");
    let reader = fs::File::open(&tmp).unwrap();
    let stop = stop.wait_with_output().unwrap();
    drop(reader);
    let _ = fs::remove_file(&tmp);
    assert!(stop.status.success(), "{}", String::from_utf8_lossy(&stop.stderr));
    let stop: serde_json::Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert_eq!(stop["stopped"], true);
}
