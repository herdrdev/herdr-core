//! Ending everything a terminal started. Each terminal's environment carries
//! a random token that its descendants inherit, even those that leave its
//! session (agents' tool shells do), so the token finds them all. A process
//! that both leaves the session and drops its environment escapes: accepted.

#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(target_os = "openbsd")]
use crate::platform::{process_environ as environ, processes};

// OpenBSD exposes wall-clock process starts, not the monotonic clock needed
// for a safe age cutoff. Inspect all processes instead of filtering by age.
#[cfg(target_os = "openbsd")]
fn now() -> u64 {
    0
}

pub const VAR: &str = "HC_SCOPE";
/// Marks hc's own cores and holders: never swept, nor anything beneath them
/// (a terminal may start another session's core, which is not its own).
pub const RUNTIME: &str = "HC_RUNTIME";

/// A fresh token for one terminal, stamped with when it was made: no process
/// started before it can carry it, so a sweep need not read those.
pub fn token() -> String {
    format!("{}-{}", now(), crate::platform::random_hex())
}

/// The oldest a process carrying one of `tokens` can be (`now()`'s clock); 0
/// when one has no stamp (made by an older hc).
#[cfg(unix)]
fn oldest(tokens: &[String]) -> u64 {
    let stamp = |t: &String| t.split_once('-').and_then(|(s, _)| s.parse::<u64>().ok());
    tokens.iter().map(stamp).min().flatten().unwrap_or(0)
}

/// Now, on a clock no process started later can show an earlier start on, even
/// when the wall clock is set back: macOS's uptime clock, which process starts
/// are kept on.
#[cfg(target_os = "macos")]
fn now() -> u64 {
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
    }
    unsafe { mach_absolute_time() }
}

/// Linux, in clock ticks: a process's start is kept on the boot clock
/// (monotonic before 5.3), and a time namespace can shift the two apart, so
/// the lower of them.
#[cfg(target_os = "linux")]
fn now() -> u64 {
    let ns = |clock| {
        let mut t: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(clock, &mut t) } == 0 {
            t.tv_sec as u128 * 1_000_000_000 + t.tv_nsec as u128
        } else {
            0
        }
    };
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u128;
    (ns(libc::CLOCK_MONOTONIC).min(ns(libc::CLOCK_BOOTTIME)) * ticks / 1_000_000_000) as u64
}

#[cfg(windows)]
fn now() -> u64 {
    0
}

/// On Windows each terminal's job object ends everything it started when the
/// job closes (its program ended, or the holder died): nothing to hunt.
#[cfg(windows)]
pub fn sweep(_: &[String]) {}

#[cfg(windows)]
pub fn members(_: &[String]) -> Vec<i32> {
    Vec::new()
}

/// Hang up, then terminate, then kill every process carrying one of
/// `tokens`, looking again before each step (they may fork). Returns once none
/// remain or KILL had its time.
#[cfg(unix)]
pub fn sweep(tokens: &[String]) {
    for (signal, wait) in [(libc::SIGHUP, 500), (libc::SIGTERM, 500), (libc::SIGKILL, 2000)] {
        let pids = members(tokens);
        if pids.is_empty() {
            return;
        }
        for pid in pids {
            unsafe { libc::kill(pid, signal) };
        }
        let deadline = Instant::now() + Duration::from_millis(wait);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            if members(tokens).is_empty() {
                return;
            }
        }
    }
}

/// Live processes (zombies, us and hc's own processes excluded) whose
/// environment holds `HC_SCOPE=<one of tokens>`, plus their descendants:
/// macOS hides the environment of its own binaries (an agent's `/bin/bash`
/// tool shell), but not their parents.
#[cfg(unix)]
pub fn members(tokens: &[String]) -> Vec<i32> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let entries: std::collections::HashSet<Vec<u8>> =
        tokens.iter().map(|t| format!("{VAR}={t}").into_bytes()).collect();
    let runtime = format!("{RUNTIME}=1").into_bytes();
    let me = std::process::id() as i32;
    let (mut set, mut hc) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    let live: Vec<(i32, i32, u64)> = processes().into_iter().filter(|(pid, ..)| *pid != me).collect();
    let oldest = oldest(tokens);
    for &(pid, ..) in live.iter().filter(|(.., started)| *started >= oldest) {
        let Some(env) = environ(pid) else { continue };
        let mut vars = env.split(|b| *b == 0);
        if vars.clone().any(|e| e == runtime) {
            hc.insert(pid);
        } else if vars.any(|e| entries.contains(e)) {
            set.insert(pid);
        }
    }
    loop {
        let before = set.len();
        let children: Vec<i32> = live
            .iter()
            .filter(|(pid, ppid, _)| set.contains(ppid) && !hc.contains(pid))
            .map(|(pid, ..)| *pid)
            .collect();
        set.extend(children);
        if set.len() == before {
            return set.into_iter().collect();
        }
    }
}

/// Every live process as (pid, parent pid, start on `now()`'s clock or
/// later, `u64::MAX` when unknown); zombies left out.
#[cfg(target_os = "linux")]
fn processes() -> Vec<(i32, i32, u64)> {
    let stat = |pid: i32| -> Option<(i32, i32, u64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
        let (state, ppid) = (*fields.first()?, fields.get(1)?.parse().ok()?);
        let started = fields.get(19).and_then(|t| t.parse().ok()).unwrap_or(u64::MAX); // field 22, in ticks
        (state != "Z" && state != "X").then_some((pid, ppid, started))
    };
    let pids =
        std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok());
    pids.filter_map(stat).collect()
}

/// NUL-separated `KEY=value` entries; a zombie has none.
#[cfg(target_os = "linux")]
fn environ(pid: i32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok().filter(|env| !env.is_empty())
}

#[cfg(target_os = "macos")]
fn processes() -> Vec<(i32, i32, u64)> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0i32; n.max(0) as usize + 64];
    let size = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    pids.truncate(got.max(0) as usize);
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
            const SZOMB: u32 = 5;
            let mut usage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
            let read = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut usage).cast()) };
            let started = if read == 0 { usage.ri_proc_start_abstime } else { u64::MAX };
            (got == size && info.pbi_status != SZOMB).then_some((pid, info.pbi_ppid as i32, started))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn environ(pid: i32) -> Option<Vec<u8>> {
    crate::foreground::procargs(pid).map(|(_, env)| env)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn finds_only_processes_carrying_the_token() {
        let tokens = [token()];
        // Not macOS's own `sleep`: it hides its environment.
        let sleeper = |scope: &str| python("import time; time.sleep(30)").env(VAR, scope).spawn().unwrap();
        let (mut ours, mut other) = (sleeper(&tokens[0]), sleeper("someone-else"));
        assert_eq!(members_settled(&tokens, 1), [ours.id() as i32]);
        sweep(&tokens);
        assert!(ours.wait().unwrap().signal().is_some(), "it was ended");
        assert!(members(&tokens).is_empty());
        other.kill().unwrap();
        other.wait().unwrap();
    }

    #[test]
    fn a_child_that_dropped_the_token_still_counts_while_its_parent_does() {
        let tokens = [token()];
        // Like an agent running a tool shell that no longer shows the token.
        let child = "import os, subprocess; env = dict(os.environ); env.pop('HC_SCOPE'); subprocess.run(['sleep', '30'], env=env)";
        let mut parent = python(child).env(VAR, &tokens[0]).spawn().unwrap();
        assert_eq!(members_settled(&tokens, 2).len(), 2, "the shell and its sleep");
        sweep(&tokens);
        parent.wait().unwrap();
        assert!(members(&tokens).is_empty());
    }

    #[test]
    fn a_process_older_than_its_token_is_never_read() {
        // A process older than the token cannot carry it, so its environment need not be read.
        let later = now() * 2 + 1_000_000;
        let (tokens, found) = ([format!("{later}-{}", crate::platform::random_hex())], [token()]);
        let mut sleeper = python("import time; time.sleep(30)").env(VAR, &tokens[0]).spawn().unwrap();
        let mut seen = python("import time; time.sleep(30)").env(VAR, &found[0]).spawn().unwrap();
        assert_eq!(members_settled(&found, 1), [seen.id() as i32], "a fresh token finds its own");
        assert!(members(&tokens).is_empty(), "a token from far ahead is newer than every process");
        for child in [&mut sleeper, &mut seen] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    /// Linux: the same inside a time namespace whose clocks are shifted apart.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_fresh_token_finds_its_own_in_a_shifted_time_namespace() {
        let me = std::env::current_exe().unwrap();
        let test = "sweep::tests::a_fresh_token_finds_its_own";
        let inside = std::process::Command::new("unshare")
            .args(["-UrTf", "--monotonic", "1000000000", "--boottime", "0"])
            .arg(me)
            .args([test, "--exact", "--ignored"])
            .output();
        match inside {
            Ok(out) if out.status.success() => {}
            // No user or time namespaces here (an old kernel, a locked-down container).
            Ok(out) if String::from_utf8_lossy(&out.stderr).contains("unshare") => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            other => panic!("inside: {other:?}"),
        }
    }

    #[test]
    #[ignore = "run by the test above, inside a time namespace"]
    fn a_fresh_token_finds_its_own() {
        let tokens = [token()];
        let mut ours = python("import time; time.sleep(30)").env(VAR, &tokens[0]).spawn().unwrap();
        assert_eq!(members_settled(&tokens, 1), [ours.id() as i32]);
        ours.kill().unwrap();
        ours.wait().unwrap();
    }

    /// `members` once it finds `n` (5 s at most): macOS's python3 is a stub
    /// that starts the real one, and can't be read in between.
    fn members_settled(tokens: &[String], n: usize) -> Vec<i32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let found = members(tokens);
            if found.len() == n || Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    use std::os::unix::process::ExitStatusExt;

    fn python(code: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("python3");
        cmd.args(["-c", code]);
        cmd
    }
}
