use super::{ProcessState, ResumeProcess, windows_exec::PROMPT_HOOK};
use crate::resume::Identity;
use std::ffi::OsString;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW;

fn query_token(process: HANDLE) -> io::Result<OwnedHandle> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(token))
    }
}

fn account(process: HANDLE, unelevated: bool) -> Option<String> {
    let token = query_token(process).ok()?;
    let token = token.as_raw_handle();
    unsafe {
        let mut n = 0;
        if unelevated {
            let mut elevation: TOKEN_ELEVATION = zeroed();
            if GetTokenInformation(
                token,
                TokenElevation,
                (&raw mut elevation).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut n,
            ) == 0
                || elevation.TokenIsElevated != 0
            {
                return None;
            }
        }
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut n);
        if n < size_of::<TOKEN_USER>() as u32 {
            return None;
        }
        let mut user = vec![0usize; (n as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(token, TokenUser, user.as_mut_ptr().cast(), n, &mut n) == 0 {
            return None;
        }
        let mut sid = null_mut();
        if ConvertSidToStringSidW((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid, &mut sid) == 0 {
            return None;
        }
        let len = (0..).take_while(|&i| *sid.add(i) != 0).count();
        let user = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        LocalFree(sid.cast());
        Some(user)
    }
}

pub(crate) fn resume_user() -> Option<String> {
    account(unsafe { GetCurrentProcess() }, true)
}

pub(crate) fn resume_process(pid: u32) -> ProcessState {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return if io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                ProcessState::Gone
            } else {
                ProcessState::Unknown
            };
        }
        let _process = OwnedHandle::from_raw_handle(h);
        let [mut start, mut exit, mut kernel, mut user]: [FILETIME; 4] = zeroed();
        if GetProcessTimes(h, &mut start, &mut exit, &mut kernel, &mut user) == 0 {
            return ProcessState::Unknown;
        }
        if exit.dwLowDateTime != 0 || exit.dwHighDateTime != 0 {
            return ProcessState::Gone;
        }
        let mut basic: PROCESS_BASIC_INFORMATION = zeroed();
        let size = size_of::<PROCESS_BASIC_INFORMATION>() as u32;
        if NtQueryInformationProcess(h, ProcessBasicInformation, (&raw mut basic).cast(), size, null_mut()) != 0 {
            return ProcessState::Unknown;
        }
        let Some(user) = account(h, false) else { return ProcessState::Unknown };
        ProcessState::Alive(ResumeProcess {
            identity: Identity { pid, start: u64::from(start.dwHighDateTime) << 32 | u64::from(start.dwLowDateTime) },
            parent: basic.InheritedFromUniqueProcessId as u32,
            user,
        })
    }
}

pub(crate) fn resume_profile() -> io::Result<PathBuf> {
    let token = query_token(unsafe { GetCurrentProcess() })?;
    let mut len = 0;
    unsafe { GetUserProfileDirectoryW(token.as_raw_handle(), null_mut(), &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut profile = vec![0; len as usize];
    if unsafe { GetUserProfileDirectoryW(token.as_raw_handle(), profile.as_mut_ptr(), &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let profile = profile.split(|&c| c == 0).next().unwrap_or_default();
    Ok(OsString::from_wide(profile).into())
}

// State inherits the user's profile ACL.
pub(crate) fn secure_state_dir(path: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || super::is_link(&meta) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "state directory must not be a reparse point"));
    }
    let profile = std::fs::canonicalize(resume_profile()?)?;
    let state = std::fs::canonicalize(path)?;
    if state.starts_with(&profile) && state != profile {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "state directory must be inside the account profile"))
    }
}

pub(crate) fn resume_shutdown_signal(_: i32) -> bool {
    false
}

pub(crate) fn private_launch_file(meta: &std::fs::Metadata) -> bool {
    meta.is_file() && !super::is_link(meta)
}

fn quoted(path: &Path) -> Option<String> {
    let mut value = String::from("'");
    for c in path.to_str()?.chars() {
        if matches!(c, '\'' | '‘' | '’' | '‚' | '‛') {
            value.push(c);
        }
        value.push(c);
    }
    value.push('\'');
    Some(value)
}

pub(crate) fn resume_shell(shell: &[String], helper: &Path, file: &Path, first: bool) -> Option<Vec<String>> {
    let exe = shell.first()?;
    let name = Path::new(exe).file_stem()?.to_str()?;
    if !name.eq_ignore_ascii_case("pwsh") && !name.eq_ignore_ascii_case("powershell") {
        return None;
    }
    let opts = &shell[1..];
    let mut argv = vec![exe.clone()];
    if opts != ["-NoExit", "-Command", PROMPT_HOOK] {
        if !opts.iter().all(|s| ["-NoLogo", "-NoProfile", "-NoExit"].iter().any(|opt| s.eq_ignore_ascii_case(opt))) {
            return None;
        }
        argv.extend(opts.iter().filter(|s| !s.eq_ignore_ascii_case("-NoExit")).cloned());
    }
    let end = if first { " --first; exit $LASTEXITCODE" } else { "" };
    if !first {
        argv.push("-NoExit".into());
    }
    argv.push("-Command".into());
    argv.push(format!("{PROMPT_HOOK}; & {} __resume {}{end}", quoted(helper)?, quoted(file)?));
    Some(argv)
}

pub(crate) fn run_resume(argv: &[String], cwd: &str) -> io::Result<i32> {
    let path = std::env::var("PATH").unwrap_or_default();
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let path = super::windows::resolve(&argv[0], &path, &exts).ok_or(io::ErrorKind::NotFound)?;
    let path = std::path::absolute(path)?;
    if path.extension().is_some_and(|ext| ["bat", "cmd"].iter().any(|suffix| ext.eq_ignore_ascii_case(suffix)))
        && path.as_os_str().as_encoded_bytes().contains(&b'%')
    {
        // Command quotes the script path, but cmd.exe still expands percent expressions there.
        return Err(io::ErrorKind::InvalidInput.into());
    }
    wait_for_program(std::process::Command::new(path).args(&argv[1..]).env("PWD", cwd))
}

fn wait_for_program(command: &mut std::process::Command) -> io::Result<i32> {
    // A registered handler is private to this process, unlike SetConsoleCtrlHandler(NULL, TRUE).
    unsafe extern "system" fn ignore(event: u32) -> i32 {
        i32::from(matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT))
    }
    if unsafe { SetConsoleCtrlHandler(Some(ignore), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let status = command.status();
    unsafe { SetConsoleCtrlHandler(Some(ignore), 0) };
    Ok(status?.code().unwrap_or(1))
}

pub(crate) fn resume_fallback(shell: &[String]) -> i32 {
    shell
        .split_first()
        .and_then(|(exe, args)| wait_for_program(std::process::Command::new(exe).args(args)).ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_resume_quotes_both_paths_as_literals() {
        let helper = Path::new(r"C:\O'Name\hc.exe");
        let file = Path::new(r"C:\‘folder’\$x.json");
        let argv = resume_shell(&["pwsh.exe".into()], helper, file, false).unwrap();
        assert_eq!(&argv[..3], ["pwsh.exe", "-NoExit", "-Command"]);
        assert_eq!(
            argv[3].strip_prefix(PROMPT_HOOK),
            Some(r"; & 'C:\O''Name\hc.exe' __resume 'C:\‘‘folder’’\$x.json'")
        );
    }
}
