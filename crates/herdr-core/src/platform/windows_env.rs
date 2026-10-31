//! Fresh user/system settings without losing process-local runtime variables.

use std::collections::BTreeMap;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub fn validate_env(env: &BTreeMap<String, String>) -> Result<(), crate::rpc::RpcError> {
    for (key, value) in env.iter() {
        // A leading '=' belongs to Windows' hidden variables, such as '=C:'.
        if key.is_empty()
            || key.contains('\0')
            || key.strip_prefix('=').unwrap_or(key).contains('=')
            || value.contains('\0')
        {
            return Err(crate::rpc::RpcError::invalid_params(format!("invalid Windows environment entry {key:?}")));
        }
    }
    Ok(())
}

pub(super) fn refresh(env: &mut BTreeMap<String, String>) -> io::Result<()> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token.cast());
        let mut block = null_mut();
        // Windows reads the registry, expands REG_EXPAND_SZ and joins system/user PATH.
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
    }
    Ok(())
}
