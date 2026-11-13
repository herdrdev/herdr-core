//! A persistent session endpoint for explicitly forwarded, connection-scoped SSH agents.

use crate::wire::{Conn, Heard};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, watch};

const OPEN_WAIT: Duration = Duration::from_secs(5);
const IDLE_WAIT: Duration = Duration::from_secs(120);
const MAX_CONNECTIONS: usize = 64;

struct Endpoint {
    path: PathBuf,
    inode: (u64, u64),
    _lock: File,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| inode(&m) == self.inode) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct Provider {
    connection: usize,
    path: PathBuf,
    live: watch::Sender<bool>,
    expiry: Mutex<Option<tokio::task::AbortHandle>>,
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Provider {
    fn cancel(&self) {
        self.live.send_replace(false);
        if let Some(task) = self.expiry.lock().unwrap_or_else(|p| p.into_inner()).take() {
            task.abort();
        }
    }
}

pub struct SshAgent {
    endpoint: Option<Endpoint>,
    listener: Mutex<Option<std::os::unix::net::UnixListener>>,
    accept_task: Mutex<Option<tokio::task::AbortHandle>>,
    providers: Mutex<Vec<Arc<Provider>>>,
    slots: Arc<Semaphore>,
    expiry: Duration,
}

fn inode(meta: &std::fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

fn owned(meta: &std::fs::Metadata) -> bool {
    meta.uid() == unsafe { libc::geteuid() }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Nonblocking even if a live socket's accept backlog is full. Only an explicit
/// refusal proves that removing a stale endpoint cannot steal a live listener.
fn refused(path: &Path) -> bool {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return false;
    }
    let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    if socket.set_nonblocking(true).is_err() {
        return false;
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_bytes();
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as _;
    }
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            &address as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&address) as _,
        )
    };
    result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::ConnectionRefused
}

impl SshAgent {
    pub fn bind(socket: &Path, enabled: bool, expiry: Duration) -> io::Result<Arc<Self>> {
        let mut agent = Self {
            endpoint: None,
            listener: Mutex::new(None),
            accept_task: Mutex::new(None),
            providers: Mutex::new(Vec::new()),
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            expiry,
        };
        if enabled {
            let mut name = socket.as_os_str().to_os_string();
            name.push(".ssh-agent");
            let path = PathBuf::from(name);
            if !path.is_absolute()
                || path.to_str().is_none()
                || path.as_os_str().as_bytes().len() > super::MAX_SOCKET_PATH
            {
                return Err(invalid("SSH agent relay needs an absolute socket path within the Unix socket limit"));
            }
            let parent = std::fs::metadata(path.parent().ok_or_else(|| invalid("agent socket has no parent"))?)?;
            if !parent.is_dir() || !owned(&parent) || parent.mode() & 0o077 != 0 {
                return Err(invalid("SSH agent relay directory must belong to this account and have mode 0700"));
            }
            let lock = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path.with_extension("ssh-agent.lock"))?;
            let meta = lock.metadata()?;
            if !meta.is_file() || !owned(&meta) || meta.mode() & 0o077 != 0 || !super::try_lock(&lock) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "SSH agent relay ownership is unsafe or already held",
                ));
            }
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_socket() && owned(&meta) && refused(&path) => {
                    if inode(&std::fs::symlink_metadata(&path)?) != inode(&meta) {
                        return Err(invalid("SSH agent relay endpoint changed while checking it"));
                    }
                    std::fs::remove_file(&path)?;
                }
                Ok(_) => {
                    return Err(io::Error::new(io::ErrorKind::AddrInUse, "SSH agent relay endpoint already exists"));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let listener = std::os::unix::net::UnixListener::bind(&path)?;
            agent.endpoint = Some(Endpoint { inode: inode(&std::fs::metadata(&path)?), path, _lock: lock });
            std::fs::set_permissions(&agent.endpoint.as_ref().unwrap().path, std::fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            *agent.listener.get_mut().unwrap() = Some(listener);
        }
        Ok(Arc::new(agent))
    }

    pub fn enabled(&self) -> bool {
        self.endpoint.is_some()
    }

    pub fn environment(&self, env: &mut BTreeMap<String, String>) {
        if let Some(endpoint) = &self.endpoint {
            env.insert("SSH_AUTH_SOCK".into(), endpoint.path.to_string_lossy().into_owned());
        }
    }

    pub fn start(self: &Arc<Self>) -> io::Result<()> {
        let Some(bound) = self.listener.lock().unwrap_or_else(|p| p.into_inner()).take() else { return Ok(()) };
        let listener = UnixListener::from_std(bound)?;
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            while let Ok((input, _)) = listener.accept().await {
                let Some(agent) = weak.upgrade() else { break };
                let Ok(slot) = agent.slots.clone().try_acquire_owned() else { continue };
                tokio::spawn(async move {
                    let _slot = slot;
                    agent.proxy(input).await;
                });
            }
        });
        *self.accept_task.lock().unwrap_or_else(|p| p.into_inner()) = Some(task.abort_handle());
        Ok(())
    }

    fn checked_path(&self, path: &Path) -> io::Result<PathBuf> {
        if !path.is_absolute() {
            return Err(invalid("forwarded agent socket must be an absolute path"));
        }
        // Follow aliases for validation, but connect through the offered pathname:
        // expanding a short alias can exceed the Unix socket path limit.
        let meta = std::fs::metadata(path)?;
        if !meta.file_type().is_socket() || !owned(&meta) {
            return Err(invalid("forwarded agent must be a Unix socket owned by this account"));
        }
        if self.endpoint.as_ref().is_some_and(|e| inode(&meta) == e.inode) {
            return Err(invalid("SSH agent relay cannot forward to itself"));
        }
        Ok(path.to_path_buf())
    }

    pub fn register(self: &Arc<Self>, conn: &Arc<Conn>, path: &str) -> io::Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        let path = self.checked_path(Path::new(path))?;
        let connection = Arc::as_ptr(conn) as usize;
        self.unregister(conn);
        let provider = Arc::new(Provider { connection, path, live: watch::channel(true).0, expiry: Mutex::new(None) });
        let agent: Weak<Self> = Arc::downgrade(self);
        let conn = conn.clone();
        let expiry = self.expiry;
        let task = tokio::spawn(async move {
            conn.heard.silent_for(expiry).await;
            if let Some(agent) = agent.upgrade() {
                agent.unregister(&conn);
            }
        });
        *provider.expiry.lock().unwrap_or_else(|p| p.into_inner()) = Some(task.abort_handle());
        self.providers.lock().unwrap_or_else(|p| p.into_inner()).push(provider);
        Ok(())
    }

    pub fn unregister(&self, conn: &Arc<Conn>) {
        let id = Arc::as_ptr(conn) as usize;
        self.providers.lock().unwrap_or_else(|p| p.into_inner()).retain(|p| {
            if p.connection == id {
                p.cancel();
                false
            } else {
                true
            }
        });
    }

    async fn upstream(&self) -> Option<(UnixStream, watch::Receiver<bool>)> {
        let providers = self.providers.lock().unwrap_or_else(|p| p.into_inner()).clone();
        tokio::time::timeout(OPEN_WAIT, async {
            for provider in providers.into_iter().rev() {
                let mut live = provider.live.subscribe();
                if !*live.borrow() || self.checked_path(&provider.path).is_err() {
                    continue;
                }
                let result = tokio::select! {
                    result = UnixStream::connect(&provider.path) => result,
                    _ = live.changed() => continue,
                };
                if let Ok(stream) = result {
                    return Some((stream, live));
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
    }

    async fn proxy(&self, input: UnixStream) {
        let Some((upstream, mut live)) = self.upstream().await else { return };
        if !*live.borrow() {
            return;
        }
        let heard = Heard::default();
        let (mut input_read, mut input_write) = input.into_split();
        let (mut agent_read, mut agent_write) = upstream.into_split();
        let transfer = async {
            let to_agent = async {
                tokio::io::copy(&mut heard.watch(&mut input_read), &mut agent_write).await?;
                agent_write.shutdown().await
            };
            let from_agent = async {
                tokio::io::copy(&mut heard.watch(&mut agent_read), &mut input_write).await?;
                input_write.shutdown().await
            };
            tokio::try_join!(to_agent, from_agent)
        };
        tokio::select! {
            _ = transfer => {},
            _ = live.changed() => {},
            () = heard.silent_for(IDLE_WAIT) => {},
        }
    }

    pub fn stop(&self) {
        if let Some(task) = self.accept_task.lock().unwrap_or_else(|p| p.into_inner()).take() {
            task.abort();
        }
        for provider in self.providers.lock().unwrap_or_else(|p| p.into_inner()).drain(..) {
            provider.cancel();
        }
    }
}

impl Drop for SshAgent {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
#[path = "ssh_agent_tests.rs"]
mod tests;
