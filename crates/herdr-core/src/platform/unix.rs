//! Linux and macOS: Unix sockets, PTYs, process groups, termios.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub fn check_via(_target: &str) -> io::Result<()> {
    Ok(())
}

pub fn via_add_hint(target: &str) -> String {
    format!("hc machine add --via '{}' --name <name>", target.replace('\'', r"'\''"))
}

pub fn via_command(target: &str, line: &str) -> io::Result<Command> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(format!("{target} \"$1\"")).arg("sh").arg(line);
    Ok(cmd)
}

// --- Connections -----------------------------------------------------------

/// A connection to a core or a holder (blocking).
pub type Stream = std::os::unix::net::UnixStream;
pub type Listener = std::os::unix::net::UnixListener;

/// Replacing a binary needs write and search permission on its folder, not
/// write permission on the binary itself.
pub fn can_replace(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Some(dir) = path.parent().and_then(|p| std::ffi::CString::new(p.as_os_str().as_bytes()).ok()) else {
        return false;
    };
    unsafe { libc::access(dir.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

pub fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(path)
}

pub fn may_autostart(_explicit: bool) -> Result<(), crate::rpc::RpcError> {
    Ok(())
}

pub fn status_may_start_core() -> bool {
    true
}

pub fn connect_error(_error: &io::Error, _existing_only: bool) -> Option<crate::rpc::RpcError> {
    None
}

pub fn merge_env(
    env: &mut std::collections::BTreeMap<String, String>,
    extra: std::collections::BTreeMap<String, String>,
) {
    env.extend(extra);
}

pub fn listen(path: &Path) -> io::Result<Listener> {
    Listener::bind(path)
}

pub type AsyncRead = tokio::net::unix::OwnedReadHalf;
pub type AsyncWrite = tokio::net::unix::OwnedWriteHalf;

/// Read EOF can be a caller's SHUT_WR: probe the reply side without sending bytes.
pub async fn reply_closed(read: &AsyncRead) {
    loop {
        let result = unsafe {
            libc::send(read.as_ref().as_raw_fd(), std::ptr::null(), 0, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL)
        };
        if result < 0
            && matches!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::EPIPE | libc::ECONNRESET | libc::ENOTCONN)
            )
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// The core's socket for clients.
pub struct AsyncListener(tokio::net::UnixListener);

impl AsyncListener {
    /// Bind before the runtime runs (the core binds, then adopts, then serves).
    pub fn bind(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    }

    pub fn serve(bound: std::os::unix::net::UnixListener) -> io::Result<AsyncListener> {
        Ok(AsyncListener(tokio::net::UnixListener::from_std(bound)?))
    }

    pub async fn accept(&mut self) -> io::Result<(AsyncRead, AsyncWrite)> {
        let (stream, _) = self.0.accept().await?;
        Ok(stream.into_split())
    }
}

pub fn remove_endpoint(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// --- Files -----------------------------------------------------------------

/// Default homes for state and config (XDG_STATE_HOME / XDG_CONFIG_HOME win).
pub fn data_dirs() -> (PathBuf, PathBuf) {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
    (home.join(".local/state"), home.join(".config"))
}

/// Where sockets go, and the prefix of their names: the user's runtime dir,
/// else beside the state.
/// The longest Unix socket path (macOS 103 bytes, Linux 107; one is the NUL).
pub const MAX_SOCKET_PATH: usize = {
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len() - 1
};

pub fn endpoints(state: &Path) -> (PathBuf, String) {
    let run = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        // Logins without XDG_RUNTIME_DIR (some ssh setups) must still find the core.
        .or_else(|| Some(PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))).filter(|p| p.is_dir()));
    (run.map_or_else(|| state.to_path_buf(), |run| run.join("herdr-core")), String::new())
}

/// A directory only this user can enter.
pub fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// A new file only this user can read or write, even in an existing directory.
pub fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

/// Takes the lock on `file` unless someone else holds it.
pub fn try_lock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

pub fn make_executable(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
}

/// Same file, same build: device, inode, mtime, size.
pub fn file_identity(m: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}:{}:{}", m.dev(), m.ino(), m.mtime(), m.size())
}

// --- Processes -------------------------------------------------------------

pub(super) fn raise_nofile_limit() -> io::Result<()> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let target = nofile_soft_limit(limit.rlim_cur, limit.rlim_max);
    if target == limit.rlim_cur {
        return Ok(());
    }
    limit.rlim_cur = target;
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn nofile_soft_limit(soft: libc::rlim_t, hard: libc::rlim_t) -> libc::rlim_t {
    // Below Darwin's OPEN_MAX; requesting an unlimited soft limit fails there.
    soft.max(hard.min(8192))
}

#[cfg(test)]
mod nofile_tests {
    use super::*;

    #[test]
    fn nofile_target_respects_hard_limit_cap_and_existing_soft_limit() {
        assert_eq!(nofile_soft_limit(256, libc::RLIM_INFINITY), 8192);
        assert_eq!(nofile_soft_limit(256, 1024), 1024);
        assert_eq!(nofile_soft_limit(16384, 32768), 16384);
        assert_eq!(nofile_soft_limit(256, 256), 256);
    }
}

/// Starts `cmd` in its own session (ending the terminal it was started from
/// won't end it), stdin/stdout to /dev/null, stderr to `log`.
pub fn spawn_detached(mut cmd: Command, log: std::fs::File) -> io::Result<()> {
    use std::process::Stdio;
    #[cfg(target_os = "macos")]
    cmd.env_remove("XPC_FLAGS");
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // Reap it if it ever exits while we live (the holder outlives crashed cores).
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Puts the hc at `from` where `dest` is: a copy beside it that `works`,
/// then renamed over it (InvalidData: the copy doesn't run).
pub fn swap_in(from: &Path, dest: &Path, works: impl Fn(&Path) -> bool) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        if super::macos_bundle::source_is_bundle(from) {
            return super::macos_bundle::install(from, dest, works);
        }
        if super::macos_bundle::source_is_bundle(dest) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a bundled core needs a complete signed bundle, not a bare executable",
            ));
        }
    }
    let tmp = dest.with_file_name(format!(".herdr-core.new.{}", std::process::id()));
    let staged = std::fs::copy(from, &tmp).and_then(|_| make_executable(&tmp));
    if staged.is_err() || !works(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        staged?;
        return Err(io::ErrorKind::InvalidData.into());
    }
    std::fs::rename(&tmp, dest)
}

pub fn parent_pid() -> u32 {
    std::os::unix::process::parent_id()
}

pub fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

/// This machine's own shell, as a person here gets it.
pub fn own_shell() -> Vec<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    if cfg!(target_os = "macos") { vec![shell, "-l".into()] } else { vec![shell] }
}

/// Drops displays whose local servers no longer accept connections.
pub fn drop_unreachable_displays(env: &mut std::collections::BTreeMap<String, String>) {
    if let Some(display) = env.get("DISPLAY").cloned()
        && !display_is_live(&display, Path::new("/tmp/.X11-unix"))
    {
        env.remove("DISPLAY");
        env.remove("XAUTHORITY");
    }
    if let Some(display) = env.get("WAYLAND_DISPLAY").cloned() {
        let runtime = env.get("XDG_RUNTIME_DIR").map(std::path::Path::new);
        if !wayland_is_live(&display, runtime) {
            env.remove("WAYLAND_DISPLAY");
        }
    }
}

/// Only probe local X transports. Host displays may use TCP or SSH forwarding
/// without any corresponding Unix socket.
fn display_is_live(display: &str, sockets: &Path) -> bool {
    let Some((host, rest)) = display.rsplit_once(':') else { return true };
    if !matches!(host, "" | "unix") {
        return true;
    }
    let number = rest.split('.').next().unwrap_or(rest);
    let Ok(number) = number.parse::<u32>() else { return true };
    let path = sockets.join(format!("X{number}"));
    if socket_is_live(&path, false) {
        return true;
    }
    #[cfg(target_os = "linux")]
    if socket_is_live(&path, true) {
        return true;
    }
    false
}

/// Whether the Wayland compositor accepts connections at its socket.
fn wayland_is_live(display: &str, runtime: Option<&Path>) -> bool {
    let path = if display.starts_with('/') {
        PathBuf::from(display)
    } else {
        match runtime {
            Some(runtime) => runtime.join(display),
            None => return true,
        }
    };
    socket_is_live(&path, false)
}

/// Only absence or a refused connection proves a server is gone. In
/// particular, a full listener backlog must not stall program startup.
fn socket_is_live(path: &Path, abstract_socket: bool) -> bool {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let name = path.as_os_str().as_bytes();
    let prefix = usize::from(abstract_socket);
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if name.len() + prefix >= address.sun_path.len() {
        return true;
    }
    address.sun_family = libc::AF_UNIX as _;
    for (dest, byte) in address.sun_path[prefix..].iter_mut().zip(name) {
        *dest = *byte as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + prefix + name.len() + usize::from(!abstract_socket))
            as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    let socket_type = libc::SOCK_STREAM;
    #[cfg(target_os = "linux")]
    let socket_type = socket_type | libc::SOCK_CLOEXEC;
    let fd = unsafe { libc::socket(libc::AF_UNIX, socket_type, 0) };
    if fd < 0 {
        return true;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    #[cfg(not(target_os = "linux"))]
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return true;
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return true;
    }
    if unsafe { libc::connect(fd.as_raw_fd(), (&raw const address).cast(), length) } == 0 {
        return true;
    }
    !matches!(io::Error::last_os_error().raw_os_error(), Some(libc::ENOENT | libc::ECONNREFUSED))
}

/// A shell line as this machine runs one.
pub fn shell_line(line: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), line.into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn assert_socket_gone(mut live: impl FnMut() -> bool) {
        // Concurrent tests may fork with this listener open; CLOEXEC closes it at exec, not fork.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while live() {
            assert!(std::time::Instant::now() < deadline, "display socket stayed live after listener drop");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_display_whose_server_is_gone_is_not_handed_out() {
        let sockets = dir("x11");
        let listener = Listener::bind(sockets.join("X1")).unwrap();
        assert!(display_is_live(":1", &sockets));
        assert!(display_is_live(":1.0", &sockets));
        assert!(display_is_live("unix:1", &sockets));
        assert!(!display_is_live(":2", &sockets));
        assert!(display_is_live("otherhost:2", &sockets));
        assert!(display_is_live("localhost:2.0", &sockets));
        assert!(display_is_live("127.0.0.1:2", &sockets));
        drop(listener);
        assert_socket_gone(|| display_is_live(":1", &sockets));
        // macOS answers a connect to a regular file with an errno we treat as uncertain.
        #[cfg(target_os = "linux")]
        {
            std::fs::write(sockets.join("X2"), "").unwrap();
            assert!(!display_is_live(":2", &sockets));
        }
        std::fs::remove_dir_all(&sockets).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_abstract_x_socket_needs_no_filesystem_entry() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::ffi::OsStrExt;

        let sockets = dir("abstract-x11");
        let path = sockets.join("X1");
        let address = std::os::unix::net::SocketAddr::from_abstract_name(path.as_os_str().as_bytes()).unwrap();
        let listener = Listener::bind_addr(&address).unwrap();
        assert!(!path.exists());
        assert!(display_is_live(":1", &sockets));
        assert!(display_is_live("unix:1.0", &sockets));
        drop(listener);
        assert_socket_gone(|| display_is_live(":1", &sockets));
        std::fs::remove_dir_all(&sockets).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_full_listener_backlog_does_not_block_or_remove_a_display() {
        let runtime = dir("full-backlog");
        let path = runtime.join("wayland-0");
        let listener = Listener::bind(&path).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued = Stream::connect(&path).unwrap();
        assert!(socket_is_live(&path, false));
        assert!(wayland_is_live("wayland-0", Some(&runtime)));
        std::fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn an_uncertain_socket_error_keeps_the_display() {
        let runtime = dir("uncertain-socket");
        std::fs::write(runtime.join("file"), "").unwrap();
        assert!(wayland_is_live("file/not-a-socket", Some(&runtime)));
        std::fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn a_missing_cookie_does_not_remove_a_host_display() {
        let mut env = std::collections::BTreeMap::from([
            ("DISPLAY".into(), "localhost:10.0".into()),
            ("XAUTHORITY".into(), "/nonexistent/hc-cookie".into()),
        ]);
        let before = env.clone();
        drop_unreachable_displays(&mut env);
        assert_eq!(env, before);
    }

    #[test]
    fn a_wayland_listener_inherited_at_fork_closes_in_the_child() {
        use std::os::fd::AsRawFd;
        let runtime = dir("forked-wayland");
        let path = runtime.join("wayland-0");
        let listener = Listener::bind(&path).unwrap();
        let (release, wait) = Stream::pair().unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Only async-signal-safe calls between fork and exit: other test threads existed at fork.
            unsafe {
                let mut byte = 0u8;
                libc::read(wait.as_raw_fd(), (&raw mut byte).cast(), 1);
                libc::close(listener.as_raw_fd());
                libc::_exit(0);
            }
        }
        drop(wait);
        drop(listener);
        let live = wayland_is_live("wayland-0", Some(&runtime));
        assert!(live, "fork keeps the listener open until the child closes it");
        io::Write::write_all(&mut &release, b"x").unwrap();
        assert_socket_gone(|| wayland_is_live("wayland-0", Some(&runtime)));
        assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) }, pid);
        std::fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn a_wayland_socket_must_accept_connections() {
        let runtime = dir("wayland");
        let path = runtime.join("wayland-0");
        assert!(!wayland_is_live("wayland-0", Some(&runtime)));
        let listener = Listener::bind(&path).unwrap();
        assert!(wayland_is_live("wayland-0", Some(&runtime)));
        assert!(wayland_is_live(path.to_str().unwrap(), None));
        assert!(wayland_is_live(path.to_str().unwrap(), Some(Path::new("/nonexistent"))));
        assert!(wayland_is_live("wayland-0", None));
        drop(listener);
        assert_socket_gone(|| wayland_is_live("wayland-0", Some(&runtime)));
        #[cfg(target_os = "linux")]
        {
            std::fs::write(runtime.join("regular-file"), "").unwrap();
            assert!(!wayland_is_live("regular-file", Some(&runtime)));
        }
        std::fs::remove_dir_all(&runtime).unwrap();
    }
}

/// Returns once the child exited, leaving it unreaped: until it is reaped
/// its pid, and so its group id, can't go to another process.
pub async fn exited(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else { return };
    let _ = tokio::task::spawn_blocking(move || {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let done = unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) } == 0;
            if done || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return;
            }
        }
    })
    .await;
}

/// An exec starts its own process group.
pub fn exec_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

/// An exec's process group: it and what it starts, unless they leave it.
#[derive(Clone)]
pub struct ExecGroup(i32);

impl ExecGroup {
    pub fn new(child: &tokio::process::Child) -> io::Result<ExecGroup> {
        Ok(ExecGroup(child.id().map_or(0, |pid| pid as i32)))
    }

    pub fn end(&self) {
        self.signal(libc::SIGTERM);
    }

    pub fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    fn signal(&self, signal: i32) {
        if self.0 > 0 {
            unsafe { libc::kill(-self.0, signal) };
        }
    }
}

pub fn random_bytes(bytes: &mut [u8]) {
    File::open("/dev/urandom").and_then(|mut f| io::Read::read_exact(&mut f, bytes)).expect("/dev/urandom readable");
}

/// Resolves when the process is asked to end (SIGTERM): a planned restart.
pub async fn termination() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut term) => drop(term.recv().await),
        Err(_) => std::future::pending().await,
    }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// This system's own id, new on a cloned disk: Linux's machine-id (made per
/// box at first boot), a Mac's hardware UUID. None when there is none or it
/// can't be read. One source only: a second one that disagrees, read when
/// the first fails once, would look like a copy and forget live terminals.
pub fn machine_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let line = text.lines().find(|l| l.contains("\"IOPlatformUUID\""))?;
        line.rsplit('"').nth(1).map(String::from)
    }
    #[cfg(not(target_os = "macos"))]
    std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "uninitialized")
}

/// Identity of this boot, not just the machine that owns the disk.
pub fn boot_id() -> io::Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        let mut bytes = [0u8; 128];
        let mut size = bytes.len();
        if unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let value = std::str::from_utf8(&bytes[..size]).map_err(io::Error::other)?.trim_end_matches('\0').trim();
        if value.is_empty() {
            return Err(io::Error::other("boot identity is empty"));
        }
        Ok(Some(value.into()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let value = value.trim();
        if value.is_empty() {
            return Err(io::Error::other("boot identity is empty"));
        }
        Ok(Some(value.into()))
    }
}

// --- Terminals -------------------------------------------------------------

/// A program on its own terminal.
pub struct Pty {
    master: File,
    stop: Stream,
    _stop_sender: Stream,
}

/// Cancels blocked IO when the holder releases the terminal, even if a
/// detached program keeps its slave open.
pub struct PtyIo {
    master: File,
    stop: Stream,
}

impl PtyIo {
    fn ready(&self, events: i16) -> io::Result<bool> {
        let mut fds = [
            libc::pollfd { fd: self.master.as_raw_fd(), events, revents: 0 },
            libc::pollfd { fd: self.stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fds[1].revents == 0)
    }
}

impl Read for PtyIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.ready(libc::POLLIN)? {
                return Ok(0);
            }
            match self.master.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
    }
}

impl Write for PtyIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.ready(libc::POLLOUT)? {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            match self.master.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.master.flush()
    }
}

pub type Process = Child;

/// Starts `argv` on a new terminal of `cols`x`rows`.
pub fn spawn_pty(
    argv: &[String],
    env: &std::collections::BTreeMap<String, String>,
    cwd: &str,
    cols: u16,
    rows: u16,
) -> io::Result<(Pty, Process)> {
    let program = argv.first().ok_or_else(|| io::Error::other("empty argv"))?;
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &raw mut size) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let (stop, stop_sender) = Stream::pair()?;
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok((Pty { master, stop, _stop_sender: stop_sender }, child))
}

impl Pty {
    /// Its output, read until the slave closes or this PTY is released.
    pub fn reader(&self) -> io::Result<PtyIo> {
        Ok(PtyIo { master: self.master.try_clone()?, stop: self.stop.try_clone()? })
    }

    pub fn writer(&self) -> io::Result<PtyIo> {
        self.reader()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
    }

    /// The process group in the foreground (a shell's job), or -1.
    pub fn foreground(&self) -> i32 {
        unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) }
    }

    /// The program's group, plus whatever group is in the foreground.
    pub fn signal(&self, pid: u32, signal: i32) {
        unsafe {
            libc::killpg(pid as i32, signal);
            let fg = libc::tcgetpgrp(self.master.as_raw_fd());
            if fg > 0 && fg != pid as i32 {
                libc::killpg(fg, signal);
            }
        }
    }

    /// Its program is gone: hang up what still holds the terminal.
    pub fn hang_up(&self, pid: u32) {
        unsafe { libc::killpg(pid as i32, libc::SIGHUP) };
    }
}

/// Waits for it to end: (exit code, signal).
pub fn wait(child: &mut Process) -> io::Result<(Option<i32>, Option<i32>)> {
    loop {
        match child.wait() {
            Ok(status) => return Ok((status.code(), status.signal())),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

// --- The console (attach, picker) --------------------------------------------

pub mod console {
    use super::*;

    static TERMINATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        TERMINATED.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// A kill (TERM, HUP, INT) only sets a flag: the input loop sees it within one
    /// poll and leaves through the normal cleanup, so the terminal is given back.
    pub fn catch_termination() {
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
        }
    }

    pub fn terminated() -> bool {
        TERMINATED.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub struct RawMode(Option<libc::termios>);

    impl RawMode {
        pub fn enter() -> RawMode {
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
                return RawMode(None);
            }
            let mut raw = saved;
            unsafe {
                libc::cfmakeraw(&mut raw);
                libc::tcsetattr(0, libc::TCSANOW, &raw);
            }
            RawMode(Some(saved))
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            if let Some(saved) = &self.0 {
                unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
            }
        }
    }

    /// Unbuffered stdin/stdout: std's stdout holds bytes until a newline.
    pub fn stdin() -> std::mem::ManuallyDrop<File> {
        std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(0) })
    }

    pub fn stdout() -> std::mem::ManuallyDrop<File> {
        std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(1) })
    }

    pub fn out(bytes: &[u8]) {
        let _ = stdout().write_all(bytes);
    }

    pub fn window_size() -> (u16, u16) {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }

    /// Waits up to `ms` for stdin; returns what arrived (empty: nothing yet,
    /// None: stdin is closed).
    pub fn read_stdin(ms: i32) -> Option<Vec<u8>> {
        let mut fds = [libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 }];
        if unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) } <= 0 {
            return Some(Vec::new());
        }
        let mut buf = [0u8; 8192];
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        (n > 0).then(|| buf[..n as usize].to_vec())
    }
}
