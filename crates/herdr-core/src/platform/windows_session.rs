//! Keep implicit startup from reserving a desktop user's core at high integrity.

use crate::rpc::RpcError;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::RemoteDesktop::*;
use windows_sys::Win32::System::Threading::*;

pub fn check_autostart() -> Result<(), RpcError> {
    let message = match unsafe { elevated_desktop_user() } {
        Ok(false) => return Ok(()),
        Ok(true) => "refusing to start an elevated Windows core while this user has a desktop session: the desktop app could not access it".into(),
        Err(e) => format!("cannot check whether an elevated core would block the Windows desktop: {e}"),
    };
    Err(RpcError::new(
        "unavailable",
        message,
        "open Herdr on that Windows machine, or run 'hc server start' there in a normal desktop terminal; then reconnect",
    ))
}

unsafe fn elevated_desktop_user() -> io::Result<bool> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token.cast());
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut n = 0;
        if GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        if elevation.TokenIsElevated == 0 {
            return Ok(false);
        }
        GetTokenInformation(token.as_raw_handle() as HANDLE, TokenUser, null_mut(), 0, &mut n);
        let mut user = vec![0usize; (n as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(token.as_raw_handle() as HANDLE, TokenUser, user.as_mut_ptr().cast(), n, &mut n) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        let mut sessions = null_mut();
        let mut count = 0;
        if WTSEnumerateSessionsW(null_mut(), 0, 1, &mut sessions, &mut count) == 0 {
            return Err(io::Error::last_os_error());
        }
        let found = std::slice::from_raw_parts(sessions, count as usize).iter().any(|session| {
            if session.SessionId == 0 {
                return false;
            }
            let name = session_text(session.SessionId, WTSUserName);
            if name.is_empty() {
                return false;
            }
            let domain = session_text(session.SessionId, WTSDomainName);
            let account: Vec<u16> = format!("{domain}\\{name}").encode_utf16().chain(Some(0)).collect();
            let (mut size, mut domain_size, mut kind) = (0, 0, 0);
            LookupAccountNameW(
                null(),
                account.as_ptr(),
                null_mut(),
                &mut size,
                null_mut(),
                &mut domain_size,
                &mut kind,
            );
            let mut other_sid = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
            let mut domain = vec![0u16; domain_size as usize];
            LookupAccountNameW(
                null(),
                account.as_ptr(),
                other_sid.as_mut_ptr().cast(),
                &mut size,
                domain.as_mut_ptr(),
                &mut domain_size,
                &mut kind,
            ) != 0
                && EqualSid(sid, other_sid.as_ptr().cast_mut().cast()) != 0
        });
        WTSFreeMemory(sessions.cast());
        Ok(found)
    }
}

unsafe fn session_text(session: u32, class: WTS_INFO_CLASS) -> String {
    unsafe {
        let mut text = null_mut();
        let mut bytes = 0;
        if WTSQuerySessionInformationW(null_mut(), session, class, &mut text, &mut bytes) == 0 {
            return String::new();
        }
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(text, (bytes as usize / 2).saturating_sub(1)));
        WTSFreeMemory(text.cast());
        result
    }
}
