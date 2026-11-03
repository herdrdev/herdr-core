//! Each terminal program starts in a launchd job of its own, so macOS puts it
//! in a coalition of its own. A force quit of an app the program started (it
//! ends the app's whole coalition) then ends that terminal only, never the
//! holder or the other terminals.
//!
//! The holder keeps the PTY. For each program it starts a one-shot job that
//! runs `herdr-core --hc-launchd-spawn` (a "stub"), hands it the slave side and
//! the program's description over a socket, and the stub starts the program as
//! its parent and reports its pid and, at the end, its exit. The program and
//! the socket live and die together: whichever side loses the other ends it.

use super::macos_launchd::{self as launchd, Limits};
use super::unix::pty_command;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

/// An older binary doesn't know it and ends with a usage error.
pub const SENTINEL: &str = "--hc-launchd-spawn";
const PROTOCOL: u32 = 1;
/// From starting the job to the stub's hello.
const JOB: Duration = Duration::from_secs(3);
/// How long a program gets to end on each of its holder's hangup and terminate before it is killed: a lost holder
/// ended its programs the same way on Linux (the core's sweep: hangup, terminate, kill at 0.5 s apart).
const HANGUP: Duration = Duration::from_millis(500);
/// A start as a whole, from the job to the program's pid: the core gives up on it at 10 s.
const START: Duration = Duration::from_secs(9);
/// How long the stub waits for what the holder sends it.
const ANSWER: Duration = Duration::from_secs(5);
/// How long a dropped program waits for its stub to reap it.
const REAP: Duration = Duration::from_secs(2);

/// What the holder tells the stub. Holders outlive updates and the stub is whatever
/// binary is at the holder's path, so builds of any age meet: fields are only ever
/// added, with `#[serde(default)]`, and `PROTOCOL` stays.
#[derive(Serialize, Deserialize)]
struct Spec {
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    limits: Limits,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Hello(u32),
    Pid(u32),
    /// The program could not start: the OS error the holder would have had.
    Errno(i32),
    Error(String),
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// A program: ours directly, or the stub's.
pub enum Process {
    Direct(Child),
    Stub(Remote),
}

impl Process {
    pub fn id(&self) -> u32 {
        match self {
            Process::Direct(child) => child.id(),
            Process::Stub(remote) => remote.pid,
        }
    }
}

/// A program the stub started. The stub reaps it only once this is dropped,
/// so its pid stays ours until the holder is done with it.
/// Dropping waits (briefly) for the reap, as a direct child's wait would have.
pub struct Remote {
    pid: u32,
    reader: BufReader<UnixStream>,
}

impl Drop for Remote {
    fn drop(&mut self) {
        let sock = self.reader.get_ref();
        let _ = sock.shutdown(std::net::Shutdown::Write);
        let _ = sock.set_read_timeout(Some(REAP));
        let _ = io::copy(&mut self.reader, &mut io::sink());
    }
}

impl Remote {
    /// (exit code, signal). A stub gone without a word was killed on its own
    /// (a killed stub leaves its program running), so the program is killed
    /// too and reported as that kill. Once the stub is gone launchd reaps the
    /// program, and its pid could in theory be reused by then; it is not
    /// worth a check.
    pub fn wait(&mut self) -> io::Result<(Option<i32>, Option<i32>)> {
        // Darwin refuses to set it on a socket whose peer has gone: nothing to wait for then.
        let _ = self.reader.get_ref().set_read_timeout(None);
        Ok(match read_reply(&mut self.reader) {
            Ok(Reply::Exit { code, signal }) => (code, signal),
            _ => {
                unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
                (None, Some(libc::SIGKILL))
            }
        })
    }
}

/// Starts the program. A holder that has its own coalition gives the program
/// a job of its own, and that either works or fails visibly: starting it
/// directly would put it back in the holder's coalition. Any other holder (SSH,
/// or one whose own job failed) was never isolated, and starts programs directly.
pub fn start(argv: &[String], env: &BTreeMap<String, String>, cwd: &str, slave: &File) -> io::Result<Process> {
    if !launchd::isolated() {
        return Ok(Process::Direct(pty_command(argv, env, cwd, slave)?.spawn()?));
    }
    let spec = Spec { argv: argv.to_vec(), env: env.clone(), cwd: cwd.into(), limits: launchd::limits()? };
    let begun = Instant::now();
    let sock = connect(&stderr_path()).map_err(unavailable)?;
    launch(sock, slave, &spec, START.saturating_sub(begun.elapsed()).max(Duration::from_secs(1))).map(Process::Stub)
}

fn unavailable(error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("cannot start the program in a launchd job of its own: {error} (holder.log has more)"),
    )
}

/// Where this process's stderr goes (the holder's log), so a stub's own errors land there too.
fn stderr_path() -> PathBuf {
    let mut path = [0 as libc::c_char; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(2, libc::F_GETPATH, path.as_mut_ptr()) } == 0 {
        let path = unsafe { std::ffi::CStr::from_ptr(path.as_ptr()) };
        return PathBuf::from(OsStr::from_bytes(path.to_bytes()));
    }
    PathBuf::from("/dev/null")
}

/// A stub is running and has connected back; its job is gone.
fn connect(log: &Path) -> io::Result<UnixStream> {
    let start = Instant::now();
    let left = || JOB.saturating_sub(start.elapsed()).max(Duration::from_millis(1));
    let (mut job, listener) = launchd::start_job(
        std::env::current_exe()?.as_os_str(),
        SENTINEL,
        Path::new("/"),
        log,
        launchd::umask(),
        Some(left()),
    )?;
    let sock = launchd::accept(&listener, left())?;
    sock.set_read_timeout(Some(left()))?;
    // The stub has forked and seen its launcher exit: only the job goes.
    if let Err(error) = job.remove() {
        eprintln!("holder: cannot remove the launchd job of a started stub: {error}");
        // Dropping it tries again, which must not hold up the hand-off the stub is waiting for.
        std::thread::spawn(move || drop(job));
    }
    Ok(sock)
}

/// Hello, then the slave and the spec, then the stub's answer. Whatever fails
/// after the spec is sent, the stub ends the program once the socket closes.
fn launch(sock: UnixStream, slave: &File, spec: &Spec, answer: Duration) -> io::Result<Remote> {
    no_sigpipe(&sock);
    let mut reader = BufReader::new(sock);
    match read_reply(&mut reader).map_err(unavailable)? {
        Reply::Hello(PROTOCOL) => {}
        other => return Err(unavailable(io::Error::other(format!("unexpected stub hello {other:?}")))),
    }
    let sock = reader.get_mut();
    // A stub that stopped reading must not hold the holder up: it acts only on a whole line.
    sock.set_write_timeout(Some(answer)).ok();
    send_fd(sock, slave.as_raw_fd())
        .and_then(|()| {
            let mut line = serde_json::to_vec(spec)?;
            line.push(b'\n');
            sock.write_all(&line)
        })
        .map_err(unavailable)?;
    sock.set_read_timeout(Some(answer)).ok();
    match read_reply(&mut reader) {
        Ok(Reply::Pid(pid)) => Ok(Remote { pid, reader }),
        Ok(Reply::Errno(errno)) => Err(io::Error::from_raw_os_error(errno)),
        Ok(Reply::Error(text)) => Err(io::Error::other(text)),
        Ok(other) => Err(unavailable(io::Error::other(format!("unexpected stub answer {other:?}")))),
        Err(error) => Err(unavailable(error)),
    }
}

/// A write to a peer that has gone is an error to handle, whatever the process does with SIGPIPE
/// (a failed exec in the same process puts it back to the default). Darwin refuses it on a socket
/// whose peer has already gone, and then the write fails the same way.
fn no_sigpipe(sock: &UnixStream) {
    let on: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw const on).cast(),
            std::mem::size_of_val(&on) as libc::socklen_t,
        )
    };
}

fn read_reply(reader: &mut impl BufRead) -> io::Result<Reply> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn send_reply(sock: &mut UnixStream, reply: &Reply) -> io::Result<()> {
    let mut line = serde_json::to_vec(reply)?;
    line.push(b'\n');
    sock.write_all(&line)
}

/// The stub: starts one program for the holder that started its job.
pub fn serve(socket: &Path) -> io::Result<()> {
    // The stub lives exactly as long as its program: only SIGKILL or the program's end takes it.
    // A handler, unlike ignoring, is reset by exec, so the program gets the default dispositions.
    extern "C" fn nothing(_: libc::c_int) {}
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        unsafe { libc::signal(signal, nothing as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
    run(launchd::connect_back(socket)?)
}

fn run(mut sock: UnixStream) -> io::Result<()> {
    no_sigpipe(&sock);
    send_reply(&mut sock, &Reply::Hello(PROTOCOL))?;
    sock.set_read_timeout(Some(ANSWER))?;
    let slave = unsafe { File::from_raw_fd(recv_fd(&sock)?) };
    unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    let mut line = String::new();
    BufReader::new(&sock).read_line(&mut line)?;
    // A spec cut short by the holder's write timeout must not start a program.
    let spec = Some(&line)
        .filter(|l| l.ends_with('\n'))
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
        .and_then(|l| serde_json::from_str::<Spec>(l).map_err(io::Error::from))
        .and_then(|spec| launchd::restore_limits(&spec.limits).map(|()| spec))
        .map_err(|error| Reply::Error(unavailable(error).to_string()));
    // What the holder would have had on its own, with the text of that error.
    let child = spec.and_then(|spec| {
        pty_command(&spec.argv, &spec.env, &spec.cwd, &slave)
            .and_then(|mut cmd| cmd.spawn())
            .map_err(|error| error.raw_os_error().map_or_else(|| Reply::Error(error.to_string()), Reply::Errno))
    });
    // Only the program may hold the terminal open.
    drop(slave);
    match child {
        Ok(child) => babysit(sock, child),
        Err(reply) => send_reply(&mut sock, &reply),
    }
}

/// Reports `child` to the holder until it ends. The holder going away first
/// (it gave up on the start, or ended) ends the program: nobody could use it.
fn babysit(mut sock: UnixStream, mut child: Child) -> io::Result<()> {
    no_sigpipe(&sock);
    let pid = child.id();
    let holder = sock.try_clone()?;
    let _ = sock.set_read_timeout(None);
    // The holder gave up on the start before the pid reached it: nobody has this program, so it ends now.
    // A pid that was written just before the holder left is hung up at once by the watcher below instead.
    if send_reply(&mut sock, &Reply::Pid(pid)).is_err() {
        let _ = child.kill();
        return child.wait().map(drop);
    }
    let watch = std::thread::spawn(move || {
        let mut holder = holder;
        // The holder says nothing after the spec: any end of the socket is its leaving.
        while matches!(holder.read(&mut [0u8; 1]), Ok(1..)) {}
        // Not reaped before this thread is joined, so the pid is still the program's.
        // As on a closed terminal: a hangup, then a terminate, a kill only for a program that ignores both.
        for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGKILL] {
            unsafe { libc::kill(pid as libc::pid_t, signal) };
            let deadline = Instant::now() + HANGUP;
            while !has_ended(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if has_ended(pid) {
                break;
            }
        }
    });
    // Not reaped yet: the pid stays the program's until the holder lets go.
    let (code, signal) = ended(pid);
    let _ = send_reply(&mut sock, &Reply::Exit { code, signal });
    let _ = watch.join();
    child.wait().map(drop)
}

fn has_ended(pid: u32) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let found = unsafe {
        libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT)
    };
    found == 0 && unsafe { info.si_pid() } != 0
}

/// Waits for the process to end without reaping it.
fn ended(pid: u32) -> (Option<i32>, Option<i32>) {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) } == 0 {
            break;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return (None, None);
        }
    }
    let status = info.si_status;
    match info.si_code {
        libc::CLD_EXITED => (Some(status), None),
        _ => (None, Some(status)),
    }
}

fn send_fd(sock: &UnixStream, fd: RawFd) -> io::Result<()> {
    let mut byte = 0u8;
    let mut iov = libc::iovec { iov_base: (&raw mut byte).cast(), iov_len: 1 };
    let mut space = [0u64; 4];
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = space.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(4) as _;
        libc::CMSG_DATA(header).cast::<RawFd>().write_unaligned(fd);
        if libc::sendmsg(sock.as_raw_fd(), &msg, 0) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_fd(sock: &UnixStream) -> io::Result<RawFd> {
    let mut byte = 0u8;
    let mut iov = libc::iovec { iov_base: (&raw mut byte).cast(), iov_len: 1 };
    let mut space = [0u64; 4];
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = space.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        if libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) != 1 {
            return Err(io::Error::last_os_error());
        }
        let header = libc::CMSG_FIRSTHDR(&msg);
        if header.is_null() || (*header).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("the holder sent no terminal"));
        }
        Ok(libc::CMSG_DATA(header).cast::<RawFd>().read_unaligned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn spec() -> Spec {
        Spec { argv: vec!["sh".into()], env: BTreeMap::new(), cwd: "/".into(), limits: Vec::new() }
    }

    /// The holder's side against a fake stub that says `hello`, takes the slave
    /// and the spec, then runs `answer` on the socket.
    fn against(hello: Option<Reply>, answer: impl FnOnce(&mut UnixStream) + Send + 'static) -> io::Result<Remote> {
        let (holder, mut stub) = UnixStream::pair().unwrap();
        let fake = std::thread::spawn(move || {
            if let Some(hello) = hello {
                send_reply(&mut stub, &hello).unwrap();
                if recv_fd(&stub).is_ok() {
                    let mut line = String::new();
                    BufReader::new(&stub).read_line(&mut line).unwrap();
                    serde_json::from_str::<Spec>(&line).expect("a spec the stub accepts");
                    answer(&mut stub);
                }
            }
        });
        let slave = File::open("/dev/null").unwrap();
        let launch = launch(holder, &slave, &spec(), Duration::from_millis(300));
        fake.join().unwrap();
        launch
    }

    fn gone(pid: u32) -> bool {
        unsafe {
            libc::kill(pid as libc::pid_t, 0) != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }
    }

    #[test]
    fn a_started_program_reports_its_pid_then_its_exit() {
        let started = |s: &mut UnixStream| {
            send_reply(s, &Reply::Pid(4242)).unwrap();
            send_reply(s, &Reply::Exit { code: Some(3), signal: None }).unwrap();
        };
        let mut remote = against(Some(Reply::Hello(PROTOCOL)), started).unwrap();
        assert_eq!((remote.pid, remote.wait().unwrap()), (4242, (Some(3), None)));
    }

    #[test]
    fn a_stub_that_goes_quiet_after_the_spec_is_an_error_naming_launchd() {
        // Alive but silent, not hung up.
        let quiet = |_: &mut UnixStream| std::thread::sleep(Duration::from_millis(600));
        let error = against(Some(Reply::Hello(PROTOCOL)), quiet).err().unwrap();
        assert!(error.to_string().contains("launchd"), "{error}");
    }

    #[test]
    fn a_start_error_keeps_the_os_error_text() {
        let error = against(Some(Reply::Hello(PROTOCOL)), |s| send_reply(s, &Reply::Errno(2)).unwrap()).err().unwrap();
        assert_eq!(error.to_string(), io::Error::from_raw_os_error(2).to_string());
    }

    #[test]
    fn a_stub_of_another_protocol_or_none_is_an_error() {
        assert!(against(Some(Reply::Hello(PROTOCOL + 1)), |_| {}).is_err());
        assert!(against(None, |_| {}).is_err());
    }

    #[test]
    fn a_lost_stub_kills_its_program_and_reports_the_kill() {
        let mut program = Command::new("sleep").arg("300").spawn().unwrap();
        let (holder, stub) = UnixStream::pair().unwrap();
        drop(stub);
        let mut remote = Remote { pid: program.id(), reader: BufReader::new(holder) };
        assert_eq!(remote.wait().unwrap(), (None, Some(libc::SIGKILL)));
        let ended = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            program.try_wait().unwrap().is_some()
        });
        assert!(ended, "the program outlived its stub");
        let _ = program.kill();
    }

    #[test]
    fn a_program_that_cannot_start_for_want_of_an_errno_has_the_error_of_a_direct_start() {
        let spec = Spec { argv: vec!["sh".into(), "a\0b".into()], ..spec() };
        let direct = pty_command(&spec.argv, &spec.env, &spec.cwd, &File::open("/dev/null").unwrap())
            .unwrap()
            .spawn()
            .unwrap_err();
        let (mut holder, stub) = UnixStream::pair().unwrap();
        let stub = std::thread::spawn(move || run(stub));
        let mut reader = BufReader::new(holder.try_clone().unwrap());
        assert_eq!(read_reply(&mut reader).unwrap(), Reply::Hello(PROTOCOL));
        send_fd(&holder, File::open("/dev/null").unwrap().as_raw_fd()).unwrap();
        holder.write_all(&[serde_json::to_vec(&spec).unwrap(), b"\n".to_vec()].concat()).unwrap();
        let reply = read_reply(&mut reader).unwrap();
        stub.join().unwrap().unwrap();
        let started = Reply::Error(direct.to_string());
        assert_eq!(reply, started);
        let err = against(Some(Reply::Hello(PROTOCOL)), move |s| send_reply(s, &started).unwrap()).err().unwrap();
        assert_eq!(err.to_string(), direct.to_string());
    }

    #[test]
    fn a_spec_cut_short_does_not_start_a_program() {
        let marker = std::env::temp_dir().join(format!("hc-stub-spec-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let (mut holder, stub) = UnixStream::pair().unwrap();
        let stub = std::thread::spawn(move || run(stub));
        let mut reader = BufReader::new(holder.try_clone().unwrap());
        assert_eq!(read_reply(&mut reader).unwrap(), Reply::Hello(PROTOCOL));
        send_fd(&holder, File::open("/dev/null").unwrap().as_raw_fd()).unwrap();
        let argv = vec!["touch".to_string(), marker.display().to_string()];
        // Whole, but for the newline the holder's write timeout cut off.
        holder.write_all(&serde_json::to_vec(&Spec { argv, ..spec() }).unwrap()).unwrap();
        holder.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(read_reply(&mut reader).unwrap(), Reply::Error(_)));
        stub.join().unwrap().unwrap();
        assert!(!marker.exists(), "a program started from a spec that was not whole");
    }

    #[test]
    fn a_program_gets_a_hangup_then_a_terminate_when_its_holder_leaves_and_a_kill_when_it_ignores_them() {
        let dir = std::env::temp_dir().join(format!("hc-stub-hup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The first program saves its work on a hangup, the second on a terminate; the third shrugs both off.
        let scripts = [
            format!(
                "trap 'touch {0}/saved; exit 0' HUP; touch {0}/ready1; while :; do sleep 0.05; done",
                dir.display()
            ),
            format!(
                "trap '' HUP; trap 'touch {0}/saved; exit 0' TERM; touch {0}/ready2; while :; do sleep 0.05; done",
                dir.display()
            ),
            format!("trap '' HUP TERM; touch {0}/ready3; while :; do sleep 0.05; done", dir.display()),
        ];
        for (n, script) in scripts.iter().enumerate() {
            let program = Command::new("sh").args(["-c", script]).spawn().unwrap();
            let pid = program.id();
            while !dir.join(format!("ready{}", n + 1)).exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
            let (mut holder, stub) = UnixStream::pair().unwrap();
            let (done, ended) = std::sync::mpsc::channel();
            std::thread::spawn(move || done.send(babysit(stub, program)).unwrap());
            assert_eq!(read_reply(&mut BufReader::new(&mut holder)).unwrap(), Reply::Pid(pid));
            drop(holder);
            let result = ended.recv_timeout(Duration::from_secs(10));
            if result.is_err() {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
            result.expect("the program outlived its holder").unwrap();
            assert_eq!(dir.join("saved").exists(), n < 2, "program {n}: hangup, then terminate, then kill");
            let _ = std::fs::remove_file(dir.join("saved"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_whose_start_the_holder_gave_up_on_is_killed_at_once() {
        let ready = std::env::temp_dir().join(format!("hc-stub-gave-up-{}", std::process::id()));
        let script = format!("trap '' HUP; touch {}; while :; do sleep 0.05; done", ready.display());
        let program = Command::new("sh").args(["-c", &script]).spawn().unwrap();
        let pid = program.id();
        // Before the trap is set, a hangup would end it anyway.
        while !ready.exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&ready);
        let (holder, stub) = UnixStream::pair().unwrap();
        no_sigpipe(&stub);
        drop(holder);
        let began = Instant::now();
        let result = babysit(stub, program);
        if began.elapsed() > Duration::from_millis(500) {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        result.unwrap();
        assert!(began.elapsed() < Duration::from_millis(500), "ran on for {:?} after the holder left", began.elapsed());
    }

    #[test]
    fn a_program_that_ends_is_reported_then_reaped_when_the_holder_lets_go() {
        let program = Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
        let pid = program.id();
        let (holder, stub) = UnixStream::pair().unwrap();
        let stub = std::thread::spawn(move || babysit(stub, program));
        let mut reader = BufReader::new(holder);
        assert_eq!(read_reply(&mut reader).unwrap(), Reply::Pid(pid));
        let mut remote = Remote { pid, reader };
        assert_eq!(remote.wait().unwrap(), (Some(3), None));
        assert!(!gone(pid), "reaped before the holder let go");
        drop(remote);
        stub.join().unwrap().unwrap();
        assert!(gone(pid));
    }
}
