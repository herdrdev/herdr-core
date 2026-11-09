//! Each holder keeps its coalition represented after its GUI children quit.

use std::ffi::{c_char, c_int, c_void};
use std::io;

#[repr(C)]
#[derive(Default)]
struct AuditInfo {
    user: u32,
    mask: [u32; 2],
    terminal: [u32; 6],
    session: u32,
    flags: u64,
}

unsafe extern "C" {
    fn getaudit_addr(info: *mut AuditInfo, size: c_int) -> c_int;
}

pub fn check_in() -> io::Result<()> {
    if !super::macos_bundle::source_is_bundle(&std::env::current_exe()?.canonicalize()?) {
        return Ok(());
    }
    let mut audit = AuditInfo::default();
    if unsafe { getaudit_addr(&mut audit, std::mem::size_of::<AuditInfo>() as c_int) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // AppKit cannot connect without a window server. SSH holders retain their headless startup.
    if audit.flags & 0x10 == 0 {
        return Ok(());
    }
    if !gui_session_usable(audit.session) {
        return Err(io::Error::other("GUI login unavailable; retaining headless holder startup"));
    }
    unsafe {
        let framework = libc::dlopen(
            c"/System/Library/Frameworks/AppKit.framework/AppKit".as_ptr(),
            libc::RTLD_LAZY | libc::RTLD_GLOBAL,
        );
        if framework.is_null() {
            return Err(io::Error::other("cannot load AppKit"));
        }
        let class = libc::dlsym(framework, c"objc_getClass".as_ptr());
        let selector = libc::dlsym(framework, c"sel_registerName".as_ptr());
        let send = libc::dlsym(framework, c"objc_msgSend".as_ptr());
        if class.is_null() || selector.is_null() || send.is_null() {
            return Err(io::Error::other("AppKit Objective-C runtime unavailable"));
        }
        let class: unsafe extern "C" fn(*const c_char) -> *mut c_void = std::mem::transmute(class);
        let selector: unsafe extern "C" fn(*const c_char) -> *mut c_void = std::mem::transmute(selector);
        let object: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(send);
        let policy: unsafe extern "C" fn(*mut c_void, *mut c_void, isize) -> bool = std::mem::transmute(send);
        let finish: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(send);
        let pool = object(class(c"NSAutoreleasePool".as_ptr()), selector(c"new".as_ptr()));
        let app = object(class(c"NSApplication".as_ptr()), selector(c"sharedApplication".as_ptr()));
        let accessory = policy(app, selector(c"setActivationPolicy:".as_ptr()), 1);
        finish(app, selector(c"finishLaunching".as_ptr()));
        finish(pool, selector(c"drain".as_ptr()));
        if app.is_null() || !accessory {
            return Err(io::Error::other("cannot register holder as an accessory application"));
        }
    }
    Ok(())
}

/// A graphical-access flag can outlive the login's window server and bootstrap services.
pub fn gui_session_usable(session: u32) -> bool {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let Ok(mut child) = Command::new("/bin/launchctl")
        .args(["print", &format!("login/{session}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(1) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_gui_login_is_not_eligible_for_appkit() {
        assert!(!gui_session_usable(u32::MAX));
    }

    #[test]
    fn audit_identity_is_readable_without_privileged_process_inspection() {
        let mut info = AuditInfo::default();
        assert_eq!(unsafe { getaudit_addr(&mut info, std::mem::size_of::<AuditInfo>() as c_int) }, 0);
        assert_ne!(info.session, 0);
    }
}
