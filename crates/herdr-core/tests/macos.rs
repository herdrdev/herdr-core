#![cfg(target_os = "macos")]

mod common;

use common::*;
use serde_json::Value;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn coalition(pid: i64) -> u64 {
    let mut info = [0u64; 5];
    let size = std::mem::size_of_val(&info) as i32;
    let read = unsafe { libc::proc_pidinfo(pid as i32, 20, 0, info.as_mut_ptr().cast(), size) };
    assert_eq!(read, size, "coalition lookup for {pid}");
    info[0]
}

fn responsible(pid: i64) -> i64 {
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_get_pid_responsible_for_pid".as_ptr()) };
    assert!(!symbol.is_null(), "responsibility lookup unavailable");
    let lookup: unsafe extern "C" fn(libc::pid_t) -> libc::pid_t = unsafe { std::mem::transmute(symbol) };
    unsafe { lookup(pid as libc::pid_t) as i64 }
}

fn gui() -> bool {
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let available = Command::new("/bin/launchctl").args(["print", &domain]).output().unwrap().status.success();
    if !available {
        eprintln!("skipping: no GUI launchd domain");
    }
    available
}

fn independent(s: &Session, pid: i64) -> bool {
    if coalition(pid) != coalition(std::process::id() as i64) {
        return true;
    }
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap_or_default();
    if log.contains("launchctl bootstrap")
        && ["Operation not permitted", "Not privileged", "Permission denied"].iter().any(|error| log.contains(error))
    {
        eprintln!("skipping: bootstrap denied by this environment: {log}");
        return false;
    }
    panic!("core did not leave caller's coalition: {log}");
}

#[test]
fn executable_has_bound_core_identity_and_usage_descriptions() {
    let output = Command::new("/usr/bin/otool").args(["-P", BIN]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let plist = String::from_utf8(output.stdout).unwrap();
    for value in ["dev.herdr.core", "Herdr Core", "NSLocalNetworkUsageDescription", "NSAppleEventsUsageDescription"] {
        assert!(plist.contains(value), "{plist}");
    }
    assert!(!plist.contains("@VERSION@"), "{plist}");
}

#[test]
fn launchd_core_and_program_survive_the_launcher_and_leave_no_job() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let child = s.cmd().args(["server", "status"]).stdout(Stdio::piped()).spawn().unwrap();
    let launcher = child.id();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let first: Value = serde_json::from_slice(&output.stdout).unwrap();
    let core = first["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    let meta = std::fs::metadata(s.root.join("state/herdr-core/test/core.log")).unwrap();
    assert_eq!(meta.uid(), unsafe { libc::getuid() }, "first log belongs to the caller");
    let jobs = Command::new("/bin/launchctl").arg("list").output().unwrap();
    assert!(
        !String::from_utf8_lossy(&jobs.stdout).contains(&format!("dev.herdr.core.{launcher}.")),
        "one-shot job removed"
    );

    let id = run(
        &s,
        &["sh", "-c", "printf '%s %s %s\\n' $$ \"${HC_LAUNCHD_DETACH-unset}\" \"$XPC_SERVICE_NAME\"; exec sleep 300"],
    );
    let mut line = String::new();
    wait_until("program output", || {
        line = String::from_utf8(s.hc(&["read", &id]).stdout).unwrap();
        line.contains("unset 0")
    });
    let program: i64 = line.split_whitespace().next().unwrap().parse().unwrap();
    assert_eq!(coalition(program), coalition(core));
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
    assert_eq!(responsible(core), core, "detached core is live and responsible");
    assert_eq!(responsible(holder), holder, "holder owns its responsibility");
    assert_eq!(responsible(program), holder, "terminal programs must not disclaim");
    s.json(&["server", "restart"]);
    wait_until("new core", || s.json(&["server", "status"])["pid"].as_i64() != Some(core));
    assert!(pid_alive(program), "holder kept the terminal across handover");
    assert_eq!(responsible(program), holder, "responsibility survives the old core exiting");
    s.json(&["server", "stop"]);
    wait_until("program stopped", || !pid_alive(program));
    assert!(!s.socket().exists());
}

#[test]
fn launchd_core_registers_the_host_shutdown_watch() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
    assert!(!log.contains("cannot watch host shutdown"), "{log}");
}

#[test]
fn a_direct_serve_from_an_old_launcher_relaunches_before_taking_the_lock() {
    if !gui() {
        return;
    }
    let s = Session::new();
    // An old binary may have passed an earlier fallback marker on. It is
    // scoped to its original launcher, not permission to skip future escapes.
    let mut launcher = s.cmd().env("HC_LAUNCHD_DIRECT", "0").arg("serve").spawn().unwrap();
    wait_until("core listening", || s.connect_ok());
    let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, pid) {
        s.json(&["server", "stop"]);
        launcher.wait().unwrap();
        return;
    }
    assert!(launcher.wait().unwrap().success());
    assert_ne!(pid, launcher.id() as i64);
}

#[test]
fn launchd_preserves_environment_bytes_and_hard_file_limit_without_its_own_variables() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let mut cmd = s.cmd();
    cmd.args(["server", "status"])
        .env("LESS_TERMCAP_md", "\x1b[1m\r")
        .env("HC_TEST_NONUTF8", OsString::from_vec(vec![0xff]));
    let limit = libc::rlimit { rlim_cur: 512, rlim_max: 1024 };
    unsafe {
        cmd.pre_exec(move || {
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    if !independent(&s, status["pid"].as_i64().unwrap()) {
        return;
    }
    let out = s.hc(&["exec", "sh", "-c", "ulimit -Sn; ulimit -Hn; printf '%s' \"$LESS_TERMCAP_md\"; printf '\\n%s %s %s' \"${HC_LAUNCHD_DETACH-unset}\" \"${HC_LAUNCHD_DIRECT-unset}\" \"$XPC_SERVICE_NAME\""]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"1024\n1024\n\x1b[1m\r\nunset unset 0");
}

#[test]
fn resource_limits_survive_launchd_except_the_raised_nofile_soft_limit() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let resources = [
        libc::RLIMIT_CPU,
        libc::RLIMIT_FSIZE,
        libc::RLIMIT_DATA,
        libc::RLIMIT_STACK,
        libc::RLIMIT_CORE,
        libc::RLIMIT_AS,
        libc::RLIMIT_MEMLOCK,
        libc::RLIMIT_NPROC,
        libc::RLIMIT_NOFILE,
    ];
    let limits: Vec<_> = resources
        .into_iter()
        .map(|resource| {
            let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            assert_eq!(unsafe { libc::getrlimit(resource, &mut limit) }, 0);
            let (soft, hard) = match resource {
                libc::RLIMIT_CORE => (123456, 123456),
                libc::RLIMIT_CPU => (1234, 2345),
                libc::RLIMIT_STACK => (16 * 1024 * 1024, 32 * 1024 * 1024),
                _ => (limit.rlim_cur, limit.rlim_max),
            };
            (resource, soft, hard)
        })
        .collect();
    let starter_limits = limits.clone();
    let mut cmd = s.cmd();
    cmd.args(["server", "status"]);
    unsafe {
        cmd.pre_exec(move || {
            for &(resource, soft, hard) in &starter_limits {
                if ![libc::RLIMIT_CORE, libc::RLIMIT_CPU, libc::RLIMIT_STACK].contains(&resource) {
                    continue;
                }
                let limit = libc::rlimit { rlim_cur: soft, rlim_max: hard };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    if !independent(&s, status["pid"].as_i64().unwrap()) {
        return;
    }
    let out = s.hc(&[
        "exec",
        "python3",
        "-c",
        &format!("import resource,json; print(json.dumps([resource.getrlimit(i) for i in range({})]))", limits.len()),
    ]);
    assert!(out.status.success(), "{out:?}");
    let actual: Vec<(i64, i64)> = serde_json::from_slice(&out.stdout).unwrap();
    let expected: Vec<_> = limits
        .iter()
        .map(|&(resource, soft, hard)| {
            let soft = if resource == libc::RLIMIT_NOFILE { soft.max(hard.min(8192)) } else { soft };
            (soft as i64, hard as i64)
        })
        .collect();
    for (name, resource) in [("CORE", libc::RLIMIT_CORE), ("CPU", libc::RLIMIT_CPU), ("STACK", libc::RLIMIT_STACK)] {
        eprintln!("{name}: starter {:?}, program {:?}", expected[resource as usize], actual[resource as usize]);
    }
    assert_eq!(actual, expected, "Darwin limits are inherited, except the raised NOFILE soft limit");
}

#[test]
fn serve_help_does_not_start_a_core_and_leading_global_options_still_escape() {
    let s = Session::new();
    let out = s.hc(&["serve", "--help"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"), "{out:?}");
    assert!(!s.socket().exists(), "help must not start a core");
    let out = s.hc(&["serve", "--not-a-real-flag"]);
    assert!(!out.status.success());
    assert!(!s.socket().exists(), "invalid arguments must not start a core");
    if !gui() {
        return;
    }
    let mut launcher = s.cmd().args(["--json", "serve"]).spawn().unwrap();
    wait_until("core listening", || s.connect_ok());
    let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, pid) {
        s.json(&["server", "stop"]);
        launcher.wait().unwrap();
        return;
    }
    assert!(launcher.wait().unwrap().success());
}

#[test]
fn ssh_starts_directly_without_an_app_lifetime_warning() {
    let mut s = Session::new();
    s.env.push(("SSH_CONNECTION".into(), "test".into()));
    let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    assert_eq!(coalition(pid), coalition(std::process::id() as i64));
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
    assert!(!log.contains("launchd startup failed"), "{log}");
}

#[test]
fn launchd_failure_logs_once_and_preserves_direct_start() {
    let s = Session::new();
    let cwd = s.root.join("line\rbreak");
    std::fs::create_dir(&cwd).unwrap();
    let out = s.cmd().current_dir(cwd).args(["server", "status"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(coalition(status["pid"].as_i64().unwrap()), coalition(std::process::id() as i64));
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
    assert_eq!(log.matches("launchd startup failed; starting directly").count(), 1, "{log}");
    assert!(log.contains("XML control character"), "{log}");
}

#[test]
fn an_old_core_hands_over_to_an_independent_fixed_core() {
    if !gui() {
        return;
    }
    let Some(old) = std::env::var_os("HC_OLD_BIN") else {
        eprintln!("skipping: no HC_OLD_BIN");
        return;
    };
    let mut s = Session::new();
    let bin = s.root.join("herdr-core");
    std::fs::copy(&old, &bin).unwrap();
    s.bin = bin;
    let id = run(&s, &["sh", "-c", "echo before; while read line; do echo got-$line; done"]);
    wait_until("old terminal output", || text(&s, &id).contains("before"));
    let old_pid = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    // The updater AND the serving core here are still the installed baseline.
    // The old CLI remembers this install path, replaces itself, then starts
    // the new `serve` directly, exactly as Herdr.app's hc update does.
    let out = s.hc(&["update", "--from", BIN]);
    assert!(out.status.success(), "{out:?}");
    wait_until("replacement core", || s.connect_ok() && s.json(&["server", "status"])["pid"].as_i64() != Some(old_pid));
    let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, pid) {
        return;
    }
    s.json(&["send", &id, "after"]);
    wait_until("terminal survived old handover", || text(&s, &id).contains("got-after"));
    let fresh = run(&s, &["sh", "-c", "echo $$; exec sleep 300"]);
    let mut program = None;
    wait_until("new program after old updater", || {
        program = text(&s, &fresh).trim().parse::<i64>().ok();
        program.is_some()
    });
    assert_eq!(coalition(program.unwrap()), coalition(pid), "new holder and terminals belong to the fixed core");
    assert_eq!(get(&s, &id)["state"], "live");
}
