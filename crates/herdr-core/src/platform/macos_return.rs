//! The person is back at this Mac: powerd posts its user-activity level, and
//! it reads active after a full wake or the first input after idling. A
//! DarkWake has no user, so the background wakes of a sleeping Mac stay quiet.

use std::ffi::{CStr, c_char, c_int};
use std::io;

const USER_ACTIVITY: &CStr = c"com.apple.system.powermanagement.useractivity2";
/// kIOPMUserPresentActive (IOPMLibPrivate.h): the person is using the Mac.
const PRESENT_ACTIVE: u64 = 1;

// libSystem provides notify(3).
unsafe extern "C" {
    fn notify_register_file_descriptor(name: *const c_char, fd: *mut c_int, flags: c_int, token: *mut c_int) -> u32;
    fn notify_get_state(token: c_int, state: *mut u64) -> u32;
}

/// Calls `returned`, on a thread of its own, each time the person becomes active.
pub fn watch(returned: fn()) -> io::Result<()> {
    watch_name(USER_ACTIVITY, returned)
}

fn watch_name(name: &CStr, returned: fn()) -> io::Result<()> {
    let (mut fd, mut token) = (-1, -1);
    let status = unsafe { notify_register_file_descriptor(name.as_ptr(), &mut fd, 0, &mut token) };
    if status != 0 {
        return Err(io::Error::other(format!("notify_register_file_descriptor: {status}")));
    }
    std::thread::Builder::new().name("hc-person-returns".into()).spawn(move || {
        // Each post writes the token (4 bytes); the level is the name's state.
        let mut posted = [0u8; 4];
        while unsafe { libc::read(fd, posted.as_mut_ptr().cast(), posted.len()) } == posted.len() as isize {
            let mut state = 0;
            if unsafe { notify_get_state(token, &mut state) } == 0 && state & PRESENT_ACTIVE != 0 {
                returned();
            }
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    unsafe extern "C" {
        fn notify_register_check(name: *const c_char, token: *mut c_int) -> u32;
        fn notify_set_state(token: c_int, state: u64) -> u32;
        fn notify_post(name: *const c_char) -> u32;
    }

    static RETURNS: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn only_an_active_level_means_the_person_returned() {
        let name =
            std::ffi::CString::new(format!("dev.herdr.test.{}.useractivity", super::super::random_hex())).unwrap();
        watch_name(&name, || {
            RETURNS.fetch_add(1, Ordering::Release);
        })
        .unwrap();
        let mut token = -1;
        assert_eq!(unsafe { notify_register_check(name.as_ptr(), &mut token) }, 0);
        let post = |level: u64| {
            assert_eq!(unsafe { notify_set_state(token, level) }, 0);
            assert_eq!(unsafe { notify_post(name.as_ptr()) }, 0);
        };
        let returns_reach = |n: usize| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while RETURNS.load(Ordering::Acquire) < n {
                assert!(std::time::Instant::now() < deadline, "notification not delivered");
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        // The level is read when a post is handled: let each idle one settle first.
        let idle = |level: u64, returns: usize| {
            post(level);
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert_eq!(RETURNS.load(Ordering::Acquire), returns, "level {level} is not a return");
        };
        idle(8, 0); // kIOPMUserNotPresent
        post(PRESENT_ACTIVE);
        returns_reach(1);
        idle(2, 1); // kIOPMUserPresentPassive: idle, display on
        post(PRESENT_ACTIVE);
        returns_reach(2);
    }
}
