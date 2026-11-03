#![cfg(target_os = "macos")]

mod common;

use common::*;
use herdr_core::holder_proto as proto;
use serde_json::Value;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn coalition(pid: i64) -> u64 {
    coalitions(pid)[0]
}

/// The resource and jetsam coalition ids of a process.
fn coalitions(pid: i64) -> [u64; 2] {
    let mut info = [0u64; 5];
    let size = std::mem::size_of_val(&info) as i32;
    let read = unsafe { libc::proc_pidinfo(pid as i32, 20, 0, info.as_mut_ptr().cast(), size) };
    assert_eq!(read, size, "coalition lookup for {pid}");
    [info[0], info[1]]
}

/// Every live process whose resource coalition is `id`.
fn members(id: u64) -> Vec<i64> {
    let mut pids = vec![0i32; 8192];
    // Despite its name for the size argument, the result is a count of pids, not bytes.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
    pids.truncate(count.max(0) as usize);
    let ours = |pid: &i32| {
        let mut info = [0u64; 5];
        let size = std::mem::size_of_val(&info) as i32;
        unsafe { libc::proc_pidinfo(*pid, 20, 0, info.as_mut_ptr().cast(), size) == size && info[0] == id }
    };
    pids.iter().filter(|p| **p > 0 && ours(p)).map(|p| *p as i64).collect()
}

/// "<program> <grandchild>": a program with a child of its own, both in one terminal.
fn tree(s: &Session, extra: &[&str]) -> (String, i64, i64) {
    // It hangs up on nothing: only being killed ends it.
    let id = run(s, &[extra, &["sh", "-c", "trap '' HUP; sleep 300 & echo \"$$ $!\"; wait"]].concat());
    let mut pids = Vec::new();
    wait_until("program and its child", || {
        pids = text(s, &id).split_whitespace().filter_map(|w| w.parse::<i64>().ok()).collect();
        pids.len() == 2
    });
    (id, pids[0], pids[1])
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
    assert_ne!(coalition(program), coalition(core), "each terminal has a coalition of its own");
    s.json(&["server", "restart"]);
    wait_until("new core", || s.json(&["server", "status"])["pid"].as_i64() != Some(core));
    assert!(pid_alive(program), "holder kept the terminal across handover");
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
    assert_programs_start_in_the_holder(&s);
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
    assert_ne!(coalition(program.unwrap()), coalition(pid), "new terminals leave the core's coalition");
    assert_eq!(get(&s, &id)["state"], "live");
}

#[test]
fn every_terminal_program_has_its_own_coalition() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let (a, a_program, a_child) = tree(&s, &[]);
    let (_, b_program, _) = tree(&s, &[]);
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
    let (a_own, b_own, holder_own) = (coalitions(a_program), coalitions(b_program), coalitions(holder));
    for (slot, name) in [(0, "resource"), (1, "jetsam")] {
        assert_ne!(a_own[slot], holder_own[slot], "{name}: a program shares the holder's coalition");
        assert_ne!(a_own[slot], b_own[slot], "{name}: two terminals share a coalition");
        assert_ne!(a_own[slot], coalitions(core)[slot], "{name}: a program shares the core's coalition");
    }
    assert_eq!(coalitions(a_child), a_own, "a program's children stay with it");
    assert_eq!(get(&s, &a)["pid"].as_i64(), Some(a_program), "the reported pid is the program, not a launcher");
}

#[test]
fn ending_one_terminals_coalition_leaves_the_holder_and_other_terminals_alone() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let (a, a_program, a_child) = tree(&s, &["--keep"]);
    let (b, b_program, _) = tree(&s, &[]);
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
    let id = coalitions(a_program)[0];
    let all = members(id);
    assert!(all.contains(&a_program) && all.contains(&a_child), "{all:?}");
    assert!(!all.contains(&holder) && !all.contains(&core) && !all.contains(&b_program), "{all:?}");
    // What a macOS force quit does to an app: every process of its coalition.
    for pid in all {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    wait_until("the terminal's processes are gone", || members(id).is_empty());
    wait_until("the kept terminal is recovered", || {
        let r = get(&s, &a);
        r["state"] == "live" && r["pid"].as_i64().is_some_and(|p| p != a_program)
    });
    assert!(pid_alive(holder) && pid_alive(core), "the holder and core outlived the force quit");
    assert!(pid_alive(b_program) && get(&s, &b)["state"] == "live", "another terminal outlived it");
    assert!(!pid_alive(a_child));
}

#[test]
fn a_force_quit_of_an_unkept_terminal_is_an_exit() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let (a, a_program, _) = tree(&s, &[]);
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    let all = members(coalitions(a_program)[0]);
    assert!(!all.contains(&core) && !all.contains(&holder_of(&s)), "{all:?}");
    for pid in all {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    wait_until("exit reported", || get(&s, &a)["state"] == "finished");
    assert_eq!(get(&s, &a)["reason"], "exited");
}

#[test]
fn the_holder_has_its_own_coalition() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let id = run(&s, &["sleep", "300"]);
    let status = s.json(&["server", "status"]);
    let (core, holder) = (status["pid"].as_i64().unwrap(), status["holder"]["pid"].as_i64().unwrap());
    if !independent(&s, core) {
        return;
    }
    let own = coalitions(holder);
    assert_ne!(own[0], coalitions(core)[0], "holder shares the core's resource coalition");
    assert_ne!(own[1], coalitions(core)[1], "holder shares the core's jetsam coalition");
    assert_eq!(get(&s, &id)["state"], "live");
}

fn direct_session() -> Session {
    let mut s = Session::new();
    s.env.push(("SSH_CONNECTION".into(), "test".into()));
    s
}

fn holder_of(s: &Session) -> i64 {
    s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap()
}

fn parent_of(pid: i64) -> i64 {
    let out = Command::new("/bin/ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// A holder that never had a job of its own starts its programs itself, as before.
fn assert_programs_start_in_the_holder(s: &Session) {
    let id = run(s, &["sh", "-c", "echo $$; exec sleep 300"]);
    wait_until("program pid", || text(s, &id).trim().parse::<i64>().is_ok());
    let program = text(s, &id).trim().parse::<i64>().unwrap();
    let holder = holder_of(s);
    assert_eq!(parent_of(program), holder);
    assert_eq!(coalition(program), coalition(holder));
}

/// The program's pid and its limits, umask, folder and environment, as it sees them.
fn view(s: &Session) -> String {
    let id =
        run(s, &["--cwd", "/var", "sh", "-c", "echo $$; ulimit -a; umask; pwd; env | sort | grep -v '^_='; sleep 300"]);
    wait_until("program output", || text(s, &id).contains("HC_SESSION"));
    text(s, &id)
}

#[test]
fn a_program_started_through_a_job_sees_what_a_direct_one_does() {
    if !gui() {
        return;
    }
    let (launchd, direct) = (Session::new(), direct_session());
    let seen = |s: &Session| {
        let root = s.root.display().to_string();
        let lines = view(s).replace(&root, "ROOT");
        lines
            .lines()
            .skip(1)
            .filter(|l| !l.starts_with("XPC_") && !l.starts_with("HC_SCOPE"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (a, b) = (seen(&launchd), seen(&direct));
    if !independent(&launchd, launchd.json(&["server", "status"])["pid"].as_i64().unwrap()) {
        return;
    }
    assert_eq!(a, b, "limits, umask, folder and environment");
}

#[test]
fn start_errors_read_the_same_through_a_job_as_directly() {
    if !gui() {
        return;
    }
    let (launchd, direct) = (Session::new(), direct_session());
    for args in [
        vec!["/no/such/program"],
        vec!["no-such-program-on-the-path"],
        vec!["/etc/passwd"],
        vec!["--cwd", "/no/such/folder", "sh"],
    ] {
        let (a, b) = (
            launchd.err(&[&["run", "--bg"], args.as_slice()].concat()),
            direct.err(&[&["run", "--bg"], args.as_slice()].concat()),
        );
        assert_eq!(a.1["error"]["kind"], "spawn_failed", "{a:?}");
        assert_eq!(a.1["error"]["message"], b.1["error"]["message"], "{args:?}");
    }
}

#[test]
fn many_terminals_start_at_once_and_leave_no_job_or_stub_behind() {
    if !gui() {
        return;
    }
    let s = Session::new();
    run(&s, &["true"]);
    let holder = holder_of(&s);
    let ids: Vec<String> = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..12)
            .map(|i| {
                scope.spawn({
                    let s = &s;
                    move || run(s, &["sh", "-c", &format!("echo up-{i}; seq 1 20000; exit {}", i % 5)])
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    for (i, id) in ids.iter().enumerate() {
        wait_until("finished", || get(&s, id)["state"] == "finished");
        assert_eq!(get(&s, id)["exit_code"], i as i64 % 5);
    }
    let ours = format!("dev.herdr.core.{holder}.");
    wait_until("no job, directory or stub of the holder's", || {
        let jobs = Command::new("/bin/launchctl").arg("list").output().unwrap();
        let stubs = Command::new("/bin/ps").args(["-axo", "command="]).output().unwrap();
        let mine = format!("/tmp/hc-launchd-{holder}-");
        !String::from_utf8_lossy(&jobs.stdout).contains(&ours)
            && !String::from_utf8_lossy(&stubs.stdout).contains(&mine)
            && !std::fs::read_dir("/tmp")
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(&format!("hc-launchd-{holder}-")))
    });
}

#[test]
fn a_terminal_that_prints_a_lot_then_exits_loses_nothing() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let id = run(&s, &["sh", "-c", "seq 1 200000; (sleep 2 &); exit 7"]);
    wait_until("finished", || get(&s, &id)["state"] == "finished");
    let r = get(&s, &id);
    assert_eq!((r["exit_code"].as_i64(), r["reason"].as_str()), (Some(7), Some("exited")));
    assert!(text(&s, &id).contains("200000"));
}

/// A holder whose path holds a build that does not know the job sentinel (it ends in a usage error).
/// A terminal printing a line every 50 ms was started before it, and its id comes back with the holder's pid.
fn holder_with_an_older_build_at_its_path(s: &mut Session) -> (i64, String) {
    let bin = s.root.join("herdr-core");
    std::fs::copy(BIN, &bin).unwrap();
    s.bin = bin.clone();
    let ticker = run(s, &["sh", "-c", "i=0; while :; do i=$((i+1)); echo tick$i; sleep 0.05; done"]);
    let old = s.root.join("old");
    std::fs::write(&old, "#!/bin/sh\nexit 2\n").unwrap();
    std::fs::set_permissions(&old, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::rename(&old, &bin).unwrap();
    s.bin = BIN.into(); // the commands come from here, the holder's path holds the old build
    (holder_of(s), ticker)
}

#[test]
fn a_program_that_cannot_get_its_job_fails_instead_of_starting_in_the_holders_coalition() {
    if !gui() {
        return;
    }
    let mut s = Session::new();
    holder_with_an_older_build_at_its_path(&mut s);
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    // Not once, and not again a moment later: nothing is ever started in the holder's coalition silently.
    for _ in 0..2 {
        let (_, error) = s.err(&["run", "--bg", "sh", "-c", "echo started; exec sleep 300"]);
        assert_eq!(error["error"]["kind"], "spawn_failed", "{error}");
        assert!(error["error"]["message"].as_str().unwrap().contains("launchd"), "{error}");
    }
}

#[test]
fn a_slow_program_start_does_not_stop_the_output_of_other_terminals() {
    if !gui() {
        return;
    }
    let mut s = Session::new();
    let (_, ticker) = holder_with_an_older_build_at_its_path(&mut s);
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    let ticks = |s: &Session| {
        let seen = text(s, &ticker);
        seen.lines().filter_map(|l| l.strip_prefix("tick")?.trim().parse::<usize>().ok()).next_back().unwrap_or(0)
    };
    // The start that follows waits several seconds for a job that never answers.
    let failing = std::thread::scope(|scope| {
        let start = scope.spawn(|| s.err(&["run", "--bg", "true"]));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let before = ticks(&s);
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let during = ticks(&s) - before;
        assert!(during >= 10, "output of another terminal advanced by {during} lines in 1.2 s while a program started");
        start.join().unwrap()
    });
    assert_eq!(failing.1["error"]["kind"], "spawn_failed");
}

/// A core that connects while a program is starting (an update does) is welcomed after the start, not in the middle
/// of it, as when starting held the holder's state lock: otherwise its welcome and a drain could miss the program.
#[test]
fn a_core_connecting_during_a_start_is_welcomed_after_it() {
    if !gui() {
        return;
    }
    let mut s = Session::new();
    let (holder, _) = holder_with_an_older_build_at_its_path(&mut s);
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    let socket = std::fs::read_dir(s.root.join("run/herdr-core"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains(".holder.") && p.extension().is_some_and(|e| e == "sock"))
        .expect("the holder's socket");
    // The start that follows waits several seconds for a job that never answers.
    std::thread::scope(|scope| {
        scope.spawn(|| s.err(&["run", "--bg", "true"]));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut newer = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        newer.set_read_timeout(Some(std::time::Duration::from_secs(15))).unwrap();
        let hello = proto::json_frame(proto::HELLO, 0, &proto::Hello { version: proto::VERSION });
        std::io::Write::write_all(&mut newer, &hello).unwrap();
        let asked = std::time::Instant::now();
        assert_eq!(proto::read(&mut newer).unwrap().unwrap().0, proto::WELCOME);
        assert!(
            asked.elapsed() > std::time::Duration::from_secs(1),
            "welcomed in {:?}, in the middle of a start",
            asked.elapsed()
        );
        assert!(pid_alive(holder));
    });
}

#[test]
fn a_stub_lives_as_long_as_its_program_through_the_signals_a_process_killer_sends() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let id = run(&s, &["sh", "-c", "trap 'echo cleaned up' TERM; echo ready; while :; do sleep 1; done"]);
    wait_until("the program", || text(&s, &id).contains("ready"));
    let program = get(&s, &id)["pid"].as_i64().unwrap();
    if !independent(&s, s.json(&["server", "status"])["pid"].as_i64().unwrap()) {
        return;
    }
    let stub = parent_of(program);
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        unsafe { libc::kill(stub as i32, signal) };
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(pid_alive(stub) && pid_alive(program) && get(&s, &id)["state"] == "live");
    assert!(!text(&s, &id).contains("cleaned up"), "the program was signalled too");
    s.json(&["kill", &id]);
}

#[test]
fn losing_only_the_stub_ends_its_program_and_reports_a_kill() {
    if !gui() {
        return;
    }
    let s = Session::new();
    let (a, program, child) = tree(&s, &[]);
    let (kept, kept_program, kept_child) = tree(&s, &["--keep"]);
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    if !independent(&s, core) {
        return;
    }
    let holder = holder_of(&s);
    // What `pkill -f herdr-core` does to a stub, while its program lives on.
    for (stub, program) in [(parent_of(program), program), (parent_of(kept_program), kept_program)] {
        assert!(![core, holder].contains(&stub), "the program's parent is its stub");
        unsafe { libc::kill(stub as i32, libc::SIGKILL) };
        wait_until("the program is gone with its stub", || !pid_alive(program));
    }
    wait_until("exit reported", || get(&s, &a)["state"] == "finished");
    let r = get(&s, &a);
    assert_eq!((r["reason"].as_str(), r["signal"].as_i64()), (Some("exited"), Some(libc::SIGKILL as i64)), "{r}");
    wait_until("the kept terminal is recovered", || {
        let r = get(&s, &kept);
        r["state"] == "live" && r["pid"].as_i64().is_some_and(|p| p != kept_program)
    });
    assert!(pid_alive(core) && pid_alive(holder));
    // Only their parents were killed: these were left to launchd.
    for orphan in [child, kept_child] {
        unsafe { libc::kill(orphan as i32, libc::SIGKILL) };
    }
}

#[test]
fn ending_the_cores_coalition_leaves_the_holder_and_its_terminals_running() {
    if !gui() {
        return;
    }
    let mut s = Session::new();
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "200".into()));
    let (a, program, _) = tree(&s, &[]);
    let status = s.json(&["server", "status"]);
    let (core, holder) = (status["pid"].as_i64().unwrap(), status["holder"]["pid"].as_i64().unwrap());
    if !independent(&s, core) {
        return;
    }
    // What a force quit does to an app that was started through `hc exec`, a core's own child.
    let all = members(coalitions(core)[0]);
    assert!(all.contains(&core) && !all.contains(&holder) && !all.contains(&program), "{all:?}");
    for pid in all {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    wait_until("a new core", || {
        let out = s.hc(&["server", "status"]);
        out.status.success()
            && serde_json::from_slice::<Value>(&out.stdout).is_ok_and(|v| v["pid"].as_i64() != Some(core))
    });
    assert!(pid_alive(holder) && pid_alive(program));
    assert_eq!(get(&s, &a)["state"], "live");
}
