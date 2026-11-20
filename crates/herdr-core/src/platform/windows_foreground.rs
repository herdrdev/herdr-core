//! A pseudo console has no foreground process group. What stands in for one:
//! the newest live program a known shell root started itself. A directly
//! launched program keeps its place when it starts tools or helper processes.

use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::Packaging::Appx::GetPackageFullName;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::JobObjects::{JobObjectBasicProcessIdList, QueryInformationJobObject};
use windows_sys::Win32::System::Threading::*;

/// A process in the terminal's job.
struct Proc {
    pid: u32,
    parent: u32,
    started: u64,
    name: String,
}

/// The pid in front of terminal `root`, whose job holds everything it started.
/// Except for a packaged root, such as PowerShell from the Microsoft Store:
/// Windows starts a packaged app's programs outside the job, whatever the job
/// allows, so only for such a root its children are looked for among all
/// processes.
pub(super) fn in_front(job: HANDLE, root: u32) -> u32 {
    let mut procs = unsafe { job_processes(job) };
    if unsafe { packaged(root) } {
        procs.extend(unsafe { children(root) });
    }
    newest_child(root, &procs).unwrap_or(root)
}

/// A known shell root's newest child, console hosts aside (Windows starts
/// them for console programs; they are never what a person is using).
fn newest_child(root: u32, procs: &[Proc]) -> Option<u32> {
    let shell = |name: &str| {
        let name = name.to_ascii_lowercase();
        matches!(
            name.strip_suffix(".exe").unwrap_or(&name),
            "zsh"
                | "bash"
                | "fish"
                | "sh"
                | "dash"
                | "nu"
                | "pwsh"
                | "powershell"
                | "cmd"
                | "elvish"
                | "xonsh"
                | "tcsh"
                | "csh"
                | "ksh"
                | "ion"
        )
    };
    let root = procs.iter().find(|p| p.pid == root && shell(&p.name))?;
    let host = |name: &str| ["conhost.exe", "OpenConsole.exe"].iter().any(|h| name.eq_ignore_ascii_case(h));
    // A process older than the shell names it as parent only through a reused pid.
    procs
        .iter()
        .filter(|p| p.parent == root.pid && p.pid != root.pid && p.started >= root.started && !host(&p.name))
        .max_by_key(|p| p.started)
        .map(|p| p.pid)
}

/// Whether `pid` runs as a packaged app, the way the Microsoft Store installs them.
unsafe fn packaged(pid: u32) -> bool {
    unsafe {
        let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if raw.is_null() {
            return false;
        }
        let _process = OwnedHandle::from_raw_handle(raw);
        let mut len = 0;
        GetPackageFullName(raw as HANDLE, &mut len, null_mut()) == ERROR_INSUFFICIENT_BUFFER
    }
}

/// `root`'s children among all processes, from a system snapshot.
unsafe fn children(root: u32) -> Vec<Proc> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let snap = OwnedHandle::from_raw_handle(snap);
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut pids = Vec::new();
        let mut ok = Process32FirstW(snap.as_raw_handle() as HANDLE, &mut entry);
        while ok != 0 {
            if entry.th32ParentProcessID == root && entry.th32ProcessID != root {
                pids.push(entry.th32ProcessID);
            }
            ok = Process32NextW(snap.as_raw_handle() as HANDLE, &mut entry);
        }
        pids.into_iter().filter_map(|pid| about(pid)).collect()
    }
}

/// The job's live processes: a few per terminal, so read one by one (a
/// whole-system snapshot would cost more, twice a second per terminal).
unsafe fn job_processes(job: HANDLE) -> Vec<Proc> {
    #[repr(C)]
    struct Ids {
        assigned: u32,
        listed: u32,
        ids: [usize; 64],
    }
    unsafe {
        let mut list: Ids = zeroed();
        // More than fit fails with the list filled as far as it goes: enough.
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            (&raw mut list).cast(),
            size_of::<Ids>() as u32,
            null_mut(),
        );
        let n = (list.listed as usize).min(list.ids.len());
        list.ids[..n].iter().filter_map(|&pid| about(pid as u32)).collect()
    }
}

unsafe fn about(pid: u32) -> Option<Proc> {
    unsafe {
        let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if raw.is_null() {
            return None;
        }
        let _process = OwnedHandle::from_raw_handle(raw);
        let h = raw as HANDLE;
        let mut basic: PROCESS_BASIC_INFORMATION = zeroed();
        let size = size_of::<PROCESS_BASIC_INFORMATION>() as u32;
        if NtQueryInformationProcess(h, ProcessBasicInformation, (&raw mut basic).cast(), size, null_mut()) != 0 {
            return None;
        }
        let [mut started, mut exited, mut kernel, mut user]: [FILETIME; 4] = zeroed();
        if GetProcessTimes(h, &mut started, &mut exited, &mut kernel, &mut user) == 0 {
            return None;
        }
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len) == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&path[..len as usize]);
        Some(Proc {
            pid,
            parent: basic.InheritedFromUniqueProcessId as u32,
            started: (started.dwHighDateTime as u64) << 32 | started.dwLowDateTime as u64,
            name: path.rsplit('\\').next().unwrap_or_default().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_program_the_shell_started_is_in_front() {
        let p = |pid, parent, started, name: &str| Proc { pid, parent, started, name: name.into() };
        let shell = p(10, 1, 1, "powershell.exe");
        assert_eq!(newest_child(10, &[p(10, 1, 1, "powershell.exe")]), None, "only the shell: it is in front");
        let procs = [
            shell,
            p(11, 10, 2, "node.exe"),
            p(12, 10, 3, "claude.exe"),
            p(13, 10, 4, "conhost.exe"),
            p(14, 12, 5, "node.exe"), // the agent's own helper
        ];
        assert_eq!(newest_child(10, &procs), Some(12));
    }

    #[test]
    fn a_process_older_than_the_shell_is_not_its_child() {
        // Found outside the job, a process may name the shell's pid as parent because its
        // own parent ended and Windows gave that pid to the shell.
        let p = |pid, parent, started, name: &str| Proc { pid, parent, started, name: name.into() };
        let procs = [p(10, 1, 5, "pwsh.exe"), p(11, 10, 3, "stale.exe"), p(12, 10, 6, "ping.exe")];
        assert_eq!(newest_child(10, &procs), Some(12));
        assert_eq!(newest_child(10, &procs[..2]), None);
    }

    #[test]
    fn a_direct_program_keeps_its_place_when_it_starts_a_helper() {
        let p = |pid, parent, name: &str| Proc { pid, parent, started: pid as u64, name: name.into() };
        assert_eq!(newest_child(10, &[p(10, 1, "node.exe"), p(11, 10, "ping.exe")]), None);
        for shell in ["cmd.exe", "PwSh.EXE", "powershell.exe", "nu.exe", "bash", "sh.exe"] {
            assert_eq!(newest_child(10, &[p(10, 1, shell), p(11, 10, "node.exe")]), Some(11), "{shell}");
        }
    }
}
