#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;

fn raw_core(s: &Session) -> std::process::Child {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let mut cmd = s.cmd();
    // Keep this serve in the child on macOS instead of testing a launchd launcher.
    cmd.env("SSH_CONNECTION", "test-direct");
    let mut vars: Vec<_> = cmd
        .get_envs()
        .filter_map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value?.as_bytes());
            Some(CString::new(entry).unwrap())
        })
        .collect();
    vars.extend([CString::new("=C:=/drive-c").unwrap(), CString::new("=::=/drive-other").unwrap()]);
    let exe = CString::new(s.bin.as_os_str().as_bytes()).unwrap();
    let serve = CString::new("serve").unwrap();
    // Build only stack-backed pointers after fork, with no allocator locks.
    assert!(vars.len() < 64);
    unsafe {
        cmd.pre_exec(move || {
            let argv = [exe.as_ptr(), serve.as_ptr(), std::ptr::null()];
            let mut env = [std::ptr::null(); 64];
            for (slot, entry) in env.iter_mut().zip(&vars) {
                *slot = entry.as_ptr();
            }
            libc::execve(exe.as_ptr(), argv.as_ptr(), env.as_ptr());
            Err(std::io::Error::last_os_error())
        });
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn fresh_restart_replaces_the_base_without_losing_live_programs() {
    let mut s = Session::new();
    s.env.push(("NO_COLOR".into(), "1".into()));
    s.env.push(("FOO".into(), "dirty".into()));
    let t = run(&s, &["sleep", "100"]);
    let pid = get(&s, &t)["pid"].as_i64().unwrap();
    let core_pid = s.json(&["server", "status"])["pid"].clone();
    s.env.retain(|(key, _)| key != "NO_COLOR" && key != "FOO");
    let probe = "printf '%s|%s' \"${NO_COLOR-unset}\" \"${FOO-unset}\"";
    s.json(&["server", "restart"]);
    assert_eq!(String::from_utf8_lossy(&s.hc(&["exec", probe]).stdout), "1|dirty");
    s.json(&["server", "restart", "--fresh"]);
    assert_ne!(s.json(&["server", "status"])["pid"], core_pid);
    assert_eq!(String::from_utf8_lossy(&s.hc(&["exec", probe]).stdout), "unset|unset");
    assert_eq!(get(&s, &t)["pid"], pid);
}

#[test]
fn fresh_restart_without_a_running_core_uses_the_caller_environment() {
    let mut s = Session::new();
    s.env.push(("FOO".into(), "abandoned".into()));
    saved_restart(&s);
    s.env.retain(|(key, _)| key != "FOO");
    s.json(&["server", "restart", "--fresh"]);
    let out = s.hc(&["exec", "printf '%s' \"${FOO-unset}\""]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "unset");
}

#[test]
fn a_holder_winning_the_restart_race_restores_the_callers_environment() {
    let mut s = Session::new();
    s.env.push(("HC_RESTART_VALUE".into(), "old-holder".into()));
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "100".into()));
    let t = run(&s, &["sleep", "100"]);
    let pid = get(&s, &t)["pid"].clone();
    let core_pid = s.json(&["server", "status"])["pid"].clone();
    let mut environment: std::collections::BTreeMap<_, _> = s
        .cmd()
        .get_envs()
        .filter_map(|(key, value)| Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned())))
        .collect();
    environment.remove("HC_RESTART_VALUE");
    environment.insert("HC_CALLER_ONLY".to_owned(), "fresh".to_owned());
    let reply = s.connect().call(1, "server.restart", serde_json::json!({"environment": environment}));
    assert!(reply.get("error").is_none(), "{reply}");
    // Connect only: the holder must launch the successor, never this test.
    wait_until("holder starts the replacement core", || {
        let Ok(stream) = herdr_core::platform::connect(&s.socket()) else { return false };
        herdr_core::client::call(&stream, "server.status", serde_json::json!({}))
            .is_ok_and(|status| status["pid"] != core_pid)
    });
    assert_eq!(get(&s, &t)["pid"], pid);
    let out = s.hc(&["exec", "printf '%s|%s' \"$HC_RESTART_VALUE\" \"$HC_CALLER_ONLY\""]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "|fresh");
}

#[test]
fn fresh_restart_refuses_a_core_without_environment_handoff_support() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let s = Session::new();
    std::fs::create_dir_all(s.socket().parent().unwrap()).unwrap();
    let listener = UnixListener::bind(s.socket()).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let req: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["method"], "hello");
        writeln!(stream, "{}", serde_json::json!({"id": 1, "result": {"features": []}})).unwrap();
        line.clear();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0, "the older core must keep running");
    });
    let out = s.hc(&["server", "restart", "--fresh"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--fresh needs a newer running core"));
    server.join().unwrap();
    std::fs::remove_file(s.socket()).unwrap();
}

#[test]
fn fresh_restart_refuses_an_in_progress_handoff_without_a_socket() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    std::fs::create_dir_all(&state).unwrap();
    let lock = std::fs::File::create(state.join("core.lock")).unwrap();
    assert!(herdr_core::platform::try_lock(&lock));
    let out = s.hc(&["server", "restart", "--fresh"]);
    drop(lock);
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("starting or restarting"), "{out:?}");
}

#[test]
fn fresh_restart_warns_inside_a_herdr_terminal() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let out = s.cmd().env("HC_TERMINAL_ID", "t99").args(["server", "restart", "--fresh"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("from a terminal outside Herdr"), "{out:?}");
}

#[test]
fn a_replacement_environment_is_validated_before_restarting() {
    let s = Session::new();
    let pid = s.json(&["server", "start"])["pid"].clone();
    for env in [
        serde_json::json!({}),
        serde_json::json!({"FOO": "missing PATH"}),
        serde_json::json!({"PATH": "/bin", "BAD=KEY": "value"}),
        serde_json::json!({"PATH": "/bin", "BAD": "nul\0value"}),
    ] {
        let reply = s.connect().call(1, "server.restart", serde_json::json!({"environment": env}));
        assert_eq!(reply["error"]["data"]["kind"], "invalid_params", "{reply}");
        assert_eq!(s.json(&["server", "status"])["pid"], pid);
    }
}

#[test]
fn inherited_drive_entries_do_not_break_saving_or_restoring_environment() {
    for restore in [false, true] {
        let mut s = Session::new();
        s.env.push(("HC_RESTART_VALUE".into(), "saved".into()));
        if restore {
            saved_restart(&s);
        }
        let mut core = raw_core(&s);
        wait_until("raw-environment core starts or exits", || core.try_wait().unwrap().is_some() || s.connect_ok());
        assert!(core.try_wait().unwrap().is_none(), "a raw inherited entry must not panic startup");
        if restore {
            let out = s.hc(&["exec", "printf '%s' \"$HC_RESTART_VALUE\""]);
            assert_eq!(String::from_utf8_lossy(&out.stdout), "saved");
            s.json(&["server", "stop"]);
        } else {
            assert!(s.connect().call(1, "server.restart", serde_json::json!({})).get("error").is_none());
            wait_until("raw-environment core exits", || core.try_wait().unwrap().is_some());
            s.env.retain(|(key, _)| key != "HC_RESTART_VALUE");
            s.env.push(("HC_RESTART_VALUE".into(), "caller".into()));
            let out = s.hc(&["exec", "printf '%s' \"$HC_RESTART_VALUE\""]);
            assert_eq!(String::from_utf8_lossy(&out.stdout), "saved");
        }
        assert!(core.wait().unwrap().success());
    }
}

fn snapshot(s: &Session) -> std::path::PathBuf {
    s.root.join("state/herdr-core/test/restart-env.json")
}

fn saved_restart(s: &Session) -> Vec<u8> {
    let pid = s.json(&["server", "start"])["pid"].as_i64().unwrap();
    assert!(s.connect().call(1, "server.restart", serde_json::json!({})).get("error").is_none());
    wait_until("the retiring core exits", || !pid_alive(pid));
    std::fs::read(snapshot(s)).unwrap()
}

#[test]
fn a_core_started_directly_restores_the_snapshot_before_adopting_holders() {
    use std::os::unix::ffi::OsStringExt;
    use std::process::Stdio;
    let mut s = Session::new();
    s.env.push(("HC_RESTART_VALUE".into(), "saved-core".into()));
    saved_restart(&s);
    let mut note: serde_json::Value = serde_json::from_slice(&std::fs::read(snapshot(&s)).unwrap()).unwrap();
    let vars = note["vars"].as_array_mut().unwrap();
    vars.push(
        serde_json::to_value((std::ffi::OsString::from("HC_RAW"), std::ffi::OsString::from_vec(vec![b'a', 0xff])))
            .unwrap(),
    );
    std::fs::write(snapshot(&s), serde_json::to_vec(&note).unwrap()).unwrap();
    let mut core = s
        .cmd()
        .env("HC_RESTART_VALUE", "old-holder")
        .env("HC_HOLDER_ONLY", "stale")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the directly started core", || s.connect_ok());
    let out = s.hc(&["exec", "printf '%s|%s' \"$HC_RESTART_VALUE\" \"${HC_HOLDER_ONLY-unset}\""]);
    let holder = run(&s, &["sleep", "100"]);
    assert_eq!(get(&s, &holder)["state"], "live");
    if cfg!(target_os = "linux") {
        let holder_pid = s.json(&["server", "status"])["holder"]["pid"].as_u64().unwrap();
        let env = std::fs::read(format!("/proc/{holder_pid}/environ")).unwrap();
        assert!(env.split(|byte| *byte == 0).any(|pair| pair == b"HC_RAW=a\xff"));
    }
    s.json(&["server", "stop"]);
    assert!(core.wait().unwrap().success());
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "saved-core|unset");
    assert!(!snapshot(&s).exists());
}

#[test]
fn invalid_or_stale_snapshots_leave_the_fresh_environment_intact() {
    for bad in ["broken", "directory", "machine", "boot", "expired", "future", "metadata", "key", "value", "duplicate"]
    {
        let mut s = Session::new();
        s.env.push(("HC_RESTART_VALUE".into(), "saved".into()));
        let bytes = saved_restart(&s);
        let mut note: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        match bad {
            "machine" => note["machine_id"] = "another-machine".into(),
            "boot" => note["boot_id"] = "another-boot".into(),
            "expired" => note["written_ms"] = 1.into(),
            "future" => note["written_ms"] = u64::MAX.into(),
            "metadata" => {
                note.as_object_mut().unwrap().remove("written_ms");
            }
            "key" | "value" | "duplicate" => {
                let vars = note["vars"].as_array_mut().unwrap();
                let (key, value) = match bad {
                    "key" => (std::ffi::OsString::from("BAD=KEY"), std::ffi::OsString::from("bad")),
                    "value" => ("BAD_VALUE".into(), "bad\0value".into()),
                    _ => ("HC_RESTART_VALUE".into(), "duplicate".into()),
                };
                vars.push(serde_json::to_value((key, value)).unwrap());
            }
            _ => {}
        }
        if bad == "directory" {
            std::fs::remove_file(snapshot(&s)).unwrap();
            std::fs::create_dir(snapshot(&s)).unwrap();
        } else {
            std::fs::write(
                snapshot(&s),
                if bad == "broken" { b"{bad".to_vec() } else { serde_json::to_vec(&note).unwrap() },
            )
            .unwrap();
        }
        s.env.retain(|(key, _)| key != "HC_RESTART_VALUE");
        s.env.push(("HC_RESTART_VALUE".into(), "fresh".into()));
        let out = s.hc(&["exec", "printf '%s' \"$HC_RESTART_VALUE\""]);
        assert!(out.status.success(), "{bad}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "fresh", "{bad}");
        if bad == "directory" {
            std::fs::remove_dir(snapshot(&s)).unwrap();
        } else {
            assert!(!snapshot(&s).exists(), "{bad}");
        }
    }
}

#[test]
fn a_failed_or_competing_start_does_not_consume_a_valid_snapshot() {
    use std::process::Stdio;
    let mut s = Session::new();
    s.env.push(("HC_RESTART_VALUE".into(), "saved".into()));
    let bytes = saved_restart(&s);
    let registry = s.root.join("state/herdr-core/test/terminals.json");
    std::fs::write(&registry, b"broken").unwrap();
    // Observe serve's failure, not a successful intermediate launchd detach.
    let failed = s.cmd().env("SSH_CONNECTION", "test-direct").arg("serve").output().unwrap();
    assert!(!failed.status.success());
    assert!(snapshot(&s).exists(), "retry must still be able to restore");
    std::fs::remove_file(registry).unwrap();
    s.json(&["server", "start"]);
    assert!(!snapshot(&s).exists());
    std::fs::write(snapshot(&s), &bytes).unwrap();
    let out = s
        .cmd()
        .env("SSH_CONNECTION", "test-direct")
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(std::fs::read(snapshot(&s)).unwrap(), bytes, "a losing starter cannot consume the note");
    s.json(&["server", "stop"]);
    assert!(!snapshot(&s).exists(), "deliberate stop discards pending handoff");
}

#[test]
fn stop_while_down_and_startup_remove_abandoned_environment_files() {
    let s = Session::new();
    let bytes = saved_restart(&s);
    let tmp = snapshot(&s).with_file_name("restart-env.abandoned.tmp");
    std::fs::write(&tmp, &bytes).unwrap();
    s.json(&["server", "stop"]);
    assert!(!snapshot(&s).exists());
    assert!(!tmp.exists());
    std::fs::write(&tmp, bytes).unwrap();
    s.json(&["server", "start"]);
    assert!(!tmp.exists());
}

#[test]
fn an_unsavable_environment_never_blocks_restart_or_sigterm() {
    for restart in [true, false] {
        let s = Session::new();
        let first = s.json(&["server", "start"])["pid"].as_i64().unwrap();
        std::fs::create_dir(snapshot(&s)).unwrap();
        if restart {
            assert_ne!(s.json(&["server", "restart"])["pid"].as_i64(), Some(first));
        } else {
            assert_eq!(unsafe { libc::kill(first as i32, libc::SIGTERM) }, 0);
            wait_until("SIGTERM exits even when the note is unsavable", || !pid_alive(first));
        }
        std::fs::remove_dir(snapshot(&s)).unwrap();
    }
}

#[test]
fn sigterm_leaves_no_environment_for_the_next_fresh_start() {
    let mut s = Session::new();
    s.env.push(("HC_RESTART_VALUE".into(), "old".into()));
    let pid = s.json(&["server", "start"])["pid"].as_i64().unwrap();
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    wait_until("SIGTERM exits", || !pid_alive(pid));
    assert!(!snapshot(&s).exists());
    s.env.retain(|(key, _)| key != "HC_RESTART_VALUE");
    s.env.push(("HC_RESTART_VALUE".into(), "fresh".into()));
    let out = s.hc(&["exec", "printf '%s' \"$HC_RESTART_VALUE\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fresh");
}

/// A display whose server is gone is worse than none: a program that reaches
/// for the clipboard fails on it instead of using its own.
#[test]
fn a_program_gets_no_display_whose_server_is_gone() {
    let mut s = Session::new();
    s.env.push(("DISPLAY".into(), ":99999".into()));
    let out = s.hc(&["exec", "echo \"[${DISPLAY-unset}]\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[unset]\n", "the dead display was handed out");
}

/// A display on another machine is taken as it is: we cannot see it.
#[test]
fn a_display_on_another_machine_stays() {
    let mut s = Session::new();
    s.env.push(("DISPLAY".into(), "otherhost:0".into()));
    let out = s.hc(&["exec", "echo \"[${DISPLAY-unset}]\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[otherhost:0]\n");
}
