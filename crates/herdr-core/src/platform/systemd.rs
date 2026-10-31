//! Linux: cores and holders run as systemd user services. They belong to the
//! user, not to whatever started them: a desktop crash, a logout or an ssh
//! login ending can't take them along, and an ssh login's environment never
//! becomes theirs.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    let out = Command::new("systemd-run")
        .env_clear()
        .envs(&env)
        .args(unit_args(&name, &dir.join("environment"), &unset, log, &here, version))
        .args(guarded.then(|| shutdown_order(cmd, log)).into_iter().flat_map(|order| [OsString::from("-p"), order]))
        .arg(literal(cmd.get_program()))
        .args(cmd.get_args().map(literal))
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
    let mut out: Vec<OsString> = ["--user", "--quiet", "--collect", "--service-type=exec"].map(OsString::from).into();
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
        format!("StandardError=append:{}", log.display()),
        format!("UnsetEnvironment={}", unset.join(" ")),
    ] {
        if !p.starts_with("--") {
            out.push("-p".into());
        }
        out.push(p.into());
    }
    out
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
    let enabled = Command::new("loginctl")
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
}

/// The user manager's environment now (a core may have started before the
/// desktop did). Blocks up to a second: call it off the core's thread.
pub fn manager_env() -> Option<BTreeMap<String, String>> {
    let run = manager().ok()?;
    let out = Command::new("busctl")
        .env("XDG_RUNTIME_DIR", run)
        .args(["--user", "--json=short", "--timeout=1", "get-property", "org.freedesktop.systemd1"])
        .args(["/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager", "Environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let shown: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let pairs = shown["data"].as_array()?.iter().filter_map(|v| v.as_str()?.split_once('='));
    Some(pairs.map(|(k, v)| (k.into(), v.into())).collect())
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
    fn programs_get_the_desktop_the_manager_has_now() {
        let mut env = map(&[("SSH_AUTH_SOCK", "/tmp/ssh-x/agent.1"), ("DISPLAY", ":1"), ("PATH", "/usr/bin")]);
        super::apply_session(&mut env, &map(&[("PATH", "/x"), ("SSH_AUTH_SOCK", "/run/user/1/my keys/ssh")]));
        assert_eq!(env["SSH_AUTH_SOCK"], "/run/user/1/my keys/ssh");
        assert_eq!((env["PATH"].as_str(), env["DISPLAY"].as_str()), ("/usr/bin", ":1"), "only what the manager has");
    }
}
