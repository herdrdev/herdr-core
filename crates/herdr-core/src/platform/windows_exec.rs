//! Windows: which shell, and execs. An exec runs in its own job, which ends
//! everything it started when terminated or closed. It starts suspended and
//! joins the job before it runs, so nothing it starts escapes.

use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr::null;
use std::sync::Arc;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::*;

pub fn split_command_line(mut line: Vec<u16>) -> io::Result<Vec<std::ffi::OsString>> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    line.push(0);
    let mut argc = 0;
    let argv = unsafe { CommandLineToArgvW(line.as_ptr(), &mut argc) };
    if argv.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        let words = std::slice::from_raw_parts(argv, argc as usize)
            .iter()
            .map(|&arg| {
                let mut len = 0;
                while *arg.add(len) != 0 {
                    len += 1;
                }
                std::ffi::OsString::from_wide(std::slice::from_raw_parts(arg, len))
            })
            .collect::<Vec<_>>();
        LocalFree(argv.cast());
        words
    })
}

fn via_words(target: &str) -> io::Result<Vec<std::ffi::OsString>> {
    // CommandLineToArgvW otherwise treats leading spaces as an empty argv[0].
    let target = target.trim_start_matches(' ');
    if target.is_empty() || target.chars().any(char::is_control) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "one line, no control characters"));
    }
    let words = split_command_line(target.encode_utf16().collect())?;
    if words.first().is_none_or(|word| word.is_empty()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty executable"));
    }
    Ok(words)
}

pub fn check_via(target: &str) -> io::Result<()> {
    via_words(target).map(|_| ())
}

pub fn via_add_hint(target: &str) -> String {
    format!("hc machine add --via '{}' --name <name>", target.replace('\'', "''"))
}

pub fn via_command(target: &str, line: &str) -> io::Result<std::process::Command> {
    use std::os::windows::process::CommandExt;
    let words = via_words(target)?;
    let mut cmd = std::process::Command::new(&words[0]);
    cmd.args(&words[1..]).arg(line).creation_flags(CREATE_NO_WINDOW);
    Ok(cmd)
}

#[cfg(test)]
mod via_tests {
    use super::*;

    #[test]
    fn windows_via_preserves_paths_quoted_arguments_and_the_remote_line() {
        let line = "sh -c 'printf \"%s\\n\" \"$HOME\" | cat'";
        let cmd = via_command(r#"  "C:\Program Files\bridge.exe" -d "Ubuntu Test" -e sh -c"#, line).unwrap();
        assert_eq!(cmd.get_program(), r"C:\Program Files\bridge.exe");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["-d", "Ubuntu Test", "-e", "sh", "-c", line]);
        let words = via_words(r#"bridge.exe "C:\folder\\" "a\"b" """#).unwrap();
        assert_eq!(words, ["bridge.exe", "C:\\folder\\", "a\"b", ""]);
        assert!(check_via(r#""" arg"#).is_err());
        assert!(check_via("bridge.exe\narg").is_err());
        assert_eq!(via_add_hint("bridge.exe it's"), "hc machine add --via 'bridge.exe it''s' --name <name>");
    }
}

/// This machine's own shell: PowerShell 7 if installed, else Windows PowerShell.
/// PowerShell's `cd` never tells Windows, so hc couldn't see its folder: each
/// prompt does, after the person's own prompt ran (it reads `$?`).
pub fn own_shell() -> Vec<String> {
    let on_path = |exe: &str| {
        std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(exe).is_file()))
    };
    let exe = if on_path("pwsh.exe") { "pwsh.exe" } else { "powershell.exe" };
    let hook = "$global:__hc_prompt = $function:prompt; function global:prompt { $p = & $global:__hc_prompt; \
                $l = $ExecutionContext.SessionState.Path.CurrentLocation; \
                if ($l.Provider.Name -eq 'FileSystem') { try { [Environment]::CurrentDirectory = $l.ProviderPath } catch {} }; $p }";
    [exe, "-NoExit", "-Command", hook].map(String::from).to_vec()
}

/// Windows has no display to reach: nothing to drop.
pub fn drop_unreachable_displays(_env: &mut std::collections::BTreeMap<String, String>) {}

/// A shell line as this machine runs one: cmd reads the rest of its command
/// line itself (`/s`: verbatim between the outer quotes), so the line goes
/// raw, never quoted as an argument.
pub fn shell_line(line: &str) -> Vec<String> {
    let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    vec![cmd, "/d".into(), "/s".into(), "/c".into(), line.into()]
}

/// Whether `argv` is a `shell_line`: cmd itself, which never reads the
/// usual argument quoting, so its line must go raw.
pub fn is_shell_line(argv: &[String]) -> bool {
    let is_cmd = |program: &String| {
        let name = std::path::Path::new(program).file_name().and_then(|f| f.to_str()).unwrap_or_default();
        name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd")
    };
    argv.len() == 5 && is_cmd(&argv[0]) && argv[1..4] == ["/d", "/s", "/c"]
}

/// Returns once the child exited (a job holds what it started, so reaping
/// it early is safe).
pub async fn exited(child: &mut tokio::process::Child) {
    let _ = child.wait().await;
}

pub fn exec_group(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

#[derive(Clone)]
pub struct ExecGroup(Arc<OwnedHandle>);

impl ExecGroup {
    /// Puts the suspended child in a new job, then lets it run. An error
    /// leaves it suspended, for the caller to end.
    pub fn new(child: &tokio::process::Child) -> io::Result<ExecGroup> {
        let (Some(process), Some(pid)) = (child.raw_handle(), child.id()) else {
            return Err(io::Error::other("it is gone already"));
        };
        let job = unsafe { join_job(process as HANDLE) }.ok_or_else(io::Error::last_os_error)?;
        match unsafe { resume(pid) } {
            0 => Err(io::Error::other("its threads could not be resumed")),
            _ => Ok(ExecGroup(Arc::new(job))),
        }
    }

    pub fn end(&self) {
        unsafe { TerminateJobObject(self.0.as_raw_handle() as HANDLE, 1) };
    }

    pub fn kill(&self) {
        self.end();
    }
}

unsafe fn join_job(process: HANDLE) -> Option<OwnedHandle> {
    unsafe {
        let job = CreateJobObjectW(null(), null());
        if job.is_null() {
            return None;
        }
        let job = OwnedHandle::from_raw_handle(job as RawHandle);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        (set != 0 && AssignProcessToJobObject(job.as_raw_handle() as HANDLE, process) != 0).then_some(job)
    }
}

/// Resumes the threads of a process started suspended (std keeps its main
/// thread's handle to itself); returns how many.
unsafe fn resume(pid: u32) -> usize {
    let mut resumed = 0;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return 0;
        }
        let snapshot = OwnedHandle::from_raw_handle(snapshot as RawHandle);
        let mut entry: THREADENTRY32 = zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut more = Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry);
        while more != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    resumed += usize::from(ResumeThread(thread) != u32::MAX);
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry);
        }
    }
    resumed
}
