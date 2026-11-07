//! Linux: cores and holders run as systemd user services. They belong to the
//! user, not to whatever started them: a desktop crash, a logout or an ssh
//! login ending can't take them along, and an ssh login's environment never
//! becomes theirs.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::iter::Peekable;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::Bytes;

/// What the login session decides. Programs get it from the user manager as
/// it is now; a service takes the caller's only when the caller isn't an ssh
/// login (desktops that don't share theirs with systemd: i3, startx).
const SESSION: &[&str] = &[
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

/// The user's runtime dir when this process uses it and a user manager runs
/// there. Err(true): the default runtime dir but no manager (warn); Err(false):
/// a custom runtime dir (isolated sessions, tests), which stays on its own.
fn manager() -> Result<PathBuf, bool> {
    let run = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
    let ours = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map_or_else(|| run.clone(), PathBuf::from);
    match (ours == run, run.join("systemd/private").exists()) {
        (true, true) => Ok(run),
        (default, _) => Err(default),
    }
}

const NO_MANAGER: &str = "hc: no systemd user manager here: the core ends with whatever started it";

/// Starts `cmd` as a user service with stderr appended to `log`. None: no
/// user manager here, start it the plain way.
pub fn spawn(cmd: &Command, log: &Path) -> Option<io::Result<()>> {
    let run = match manager() {
        Ok(run) => run,
        Err(warn) => {
            if warn {
                eprintln!("{NO_MANAGER}");
            }
            return None;
        }
    };
    let guarded = match ensure_shutdown_guard(&run, log) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("hc: {e}; starting without shutdown ordering");
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
    env.insert("XDG_RUNTIME_DIR".into(), run.display().to_string());
    let args: Vec<&OsStr> = cmd.get_args().collect();
    let name = format!("hc-{}-{}", args.first().and_then(|a| a.to_str()).unwrap_or("run"), &super::random_hex()[..8]);
    let here = std::env::current_dir().ok().filter(|d| d.is_dir()).unwrap_or_else(|| PathBuf::from("/"));
    static VERSION: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let version = *VERSION.get_or_init(|| {
        Command::new("systemd-run")
            .arg("--version")
            .output()
            .ok()
            .and_then(|out| String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1)?.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    let dir = match write_environment(&run, &name, &env) {
        Ok(dir) => dir,
        Err(e) => return Some(Err(e)),
    };
    let command_arguments = service_command_args(cmd, log, version);
    let out = Command::new("systemd-run")
        .env_clear()
        .envs(&env)
        .args(unit_args(&name, &dir.join("environment"), &unset, log, &here, version))
        .args(guarded.then(|| shutdown_order(cmd, log)).into_iter().flat_map(|order| [OsString::from("-p"), order]))
        .args(command_arguments.iter().map(|argument| literal(argument)))
        .stdin(Stdio::null())
        .output();
    let started = match out {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(io::Error::other(format!("systemd-run: {}", String::from_utf8_lossy(&out.stderr).trim()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("{NO_MANAGER} (no systemd-run on PATH)");
            return None;
        }
        Err(e) => Err(io::Error::new(e.kind(), format!("systemd-run: {e}"))),
    };
    if started.is_ok() {
        linger();
    } else {
        let _ = std::fs::remove_dir_all(dir);
    }
    Some(started)
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

fn ensure_shutdown_guard(run: &Path, log: &Path) -> io::Result<()> {
    let name = shutdown_guard(log);
    let start = || {
        Command::new("systemctl")
            .env("XDG_RUNTIME_DIR", run)
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
    let out = Command::new("systemd-run")
        .env("XDG_RUNTIME_DIR", run)
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
fn service_env(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
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

/// Without linger the user manager, and every hc service with it, stops when
/// the user's last login ends.
fn linger() {
    let user = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return;
        }
        std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
    };
    if Path::new("/var/lib/systemd/linger").join(&user).exists() {
        return;
    }
    // Polkit can leave loginctl waiting for 25 seconds, longer than a
    // holder's handshake deadline. Try once per process, off the startup path.
    static ATTEMPT: std::sync::Once = std::sync::Once::new();
    ATTEMPT.call_once(|| {
        let _ = enable_linger(Command::new("loginctl"), user);
    });
}

fn enable_linger(mut command: Command, user: String) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("hc-linger".into()).spawn(move || {
        let enabled = command
            .args(["--no-ask-password", "enable-linger", &user])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if enabled {
            eprintln!("hc: turned on linger for {user}, so the core outlives your logins");
        } else {
            eprintln!("hc: the core stops when your last login ends; `sudo loginctl enable-linger {user}` keeps it");
        }
    })
}

/// The user manager's environment now (a core may have started before the
/// desktop did). Blocks up to a second: call it off the core's thread.
pub fn manager_env() -> Option<BTreeMap<String, String>> {
    let run = manager().ok()?;
    let out = Command::new("busctl")
        .env("XDG_RUNTIME_DIR", run)
        .args(["--user", "--timeout=1", "get-property", "org.freedesktop.systemd1"])
        .args(["/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager", "Environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    parse_busctl_environment(&out.stdout)
}

fn parse_busctl_environment(reply_bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let reply_text = std::str::from_utf8(reply_bytes).ok()?;
    let mut header_fields = reply_text.trim().splitn(3, ' ');
    let signature = header_fields.next()?;
    if signature != "as" {
        return None;
    }
    let environment_entry_count = header_fields.next()?.parse::<usize>().ok()?;
    let mut remaining_reply = header_fields.next().unwrap_or_default().bytes().peekable();
    let mut environment = BTreeMap::new();
    for _ in 0..environment_entry_count {
        let environment_entry = parse_busctl_quoted_string(&mut remaining_reply)?;
        if let Some((variable_name, variable_value)) = environment_entry.split_once('=') {
            environment.insert(variable_name.into(), variable_value.into());
        }
    }
    if remaining_reply.any(|byte| !byte.is_ascii_whitespace()) {
        return None;
    }
    Some(environment)
}

fn parse_busctl_quoted_string(remaining_reply: &mut Peekable<Bytes<'_>>) -> Option<String> {
    while remaining_reply.peek().is_some_and(u8::is_ascii_whitespace) {
        remaining_reply.next();
    }
    if remaining_reply.next()? != b'"' {
        return None;
    }
    let mut decoded_bytes = Vec::new();
    loop {
        match remaining_reply.next()? {
            b'"' => break,
            b'\\' => decoded_bytes.push(decode_busctl_escape(remaining_reply)?),
            byte if byte.is_ascii_control() => return None,
            byte => decoded_bytes.push(byte),
        }
    }
    let has_invalid_delimiter = remaining_reply.peek().is_some_and(|byte| !byte.is_ascii_whitespace());
    if has_invalid_delimiter {
        return None;
    }
    // Octal escapes can encode individual bytes of a multibyte UTF-8 character.
    String::from_utf8(decoded_bytes).ok()
}

fn decode_busctl_escape(remaining_reply: &mut Peekable<Bytes<'_>>) -> Option<u8> {
    let escape_code = remaining_reply.next()?;
    let decoded_byte = match escape_code {
        b'a' => b'\x07',
        b'b' => b'\x08',
        b'f' => b'\x0c',
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'v' => b'\x0b',
        b'\\' => b'\\',
        b'\'' => b'\'',
        b'"' => b'"',
        b'0'..=b'3' => {
            let first_octal_digit = escape_code - b'0';
            let second_octal_digit = remaining_reply.next()?.checked_sub(b'0')?;
            let third_octal_digit = remaining_reply.next()?.checked_sub(b'0')?;
            if second_octal_digit > 7 || third_octal_digit > 7 {
                return None;
            }
            first_octal_digit * 64 + second_octal_digit * 8 + third_octal_digit
        }
        _ => return None,
    };
    if decoded_byte == b'\0' {
        return None;
    }
    Some(decoded_byte)
}

/// Puts the login session's variables the manager has into `env`.
pub fn apply_session(env: &mut BTreeMap<String, String>, manager: &BTreeMap<String, String>) {
    for &k in SESSION {
        if let Some(v) = manager.get(k) {
            env.insert(k.into(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::process::Command;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn linger_does_not_wait_for_loginctl() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 1"]);
        let start = std::time::Instant::now();
        let worker = super::enable_linger(command, "hc-test".into()).unwrap();
        let elapsed = start.elapsed();
        worker.join().unwrap();
        assert!(elapsed < std::time::Duration::from_millis(500), "loginctl blocked startup");
    }

    #[test]
    fn missing_loginctl_is_best_effort() {
        let worker = super::enable_linger(Command::new("/no-such-hc-test-loginctl"), "hc-test".into()).unwrap();
        worker.join().unwrap();
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
        let test_directory = Path::new("/var/tmp").join(format!("hc-legacy-log-{}", super::super::random_hex()));
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
        let dir =
            super::write_environment(Path::new("/var/tmp"), &name, &map(&[("API_KEY", "secret-canary")])).unwrap();
        let file = dir.join("environment");
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "API_KEY=\"secret-canary\"\n");
        assert!(super::write_environment(Path::new("/var/tmp"), &name, &BTreeMap::new()).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "API_KEY=\"secret-canary\"\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn manager_environment_decodes_busctl_strings_without_evaluating_values() {
        let busctl_reply = br#"as 6 "EMPTY=" "PATH=/home/u/my tools" "SPECIAL=\"double\" \'single\' \\slash $dollar `ticks`=tail" "LINES=one\ntwo\r\tend" "UTF8=\303\251\342\230\203" "CONTROLS=\a\b\f\v\177""#;
        let expected_environment = map(&[
            ("EMPTY", ""),
            ("PATH", "/home/u/my tools"),
            ("SPECIAL", "\"double\" 'single' \\slash $dollar `ticks`=tail"),
            ("LINES", "one\ntwo\r\tend"),
            ("UTF8", "é☃"),
            ("CONTROLS", "\x07\x08\x0c\x0b\x7f"),
        ]);
        let parsed_environment = super::parse_busctl_environment(busctl_reply);
        assert_eq!(parsed_environment, Some(expected_environment));
        assert_eq!(super::parse_busctl_environment(b"as 0\n"), Some(BTreeMap::new()));
        let raw_utf8_reply = "as 1 \"UTF8=é☃\"".as_bytes();
        assert_eq!(super::parse_busctl_environment(raw_utf8_reply), Some(map(&[("UTF8", "é☃")])));
    }

    #[test]
    fn manager_environment_rejects_malformed_or_partial_replies() {
        for malformed_reply in [
            &b"s \"KEY=value\""[..],
            b"as nope",
            b"as 1",
            b"as 1 \"KEY=unterminated",
            b"as 2 \"KEY=only-one\"",
            b"as 0 \"KEY=extra\"",
            b"as 2 \"A=1\"\"B=2\"",
            b"as 1 \"KEY=\\q\"",
            b"as 1 \"KEY=\\40\"",
            b"as 1 \"KEY=\\400\"",
            b"as 1 \"KEY=\\300\"",
            b"as 1 \"KEY=\\000\"",
        ] {
            assert_eq!(super::parse_busctl_environment(malformed_reply), None, "{malformed_reply:?}");
        }
    }

    #[test]
    fn programs_get_the_desktop_the_manager_has_now() {
        let mut env = map(&[("SSH_AUTH_SOCK", "/tmp/ssh-x/agent.1"), ("DISPLAY", ":1"), ("PATH", "/usr/bin")]);
        super::apply_session(&mut env, &map(&[("PATH", "/x"), ("SSH_AUTH_SOCK", "/run/user/1/my keys/ssh")]));
        assert_eq!(env["SSH_AUTH_SOCK"], "/run/user/1/my keys/ssh");
        assert_eq!((env["PATH"].as_str(), env["DISPLAY"].as_str()), ("/usr/bin", ":1"), "only what the manager has");
    }
}
