//! Non-interactive login capture for a newly started Unix core.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::LOGIN_CAPTURE as CAPTURING;
const MARKER: &[u8] = b"\0HC_LOGIN_ENV\0";
const END: &[u8] = b"\0HC_LOGIN_END\0";
const DUMP: &str = "--hc-dump-login-env";
const SCRIPT: &str =
    "printf '\\0HC_LOGIN_ENV\\0'; \"$HC_LOGIN_CAPTURE\" --hc-dump-login-env; printf '\\0HC_LOGIN_END\\0'";

pub(super) fn maybe_dump() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(DUMP)) || std::env::var_os(CAPTURING).is_none() {
        return None;
    }
    // Darwin's env doesn't provide the NUL-delimited output needed for multiline values.
    let mut stdout = io::stdout().lock();
    let result = std::env::vars_os().try_for_each(|(key, value)| {
        stdout.write_all(key.as_encoded_bytes())?;
        stdout.write_all(b"=")?;
        stdout.write_all(value.as_encoded_bytes())?;
        stdout.write_all(b"\0")
    });
    Some(i32::from(result.is_err()))
}

const SYSTEM_PATH: &str = if cfg!(target_os = "macos") {
    "/usr/bin:/bin:/usr/sbin:/sbin"
} else {
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
};

type Environment = BTreeMap<OsString, OsString>;

pub(super) fn environment(exe: &OsStr, state: &Path, implicit: bool) -> Option<Environment> {
    let caller: Environment = std::env::vars_os().collect();
    let ssh = super::SSH_LOGIN.iter().any(|key| caller.contains_key(OsStr::new(key)));
    if !implicit && (!cfg!(target_os = "linux") || !ssh || caller.contains_key(OsStr::new(crate::sweep::RUNTIME))) {
        return None;
    }

    let seed = if implicit {
        let mut clean = Environment::new();
        copy_inputs(
            &caller,
            &mut clean,
            &[
                "HOME",
                "USER",
                "LOGNAME",
                "SHELL",
                "LANG",
                "LC_ALL",
                "LC_CTYPE",
                "LC_COLLATE",
                "LC_MESSAGES",
                "LC_MONETARY",
                "LC_NUMERIC",
                "LC_TIME",
            ],
        );
        copy_inputs(&caller, &mut clean, super::CORE_INPUTS);
        copy_inputs(&caller, &mut clean, super::SSH_LOGIN);
        #[cfg(target_os = "linux")]
        copy_inputs(&caller, &mut clean, super::systemd::SESSION);
        #[cfg(not(target_os = "linux"))]
        copy_inputs(
            &caller,
            &mut clean,
            &["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "SSH_AUTH_SOCK"],
        );
        if !clean.contains_key(OsStr::new("HOME"))
            && let Some(home) = std::env::home_dir()
        {
            clean.insert("HOME".into(), home.into_os_string());
        }
        if ["USER", "LOGNAME", "SHELL"].iter().any(|key| !clean.contains_key(OsStr::new(key)))
            && let Some((name, shell)) = super::unix::account()
        {
            clean.entry("USER".into()).or_insert_with(|| name.clone());
            clean.entry("LOGNAME".into()).or_insert(name);
            clean.entry("SHELL".into()).or_insert(shell);
        }
        clean.insert("PATH".into(), SYSTEM_PATH.into());
        clean.insert("TMPDIR".into(), platform_temp());
        clean
    } else {
        caller.clone()
    };
    #[cfg(target_os = "macos")]
    let (seed, default_lang) = {
        let mut seed = seed;
        let locale = ["LANG", "LC_ALL", "LC_CTYPE"]
            .into_iter()
            .filter_map(|key| seed.get(OsStr::new(key)).map(|value| (key.into(), value.to_string_lossy().into_owned())))
            .collect();
        let default_lang = crate::locale::for_programs(&locale);
        if let Some(lang) = &default_lang {
            seed.insert("LANG".into(), lang.into());
        }
        (seed, default_lang)
    };
    let shell = seed.get(OsStr::new("SHELL")).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| "/bin/sh".into());
    let mut vars = match capture(exe, &shell, &seed, implicit) {
        Ok(mut vars) => {
            for key in super::SSH_LOGIN {
                vars.remove(OsStr::new(key));
            }
            // Session paths and SSH routing stay with the launcher.
            super::restored_environment(vars, seed, false)
        }
        Err(error) => {
            if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(state.join("core.log")) {
                let _ = writeln!(log, "core: login environment from {:?} unavailable: {error}", Path::new(&shell));
            }
            if !implicit {
                return None;
            }
            seed
        }
    };
    copy_inputs(&caller, &mut vars, super::SSH_LOGIN);
    #[cfg(target_os = "macos")]
    if let Some(lang) = default_lang
        && vars.get(OsStr::new("LANG")).is_some_and(|value| value == OsStr::new(&lang))
    {
        // The profile needed a default, but launchd may already supply a locale.
        // Programs choose the region default if the finished environment lacks one.
        vars.remove(OsStr::new("LANG"));
    }
    if implicit {
        let mut path: Vec<_> =
            std::env::split_paths(vars.get(OsStr::new("PATH")).map_or(OsStr::new(SYSTEM_PATH), OsString::as_os_str))
                .filter(|dir| !dir.as_os_str().is_empty())
                .collect();
        if path.is_empty() {
            path.extend(std::env::split_paths(SYSTEM_PATH));
        }
        if let Some(caller_path) = caller.get(OsStr::new("PATH")) {
            for directory in std::env::split_paths(caller_path).filter(|dir| !dir.as_os_str().is_empty()) {
                if !path.contains(&directory) {
                    path.push(directory);
                }
            }
        }
        if let Ok(path) = std::env::join_paths(path) {
            vars.insert("PATH".into(), path);
        }
    }
    Some(vars)
}

fn copy_inputs(source: &Environment, dest: &mut Environment, names: &[&str]) {
    for &key in names {
        if let Some(value) = source.get(OsStr::new(key)) {
            dest.insert(key.into(), value.clone());
        }
    }
}

fn platform_temp() -> OsString {
    #[cfg(target_os = "macos")]
    {
        let mut path = [0u8; libc::PATH_MAX as usize];
        let size = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, path.as_mut_ptr().cast(), path.len()) };
        if size > 1 && size <= path.len() {
            return OsString::from_vec(path[..size - 1].to_vec());
        }
    }
    "/tmp".into()
}

fn capture(exe: &OsStr, shell: &OsStr, seed: &Environment, implicit: bool) -> io::Result<Environment> {
    let mut cmd = Command::new(shell);
    cmd.env_clear().envs(seed).env(CAPTURING, exe);
    if implicit {
        cmd.current_dir(seed.get(OsStr::new("HOME")).map_or(Path::new("/"), Path::new));
    }
    let name = Path::new(shell).file_name().unwrap_or_default().to_string_lossy();
    match name.as_ref() {
        "csh" | "tcsh" => {
            cmd.arg0(format!("-{name}"));
        }
        _ => {
            cmd.arg("-l");
        }
    }
    cmd.args(["-c", SCRIPT]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut bytes = Vec::new();
    let result = (|| {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let start = Instant::now();
        let mut remaining = 1 << 20;
        loop {
            let status = child.try_wait()?;
            for (stream, keep) in [(&mut stdout as &mut dyn Read, true), (&mut stderr as &mut dyn Read, false)] {
                let mut buffer = [0; 8192];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if n > remaining {
                                return Err(io::Error::other("profile output exceeded 1 MiB"));
                            }
                            remaining -= n;
                            if keep {
                                bytes.extend_from_slice(&buffer[..n]);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    }
                    if start.elapsed() >= Duration::from_secs(5) {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "profile exceeded 5 s"));
                    }
                }
            }
            if let Some(status) = status {
                if !status.success() {
                    return Err(io::Error::other(format!("profile exited with {status}")));
                }
                return parse(&bytes);
            }
            if start.elapsed() >= Duration::from_secs(5) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "profile exceeded 5 s"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // The shell may exit before its background jobs close the capture pipes.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    result
}

fn parse(bytes: &[u8]) -> io::Result<BTreeMap<OsString, OsString>> {
    let invalid = || io::Error::other("profile did not produce a complete environment");
    let start = bytes.windows(MARKER.len()).position(|part| part == MARKER).ok_or_else(invalid)? + MARKER.len();
    let tail = &bytes[start..];
    let end = tail.windows(END.len()).position(|part| part == END).ok_or_else(invalid)?;
    let bytes = &tail[..end];
    if bytes.is_empty() || !bytes.ends_with(&[0]) {
        return Err(invalid());
    }
    bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .map(|entry| {
            let at = entry.iter().position(|byte| *byte == b'=').filter(|at| *at > 0).ok_or_else(invalid)?;
            Ok((OsString::from_vec(entry[..at].to_vec()), OsString::from_vec(entry[at + 1..].to_vec())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_environment_ignores_profile_chatter_and_preserves_values() {
        let vars =
            parse(b"hello\n\0HC_LOGIN_ENV\0PATH=/home/me/bin:/bin\0VALUE=one\ntwo=three\0\0HC_LOGIN_END\0goodbye\n")
                .unwrap();
        assert_eq!(vars[std::ffi::OsStr::new("PATH")], "/home/me/bin:/bin");
        assert_eq!(vars[std::ffi::OsStr::new("VALUE")], "one\ntwo=three");
        assert!(parse(b"hello\n").is_err());
        assert!(parse(b"\0HC_LOGIN_ENV\0PATH=/bin").is_err());
    }
}
