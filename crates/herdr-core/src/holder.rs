//! The holder: owns every PTY, is every program's parent, always reads output.
//! Spawn, pump, resize, signal, reap. No parsing, no policy. It keeps a ~1 MB
//! tail per terminal so a core that crashed can rebuild screens, and restarts
//! a crashed core itself.

use crate::holder_proto::{self as proto, Exit, Goodbye, HeldTerminal};
use crate::paths::Paths;
use crate::platform::{self, Process, Pty, Stream};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const TAIL_BYTES: usize = 1 << 20;
const EXIT_DRAIN: Duration = Duration::from_millis(200);

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
    /// Its `HC_SCOPE` token: what it started carries it too.
    scope: Option<String>,
}

struct State {
    terms: HashMap<u32, Term>,
    core: Option<(u64, Stream)>,
    generation: u64,
    previous: &'static str,
    core_left: Instant,
    restarts: Vec<Instant>,
}

struct Holder {
    state: Mutex<State>,
    changed: Condvar,
    instance: String,
    grace: Duration,
    paths: Paths,
    socket: std::path::PathBuf,
    /// A newer build took over: exit once the last terminal is gone.
    draining: std::sync::atomic::AtomicBool,
    /// Sweeps still ending what a finished program left behind.
    sweeps: std::sync::atomic::AtomicUsize,
}

/// Runs a holder on `socket` (each holder has its own; the core picks the path).
pub fn run(socket: std::path::PathBuf) -> io::Result<()> {
    let paths = Paths::from_env().map_err(|e| io::Error::other(e.message))?;
    paths.create_dirs()?;
    // A taken path means another holder lives there; a stale file is removed by the core.
    let listener = platform::listen(&socket)?;
    let grace = std::env::var("HC_HOLDER_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(3), Duration::from_millis);
    let holder = Arc::new(Holder {
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
        sweeps: Default::default(),
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
        // Never leave with leftovers still being ended: nobody else would.
        self.wait_for_sweeps();
        std::process::exit(0);
    }

    fn wait_for_sweeps(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.sweeps.load(std::sync::atomic::Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Ends everything carrying `scope`, in the background.
    fn sweep(self: &Arc<Self>, scope: Option<String>) {
        let Some(scope) = scope else { return };
        self.sweeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let holder = self.clone();
        std::thread::spawn(move || {
            crate::sweep::sweep(&[scope]);
            holder.sweeps.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }

    fn exit_if_drained(&self) {
        if self.draining.load(std::sync::atomic::Ordering::SeqCst) && self.lock().terms.is_empty() {
            self.exit();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Sends to the current core; a failed write means it is gone.
    fn to_core(&self, st: &mut State, frame: &[u8]) {
        if let Some((_, stream)) = &mut st.core
            && stream.write_all(frame).is_err()
        {
            self.core_gone(st, "crash");
        }
    }

    fn core_gone(&self, st: &mut State, how: &'static str) {
        if let Some((_, stream)) = st.core.take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        st.previous = how;
        st.core_left = Instant::now();
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
            stream.write_all(&out)?;
            st.core = Some((st.generation, stream.try_clone()?));
            st.generation
        };

        let result = self.commands(&mut stream);
        let mut st = self.lock();
        if st.core.as_ref().is_some_and(|(g, _)| *g == generation) {
            self.core_gone(&mut st, "crash");
        }
        result
    }

    fn commands(self: &Arc<Self>, stream: &mut Stream) -> io::Result<()> {
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
                    self.to_core(&mut st, &proto::json_frame(proto::FOREGROUND_IS, id, &proto::Foreground { pgid }));
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
                    if mode == Goodbye::Handover {
                        // The core saved screens up to here; the next one replays only what follows.
                        st.terms.values_mut().for_each(|t| t.tail.clear());
                    }
                    if mode == Goodbye::Shutdown {
                        // Whatever the core's stop missed (started since, or hiding) ends
                        // here, before the goodbye: the stop returns when all is gone.
                        let scopes: Vec<Option<String>> = st.terms.values().map(|t| t.scope.clone()).collect();
                        drop(st);
                        scopes.into_iter().for_each(|scope| self.sweep(scope));
                        self.wait_for_sweeps();
                        let _ = stream.write_all(&proto::frame(proto::BYE, 0, &[]));
                        self.exit();
                    }
                    let _ = stream.write_all(&proto::frame(proto::BYE, 0, &[]));
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
            return self.to_core(&mut st, &frame);
        }
        let started = platform::spawn_pty(&spec.argv, &spec.env, &spec.cwd, spec.cols, spec.rows)
            .and_then(|(pty, child)| Ok((pty.reader()?, pty.writer()?, pty, child)));
        let frame = match started {
            Err(e) => {
                let error = format!("cannot start {:?}: {e}", spec.argv.first().map_or("", |s| s.as_str()));
                eprintln!("holder: {}: {error}", crate::terminals::address(id));
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
                        scope: spec.env.get(crate::sweep::VAR).cloned(),
                    },
                );
                let (holder, waiter) = (self.clone(), self.clone());
                let scope = spec.env.get(crate::sweep::VAR).cloned();
                std::thread::spawn(move || holder.pump(id, reader));
                std::thread::spawn(move || waiter.reap(id, child, scope));
                proto::json_frame(proto::SPAWNED, id, &proto::Spawned { pid })
            }
        };
        self.to_core(&mut st, &frame);
    }

    /// Always reads, core or not, so programs never stall on a full PTY.
    fn pump(&self, id: u32, mut reader: impl Read) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
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
            self.to_core(&mut st, &proto::frame(proto::OUTPUT, id, &buf[..n]));
        }
        if let Some(t) = self.lock().terms.get_mut(&id) {
            t.reader_done = true;
        }
        self.changed.notify_all();
    }

    /// The exit code comes from here, after the last output has been forwarded.
    /// `scope` is its own copy: the terminal may be released (forgotten) first.
    fn reap(self: &Arc<Self>, id: u32, mut child: Process, scope: Option<String>) {
        let (code, signal) = match platform::wait(&mut child) {
            Ok(ended) => ended,
            Err(e) => {
                eprintln!("holder: wait {}: {e}", crate::terminals::address(id));
                return;
            }
        };
        let exit = Exit { code, signal };
        // The terminal lived exactly as long as its program: what it left behind
        // goes too, before the exit is reported. If we die first, the core still
        // sees it live and ends what carries its scope itself.
        if let Some(scope) = scope {
            self.sweeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::sweep::sweep(&[scope]);
            self.sweeps.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        let deadline = Instant::now() + EXIT_DRAIN;
        let mut st = self.lock();
        while st.terms.get(&id).is_some_and(|t| !t.reader_done) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self.changed.wait_timeout(st, left).unwrap_or_else(|p| p.into_inner()).0;
        }
        if let Some(t) = st.terms.get_mut(&id) {
            t.exit = Some(exit);
            self.to_core(&mut st, &proto::json_frame(proto::EXITED, id, &exit));
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
            if let Err(e) = crate::client::spawn_core(&self.paths) {
                eprintln!("holder: cannot start core: {e:?}");
            }
        }
    }
}
