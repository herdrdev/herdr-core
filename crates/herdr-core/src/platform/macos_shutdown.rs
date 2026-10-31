//! loginwindow can end agents well before launchd sends the core SIGTERM.
//! Arm only restart/shutdown, not plain logout, and wait for the point of no return.

use std::ffi::{CString, c_char, c_int, c_ulong, c_void};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const PREFIX: &str = "com.apple.system.loginwindow.";
const INTENT_LIFETIME_NS: u64 = 2 * 60 * 60 * 1_000_000_000;
const SHUTDOWN_LIFETIME_NS: u64 = 5 * 60 * 1_000_000_000;
static SHUTDOWN: OnceLock<Arc<AtomicU64>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Intent,
    Cancelled,
    NoReturn,
}

const KEYS: [(&str, Event); 6] = [
    ("restartinitiated", Event::Intent),
    ("shutdownInitiated", Event::Intent),
    ("likelyShutdown", Event::Intent),
    ("logoutcancelled", Event::Cancelled),
    ("logoutNoReturn", Event::NoReturn),
    ("shutdownNoReturn", Event::NoReturn),
];

fn observe(intent: &mut Option<u64>, event: Event, now_ns: u64) -> bool {
    match event {
        Event::Intent => {
            *intent = Some(now_ns);
            false
        }
        Event::Cancelled => {
            *intent = None;
            false
        }
        Event::NoReturn => {
            intent.take().is_some_and(|at| now_ns.checked_sub(at).is_some_and(|age| age <= INTENT_LIFETIME_NS))
        }
    }
}

fn now_ns() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // Darwin CLOCK_MONOTONIC includes sleep, so an abandoned intent still ages.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(ts.tv_nsec as u64)
}

struct Watcher {
    intent: Mutex<Option<u64>>,
    requested: Arc<AtomicU64>,
    #[cfg(test)]
    seen: std::sync::atomic::AtomicUsize,
}

impl Watcher {
    fn handle(&self, event: Event) {
        let mut intent = self.intent.lock().unwrap_or_else(|p| p.into_inner());
        if observe(&mut intent, event, now_ns()) {
            self.requested.store(now_ns(), Ordering::Release);
        }
        #[cfg(test)]
        self.seen.fetch_add(1, Ordering::Release);
    }
}

pub fn in_progress() -> bool {
    SHUTDOWN.get().is_some_and(|s| requested_recently(s.load(Ordering::Acquire), now_ns()))
}

fn requested_recently(at: u64, now_ns: u64) -> bool {
    at != 0 && now_ns.checked_sub(at).is_some_and(|age| age <= SHUTDOWN_LIFETIME_NS)
}

pub fn monitor() -> Option<Registration> {
    let requested = SHUTDOWN.get_or_init(Default::default).clone();
    let watcher = Arc::new(Watcher {
        intent: Mutex::new(None),
        requested,
        #[cfg(test)]
        seen: Default::default(),
    });
    match Registration::new(PREFIX, watcher) {
        Ok(registration) => Some(registration),
        Err(e) => {
            eprintln!("core: cannot watch host shutdown: {e}");
            None
        }
    }
}

// libSystem provides notify(3), libdispatch and the Blocks runtime.
type Queue = *mut c_void;
unsafe extern "C" {
    fn notify_register_dispatch(name: *const c_char, token: *mut c_int, queue: Queue, handler: *mut c_void) -> u32;
    fn notify_cancel(token: c_int) -> u32;
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> Queue;
    fn dispatch_release(object: Queue);
    static _NSConcreteStackBlock: [*const c_void; 32];
}

#[repr(C)]
struct Handler {
    isa: *const c_void,
    flags: c_int,
    reserved: c_int,
    invoke: unsafe extern "C" fn(*mut Handler, c_int),
    descriptor: *const Descriptor,
    watcher: *const Watcher,
    event: Event,
}

#[repr(C)]
struct Descriptor {
    reserved: c_ulong,
    size: c_ulong,
    copy: unsafe extern "C" fn(*mut Handler, *const Handler),
    dispose: unsafe extern "C" fn(*const Handler),
}

static DESCRIPTOR: Descriptor = Descriptor {
    reserved: 0,
    size: std::mem::size_of::<Handler>() as c_ulong,
    copy: copy_handler,
    dispose: dispose_handler,
};

// The Blocks runtime mutates flags concurrently; only read immutable captures,
// never form a reference to the whole block.
unsafe extern "C" fn invoke_handler(block: *mut Handler, _token: c_int) {
    let (watcher, event) =
        unsafe { (std::ptr::addr_of!((*block).watcher).read(), std::ptr::addr_of!((*block).event).read()) };
    let watcher = unsafe { &*watcher };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| watcher.handle(event)));
}

unsafe extern "C" fn copy_handler(_dst: *mut Handler, src: *const Handler) {
    // libnotify copies the block before registration returns; the heap copy owns a reference.
    unsafe { Arc::increment_strong_count(std::ptr::addr_of!((*src).watcher).read()) };
}

unsafe extern "C" fn dispose_handler(block: *const Handler) {
    unsafe { Arc::decrement_strong_count(std::ptr::addr_of!((*block).watcher).read()) };
}

pub struct Registration {
    queue: Queue,
    tokens: Vec<c_int>,
}

impl Registration {
    fn new(prefix: &str, watcher: Arc<Watcher>) -> io::Result<Self> {
        // A serial queue preserves the order in which libnotify forwards events.
        let queue = unsafe { dispatch_queue_create(c"hc.host-shutdown".as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            return Err(io::Error::other("dispatch_queue_create failed"));
        }
        let mut registration = Self { queue, tokens: Vec::with_capacity(KEYS.len()) };
        for (key, event) in KEYS {
            let name = CString::new(format!("{prefix}{key}")).map_err(io::Error::other)?;
            let mut handler = Handler {
                isa: std::ptr::addr_of!(_NSConcreteStackBlock).cast(),
                flags: 1 << 25, // BLOCK_HAS_COPY_DISPOSE
                reserved: 0,
                invoke: invoke_handler,
                descriptor: &DESCRIPTOR,
                watcher: Arc::as_ptr(&watcher),
                event,
            };
            let mut token = -1;
            let status = unsafe {
                notify_register_dispatch(name.as_ptr(), &mut token, queue, std::ptr::addr_of_mut!(handler).cast())
            };
            if status != 0 {
                return Err(io::Error::other(format!("notify_register_dispatch({key}): {status}")));
            }
            registration.tokens.push(token);
        }
        Ok(registration)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        for token in &self.tokens {
            unsafe { notify_cancel(*token) };
        }
        unsafe { dispatch_release(self.queue) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_uncancelled_recent_shutdown_intent_arms_no_return() {
        let mut intent = None;
        assert!(!observe(&mut intent, Event::NoReturn, 0), "plain logout");
        assert!(!observe(&mut intent, Event::Intent, 1));
        assert!(!observe(&mut intent, Event::Cancelled, 2));
        assert!(!observe(&mut intent, Event::NoReturn, 3), "cancelled restart");
        assert!(!observe(&mut intent, Event::Intent, 4));
        assert!(!observe(&mut intent, Event::NoReturn, INTENT_LIFETIME_NS + 5), "abandoned restart");
        assert!(!observe(&mut intent, Event::Intent, 6));
        assert!(observe(&mut intent, Event::NoReturn, 6 + INTENT_LIFETIME_NS));
        assert!(!observe(&mut intent, Event::NoReturn, 6 + INTENT_LIFETIME_NS), "intent consumed");
    }

    #[test]
    fn abandoned_shutdown_requests_expire() {
        assert!(!requested_recently(0, 1));
        assert!(requested_recently(1, 1));
        assert!(requested_recently(1, 1 + SHUTDOWN_LIFETIME_NS));
        assert!(!requested_recently(1, 2 + SHUTDOWN_LIFETIME_NS));
        assert!(!requested_recently(2, 1));
    }

    unsafe extern "C" {
        fn notify_post(name: *const c_char) -> u32;
    }

    #[test]
    fn private_loginwindow_notifications_keep_shutdown_exits_but_not_explicit_kills() {
        // Shutdown evidence is process-global; keep it out of the other tests.
        if std::env::var_os("HC_MAC_SHUTDOWN_TEST_CHILD").is_none() {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "platform::macos_shutdown::tests::private_loginwindow_notifications_keep_shutdown_exits_but_not_explicit_kills", "--nocapture"])
                .env("HC_MAC_SHUTDOWN_TEST_CHILD", "1")
                .status().unwrap().success());
            return;
        }
        let requested = SHUTDOWN.get_or_init(Default::default).clone();
        let watcher = Arc::new(Watcher { intent: Mutex::new(None), requested, seen: Default::default() });
        let prefix = format!("dev.herdr.test.{}.", super::super::random_hex());
        let registration = Registration::new(&prefix, watcher.clone()).unwrap();
        for (index, key) in
            ["restartinitiated", "logoutcancelled", "logoutNoReturn", "shutdownInitiated", "shutdownNoReturn"]
                .iter()
                .enumerate()
        {
            let key = CString::new(format!("{prefix}{key}")).unwrap();
            assert_eq!(unsafe { notify_post(key.as_ptr()) }, 0);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while watcher.seen.load(Ordering::Acquire) <= index {
                assert!(std::time::Instant::now() < deadline, "notification not delivered");
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert_eq!(in_progress(), index == 4);
        }
        use crate::{api::CoreInfo, holder_proto::Exit, paths::Paths, runtime::Core, terminals::NewTerminal};
        use std::io::{BufRead, BufReader};
        let root = std::path::PathBuf::from(format!("/var/tmp/hc-mac-shutdown-{}", super::super::random_hex()));
        let paths = || Paths {
            state: root.join("state"),
            config: root.join("config.toml"),
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
        };
        paths().create_dirs().unwrap();
        let info = || CoreInfo { id: "test".into(), name: "test".into() };
        let core = Core::start(paths(), info()).unwrap();
        for killed in [false, true] {
            let mut child = std::process::Command::new("sh")
                .args(["-c", "trap 'exit 1' HUP TERM; echo ready; while :; do sleep 0.02; done"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut ready = String::new();
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
            assert_eq!(ready.trim(), "ready");
            let id = {
                let mut st = core.lock();
                let id = st
                    .reg
                    .create(
                        NewTerminal { argv: vec!["sh".into()], keep: true, ..Default::default() },
                        crate::runtime::now_ms(),
                    )
                    .unwrap();
                if killed {
                    st.killing.insert(id);
                }
                id
            };
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
            assert_eq!(child.wait().unwrap().code(), Some(1));
            let mut st = core.lock();
            core.finish(&mut st, id, Exit { code: Some(1), signal: None });
            assert_eq!(st.reg.terminals[&id].is_live(), !killed);
            assert_eq!(st.reg.terminals[&id].reason.as_deref(), Some(if killed { "killed" } else { "host_shutdown" }));
        }
        drop(core);
        drop(registration);
        SHUTDOWN.get().unwrap().store(0, Ordering::Release);
        let restarted = Core::start(paths(), info()).unwrap();
        assert!(restarted.lock().reg.terminals[&1].is_live());
        assert!(!restarted.lock().reg.terminals[&2].is_live());
        drop(restarted);
        std::fs::remove_dir_all(root).unwrap();
    }
}
