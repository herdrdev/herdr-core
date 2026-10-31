//! Windows process observation: shell children and native/WoW64 process parameters.

/// The folder and command line Windows keeps in this process's memory.
pub fn about(pid: i32) -> Option<crate::foreground::Foreground> {
    use crate::foreground::Foreground;
    use std::ffi::c_void;
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessBasicInformation, ProcessWow64Information,
    };
    use windows_sys::Win32::Foundation::{HANDLE, LocalFree, UNICODE_STRING};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Kernel::STRING32;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid as u32) };
    if raw.is_null() {
        return None;
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let h = process.as_raw_handle() as HANDLE;
    let read = |at: *const c_void, into: *mut c_void, len: usize| {
        let mut n = 0;
        unsafe { ReadProcessMemory(h, at, into, len, &mut n) != 0 && n == len }
    };
    fn value<T>(read: impl Fn(*const c_void, *mut c_void, usize) -> bool, at: *const c_void) -> Option<T> {
        let mut v = MaybeUninit::<T>::uninit();
        read(at, v.as_mut_ptr().cast(), size_of::<T>()).then(|| unsafe { v.assume_init() })
    }
    let text = |s: UNICODE_STRING| -> Option<Vec<u16>> {
        let mut buf = vec![0u16; s.Length as usize / 2];
        read(s.Buffer.cast(), buf.as_mut_ptr().cast(), buf.len() * 2).then_some(buf)
    };

    let mut basic: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let size = size_of::<PROCESS_BASIC_INFORMATION>() as u32;
    let status =
        unsafe { NtQueryInformationProcess(h, ProcessBasicInformation, (&raw mut basic).cast(), size, &mut 0) };
    if status != 0 || basic.PebBaseAddress.is_null() {
        return None;
    }
    let mut peb32 = 0usize;
    if unsafe {
        NtQueryInformationProcess(
            h,
            ProcessWow64Information,
            (&raw mut peb32).cast(),
            size_of::<usize>() as u32,
            &mut 0,
        )
    } != 0
    {
        return None;
    }
    let (dir, image, command_line) = if peb32 != 0 {
        // WoW64 keeps the shell's current directory in its 32-bit PEB, not the native PEB.
        let params: u32 = value(read, (peb32 + 0x10) as *const c_void)?;
        let descriptor = |offset: usize| -> Option<UNICODE_STRING> {
            let s: STRING32 = value(read, (params as usize + offset) as *const c_void)?;
            Some(UNICODE_STRING {
                Length: s.Length,
                MaximumLength: s.MaximumLength,
                Buffer: s.Buffer as usize as *mut u16,
            })
        };
        (descriptor(0x24)?, descriptor(0x38)?, descriptor(0x40)?)
    } else {
        let peb: PEB = value(read, basic.PebBaseAddress.cast())?;
        let params: RTL_USER_PROCESS_PARAMETERS = value(read, peb.ProcessParameters.cast())?;
        // CurrentDirectory.DosPath follows the standard handles in Reserved2.
        let dir = value(read, peb.ProcessParameters.cast::<u8>().wrapping_add(0x38).cast())?;
        (dir, params.ImagePathName, params.CommandLine)
    };
    let mut cwd = String::from_utf16(&text(dir)?).ok()?;
    if cwd.len() > 3 && cwd.ends_with('\\') {
        cwd.pop(); // Windows keeps C:\dir\; only a drive's root has one
    }
    let image = String::from_utf16(&text(image)?).ok()?;
    let name = image.rsplit('\\').next().unwrap_or(&image).to_string();

    let mut line = text(command_line)?;
    line.push(0);
    let mut argc = 0;
    let list = unsafe { CommandLineToArgvW(line.as_ptr(), &mut argc) };
    if list.is_null() {
        return None;
    }
    let argv = (0..argc as usize)
        .map(|i| unsafe {
            let arg = *list.add(i);
            String::from_utf16_lossy(std::slice::from_raw_parts(arg, (0..).take_while(|&n| *arg.add(n) != 0).count()))
        })
        .collect();
    unsafe { LocalFree(list.cast()) };
    Some(Foreground { pid, name, argv, cwd })
}

/// ConPTY has no foreground group. Follow a unique shell child through launchers;
/// when several shells are running, keep the last unambiguous process.
pub fn shell_pid(pid: u32) -> u32 {
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    let Some(host) = console_host(pid) else { return pid };
    let mut shells = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return pid;
        }
        let snapshot = OwnedHandle::from_raw_handle(snapshot);
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snapshot.as_raw_handle() as HANDLE, &mut entry);
        while ok != 0 {
            let end = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "cmd.exe" | "powershell.exe" | "pwsh.exe" | "sh.exe" | "bash.exe" | "zsh.exe" | "fish.exe" | "nu.exe"
            ) {
                shells.push((entry.th32ProcessID, entry.th32ParentProcessID));
            }
            ok = Process32NextW(snapshot.as_raw_handle() as HANDLE, &mut entry);
        }
    }
    unique_shell(pid, shells, |child| console_host(child) == Some(host))
}

fn console_host(pid: u32) -> Option<usize> {
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Wdk::System::Threading::NtQueryInformationProcess;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut host = 0usize;
    // ProcessConsoleHostProcess (49): the host PID with flags in its low two bits.
    let status = unsafe {
        NtQueryInformationProcess(handle.as_raw_handle(), 49, (&raw mut host).cast(), size_of::<usize>() as u32, &mut 0)
    };
    (status == 0 && host & !3 != 0).then_some(host & !3)
}

fn unique_shell(mut pid: u32, mut shells: Vec<(u32, u32)>, same_console: impl Fn(u32) -> bool) -> u32 {
    loop {
        let mut children = shells.iter().filter(|(child, parent)| *parent == pid && same_console(*child));
        let Some(&(child, _)) = children.next() else { return pid };
        if children.next().is_some() {
            return pid;
        }
        // Remove visited entries so even a snapshot with recycled parent PIDs terminates.
        shells.retain(|(p, _)| *p != pid);
        pid = child;
    }
}

#[cfg(test)]
mod tests {
    use super::unique_shell;

    #[test]
    fn follows_shell_chains_but_does_not_guess_between_siblings() {
        assert_eq!(unique_shell(1, vec![(2, 1), (3, 2)], |_| true), 3);
        assert_eq!(unique_shell(1, vec![(2, 1), (3, 2), (4, 2)], |_| true), 2);
        assert_eq!(unique_shell(1, vec![], |_| true), 1);
        assert_eq!(unique_shell(1, vec![(2, 1), (3, 1)], |pid| pid == 3), 3);
        assert_eq!(unique_shell(1, vec![(2, 1), (1, 2)], |_| true), 2);
    }
}
