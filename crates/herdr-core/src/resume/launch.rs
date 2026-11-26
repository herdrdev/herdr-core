use super::Record;
use crate::platform;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::Path;

const MAX_FILE: u64 = 128 << 10;

#[derive(Deserialize, Serialize)]
struct Launch {
    argv: Vec<String>,
    cwd: String,
    shell: Vec<String>,
}

pub(super) fn prepare(
    state: &Path,
    id: u32,
    shell: &[String],
    fallback_cwd: &str,
    record: &Record,
) -> io::Result<(String, Vec<String>, String)> {
    platform::secure_state_dir(state)?;
    let is_dir = |path: &str| Path::new(path).is_dir();
    let cwd = if let Some(dir) = &record.reported_cwd {
        if !is_dir(dir) {
            return Err(io::ErrorKind::NotFound.into());
        }
        dir
    } else {
        record.owner_cwd.as_deref().filter(|dir| is_dir(dir)).unwrap_or(fallback_cwd)
    };
    let name = format!("resume-{id}-{}.json", platform::random_hex());
    let path = std::path::absolute(state)?.join(&name);
    let helper = std::env::current_exe()?;
    if !helper.is_file() {
        return Err(io::ErrorKind::NotFound.into());
    }
    let argv =
        platform::resume_shell(shell, &helper, &path, record.ancestors.is_empty()).ok_or(io::ErrorKind::Unsupported)?;
    let launch = Launch { argv: record.argv.clone(), cwd: cwd.into(), shell: shell.to_vec() };
    let contents = serde_json::to_vec(&launch)?;
    let mut file = platform::private_file(&path)?;
    if let Err(error) = file.write_all(&contents) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok((name, argv, cwd.into()))
}

fn launch_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else { return false };
    let Some((id, nonce)) =
        name.strip_prefix("resume-").and_then(|s| s.strip_suffix(".json")).and_then(|s| s.split_once('-'))
    else {
        return false;
    };
    id.parse::<u32>().is_ok() && nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn remove_launch(state: &Path, name: Option<&str>) {
    let Some(name) = name.filter(|n| !n.chars().any(std::path::is_separator)) else { return };
    let path = state.join(name);
    if launch_name(&path) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("claim"));
    }
}

pub(super) fn remove_abandoned(state: &Path, active: impl Fn(&str) -> bool) {
    for item in std::fs::read_dir(state).into_iter().flatten().flatten() {
        let path = item.path();
        let json = path.with_extension("json");
        if path.extension().is_some_and(|ext| ext == "json" || ext == "claim")
            && launch_name(&json)
            && json.file_name().and_then(|s| s.to_str()).is_some_and(|name| !active(name))
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn claim(path: &Path) -> io::Result<Launch> {
    if !path.is_absolute() || !launch_name(path) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    platform::secure_state_dir(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let claimed = path.with_extension("claim");
    platform::rename_noreplace(path, &claimed)?;
    let read = || -> io::Result<Launch> {
        let file = std::fs::File::open(&claimed)?;
        if !platform::private_launch_file(&file.metadata()?) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(serde_json::from_slice(&bytes)?)
    };
    let data = read();
    std::fs::remove_file(claimed)?;
    data
}

pub(crate) fn helper(path: &Path, first: bool) -> i32 {
    let launch = claim(path).ok();
    if let Some(launch) = &launch
        && !launch.argv.is_empty()
        && std::env::set_current_dir(&launch.cwd).is_ok()
        && let Ok(code) = platform::run_resume(&launch.argv, &launch.cwd)
    {
        return code;
    }
    eprintln!("hc: could not resume the program");
    if !first {
        return 1;
    }
    let shell = launch
        .map(|l| l.shell)
        .or_else(|| {
            let paths = crate::paths::Paths::from_env().ok()?;
            crate::config::load(&paths.config).ok()?.shell().ok()
        })
        .unwrap_or_else(platform::own_shell);
    platform::resume_fallback(&shell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_file_can_only_be_claimed_once() {
        let dir = std::env::temp_dir().join(format!("hc-resume-{}", platform::random_hex()));
        platform::private_dir(&dir).unwrap();
        let path = dir.join(format!("resume-1-{}.json", platform::random_hex()));
        let contents = Launch { argv: vec!["example".into()], cwd: String::new(), shell: Vec::new() };
        serde_json::to_writer(platform::private_file(&path).unwrap(), &contents).unwrap();
        assert_eq!(claim(&path).unwrap().argv, contents.argv);
        assert!(claim(&path).is_err());
        std::fs::remove_dir(dir).unwrap();
    }
}
