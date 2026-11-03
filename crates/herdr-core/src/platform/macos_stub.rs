//! Each terminal program starts in a launchd job of its own, so macOS puts it
//! in a coalition of its own. A force quit of an app the program started (it
//! ends the app's whole coalition) then ends that terminal only, never the
//! holder or the other terminals.
//!
//! The holder keeps the PTY. For each program it starts a one-shot job that
//! runs `herdr-core --hc-launchd-spawn` (a "stub"), hands it the slave side and
//! the program's description over a socket, and the stub starts the program as
//! its parent and reports its pid and, at the end, its exit.

use super::macos_launchd::{self as launchd, Limits};
use super::unix::{pty_command, spawn_direct};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// An older binary doesn't know it and ends with a usage error.
pub const SENTINEL: &str = "--hc-launchd-spawn";
const PROTOCOL: u32 = 1;
/// From starting the job to the stub's hello: past it, programs start directly.
const JOB: Duration = Duration::from_secs(2);
/// A start as a whole, from the job to the program's pid: the core gives up on it at 10 s.
const START: Duration = Duration::from_secs(9);
/// How long the stub waits for what the holder sends it.
const ANSWER: Duration = Duration::from_secs(5);
/// After launchd failed to start a stub, programs start directly this long.
const COOLDOWN: Duration = Duration::from_secs(60);
/// How long a dropped program waits for its stub to reap it.
const REAP: Duration = Duration::from_secs(2);

/// What the holder tells the stub. A build that changes it bumps `PROTOCOL`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    limits: Limits,
    /// Replace the stub with `argv` (the holder) instead of starting it as a child.
    #[serde(default)]
    exec: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Hello(u32),
    /// Nothing was started: the holder may start the program itself.
    Refused(String),
    /// The stub is about to `exec` the holder: the socket closing next is the success.
    Execing,
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
    /// (exit code, signal). A stub gone without a word (its coalition was
    /// ended, so the program went too) is neither.
    pub fn wait(&mut self) -> io::Result<(Option<i32>, Option<i32>)> {
        // Darwin refuses to set it on a socket whose peer has gone: nothing to wait for then.
        let _ = self.reader.get_ref().set_read_timeout(None);
        Ok(match read_reply(&mut self.reader) {
            Ok(Reply::Exit { code, signal }) => (code, signal),
            _ => (None, None),
        })
    }
}

/// Starts the program: in a job of its own when launchd allows, else directly.
pub fn start(argv: &[String], env: &BTreeMap<String, String>, cwd: &str, slave: &File) -> io::Result<Process> {
    let direct = || Ok(Process::Direct(spawn_direct(argv, env, cwd, slave)?));
    if launchd::ssh() || cooling_down() {
        return direct();
    }
    let Ok(limits) = launchd::limits() else { return direct() };
    let spec = Spec { argv: argv.to_vec(), env: env.clone(), cwd: cwd.into(), limits, exec: false };
    let begun = Instant::now();
    let sock = match connect(&stderr_path()) {
        Ok(sock) => sock,
        Err(error) => {
            cool_down("a program", &error);
            return direct();
        }
    };
    match launch(sock, slave, &spec, left(begun)) {
        Launch::Started(remote) => Ok(Process::Stub(remote)),
        Launch::Unavailable(error) => {
            cool_down("a program", &error);
            direct()
        }
        Launch::Refused(reason) => {
            cool_down("a program", &io::Error::other(reason));
            direct()
        }
        Launch::Failed(error) => Err(error),
        Launch::Execed => unreachable!("only a holder's job replaces itself"),
    }
}

/// Starts `cmd` (a holder) as a job's own process, so it has a coalition of its
/// own too. False when it must start directly, with the reason logged.
pub fn start_exec(cmd: &Command, log: &Path) -> bool {
    if launchd::ssh() || cooling_down() {
        return false;
    }
    let spec = (|| {
        let mut env: BTreeMap<String, String> = super::utf8_vars().collect();
        for (key, value) in cmd.get_envs() {
            match (key.to_str(), value.map(OsStr::to_str)) {
                (Some(key), Some(Some(value))) => env.insert(key.into(), value.into()),
                (Some(key), None) => env.remove(key),
                _ => None,
            };
        }
        let argv = std::iter::once(cmd.get_program()).chain(cmd.get_args());
        let argv = argv.map(|arg| arg.to_str().map(String::from)).collect::<Option<Vec<_>>>()?;
        let cwd = std::env::current_dir().ok()?.to_str()?.to_string();
        Some(Spec { argv, env, cwd, limits: launchd::limits().ok()?, exec: true })
    })();
    let Some(spec) = spec else { return false };
    let nothing = File::open("/dev/null").ok();
    let begun = Instant::now();
    let launched = connect(log).and_then(|sock| {
        match launch(sock, nothing.as_ref().ok_or(io::Error::other("no /dev/null"))?, &spec, left(begun)) {
            Launch::Execed => Ok(()),
            Launch::Unavailable(error) | Launch::Failed(error) => Err(error),
            Launch::Refused(reason) => Err(io::Error::other(reason)),
            Launch::Started(_) => Err(io::Error::other("a stub started the holder as a program")),
        }
    });
    match launched {
        Ok(()) => true,
        Err(error) => {
            cool_down("the holder", &error);
            false
        }
    }
}

/// What is left of `START`.
fn left(begun: Instant) -> Duration {
    START.saturating_sub(begun.elapsed()).max(Duration::from_secs(1))
}

static COOL_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

fn cooling_down() -> bool {
    COOL_UNTIL.lock().unwrap_or_else(|p| p.into_inner()).is_some_and(|until| Instant::now() < until)
}

fn cool_down(what: &str, error: &io::Error) {
    eprintln!("hc: no launchd job for {what} ({error}); starting programs directly for a minute");
    *COOL_UNTIL.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now() + COOLDOWN);
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
    job.remove()?;
    Ok(sock)
}

enum Launch {
    Started(Remote),
    /// No usable stub: launchd or the binary is at fault; nothing was started.
    Unavailable(io::Error),
    /// The stub declined before starting anything.
    Refused(String),
    /// The stub became the program (its socket closed on exec).
    Execed,
    /// The program was (or may have been) started and did not come up.
    Failed(io::Error),
}

/// Hello, then the slave and the spec, then the stub's answer. Before the spec
/// is sent, any failure leaves the holder free to start the program itself.
/// After it, only a refusal does: a stub that went quiet may have started it.
fn launch(sock: UnixStream, slave: &File, spec: &Spec, answer: Duration) -> Launch {
    let mut reader = BufReader::new(sock);
    match read_reply(&mut reader) {
        Ok(Reply::Hello(PROTOCOL)) => {}
        Ok(other) => return Launch::Unavailable(io::Error::other(format!("unexpected stub hello {other:?}"))),
        Err(error) => return Launch::Unavailable(error),
    }
    let sock = reader.get_mut();
    // A stub that stopped reading must not hold the holder up: it acts only on a whole line.
    sock.set_write_timeout(Some(answer)).ok();
    let sent = send_fd(sock, slave.as_raw_fd()).and_then(|()| {
        let mut line = serde_json::to_vec(spec)?;
        line.push(b'\n');
        sock.write_all(&line)
    });
    if let Err(error) = sent {
        return Launch::Unavailable(error);
    }
    sock.set_read_timeout(Some(answer)).ok();
    match read_reply(&mut reader) {
        Ok(Reply::Pid(pid)) => Launch::Started(Remote { pid, reader }),
        Ok(Reply::Refused(reason)) => Launch::Refused(reason),
        Ok(Reply::Execing) if spec.exec => match read_reply(&mut reader) {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Launch::Execed,
            Ok(Reply::Errno(errno)) => Launch::Failed(io::Error::from_raw_os_error(errno)),
            Ok(Reply::Error(text)) => Launch::Failed(io::Error::other(text)),
            _ => Launch::Failed(io::Error::other("launcher lost")),
        },
        Ok(Reply::Errno(errno)) => Launch::Failed(io::Error::from_raw_os_error(errno)),
        Ok(Reply::Error(text)) => Launch::Failed(io::Error::other(text)),
        _ => Launch::Failed(io::Error::other("launcher lost")),
    }
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
    let mut sock = launchd::connect_back(socket)?;
    send_reply(&mut sock, &Reply::Hello(PROTOCOL))?;
    sock.set_read_timeout(Some(ANSWER))?;
    let slave = unsafe { File::from_raw_fd(recv_fd(&sock)?) };
    unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    let mut line = String::new();
    BufReader::new(&sock).read_line(&mut line)?;
    // A spec cut short by the holder's write timeout must not start a program the holder will start again.
    let started = Some(&line)
        .filter(|l| l.ends_with('\n'))
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
        .and_then(|l| serde_json::from_str::<Spec>(l).map_err(io::Error::from))
        .and_then(|spec| {
            launchd::restore_limits(&spec.limits)?;
            let cmd =
                if spec.exec { plain_command(&spec)? } else { pty_command(&spec.argv, &spec.env, &spec.cwd, &slave)? };
            Ok((spec, cmd))
        })
        .map_err(|error| error.to_string());
    let (spec, mut cmd) = match started {
        Ok(started) => started,
        Err(error) => return send_reply(&mut sock, &Reply::Refused(error)),
    };
    if spec.exec {
        drop(slave);
        send_reply(&mut sock, &Reply::Execing)?;
        let error = cmd.exec();
        return send_reply(
            &mut sock,
            &error.raw_os_error().map_or_else(|| Reply::Error(error.to_string()), Reply::Errno),
        );
    }
    let child = cmd.spawn();
    // Only the program may hold the terminal open.
    drop(cmd);
    drop(slave);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let reply = error.raw_os_error().map_or_else(|| Reply::Error(error.to_string()), Reply::Errno);
            return send_reply(&mut sock, &reply);
        }
    };
    send_reply(&mut sock, &Reply::Pid(child.id()))?;
    // Not reaped yet: the pid stays the program's until the holder lets go.
    let (code, signal) = ended(child.id());
    let _ = send_reply(&mut sock, &Reply::Exit { code, signal });
    let _ = sock.set_read_timeout(None);
    let _ = io::Read::read(&mut &sock, &mut [0u8; 1]);
    child.wait().map(drop)
}

fn plain_command(spec: &Spec) -> io::Result<Command> {
    let program = spec.argv.first().ok_or_else(|| io::Error::other("empty argv"))?;
    let mut cmd = Command::new(program);
    cmd.args(&spec.argv[1..]).env_clear().envs(&spec.env).current_dir(&spec.cwd);
    Ok(cmd)
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

    fn spec() -> Spec {
        Spec { argv: vec!["sh".into()], env: BTreeMap::new(), cwd: "/".into(), limits: Vec::new(), exec: false }
    }

    /// The holder's side against a fake stub that says `hello`, takes the slave
    /// and the spec, then runs `answer` on the socket.
    fn against(hello: Option<Reply>, answer: impl FnOnce(&mut UnixStream) + Send + 'static) -> Launch {
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

    #[test]
    fn a_stub_that_refuses_leaves_the_program_to_the_holder() {
        let refuse = |s: &mut UnixStream| send_reply(s, &Reply::Refused("old spec".into())).unwrap();
        assert!(
            matches!(against(Some(Reply::Hello(PROTOCOL)), refuse), Launch::Refused(reason) if reason == "old spec")
        );
    }

    #[test]
    fn a_started_program_reports_its_pid_then_its_exit() {
        let started = |s: &mut UnixStream| {
            send_reply(s, &Reply::Pid(4242)).unwrap();
            send_reply(s, &Reply::Exit { code: Some(3), signal: None }).unwrap();
        };
        let Launch::Started(mut remote) = against(Some(Reply::Hello(PROTOCOL)), started) else { panic!() };
        assert_eq!((remote.pid, remote.wait().unwrap()), (4242, (Some(3), None)));
    }

    #[test]
    fn a_stub_that_dies_after_the_spec_never_leads_to_a_second_program() {
        let Launch::Failed(error) = against(Some(Reply::Hello(PROTOCOL)), |_| {}) else { panic!() };
        assert_eq!(error.to_string(), "launcher lost");
    }

    #[test]
    fn a_stub_that_closes_after_announcing_the_exec_has_become_the_holder() {
        let exec = Spec { exec: true, ..spec() };
        let (holder, mut stub) = UnixStream::pair().unwrap();
        let fake = std::thread::spawn(move || {
            send_reply(&mut stub, &Reply::Hello(PROTOCOL)).unwrap();
            recv_fd(&stub).unwrap();
            BufReader::new(&stub).read_line(&mut String::new()).unwrap();
            send_reply(&mut stub, &Reply::Execing).unwrap();
        });
        let slave = File::open("/dev/null").unwrap();
        assert!(matches!(launch(holder, &slave, &exec, Duration::from_millis(300)), Launch::Execed));
        fake.join().unwrap();
        let eof = |_: &mut UnixStream| {};
        assert!(
            matches!(against(Some(Reply::Hello(PROTOCOL)), eof), Launch::Failed(_)),
            "closing alone is not success"
        );
    }

    #[test]
    fn a_start_error_keeps_the_os_error_text() {
        let Launch::Failed(error) = against(Some(Reply::Hello(PROTOCOL)), |s| send_reply(s, &Reply::Errno(2)).unwrap())
        else {
            panic!()
        };
        assert_eq!(error.to_string(), io::Error::from_raw_os_error(2).to_string());
    }

    #[test]
    fn a_stub_of_another_protocol_or_none_means_direct_start() {
        assert!(matches!(against(Some(Reply::Hello(PROTOCOL + 1)), |_| {}), Launch::Unavailable(_)));
        assert!(matches!(against(None, |_| {}), Launch::Unavailable(_)));
    }

    #[test]
    fn a_lost_stub_is_neither_an_exit_code_nor_a_signal() {
        let (holder, stub) = UnixStream::pair().unwrap();
        drop(stub);
        let mut remote = Remote { pid: 1, reader: BufReader::new(holder) };
        assert_eq!(remote.wait().unwrap(), (None, None));
    }
}
