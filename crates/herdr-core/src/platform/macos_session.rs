//! Audit identity survives logout; a fresh GUI launcher supplies the new context.

use super::LoginSession;
use super::macos_appkit::gui_session_usable as usable;
use crate::{client, paths::Paths, rpc::RpcError};
use std::io::{self, Read};
use std::time::Duration;

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
    fn getaudit_addr(info: *mut AuditInfo, size: libc::c_int) -> libc::c_int;
    static bootstrap_port: libc::mach_port_t;
    static mach_task_self_: libc::mach_port_t;
    fn task_set_special_port(task: libc::mach_port_t, which: libc::c_int, port: libc::mach_port_t) -> libc::c_int;
    fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::c_int;
}

const TASK_BOOTSTRAP_PORT: libc::c_int = 4;
type GetRoot = unsafe extern "C" fn(libc::mach_port_t, *mut libc::mach_port_t) -> libc::c_int;
type LookUpPerUser =
    unsafe extern "C" fn(libc::mach_port_t, *const libc::c_char, libc::uid_t, *mut libc::mach_port_t) -> libc::c_int;

/// Programs reach macOS services (user lookup, keychain, certificate checks) through the bootstrap
/// port they inherit. A login's own port dies at logout while the holder lives on, so terminals get
/// launchd's per-user port instead, as tmux does. The holder itself keeps the login's for AppKit.
pub fn outlive_logout() -> io::Result<()> {
    let per_user = per_user_bootstrap()?;
    let set = unsafe { task_set_special_port(mach_task_self_, TASK_BOOTSTRAP_PORT, per_user) };
    unsafe { mach_port_deallocate(mach_task_self_, per_user) };
    kern(set, "task_set_special_port")
}

fn per_user_bootstrap() -> io::Result<libc::mach_port_t> {
    // Private launchd calls: a release without them keeps the inherited port.
    let symbol = |name: &std::ffi::CStr| {
        let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        (!symbol.is_null()).then_some(symbol).ok_or_else(|| io::Error::other(format!("{name:?} unavailable")))
    };
    let get_root: GetRoot = unsafe { std::mem::transmute(symbol(c"bootstrap_get_root")?) };
    let look_up: LookUpPerUser = unsafe { std::mem::transmute(symbol(c"bootstrap_look_up_per_user")?) };
    let mut root = 0;
    kern(unsafe { get_root(bootstrap_port, &mut root) }, "bootstrap_get_root")?;
    let mut per_user = 0;
    // A null name selects the user's bootstrap itself. tmux passes uid -1, which current macOS
    // refuses without an entitlement (launchd error 144); the caller's own uid needs none.
    let found = unsafe { look_up(root, std::ptr::null(), libc::geteuid(), &mut per_user) };
    unsafe { mach_port_deallocate(mach_task_self_, root) };
    kern(found, "bootstrap_look_up_per_user")?;
    Ok(per_user)
}

fn kern(code: libc::c_int, call: &str) -> io::Result<()> {
    if code == 0 { Ok(()) } else { Err(io::Error::other(format!("{call} failed: {code:#x}"))) }
}

pub fn graphic_session() -> io::Result<Option<u32>> {
    audit().map(|a| (a.flags & 0x10 != 0).then_some(a.session))
}

fn audit() -> io::Result<AuditInfo> {
    let mut info = AuditInfo::default();
    if unsafe { getaudit_addr(&mut info, std::mem::size_of::<AuditInfo>() as libc::c_int) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

pub fn current() -> Option<LoginSession> {
    static SESSION: std::sync::OnceLock<Option<LoginSession>> = std::sync::OnceLock::new();
    *SESSION.get_or_init(|| {
        let audit = match audit() {
            Ok(audit) if ![0, u32::MAX].contains(&audit.session) => audit,
            result => {
                eprintln!("hc: login session identity unavailable: {:?}", result.err());
                return None;
            }
        };
        let launchd = std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--hc-launchd"));
        Some(identity(audit, launchd))
    })
}

fn identity(audit: AuditInfo, launchd: bool) -> LoginSession {
    LoginSession { id: audit.session, user: unsafe { libc::geteuid() }, gui: Some(launchd || audit.flags & 0x10 != 0) }
}

pub fn connect(paths: &Paths) -> Result<super::Stream, RpcError> {
    let Some(local) = current().filter(|s| s.gui == Some(true) && usable(s.id)) else {
        return client::connect_or_start(paths);
    };
    repair(paths, local)
}

fn repair(paths: &Paths, local: LoginSession) -> Result<super::Stream, RpcError> {
    let mut repairs = 0;
    let mut interrupted = 0;
    loop {
        let control = client::connect_or_start(paths)?;
        // Darwin rejects socket options on a socket a handing-over core already closed.
        let hello = control
            .set_read_timeout(Some(Duration::from_secs(10)))
            .and_then(|()| control.set_write_timeout(Some(Duration::from_secs(10))))
            .map_err(connection_error)
            .and_then(|()| client::call(&control, "hello", serde_json::json!({})));
        let hello = match hello {
            Ok(hello) => hello,
            Err(error) => {
                interrupted += 1;
                if interrupted == 3 {
                    return Err(error);
                }
                continue; // A concurrent launcher may already be handing over.
            }
        };
        let old = hello.get("login_session").cloned().and_then(|value| serde_json::from_value(value).ok());
        if !local.replaces(old) {
            drop(control);
            return client::connect_or_start(paths);
        }
        if repairs == 3 {
            break;
        }
        repairs += 1;
        eprintln!("hc: refreshing core after a new macOS login");
        // Set before the request; if the old core already closed, the read below ends at once.
        let _ = control.set_read_timeout(Some(Duration::from_secs(30)));
        let requested = client::call(&control, "server.restart", serde_json::json!({}));
        let closed = (&control).read_to_end(&mut Vec::new());
        if let Err(error) = closed
            && error.kind() != io::ErrorKind::ConnectionReset
        {
            return Err(requested.err().unwrap_or_else(|| connection_error(error)));
        }
        drop(control);
        // Existing startup waits for the old process's lock before launching in this login.
        drop(client::connect_or_start(paths)?);
    }
    Err(RpcError::new(
        "unavailable",
        "the core still belongs to an earlier macOS login",
        "retry from Herdr or a terminal in the current login; existing terminals were kept",
    ))
}

fn connection_error(error: io::Error) -> RpcError {
    RpcError::new("unavailable", error.to_string(), "retry the connection from the current macOS login")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    unsafe extern "C" {
        fn task_get_special_port(
            task: libc::mach_port_t,
            which: libc::c_int,
            port: *mut libc::mach_port_t,
        ) -> libc::c_int;
    }

    #[test]
    fn terminals_inherit_the_per_user_bootstrap_and_still_find_their_user() {
        let special = || {
            let mut port = 0;
            assert_eq!(unsafe { task_get_special_port(mach_task_self_, TASK_BOOTSTRAP_PORT, &mut port) }, 0);
            port
        };
        let login = special();
        let per_user = per_user_bootstrap().unwrap();
        outlive_logout().unwrap();
        let inherited = special();
        // whoami prints the bare uid when the user lookup service is unreachable (#294).
        let whoami = std::process::Command::new("/usr/bin/whoami").output().unwrap();
        unsafe { task_set_special_port(mach_task_self_, TASK_BOOTSTRAP_PORT, login) };
        for port in [login, inherited, per_user] {
            unsafe { mach_port_deallocate(mach_task_self_, port) };
        }
        assert_eq!(inherited, per_user);
        assert!(whoami.status.success(), "{whoami:?}");
        let name = String::from_utf8_lossy(&whoami.stdout);
        assert_ne!(name.trim(), unsafe { libc::geteuid() }.to_string());
    }

    #[test]
    fn audit_identity_is_readable_without_privileged_process_inspection() {
        assert_ne!(super::audit().unwrap().session, 0);
    }

    #[test]
    fn a_graphic_core_remains_repairable_after_its_login_domain_is_gone() {
        let stale = identity(AuditInfo { session: u32::MAX - 1, flags: 0x10, ..AuditInfo::default() }, false);
        assert!(!usable(stale.id));
        assert_eq!(stale.gui, Some(true));
        let local = LoginSession { id: super::audit().unwrap().session, ..stale };
        assert!(local.replaces(Some(stale)));
    }

    #[test]
    fn the_last_repair_is_observed_after_interrupted_handshakes() {
        let root = crate::test_tmp().join(format!("hc-login-{}", super::super::random_hex()));
        let paths = Paths {
            state: root.join("state"),
            config: root.join("config.toml"),
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
        };
        paths.create_dirs().unwrap();
        let listener = super::super::listen(&paths.socket).unwrap();
        let local = LoginSession { id: 20, user: 501, gui: Some(true) };
        let request = |stream: &super::super::Stream| {
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            (!line.is_empty()).then(|| serde_json::from_str::<serde_json::Value>(&line).unwrap())
        };
        let server = std::thread::spawn(move || {
            // Two peers disappear during concurrent handoffs before any restart request.
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                assert_eq!(request(&stream).unwrap()["method"], "hello");
            }
            for index in 0..=3 {
                let mut stream = loop {
                    let (stream, _) = listener.accept().unwrap();
                    if let Some(message) = request(&stream) {
                        assert_eq!(message["method"], "hello");
                        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                        break stream;
                    }
                    // Darwin rejects socket options on an already-closed, empty startup probe.
                };
                let session = LoginSession { id: if index == 3 { 20 } else { 10 }, ..local };
                writeln!(stream, "{}", serde_json::json!({"id": 1, "result": {"login_session": session}})).unwrap();
                if index == 3 {
                    break;
                }
                assert_eq!(request(&stream).unwrap()["method"], "server.restart");
                writeln!(stream, "{}", serde_json::json!({"id": 1, "result": {"stopped": true}})).unwrap();
            }
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut text = String::new();
            BufReader::new(&stream).read_line(&mut text).unwrap();
            assert_eq!(text, "app bytes\n", "the control handshake never reaches the app stream");
        });
        let mut stream = repair(&paths, local).unwrap();
        writeln!(stream, "app bytes").unwrap();
        server.join().unwrap();
        drop(stream);
        std::fs::remove_dir_all(root).unwrap();
    }
}
