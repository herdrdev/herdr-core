//! Linux: cores and holders run as systemd user services. They belong to the
//! user, not to whatever started them: a desktop crash, a logout or an ssh
//! login ending can't take them along, and an ssh login's environment never
//! becomes theirs.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::linux_session::{SessionScope, resolve_tool, session_scope};
use crate::paths::Paths;

/// What the login session decides. Programs get it from the user manager as
/// it is now; a service takes the caller's only when the caller isn't an ssh
/// login (desktops that don't share theirs with systemd: i3, startx).
pub(super) const SESSION: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_CLASS",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_ID",
    "XDG_CURRENT_DESKTOP",
    "DESKTOP_SESSION",
];

/// What systemd sets for a service: not a program's business, and a service
/// starting another must not pass its own on.
pub const SERVICE: &[&str] = &[
    "INVOCATION_ID",
    "RUNTIME_DIRECTORY",
    "JOURNAL_STREAM",
    "SYSTEMD_EXEC_PID",
    "MANAGERPID",
    "MANAGERPIDFDID",
    "MEMORY_PRESSURE_WATCH",
    "MEMORY_PRESSURE_WRITE",
    "NOTIFY_SOCKET",
    "WATCHDOG_PID",
    "WATCHDOG_USEC",
    "LISTEN_PID",
    "LISTEN_FDS",
    "LISTEN_FDNAMES",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UserManager {
    pub(super) runtime_dir: PathBuf,
    pub(super) systemd_run: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NoUserManager {
    CustomRuntimeDir { ours: PathBuf, expected: PathBuf },
    NoPrivateSocket { expected_socket: PathBuf },
    MissingSystemdRun,
}

impl fmt::Display for NoUserManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CustomRuntimeDir { ours, expected } => {
                write!(f, "XDG_RUNTIME_DIR is {} instead of {}", ours.display(), expected.display())
            }
            Self::NoPrivateSocket { expected_socket } => {
                write!(f, "{} does not exist", expected_socket.display())
            }
            Self::MissingSystemdRun => f.write_str("systemd-run not found on PATH or standard directories"),
        }
    }
}

pub(super) fn runtime_dir() -> PathBuf {
    super::linux_session::default_runtime_dir()
}

pub(super) fn user_service_runner() -> Result<UserManager, NoUserManager> {
    user_service_runner_with(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        &runtime_dir(),
        |p| p.exists(),
        || resolve_tool("systemd-run"),
    )
}

fn user_service_runner_with(
    xdg_runtime_dir: Option<&OsStr>,
    expected_runtime_dir: &Path,
    has_private_socket: impl FnOnce(&Path) -> bool,
    resolve_systemd_run: impl FnOnce() -> Option<PathBuf>,
) -> Result<UserManager, NoUserManager> {
    let ours =
        xdg_runtime_dir.filter(|v| !v.is_empty()).map_or_else(|| expected_runtime_dir.to_path_buf(), PathBuf::from);
    if ours != expected_runtime_dir {
        return Err(NoUserManager::CustomRuntimeDir { ours, expected: expected_runtime_dir.to_path_buf() });
    }
    let expected_socket = expected_runtime_dir.join("systemd/private");
    if !has_private_socket(&expected_socket) {
        return Err(NoUserManager::NoPrivateSocket { expected_socket });
    }
    let Some(systemd_run) = resolve_systemd_run() else {
        return Err(NoUserManager::MissingSystemdRun);
    };
    Ok(UserManager { runtime_dir: expected_runtime_dir.to_path_buf(), systemd_run })
}

const NO_MANAGER: &str = "hc: no systemd user manager here: the core ends with whatever started it";

#[derive(Debug)]
pub(super) enum Spawn {
    Started,
    NoUserManager,
    Failed(io::Error),
}

pub(super) fn spawn(paths: &Paths, cmd: &Command, log: &Path) -> Spawn {
    let mut stderr = io::stderr();
    let scope = session_scope(paths);
    let mgr = user_service_runner();
    let started = spawn_inner(cmd, log, scope, mgr, cached_systemd_version, ensure_shutdown_guard, &mut stderr);
    if matches!(started, Spawn::Started) {
        super::systemd_linger::linger();
    }
    started
}

fn cached_systemd_version(systemd_run: &Path) -> u32 {
    static VERSION: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *VERSION.get_or_init(|| {
        Command::new(systemd_run)
            .arg("--version")
            .output()
            .ok()
            .and_then(|out| String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1)?.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    })
}

fn spawn_inner(
    cmd: &Command,
    log: &Path,
    scope: SessionScope,
    manager: Result<UserManager, NoUserManager>,
    systemd_version: impl FnOnce(&Path) -> u32,
    ensure_guard: impl FnOnce(&UserManager, &Path) -> io::Result<()>,
    stderr: &mut dyn Write,
) -> Spawn {
    let mgr = match manager {
        Ok(mgr) => mgr,
        Err(_) => {
            if scope == SessionScope::Shared {
                let _ = writeln!(stderr, "{NO_MANAGER}");
            }
            return Spawn::NoUserManager;
        }
    };
    let guarded = match ensure_guard(&mgr, log) {
        Ok(()) => true,
        Err(e) => {
            let _ = writeln!(stderr, "hc: {e}; starting without shutdown ordering");
            false
        }
    };
    let mut caller: BTreeMap<String, String> = super::utf8_vars().collect();
    let mut unset: Vec<&str> = super::SSH_LOGIN.to_vec();
    for (k, v) in cmd.get_envs() {
        let Some(k) = k.to_str() else { continue };
        match v.and_then(OsStr::to_str) {
            Some(v) => caller.insert(k.into(), v.into()),
            None => {
                unset.push(k);
                caller.remove(k)
            }
        };
    }
    let mut env = service_env(caller);
    env.insert("XDG_RUNTIME_DIR".into(), mgr.runtime_dir.display().to_string());
    let args: Vec<&OsStr> = cmd.get_args().collect();
    let name = format!("hc-{}-{}", args.first().and_then(|a| a.to_str()).unwrap_or("run"), &super::random_hex()[..8]);
    let here = std::env::current_dir().ok().filter(|d| d.is_dir()).unwrap_or_else(|| PathBuf::from("/"));
    let version = systemd_version(&mgr.systemd_run);
    let dir = match write_environment(&mgr.runtime_dir, &name, &env) {
        Ok(dir) => dir,
        Err(e) => return Spawn::Failed(e),
    };
    let command_arguments = service_command_args(cmd, log, version);
    let out = Command::new(&mgr.systemd_run)
        .env_clear()
        .envs(&env)
        .args(unit_args(&name, &dir.join("environment"), &unset, log, &here, version))
        .args(guarded.then(|| shutdown_order(cmd, log)).into_iter().flat_map(|order| [OsString::from("-p"), order]))
        .args(command_arguments.iter().map(|argument| literal(argument)))
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => Spawn::Started,
        Ok(out) => {
            let _ = std::fs::remove_dir_all(dir);
            Spawn::Failed(io::Error::other(format!("systemd-run: {}", String::from_utf8_lossy(&out.stderr).trim())))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(dir);
            Spawn::Failed(io::Error::new(e.kind(), format!("systemd-run: {e}")))
        }
    }
}

// The guard stays active across core upgrades. On shutdown, reversing the
// holder -> guard -> core start order saves the core before shells can exit.
fn shutdown_guard(log: &Path) -> String {
    let hash = log
        .parent()
        .unwrap_or(log)
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3));
    format!("hc-shutdown-{hash:016x}.service")
}

fn ensure_shutdown_guard(mgr: &UserManager, log: &Path) -> io::Result<()> {
    let name = shutdown_guard(log);
    let start = || {
        let Some(systemctl) = resolve_tool("systemctl") else {
            return false;
        };
        Command::new(systemctl)
            .env("XDG_RUNTIME_DIR", &mgr.runtime_dir)
            .args(["--user", "start", &name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if start() {
        return Ok(());
    }
    let out = Command::new(&mgr.systemd_run)
        .env("XDG_RUNTIME_DIR", &mgr.runtime_dir)
        .args(["--user", "--quiet", "--collect", "--service-type=oneshot", "--unit", &name])
        .args(["-p", "RemainAfterExit=yes", "/bin/sh", "-c", ":"])
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() || start() {
        Ok(())
    } else {
        Err(io::Error::other(format!("systemd shutdown guard: {}", String::from_utf8_lossy(&out.stderr).trim())))
    }
}

/// EnvironmentFile uses double-quote escaping, not shell evaluation. Literal
/// newlines are allowed inside quotes; neither $ nor % expands in its values.
fn environment_file(env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in env {
        out.push_str(key);
        out.push_str("=\"");
        for c in value.chars() {
            if matches!(c, '\\' | '"' | '$' | '`') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push_str("\"\n");
    }
    out
}

/// Pre-create the private runtime directory so the file exists before systemd
/// reads it. The unit owns its lifetime: restarts keep it, final stops remove it.
fn write_environment(run: &Path, name: &str, env: &BTreeMap<String, String>) -> io::Result<PathBuf> {
    let dir = run.join(name);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let result = (|| {
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(dir.join("environment"))?;
        file.write_all(environment_file(env).as_bytes())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(dir)
}

/// The caller's environment as a service gets it: no ssh login's session, no
/// service's, and only names systemd accepts (a bash function's isn't one).
pub(super) fn service_env(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let ssh = super::SSH_LOGIN.iter().any(|k| env.contains_key(*k));
    let valid = |k: &str| {
        k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    env.retain(|k, _| {
        valid(k)
            && (!ssh || !SESSION.contains(&k.as_str()))
            && !SERVICE.contains(&k.as_str())
            && !super::SSH_LOGIN.contains(&k.as_str())
    });
    env
}

/// systemd-run's arguments for one hc service, without environment values.
/// OOMPolicy: a program killed for memory must not stop the whole service.
/// No ManagedOOMPreference: systemd rewrites its cgroup xattr on every property
/// change, and uresourced < 0.5.5 answers each rewrite with another change,
/// flooding the user manager until it starts nothing. KillMode: when the core
/// or holder exits, nothing else goes with it.
fn unit_args(name: &str, environment: &Path, unset: &[&str], log: &Path, here: &Path, version: u32) -> Vec<OsString> {
    let mut unset_environment_names = unset.to_vec();
    unset_environment_names.sort_unstable();
    unset_environment_names.dedup();
    let service_type = if version >= 240 { "--service-type=exec" } else { "--service-type=simple" };
    let supports_native_append_logging = version >= 240;
    let stderr_destination = if supports_native_append_logging {
        format!("StandardError=append:{}", log.display())
    } else {
        "StandardError=journal".into()
    };
    let mut out: Vec<OsString> = ["--user", "--quiet", "--collect", service_type].map(OsString::from).into();
    if version >= 243 {
        out.extend([OsString::from("-p"), OsString::from("OOMPolicy=continue")]);
    }
    for p in [
        format!("--unit={name}"),
        format!("EnvironmentFile={}", environment.display()),
        format!("RuntimeDirectory={name}"),
        "RuntimeDirectoryMode=0700".into(),
        "RuntimeDirectoryPreserve=restart".into(),
        format!("WorkingDirectory={}", here.display()),
        "KillMode=process".into(),
        "StandardOutput=null".into(),
        stderr_destination,
        format!("UnsetEnvironment={}", unset_environment_names.join(" ")),
    ] {
        if !p.starts_with("--") {
            out.push("-p".into());
        }
        out.push(p.into());
    }
    out
}

fn service_command_args(command: &Command, log_path: &Path, systemd_version: u32) -> Vec<OsString> {
    let mut command_arguments = Vec::new();
    let supports_native_append_logging = systemd_version >= 240;
    if !supports_native_append_logging {
        // exec preserves systemd's main PID; positional parameters prevent shell evaluation of paths.
        let append_stderr_and_exec_script = "exec \"$@\" 2>>\"$0\"";
        command_arguments.extend([
            OsString::from("/bin/sh"),
            "-c".into(),
            append_stderr_and_exec_script.into(),
            log_path.as_os_str().to_owned(),
        ]);
    }
    command_arguments.push(command.get_program().to_owned());
    command_arguments.extend(command.get_args().map(OsStr::to_owned));
    command_arguments
}

fn shutdown_order(cmd: &Command, log: &Path) -> OsString {
    let order = if cmd.get_args().next().is_some_and(|arg| arg == "hold") { "Before" } else { "After" };
    format!("{order}={}", shutdown_guard(log)).into()
}

// systemd expands $ in a service's command line; $$ is a plain $.
fn literal(arg: &OsStr) -> OsString {
    arg.to_string_lossy().replace('$', "$$").into()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{NoUserManager, Spawn, UserManager};
    use crate::platform::linux_session::SessionScope;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn user_service_runner_reports_specific_missing_preconditions() {
        let expected = Path::new("/run/user/1000");
        assert_eq!(
            super::user_service_runner_with(
                Some(OsStr::new("/tmp/custom-run")),
                expected,
                |_| true,
                || Some(PathBuf::from("/usr/bin/systemd-run")),
            ),
            Err(NoUserManager::CustomRuntimeDir {
                ours: PathBuf::from("/tmp/custom-run"),
                expected: expected.to_path_buf(),
            })
        );
        assert_eq!(
            super::user_service_runner_with(None, expected, |_| false, || Some(PathBuf::from("/usr/bin/systemd-run")),),
            Err(NoUserManager::NoPrivateSocket { expected_socket: expected.join("systemd/private") })
        );
        assert_eq!(
            super::user_service_runner_with(Some(OsStr::new("")), expected, |_| true, || None),
            Err(NoUserManager::MissingSystemdRun)
        );
        assert_eq!(
            super::user_service_runner_with(
                Some(expected.as_os_str()),
                expected,
                |_| true,
                || Some(PathBuf::from("/run/current-system/sw/bin/systemd-run")),
            ),
            Ok(UserManager {
                runtime_dir: expected.to_path_buf(),
                systemd_run: PathBuf::from("/run/current-system/sw/bin/systemd-run"),
            })
        );
    }

    #[test]
    fn spawn_stays_quiet_on_isolated_sessions_and_refuses_fallback_after_manager_detection() {
        let root = crate::test_tmp().join(format!("hc-systemd-spawn-{}", super::super::random_hex()));
        std::fs::create_dir_all(&root).unwrap();
        let cmd = Command::new("/bin/hc");
        let log = root.join("core.log");

        let mut isolated_stderr = Vec::new();
        let res = super::spawn_inner(
            &cmd,
            &log,
            SessionScope::Isolated,
            Err(NoUserManager::MissingSystemdRun),
            |_| 259,
            |_, _| Ok(()),
            &mut isolated_stderr,
        );
        assert!(matches!(res, Spawn::NoUserManager));
        assert!(isolated_stderr.is_empty(), "isolated session must not print NO_MANAGER to stderr");

        let mut shared_stderr = Vec::new();
        let res = super::spawn_inner(
            &cmd,
            &log,
            SessionScope::Shared,
            Err(NoUserManager::NoPrivateSocket { expected_socket: root.join("systemd/private") }),
            |_| 259,
            |_, _| Ok(()),
            &mut shared_stderr,
        );
        assert!(matches!(res, Spawn::NoUserManager));
        assert!(String::from_utf8_lossy(&shared_stderr).contains(super::NO_MANAGER));

        let mgr = UserManager { runtime_dir: root.clone(), systemd_run: root.join("missing-systemd-run") };
        let mut mgr_stderr = Vec::new();
        let Spawn::Failed(err) = super::spawn_inner(
            &cmd,
            &log,
            SessionScope::Shared,
            Ok(mgr),
            |_| 259,
            |_, _| Err(io::Error::other("guard skipped")),
            &mut mgr_stderr,
        ) else {
            panic!("detected manager must never silently fall back to setsid");
        };
        assert!(err.to_string().contains("systemd-run:"), "{err}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_service_keeps_the_callers_paths_but_not_its_ssh_login() {
        let ssh = super::service_env(map(&[
            ("HC_SESSION", "dev"),
            ("PATH", "/home/u/.local/bin"),
            ("SSH_CONNECTION", "100.1.1.1 1 100.2.2.2 22"),
            ("SSH_AUTH_SOCK", "/tmp/ssh-x/agent.1"),
            ("INVOCATION_ID", "abc"),
            ("RUNTIME_DIRECTORY", "/run/user/1/hc-serve-12345678"),
            ("BASH_FUNC_f%%", "() { :; }"),
        ]));
        assert_eq!(ssh.keys().collect::<Vec<_>>(), ["HC_SESSION", "PATH"]);
        let desktop = super::service_env(map(&[("DISPLAY", ":0"), ("PATH", "/usr/bin")]));
        assert!(desktop.contains_key("DISPLAY"), "a desktop that doesn't tell systemd keeps its display");
    }

    #[test]
    fn a_service_command_line_holds_no_values_and_no_expansions() {
        let socket = OsStr::new("/run/${USER}.sock");
        let args = super::unit_args(
            "hc-hold-1",
            Path::new("/run/user/1/hc-hold-1/environment"),
            &["HC_SCOPE"],
            Path::new("/s/x.log"),
            Path::new("/"),
            259,
        );
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(!args.iter().any(|a| a.contains("secret-canary")), "{args:?}");
        for want in [
            "EnvironmentFile=/run/user/1/hc-hold-1/environment",
            "RuntimeDirectory=hc-hold-1",
            "RuntimeDirectoryMode=0700",
            "RuntimeDirectoryPreserve=restart",
            "UnsetEnvironment=HC_SCOPE",
            "OOMPolicy=continue",
            "WorkingDirectory=/",
            "--service-type=exec",
            "StandardError=append:/s/x.log",
        ] {
            assert!(args.iter().any(|a| a == want), "{want} missing from {args:?}");
        }
        assert_eq!(super::literal(socket), "/run/$${USER}.sock");
        assert!(!args.iter().any(|a| a.starts_with("ManagedOOM")), "uresourced loops on oomd's xattr: {args:?}");
    }

    #[test]
    fn el8_services_keep_the_callers_directory_without_newer_options() {
        let args = super::unit_args(
            "hc-serve-1",
            Path::new("/run/user/1/hc-serve-1/environment"),
            &[],
            Path::new("/s/x.log"),
            Path::new("/home/user/my project/a%hb"),
            239,
        );
        assert!(args.contains(&"WorkingDirectory=/home/user/my project/a%hb".into()));
        assert!(!args.iter().any(|a| a.to_string_lossy().contains("OOMPolicy")));
        assert!(!args.iter().any(|a| a == "--same-dir" || a.to_string_lossy().starts_with("--working-directory")));
        assert!(args.contains(&"--service-type=simple".into()));
        assert!(args.contains(&"StandardError=journal".into()));
        assert!(
            !args
                .iter()
                .any(|a| a == "--service-type=exec" || a.to_string_lossy().starts_with("StandardError=append:"))
        );
    }

    #[test]
    fn restart_unsets_each_environment_name_only_once() {
        for systemd_version in [239, 252] {
            let unit_arguments = super::unit_args(
                "hc-serve-1",
                Path::new("/run/user/1/hc-serve-1/environment"),
                &["SSH_CONNECTION", "HC_SCOPE", "SSH_CONNECTION", "HC_SCOPE"],
                Path::new("/s/x.log"),
                Path::new("/"),
                systemd_version,
            );
            assert!(unit_arguments.contains(&"UnsetEnvironment=HC_SCOPE SSH_CONNECTION".into()));
        }
    }

    #[test]
    fn newer_systemd_keeps_direct_exec_and_append_logging() {
        let mut core_command = Command::new("/bin/hc");
        core_command.args(["serve", "--socket", "/run/${USER}.sock"]);
        for systemd_version in [240, 243, 259] {
            let unit_arguments = super::unit_args(
                "hc-serve-1",
                Path::new("/run/user/1/hc-serve-1/environment"),
                &[],
                Path::new("/s/x.log"),
                Path::new("/"),
                systemd_version,
            );
            assert!(unit_arguments.contains(&"--service-type=exec".into()));
            assert!(unit_arguments.contains(&"StandardError=append:/s/x.log".into()));
            assert_eq!(unit_arguments.contains(&"OOMPolicy=continue".into()), systemd_version >= 243);
            assert_eq!(
                super::service_command_args(&core_command, Path::new("/s/x.log"), systemd_version),
                ["/bin/hc", "serve", "--socket", "/run/${USER}.sock"].map(std::ffi::OsString::from),
            );
        }
    }

    #[test]
    fn older_systemd_appends_logs_without_changing_the_main_pid() {
        use std::os::unix::fs::DirBuilderExt;
        let test_directory = crate::test_tmp().join(format!("hc-legacy-log-{}", super::super::random_hex()));
        std::fs::DirBuilder::new().mode(0o700).create(&test_directory).unwrap();
        let log_path = test_directory.join("a '$dollar`tick` %h.log");
        std::fs::write(&log_path, "previous log\n").unwrap();
        let literal_argument = "literal '$value`command` %h and spaces";
        let mut terminal_command = Command::new("/bin/sh");
        terminal_command.args(["-c", "printf '%s\\n%s\\n' \"$$\" \"$1\" >&2", "sh", literal_argument]);
        let command_arguments = super::service_command_args(&terminal_command, &log_path, 239);
        let terminal_process = Command::new(&command_arguments[0])
            .args(&command_arguments[1..])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let terminal_pid = terminal_process.id();
        let process_output = terminal_process.wait_with_output().unwrap();
        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        std::fs::remove_file(&log_path).unwrap();
        std::fs::remove_dir(&test_directory).unwrap();
        assert!(process_output.status.success(), "{process_output:?}");
        assert!(process_output.stderr.is_empty(), "stderr must go to the log: {process_output:?}");
        assert_eq!(log_contents, format!("previous log\n{terminal_pid}\n{literal_argument}\n"));
    }

    #[test]
    fn shutdown_stops_every_core_before_its_holders_even_after_a_restart() {
        let order =
            |command: &str, log: &str| super::shutdown_order(Command::new("/bin/hc").arg(command), Path::new(log));
        let first = order("serve", "/state/session/core.log");
        let next = order("serve", "/state/session/core.log");
        let holder = order("hold", "/state/session/holder.log");
        let guard = first.to_str().unwrap().strip_prefix("After=hc-shutdown-").expect("core after guard");
        assert_eq!(next, first, "new cores reuse the guard");
        assert_eq!(holder, OsStr::new(&format!("Before=hc-shutdown-{guard}")), "holder stops last");
        assert_ne!(order("serve", "/state/other/core.log"), first, "sessions stay separate");
    }

    #[test]
    fn environment_file_quotes_values_without_shell_expansion() {
        let env = map(&[
            ("EMPTY", ""),
            ("SECRET", " leading\tspaces\n\"quotes\" 'single' \\slash $dollar `ticks`\r\ntrailing "),
        ]);
        assert_eq!(
            super::environment_file(&env),
            "EMPTY=\"\"\nSECRET=\" leading\tspaces\n\\\"quotes\\\" 'single' \\\\slash \\$dollar \\`ticks\\`\r\ntrailing \"\n"
        );
    }

    #[test]
    fn environment_file_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let name = format!("hc-env-test-{}", super::super::random_hex());
        let dir = super::write_environment(&crate::test_tmp(), &name, &map(&[("API_KEY", "secret-canary")])).unwrap();
        let file = dir.join("environment");
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "API_KEY=\"secret-canary\"\n");
        assert!(super::write_environment(&crate::test_tmp(), &name, &BTreeMap::new()).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "API_KEY=\"secret-canary\"\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
