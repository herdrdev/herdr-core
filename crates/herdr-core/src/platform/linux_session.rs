use crate::paths::Paths;
use crate::rpc::RpcError;
#[cfg(target_os = "linux")]
use std::ffi::CStr;
use std::ffi::{CString, OsStr};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionScope {
    Isolated,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnitManager {
    System,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CgroupOwner {
    Service { manager: UnitManager, unit: String },
    Scope,
    Unscoped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ServiceSafety {
    Safe { kill_mode: String },
    Unsafe { kill_mode: String },
    Unverified { reason: String },
}

#[cfg(target_os = "linux")]
pub(super) const FALLBACK_TOOL_DIRS: &[&str] = &["/usr/bin", "/bin", "/run/current-system/sw/bin"];
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CAPTURE_BYTES: usize = 64 * 1024;

#[cfg(target_os = "linux")]
pub(super) fn default_runtime_dir() -> PathBuf {
    PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
}

#[cfg(target_os = "linux")]
pub(super) fn resolve_tool(name: &str) -> Option<PathBuf> {
    let fallback: Vec<&Path> = FALLBACK_TOOL_DIRS.iter().map(Path::new).collect();
    resolve_tool_in(name, std::env::var_os("PATH").as_deref(), &fallback)
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

fn resolve_tool_in(name: &str, path_env: Option<&OsStr>, fallback_dirs: &[&Path]) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if Path::new(name).components().count() > 1 {
        let direct = PathBuf::from(name);
        return is_executable_file(&direct).then_some(direct);
    }
    let path_dirs = path_env.into_iter().flat_map(std::env::split_paths).filter(|dir| !dir.as_os_str().is_empty());
    for dir in path_dirs.chain(fallback_dirs.iter().map(|dir| dir.to_path_buf())) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(target_os = "linux")]
pub(super) fn session_scope(paths: &Paths) -> SessionScope {
    let hc_session = std::env::var("HC_SESSION").ok();
    let xdg_state_home = std::env::var_os("XDG_STATE_HOME");
    let runtime_dir = default_runtime_dir();
    let pw_dir = passwd_home_dir();
    let env_home = std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    classify_session_scope(
        paths,
        hc_session.as_deref(),
        xdg_state_home.as_deref(),
        &runtime_dir,
        pw_dir.as_deref(),
        env_home.as_deref(),
    )
}

#[cfg(target_os = "linux")]
fn passwd_home_dir() -> Option<PathBuf> {
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0u8; 16384];
    let mut result = std::ptr::null_mut();
    let error = unsafe {
        libc::getpwuid_r(libc::getuid(), entry.as_mut_ptr(), buffer.as_mut_ptr().cast(), buffer.len(), &mut result)
    };
    if error != 0 || result.is_null() {
        return None;
    }
    let dir_ptr = unsafe { (*result).pw_dir };
    if dir_ptr.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(dir_ptr) }.to_bytes();
    (!bytes.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(bytes)))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push(comp.as_os_str());
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

fn make_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn canonicalize_with_tail(path: &Path) -> PathBuf {
    let abs = lexical_normalize(&make_absolute(path));
    let mut cursor = abs.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(canon) = std::fs::canonicalize(cursor) {
            let mut joined = canon;
            for part in tail.into_iter().rev() {
                joined.push(part);
            }
            return lexical_normalize(&joined);
        }
        let Some(last) = cursor.components().next_back() else {
            return abs;
        };
        let Some(parent) = cursor.parent() else {
            return abs;
        };
        tail.push(last.as_os_str().to_os_string());
        cursor = parent;
    }
}

fn path_starts_with_any(path: &Path, roots: &[PathBuf]) -> bool {
    let raw_path = lexical_normalize(&make_absolute(path));
    let canon_path = canonicalize_with_tail(path);
    roots.iter().any(|root| {
        let raw_root = lexical_normalize(&make_absolute(root));
        let canon_root = canonicalize_with_tail(root);
        raw_path.starts_with(&raw_root) || canon_path.starts_with(&canon_root)
    })
}

fn classify_session_scope(
    paths: &Paths,
    hc_session: Option<&str>,
    xdg_state_home: Option<&OsStr>,
    runtime_dir: &Path,
    pw_dir: Option<&Path>,
    env_home: Option<&Path>,
) -> SessionScope {
    let session = hc_session.map(str::trim).filter(|s| !s.is_empty());
    if session.is_none_or(|name| name == "default") || paths.state.file_name() == Some(OsStr::new("default")) {
        return SessionScope::Shared;
    }
    if path_starts_with_any(&paths.socket, &[runtime_dir.to_path_buf()]) {
        return SessionScope::Shared;
    }
    if xdg_state_home.is_none_or(OsStr::is_empty) {
        return SessionScope::Shared;
    }
    let shared_state_roots: Vec<PathBuf> = [pw_dir, env_home]
        .into_iter()
        .flatten()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|home| home.join(".local/state/herdr-core"))
        .collect();
    if path_starts_with_any(&paths.state, &shared_state_roots) {
        return SessionScope::Shared;
    }
    SessionScope::Isolated
}

fn is_user_manager_unit(seg: &str) -> bool {
    let Some(uid) = seg.strip_prefix("user@").and_then(|s| s.strip_suffix(".service")) else {
        return false;
    };
    !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit())
}

fn parse_cgroup_owner(proc_cgroup: &str) -> CgroupOwner {
    let mut systemd_v1 = None;
    let mut unified_v2 = None;
    for line in proc_cgroup.lines() {
        let line = line.trim_end_matches('\r');
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if controllers.split(',').any(|c| c == "name=systemd") {
            systemd_v1 = Some(path);
        } else if hierarchy == "0" && controllers.is_empty() && unified_v2.is_none() {
            unified_v2 = Some(path);
        }
    }
    let Some(raw_path) = systemd_v1.or(unified_v2) else {
        return CgroupOwner::Unscoped;
    };
    let clean_path = raw_path.strip_suffix(" (deleted)").unwrap_or(raw_path);
    let mut manager = UnitManager::System;
    let mut last_seg = None;
    let mut segments = clean_path.split('/').filter(|s| !s.is_empty()).peekable();
    while let Some(seg) = segments.next() {
        if is_user_manager_unit(seg) && segments.clone().any(|rest| rest != "init.scope") {
            manager = UnitManager::User;
            last_seg = Some(seg);
            continue;
        }
        if seg.ends_with(".service") {
            return CgroupOwner::Service { manager, unit: seg.to_owned() };
        }
        last_seg = Some(seg);
    }
    if last_seg.is_some_and(|seg| seg.ends_with(".scope")) { CgroupOwner::Scope } else { CgroupOwner::Unscoped }
}

fn parse_systemctl_show(stdout: &str) -> ServiceSafety {
    let mut load_state = None;
    let mut kill_mode = None;
    for line in stdout.lines() {
        let Some((key, value)) = line.trim_end_matches('\r').split_once('=') else {
            continue;
        };
        match key {
            "LoadState" => load_state = Some(value.trim()),
            "KillMode" => kill_mode = Some(value.trim()),
            _ => {}
        }
    }
    match load_state {
        None | Some("") => return ServiceSafety::Unverified { reason: "systemctl did not report LoadState".into() },
        Some("loaded") => {}
        Some(other) => return ServiceSafety::Unverified { reason: format!("unit LoadState is {other}") },
    }
    match kill_mode {
        Some("process") | Some("none") => ServiceSafety::Safe { kill_mode: kill_mode.unwrap().into() },
        Some("mixed") | Some("control-group") => ServiceSafety::Unsafe { kill_mode: kill_mode.unwrap().into() },
        Some(other) => ServiceSafety::Unverified { reason: format!("unrecognized KillMode={other}") },
        None => ServiceSafety::Unverified { reason: "systemctl did not report KillMode".into() },
    }
}

fn set_nonblocking(fd: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_pipe(pipe: &mut Option<impl Read>, buf: &mut Vec<u8>) -> io::Result<()> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => {
                *pipe = None;
                return Ok(());
            }
            Ok(n) => {
                if buf.len() < MAX_CAPTURE_BYTES {
                    let take = n.min(MAX_CAPTURE_BYTES - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn run_command_with_timeout(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    struct GroupGuard {
        child: Option<Child>,
        pgid: libc::pid_t,
    }
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
                let _ = child.wait();
            }
        }
    }

    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn()?;
    let pgid = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut guard = GroupGuard { child: Some(child), pgid };

    if let Some(ref out) = stdout {
        set_nonblocking(out.as_raw_fd())?;
    }
    if let Some(ref err) = stderr {
        set_nonblocking(err.as_raw_fd())?;
    }

    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let deadline = Instant::now() + timeout;

    loop {
        drain_pipe(&mut stdout, &mut out_buf)?;
        drain_pipe(&mut stderr, &mut err_buf)?;

        if let Some(status) = guard.child.as_mut().expect("child armed").try_wait()? {
            guard.child = None;
            let _ = drain_pipe(&mut stdout, &mut out_buf);
            let _ = drain_pipe(&mut stderr, &mut err_buf);
            return Ok(Output { status, stdout: out_buf, stderr: err_buf });
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            drop(guard);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("command timed out after {}ms", timeout.as_millis()),
            ));
        }

        let mut pollfds = Vec::with_capacity(2);
        if let Some(ref out) = stdout {
            pollfds.push(libc::pollfd { fd: out.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        }
        if let Some(ref err) = stderr {
            pollfds.push(libc::pollfd { fd: err.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        }
        if pollfds.is_empty() {
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
            continue;
        }
        let wait_ms = remaining.as_millis().clamp(1, 20) as libc::c_int;
        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, wait_ms) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

fn check_autostart_with(
    scope: SessionScope,
    systemd_booted: bool,
    has_user_manager: impl FnOnce() -> Result<(), String>,
    read_cgroup: impl FnOnce() -> io::Result<String>,
    query_service: impl FnOnce(&UnitManager, &str) -> ServiceSafety,
) -> Result<(), RpcError> {
    const HINT: &str = "enable systemd --user (with loginctl enable-linger and standard XDG_RUNTIME_DIR), start `hc server start` from a user login or terminal session outside this .service, or configure KillMode=process on the unit";

    if scope == SessionScope::Isolated || !systemd_booted {
        return Ok(());
    }
    let no_mgr_reason = match has_user_manager() {
        Ok(()) => return Ok(()),
        Err(reason) => reason,
    };
    let cgroup = match read_cgroup() {
        Ok(contents) => contents,
        Err(e) => {
            return Err(RpcError::new(
                "unavailable",
                format!(
                    "refusing to start or restart shared hc core without systemd --user ({no_mgr_reason}): could not read /proc/self/cgroup ({e})"
                ),
                HINT,
            ));
        }
    };
    let CgroupOwner::Service { manager, unit } = parse_cgroup_owner(&cgroup) else {
        return Ok(());
    };
    let scope_label = match manager {
        UnitManager::System => "system",
        UnitManager::User => "user",
    };
    match query_service(&manager, &unit) {
        ServiceSafety::Safe { .. } => Ok(()),
        ServiceSafety::Unsafe { kill_mode } => Err(RpcError::new(
            "unavailable",
            format!(
                "refusing to start or restart shared hc core inside systemd {scope_label} unit {unit} with KillMode={kill_mode}: stopping or restarting {unit} would SIGKILL hc and live terminals; systemd --user fallback is unavailable ({no_mgr_reason})"
            ),
            HINT,
        )),
        ServiceSafety::Unverified { reason } => Err(RpcError::new(
            "unavailable",
            format!(
                "refusing to start or restart shared hc core inside systemd {scope_label} unit {unit}: could not verify KillMode ({reason}); systemd --user fallback is unavailable ({no_mgr_reason})"
            ),
            HINT,
        )),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn check_autostart(paths: &Paths) -> Result<(), RpcError> {
    check_autostart_with(
        session_scope(paths),
        Path::new("/run/systemd/system").is_dir(),
        || super::systemd::user_service_runner().map(|_| ()).map_err(|e| e.to_string()),
        || std::fs::read_to_string("/proc/self/cgroup"),
        query_service_safety,
    )
}

#[cfg(target_os = "linux")]
fn query_service_safety(manager: &UnitManager, unit: &str) -> ServiceSafety {
    let Some(systemctl) = resolve_tool("systemctl") else {
        return ServiceSafety::Unverified { reason: "systemctl not found on PATH or standard directories".into() };
    };
    let mut cmd = Command::new(systemctl);
    match manager {
        UnitManager::System => {
            cmd.arg("--system");
        }
        UnitManager::User => {
            cmd.args(["--user"]).env("XDG_RUNTIME_DIR", default_runtime_dir());
        }
    }
    cmd.args(["show", "-p", "LoadState", "-p", "KillMode", "--", unit]);
    match run_command_with_timeout(cmd, SYSTEMCTL_TIMEOUT) {
        Ok(out) if out.status.success() => parse_systemctl_show(&String::from_utf8_lossy(&out.stdout)),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr.trim();
            let reason = if detail.is_empty() {
                format!("systemctl show exited with {}", out.status)
            } else {
                format!("systemctl show failed: {detail}")
            };
            ServiceSafety::Unverified { reason }
        }
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            ServiceSafety::Unverified { reason: format!("systemctl timed out after {}s", SYSTEMCTL_TIMEOUT.as_secs()) }
        }
        Err(e) => ServiceSafety::Unverified { reason: format!("failed to run systemctl: {e}") },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct TestDir(PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_dir(name: &str) -> TestDir {
        let dir = crate::test_tmp().join(format!("hc-linux-session-{name}-{}", super::super::random_hex()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }

    fn make_paths(root: &Path, session: &str) -> Paths {
        Paths {
            socket: root.join("run/herdr-core").join(format!("{session}.sock")),
            holder_socket: root.join("run/herdr-core").join(format!("{session}.holder.sock")),
            state: root.join("state/herdr-core").join(session),
            config: root.join("config/herdr-core/config.toml"),
        }
    }

    #[test]
    fn parses_cgroup_v2_v1_hybrid_deleted_and_user_manager_slices() {
        let svc = |manager, unit: &str| CgroupOwner::Service { manager, unit: unit.into() };
        for (raw, expected) in [
            ("0::/system.slice/conductor.service\n", svc(UnitManager::System, "conductor.service")),
            (
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/daemon.service\n",
                svc(UnitManager::User, "daemon.service"),
            ),
            ("0::/system.slice/foo.service/init.scope\n", svc(UnitManager::System, "foo.service")),
            (
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/foo.service/init.scope\n",
                svc(UnitManager::User, "foo.service"),
            ),
            ("0::/user.slice/user-1000.slice/user@1000.service\n", svc(UnitManager::System, "user@1000.service")),
            (
                "0::/user.slice/user-1000.slice/user@1000.service/init.scope\n",
                svc(UnitManager::System, "user@1000.service"),
            ),
            ("0::/user.slice/user-1000.slice/user@1000.service/app.slice/vte-spawn-12.scope\n", CgroupOwner::Scope),
            ("0::/user.slice/user-1000.slice/session-2.scope\n", CgroupOwner::Scope),
            ("0::/init.scope\n", CgroupOwner::Scope),
            ("0::/\n", CgroupOwner::Unscoped),
            ("0::/user.slice/user-1000.slice\n", CgroupOwner::Unscoped),
            ("0::/docker/0123456789abcdef\n", CgroupOwner::Unscoped),
            ("", CgroupOwner::Unscoped),
            ("1:name=systemd:/system.slice/docker.service\n", svc(UnitManager::System, "docker.service")),
            (
                "0::/\n12:memory:/system.slice/ignored.service\n1:name=systemd:/user.slice/user-1000.slice/user@1000.service/app.slice/my.service\n",
                svc(UnitManager::User, "my.service"),
            ),
            (
                "0::/system.slice/other.service\n1:cpu,name=systemd:/user.slice/user-1000.slice/session-1.scope\n",
                CgroupOwner::Scope,
            ),
            ("0::/system.slice/foo.service (deleted)\n", svc(UnitManager::System, "foo.service")),
            ("0::/user.slice/user-1000.slice/session-5.scope (deleted)\n", CgroupOwner::Scope),
            ("0::/system.slice/foo:bar.service\n", svc(UnitManager::System, "foo:bar.service")),
        ] {
            assert_eq!(parse_cgroup_owner(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn parses_systemctl_show_load_state_and_kill_mode() {
        for mode in ["process", "none"] {
            assert_eq!(
                parse_systemctl_show(&format!("LoadState=loaded\nKillMode={mode}\n")),
                ServiceSafety::Safe { kill_mode: mode.into() }
            );
        }
        assert_eq!(
            parse_systemctl_show("KillMode=mixed\nLoadState=loaded\n"),
            ServiceSafety::Unsafe { kill_mode: "mixed".into() }
        );
        assert_eq!(
            parse_systemctl_show("LoadState=loaded\nKillMode=control-group\n"),
            ServiceSafety::Unsafe { kill_mode: "control-group".into() }
        );
        for unverified in [
            "LoadState=not-found\nKillMode=process\n",
            "LoadState=loaded\n",
            "KillMode=process\n",
            "LoadState=loaded\nKillMode=unknown\n",
        ] {
            assert!(matches!(parse_systemctl_show(unverified), ServiceSafety::Unverified { .. }), "{unverified:?}");
        }
    }

    #[test]
    fn session_scope_distinguishes_isolated_tests_from_shared_or_custom_runtime_dir_daemons() {
        let dir = test_dir("scope");
        let root = &dir.0;
        let run_user = root.join("run_user_1000");
        let pw_home = root.join("pw_home");
        let env_home = root.join("env_home");
        std::fs::create_dir_all(&run_user).unwrap();
        std::fs::create_dir_all(pw_home.join(".local/state/herdr-core")).unwrap();
        std::fs::create_dir_all(env_home.join(".local/state/herdr-core")).unwrap();

        let scope = |paths: &Paths, sess: Option<&str>, state_env: Option<&OsStr>| {
            classify_session_scope(paths, sess, state_env, &run_user, Some(&pw_home), Some(&env_home))
        };
        let state_tmp = root.join("state");
        let isolated = make_paths(root, "test");
        assert_eq!(scope(&isolated, Some("test"), Some(state_tmp.as_os_str())), SessionScope::Isolated);

        let default_name = make_paths(root, "default");
        assert_eq!(scope(&default_name, Some("default"), Some(state_tmp.as_os_str())), SessionScope::Shared);
        assert_eq!(scope(&isolated, None, Some(state_tmp.as_os_str())), SessionScope::Shared);
        assert_eq!(scope(&isolated, Some("   "), Some(state_tmp.as_os_str())), SessionScope::Shared);

        let socket_in_run_user = Paths {
            socket: run_user.join("herdr-core/test.sock"),
            holder_socket: run_user.join("herdr-core/test.holder.sock"),
            ..make_paths(root, "test")
        };
        assert_eq!(scope(&socket_in_run_user, Some("test"), Some(state_tmp.as_os_str())), SessionScope::Shared);

        let symlink_run = root.join("run_link");
        std::os::unix::fs::symlink(&run_user, &symlink_run).unwrap();
        let socket_via_link = Paths {
            socket: symlink_run.join("herdr-core/test.sock"),
            holder_socket: symlink_run.join("herdr-core/test.holder.sock"),
            ..make_paths(root, "test")
        };
        assert_eq!(scope(&socket_via_link, Some("test"), Some(state_tmp.as_os_str())), SessionScope::Shared);

        for empty in [None, Some(OsStr::new(""))] {
            assert_eq!(scope(&isolated, Some("test"), empty), SessionScope::Shared);
        }

        for home in [&env_home, &pw_home] {
            let paths = Paths { state: home.join(".local/state/herdr-core/custom"), ..make_paths(root, "custom") };
            assert_eq!(
                scope(&paths, Some("custom"), Some(home.join(".local/state").as_os_str())),
                SessionScope::Shared
            );
        }

        let state_link = root.join("state_link");
        std::os::unix::fs::symlink(pw_home.join(".local/state/herdr-core"), &state_link).unwrap();
        let via_state_link = Paths { state: state_link.join("custom"), ..isolated };
        assert_eq!(scope(&via_state_link, Some("custom"), Some(state_link.as_os_str())), SessionScope::Shared);
    }

    #[test]
    fn resolve_tool_prefers_path_uses_fallbacks_and_skips_non_executables() {
        let dir = test_dir("tools");
        let root = &dir.0;
        let (path_a, path_b, fallback) = (root.join("path_a"), root.join("path_b"), root.join("fallback"));
        for d in [&path_a, &path_b, &fallback] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(path_a.join("mytool")).unwrap();
        std::fs::write(path_b.join("mytool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path_b.join("mytool"), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(fallback.join("mytool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(fallback.join("mytool"), std::fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = std::env::join_paths([&path_a, &path_b]).unwrap();
        assert_eq!(resolve_tool_in("mytool", Some(&path_env), &[fallback.as_path()]), Some(fallback.join("mytool")));

        std::fs::set_permissions(path_b.join("mytool"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve_tool_in("mytool", Some(&path_env), &[fallback.as_path()]), Some(path_b.join("mytool")));
        assert_eq!(resolve_tool_in("mytool", None, &[fallback.as_path()]), Some(fallback.join("mytool")));
        assert_eq!(resolve_tool_in("absent-tool", Some(&path_env), &[fallback.as_path()]), None);
    }

    #[test]
    fn command_timeout_kills_process_group_and_orphans_after_one_second() {
        let dir = test_dir("timeout");
        let root = &dir.0;
        let (child_pid_file, orphan_pid_file, script) =
            (root.join("child.pid"), root.join("orphan.pid"), root.join("hang.sh"));
        std::fs::write(&script, "#!/bin/sh\n/bin/sleep 30 &\necho $$ > \"$1\"\necho $! > \"$2\"\nexec /bin/sleep 30\n")
            .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut fast = Command::new("/bin/sh");
        fast.args(["-c", "printf 'LoadState=loaded\\nKillMode=process\\n'"]);
        let out = run_command_with_timeout(fast, SYSTEMCTL_TIMEOUT).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "LoadState=loaded\nKillMode=process\n");

        let mut hang = Command::new(&script);
        hang.arg(&child_pid_file).arg(&orphan_pid_file);
        let start = Instant::now();
        let err = run_command_with_timeout(hang, SYSTEMCTL_TIMEOUT).unwrap_err();
        let elapsed = start.elapsed();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(3), "{elapsed:?}");

        let child_pid: i32 = std::fs::read_to_string(&child_pid_file).unwrap().trim().parse().unwrap();
        let orphan_pid: i32 = std::fs::read_to_string(&orphan_pid_file).unwrap().trim().parse().unwrap();
        assert_eq!(unsafe { libc::kill(child_pid, 0) }, -1, "timed out child leaked");
        let deadline = Instant::now() + Duration::from_millis(500);
        while unsafe { libc::kill(orphan_pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "timed out process-group orphan leaked");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn autostart_decision_enforces_cgroup_policy_with_observed_facts_only() {
        let mixed = || ServiceSafety::Unsafe { kill_mode: "mixed".into() };
        let svc = "0::/system.slice/app-svc.service\n";
        let no_mgr = "XDG_RUNTIME_DIR is /tmp/run instead of /run/user/1000";
        let run = |scope, booted, mgr: Result<(), &str>, cgroup: &str, safety: ServiceSafety| {
            check_autostart_with(scope, booted, || mgr.map_err(str::to_string), || Ok(cgroup.into()), |_, _| safety)
        };

        assert!(run(SessionScope::Isolated, true, Err(no_mgr), svc, mixed()).is_ok());
        assert!(
            run(SessionScope::Shared, false, Err(no_mgr), "1:name=systemd:/system.slice/docker.service\n", mixed())
                .is_ok()
        );
        assert!(run(SessionScope::Shared, true, Ok(()), svc, mixed()).is_ok());
        for cgroup in ["0::/user.slice/user-1000.slice/session-1.scope\n", "0::/\n"] {
            assert!(run(SessionScope::Shared, true, Err(no_mgr), cgroup, mixed()).is_ok());
        }
        for safe_mode in ["process", "none"] {
            assert!(
                run(SessionScope::Shared, true, Err(no_mgr), svc, ServiceSafety::Safe { kill_mode: safe_mode.into() })
                    .is_ok()
            );
        }
        assert!(
            check_autostart_with(
                SessionScope::Shared,
                true,
                || Err(no_mgr.into()),
                || Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
                |_, _| mixed()
            )
            .is_err()
        );

        for safety in [
            ServiceSafety::Unsafe { kill_mode: "mixed".into() },
            ServiceSafety::Unsafe { kill_mode: "control-group".into() },
            ServiceSafety::Unverified { reason: "systemctl timed out after 1s".into() },
        ] {
            let err = run(SessionScope::Shared, true, Err(no_mgr), svc, safety.clone()).unwrap_err();
            assert_eq!(err.kind, "unavailable");
            assert!(err.message.contains("app-svc.service"), "{}", err.message);
            assert!(err.message.contains(no_mgr), "{}", err.message);
            match &safety {
                ServiceSafety::Unsafe { kill_mode } => {
                    assert!(err.message.contains(&format!("KillMode={kill_mode}")), "{}", err.message)
                }
                ServiceSafety::Unverified { reason } => assert!(err.message.contains(reason), "{}", err.message),
                ServiceSafety::Safe { .. } => unreachable!(),
            }
            for remedy in ["systemd --user", "linger", "hc server start", "KillMode=process"] {
                assert!(err.hint.contains(remedy), "missing remedy {remedy:?} in {}", err.hint);
            }
        }
    }
}
