#![cfg(unix)]

use serde_json::{Value, json};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Output {
    Pipe,
    Socket,
    HalfClosedSocket,
    Pty,
    File,
}

struct Connect {
    root: PathBuf,
    listener: UnixListener,
    child: Child,
    output: Option<Box<dyn Read>>,
    _terminal: Option<File>,
}

impl Connect {
    fn new(output: Output) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = PathBuf::from("/var/tmp").join(format!(
            "hc-connect-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let socket = root.join("core.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut terminal = None;
        let (stdout, reader): (Stdio, Option<Box<dyn Read>>) = match output {
            Output::Pipe => (Stdio::piped(), None),
            Output::Socket | Output::HalfClosedSocket => {
                let (reader, writer) = UnixStream::pair().unwrap();
                reader.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                if matches!(output, Output::HalfClosedSocket) {
                    reader.shutdown(Shutdown::Write).unwrap();
                }
                (Stdio::from(OwnedFd::from(writer)), Some(Box::new(reader)))
            }
            Output::Pty => {
                let (mut master, mut slave) = (-1, -1);
                assert_eq!(
                    unsafe {
                        libc::openpty(
                            &mut master,
                            &mut slave,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        )
                    },
                    0
                );
                let (reader, writer) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
                for file in [&reader, &writer] {
                    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
                    assert!(flags >= 0);
                    assert_eq!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) }, 0);
                }
                let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
                assert!(flags >= 0);
                assert_eq!(unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }, 0);
                let mut termios = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { libc::tcgetattr(writer.as_raw_fd(), &mut termios) }, 0);
                unsafe { libc::cfmakeraw(&mut termios) };
                assert_eq!(unsafe { libc::tcsetattr(writer.as_raw_fd(), libc::TCSANOW, &termios) }, 0);
                // Darwin discards unread output when the final slave closes.
                terminal = Some(writer.try_clone().unwrap());
                (Stdio::from(writer), Some(Box::new(reader)))
            }
            Output::File => {
                let path = root.join("reply");
                let writer = File::create(&path).unwrap();
                (Stdio::from(writer), Some(Box::new(File::open(path).unwrap())))
            }
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_herdr-core"))
            .arg("connect")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &root)
            .env("HC_SESSION", "connect-test")
            .env("HC_SOCKET", &socket)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = reader.or_else(|| child.stdout.take().map(|reader| Box::new(reader) as Box<dyn Read>));
        Self { root, listener, child, output, _terminal: terminal }
    }

    fn request(&mut self) -> UnixStream {
        writeln!(self.child.stdin.as_mut().unwrap(), "{}", json!({"id": 77, "method": "server.status"})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (stream, _) = loop {
                match self.listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    result => panic!("connect did not reach the fake core: {result:?}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            if request["id"] == 77 {
                return reader.into_inner();
            }
            // A macOS GUI caller first checks which login owns the core.
            assert_eq!(request["method"], "hello", "{request}");
            writeln!(reader.get_mut(), "{}", json!({"jsonrpc": "2.0", "id": request["id"], "result": {}})).unwrap();
        }
    }

    fn end_input(&mut self, core: &mut UnixStream) {
        drop(self.child.stdin.take());
        let mut rest = Vec::new();
        core.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "the request was already forwarded");
    }

    fn exited(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "connect exited with {status}");
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Connect {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn connect_exits_when_its_caller_leaves_even_if_the_core_is_silent() {
    for output in [Output::Pipe, Output::Socket, Output::HalfClosedSocket, Output::Pty] {
        let mut connect = Connect::new(output);
        let mut core = connect.request();
        connect.end_input(&mut core);
        drop(connect.output.take());
        assert!(connect.exited(), "{output:?}: connect kept waiting for the silent core after its caller left");
    }
}

#[test]
fn connect_keeps_a_reply_after_its_caller_is_done_sending() {
    for output in [Output::Pipe, Output::Socket, Output::HalfClosedSocket, Output::Pty, Output::File] {
        let mut connect = Connect::new(output);
        let mut core = connect.request();
        connect.end_input(&mut core);
        std::thread::sleep(Duration::from_millis(100));
        assert!(connect.child.try_wait().unwrap().is_none(), "{output:?}: stdin EOF discarded a pending reply");
        let reply = b"{\"id\":77,\"result\":\"delayed reply\"}\n";
        core.write_all(reply).unwrap();
        core.shutdown(Shutdown::Write).unwrap();
        assert!(connect.exited(), "{output:?}: connect did not finish after the reply");
        let mut received = vec![0; reply.len()];
        connect.output.as_mut().unwrap().read_exact(&mut received).unwrap();
        assert_eq!(received, reply, "{output:?}");
    }
}
