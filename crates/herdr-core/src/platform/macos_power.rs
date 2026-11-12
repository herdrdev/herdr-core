//! Prevent idle system sleep. Display sleep and manual sleep remain available.

use std::ffi::{c_char, c_void};
use std::io;
use std::time::Duration;

type CFType = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: CFType, text: *const c_char, encoding: u32) -> CFType;
    fn CFRelease(value: CFType);
    fn CFNumberCreate(allocator: CFType, kind: isize, value: *const c_void) -> CFType;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(kind: CFType, level: u32, name: CFType, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
    fn IOPMAssertionSetProperty(id: u32, key: CFType, value: CFType) -> i32;
}

struct Text(CFType);

impl Text {
    fn new(text: &str) -> io::Result<Self> {
        let text = std::ffi::CString::new(text).map_err(io::Error::other)?;
        let value = unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), 0x08000100) };
        if value.is_null() { Err(io::Error::other("cannot create assertion text")) } else { Ok(Self(value)) }
    }
}

impl Drop for Text {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

pub struct IdleSleep {
    id: u32,
    working: usize,
    deadline: Duration,
}

impl IdleSleep {
    pub fn new(working: usize, deadline: Duration, left: Duration) -> io::Result<Self> {
        let kind = Text::new("PreventUserIdleSystemSleep")?;
        let name = Text::new(&format!("herdr-core: {working} terminals working"))?;
        let mut id = 0;
        check(unsafe { IOPMAssertionCreateWithName(kind.0, 255, name.0, &mut id) })?;
        let assertion = Self { id, working, deadline };
        assertion.timeout(left)?;
        Ok(assertion)
    }

    pub fn update(&mut self, working: usize, deadline: Duration, left: Duration) -> io::Result<()> {
        if deadline != self.deadline {
            self.timeout(left)?;
            self.deadline = deadline;
        }
        if working != self.working {
            let key = Text::new("AssertName")?;
            let name = Text::new(&format!("herdr-core: {working} terminals working"))?;
            check(unsafe { IOPMAssertionSetProperty(self.id, key.0, name.0) })?;
            self.working = working;
        }
        Ok(())
    }

    fn timeout(&self, left: Duration) -> io::Result<()> {
        let key = Text::new("TimeoutSeconds")?;
        let seconds = left.as_secs_f64();
        let number = unsafe { CFNumberCreate(std::ptr::null(), 13, std::ptr::addr_of!(seconds).cast()) };
        if number.is_null() {
            return Err(io::Error::other("cannot create assertion timeout"));
        }
        let result = unsafe { IOPMAssertionSetProperty(self.id, key.0, number) };
        unsafe { CFRelease(number) };
        check(result)
    }
}

fn check(result: i32) -> io::Result<()> {
    if result == 0 { Ok(()) } else { Err(io::Error::other(format!("IOPM assertion: {result:#x}"))) }
}

impl Drop for IdleSleep {
    fn drop(&mut self) {
        unsafe { IOPMAssertionRelease(self.id) };
    }
}

pub fn clock() -> Duration {
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    // Darwin CLOCK_MONOTONIC includes sleep; CLOCK_UPTIME_RAW does not.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        panic!("CLOCK_MONOTONIC unavailable: {}", io::Error::last_os_error());
    }
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCopyProperties(id: u32) -> CFType;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDictionaryGetValue(dictionary: CFType, key: CFType) -> CFType;
        fn CFEqual(a: CFType, b: CFType) -> u8;
    }

    #[test]
    fn the_native_assertion_has_the_right_type_name_timeout_and_lifetime() {
        let left = Duration::from_secs(1);
        let mut assertion = IdleSleep::new(2, clock() + left, left).unwrap();
        let id = assertion.id;
        let properties = unsafe { IOPMAssertionCopyProperties(id) };
        assert!(!properties.is_null());
        for (key, value) in
            [("AssertType", "PreventUserIdleSystemSleep"), ("AssertName", "herdr-core: 2 terminals working")]
        {
            let key = Text::new(key).unwrap();
            let value = Text::new(value).unwrap();
            assert_ne!(unsafe { CFEqual(CFDictionaryGetValue(properties, key.0), value.0) }, 0);
        }
        unsafe { CFRelease(properties) };
        assertion.update(1, assertion.deadline, Duration::from_millis(500)).unwrap();
        let properties = unsafe { IOPMAssertionCopyProperties(id) };
        let key = Text::new("AssertName").unwrap();
        let name = Text::new("herdr-core: 1 terminals working").unwrap();
        assert_ne!(unsafe { CFEqual(CFDictionaryGetValue(properties, key.0), name.0) }, 0);
        unsafe { CFRelease(properties) };
        // The OS also bounds the assertion if the core cannot run its timer.
        std::thread::sleep(Duration::from_millis(1300));
        // CopyProperties caches the original level locally. Read powerd's observed state instead.
        let output = std::process::Command::new("/usr/bin/pmset").args(["-g", "assertions"]).output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        let owner = format!("pid {}(", std::process::id());
        assert!(
            !output.lines().any(|line| line.contains(&owner) && line.contains("herdr-core: 1 terminals working")),
            "{output}"
        );
        drop(assertion);
        assert!(unsafe { IOPMAssertionCopyProperties(id) }.is_null());
    }
}
