//! A planned restart keeps the retiring core's environment, not its updater's.

use crate::paths::{Paths, read_state};
use crate::runtime::Core;
use std::ffi::OsString;
use std::io;
use std::process::Command;
use std::sync::atomic::Ordering;

const ENVIRONMENT: &str = "restart-env.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct Environment {
    machine_id: Option<String>,
    vars: Vec<(OsString, OsString)>,
}

pub(crate) fn prepare(core: &Core) -> io::Result<()> {
    let path = core.paths.state.join(ENVIRONMENT);
    let tmp = path.with_extension(format!("{}.tmp", crate::platform::random_hex()));
    let saved = (|| {
        let mut file = crate::platform::private_file(&tmp)?;
        let env = Environment { machine_id: crate::platform::machine_id(), vars: std::env::vars_os().collect() };
        serde_json::to_writer(&mut file, &env)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(error) = saved {
        let _ = std::fs::remove_file(tmp);
        return Err(error);
    }
    core.restart.store(true, Ordering::SeqCst);
    Ok(())
}

pub(crate) fn apply(paths: &Paths, cmd: &mut Command) -> io::Result<()> {
    let env: Option<Environment> = read_state(&paths.state.join(ENVIRONMENT))?;
    if let Some(env) = env {
        if env.machine_id.is_some() && crate::platform::machine_id().is_some_and(|id| Some(id) != env.machine_id) {
            return Ok(());
        }
        crate::platform::replace_env(cmd, env.vars)?;
    }
    Ok(())
}

pub(crate) fn consumed(paths: &Paths) -> io::Result<()> {
    match std::fs::remove_file(paths.state.join(ENVIRONMENT)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
