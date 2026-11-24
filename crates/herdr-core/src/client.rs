//! Talking to the core from a short-lived process: connect, autostart, one call.

use crate::paths::Paths;
use crate::platform::{self, Stream};
use crate::rpc::RpcError;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::Command;
use std::time::{Duration, Instant};

static EXE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Remembers our binary's path at startup. After an update swaps the file,
/// asking the OS again would name the deleted old binary.
pub fn remember_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = EXE.set(exe);
    }
    build_id();
}

/// The package manager this hc came from, or "system package manager" as a
/// compatibility marker when its folder isn't writable. None for a replaceable install.
pub fn installed_by() -> Option<&'static str> {
    let exe = EXE.get().cloned().or_else(|| std::env::current_exe().ok())?;
    installed_by_path(&exe)
}

pub(crate) fn installed_by_path(exe: &std::path::Path) -> Option<&'static str> {
    let path = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    manager_of(&path.to_string_lossy()).or_else(|| (!platform::can_replace(&path)).then_some("system package manager"))
}

fn manager_of(path: &str) -> Option<&'static str> {
    [("/Cellar/", "homebrew"), ("/nix/store/", "nix"), ("/mise/installs/", "mise")]
        .into_iter()
        .find(|(mark, _)| path.contains(mark))
        .map(|(_, manager)| manager)
}

#[cfg(test)]
#[test]
fn the_package_manager_is_read_from_the_path() {
    assert_eq!(manager_of("/opt/homebrew/Cellar/herdr-core/0.2.0/bin/herdr-core"), Some("homebrew"));
    assert_eq!(manager_of("/nix/store/abc-herdr-core/bin/herdr-core"), Some("nix"));
    assert_eq!(manager_of("/home/me/.local/share/mise/installs/herdr-core/0.2.0/herdr-core"), Some("mise"));
    assert_eq!(manager_of("/home/me/.local/bin/herdr-core"), None);
}

/// Which build this process runs: the linker's build id, so a copy of the same
/// bytes is the same build (a lazy filesystem swaps in such a copy under a
/// running core). Elsewhere, or without one: same file, same build. Taken at
/// startup, so an update that swaps the file later doesn't change it.
pub fn build_id() -> String {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD
        .get_or_init(|| {
            if let Some(id) = platform::running_build_id() {
                return id;
            }
            let exe = EXE.get().cloned().or_else(|| std::env::current_exe().ok());
            exe.and_then(|e| std::fs::metadata(e).ok()).map(|m| platform::file_identity(&m)).unwrap_or_default()
        })
        .clone()
}

fn unavailable(message: String) -> RpcError {
    RpcError::new("unavailable", message, "check the core log in the state directory")
}

pub fn connect_existing(paths: &Paths) -> Option<Stream> {
    platform::connect(&paths.socket).ok()
}

/// Connects to this session's core, starting it if nobody is listening.
pub fn connect_or_start(paths: &Paths) -> Result<Stream, RpcError> {
    connect_starting(paths, false)
}

pub fn connect_or_start_explicit(paths: &Paths) -> Result<Stream, RpcError> {
    connect_starting(paths, true)
}

fn connect_starting(paths: &Paths, explicit: bool) -> Result<Stream, RpcError> {
    match platform::connect(&paths.socket) {
        Ok(stream) => return Ok(stream),
        Err(e) => {
            if let Some(error) = platform::connect_error(&e, false) {
                return Err(error);
            }
        }
    }
    if let Some(err) = listening_elsewhere(paths) {
        return Err(err);
    }
    platform::may_autostart(paths, explicit)?;
    paths.create_dirs().map_err(|e| unavailable(format!("cannot start core: {e}")))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.state.join("core.lock"))
        .map_err(|e| unavailable(format!("cannot open core lock: {e}")))?;
    let start = Instant::now();
    loop {
        if let Ok(stream) = platform::connect(&paths.socket) {
            return Ok(stream);
        }
        // A closing connection does not mean serve has released its lock yet.
        // Starting its replacement early would make that process exit immediately.
        if platform::try_lock(&lock) {
            break;
        }
        if start.elapsed() > START_WAIT {
            return Err(unavailable(format!("core did not stop or start within {} s", START_WAIT.as_secs())));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(lock);
    spawn_core(paths)?;
    wait_for_socket(&paths.socket, "core").map_err(unavailable)
}

pub fn require_existing(paths: &Paths) -> Result<Stream, RpcError> {
    platform::connect(&paths.socket)
        .map_err(|e| platform::connect_error(&e, true).unwrap_or_else(|| unavailable(e.to_string())))
}

/// This session's core runs, but on a socket this env doesn't point at.
fn listening_elsewhere(paths: &Paths) -> Option<RpcError> {
    let actual = std::path::PathBuf::from(std::fs::read_to_string(paths.state.join("core.lock")).ok()?);
    if actual == paths.socket || platform::connect(&actual).is_err() {
        return None;
    }
    Some(RpcError::new(
        "unavailable",
        format!("the core listens at {}, but this shell looks at {}", actual.display(), paths.socket.display()),
        "XDG_RUNTIME_DIR or HC_SOCKET differs from where the core started; set HC_SOCKET to the first path",
    ))
}

pub fn spawn_core(paths: &Paths) -> Result<(), RpcError> {
    spawn_detached(paths, &["serve"], "core.log").map_err(|e| unavailable(format!("cannot start core: {e}")))
}

/// Starts `herdr-core <args>` on its own (Linux: as a user service), logging
/// to the state dir.
pub fn spawn_detached(paths: &Paths, args: &[&str], log: &str) -> std::io::Result<()> {
    paths.create_dirs()?;
    let log = paths.state.join(log);
    let exe = EXE.get().cloned().map_or_else(std::env::current_exe, Ok)?;
    let exe = if args.first() == Some(&"serve") { platform::core_executable(&exe) } else { exe };
    let mut cmd = Command::new(exe);
    if args.first() == Some(&"serve") {
        platform::core_environment(&mut cmd, &paths.state);
    }
    // A core or holder started from inside a terminal is not that terminal's:
    // ending the terminal must not end them.
    cmd.args(args).env_remove(crate::sweep::VAR).env(crate::sweep::RUNTIME, "1");
    platform::spawn_runtime(paths, cmd, &log)
}

/// How long a starting core or holder may take to answer. A busy machine (or
/// one right after an update) can need well over 5 s; giving up early reports
/// a failure while the program is still coming up.
const START_WAIT: Duration = Duration::from_secs(15);

pub fn wait_for_socket(path: &std::path::Path, what: &str) -> Result<Stream, String> {
    let start = Instant::now();
    loop {
        if let Ok(stream) = platform::connect(path) {
            return Ok(stream);
        }
        if start.elapsed() > START_WAIT {
            return Err(format!("{what} did not start within {} s", START_WAIT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

const CLOSED: &str = "the core closed the connection";

/// A call the core cut off: if a core answers again soon it was a restart
/// (an update, usually) and the command can simply run again.
pub fn explain_closed(paths: &Paths, err: RpcError) -> RpcError {
    if err.message != CLOSED {
        return err;
    }
    // A loaded machine can take seconds to bring the new core up (seen on CI
    // at over 3 s); only a core that really stopped waits this out.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if connect_existing(paths).is_some() {
            return RpcError::new(
                "unavailable",
                "the core restarted while this ran (an update?)",
                "run the command again; terminals kept running",
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    RpcError::new(
        "unavailable",
        "the core stopped while this ran",
        "run the command again to start it; see core.log in the state directory if it keeps stopping",
    )
}

/// One plain-mode call. Returns the result, the core's error, or `unavailable`.
pub fn call(stream: &Stream, method: &str, params: Value) -> Result<Value, RpcError> {
    let lost = |e: std::io::Error| unavailable(format!("lost the core: {e}"));
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    (&*stream).write_all(format!("{req}\n").as_bytes()).map_err(lost)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(lost)?;
    if line.is_empty() {
        return Err(unavailable(CLOSED.into()));
    }
    let reply: Value = serde_json::from_str(&line).map_err(|e| unavailable(e.to_string()))?;
    match reply.get("error") {
        Some(err) => Err(RpcError::from_json(err)),
        None => Ok(reply["result"].clone()),
    }
}

/// The hc terminal we run in, if it belongs to the core we talk to: its id
/// means nothing to another session's core.
pub fn terminal_id() -> Option<String> {
    let own_core = std::env::var_os("HC_TERMINAL_SOCKET")
        .is_none_or(|s| Paths::from_env().is_ok_and(|p| p.socket.as_os_str() == s));
    std::env::var("HC_TERMINAL_ID").ok().filter(|_| own_core)
}
