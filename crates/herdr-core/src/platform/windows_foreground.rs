//! A pseudo console has no foreground process group. What stands in for one:
//! the newest live program a known shell root started itself. A directly
//! launched program keeps its place when it starts tools or helper processes.

use std::mem::{size_of, zeroed};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr::null_mut;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
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
pub(super) fn in_front(job: HANDLE, root: u32) -> u32 {
    newest_child(root, &unsafe { job_processes(job) }).unwrap_or(root)
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
    if !procs.iter().any(|p| p.pid == root && shell(&p.name)) {
        return None;
    }
    let host = |name: &str| ["conhost.exe", "OpenConsole.exe"].iter().any(|h| name.eq_ignore_ascii_case(h));
    procs
        .iter()
        .filter(|p| p.parent == root && p.pid != root && !host(&p.name))
        .max_by_key(|p| p.started)
        .map(|p| p.pid)
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
    fn a_direct_program_keeps_its_place_when_it_starts_a_helper() {
        let p = |pid, parent, name: &str| Proc { pid, parent, started: pid as u64, name: name.into() };
        assert_eq!(newest_child(10, &[p(10, 1, "node.exe"), p(11, 10, "ping.exe")]), None);
        for shell in ["cmd.exe", "PwSh.EXE", "powershell.exe", "nu.exe", "bash", "sh.exe"] {
            assert_eq!(newest_child(10, &[p(10, 1, shell), p(11, 10, "node.exe")]), Some(11), "{shell}");
        }
    }
}
