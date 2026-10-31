//! What differs between Unix (Linux, macOS) and Windows, behind one small
//! surface: connections, terminals, process trees, the console, files.
//! Everything above it is shared.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(target_os = "linux")]
pub mod systemd;

#[cfg(target_os = "macos")]
mod macos_shutdown;

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

/// Capture values and explicit absences before the retiring core stops.
pub fn restart_env() -> std::io::Result<Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>> {
    #[allow(unused_mut)]
    let mut env: std::collections::BTreeMap<_, _> =
        std::env::vars_os().map(|(key, value)| (key, Some(value))).collect();
    #[cfg(target_os = "linux")]
    for key in systemd::inherited_env()?.into_keys() {
        env.entry(key.into()).or_insert(None);
    }
    Ok(env.into_iter().collect())
}

/// Replace inherited values with the saved values and absences.
pub fn replace_env(
    cmd: &mut std::process::Command,
    env: impl IntoIterator<Item = (std::ffi::OsString, Option<std::ffi::OsString>)>,
) {
    // Detached launchers rebuild from get_envs(), which does not expose env_clear().
    for (key, _) in std::env::vars_os() {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
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
    env.retain(|k, _| !SSH_LOGIN.contains(&k.as_str()));
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
