//! What differs between Unix (Linux, macOS) and Windows, behind one small
//! surface: connections, terminals, process trees, the console, files.
//! Everything above it is shared.

mod session;
pub use session::{LoginSession, holder_current};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(target_os = "linux")]
mod linux_login;
#[cfg(target_os = "linux")]
pub mod systemd;

/// Only a new SSH-started Linux core imports login settings; holders inherit the core.
#[allow(unused_variables)]
pub fn core_environment(cmd: &mut std::process::Command, state: &std::path::Path) {
    #[cfg(target_os = "linux")]
    linux_login::apply(cmd, state);
}

#[cfg(target_os = "macos")]
mod macos_appkit;
#[cfg(target_os = "macos")]
mod macos_bundle;
#[cfg(target_os = "macos")]
mod macos_launchd;
#[cfg(target_os = "macos")]
mod macos_responsibility;
#[cfg(target_os = "macos")]
mod macos_session;
#[cfg(target_os = "macos")]
mod macos_shutdown;

/// Whether this core's systemd user manager stays alive after the last logout,
/// and the account an administrator can enable lingering for.
pub fn logout_linger() -> (Option<bool>, Option<String>) {
    #[cfg(target_os = "linux")]
    {
        systemd::logout_linger()
    }
    #[cfg(not(target_os = "linux"))]
    {
        (None, None)
    }
}

/// Refresh logout survival away from request handling.
pub fn start_logout_linger() {
    #[cfg(target_os = "linux")]
    systemd::start_logout_linger();
}

/// Whether foreground observations can distinguish a shell from its jobs.
pub fn foreground_tracks_jobs() -> bool {
    cfg!(unix)
}

/// Run before threads or runtime setup: a launchd launcher forks the core here.
pub fn daemon_startup() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_bundle::installer_startup();
        macos_responsibility::holder_startup();
        macos_launchd::startup()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// A parsed serve may have come from an older updater's direct spawn.
pub fn prepare_core() {
    #[cfg(target_os = "macos")]
    macos_launchd::prepare_core();
}

/// Complete the holder's background-app registration before starting threads or terminals.
pub fn prepare_holder() {
    #[cfg(target_os = "macos")]
    if let Err(error) = macos_appkit::check_in() {
        eprintln!("holder: background app registration unavailable: {error}");
    }
}

pub fn login_session() -> Option<LoginSession> {
    #[cfg(target_os = "macos")]
    {
        macos_session::current()
    }
    #[cfg(not(target_os = "macos"))]
    None
}

pub fn connect_for_app(paths: &crate::paths::Paths) -> Result<Stream, crate::rpc::RpcError> {
    #[cfg(target_os = "macos")]
    {
        macos_session::connect(paths)
    }
    #[cfg(not(target_os = "macos"))]
    crate::client::connect_or_start(paths)
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn holder_can_start_core(session: Option<LoginSession>) -> bool {
    #[cfg(target_os = "macos")]
    if let Some(id) = macos_session::graphic_session()
        .ok()
        .flatten()
        .or_else(|| session.filter(|s| s.gui == Some(true)).map(|s| s.id))
    {
        return macos_appkit::gui_session_usable(id);
    }
    true
}

/// Daemons must not keep launchd's 256-fd or systemd's 1024-fd soft limit.
pub(crate) fn raise_nofile_limit() {
    #[cfg(unix)]
    if let Err(error) = unix::raise_nofile_limit() {
        eprintln!("cannot raise open file limit; using inherited limit: {error}");
    }
}

pub fn core_executable(exe: &std::path::Path) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        macos_bundle::core_executable(exe)
    }
    #[cfg(not(target_os = "macos"))]
    exe.to_path_buf()
}

pub fn prefer_release_bundle() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::env::current_exe().is_ok_and(|exe| macos_bundle::bundle_destination(&exe))
    }
    #[cfg(not(target_os = "macos"))]
    true
}

pub fn needs_bundle_update() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_bundle::needs_bundle_update()
    }
    #[cfg(not(target_os = "macos"))]
    false
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn legacy_bundle_notice(state: &std::path::Path) {
    #[cfg(target_os = "macos")]
    macos_bundle::legacy_notice(state);
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn verify_release_bundle(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    if macos_bundle::source_is_bundle(path) {
        let local = std::env::var("HERDR_RELEASES_URL")
            .is_ok_and(|url| url.trim_end_matches('/') != "https://releases.herdr.dev");
        macos_bundle::verify(path.parent().unwrap().parent().unwrap().parent().unwrap(), local)?;
    }
    Ok(())
}

/// macOS cores leave the app's process coalition; holders inherit the core's.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn spawn_core_job(cmd: &mut std::process::Command, log: &std::path::Path) -> bool {
    #[cfg(target_os = "macos")]
    if cmd.get_args().next() == Some(std::ffi::OsStr::new("serve")) {
        return macos_launchd::spawn(cmd, log);
    }
    false
}

pub fn update_source(path: &std::path::Path) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    if path.is_dir() {
        return path.join(macos_bundle::EXECUTABLE);
    }
    path.to_path_buf()
}

pub fn bundle_update_hint() -> Option<&'static str> {
    cfg!(target_os = "macos")
        .then_some("use hc update, or --from 'Herdr Core.app' (its Contents/MacOS/herdr-core also works)")
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn privacy_upgrade_notice(previous_exe: &std::path::Path) -> &'static str {
    #[cfg(target_os = "macos")]
    if !macos_bundle::source_is_bundle(previous_exe) {
        return "; recreate terminals started before the Herdr Core.app upgrade if macOS privacy access is broken (they were not stopped)";
    }
    ""
}

/// Keep the early shutdown watch alive for the core's lifetime.
pub fn monitor_host_shutdown() -> impl Sized {
    #[cfg(target_os = "macos")]
    {
        macos_shutdown::monitor()
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
#[cfg(windows)]
mod windows_exec;
#[cfg(windows)]
pub use windows_exec::*;
#[cfg(windows)]
mod windows_env;
#[cfg(windows)]
mod windows_foreground;
#[cfg(windows)]
mod windows_session;

/// Whether the host is shutting down (Windows also includes logout).
pub fn host_shutdown_in_progress() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
        // Keep USER32 statically linked: first loading it during shutdown can interrupt exit recording.
        unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
    }
    #[cfg(target_os = "macos")]
    {
        macos_shutdown::in_progress()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// What marks a process as an ssh login's.
pub const SSH_LOGIN: &[&str] = &["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];

/// This process's environment, without what isn't UTF-8.
pub fn utf8_vars() -> impl Iterator<Item = (String, String)> {
    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
}

pub(crate) fn environment_key_eq(key: &std::ffi::OsStr, name: &str) -> bool {
    if cfg!(windows) { key.eq_ignore_ascii_case(std::ffi::OsStr::new(name)) } else { key == name }
}

/// Application settings survive a restart; session paths and lifecycle marks do not move.
pub(crate) fn restored_environment(
    saved: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    current: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    fresh: bool,
) -> std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    use std::ffi::OsStr;
    let current: std::collections::BTreeMap<_, _> = current.into_iter().collect();
    #[allow(unused_mut)]
    let mut saved: Vec<_> = saved.into_iter().collect();
    #[cfg(target_os = "linux")]
    let service = fresh
        && current.contains_key(OsStr::new("INVOCATION_ID"))
        && current.get(OsStr::new("XDG_RUNTIME_DIR")).is_some_and(|dir| dir == systemd::runtime_dir().as_os_str());
    #[cfg(target_os = "linux")]
    let ssh = service && saved.iter().any(|(k, _)| SSH_LOGIN.iter().any(|name| k == name));
    #[cfg(target_os = "linux")]
    if service {
        let utf8 =
            saved.into_iter().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
        saved = systemd::service_env(utf8).into_iter().map(|(k, v)| (k.into(), v.into())).collect();
    }
    #[cfg(not(target_os = "linux"))]
    let _ = fresh;
    #[cfg(target_os = "macos")]
    // A logged-out login leaves its agent socket on disk with nobody listening.
    let stale_agent = saved
        .iter()
        .any(|(key, value)| key == "SSH_AUTH_SOCK" && std::os::unix::net::UnixStream::connect(value).is_err());
    // An SSH launcher's agent is forwarded: it ends with that connection.
    #[cfg(target_os = "macos")]
    let forwarded = SSH_LOGIN.iter().any(|name| current.contains_key(OsStr::new(name)));
    let named = environment_key_eq;
    let local = |key: &OsStr| {
        let paths = [
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_STATE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
            "HC_SESSION",
            "HC_SOCKET",
        ];
        #[allow(unused_mut)]
        let mut keep = paths.iter().any(|name| named(key, name));
        #[cfg(target_os = "linux")]
        {
            keep |= systemd::SERVICE.iter().any(|name| named(key, name));
            keep |= ssh && systemd::SESSION.iter().any(|name| named(key, name));
        }
        #[cfg(target_os = "macos")]
        {
            keep |= named(key, "XPC_SERVICE_NAME");
            keep |= stale_agent && named(key, "SSH_AUTH_SOCK");
        }
        keep
    };
    let discarded = |key: &OsStr| {
        #[cfg(target_os = "macos")]
        if named(key, "XPC_FLAGS") {
            return true;
        }
        [crate::sweep::VAR, crate::sweep::RUNTIME, "HC_LAUNCHD_DIRECT", "HC_LAUNCHD_DETACH", "_HC_RESPONSIBLE_PID"]
            .iter()
            .any(|name| named(key, name))
    };
    let mut env: std::collections::BTreeMap<_, _> =
        saved.into_iter().filter(|(key, _)| !local(key) && !discarded(key)).collect();
    env.extend(current.into_iter().filter(|(key, _)| local(key) && !discarded(key)));
    #[cfg(target_os = "macos")]
    if stale_agent && forwarded {
        env.remove(OsStr::new("SSH_AUTH_SOCK"));
    }
    env.insert(crate::sweep::RUNTIME.into(), "1".into());
    env
}

/// The core's environment as programs get it: never an ssh login's marks,
/// and on Linux the login session as the user manager has it now.
pub async fn program_env() -> std::collections::BTreeMap<String, String> {
    #[allow(unused_mut)]
    let mut env: std::collections::BTreeMap<String, String> = utf8_vars().collect();
    #[cfg(target_os = "linux")]
    {
        env.retain(|k, _| !systemd::SERVICE.contains(&k.as_str()));
        if let Ok(Some(manager)) = tokio::task::spawn_blocking(systemd::manager_env).await {
            systemd::apply_session(&mut env, &manager);
        }
    }
    #[cfg(windows)]
    {
        env = tokio::task::spawn_blocking(move || {
            if let Err(error) = windows_env::refresh(&mut env) {
                eprintln!("cannot refresh Windows environment; using inherited settings: {error}");
            }
            env
        })
        .await
        .expect("environment reader panicked");
    }
    #[cfg(target_os = "macos")]
    env.remove("XPC_FLAGS");
    env.retain(|k, _| !SSH_LOGIN.contains(&k.as_str()));
    drop_unreachable_displays(&mut env);
    env
}

/// 32 random hex digits (ids, socket and folder names).
pub fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    random_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Signal numbers as the holder protocol carries them (the Unix ones; the
/// Windows holder maps them onto its own means).
pub mod signal {
    pub const HUP: i32 = 1;
    pub const INT: i32 = 2;
    pub const KILL: i32 = 9;
    pub const TERM: i32 = 15;
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod environment_tests;
