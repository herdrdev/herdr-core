//! Unix login settings: full exports once on initial Linux SSH start, and
//! fresh login `PATH` resolution for programs launched by a running core.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const CAPTURING: &str = "HC_LOGIN_CAPTURE";
pub(super) const SEED: &str = "_HC_LOGIN_SEED";
const MARKER: &[u8] = b"\0HC_LOGIN_ENV\0";
const END: &[u8] = b"\0HC_LOGIN_END\0";
const SCRIPT: &str = "printf '\\0HC_LOGIN_ENV\\0'; /usr/bin/env -0; printf '\\0HC_LOGIN_END\\0'";
const CAPTURE_TTL: Duration = Duration::from_secs(2);
const MISS_COOLDOWN: Duration = Duration::from_secs(1);
const CAPTURE_WAIT: Duration = Duration::from_secs(1);

const CAPTURE_ENV_KEYS: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
];

const HOME_WATCHED: &[&str] = &[
    ".profile",
    ".bash_profile",
    ".bash_login",
    ".bashrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zshrc",
    ".cshrc",
    ".tcshrc",
    ".login",
    ".tool-versions",
    ".config/fish/config.fish",
    ".config/fish/conf.d",
    ".config/mise/config.toml",
    ".local/bin",
    ".local/bin/env",
    "bin",
    ".cargo/env",
    ".cargo/bin",
    ".bun/bin",
    ".deno/bin",
    ".volta/bin",
    ".local/share/pnpm",
    ".local/share/mise/shims",
    ".npm-global/bin",
    ".nvm/alias/default",
    ".nvm/versions/node",
    ".pyenv/version",
    ".pyenv/shims",
    ".rbenv/version",
    ".rbenv/shims",
    ".asdf/shims",
    ".sdkman/candidates",
];

const SYSTEM_WATCHED: &[&str] = &[
    "/etc/profile",
    "/etc/profile.d",
    "/etc/zshenv",
    "/etc/zprofile",
    "/etc/paths",
    "/etc/paths.d",
    "/opt/homebrew/bin",
    "/usr/local/bin",
];

#[derive(Clone, PartialEq, Eq)]
struct CacheKey {
    shell: OsString,
    home: OsString,
    fingerprint: u64,
}

struct CacheEntry {
    key: CacheKey,
    initial_login: Option<OsString>,
    last_good: Option<OsString>,
    outcome_ok: bool,
    captured_at: Instant,
    last_miss_probe: Option<Instant>,
}

struct CacheState {
    entry: Option<CacheEntry>,
    capturing: bool,
}

static CACHE: Mutex<CacheState> = Mutex::new(CacheState { entry: None, capturing: false });
static CAPTURE_DONE: Condvar = Condvar::new();

struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let mut st = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        st.capturing = false;
        CAPTURE_DONE.notify_all();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SeedKind {
    Ok,
    Err,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RecaptureReason {
    Cold,
    Changed,
    Expired,
    MissingCommand,
}

#[cfg(target_os = "linux")]
pub(super) fn apply(cmd: &mut Command, state: &Path) {
    use std::io::Write;
    if std::env::var_os(CAPTURING).is_some() {
        cmd.env_remove(CAPTURING);
        return;
    }
    if std::env::var_os(crate::sweep::RUNTIME).is_some()
        || !super::SSH_LOGIN.iter().any(|key| std::env::var_os(key).is_some())
    {
        return;
    }
    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let home = std::env::var_os("HOME").unwrap_or_default();
    let mut probe = login_shell_command(&shell);
    probe.env(CAPTURING, "1");
    let (result, diagnostics) = capture(probe);
    if !diagnostics.is_empty()
        && let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(state.join("core.log"))
    {
        let _ = log.write_all(&diagnostics[..diagnostics.len().min(1 << 20)]);
    }
    let fp = fingerprint(&shell, Path::new(&home));
    match result {
        Ok(mut vars) => {
            vars.remove(OsStr::new(CAPTURING));
            let mut vars = super::restored_environment(vars, std::env::vars_os(), false);
            // Profiles must not hide the SSH origin from systemd's session filtering.
            for key in super::SSH_LOGIN {
                if let Some(value) = std::env::var_os(key) {
                    vars.insert((*key).into(), value);
                }
            }
            for (key, _) in std::env::vars_os() {
                if !vars.contains_key(&key) {
                    cmd.env_remove(key);
                }
            }
            cmd.envs(vars);
            cmd.env(SEED, format!("ok:{fp:016x}"));
        }
        Err(error) => {
            if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(state.join("core.log")) {
                let _ = writeln!(
                    log,
                    "core: login environment from {} unavailable: {error}; using inherited environment",
                    Path::new(&shell).display()
                );
            }
            cmd.env(SEED, format!("err:{fp:016x}"));
        }
    }
}

pub(super) fn refresh_path(env: &mut BTreeMap<String, String>, program: Option<&str>) {
    let seed = env.remove(SEED);
    if std::env::var_os(CAPTURING).is_some() {
        return;
    }
    let core_path = env.get("PATH").cloned().unwrap_or_default();
    let shell = env.get("SHELL").filter(|s| !s.is_empty()).map_or("/bin/sh", String::as_str);
    let home = env.get("HOME").filter(|s| !s.is_empty()).map_or("", String::as_str);
    let key = CacheKey {
        shell: OsString::from(shell),
        home: OsString::from(home),
        fingerprint: fingerprint(OsStr::new(shell), Path::new(home)),
    };

    let (reason, prev_initial, prev_good) = {
        let mut st = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        while st.capturing {
            let (guard, wait) = CAPTURE_DONE.wait_timeout(st, CAPTURE_WAIT).unwrap_or_else(|p| p.into_inner());
            st = guard;
            if wait.timed_out() {
                apply_cached(env, st.entry.as_ref(), &key, &core_path);
                return;
            }
        }
        if st.entry.is_none()
            && let Some(raw) = seed.as_deref()
            && let Some(kind) = parse_seed(raw, key.fingerprint)
        {
            let seeded_path = (kind == SeedKind::Ok).then(|| OsString::from(&core_path));
            st.entry = Some(CacheEntry {
                key: key.clone(),
                initial_login: seeded_path.clone(),
                last_good: seeded_path,
                outcome_ok: kind == SeedKind::Ok,
                captured_at: Instant::now(),
                last_miss_probe: None,
            });
        }
        let now = Instant::now();
        let reason = match &st.entry {
            None => Some(RecaptureReason::Cold),
            Some(entry) => {
                let effective = effective_path(entry, &key, &core_path);
                let missing = is_bare_command_missing(program, &effective);
                should_recapture(
                    entry.key == key,
                    entry.outcome_ok,
                    now.saturating_duration_since(entry.captured_at),
                    entry.last_miss_probe.map(|t| now.saturating_duration_since(t)),
                    missing,
                )
            }
        };
        let Some(reason) = reason else {
            apply_cached(env, st.entry.as_ref(), &key, &core_path);
            return;
        };
        let same_identity = st.entry.as_ref().is_some_and(|e| e.key.shell == key.shell && e.key.home == key.home);
        let prev_initial = if same_identity { st.entry.as_ref().and_then(|e| e.initial_login.clone()) } else { None };
        let prev_good = if same_identity { st.entry.as_ref().and_then(|e| e.last_good.clone()) } else { None };
        st.capturing = true;
        (reason, prev_initial, prev_good)
    };
    let guard = CaptureGuard;
    let (result, _) = capture(capture_command(OsStr::new(shell), env, OsStr::new(&core_path)));
    let now = Instant::now();
    let (initial, good) = {
        let mut st = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        let same_identity = st.entry.as_ref().is_some_and(|e| e.key.shell == key.shell && e.key.home == key.home);
        let prev_miss = if same_identity { st.entry.as_ref().and_then(|e| e.last_miss_probe) } else { None };
        let prev_failed_same = st.entry.as_ref().is_some_and(|e| e.key == key && !e.outcome_ok);
        let last_miss_probe = match reason {
            RecaptureReason::MissingCommand => Some(now),
            _ => prev_miss,
        };
        match result {
            Ok(vars) => {
                let login_path = vars.get(OsStr::new("PATH")).cloned().unwrap_or_default();
                let initial = prev_initial.or_else(|| Some(login_path.clone()));
                let good = Some(login_path);
                st.entry = Some(CacheEntry {
                    key,
                    initial_login: initial.clone(),
                    last_good: good.clone(),
                    outcome_ok: true,
                    captured_at: now,
                    last_miss_probe,
                });
                (initial, good)
            }
            Err(error) => {
                if !prev_failed_same {
                    eprintln!("core: login PATH from {} unavailable: {error}", Path::new(shell).display());
                }
                st.entry = Some(CacheEntry {
                    key,
                    initial_login: prev_initial.clone(),
                    last_good: prev_good.clone(),
                    outcome_ok: false,
                    captured_at: now,
                    last_miss_probe,
                });
                (prev_initial, prev_good)
            }
        }
    };
    drop(guard);
    if let Some(good) = good {
        apply_merged(env, &core_path, initial.as_deref(), &good);
    }
}

fn effective_path(entry: &CacheEntry, key: &CacheKey, core_path: &str) -> OsString {
    if entry.key.shell == key.shell
        && entry.key.home == key.home
        && let Some(good) = &entry.last_good
    {
        merge_path(OsStr::new(core_path), entry.initial_login.as_deref(), good)
    } else {
        OsString::from(core_path)
    }
}

fn apply_cached(env: &mut BTreeMap<String, String>, entry: Option<&CacheEntry>, key: &CacheKey, core_path: &str) {
    if let Some(entry) = entry
        && entry.key.shell == key.shell
        && entry.key.home == key.home
        && let Some(good) = &entry.last_good
    {
        apply_merged(env, core_path, entry.initial_login.as_deref(), good);
    }
}

fn apply_merged(
    env: &mut BTreeMap<String, String>,
    core_path: &str,
    initial_login: Option<&OsStr>,
    current_login: &OsStr,
) {
    let merged = merge_path(OsStr::new(core_path), initial_login, current_login);
    if let Ok(utf8) = merged.into_string() {
        env.insert("PATH".into(), utf8);
    }
}

fn parse_seed(raw: &str, fp: u64) -> Option<SeedKind> {
    let (prefix, hex) = raw.split_once(':')?;
    if hex.len() != 16 || hex != format!("{fp:016x}") {
        return None;
    }
    match prefix {
        "ok" => Some(SeedKind::Ok),
        "err" => Some(SeedKind::Err),
        _ => None,
    }
}

fn should_recapture(
    cached_key_matches: bool,
    outcome_ok: bool,
    since_capture: Duration,
    since_miss_probe: Option<Duration>,
    command_missing: bool,
) -> Option<RecaptureReason> {
    if !cached_key_matches {
        return Some(RecaptureReason::Changed);
    }
    if !outcome_ok {
        return None;
    }
    if since_capture >= CAPTURE_TTL {
        return Some(RecaptureReason::Expired);
    }
    if command_missing && since_miss_probe.is_none_or(|d| d >= MISS_COOLDOWN) {
        return Some(RecaptureReason::MissingCommand);
    }
    None
}

fn is_bare_command_missing(program: Option<&str>, path: &OsStr) -> bool {
    let Some(program) = program.filter(|p| !p.is_empty() && !p.contains('/')) else {
        return false;
    };
    !std::env::split_paths(path).filter(|dir| !dir.as_os_str().is_empty()).any(|dir| {
        std::fs::metadata(dir.join(program)).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
    })
}

fn merge_path(core_path: &OsStr, initial_login: Option<&OsStr>, current_login: &OsStr) -> OsString {
    let split =
        |s: &OsStr| -> Vec<PathBuf> { std::env::split_paths(s).filter(|p| !p.as_os_str().is_empty()).collect() };
    let current = split(current_login);
    if current.is_empty() {
        return core_path.to_os_string();
    }
    let core = split(core_path);
    let initial = initial_login.map(split).unwrap_or_default();
    let (prefix, suffix): (&[PathBuf], Vec<PathBuf>) = if initial.is_empty() {
        (&[], core)
    } else if let Some(at) = core.iter().position(|p| initial.contains(p)) {
        let rest = core[at + 1..].iter().filter(|p| !initial.contains(p)).cloned().collect();
        (&core[..at], rest)
    } else {
        (&[], core)
    };
    join_unique_paths(prefix.iter().cloned().chain(current).chain(suffix))
}

fn capture_base_path(core_path: &OsStr) -> OsString {
    let base = ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"];
    let nix = std::env::split_paths(core_path).filter(|p| {
        let bytes = p.as_os_str().as_bytes();
        bytes.starts_with(b"/nix/") || bytes.starts_with(b"/run/current-system/")
    });
    join_unique_paths(base.into_iter().map(PathBuf::from).chain(nix))
}

fn join_unique_paths(paths: impl IntoIterator<Item = PathBuf>) -> OsString {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() || !seen.insert(path.clone()) {
            continue;
        }
        if !out.is_empty() {
            out.push(b':');
        }
        out.extend_from_slice(bytes);
    }
    OsString::from_vec(out)
}

fn fingerprint(shell: &OsStr, home: &Path) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    shell.as_bytes().hash(&mut hasher);
    stat_hash(&mut hasher, Path::new(shell));
    home.as_os_str().as_bytes().hash(&mut hasher);
    if !home.as_os_str().is_empty() {
        for rel in HOME_WATCHED {
            stat_hash(&mut hasher, &home.join(rel));
        }
    }
    for sys in SYSTEM_WATCHED {
        stat_hash(&mut hasher, Path::new(sys));
    }
    hasher.finish()
}

fn stat_hash(hasher: &mut impl Hasher, path: &Path) {
    match std::fs::metadata(path) {
        Ok(m) => {
            true.hash(hasher);
            m.dev().hash(hasher);
            m.ino().hash(hasher);
            m.mode().hash(hasher);
            m.len().hash(hasher);
            m.mtime().hash(hasher);
            m.mtime_nsec().hash(hasher);
            m.ctime().hash(hasher);
            m.ctime_nsec().hash(hasher);
        }
        Err(_) => false.hash(hasher),
    }
}

fn login_shell_command(shell: &OsStr) -> Command {
    let mut cmd = Command::new(shell);
    let name = Path::new(shell).file_name().unwrap_or_default().to_string_lossy();
    match name.as_ref() {
        "csh" | "tcsh" => {
            cmd.arg0(format!("-{name}"));
        }
        _ => {
            cmd.arg("-l");
        }
    }
    cmd.args(["-c", SCRIPT]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

fn capture_command(shell: &OsStr, env: &BTreeMap<String, String>, core_path: &OsStr) -> Command {
    let mut cmd = login_shell_command(shell);
    cmd.env_clear();
    for key in CAPTURE_ENV_KEYS {
        if let Some(val) = env.get(*key) {
            cmd.env(key, val);
        }
    }
    cmd.env("TERM", "dumb");
    cmd.env(CAPTURING, "1");
    cmd.env("PATH", capture_base_path(core_path));
    cmd
}

fn capture(mut cmd: Command) -> (io::Result<BTreeMap<OsString, OsString>>, Vec<u8>) {
    use std::os::fd::AsRawFd;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return (Err(error), Vec::new()),
    };
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut bytes = Vec::new();
    let mut diagnostics = Vec::new();
    let result = (|| {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let start = Instant::now();
        let mut remaining = 1 << 20;
        loop {
            let status = child.try_wait()?;
            for (stream, output) in
                [(&mut stdout as &mut dyn Read, &mut bytes), (&mut stderr as &mut dyn Read, &mut diagnostics)]
            {
                let mut buffer = [0; 8192];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if n > remaining {
                                return Err(io::Error::other("profile output exceeded 1 MiB"));
                            }
                            remaining -= n;
                            output.extend_from_slice(&buffer[..n]);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    }
                    if start.elapsed() >= Duration::from_secs(5) {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "profile exceeded 5 s"));
                    }
                }
            }
            if let Some(status) = status {
                if !status.success() {
                    return Err(io::Error::other(format!("profile exited with {status}")));
                }
                return parse(&bytes);
            }
            if start.elapsed() >= Duration::from_secs(5) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "profile exceeded 5 s"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // The shell may exit before its background jobs close the capture pipes.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    (result, diagnostics)
}

fn parse(bytes: &[u8]) -> io::Result<BTreeMap<OsString, OsString>> {
    let invalid = || io::Error::other("profile did not produce a complete environment");
    let start = bytes.windows(MARKER.len()).position(|part| part == MARKER).ok_or_else(invalid)? + MARKER.len();
    let tail = &bytes[start..];
    let end = tail.windows(END.len()).position(|part| part == END).ok_or_else(invalid)?;
    let bytes = &tail[..end];
    if bytes.is_empty() || !bytes.ends_with(&[0]) {
        return Err(invalid());
    }
    bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .map(|entry| {
            let at = entry.iter().position(|byte| *byte == b'=').filter(|at| *at > 0).ok_or_else(invalid)?;
            Ok((OsString::from_vec(entry[..at].to_vec()), OsString::from_vec(entry[at + 1..].to_vec())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_environment_ignores_profile_chatter_and_preserves_values() {
        let vars =
            parse(b"hello\n\0HC_LOGIN_ENV\0PATH=/home/me/bin:/bin\0VALUE=one\ntwo=three\0\0HC_LOGIN_END\0goodbye\n")
                .unwrap();
        assert_eq!(vars[OsStr::new("PATH")], "/home/me/bin:/bin");
        assert_eq!(vars[OsStr::new("VALUE")], "one\ntwo=three");
        assert!(parse(b"hello\n").is_err());
        assert!(parse(b"\0HC_LOGIN_ENV\0PATH=/bin").is_err());
    }

    #[test]
    fn merge_path_keeps_explicit_launcher_additions_and_drops_stale_login_entries() {
        let merge = |core: &str, initial: Option<&str>, current: &str| {
            merge_path(OsStr::new(core), initial.map(OsStr::new), OsStr::new(current))
        };
        assert_eq!(
            merge("/usr/local/bin:/usr/bin:/bin", Some("/usr/local/bin:/usr/bin:/bin"), "/u/.local/bin:/usr/bin"),
            "/u/.local/bin:/usr/bin"
        );
        assert_eq!(
            merge("/nvm/v20/bin:/usr/bin:/bin", Some("/nvm/v20/bin:/usr/bin:/bin"), "/nvm/v22/bin:/usr/bin:/bin"),
            "/nvm/v22/bin:/usr/bin:/bin"
        );
        assert_eq!(
            merge(
                "/venv/bin:/usr/local/bin:/usr/bin",
                Some("/usr/local/bin:/usr/bin"),
                "/u/.local/bin:/usr/local/bin:/usr/bin"
            ),
            "/venv/bin:/u/.local/bin:/usr/local/bin:/usr/bin"
        );
        assert_eq!(merge("/a:/usr/bin:/b:/old", Some("/usr/bin:/old"), "/new:/usr/bin:/a"), "/a:/new:/usr/bin:/b");
        assert_eq!(merge("/custom/only", Some("/usr/bin"), "/new:/usr/bin"), "/new:/usr/bin:/custom/only");
        assert_eq!(merge("/custom/only", None, "/new:/usr/bin"), "/new:/usr/bin:/custom/only");
        assert_eq!(merge("/usr/bin", Some("/usr/bin"), ""), "/usr/bin");
    }

    #[test]
    fn capture_base_path_keeps_only_standard_and_nix_directories() {
        let base = capture_base_path(OsStr::new("/u/.local/bin:/nix/store/x/bin:/venv/bin:/run/current-system/sw/bin"));
        assert_eq!(
            base,
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/nix/store/x/bin:/run/current-system/sw/bin"
        );
    }

    #[test]
    fn recapture_policy_blocks_broken_repeat_and_responds_to_changes_and_misses() {
        assert_eq!(should_recapture(false, false, Duration::ZERO, None, false), Some(RecaptureReason::Changed));
        assert_eq!(should_recapture(true, false, Duration::from_secs(60), None, true), None);
        assert_eq!(should_recapture(true, true, Duration::from_millis(200), None, false), None);
        assert_eq!(should_recapture(true, true, Duration::from_secs(2), None, false), Some(RecaptureReason::Expired));
        assert_eq!(
            should_recapture(true, true, Duration::from_millis(100), None, true),
            Some(RecaptureReason::MissingCommand)
        );
        assert_eq!(
            should_recapture(true, true, Duration::from_millis(100), Some(Duration::from_millis(200)), true),
            None
        );
        assert_eq!(
            should_recapture(true, true, Duration::from_millis(100), Some(Duration::from_secs(1)), true),
            Some(RecaptureReason::MissingCommand)
        );
    }

    #[test]
    fn seed_parser_requires_exact_fingerprint_hex() {
        assert_eq!(parse_seed("ok:000000000000002a", 42), Some(SeedKind::Ok));
        assert_eq!(parse_seed("err:000000000000002a", 42), Some(SeedKind::Err));
        assert_eq!(parse_seed("ok:000000000000002b", 42), None);
        assert_eq!(parse_seed("ok:2a", 42), None);
        assert_eq!(parse_seed("other:000000000000002a", 42), None);
    }
}
