#![allow(dead_code)]

pub mod framed;

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_herdr-core");

/// A program that prints every byte it receives as hex, one at a time
/// (GNU and BSD alike; plain od waits for 16 bytes).
pub const BYTES: &str = "stty raw -echo; while :; do dd bs=1 count=1 2>/dev/null | od -An -tx1; done";

/// An isolated core: its own HC_SESSION and XDG dirs under /var/tmp.
pub struct Session {
    pub root: PathBuf,
    pub env: Vec<(String, String)>,
    pub bin: PathBuf,
}

impl Session {
    pub fn new() -> Session {
        static N: AtomicU32 = AtomicU32::new(0);
        static WATCHDOG: std::sync::Once = std::sync::Once::new();
        // A hung test should fail in seconds, not stall the suite.
        WATCHDOG.call_once(|| {
            std::thread::spawn(|| {
                // 30 s here; CI's slower runners set HC_TEST_WATCHDOG_S.
                let limit = std::env::var("HC_TEST_WATCHDOG_S").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
                std::thread::sleep(Duration::from_secs(limit));
                eprintln!("test binary still running after {limit} s: cleaning up and aborting");
                // Abort skips every Drop, so end this binary's sessions here first.
                let ours = format!("/var/tmp/hc-test-{}-", std::process::id());
                for pid in procs_with(&ours) {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                for dir in std::fs::read_dir("/var/tmp").into_iter().flatten().flatten() {
                    if dir.path().display().to_string().starts_with(&ours) {
                        let _ = std::fs::remove_dir_all(dir.path());
                    }
                }
                std::process::abort();
            });
        });
        let root =
            PathBuf::from(format!("/var/tmp/hc-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["run", "state", "config"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        // Sends don't wait for fake agents to settle unless a test asks.
        Session { root, env: vec![("HC_SETTLE_MS".into(), "0".into())], bin: BIN.into() }
    }

    pub fn cmd(&self) -> Command {
        self.program(&self.bin)
    }

    /// Any program with this session's environment (its HOME, XDG dirs, env).
    pub fn program(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.root)
            .env("HC_SESSION", "test")
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .envs(self.env.iter().cloned());
        cmd
    }

    pub fn hc(&self, args: &[&str]) -> Output {
        self.try_hc(args).unwrap()
    }

    /// Runs `hc`, retrying while a binary copied by a test is briefly held open by
    /// a process another test thread is starting ("text file busy").
    fn try_hc(&self, args: &[&str]) -> std::io::Result<Output> {
        let mut tries = 0;
        loop {
            match self.cmd().args(args).stdin(Stdio::null()).output() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => return result,
            }
        }
    }

    /// Runs `hc` with `input` piped into it.
    pub fn hc_input(&self, args: &[&str], input: &[u8]) -> Output {
        let mut cmd = self.cmd();
        cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let feed = std::thread::spawn(move || drop(stdin.write_all(&input)));
        let out = child.wait_with_output().unwrap();
        feed.join().unwrap();
        out
    }

    /// Runs a command that must succeed and returns its JSON stdout.
    pub fn json(&self, args: &[&str]) -> Value {
        let out = self.hc(&[&["--json"], args].concat());
        assert!(out.status.success(), "hc {args:?} failed: {}\n{}", out.status, String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("hc {args:?}: bad json ({e}): {}", String::from_utf8_lossy(&out.stdout)))
    }

    /// Runs a command that must fail; returns (exit code, JSON error from stderr).
    pub fn err(&self, args: &[&str]) -> (i32, Value) {
        let out = self.hc(args);
        assert!(!out.status.success(), "hc {args:?} unexpectedly succeeded");
        let err = serde_json::from_slice(&out.stderr)
            .unwrap_or_else(|e| panic!("hc {args:?}: bad json error ({e}): {}", String::from_utf8_lossy(&out.stderr)));
        (out.status.code().unwrap_or(-1), err)
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("run/herdr-core/test.sock")
    }

    pub fn connect_ok(&self) -> bool {
        UnixStream::connect(self.socket()).is_ok()
    }

    /// A raw plain-mode connection to an already running core.
    pub fn connect(&self) -> Conn {
        let stream = UnixStream::connect(self.socket()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Conn { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Bring the core up (it adopts the holder's terminals), then stop everything.
        // Never panic here: a panic during cleanup aborts every other test's cleanup.
        let _ = self.try_hc(&["server", "status"]);
        let _ = self.try_hc(&["server", "stop"]);
        // Anything still running from this session is a bug; never leave it behind.
        let mut leaks = Vec::new();
        let ours = format!("XDG_STATE_HOME={}\0", self.root.join("state").display());
        // A process in the act of exiting is not a leak: give stragglers a moment.
        let start = Instant::now();
        while !procs_with(&ours).is_empty() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(50));
        }
        for pid in procs_with(&ours) {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            let test = std::thread::current().name().unwrap_or("?").to_string();
            let note = format!("LEAK in {test}: pid {pid} ({}) still running after server stop\n", cmdline.trim());
            eprint!("{note}");
            let log = std::fs::OpenOptions::new().create(true).append(true).open("/var/tmp/hc-test-leaks.log");
            let _ = log.and_then(|mut f| f.write_all(note.as_bytes()));
            unsafe { libc::kill(pid, libc::SIGKILL) };
            leaks.push(note);
        }
        let _ = std::fs::remove_dir_all(&self.root);
        // Cleaned up first; then the test fails (unless it is failing already:
        // a second panic would abort every other test's cleanup).
        if !leaks.is_empty() && !std::thread::panicking() {
            panic!("processes outlived server stop:\n{}", leaks.concat());
        }
    }
}

/// Reads plain-mode messages until one matches.
pub fn next_event(conn: &mut Conn, mut want: impl FnMut(&Value) -> bool) -> Value {
    loop {
        let mut line = String::new();
        conn.reader.read_line(&mut line).expect("a message");
        let msg: Value = serde_json::from_str(&line).unwrap();
        if want(&msg) {
            return msg;
        }
    }
}

pub struct Conn {
    pub reader: BufReader<UnixStream>,
    pub writer: UnixStream,
}

impl Conn {
    pub fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let req = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.writer, "{req}").unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

/// Processes (other than us) whose environment mentions `needle`. Linux
/// only: macOS shows no other process's environment (`ps -E` prints none,
/// checked on macOS 27), so there the leak check finds nothing; the ghost
/// tests are what cover leftovers on macOS.
fn procs_with(needle: &str) -> Vec<i32> {
    if cfg!(target_os = "macos") {
        return Vec::new();
    }
    let me = std::process::id() as i32;
    std::fs::read_dir("/proc")
        .expect("the leak check needs /proc")
        .flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_string_lossy().parse().ok()?;
            let env = std::fs::read(e.path().join("environ")).ok()?;
            let hit = env.windows(needle.len()).any(|w| w == needle.as_bytes());
            (hit && pid != me).then_some(pid)
        })
        .collect()
}

/// The env that points `local`'s core at a fake `ssh`: a target containing a host's name
/// runs the remote command as that session's machine would (its own HOME and
/// XDG dirs, `herdr-core` on PATH, nothing else from here); `nohc` is a
/// machine without hc; anything else can't be reached. `otp-` targets need a
/// login first (`<target> exit` asks for code 123456; ssh's master then lets
/// later calls in); `noisy-` targets print a banner before anything. While
/// `<local root>/down` exists, nothing can be reached. HC_TEST_SSH_DELAY
/// (seconds) makes every login take that long, like a real one.
pub fn fake_ssh(local: &Session, hosts: &[(&str, &Session)]) -> (String, String) {
    let mut cases = String::new();
    for (host, s) in hosts {
        let r = s.root.display();
        // `<root>/fakebin` first: a test can give a machine its own `uname`.
        let bin_dir = format!("{r}/fakebin:{}", s.bin.parent().unwrap().display());
        cases.push_str(&format!(
            "  *{host}*) exec env -i HOME={r} PATH={bin_dir}:/usr/bin:/bin HC_SESSION=test HC_SETTLE_MS=0 \
             XDG_RUNTIME_DIR={r}/run XDG_STATE_HOME={r}/state XDG_CONFIG_HOME={r}/config sh -c \"$*\" ;;\n"
        ));
    }
    let script = format!(
        "#!/bin/sh\ntarget=$1; shift\nok={root}/otp-ok\nsleep ${{HC_TEST_SSH_DELAY:-0}}\n\
         [ -e {root}/down ] && {{ echo \"ssh: connect to host $target port 22: Connection refused\" >&2; exit 255; }}\ncase \"$target\" in otp-*)\n  \
         if [ \"$*\" = exit ]; then printf 'Verification code: '; read c; [ \"$c\" = 123456 ] || exit 255; touch $ok; exit 0; fi\n  \
         [ -e $ok ] || {{ echo \"$target: Permission denied (keyboard-interactive).\" >&2; exit 255; }} ;;\nesac\n\
         case \"$target\" in noisy-*) echo 'Welcome to rohan!' ;; esac\ncase \"$target\" in\n{cases}  *nohc*) exec env -i HOME=/nonexistent PATH=/usr/bin:/bin sh -c \"$*\" ;;\n  \
         *) echo \"ssh: Could not resolve hostname $target: Name or service not known\" >&2; exit 255 ;;\nesac\n",
        root = local.root.display()
    );
    let path = local.root.join("fake-ssh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    ("HC_SSH".into(), path.display().to_string())
}

/// On a runner too loaded to keep fixed timings (CI): timing-only checks skip.
pub fn slow_runner() -> bool {
    std::env::var_os("HC_TEST_SLOW_RUNNER").is_some()
}

/// `hc run --bg <args>`: started, not waited on; returns the address.
pub fn run(s: &Session, args: &[&str]) -> String {
    s.json(&[&["run", "--bg"], args].concat())["address"].as_str().unwrap().to_string()
}

pub fn get(s: &Session, t: &str) -> Value {
    s.json(&["get", t])
}

/// Its text, with up to 100 lines of history.
pub fn text(s: &Session, t: &str) -> String {
    s.json(&["read", t, "--lines", "100"])["text"].as_str().unwrap().to_string()
}

/// A shell script with `r <report>` to send OSC 7501 and `w <name>` to wait for a file.
pub fn script(s: &Session, body: &str) -> String {
    format!(
        "r(){{ printf '\\033]7501;%s\\033\\\\' \"$1\"; }}; w(){{ while [ ! -e {}/$1 ]; do sleep 0.02; done; }}; {body}",
        s.root.display()
    )
}

/// Creates the file a script's `w <name>` waits for.
pub fn touch(s: &Session, name: &str) {
    std::fs::write(s.root.join(name), "").unwrap();
}

/// Writes hc to `to` as another build, the way an update brings one: on Linux
/// a copy is the same build, so the copy gets another linker build id.
pub fn write_new_build(to: &std::path::Path) {
    let mut exe = std::fs::read(BIN).unwrap();
    if let Some(note) = exe.windows(16).position(|w| w[12..] == *b"GNU\0" && w[8..12] == [3, 0, 0, 0]) {
        exe[note + 16] ^= 0xff;
    }
    std::fs::write(to, exe).unwrap();
    std::fs::set_permissions(to, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

pub fn wait_until(what: &str, ok: impl FnMut() -> bool) {
    wait_until_for(what, Duration::from_secs(10), ok);
}

pub fn wait_until_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn pid_alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
