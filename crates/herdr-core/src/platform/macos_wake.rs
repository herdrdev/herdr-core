//! IOKit system wake notifications for a headless core, without polling or an
//! AppKit application. Sleep acknowledgements must never wait for networking.

use std::ffi::c_void;
use std::io;
use tokio::sync::watch;

const CAN_SLEEP: u32 = 0xe000_0270;
const WILL_SLEEP: u32 = 0xe000_0280;
const HAS_POWERED_ON: u32 = 0xe000_0300;

struct Context {
    connection: u32,
    events: watch::Sender<u64>,
}

impl Context {
    fn event(&self, message: u32) -> bool {
        if message == HAS_POWERED_ON {
            self.events.send_modify(|generation| *generation = generation.wrapping_add(1));
        }
        matches!(message, CAN_SLEEP | WILL_SLEEP)
    }
}

unsafe extern "C" fn callback(context: *mut c_void, _service: u32, message: u32, argument: *mut c_void) {
    let context = unsafe { &*context.cast::<Context>() };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if context.event(message) {
            // Acknowledge even idle sleep; never veto or postpone the user's sleep.
            unsafe { IOAllowPowerChange(context.connection, argument as isize) };
        }
    }));
}

pub fn start(events: watch::Sender<u64>) {
    if let Err(error) =
        std::thread::Builder::new().name("hc-wake".into()).spawn(move || match Registration::new(events) {
            Ok(_registration) => unsafe { CFRunLoopRun() },
            Err(error) => eprintln!("core: cannot watch system wake: {error}"),
        })
    {
        eprintln!("core: cannot start system wake watcher: {error}");
    }
}

// Native handles and the callback's context stay on their owning run-loop
// thread. If setup fails (or that loop returns), tear them down in IOKit's order.
struct Registration {
    context: Box<Context>,
    port: *mut c_void,
    notifier: u32,
    run_loop: *mut c_void,
    source: *mut c_void,
}

impl Registration {
    fn new(events: watch::Sender<u64>) -> io::Result<Self> {
        let mut registration = Self {
            context: Box::new(Context { connection: 0, events }),
            port: std::ptr::null_mut(),
            notifier: 0,
            run_loop: unsafe { CFRunLoopGetCurrent() },
            source: std::ptr::null_mut(),
        };
        registration.context.connection = unsafe {
            IORegisterForSystemPower(
                std::ptr::from_mut(registration.context.as_mut()).cast(),
                &mut registration.port,
                callback,
                &mut registration.notifier,
            )
        };
        if registration.context.connection == 0 || registration.port.is_null() {
            return Err(io::Error::other("IORegisterForSystemPower failed"));
        }
        registration.source = unsafe { IONotificationPortGetRunLoopSource(registration.port) };
        if registration.source.is_null() {
            return Err(io::Error::other("IONotificationPortGetRunLoopSource failed"));
        }
        unsafe { CFRunLoopAddSource(registration.run_loop, registration.source, kCFRunLoopDefaultMode) };
        Ok(registration)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        unsafe {
            if !self.source.is_null() {
                CFRunLoopRemoveSource(self.run_loop, self.source, kCFRunLoopDefaultMode);
            }
            if self.notifier != 0 {
                IODeregisterForSystemPower(&mut self.notifier);
            }
            if !self.port.is_null() {
                IONotificationPortDestroy(self.port);
            }
            if self.context.connection != 0 {
                IOServiceClose(self.context.connection);
            }
        }
    }
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        context: *mut c_void,
        port: *mut *mut c_void,
        callback: unsafe extern "C" fn(*mut c_void, u32, u32, *mut c_void),
        notifier: *mut u32,
    ) -> u32;
    fn IOAllowPowerChange(connection: u32, notification: isize) -> i32;
    fn IODeregisterForSystemPower(notifier: *mut u32) -> i32;
    fn IONotificationPortGetRunLoopSource(port: *mut c_void) -> *mut c_void;
    fn IONotificationPortDestroy(port: *mut c_void);
    fn IOServiceClose(connection: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_is_allowed_and_only_completed_wakes_notify_every_subscriber() {
        let (events, mut first) = watch::channel(0);
        let second = events.subscribe();
        let context = Context { connection: 0, events };
        for event in [CAN_SLEEP, WILL_SLEEP] {
            assert!(context.event(event), "sleep must be acknowledged immediately");
            assert!(!first.has_changed().unwrap());
        }
        assert!(!context.event(0xe000_0320), "hardware is not ready at WillPowerOn");
        assert!(!first.has_changed().unwrap());
        assert!(!context.event(HAS_POWERED_ON), "wake must not be acknowledged");
        assert!(first.has_changed().unwrap() && second.has_changed().unwrap());
        first.borrow_and_update();
        assert!(!context.event(HAS_POWERED_ON));
        assert!(first.has_changed().unwrap(), "a later wake must be delivered too");
    }

    #[test]
    fn native_registration_can_be_created_and_released_without_sleeping() {
        let (events, _) = watch::channel(0);
        drop(Registration::new(events).unwrap());
    }
}
