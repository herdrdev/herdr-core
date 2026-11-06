//! Fresh user/system settings without losing process-local runtime variables.

use std::collections::BTreeMap;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
use windows_sys::Win32::System::Environment::{
    CreateEnvironmentBlock, DestroyEnvironmentBlock, ExpandEnvironmentStringsForUserW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub(super) fn refresh(env: &mut BTreeMap<String, String>) -> io::Result<()> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token.cast());
        let mut block = null_mut();
        // Windows reads the registry and joins system/user PATH.
        if CreateEnvironmentBlock(&mut block, token.as_raw_handle() as HANDLE, 0) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut at = block.cast::<u16>();
        while *at != 0 {
            let len = (0..).take_while(|&i| *at.add(i) != 0).count();
            let entry = String::from_utf16_lossy(std::slice::from_raw_parts(at, len));
            if let Some((key, value)) = entry.split_once('=')
                && !key.is_empty()
            {
                env.retain(|k, _| !k.eq_ignore_ascii_case(key));
                env.insert(key.into(), value.into());
            }
            at = at.add(len + 1);
        }
        DestroyEnvironmentBlock(block);
        if let Some((_, path)) = env.iter_mut().find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
            && path.contains('%')
        {
            // userenv can leave references to late-added variables such as ProgramFiles in PATH.
            *path = expand_path(path, token.as_raw_handle() as HANDLE)?;
        }
    }
    Ok(())
}

fn expand_path(path: &str, token: HANDLE) -> io::Result<String> {
    let source: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let mut expanded = vec![0u16; 32768];
    if unsafe { ExpandEnvironmentStringsForUserW(token, source.as_ptr(), expanded.as_mut_ptr(), expanded.len() as u32) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    let len = expanded.iter().position(|&c| c == 0).unwrap_or(expanded.len());
    Ok(String::from_utf16_lossy(&expanded[..len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_expands_user_and_late_added_system_variables() {
        unsafe {
            let mut token = null_mut();
            assert_ne!(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token), 0);
            let token = OwnedHandle::from_raw_handle(token.cast());
            let path = expand_path(
                r"%ProgramFiles%\PowerShell\7;%APPDATA%\npm;%HC_UNKNOWN_WINPATH_VAR%",
                token.as_raw_handle() as HANDLE,
            )
            .unwrap();
            assert_eq!(
                path,
                format!(
                    r"{}\PowerShell\7;{}\npm;%HC_UNKNOWN_WINPATH_VAR%",
                    std::env::var("ProgramFiles").unwrap(),
                    std::env::var("APPDATA").unwrap()
                )
            );
        }
    }
}
