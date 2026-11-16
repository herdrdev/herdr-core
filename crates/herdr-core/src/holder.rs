//! The holder: owns every PTY, is every program's parent, always reads output.
//! Spawn, pump, resize, signal, reap. No parsing, no policy. It keeps a ~1 MB
//! tail per terminal so a core that crashed can rebuild screens, and restarts
//! a crashed core itself.

use crate::holder_proto::{self as proto, Exit, Goodbye, HeldTerminal};
use crate::paths::Paths;
use crate::platform::{self, Process, Pty, Stream};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const TAIL_BYTES: usize = 1 << 20;
const EXIT_DRAIN: Duration = Duration::from_millis(200);
/// Output queued for a core that reads slowly before programs have to wait for it.
const BACKLOG_BYTES: usize = 8 << 20;
/// The most output one frame carries.
const FRAME_BYTES: usize = 64 << 10;

// A program that writes in tiny pieces (`yes`, `seq`, a busy log) would cost a
// wakeup, a read and a frame per piece in the holder, the core and every viewer.
// So a small read that follows another small one closely waits a moment and
// takes what arrived meanwhile, in one frame. A write after a quiet spell or
// after a big read (an echoed keystroke, a prompt) never waits.
/// Not on Windows: ConPTY already batches its output, and the wait there is unmeasured.
const GATHERS: bool = cfg!(unix);
/// A small read this soon after the last small one is part of a burst.
const BURST_GAP: Duration = Duration::from_millis(1);
/// The longest a burst waits: short enough not to be seen, long enough to
/// gather a few hundred bytes from a fast writer. Sub-millisecond, which
/// poll can't time (and macOS has no ppoll), so it sleeps. macOS oversleeps
/// (250 µs asked: 330 µs median, 560 µs p95), hence 200.
const GATHER_WAIT: Duration = Duration::from_micros(200);
/// The shortest; below it waiting saves little.
const GATHER_WAIT_MIN: Duration = Duration::from_micros(16);
/// A read at least this big found output piling up: no need to wait for more.
/// Half the smallest PTY queue (macOS: 1 KiB).
const SMALL_READ: usize = 512;

struct Term {
    pid: u32,
    pty: Arc<Pty>,
    /// Input goes through its own thread: a program that stops reading blocks only itself.
    input: std::sync::mpsc::Sender<Vec<u8>>,
    cols: u16,
    rows: u16,
    tail: VecDeque<u8>,
    exit: Option<Exit>,
    reader_done: bool,
    /// Since when its output could drain; None while its reader waits for a
    /// core that fell behind. An exit waits for its output from then on.
    drains_since: Option<Instant>,
}

/// The connected core. Frames go out through its own writer thread: a core
/// that stops reading must never block anyone holding the state lock.
struct Core {
    generation: u64,
    frames: std::sync::mpsc::Sender<Vec<u8>>,
    backlog: Arc<Backlog>,
    /// Shut down to free a writer stuck on a core that is gone.
    stream: Stream,
}

/// Bytes queued for one core; None once its writer stopped.
struct Backlog {
    bytes: Mutex<Option<usize>>,
    changed: Condvar,
}

impl Backlog {
    fn update(&self, f: impl FnOnce(&mut Option<usize>)) {
        f(&mut self.bytes.lock().unwrap_or_else(|p| p.into_inner()));
        self.changed.notify_all();
    }

    fn over(&self, limit: usize) -> bool {
        self.bytes.lock().unwrap_or_else(|p| p.into_inner()).is_some_and(|n| n > limit)
    }

    /// Waits (never under the state lock) until at most `limit` bytes are queued or the writer stopped.
    fn wait(&self, limit: usize, timeout: Duration) {
        let bytes = self.bytes.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self.changed.wait_timeout_while(bytes, timeout, |b| b.is_some_and(|n| n > limit));
    }
}

/// One terminal's reads, gathered while it writes in bursts (see BURST_GAP).
struct Gather {
    last: Option<Instant>,
    /// Shrinks while output piles up during it, since a writer that fast would
    /// fill the PTY queue and stall; grows back once it doesn't.
    wait: Duration,
}

impl Default for Gather {
    fn default() -> Self {
        Gather { last: None, wait: GATHER_WAIT }
    }
}

impl Gather {
    /// What one read returns; in a burst, also what arrives during the wait.
    fn read(&mut self, reader: &mut impl platform::PtyOutput, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = reader.read(buf)?;
        let small = GATHERS && n > 0 && n < SMALL_READ;
        if small && self.last.is_some_and(|t| t.elapsed() < BURST_GAP) {
            std::thread::sleep(self.wait);
            let first = n;
            // An end or error is left for the next read, after these bytes went out.
            while let Ok(Some(m @ 1..)) = reader.read_now(&mut buf[n..]) {
                n += m;
                if m < SMALL_READ || n == buf.len() {
                    break;
                }
            }
            self.wait = if n - first >= SMALL_READ {
                (self.wait / 2).max(GATHER_WAIT_MIN)
            } else {
                (self.wait * 2).min(GATHER_WAIT)
            };
        }
        // Only small reads make a burst: what follows a big one (the prompt after a `cat`) goes at once.
        self.last = small.then(Instant::now);
        Ok(n)
    }
}

struct State {
    terms: HashMap<u32, Term>,
    core: Option<Core>,
    generation: u64,
    previous: &'static str,
    core_left: Instant,
    restarts: Vec<Instant>,
}

struct Holder {
    login_session: Option<platform::LoginSession>,
    state: Mutex<State>,
    changed: Condvar,
    instance: String,
    grace: Duration,
    paths: Paths,
    socket: std::path::PathBuf,
    /// A newer build took over: exit once the last terminal is gone.
    draining: std::sync::atomic::AtomicBool,
}

/// Runs a holder on `socket` (each holder has its own; the core picks the path).
pub fn run(socket: std::path::PathBuf) -> io::Result<()> {
    let login_session = platform::login_session();
    platform::prepare_holder();
    platform::raise_nofile_limit();
    let paths = Paths::from_env().map_err(|e| io::Error::other(e.message))?;
    paths.create_dirs()?;
    // A taken path means another holder lives there; a stale file is removed by the core.
    let listener = platform::listen(&socket)?;
    let grace = std::env::var("HC_HOLDER_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(3), Duration::from_millis);
    let holder = Arc::new(Holder {
        login_session,
        state: Mutex::new(State {
            terms: HashMap::new(),
            core: None,
            generation: 0,
            previous: "none",
            core_left: Instant::now(),
            restarts: Vec::new(),
        }),
        changed: Condvar::new(),
        instance: crate::platform::random_hex(),
        grace,
        paths,
        socket,
        draining: Default::default(),
    });
    let supervisor = holder.clone();
    std::thread::spawn(move || supervisor.supervise());
    for stream in listener.incoming() {
        let holder = holder.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(e) = holder.serve(stream) {
                eprintln!("holder: core connection: {e}");
            }
        });
    }
    Ok(())
}

impl Holder {
    fn exit(&self) -> ! {
        platform::remove_endpoint(&self.socket);
        std::process::exit(0);
    }

    fn exit_if_drained(&self) {
        if self.draining.load(std::sync::atomic::Ordering::SeqCst) && self.lock().terms.is_empty() {
            self.exit();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queues a frame for the current core.
    fn to_core(&self, st: &mut State, frame: Vec<u8>) {
        if let Some(core) = &st.core {
            core.backlog.update(|b| *b = b.map(|n| n + frame.len()));
            let _ = core.frames.send(frame);
        }
    }

    /// A handover lets the writer deliver what is queued (BYE last) but no longer
    /// holds programs back for it; any other end cuts it off.
    fn core_gone(&self, st: &mut State, how: &'static str) {
        if let Some(core) = st.core.take() {
            match how {
                "handover" => core.backlog.update(|b| *b = None),
                _ => drop(core.stream.shutdown(std::net::Shutdown::Both)),
            }
        }
        st.previous = how;
        st.core_left = Instant::now();
    }

    /// Writes one core's frames in order; a failed write means that core is gone.
    fn write_to_core(&self, generation: u64, mut stream: Stream, frames: Receiver<Vec<u8>>, backlog: Arc<Backlog>) {
        for frame in frames {
            if stream.write_all(&frame).is_err() {
                break;
            }
            backlog.update(|b| *b = b.map(|n| n.saturating_sub(frame.len())));
        }
        backlog.update(|b| *b = None);
        let mut st = self.lock();
        if st.core.as_ref().is_some_and(|c| c.generation == generation) {
            self.core_gone(&mut st, "crash");
        }
    }

    fn serve(self: &Arc<Self>, mut stream: Stream) -> io::Result<()> {
        match proto::read(&mut stream)? {
            Some((proto::HELLO, _, payload)) => proto::parse::<proto::Hello>(&payload)?,
            _ => return Ok(()),
        };
        let generation = {
            let mut st = self.lock();
            if st.core.is_some() {
                self.core_gone(&mut st, "crash");
            }
            st.generation += 1;
            let mut ids: Vec<u32> = st.terms.keys().copied().collect();
            ids.sort();
            let welcome = proto::Welcome {
                version: proto::VERSION,
                instance: self.instance.clone(),
                pid: std::process::id(),
                build: crate::client::build_id(),
                login_session: self.login_session,
                draining: self.draining.load(std::sync::atomic::Ordering::SeqCst),
                previous: st.previous.into(),
                terminals: ids
                    .iter()
                    .map(|id| {
                        let t = &st.terms[id];
                        HeldTerminal { id: *id, pid: t.pid, cols: t.cols, rows: t.rows, exit: t.exit }
                    })
                    .collect(),
            };
            let mut out = proto::json_frame(proto::WELCOME, 0, &welcome);
            for id in &ids {
                let (a, b) = st.terms[id].tail.as_slices();
                out.extend(proto::frame(proto::TAIL, *id, &[a, b].concat()));
            }
            let (frames, queued) = std::sync::mpsc::channel();
            let backlog = Arc::new(Backlog { bytes: Mutex::new(Some(0)), changed: Condvar::new() });
            let (writer, kept) = (stream.try_clone()?, stream.try_clone()?);
            let (holder, written, generation) = (self.clone(), backlog.clone(), st.generation);
            std::thread::spawn(move || holder.write_to_core(generation, writer, queued, written));
            st.core = Some(Core { generation, frames, backlog, stream: kept });
            self.to_core(&mut st, out);
            generation
        };

        let result = self.commands(&mut stream, generation);
        let mut st = self.lock();
        if st.core.as_ref().is_some_and(|c| c.generation == generation) {
            self.core_gone(&mut st, "crash");
        }
        result
    }

    fn commands(self: &Arc<Self>, stream: &mut Stream, generation: u64) -> io::Result<()> {
        while let Some((kind, id, payload)) = proto::read(stream)? {
            match kind {
                proto::SPAWN => self.spawn(id, proto::parse(&payload)?),
                proto::INPUT => {
                    if let Some(t) = self.lock().terms.get(&id) {
                        let _ = t.input.send(payload);
                    }
                }
                proto::RESIZE => {
                    let size: proto::Size = proto::parse(&payload)?;
                    let mut st = self.lock();
                    if let Some(t) = st.terms.get_mut(&id) {
                        t.cols = size.cols;
                        t.rows = size.rows;
                        t.pty.resize(size.cols, size.rows);
                    }
                }
                proto::SIGNAL => {
                    let sig: proto::Signal = proto::parse(&payload)?;
                    let st = self.lock();
                    // Never signal after reaping: the pid may belong to someone else now.
                    if let Some(t) = st.terms.get(&id).filter(|t| t.exit.is_none()) {
                        t.pty.signal(t.pid, sig.signal);
                    }
                }
                proto::FOREGROUND => {
                    let mut st = self.lock();
                    let pgid = st.terms.get(&id).filter(|t| t.exit.is_none()).map_or(-1, |t| t.pty.foreground());
                    self.to_core(&mut st, proto::json_frame(proto::FOREGROUND_IS, id, &proto::Foreground { pgid }));
                }
                proto::RELEASE => {
                    if let Some(t) = self.lock().terms.remove(&id)
                        && !t.reader_done
                    {
                        // Leftover background jobs still hold the PTY: hang them up.
                        t.pty.hang_up(t.pid);
                    }
                    self.exit_if_drained();
                }
                proto::DRAIN => {
                    self.draining.store(true, std::sync::atomic::Ordering::SeqCst);
                    self.exit_if_drained();
                }
                proto::GOODBYE => {
                    let mode: Goodbye = proto::parse(&payload)?;
                    let mut st = self.lock();
                    // Read late, after a newer core said hello: that one stays.
                    if st.core.as_ref().is_none_or(|c| c.generation != generation) {
                        return Ok(());
                    }
                    if mode == Goodbye::Handover {
                        // The core saved screens up to here; the next one replays only what follows.
                        st.terms.values_mut().for_each(|t| t.tail.clear());
                    }
                    if mode == Goodbye::Shutdown {
                        // A spawn may have completed after the core gave up waiting.
                        // End its program and foreground job, not detached descendants.
                        for t in st.terms.values().filter(|t| t.exit.is_none()) {
                            t.pty.signal(t.pid, crate::platform::signal::KILL);
                        }
                        self.to_core(&mut st, proto::frame(proto::BYE, 0, &[]));
                        let backlog = st.core.as_ref().map(|c| c.backlog.clone());
                        drop(st);
                        if let Some(backlog) = backlog {
                            backlog.wait(0, Duration::from_secs(5));
                        }
                        self.exit();
                    }
                    self.to_core(&mut st, proto::frame(proto::BYE, 0, &[]));
                    self.core_gone(&mut st, "handover");
                    return Ok(());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn spawn(self: &Arc<Self>, id: u32, spec: proto::Spawn) {
        let mut st = self.lock();
        if let Some(t) = st.terms.get(&id) {
            let frame = proto::json_frame(proto::SPAWNED, id, &proto::Spawned { pid: t.pid });
            return self.to_core(&mut st, frame);
        }
        if self.draining.load(std::sync::atomic::Ordering::SeqCst) {
            let frame = proto::json_frame(
                proto::SPAWN_FAILED,
                id,
                &proto::SpawnFailed { error: "holder is draining; start a terminal through the current core".into() },
            );
            return self.to_core(&mut st, frame);
        }
        let started = platform::spawn_pty(&spec.argv, &spec.env, &spec.cwd, spec.cols, spec.rows)
            .and_then(|(pty, child)| Ok((pty.reader()?, pty.writer()?, pty, child)));
        let frame = match started {
            Err(e) => {
                let error = format!("cannot start {:?}: {e}", spec.argv.first().map_or("", |s| s.as_str()));
                let _ = writeln!(io::stderr(), "holder: {}: {error}", crate::terminals::address(id));
                proto::json_frame(proto::SPAWN_FAILED, id, &proto::SpawnFailed { error })
            }
            Ok((reader, mut writer, pty, child)) => {
                let pid = child.id();
                let (input, queued) = std::sync::mpsc::channel::<Vec<u8>>();
                std::thread::spawn(move || {
                    for bytes in queued {
                        if writer.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                });
                st.terms.insert(
                    id,
                    Term {
                        pid,
                        input,
                        pty: Arc::new(pty),
                        cols: spec.cols,
                        rows: spec.rows,
                        tail: VecDeque::new(),
                        exit: None,
                        reader_done: false,
                        drains_since: Some(Instant::now()),
                    },
                );
                let (holder, waiter) = (self.clone(), self.clone());
                std::thread::spawn(move || holder.pump(id, reader));
                std::thread::spawn(move || waiter.reap(id, child));
                proto::json_frame(proto::SPAWNED, id, &proto::Spawned { pid })
            }
        };
        self.to_core(&mut st, frame);
    }

    /// Always reads, core or not, so programs never stall on a full PTY.
    fn pump(&self, id: u32, mut reader: impl platform::PtyOutput) {
        let mut buf = vec![0u8; FRAME_BYTES];
        let mut gather = Gather::default();
        loop {
            let n = match gather.read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // EIO: every slave side closed
            };
            let mut st = self.lock();
            let Some(t) = st.terms.get_mut(&id) else { break };
            t.tail.extend(&buf[..n]);
            let excess = t.tail.len().saturating_sub(TAIL_BYTES);
            t.tail.drain(..excess);
            self.to_core(&mut st, proto::frame(proto::OUTPUT, id, &buf[..n]));
            // A core that falls behind slows its programs, as a PTY nobody reads would.
            let Some(backlog) = st.core.as_ref().map(|c| c.backlog.clone()).filter(|b| b.over(BACKLOG_BYTES)) else {
                continue;
            };
            st.terms.get_mut(&id).expect("just found").drains_since = None;
            drop(st);
            backlog.wait(BACKLOG_BYTES, Duration::MAX);
            if let Some(t) = self.lock().terms.get_mut(&id) {
                t.drains_since = Some(Instant::now());
            }
            self.changed.notify_all();
        }
        if let Some(t) = self.lock().terms.get_mut(&id) {
            t.reader_done = true;
        }
        self.changed.notify_all();
    }

    /// The exit code comes from here, after the last output has been forwarded.
    fn reap(self: &Arc<Self>, id: u32, mut child: Process) {
        let (code, signal) = match platform::wait(&mut child) {
            Ok(ended) => ended,
            Err(e) => {
                eprintln!("holder: wait {}: {e}", crate::terminals::address(id));
                return;
            }
        };
        let exit = Exit { code, signal };
        let exited = Instant::now();
        let mut st = self.lock();
        while let Some(t) = st.terms.get(&id).filter(|t| !t.reader_done) {
            // Its output goes first: time a slow core holds the reader back doesn't count.
            let Some(since) = t.drains_since else {
                st = self.changed.wait(st).unwrap_or_else(|p| p.into_inner());
                continue;
            };
            let deadline = since.max(exited) + EXIT_DRAIN;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self.changed.wait_timeout(st, left).unwrap_or_else(|p| p.into_inner()).0;
        }
        if let Some(t) = st.terms.get_mut(&id) {
            t.exit = Some(exit);
            self.to_core(&mut st, proto::json_frame(proto::EXITED, id, &exit));
        }
    }

    /// Starts a core when the last one crashed and none came back in time.
    fn supervise(&self) {
        loop {
            std::thread::sleep(self.grace.min(Duration::from_millis(100)));
            let mut st = self.lock();
            if st.core.is_some() || st.core_left.elapsed() < self.grace {
                continue;
            }
            if st.terms.is_empty() {
                self.exit();
            }
            let now = Instant::now();
            st.restarts.retain(|t| now.duration_since(*t) < Duration::from_secs(300));
            let backoff = self.grace * 2u32.pow(st.restarts.len().min(6) as u32);
            if st.restarts.last().is_some_and(|t| now.duration_since(*t) < backoff.min(Duration::from_secs(60))) {
                continue;
            }
            st.restarts.push(now);
            st.core_left = now;
            drop(st);
            if !platform::holder_can_start_core(self.login_session) {
                let _ = writeln!(io::stderr(), "holder: original GUI login unavailable; waiting for a fresh launcher");
                continue;
            }
            if let Err(e) = crate::client::spawn_core(&self.paths) {
                let _ = writeln!(io::stderr(), "holder: cannot start core: {e:?}");
            }
        }
    }
}

#[cfg(all(test, unix))] // GATHERS
mod tests {
    use super::*;
    use std::io::Read;

    enum Item {
        Data(Vec<u8>),
        /// Nothing has arrived yet: read_now finds nothing, a blocking read waits past it.
        Pause,
        Eof,
        Fail,
    }

    /// Output the test lays out in advance.
    #[derive(Default)]
    struct Script {
        items: VecDeque<Item>,
        read_nows: usize,
    }

    impl Script {
        fn new(items: impl IntoIterator<Item = Item>) -> Self {
            Script { items: items.into_iter().collect(), read_nows: 0 }
        }

        fn take(&mut self, data: Vec<u8>, buf: &mut [u8]) -> usize {
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            if n < data.len() {
                self.items.push_front(Item::Data(data[n..].to_vec()));
            }
            n
        }
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                match self.items.pop_front() {
                    None => return Ok(0),
                    Some(Item::Pause) => continue,
                    Some(Item::Data(data)) => return Ok(self.take(data, buf)),
                    Some(Item::Eof) => {
                        self.items.push_front(Item::Eof);
                        return Ok(0);
                    }
                    Some(Item::Fail) => return Err(io::Error::other("EIO")),
                }
            }
        }
    }

    impl platform::PtyOutput for Script {
        fn read_now(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
            self.read_nows += 1;
            match self.items.pop_front() {
                None => Ok(None),
                Some(Item::Pause) => Ok(None),
                Some(Item::Data(data)) => Ok(Some(self.take(data, buf))),
                Some(end @ (Item::Eof | Item::Fail)) => {
                    let result = match end {
                        Item::Eof => Ok(Some(0)),
                        _ => Err(io::Error::other("EIO")),
                    };
                    self.items.push_front(end);
                    result
                }
            }
        }
    }

    fn in_burst() -> Gather {
        Gather { last: Some(Instant::now()), ..Gather::default() }
    }

    /// Every frame until the end, each checked against the cap. Each read is
    /// in a burst whatever the clock says: a busy test machine may pause the
    /// test between reads for longer than BURST_GAP.
    fn frames(gather: &mut Gather, script: &mut Script) -> Vec<Vec<u8>> {
        let mut buf = vec![0u8; FRAME_BYTES];
        let mut out = Vec::new();
        loop {
            gather.last = Some(Instant::now());
            let Ok(n @ 1..) = gather.read(script, &mut buf) else { break };
            assert!(n <= FRAME_BYTES);
            out.push(buf[..n].to_vec());
        }
        out
    }

    #[test]
    fn a_lone_write_goes_out_without_waiting() {
        let mut buf = vec![0u8; FRAME_BYTES];
        // The fastest of several: a busy test machine may pause any one of them.
        let mut fastest = Duration::MAX;
        for _ in 0..20 {
            let mut script = Script::new([Item::Data(b"x".to_vec()), Item::Data(b"more".to_vec())]);
            let mut gather = Gather::default();
            let started = Instant::now();
            assert_eq!(gather.read(&mut script, &mut buf).unwrap(), 1);
            fastest = fastest.min(started.elapsed());
            assert_eq!(script.read_nows, 0, "a first write takes nothing after it");

            // A write after a quiet spell is isolated too.
            let mut script = Script::new([Item::Data(b"y".to_vec()), Item::Data(b"more".to_vec())]);
            gather.last = Instant::now().checked_sub(BURST_GAP * 2);
            let started = Instant::now();
            assert_eq!(gather.read(&mut script, &mut buf).unwrap(), 1);
            fastest = fastest.min(started.elapsed());
            assert_eq!(script.read_nows, 0, "a write after a quiet spell takes nothing after it");
        }
        assert!(fastest < GATHER_WAIT / 10, "even the fastest took {fastest:?}");
    }

    #[test]
    fn a_burst_of_small_writes_goes_out_in_far_fewer_frames_unchanged() {
        let (mut sent, mut items) = (Vec::new(), Vec::new());
        for i in 0..50u8 {
            let rest: Vec<u8> = (0..200u8).map(|j| i ^ j).collect();
            sent.extend([i]);
            sent.extend(&rest);
            items.extend([Item::Data(vec![i]), Item::Data(rest), Item::Pause]);
        }
        let mut gather = in_burst();
        let started = Instant::now();
        let frames = frames(&mut gather, &mut Script::new(items));
        assert_eq!(frames.len(), 50, "one frame per gathered burst, not per write");
        assert_eq!(frames.concat(), sent, "same bytes, same order");
        assert!(started.elapsed() >= GATHER_WAIT * 50, "each burst waited for more");
        assert_eq!(gather.wait, GATHER_WAIT, "output never piled up, so the wait stays");
    }

    #[test]
    fn a_frame_never_exceeds_the_cap() {
        let chunk = |i: u8| vec![i; 1024];
        let mut items = vec![Item::Data(b"z".to_vec())];
        items.extend((0..100).map(|i| Item::Data(chunk(i))));
        let mut gather = in_burst();
        let frames = frames(&mut gather, &mut Script::new(items));
        assert_eq!(frames[0].len(), FRAME_BYTES, "a full queue fills the frame to the cap");
        let mut sent = b"z".to_vec();
        (0..100).for_each(|i| sent.extend(chunk(i)));
        assert_eq!(frames.concat(), sent);
        assert!(gather.wait < GATHER_WAIT, "output piled up during the wait, so it shrinks");
    }

    #[test]
    fn the_wait_shrinks_while_output_piles_up_and_grows_back() {
        let mut gather = Gather::default();
        let mut buf = vec![0u8; FRAME_BYTES];
        for _ in 0..10 {
            let mut script = Script::new([Item::Data(b"a".to_vec()), Item::Data(vec![b'b'; 600])]);
            gather.last = Some(Instant::now());
            gather.read(&mut script, &mut buf).unwrap();
        }
        assert_eq!(gather.wait, GATHER_WAIT_MIN);
        for _ in 0..10 {
            let mut script = Script::new([Item::Data(b"a".to_vec()), Item::Data(vec![b'b'; 10])]);
            gather.last = Some(Instant::now());
            gather.read(&mut script, &mut buf).unwrap();
        }
        assert_eq!(gather.wait, GATHER_WAIT);
    }

    #[test]
    fn a_small_write_right_after_a_big_read_goes_out_at_once() {
        // The prompt after a `cat`, the cursor move after a full redraw.
        let mut script =
            Script::new([Item::Data(vec![b'x'; 4096]), Item::Data(b"$ ".to_vec()), Item::Data(b"more".to_vec())]);
        let mut gather = in_burst();
        let mut buf = vec![0u8; FRAME_BYTES];
        assert_eq!(gather.read(&mut script, &mut buf).unwrap(), 4096);
        assert_eq!(gather.read(&mut script, &mut buf).unwrap(), 2);
        assert_eq!(script.read_nows, 0, "neither read waited for more");
    }

    #[test]
    fn the_end_of_output_still_sends_what_was_gathered() {
        for end in [Item::Eof, Item::Fail] {
            let fails = matches!(end, Item::Fail);
            // Enough arrives that the gathering reads on into the end.
            let mut script = Script::new([Item::Data(b"a".to_vec()), Item::Data(vec![b'b'; 600]), end]);
            let mut gather = in_burst();
            let mut buf = vec![0u8; FRAME_BYTES];
            assert_eq!(gather.read(&mut script, &mut buf).unwrap(), 601);
            assert_eq!(&buf[..601], [b"a".as_slice(), &[b'b'; 600]].concat());
            assert!(script.read_nows >= 2, "it reached the end");
            let next = gather.read(&mut script, &mut buf);
            assert!(if fails { next.is_err() } else { next.unwrap() == 0 }, "then the end itself");
        }
    }
}
