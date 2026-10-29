//! exec: one program without a terminal, exactly what it wrote, here or on a
//! machine; nothing it started outlives it.

#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::json;
use std::io::Write;
use std::time::{Duration, Instant};

/// Tabs, colors, a NUL and invalid UTF-8: what a screen would mangle.
const TABLE: &str = r#"BEGIN { for (i = 1; i <= 30000; i++) printf "%d\t\033[31mx\033[0m\n", i }"#;

fn table() -> Vec<u8> {
    (1..=30000).map(|i| format!("{i}\t\x1b[31mx\x1b[0m\n")).collect::<String>().into_bytes()
}

/// Starts a process that leaves its session and ignores hangup and terminate
/// (only the scope token finds it), and waits until it wrote its pid to `file`.
fn stubborn(s: &Session, file: &str) -> String {
    let file = s.root.join(file).display().to_string();
    format!(
        "python3 -c 'import os, signal, time; os.setsid(); signal.signal(signal.SIGHUP, signal.SIG_IGN); \
         signal.signal(signal.SIGTERM, signal.SIG_IGN); open(\"{file}.tmp\", \"w\").write(str(os.getpid())); \
         os.rename(\"{file}.tmp\", \"{file}\"); time.sleep(300)' & while [ ! -e {file} ]; do sleep 0.02; done;"
    )
}

fn pid_in(s: &Session, file: &str) -> i64 {
    wait_until(&format!("its pid file {file}"), || {
        std::fs::read_to_string(s.root.join(file)).is_ok_and(|p| !p.is_empty())
    });
    std::fs::read_to_string(s.root.join(file)).unwrap().trim().parse().unwrap()
}

fn running(pid: i64) -> bool {
    #[cfg(target_os = "openbsd")]
    return herdr_core::platform::processes().iter().any(|p| i64::from(p.0) == pid);
    #[cfg(not(target_os = "openbsd"))]
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
        Err(_) => cfg!(target_os = "macos") && pid_alive(pid),
    }
}

fn gone(pid: i64) {
    wait_until("what it started ended", || !running(pid));
}

#[test]
fn exec_returns_exactly_what_the_program_wrote_here_and_on_a_machine() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    for on in [&[][..], &["--on", "rohan"][..]] {
        let script = format!("awk '{TABLE}'; printf 'oops\\0\\377' >&2; exit 3");
        let out = s.hc(&[&["exec"], on, &["sh", "-c", &script]].concat());
        assert!(out.stdout == table(), "{on:?}: every line, tabs and escapes kept");
        assert_eq!((out.stderr.as_slice(), out.status.code()), (&b"oops\0\xff"[..], Some(3)), "{on:?}");
    }

    // One string with shell signs is a shell line, run by that machine's shell.
    let out = s.hc(&["exec", "--on", "rohan", "--env", "WHO=rohan", "echo $WHO | tr r R; pwd"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("Rohan\n{}\n", rohan.root.display()), "its home");
    let here = s.json(&["exec", "--cwd", "/", "pwd"]);
    assert_eq!((here["code"].clone(), here["stdout"].clone()), (json!(0), json!("Lwo=")), "{here}");
    assert_eq!((here["timed_out"].clone(), here["truncated"].clone()), (json!(false), json!(false)));
    assert_eq!(s.err(&["--json", "exec", "no-such-program-here"]).1["error"]["kind"], "spawn_failed");
    assert!(s.json(&["ls", "--all"])["terminals"].as_array().unwrap().is_empty(), "no terminal, no record");

    // A machine calling back can't run things here.
    let me = s.json(&["server", "status"])["core"]["id"].as_str().unwrap().to_string();
    assert_eq!(rohan.err(&["--json", "exec", "--on", &me, "true"]).1["error"]["kind"], "machine_unknown");
}

#[test]
fn a_relative_folder_there_starts_in_its_home_never_where_its_core_runs() {
    // The remote core's cwd differs from its HOME.
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    std::fs::create_dir_all(rohan.root.join("app")).unwrap();
    std::fs::write(rohan.root.join("app/x.txt"), "x\n").unwrap();
    let want = format!("{}/app\n", rohan.root.display());
    let exec = s.hc(&["exec", "--on", "rohan", "--cwd", "app", "sh", "-c", "echo $PWD"]);
    assert_eq!(String::from_utf8_lossy(&exec.stdout), want, "exec");
    let run = s.hc(&["run", "--on", "rohan", "--cwd", "app", "sh", "-c", "echo $PWD"]);
    assert!(String::from_utf8_lossy(&run.stdout).contains(want.trim()), "run");
    let read = s.hc(&["file", "read", "--on", "rohan", "--cwd", "app", "x.txt"]);
    assert_eq!(String::from_utf8_lossy(&read.stdout), "x\n", "file");
}

#[test]
fn exec_feeds_what_is_piped_with_stdin_flag_and_nothing_without() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    // Past one argument's limit, binary, and echoed while it comes (no deadlock).
    let input: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    for on in [&[][..], &["--on", "rohan"][..]] {
        let out = s.hc_input(&[&["exec", "-i"], on, &["cat"]].concat(), &input);
        assert!(out.stdout == input && out.status.success(), "{on:?}: {}", String::from_utf8_lossy(&out.stderr));
        let out = s.hc_input(&[&["exec"], on, &["wc", "-c"]].concat(), &input);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0", "{on:?}: not read without -i");
    }
}

#[test]
fn exec_ends_everything_it_started_when_it_exits_or_times_out() {
    let s = Session::new();
    let start = Instant::now();
    let out = s.hc(&["exec", "sh", "-c", &format!("{} echo ok", stubborn(&s, "left"))]);
    assert_eq!((out.stdout.as_slice(), out.status.code()), (&b"ok\n"[..], Some(0)));
    gone(pid_in(&s, "left"));
    assert!(start.elapsed() < Duration::from_secs(8), "a helper holding its output doesn't keep it waiting");

    let out =
        s.hc(&["exec", "--timeout", "1", "sh", "-c", &format!("{} echo started; sleep 100", stubborn(&s, "slow"))]);
    assert_eq!((out.stdout.as_slice(), out.status.code()), (&b"started\n"[..], Some(124)), "what came so far");
    gone(pid_in(&s, "slow"));
}

#[test]
fn exec_keeps_running_past_the_output_cap_and_says_it_cut() {
    let s = Session::new();
    let r = s.json(&[
        "exec",
        "python3",
        "-c",
        "import sys; sys.stdout.buffer.write(bytes(9437184)); sys.stderr.write('done\\n')",
    ]);
    assert_eq!((r["code"].clone(), r["truncated"].clone()), (json!(0), json!(true)), "not killed for its output");
    assert_eq!(r["stdout"].as_str().unwrap().len(), (8 << 20) / 3 * 4 + 4, "the first 8 MB");
    assert_eq!(r["stderr"], "ZG9uZQo=", "errors aren't crowded out");
}

/// Starts an exec on a raw connection and returns it unanswered.
fn exec_on_conn(s: &Session, params: serde_json::Value) -> Conn {
    let mut conn = s.connect();
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "machine.exec", "params": params});
    writeln!(conn.writer, "{req}").unwrap();
    conn
}

#[test]
fn exec_ends_when_its_caller_goes_away_here_or_across_a_line() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    for (on, file) in [(None, "here"), (Some("rohan"), "there")] {
        let script = format!("{} sleep 100", stubborn(&s, file));
        let conn = exec_on_conn(&s, json!({"on": on, "argv": ["sh", "-c", script]}));
        let pid = pid_in(&s, file);
        drop(conn);
        gone(pid);
    }

    // More sent after it doesn't hide that the caller left.
    let mut conn = exec_on_conn(&s, json!({"argv": ["sh", "-c", format!("{} sleep 100", stubborn(&s, "ahead"))]}));
    writeln!(conn.writer).unwrap();
    let pid = pid_in(&s, "ahead");
    drop(conn);
    gone(pid);

    // On a framed connection, every exec ends with it, even ones sent without an id.
    let mut framed = common::framed::Framed::open(&s);
    for file in ["first", "second"] {
        let script = format!("{} sleep 100", stubborn(&s, file));
        let note = json!({"jsonrpc": "2.0", "method": "machine.exec", "params": {"argv": ["sh", "-c", script]}});
        framed.send(0, herdr_core::wire::DATA, note.to_string().as_bytes());
    }
    let pids = [pid_in(&s, "first"), pid_in(&s, "second")];
    drop(framed);
    pids.into_iter().for_each(gone);
}

#[test]
fn a_client_that_stops_reading_its_replies_is_disconnected_not_kept_in_memory() {
    use std::io::Read;
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut stream = std::os::unix::net::UnixStream::connect(s.socket()).unwrap();
    writeln!(stream, r#"{{"jsonrpc":"2.0","id":0,"method":"hello","params":{{"framed":true}}}}"#).unwrap();
    for id in 1..=8 {
        let call = json!({"jsonrpc": "2.0", "id": id, "method": "machine.exec",
            "params": {"argv": ["python3", "-c", "import sys; sys.stdout.buffer.write(bytes(8388608))"]}});
        stream.write_all(&herdr_core::wire::encode(0, herdr_core::wire::DATA, call.to_string().as_bytes())).unwrap();
    }
    // 8 replies of ~11 MB, none read: past the cap it hangs up on us.
    std::thread::sleep(Duration::from_secs(3));
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut got = Vec::new();
    stream.read_to_end(&mut got).expect("hung up on, not left waiting");
    assert!(got.len() < 64 << 20, "only what was in flight came");
    assert!(s.json(&["server", "status"])["pid"].is_u64(), "the core is fine");
}

#[test]
fn a_stop_right_after_an_exec_was_given_up_waits_for_its_cleanup() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let conn = exec_on_conn(&s, json!({"argv": ["sh", "-c", format!("{} sleep 100", stubborn(&s, "late"))]}));
    let pid = pid_in(&s, "late");
    drop(conn);
    s.json(&["server", "stop"]);
    assert!(!running(pid), "ended before stop returned");
}

#[test]
fn exec_left_by_a_crashed_core_ends_at_its_next_start() {
    let s = Session::new();
    let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    let _conn = exec_on_conn(&s, json!({"argv": ["sh", "-c", format!("{} sleep 100", stubborn(&s, "orphan"))]}));
    let pid = pid_in(&s, "orphan");
    unsafe { libc::kill(core as i32, libc::SIGKILL) };
    wait_until("the core is gone", || !pid_alive(core));
    s.json(&["server", "status"]);
    gone(pid);
}
