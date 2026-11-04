//! A one-shot launchd job gives the core its own coalition. setsid alone
//! leaves it in the app's coalition, which macOS can kill when the app quits.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DETACH: &str = "HC_LAUNCHD_DETACH";
const SENTINEL: &str = "--hc-launchd";
const DIRECT: &str = "HC_LAUNCHD_DIRECT";
const LAUNCHCTL: &str = "/bin/launchctl";
const PREFIX: &str = "dev.herdr.core.";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(serde::Serialize, serde::Deserialize)]
struct Inherited {
    env: Vec<(Vec<u8>, Vec<u8>)>,
    limits: Vec<(libc::c_int, libc::rlim_t, libc::rlim_t)>,
}

impl Inherited {
    fn capture(cmd: &Command) -> io::Result<Self> {
        let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
        for (key, value) in cmd.get_envs() {
            match value {
                Some(value) => {
                    env.insert(key.to_owned(), value.to_owned());
                }
                None => {
                    env.remove(key);
                }
            }
        }
        env.remove(OsStr::new(DETACH));
        env.remove(OsStr::new(DIRECT));
        env.insert("XPC_SERVICE_NAME".into(), "0".into());
        let mut limits = Vec::new();
        // Darwin's RLIM_NLIMITS is not stable across OS versions. Ask the
        // running kernel for every slot rather than baking in a header's count.
        for resource in 0.. {
            let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if unsafe { libc::getrlimit(resource, &mut limit) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINVAL) {
                    break;
                }
                return Err(error);
            }
            limits.push((resource, limit.rlim_cur, limit.rlim_max));
        }
        Ok(Self { env: env.into_iter().map(|(k, v)| (k.into_vec(), v.into_vec())).collect(), limits })
    }

    /// Runs only in the newly forked core, before it creates any threads.
    fn restore(self) -> io::Result<()> {
        for (resource, soft, hard) in self.limits {
            let mut current = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // Darwin can clamp NPROC even when setting its inherited value.
            // Do not rewrite a limit that launchd already preserved exactly.
            if (current.rlim_cur, current.rlim_max) == (soft, hard) {
                continue;
            }
            let limit = libc::rlimit { rlim_cur: soft, rlim_max: hard };
            if unsafe { libc::setrlimit(resource, &limit) } != 0 {
                return Err(io::Error::other(format!(
                    "cannot restore resource limit {resource}: {}",
                    io::Error::last_os_error()
                )));
            }
            if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if (current.rlim_cur, current.rlim_max) != (soft, hard) {
                return Err(io::Error::other(format!("resource limit {resource} was clamped instead of inherited")));
            }
        }
        unsafe {
            for (key, _) in std::env::vars_os() {
                std::env::remove_var(key);
            }
            for (key, value) in self.env {
                std::env::set_var(OsString::from_vec(key), OsString::from_vec(value));
            }
        }
        Ok(())
    }
}

/// Detach only the internal launchd sentinel before any CLI/runtime setup.
pub fn startup() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(SENTINEL)) {
        return false;
    }
    let result = if let Some(socket) = std::env::var_os(DETACH) {
        detach(Path::new(&socket))
    } else if let Some(socket) = std::env::args_os().nth(2) {
        receive(Path::new(&socket))
    } else {
        return false;
    };
    if let Err(error) = result {
        eprintln!("hc: launchd detach failed: {error}");
        std::process::exit(1);
    }
    true
}

/// Called only after parsing a real Cmd::Serve, so help/errors never launch.
/// Also catches the first fixed core started directly by an OLD updater.
pub fn prepare_core() {
    let direct = std::env::var(DIRECT).ok().and_then(|pid| pid.parse::<u32>().ok()) == Some(super::parent_pid());
    // Scope the bypass to its immediate launcher: an older binary may ignore
    // it and pass it on during a later update. That must not skip the escape.
    unsafe {
        std::env::remove_var(DIRECT);
    }
    if direct || std::env::args_os().nth(1).as_deref() == Some(OsStr::new(SENTINEL)) || ssh() {
        return;
    }
    let result = (|| {
        let paths = crate::paths::Paths::from_env().map_err(|e| io::Error::other(e.message))?;
        paths.create_dirs()?;
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("serve");
        launch(&cmd, &paths.state.join("core.log"))
    })();
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            if let Ok(paths) = crate::paths::Paths::from_env() {
                fallback(&paths.state.join("core.log"), &error);
            } else {
                eprintln!("hc: cannot move core startup to launchd: {error}");
            }
        }
    }
}

fn ssh() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
}

/// Only `serve` uses this path. Holders and programs inherit its coalition.
/// SSH is already outside the app, and must retain its existing lifetime.
pub fn spawn(cmd: &mut Command, log: &Path) -> bool {
    if ssh() {
        return false;
    }
    match launch(cmd, log) {
        Ok(()) => true,
        Err(error) => {
            fallback(log, &error);
            // Do not retry in a directly spawned fixed binary after a failure.
            cmd.env(DIRECT, std::process::id().to_string());
            false
        }
    }
}

fn fallback(log: &Path, error: &io::Error) {
    let reason = format!("hc: launchd startup failed; starting directly (may end with the launching app): {error}");
    eprintln!("{reason}");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(file, "{reason}");
    }
}

fn launch(cmd: &Command, log: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let inherited = Inherited::capture(cmd)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?;
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    remove_stale(&domain, now);
    remove_stale_dirs(Path::new("/tmp"), now);
    let dir =
        PathBuf::from(format!("/tmp/hc-launchd-{}-{}-{}", std::process::id(), now.as_nanos(), super::random_hex()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let mut job =
        Job { dir, target: format!("{domain}/{PREFIX}{}.{}", std::process::id(), now.as_nanos()), loaded: false };
    let socket = job.dir.join("ready");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let label = job.target.rsplit('/').next().unwrap();
    let mask = unsafe {
        let mask = libc::umask(0o077);
        libc::umask(mask);
        mask
    };
    // Create the log ourselves: same ownership, mask and append behavior as
    // direct startup, even before launchd has run the job for the first time.
    std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let contents = plist(cmd, log, label, &socket, mask)?;
    let file = job.dir.join("job.plist");
    std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&file)?.write_all(contents.as_bytes())?;
    let bootstrapped = launchctl(&[OsStr::new("bootstrap"), OsStr::new(&domain), file.as_os_str()]);
    let _ = std::fs::remove_file(file);
    if let Err(error) = bootstrapped {
        // A failed bootstrap normally creates no job; clean a partial one
        // quietly without hiding the original error or logging a bogus leak.
        let _ = launchctl(&[OsStr::new("bootout"), OsStr::new(&job.target)]);
        return Err(error);
    }
    job.loaded = true;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut ready, _)) => {
                // Darwin inherits O_NONBLOCK from the listener on accept.
                ready.set_nonblocking(false)?;
                ready.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
                ready.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
                // The child has forked and observed its parent exit. Remove
                // the one-shot job before transferring state and letting it serve.
                job.remove()?;
                serde_json::to_writer(&mut ready, &inherited)?;
                ready.shutdown(std::net::Shutdown::Write)?;
                let mut ack = [0];
                ready.read_exact(&mut ack)?;
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "launchd process did not complete the detach handshake within 10 s (it may be an older binary); removing the job before direct startup",
                ));
            }
            Err(e) => return Err(e),
        }
    }
}

fn detach(socket: &Path) -> io::Result<()> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg(SENTINEL).arg(socket).env_remove(DETACH);
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => unsafe { libc::_exit(0) },
    }
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    // libSystem can initialize libnotify before main. fork disables it in the
    // child; exec gives the detached core fresh libnotify/libdispatch state.
    super::macos_responsibility::exec(cmd)
}

fn receive(socket: &Path) -> io::Result<()> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while unsafe { libc::getppid() } != 1 {
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "launcher did not exit"));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut ready = UnixStream::connect(socket)?;
    ready.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let inherited: Inherited = serde_json::from_reader(&mut ready)?;
    inherited.restore()?;
    ready.write_all(&[1])
}

struct Job {
    dir: PathBuf,
    target: String,
    loaded: bool,
}
impl Job {
    fn remove(&mut self) -> io::Result<()> {
        if self.loaded {
            launchctl(&[OsStr::new("bootout"), OsStr::new(&self.target)])?;
            self.loaded = false;
        }
        Ok(())
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("hc: cannot remove launchd job {}: {error}", self.target);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn launchctl(args: &[&OsStr]) -> io::Result<String> {
    let out = Command::new(LAUNCHCTL).args(args).stdin(Stdio::null()).output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(io::Error::other(format!(
            "launchctl {}: {} ({})",
            args[0].to_string_lossy(),
            String::from_utf8_lossy(&out.stderr).trim(),
            out.status
        )))
    }
}

fn remove_stale(domain: &str, now: Duration) {
    let Ok(list) = launchctl(&[OsStr::new("list")]) else { return };
    for label in stale_labels(&list, now) {
        let target = format!("{domain}/{label}");
        if let Err(error) = launchctl(&[OsStr::new("bootout"), OsStr::new(&target)]) {
            eprintln!("hc: cannot remove finished launchd job {target}: {error}");
        }
    }
}

fn remove_stale_dirs(root: &Path, now: Duration) {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
            continue;
        }
        let name = entry.file_name();
        let Some(attempt) = name.to_str().and_then(|name| name.strip_prefix("hc-launchd-")) else { continue };
        let mut fields = attempt.splitn(3, '-');
        let Some(pid) = fields.next().and_then(|p| p.parse::<libc::pid_t>().ok()).filter(|pid| *pid > 0) else {
            continue;
        };
        let Some(stamp) = fields.next().and_then(|s| s.parse::<u128>().ok()) else { continue };
        if !fields.next().is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            || now.as_nanos().saturating_sub(stamp) < Duration::from_secs(60).as_nanos()
        {
            continue;
        }
        // ESRCH only: a live/reused pid, or an inaccessible process, is never
        // evidence that this launch attempt has ended.
        if unsafe { libc::kill(pid, 0) } == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(&path) {
            eprintln!("hc: cannot remove abandoned launchd directory {}: {error}", path.display());
        }
    }
}

fn stale_labels(list: &str, now: Duration) -> Vec<&str> {
    list.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != "-" {
                return None;
            }
            let label = fields.nth(1)?;
            let stamp = label.strip_prefix(PREFIX)?.rsplit_once('.')?.1.parse::<u128>().ok()?;
            (now.as_nanos().saturating_sub(stamp) >= Duration::from_secs(60).as_nanos()).then_some(label)
        })
        .collect()
}

fn plist(cmd: &Command, log: &Path, label: &str, socket: &Path, mask: libc::mode_t) -> io::Result<String> {
    let mut result = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>");
    let pair = |key: &str, value: &OsStr| -> io::Result<String> {
        Ok(format!("<key>{key}</key><string>{}</string>", xml(value)?))
    };
    result.push_str(&pair("Label", OsStr::new(label))?);
    result.push_str("<key>ProgramArguments</key><array>");
    // An older target must reject the job, not start `serve` with launchd's
    // default environment before it knows how to receive the caller's state.
    for arg in [cmd.get_program(), OsStr::new(SENTINEL)] {
        result.push_str(&format!("<string>{}</string>", xml(arg)?));
    }
    result.push_str("</array><key>EnvironmentVariables</key><dict>");
    result.push_str(&pair(DETACH, socket.as_os_str())?);
    result.push_str("</dict>");
    let cwd = cmd.get_current_dir().map(Path::to_path_buf).map_or_else(std::env::current_dir, Ok)?;
    for (key, value) in [
        ("WorkingDirectory", cwd.as_os_str()),
        ("StandardInPath", OsStr::new("/dev/null")),
        ("StandardOutPath", OsStr::new("/dev/null")),
        ("StandardErrorPath", log.as_os_str()),
        ("ProcessType", OsStr::new("Interactive")),
    ] {
        result.push_str(&pair(key, value)?);
    }
    result.push_str(&format!("<key>RunAtLoad</key><true/><key>KeepAlive</key><false/><key>AbandonProcessGroup</key><true/><key>Umask</key><integer>{mask}</integer></dict></plist>"));
    Ok(result)
}

/// Paths/arguments still need exact XML representation. Environment bytes
/// travel over the private socket, so shell escape/color variables work too.
fn xml(value: &OsStr) -> io::Result<String> {
    let text =
        value.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "launchd value is not UTF-8"))?;
    let mut result = String::new();
    for c in text.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\t' | '\n' => result.push(c),
            c if c < ' ' || c == '\u{fffe}' || c == '\u{ffff}' => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "launchd value contains an XML control character",
                ));
            }
            c => result.push(c),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn job_preserves_the_command_and_never_relaunches() {
        let mut cmd = Command::new("/a<&>/hc");
        cmd.arg("serve").env("LESS_TERMCAP_md", "\x1b[1m\r").env_remove("HOME");
        let job = plist(&cmd, Path::new("/state/core.log"), "test", Path::new("/private/job"), 0o027).unwrap();
        assert!(job.contains("<string>/a&lt;&amp;&gt;/hc</string><string>--hc-launchd</string>"));
        assert!(!job.contains("<string>serve</string>"));
        assert!(!job.contains("LESS_TERMCAP_md"));
        assert!(!job.contains("<key>HOME</key>"));
        assert!(job.contains("<key>KeepAlive</key><false/>"));
        assert!(job.contains("<key>AbandonProcessGroup</key><true/>"));
        assert!(job.contains("<key>Umask</key><integer>23</integer>"));
        assert!(job.contains("<key>StandardErrorPath</key><string>/state/core.log</string>"));
    }

    #[test]
    fn environment_transfer_preserves_arbitrary_unix_bytes_and_removals() {
        let mut cmd = Command::new("hc");
        cmd.env("LESS_TERMCAP_md", OsStr::from_bytes(b"\x1b[1m\r\xff"))
            .env_remove("HOME")
            .env(DETACH, "old")
            .env(DIRECT, "1");
        let captured = Inherited::capture(&cmd).unwrap();
        let received: Inherited = serde_json::from_slice(&serde_json::to_vec(&captured).unwrap()).unwrap();
        assert_eq!(received.limits, captured.limits);
        assert!(received.limits.len() > libc::RLIMIT_NOFILE as usize);
        let env: BTreeMap<_, _> = received.env.into_iter().collect();
        assert_eq!(env.get(b"LESS_TERMCAP_md".as_slice()), Some(&b"\x1b[1m\r\xff".to_vec()));
        for name in [b"HOME".as_slice(), DETACH.as_bytes(), DIRECT.as_bytes()] {
            assert!(!env.contains_key(name));
        }
        assert_eq!(env.get(b"XPC_SERVICE_NAME".as_slice()), Some(&b"0".to_vec()));
    }

    #[test]
    fn only_old_finished_core_jobs_are_removed() {
        let now = Duration::from_secs(1000);
        let old = Duration::from_secs(900).as_nanos();
        let recent = Duration::from_secs(990).as_nanos();
        let label = format!("{PREFIX}1.{old}");
        let list = format!(
            "PID\tStatus\tLabel\n-\t0\t{label}\n42\t0\t{PREFIX}2.{old}\n-\t0\t{PREFIX}3.{recent}\n-\t0\tother.4.{old}\n"
        );
        assert_eq!(stale_labels(&list, now), vec![label.as_str()]);
    }

    #[test]
    fn stale_directories_need_a_dead_starter_and_never_follow_symlinks() {
        let dir = PathBuf::from(format!("/var/tmp/hc-c-{}", &super::super::random_hex()[..8]));
        super::super::private_dir(&dir).unwrap();
        let scratch = Job { dir, target: String::new(), loaded: false };
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let now = Duration::from_secs(1000);
        let make = |pid: u32, stamp: u64| {
            let path = scratch.dir.join(format!(
                "hc-launchd-{pid}-{}-{}",
                Duration::from_secs(stamp).as_nanos(),
                super::super::random_hex()
            ));
            super::super::private_dir(&path).unwrap();
            std::fs::write(path.join("job.plist"), "interrupted before unlink").unwrap();
            UnixListener::bind(path.join("ready")).unwrap();
            path
        };
        let abandoned = make(dead, 900);
        let recent = make(dead, 990);
        let live = make(std::process::id(), 900);
        let kept = scratch.dir.join("kept");
        super::super::private_dir(&kept).unwrap();
        let link = scratch.dir.join(format!(
            "hc-launchd-{dead}-{}-{}",
            Duration::from_secs(900).as_nanos(),
            super::super::random_hex()
        ));
        std::os::unix::fs::symlink(&kept, &link).unwrap();
        remove_stale_dirs(&scratch.dir, now);
        assert!(!abandoned.exists(), "dead starter's socket and plist reclaimed");
        assert!(recent.exists(), "recent attempts left alone");
        assert!(live.exists(), "live starters left alone");
        assert!(link.exists() && kept.exists(), "symlinks left alone");
    }

    #[test]
    fn an_older_target_rejects_the_job_instead_of_starting_a_default_core() {
        let Some(old) = std::env::var_os("HC_OLD_BIN") else { return };
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        if launchctl(&[OsStr::new("print"), OsStr::new(&domain)]).is_err() {
            return;
        }
        let dir = PathBuf::from(format!("/var/tmp/hc-old-launchd-{}", super::super::random_hex()));
        super::super::private_dir(&dir).unwrap();
        let scratch = Job { dir, target: String::new(), loaded: false };
        let log = scratch.dir.join("core.log");
        let mut cmd = Command::new(old);
        cmd.arg("serve");
        let error = launch(&cmd, &log).unwrap_err();
        if error.to_string().contains("launchctl bootstrap") {
            eprintln!("skipping: bootstrap denied: {error}");
            return;
        }
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(error.to_string().contains("older binary"));
        let log = std::fs::read_to_string(log).unwrap();
        assert!(log.contains(SENTINEL), "old target reported usage, never served: {log}");
        let list = launchctl(&[OsStr::new("list")]).unwrap();
        assert!(!list.contains(&format!("{PREFIX}{}.", std::process::id())), "job removed after timeout");
    }

    #[test]
    fn unrepresentable_paths_require_direct_spawn() {
        for value in [OsStr::from_bytes(b"bad\xff"), OsStr::new("line\rbreak"), OsStr::new("\x1b[0m")] {
            assert!(xml(value).is_err());
        }
        assert_eq!(xml(OsStr::new("<&>\"'\t\n")).unwrap(), "&lt;&amp;&gt;&quot;'\t\n");
    }
}
