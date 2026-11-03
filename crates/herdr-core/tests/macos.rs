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
    let bytes = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
    pids.truncate(bytes.max(0) as usize / 4);
    let ours = |pid: &i32| {
        let mut info = [0u64; 5];
        let size = std::mem::size_of_val(&info) as i32;
        unsafe { libc::proc_pidinfo(*pid, 20, 0, info.as_mut_ptr().cast(), size) == size && info[0] == id }
    };
    pids.iter().filter(|p| **p > 0 && ours(p)).map(|p| *p as i64).collect()
}

/// "<program> <grandchild>": a program with a child of its own, both in one terminal.
fn tree(s: &Session, extra: &[&str]) -> (String, i64, i64) {
    let id = run(s, &[extra, &["sh", "-c", "sleep 300 & echo \"$$ $!\"; wait"]].concat());
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
fn launchd_preserves_environment_bytes_and_file_limits_without_its_own_variables() {
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
    assert_eq!(out.stdout, b"512\n1024\n\x1b[1m\r\nunset unset 0");
}

#[test]
fn every_resource_limit_survives_launchd() {
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
    let expected: Vec<_> = limits.iter().map(|&(_, soft, hard)| (soft as i64, hard as i64)).collect();
    for (name, resource) in [("CORE", libc::RLIMIT_CORE), ("CPU", libc::RLIMIT_CPU), ("STACK", libc::RLIMIT_STACK)] {
        eprintln!("{name}: starter {:?}, program {:?}", expected[resource as usize], actual[resource as usize]);
    }
    assert_eq!(actual, expected, "every Darwin resource limit is inherited");
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
    for pid in members(coalitions(a_program)[0]) {
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
    for (s, name) in [(&launchd, "launchd"), (&direct, "direct")] {
        let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/holder.log")).unwrap_or_default();
        assert!(!log.contains("no launchd job"), "{name}: {log}");
    }
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
        let jobs: Vec<_> = (0..30)
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

#[test]
fn programs_start_directly_when_the_job_binary_is_an_older_build() {
    if !gui() {
        return;
    }
    let Some(old) = std::env::var_os("HC_OLD_BIN") else {
        eprintln!("skipping: no HC_OLD_BIN");
        return;
    };
    let mut s = Session::new();
    let bin = s.root.join("herdr-core");
    std::fs::copy(BIN, &bin).unwrap();
    s.bin = bin.clone();
    run(&s, &["sleep", "300"]);
    let (core, holder) = (s.json(&["server", "status"])["pid"].as_i64().unwrap(), holder_of(&s));
    if !independent(&s, core) {
        return;
    }
    // The holder now finds an older build at its path: its jobs end in a usage error.
    std::fs::copy(&old, s.root.join("old")).unwrap();
    std::fs::rename(s.root.join("old"), &bin).unwrap();
    let started = std::time::Instant::now();
    let id = run(&s, &["sh", "-c", "echo $$; exec sleep 300"]);
    wait_until("direct start", || text(&s, &id).trim().parse::<i64>().is_ok());
    assert!(started.elapsed() < std::time::Duration::from_secs(8), "{:?}", started.elapsed());
    assert_eq!(coalitions(text(&s, &id).trim().parse().unwrap()), coalitions(holder), "started by the holder itself");
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/holder.log")).unwrap();
    assert!(log.contains("starting programs directly"), "{log}");
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
