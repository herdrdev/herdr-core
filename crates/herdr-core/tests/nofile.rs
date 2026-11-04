#![cfg(unix)]

mod common;

use common::*;
use std::os::unix::process::CommandExt;

#[test]
fn low_nofile_startup_can_hold_one_hundred_terminals() {
    let s = Session::new();
    let mut cmd = s.cmd();
    cmd.args(["server", "status"]);
    unsafe {
        cmd.pre_exec(|| {
            let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = 256;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    if !out.status.success() {
        eprintln!("core log: {:?}", std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")));
    }
    assert!(out.status.success(), "{out:?}");
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let pid = status["pid"].as_u64().unwrap();
    for n in 0..100 {
        let out = s.hc(&["run", "--bg", "sleep", "600"]);
        if n % 10 == 0 || !out.status.success() {
            let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).map(|fds| fds.count()).ok();
            let opened = n + usize::from(out.status.success());
            let holder = s.json(&["server", "status"])["holder"]["pid"].as_u64().unwrap();
            let holder_fds = std::fs::read_dir(format!("/proc/{holder}/fd")).map(|fds| fds.count()).ok();
            eprintln!("{opened} terminals: core fds={fds:?}, holder fds={holder_fds:?}");
        }
        assert!(out.status.success(), "terminal {} failed: {}", n + 1, String::from_utf8_lossy(&out.stderr));
    }
    let out = s.hc(&["exec", "sh", "-c", "ulimit -Sn"]);
    assert!(out.status.success(), "{out:?}");
    let limit: u64 = String::from_utf8(out.stdout).unwrap().trim().parse().unwrap();
    eprintln!("program inherited NOFILE={limit}");
    assert!(limit >= 1024, "program inherited NOFILE={limit}");
}
