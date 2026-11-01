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

/// A shell line as this machine runs one: cmd reads the rest of its command
/// line itself (`/s`: verbatim between the outer quotes), so the line goes
/// raw, never quoted as an argument.
pub fn shell_line(line: &str) -> Vec<String> {
    let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    vec![cmd, "/d".into(), "/s".into(), "/c".into(), line.into()]
}

/// A machine's `--via` command. There is no sh here to read it as a line, and
/// cmd would read the line's quotes and `|`: it is a program and its arguments,
/// split on spaces (double quotes keep a path with spaces whole), given `line`
/// as its last argument. No console of its own: the core runs detached.
pub fn via_command(target: &str, line: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let words = via_words(target);
    let mut cmd = std::process::Command::new(words.first().map_or("", String::as_str));
    cmd.args(words.iter().skip(1)).arg(line).creation_flags(CREATE_NO_WINDOW);
    cmd
}

fn via_words(target: &str) -> Vec<String> {
    let (mut words, mut word, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    for c in target.chars() {
        match c {
            '"' => (quoted, started) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_via_command_is_its_words_then_the_line_as_one_argument() {
        let cmd = super::via_command(r#""C:\Program Files\WSL\wsl.exe" -d Ubuntu  -e sh -c"#, "sh -c 'a \"$1\" | b'");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(cmd.get_program(), r"C:\Program Files\WSL\wsl.exe");
        assert_eq!(args, ["-d", "Ubuntu", "-e", "sh", "-c", "sh -c 'a \"$1\" | b'"]);
    }
}
