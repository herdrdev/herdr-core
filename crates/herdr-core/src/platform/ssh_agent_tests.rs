use super::*;
use std::os::unix::fs::PermissionsExt;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = PathBuf::from(format!("/var/tmp/hc-agent-{}", crate::platform::random_hex()));
        crate::platform::private_dir(&path).unwrap();
        Self(path)
    }

    fn relay(&self, expiry: Duration) -> Arc<SshAgent> {
        SshAgent::bind(&self.0.join("core.sock"), true, expiry).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn binding_is_private_exclusive_and_never_removes_an_unrelated_file() {
    let dir = Scratch::new();
    let endpoint = dir.0.join("core.sock.ssh-agent");
    std::fs::write(&endpoint, "not a socket").unwrap();
    assert!(SshAgent::bind(&dir.0.join("core.sock"), true, Duration::from_secs(1)).is_err());
    assert_eq!(std::fs::read_to_string(&endpoint).unwrap(), "not a socket");
    std::fs::remove_file(&endpoint).unwrap();
    let relay = dir.relay(Duration::from_secs(1));
    assert_eq!(std::fs::metadata(&endpoint).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(SshAgent::bind(&dir.0.join("core.sock"), true, Duration::from_secs(1)).is_err());
    assert!(std::os::unix::net::UnixStream::connect(&endpoint).is_ok());
    drop(relay);
    assert!(!endpoint.exists());
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(SshAgent::bind(&dir.0.join("core.sock"), true, Duration::from_secs(1)).is_err());
}

#[test]
fn only_a_refused_owned_socket_is_reclaimed_and_cleanup_checks_its_inode() {
    let dir = Scratch::new();
    let endpoint = dir.0.join("core.sock.ssh-agent");
    let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
    assert!(SshAgent::bind(&dir.0.join("core.sock"), true, Duration::from_secs(1)).is_err());
    drop(listener);
    let relay = dir.relay(Duration::from_secs(1));
    std::fs::rename(&endpoint, dir.0.join("moved")).unwrap();
    std::fs::write(&endpoint, "replacement").unwrap();
    drop(relay);
    assert_eq!(std::fs::read_to_string(&endpoint).unwrap(), "replacement");
}

#[test]
fn unsafe_lockfiles_and_unrepresentable_endpoint_names_are_rejected() {
    use std::os::unix::ffi::OsStringExt;
    let dir = Scratch::new();
    let target = dir.0.join("important");
    std::fs::write(&target, "leave alone").unwrap();
    std::os::unix::fs::symlink(&target, dir.0.join("core.sock.ssh-agent.lock")).unwrap();
    assert!(SshAgent::bind(&dir.0.join("core.sock"), true, Duration::from_secs(1)).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "leave alone");
    let non_utf8 = dir.0.join(std::ffi::OsString::from_vec(vec![0xff]));
    assert!(SshAgent::bind(&non_utf8, true, Duration::from_secs(1)).is_err());
    let overlong = dir.0.join("x".repeat(super::super::MAX_SOCKET_PATH));
    assert!(SshAgent::bind(&overlong, true, Duration::from_secs(1)).is_err());
}

#[tokio::test]
async fn malformed_and_recursive_offers_never_enter_the_registry() {
    let dir = Scratch::new();
    let relay = dir.relay(Duration::from_secs(1));
    let conn = Arc::new(Conn::default());
    let own = dir.0.join("core.sock.ssh-agent");
    let alias = dir.0.join("alias");
    std::os::unix::fs::symlink(&own, &alias).unwrap();
    let regular = dir.0.join("regular");
    std::fs::write(&regular, b"not a socket").unwrap();
    for path in [Path::new("relative"), own.as_path(), alias.as_path(), regular.as_path()] {
        assert!(relay.register(&conn, path.to_str().unwrap()).is_err(), "{path:?}");
    }
    assert!(relay.providers.lock().unwrap().is_empty());
}

#[test]
fn a_fifo_lockfile_is_rejected_without_waiting_for_a_reader() {
    let dir = Scratch::new();
    let lock = dir.0.join("core.sock.ssh-agent.lock");
    let name = std::ffi::CString::new(lock.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let socket = dir.0.join("core.sock");
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let rejected = SshAgent::bind(&socket, true, Duration::from_secs(1)).is_err();
        tx.send(rejected).unwrap();
    });
    let rejected = rx.recv_timeout(Duration::from_secs(1));
    // Unblock the old implementation before asserting, so a failing test leaves no thread behind.
    let _reader = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&lock).unwrap();
    worker.join().unwrap();
    assert_eq!(rejected.ok(), Some(true), "a non-regular lockfile must fail without blocking startup");
}

#[tokio::test]
async fn a_short_agent_alias_works_when_its_canonical_path_exceeds_the_socket_limit() {
    use tokio::io::AsyncReadExt;
    let dir = Scratch::new();
    let long = dir.0.join("d".repeat(super::super::MAX_SOCKET_PATH));
    std::fs::create_dir(&long).unwrap();
    let alias = dir.0.join("short");
    std::os::unix::fs::symlink(&long, &alias).unwrap();
    let path = alias.join("agent");
    let agent = UnixListener::bind(&path).unwrap();
    assert!(std::fs::canonicalize(&path).unwrap().as_os_str().len() > super::super::MAX_SOCKET_PATH);
    let relay = dir.relay(Duration::from_secs(30));
    relay.start().unwrap();
    let conn = Arc::new(Conn::default());
    relay.register(&conn, path.to_str().unwrap()).unwrap();
    let mut client = UnixStream::connect(dir.0.join("core.sock.ssh-agent")).await.unwrap();
    let upstream = async {
        let (mut stream, _) = agent.accept().await?;
        let mut request = [0];
        stream.read_exact(&mut request).await?;
        stream.write_all(b"A").await
    };
    let downstream = async {
        client.write_all(b"?").await?;
        let mut answer = [0];
        client.read_exact(&mut answer).await?;
        Ok::<_, io::Error>(answer)
    };
    let result = tokio::time::timeout(Duration::from_secs(1), async { tokio::try_join!(upstream, downstream) }).await;
    relay.stop();
    assert_eq!(result.unwrap().unwrap().1, [b'A']);
}

#[tokio::test(start_paused = true)]
async fn expiry_revokes_a_silent_peer_but_inbound_progress_renews_it() {
    let dir = Scratch::new();
    let relay = dir.relay(Duration::from_secs(10));
    let path = dir.0.join("upstream");
    let _listener = UnixListener::bind(&path).unwrap();
    let conn = Arc::new(Conn::default());
    relay.register(&conn, path.to_str().unwrap()).unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(9)).await;
    // Bytes from the real peer, not a timer in its local bridge.
    let (mut tx, mut rx) = tokio::io::duplex(1);
    tx.write_all(b"x").await.unwrap();
    let mut watched = conn.heard.watch(&mut rx);
    tokio::io::AsyncReadExt::read_exact(&mut watched, &mut [0]).await.unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(relay.providers.lock().unwrap().len(), 1);
    tokio::time::advance(Duration::from_secs(9)).await;
    tokio::task::yield_now().await;
    assert!(relay.providers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_delivered_request_is_not_replayed_to_an_older_provider() {
    let dir = Scratch::new();
    let relay = dir.relay(Duration::from_secs(30));
    relay.start().unwrap();
    let older_path = dir.0.join("older");
    let newer_path = dir.0.join("newer");
    let older = UnixListener::bind(&older_path).unwrap();
    let newer = UnixListener::bind(&newer_path).unwrap();
    let c1 = Arc::new(Conn::default());
    let c2 = Arc::new(Conn::default());
    relay.register(&c1, older_path.to_str().unwrap()).unwrap();
    relay.register(&c2, newer_path.to_str().unwrap()).unwrap();
    let mut input = UnixStream::connect(dir.0.join("core.sock.ssh-agent")).await.unwrap();
    input.write_all(b"sign once").await.unwrap();
    let (mut upstream, _) = newer.accept().await.unwrap();
    let mut request = [0; 9];
    tokio::io::AsyncReadExt::read_exact(&mut upstream, &mut request).await.unwrap();
    assert_eq!(&request, b"sign once");
    relay.unregister(&c2);
    let mut byte = [0];
    assert_eq!(tokio::io::AsyncReadExt::read(&mut input, &mut byte).await.unwrap(), 0);
    assert!(tokio::time::timeout(Duration::from_millis(20), older.accept()).await.is_err());
    relay.stop();
}

#[tokio::test(start_paused = true)]
async fn an_unresponsive_agent_is_closed_after_the_inactivity_limit() {
    let dir = Scratch::new();
    let relay = dir.relay(Duration::from_secs(600));
    relay.start().unwrap();
    let path = dir.0.join("upstream");
    let agent = UnixListener::bind(&path).unwrap();
    let conn = Arc::new(Conn::default());
    relay.register(&conn, path.to_str().unwrap()).unwrap();
    let mut client = UnixStream::connect(dir.0.join("core.sock.ssh-agent")).await.unwrap();
    client.write_all(b"?").await.unwrap();
    let (mut upstream, _) = agent.accept().await.unwrap();
    tokio::io::AsyncReadExt::read_exact(&mut upstream, &mut [0]).await.unwrap();
    tokio::time::advance(IDLE_WAIT + Duration::from_secs(1)).await;
    assert_eq!(tokio::io::AsyncReadExt::read(&mut client, &mut [0]).await.unwrap(), 0);
    relay.stop();
}

#[tokio::test]
async fn connection_slots_are_bounded_and_return_after_revocation() {
    let dir = Scratch::new();
    let relay = dir.relay(Duration::from_secs(30));
    relay.start().unwrap();
    let path = dir.0.join("upstream");
    let agent = UnixListener::bind(&path).unwrap();
    let conn = Arc::new(Conn::default());
    relay.register(&conn, path.to_str().unwrap()).unwrap();
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        let client = UnixStream::connect(dir.0.join("core.sock.ssh-agent")).await.unwrap();
        let (upstream, _) = agent.accept().await.unwrap();
        held.push((client, upstream));
    }
    assert_eq!(relay.slots.available_permits(), 0);
    let mut extra = UnixStream::connect(dir.0.join("core.sock.ssh-agent")).await.unwrap();
    assert_eq!(tokio::io::AsyncReadExt::read(&mut extra, &mut [0]).await.unwrap(), 0);
    relay.unregister(&conn);
    for (mut client, _) in held {
        assert_eq!(tokio::io::AsyncReadExt::read(&mut client, &mut [0]).await.unwrap(), 0);
    }
    tokio::task::yield_now().await;
    assert_eq!(relay.slots.available_permits(), MAX_CONNECTIONS);
    relay.stop();
}
