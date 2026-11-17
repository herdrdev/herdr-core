//! The first Unix SSH start needs the same exported settings as a login.
//! Capture outside the protocol and before any session teardown.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CAPTURING: &str = "HC_LOGIN_CAPTURE";
const MARKER: &[u8] = b"\0HC_LOGIN_ENV\0";
const END: &[u8] = b"\0HC_LOGIN_END\0";
const SCRIPT: &str = "printf '\\0HC_LOGIN_ENV\\0'; /usr/bin/env -0; printf '\\0HC_LOGIN_END\\0'";

pub(super) fn apply(cmd: &mut Command, state: &Path) {
    if std::env::var_os(CAPTURING).is_some() {
        cmd.env_remove(CAPTURING);
        return;
    }
    if std::env::var_os(crate::sweep::RUNTIME).is_some()
        || !super::SSH_LOGIN.iter().any(|key| std::env::var_os(key).is_some())
    {
        return;
    }
    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let incoming_path = std::env::var_os("PATH").unwrap_or_default();
    match capture(&shell, &incoming_path, state) {
        Ok(mut vars) => {
            vars.remove(OsStr::new(CAPTURING));
            let merged = merge_ssh_path(
                vars.get(OsStr::new("PATH")).map(OsString::as_os_str).unwrap_or_default(),
                &incoming_path,
            );
            if !merged.is_empty() {
                vars.insert("PATH".into(), merged);
            }
            let mut vars = super::restored_environment(vars, std::env::vars_os(), false);
            // Profiles must not hide the SSH origin from service or launchd filtering.
            for key in super::SSH_LOGIN {
                if let Some(value) = std::env::var_os(key) {
                    vars.insert((*key).into(), value);
                }
            }
            for (key, _) in std::env::vars_os() {
                if !vars.contains_key(&key) {
                    cmd.env_remove(key);
                }
            }
            cmd.envs(vars);
        }
        Err(error) => {
            if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(state.join("core.log")) {
                let _ = writeln!(
                    log,
                    "core: login environment from {} unavailable: {error}; using inherited environment",
                    Path::new(&shell).display()
                );
            }
        }
    }
}

fn capture(shell: &OsStr, incoming_path: &OsStr, state: &Path) -> io::Result<BTreeMap<OsString, OsString>> {
    let mut cmd = Command::new(shell);
    cmd.env(CAPTURING, "1");
    cmd.env("PATH", capture_base_path(incoming_path));
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "dumb");
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
    let mut diagnostics = Vec::new();
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
            for (stream, output) in
                [(&mut stdout as &mut dyn Read, &mut bytes), (&mut stderr as &mut dyn Read, &mut diagnostics)]
            {
                let mut buffer = [0; 8192];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if n > remaining {
                                return Err(io::Error::other("profile output exceeded 1 MiB"));
                            }
                            remaining -= n;
                            output.extend_from_slice(&buffer[..n]);
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
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(state.join("core.log")) {
        let _ = log.write_all(&diagnostics[..diagnostics.len().min(1 << 20)]);
    }
    result
}

fn capture_base_path(incoming: &OsStr) -> OsString {
    let base = ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"];
    // Keep only system and Nix store roots so macOS path_helper and profile PATH guards
    // preserve front priority for directories the profile prepends.
    let nix = std::env::split_paths(incoming).filter(|p| {
        let bytes = p.as_os_str().as_bytes();
        bytes.starts_with(b"/nix/") || bytes.starts_with(b"/run/current-system/")
    });
    join_unique_paths(base.into_iter().map(PathBuf::from).chain(nix))
}

fn merge_ssh_path(login: &OsStr, incoming: &OsStr) -> OsString {
    join_unique_paths(std::env::split_paths(login).chain(std::env::split_paths(incoming)))
}

fn join_unique_paths(paths: impl IntoIterator<Item = PathBuf>) -> OsString {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        if path.as_os_str().is_empty() || seen.contains(&path) {
            continue;
        }
        if !out.is_empty() {
            out.push(b':');
        }
        out.extend_from_slice(path.as_os_str().as_bytes());
        seen.insert(path);
    }
    OsString::from_vec(out)
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
        assert_eq!(vars[OsStr::new("PATH")], "/home/me/bin:/bin");
        assert_eq!(vars[OsStr::new("VALUE")], "one\ntwo=three");
        assert!(parse(b"hello\n").is_err());
        assert!(parse(b"\0HC_LOGIN_ENV\0PATH=/bin").is_err());
    }

    #[test]
    fn capture_base_path_keeps_only_system_and_nix_entries() {
        let incoming = OsStr::new(
            "/Users/me/.local/bin:/opt/homebrew/bin:/nix/var/nix/profiles/default/bin:/nix_fake/bin::/usr/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin",
        );
        assert_eq!(
            capture_base_path(incoming),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin"
        );
    }

    #[test]
    fn merge_ssh_path_puts_login_entries_first_and_retains_caller_extras() {
        let login = OsStr::new("/home/me/profile_bin:/opt/homebrew/bin:/usr/bin:/bin");
        let incoming = OsStr::new("/home/me/shadow_bin:/home/me/profile_bin:/home/me/caller_only_bin::/usr/bin:/bin");
        assert_eq!(
            merge_ssh_path(login, incoming),
            "/home/me/profile_bin:/opt/homebrew/bin:/usr/bin:/bin:/home/me/shadow_bin:/home/me/caller_only_bin"
        );
        assert_eq!(merge_ssh_path(OsStr::new(""), OsStr::new("/a:/b:/a")), "/a:/b");
        assert_eq!(merge_ssh_path(OsStr::new("/login:/usr/bin"), OsStr::new("")), "/login:/usr/bin");
    }
}
