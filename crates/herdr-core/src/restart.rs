//! A short-lived handoff for explicitly requested restarts, never for SIGTERM.

use crate::paths::Paths;
use crate::runtime::Core;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

const ENVIRONMENT: &str = "restart-env.json";
const VALID_MS: u64 = 3 * 60 * 1000;

#[derive(serde::Serialize, serde::Deserialize)]
struct Environment {
    machine_id: Option<String>,
    boot_id: Option<String>,
    written_ms: u64,
    vars: Vec<(OsString, OsString)>,
    #[serde(default)]
    fresh: bool,
}

fn now_ms() -> io::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_millis() as u64)
}

pub(crate) fn prepare(core: &Core, environment: Option<BTreeMap<String, String>>) -> io::Result<()> {
    let fresh = environment.is_some();
    let vars = environment.map(replacement_environment).transpose()?.unwrap_or_else(current_environment);
    let state = core.lock();
    if state.stopping || core.restart.swap(true, Ordering::SeqCst) {
        return Err(io::Error::other("the core is stopping or restarting"));
    }
    drop(state);
    discard(&core.paths);
    if let Err(error) = save(&core.paths, vars, fresh) {
        eprintln!("core: cannot preserve restart environment; using the next launcher's environment: {error}");
        discard(&core.paths);
    }
    Ok(())
}

fn current_environment() -> Vec<(OsString, OsString)> {
    std::env::vars_os().filter(|(key, _)| !key.as_encoded_bytes().starts_with(b"=")).collect()
}

fn replacement_environment(env: BTreeMap<String, String>) -> io::Result<Vec<(OsString, OsString)>> {
    let vars: Vec<(OsString, OsString)> = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
    if !vars.iter().any(|(k, _)| crate::platform::environment_key_eq(k, "PATH")) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "replacement environment must include PATH"));
    }
    validate(&vars).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    Ok(vars)
}

/// Write under the idle lock so even a competing starter restores this environment.
pub(crate) fn prepare_start(paths: &Paths, environment: BTreeMap<String, String>) -> io::Result<()> {
    let vars = replacement_environment(environment)?;
    paths.create_dirs()?;
    let lock =
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths.state.join("core.lock"))?;
    if !crate::platform::try_lock(&lock) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the core is starting or restarting; retry --fresh once it is running",
        ));
    }
    discard(paths);
    save(paths, vars, true)
}

fn save(paths: &Paths, vars: Vec<(OsString, OsString)>, fresh: bool) -> io::Result<()> {
    let env = Environment {
        machine_id: crate::platform::machine_id(),
        boot_id: crate::platform::boot_id()?,
        written_ms: now_ms()?,
        vars,
        fresh,
    };
    let tmp = paths.state.join("restart-env.tmp");
    let mut file = crate::platform::private_file(&tmp)?;
    serde_json::to_writer(&mut file, &env)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, paths.state.join(ENVIRONMENT))
}

fn load(paths: &Paths) -> io::Result<Option<Environment>> {
    let bytes = match std::fs::read(paths.state.join(ENVIRONMENT)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => result?,
    };
    // Decode and validate everything before touching the process environment.
    let mut env: Environment =
        serde_json::from_slice(&bytes).map_err(|_| io::Error::other("invalid restart environment"))?;
    // cmd.exe's per-drive working directories are not variables set_var can restore.
    env.vars.retain(|(key, _)| !key.as_encoded_bytes().starts_with(b"="));
    let age = now_ms()?.checked_sub(env.written_ms);
    if env.machine_id != crate::platform::machine_id()
        || env.boot_id != crate::platform::boot_id()?
        || age.is_none_or(|age| age > VALID_MS)
    {
        return Err(io::Error::other("restart environment is from another machine, boot, or time"));
    }
    validate(&env.vars)?;
    Ok(Some(env))
}

fn validate(vars: &[(OsString, OsString)]) -> io::Result<()> {
    if vars.iter().any(|(key, value)| !valid_key(key) || value.as_encoded_bytes().contains(&0)) {
        return Err(io::Error::other("invalid restart environment entry"));
    }
    // Command uses the OS's key comparison, including Windows' case-insensitive keys.
    let mut unique = std::process::Command::new("unused");
    unique.env_clear().envs(vars.iter().cloned());
    if unique.get_envs().count() != vars.len() {
        return Err(io::Error::other("duplicate restart environment entries"));
    }
    Ok(())
}

/// The caller owns core.lock and has not started threads or adopted holders.
/// The selected session's paths and the new process's lifecycle marks stay its own.
///
/// # Safety
/// No other thread may read or write the process environment.
pub(crate) unsafe fn restore(paths: &Paths) {
    sweep_temporary(paths);
    let env = match load(paths) {
        Ok(Some(env)) => env,
        Ok(None) => return,
        Err(error) => {
            eprintln!("core: ignoring restart environment: {error}");
            discard(paths);
            return;
        }
    };
    let restored = crate::platform::restored_environment(env.vars, std::env::vars_os(), env.fresh);
    unsafe {
        for (key, _) in std::env::vars_os().filter(|(key, _)| valid_key(key)) {
            std::env::remove_var(key);
        }
        for (key, value) in restored {
            std::env::set_var(key, value);
        }
    }
}

fn valid_key(key: &OsStr) -> bool {
    !key.is_empty() && !key.as_encoded_bytes().iter().any(|byte| matches!(byte, 0 | b'='))
}

pub(crate) fn on_sigterm(core: &Core) {
    if !core.restart.swap(true, Ordering::SeqCst) {
        discard(&core.paths);
    }
}

fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!("core: cannot remove {}: {error}", path.display());
    }
}

fn sweep_temporary(paths: &Paths) {
    if let Ok(entries) = std::fs::read_dir(&paths.state) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("restart-env.") && name.ends_with(".tmp") {
                remove(&entry.path());
            }
        }
    }
}

pub(crate) fn discard(paths: &Paths) {
    remove(&paths.state.join(ENVIRONMENT));
    sweep_temporary(paths);
}

/// A connect-only stop can race an active handoff; only an idle session is ours to clean.
pub(crate) fn discard_if_stopped(paths: &Paths) {
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths.state.join("core.lock"));
    match lock {
        Ok(lock) if crate::platform::try_lock(&lock) => discard(paths),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("core: cannot lock restart environment for cleanup: {error}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn inherited_keys_that_cannot_be_unset_are_skipped() {
        for key in ["", "=C:", "=::", "BAD=KEY", "BAD\0KEY"] {
            assert!(!super::valid_key(std::ffi::OsStr::new(key)), "{key:?}");
        }
        assert!(super::valid_key(std::ffi::OsStr::new("PATH")));
    }
}
