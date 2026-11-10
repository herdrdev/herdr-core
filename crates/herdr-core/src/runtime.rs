//! The core's live side: its link to the holder, one parsed screen per live
//! terminal, adoption after a restart, and what happens when a program ends.

use crate::api::CoreInfo;
use crate::holder_proto::{self as proto, Exit, Goodbye};
use crate::paths::Paths;
use crate::paths::{read_state, write_atomic};
use crate::platform::{self, Stream as UnixStream, signal};
use crate::relay::Relay;
use crate::terminals::Registry;
use crate::viewers::Viewer;
use ghostty_vt::Terminal as Screen;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Notify, oneshot};

mod persistence;
use persistence::RegistryWriter;

pub struct Core {
    pub info: CoreInfo,
    pub login_session: Option<platform::LoginSession>,
    pub started: Instant,
    pub stop: Notify,
    /// Set by `server.restart`: on stop, hand over instead of ending programs.
    pub restart: std::sync::atomic::AtomicBool,
    pub paths: Paths,
    pub state: Mutex<State>,
    /// Woken on every lifecycle change; waiters re-check what they need.
    pub changed: Notify,
    /// Work for the runtime that the holder thread can't do itself.
    pub jobs: tokio::sync::mpsc::UnboundedSender<crate::notes::Job>,
    pub jobs_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<crate::notes::Job>>>,
    /// Sweeps of lost terminals in flight: stop and restart wait for them.
    pub(crate) sweeps: Arc<std::sync::atomic::AtomicUsize>,
    /// Holder startup and goodbye cannot cross, or a stop could leave a new holder behind.
    holder_start: Mutex<()>,
    /// The registry changed in a way a crash may lose: `save_soon` writes it shortly.
    dirty: std::sync::atomic::AtomicBool,
    save_due: Notify,
    registry_writer: RegistryWriter,
}

#[derive(Default)]
pub struct State {
    pub reg: Registry,
    pub docs: std::collections::BTreeMap<String, crate::api::DocGetResult>,
    pub screens: HashMap<u32, Screen>,
    pub relays: HashMap<u32, Relay>,
    pub viewers: HashMap<u32, Vec<Viewer>>,
    pub events: crate::events::Events,
    pub gates: HashMap<u32, crate::input::Gate>,
    pub queues: HashMap<u32, Arc<tokio::sync::Mutex<()>>>,
    pub sends: HashMap<u32, Arc<tokio::sync::Mutex<()>>>,
    pub foreground: HashMap<u32, Vec<oneshot::Sender<i32>>>,
    /// Waits matching output: they get every byte viewers get.
    pub output_watch: HashMap<u32, Vec<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    /// The turn of each caller's last send to a terminal, until a bare wait reports it.
    pub last_send: HashMap<(String, u32), u64>,
    /// Waits in progress by (caller, terminal): a parent waiting needs no note.
    pub waiting: HashMap<(String, u32), usize>,
    /// A message was typed into this agent while it worked (so it sits in its
    /// queue) and no wait of its has returned since: who sent it.
    pub messages: HashMap<u32, Option<String>>,
    /// Parents with a note delivery running.
    pub note_tasks: HashSet<u32>,
    /// When each terminal last printed anything, and how many times.
    pub last_output: HashMap<u32, Instant>,
    pub output_seq: HashMap<u32, u64>,
    /// Terminals a read is scrolling right now (viewers paused).
    pub stitching: HashSet<u32>,
    /// The last stitched read per terminal, good while it prints nothing:
    /// (output count after it, what it captured).
    pub stitched: HashMap<u32, (u64, crate::stitch::Capture)>,
    /// Every holder we talk to: one of our build takes new terminals, older ones drain.
    pub holders: Vec<Link>,
    pub pending: HashMap<u32, oneshot::Sender<Result<crate::terminals::Terminal, String>>>,
    pub killing: HashSet<u32>,
    pub stopping: bool,
    pub machines: Vec<crate::machines::Machine>,
    /// The line to each machine, by its core id, and the task keeping it up.
    pub lines: HashMap<String, crate::remote::LineState>,
    pub line_tasks: HashMap<String, tokio::task::AbortHandle>,
    /// Viewers here on terminals there, by (connection, channel here).
    pub relayed: HashMap<(usize, u32), crate::remote::Relayed>,
    /// Cores that dialed in, by id (kept), and the line to each while connected.
    pub peers: std::collections::BTreeMap<String, crate::remote::Peer>,
    pub peer_lines: HashMap<String, (usize, Arc<crate::remote::Line>)>,
    /// Notes from other machines for parents here, until typed.
    pub afar_notes: HashMap<u32, Vec<String>>,
    /// What a machine must hear once its line is back (a parent here finished).
    pub machine_outbox: HashMap<String, Vec<serde_json::Value>>,
    /// When each holder was last reconnected to after our reading of it failed.
    pub relinked: HashMap<String, Instant>,
    /// Failed relinks still own programs; explicit stop must tell them to exit.
    pub disconnected: HashMap<String, PathBuf>,
    /// Each connected machine's terminals as seen from here, kept current by
    /// its events: machine id -> address (`rohan/t7`) -> record.
    pub mirrors: HashMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
}

pub struct Link {
    /// Frames for the holder, written by their own thread: the core never
    /// blocks on the socket while holding its lock (the holder writes to us
    /// under its own lock; both waiting would freeze the session).
    writer: mpsc::Sender<Vec<u8>>,
    pub pid: u32,
    pub instance: String,
    /// Same build as this core: new terminals go here.
    pub current: bool,
    bye: mpsc::Sender<()>,
    bye_rx: Option<mpsc::Receiver<()>>,
}

impl State {
    /// The holder that runs terminal `id`.
    pub fn link(&self, id: u32) -> Option<&Link> {
        let instance = self.reg.terminals.get(&id)?.holder.as_deref()?;
        self.holders.iter().find(|l| l.instance == instance)
    }

    /// The holder new terminals go to.
    pub fn current(&self) -> Option<&Link> {
        self.holders.iter().find(|l| l.current)
    }
}

impl Link {
    pub fn send(&self, frame: &[u8]) {
        let _ = self.writer.send(frame.to_vec());
    }

    pub fn input(&self, id: u32, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.send(&proto::frame(proto::INPUT, id, bytes));
        }
    }
}

const SAVE_SOON: Duration = Duration::from_secs(2);
/// At most one reconnect per holder in this long: a failure that comes back is not retried.
const RELINK_GAP: Duration = Duration::from_secs(5);
/// How long a reconnect waits for the holder's welcome.
const RELINK_WAIT: Duration = Duration::from_millis(if cfg!(test) { 200 } else { 5000 });

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn finish_reason(killed: bool, stopping: bool, host_shutdown: bool) -> &'static str {
    if killed {
        "killed"
    } else if stopping {
        "server_stop"
    } else if host_shutdown {
        "host_shutdown"
    } else {
        "exited"
    }
}

impl Core {
    pub fn start(paths: Paths, info: CoreInfo) -> io::Result<Arc<Core>> {
        let reg: Registry = read_state(&paths.state.join("terminals.json"))?;
        let docs_path = paths.state.join("docs.json");
        let docs = read_state(&docs_path).unwrap_or_else(|e| {
            eprintln!("core: cannot load documents, starting empty: {e}");
            if let Err(e) = std::fs::rename(&docs_path, paths.state.join("docs.json.bad")) {
                eprintln!("core: cannot quarantine documents: {e}");
            }
            Default::default()
        });
        std::fs::create_dir_all(paths.state.join("screens"))?;
        let machines = read_state(&paths.state.join("machines.json"))?;
        let peers = read_state(&paths.state.join("peers.json"))?;
        let (jobs, jobs_rx) = tokio::sync::mpsc::unbounded_channel();
        let core = Arc::new(Core {
            info,
            login_session: platform::login_session(),
            started: Instant::now(),
            stop: Notify::new(),
            restart: Default::default(),
            registry_writer: RegistryWriter::new(paths.state.join("terminals.json"))?,
            paths,
            state: Mutex::new(State { reg, docs, machines, peers, ..Default::default() }),
            changed: Notify::new(),
            jobs,
            jobs_rx: Mutex::new(Some(jobs_rx)),
            sweeps: Default::default(),
            holder_start: Default::default(),
            dirty: Default::default(),
            save_due: Notify::new(),
        });
        core.attach_all()?;
        Ok(core)
    }

    /// Settings from config.toml, read fresh (a broken file means defaults here;
    /// `run` reports it).
    pub fn settings(&self) -> crate::config::Config {
        crate::config::load(&self.paths.config).unwrap_or_default()
    }

    pub fn headless(&self) -> (u16, u16) {
        self.settings().headless().unwrap_or((200, 120))
    }

    pub fn history_bytes(&self) -> usize {
        self.settings().history_bytes()
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn snapshot_path(&self, id: u32) -> PathBuf {
        self.paths.state.join("screens").join(format!("{}.snap", crate::terminals::address(id)))
    }

    pub fn save(&self, st: &State) {
        self.dirty.store(false, std::sync::atomic::Ordering::SeqCst);
        let bytes = serde_json::to_vec(&st.reg).expect("registry serializes");
        self.registry_writer.save(bytes);
    }

    /// Acknowledged mutations wait here without holding the state lock or blocking the executor.
    /// Attach, events.subscribe snapshots, forwarded run `afar`, and NotesAfar replies do not wait.
    /// Completed write failures are logged, as before; a stuck disk still times out.
    pub async fn flush_saved(&self) -> Result<(), crate::rpc::RpcError> {
        tokio::time::timeout(Duration::from_secs(10), self.registry_writer.flush())
            .await
            .map_err(|_| {
                crate::rpc::RpcError::new(
                    "unavailable",
                    "registry persistence timed out; the operation may already have completed",
                    "check the terminal and disk before retrying",
                )
            })?
            .map_err(|e| crate::rpc::RpcError::internal(format!("cannot save terminals: {e}")))
    }

    /// For frequent changes a crash may lose (the next start rebuilds them):
    /// saved within `SAVE_SOON`, once for a burst. Call with the lock held.
    pub fn save_soon(&self) {
        if !self.dirty.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.save_due.notify_one();
        }
    }

    pub async fn save_when_due(self: Arc<Self>) {
        loop {
            self.save_due.notified().await;
            tokio::time::sleep(SAVE_SOON).await;
            let st = self.lock();
            if self.dirty.load(std::sync::atomic::Ordering::SeqCst) {
                self.save(&st);
            }
        }
    }

    /// A holder of our build for new terminals, started if there is none.
    pub async fn ensure_holder(self: &Arc<Self>) -> io::Result<()> {
        let core = self.clone();
        tokio::task::spawn_blocking(move || core.start_holder()).await.map_err(io::Error::other)?
    }

    fn start_holder(self: &Arc<Self>) -> io::Result<()> {
        let _starting = self.holder_start.lock().unwrap_or_else(|p| p.into_inner());
        if self.lock().stopping || self.restart.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::other("the core is stopping or restarting"));
        }
        if self.lock().current().is_some() {
            return Ok(());
        }
        // 8 hex: unique enough among one session's holders, and socket paths are short (macOS: 104 bytes).
        let socket =
            format!("{}{}.sock", self.holder_prefix(), &crate::platform::random_hex()[..crate::paths::HOLDER_ID]);
        crate::client::spawn_detached(&self.paths, &["hold", "--socket", &socket], "holder.log")?;
        let stream = crate::client::wait_for_socket(socket.as_ref(), "holder").map_err(io::Error::other)?;
        self.take_over(stream, true, socket.as_ref(), None).map(drop)
    }

    /// Holder sockets are `<session>.holder.<id>.sock`, next to `<session>.holder.sock`
    /// (older holders: a 32-hex id).
    fn holder_prefix(&self) -> String {
        let base = self.paths.holder_socket.display().to_string();
        format!("{}.", base.strip_suffix(".sock").unwrap_or(&base))
    }

    /// At startup: adopt from every holder still running. Live records no holder
    /// runs any more ended with it (a reboot, a killed holder).
    fn attach_all(self: &Arc<Self>) -> io::Result<()> {
        let prefix = self.holder_prefix();
        let dir = self.paths.holder_socket.parent().expect("socket has a dir");
        let mut sockets: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let p = p.display().to_string();
                p == self.paths.holder_socket.display().to_string() || (p.starts_with(&prefix) && p.ends_with(".sock"))
            })
            .collect();
        sockets.sort();
        let mut adopted = HashSet::new();
        for socket in sockets {
            match platform::connect(&socket) {
                Ok(stream) => adopted.extend(self.take_over(stream, false, &socket, None)?),
                Err(_) => {
                    platform::remove_endpoint(&socket); // its holder is gone
                }
            }
        }
        let mut st = self.lock();
        // Kept records already waiting for a shell retain why they stopped.
        // A normal stop must not turn into crash cleanup on the next start.
        // A replacement still claimed by a holder (holder set, no pid yet) is
        // not waiting: if that holder is gone, it was lost like any other.
        let lost: Vec<u32> = st
            .reg
            .terminals
            .values()
            .filter(|t| t.is_live() && !(t.program_lost() && t.holder.is_none()) && !adopted.contains(&t.id()))
            .map(|t| t.id())
            .collect();
        for id in lost {
            self.record_finish(&mut st, id, Exit { code: None, signal: None }, "stopped_by_restart");
        }
        // Whatever lost holders left running, even if a crash cut an earlier
        // cleanup short: sweeping again is harmless.
        let leftovers = st
            .reg
            .terminals
            .values()
            .filter(|t| matches!(t.reason.as_deref(), Some("holder_lost" | "stopped_by_restart")));
        self.sweep_in_background(leftovers.filter_map(|t| t.scope.clone()).collect());
        crate::exec::sweep_leftovers(&self.paths.state);
        self.prune(&mut st);
        self.save(&st);
        Ok(())
    }

    /// Takes over one holder and adopts what it runs; returns the adopted ids.
    /// `answered` is dropped once the holder's welcome and tails are in.
    fn take_over(
        self: &Arc<Self>,
        mut stream: UnixStream,
        require_current: bool,
        path: &std::path::Path,
        answered: Option<mpsc::Sender<()>>,
    ) -> io::Result<Vec<u32>> {
        std::io::Write::write_all(
            &mut stream,
            &proto::json_frame(proto::HELLO, 0, &proto::Hello { version: proto::VERSION }),
        )?;
        let welcome: proto::Welcome = match proto::read(&mut stream)? {
            Some((proto::WELCOME, _, payload)) => proto::parse(&payload)?,
            _ => return Err(io::Error::other("holder sent no welcome")),
        };
        let mut tails = HashMap::new();
        for _ in &welcome.terminals {
            match proto::read(&mut stream)? {
                Some((proto::TAIL, id, bytes)) => tails.insert(id, bytes),
                _ => return Err(io::Error::other("holder sent no tail")),
            };
        }
        drop(answered);
        let (bye, bye_rx) = mpsc::channel();
        let build = crate::client::build_id();
        let current = platform::holder_current(
            &welcome.build,
            &build,
            welcome.draining,
            self.login_session,
            welcome.login_session,
        );
        if welcome.build == build && self.login_session.is_some() && welcome.login_session.is_none() {
            eprintln!("core: holder {} login identity unavailable; retaining build eligibility", welcome.pid);
        }
        if require_current && !current {
            io::Write::write_all(&mut stream, &proto::frame(proto::DRAIN, 0, &[]))?;
            return Err(io::Error::other(format!(
                "holder build {} / login {:?} is not current for core build {build} / login {:?} (draining={})",
                welcome.build, welcome.login_session, self.login_session, welcome.draining
            )));
        }
        let (writer, frames) = mpsc::channel::<Vec<u8>>();
        let mut socket = stream.try_clone()?;
        std::thread::spawn(move || {
            for frame in frames {
                if io::Write::write_all(&mut socket, &frame).is_err() {
                    break;
                }
            }
        });
        let link =
            Link { writer, pid: welcome.pid, instance: welcome.instance.clone(), current, bye, bye_rx: Some(bye_rx) };
        if !current {
            // Another build/login or already draining: retain its programs, take no new work.
            link.send(&proto::frame(proto::DRAIN, 0, &[]));
        }
        let (adopted, nudge) = {
            let mut st = self.lock();
            st.disconnected.remove(&welcome.instance);
            st.holders.push(link);
            let adopted = self.adopt(&mut st, &welcome, tails);
            self.save(&st);
            adopted
        };
        let core = self.clone();
        let (instance, path) = (welcome.instance, path.to_path_buf());
        std::thread::spawn(move || core.read_holder(stream, instance, path));
        if !nudge.is_empty() {
            let core = self.clone();
            std::thread::spawn(move || core.nudge(nudge));
        }
        Ok(adopted)
    }

    /// Matches saved records with what one holder runs. Returns the adopted ids,
    /// and the terminals whose screens were rebuilt from the tail and need
    /// their program to redraw.
    fn adopt(
        &self,
        st: &mut State,
        welcome: &proto::Welcome,
        mut tails: HashMap<u32, Vec<u8>>,
    ) -> (Vec<u32>, Vec<(u32, u16, u16)>) {
        let instance = welcome.instance.as_str();
        let handover = welcome.previous == "handover";
        // Records from before holders were tracked per terminal name one holder for all.
        let legacy = st.reg.holder.clone();
        let ours =
            |t: &crate::terminals::Terminal| t.is_live() && t.holder.as_deref().or(legacy.as_deref()) == Some(instance);
        let mut nudge = Vec::new();
        let mut exited = Vec::new();
        let mut adopted = Vec::new();
        for h in &welcome.terminals {
            let id = h.id;
            if !st.reg.terminals.get(&id).is_some_and(ours) {
                // Spawned for a core that never recorded it: nobody can address it.
                let link = st.holders.iter().find(|l| l.instance == instance).expect("attached");
                link.send(&proto::json_frame(proto::SIGNAL, id, &proto::Signal { signal: signal::KILL }));
                link.send(&proto::frame(proto::RELEASE, id, &[]));
                continue;
            }
            adopted.push(id);
            if let Some(saved) = st.reg.drafts.remove(&id)
                && handover
            {
                st.gates.insert(id, crate::input::Gate::restore(saved));
            }
            st.reg.terminals.get_mut(&id).expect("ours").holder = Some(instance.to_string());
            let tail = tails.remove(&id).unwrap_or_default();
            let saved = handover
                .then(|| std::fs::read(self.snapshot_path(id)).ok())
                .flatten()
                .and_then(|bytes| Screen::restore(&bytes, self.history_bytes()));
            let exact = saved.is_some();
            let mut screen = saved.unwrap_or_else(|| Screen::new(h.cols, h.rows, self.history_bytes()));
            // Through a relay, like live output, so requests the screen doesn't keep are seen.
            let mut relay = Relay::default();
            relay.feed(&mut screen, &tail);
            relay.flush(&mut screen);
            // After a handover the tail is new output nobody answered yet; after a
            // crash it was answered long ago and replies would be typed as garbage.
            let replies = Relay::take_responses(&mut screen);
            if exact && let Some(link) = st.link(id) {
                link.input(id, &replies);
            }
            let reports = screen.take_status_reports();
            let rec = st.reg.terminals.get_mut(&id).expect("live id");
            screen.set_xtversion(&rec.identity);
            rec.title = screen.title();
            if let Some(on) = relay.win32_input {
                rec.win32_input = on;
            }
            st.screens.insert(id, screen);
            st.relays.remove(&id); // half a sequence from the old link must not prefix the new
            rec.pid = Some(h.pid);
            rec.cols = h.cols;
            rec.rows = h.rows;
            rec.screen = if exact { "exact" } else { "inexact" }.into();
            let spawning = rec.reason.is_some() || st.pending.contains_key(&id);
            if exact {
                // Every handover report is fresh, including endpoints followed by idle.
                for r in &reports {
                    let rec = st.reg.terminals.get_mut(&id).expect("live id");
                    let change = crate::status::apply(rec, r);
                    self.status_changed(st, id, change);
                }
            } else {
                // A crash tail repeats reports we already counted: take its final word,
                // but count at most the one turn that may have ended while we were down.
                let (turn, was, since) = (rec.turn, rec.status.as_ref().map(|s| s.state.clone()), rec.status_ms);
                for r in &reports {
                    crate::status::apply(rec, r);
                }
                let now = rec.status.as_ref().map(|s| s.state.clone());
                rec.turn = turn + (was.as_deref() == Some("working") && now.as_deref() != Some("working")) as u64;
                if now == was {
                    rec.status_ms = since;
                }
                // Only a changed final word/turn proves new activity in a crash replay.
                // A repeated done, or done -> idle, entirely during downtime can remain unobservable.
                let root_report = rec.turn != turn || now != was;
                self.status_changed(st, id, crate::status::Change { root_report, ..Default::default() });
            }
            if spawning {
                self.spawn_result(st, instance, id, Ok(h.pid));
            }
            match h.exit {
                Some(exit) => exited.push((id, exit)),
                None if !exact => nudge.push((id, h.cols, h.rows)),
                None => {}
            }
        }
        for (id, exit) in exited {
            self.finish(st, id, exit);
        }
        (adopted, nudge)
    }

    /// Resize down and back so full-screen programs repaint.
    fn nudge(&self, terminals: Vec<(u32, u16, u16)>) {
        let send = |id, cols, rows| {
            if let Some(link) = self.lock().link(id) {
                link.send(&proto::json_frame(proto::RESIZE, id, &proto::Size { cols, rows }));
            }
        };
        for &(id, cols, rows) in &terminals {
            send(id, cols, rows.saturating_sub(1).max(1));
        }
        std::thread::sleep(Duration::from_millis(50));
        for &(id, cols, rows) in &terminals {
            send(id, cols, rows);
        }
    }

    fn read_holder(self: Arc<Self>, mut stream: UnixStream, instance: String, socket: PathBuf) {
        let ended =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.holder_frames(&mut stream, &instance)));
        if matches!(ended, Ok(Ok(true))) {
            return;
        }
        // Never leave the link open with nobody reading: the holder would wait on us.
        let _ = stream.shutdown(std::net::Shutdown::Both);
        // Only our reading failed (a panic, a bad frame): a holder still there takes us
        // back. Not while stopping: the goodbye holds `holder_start` until this link ends.
        let stopping = || self.lock().stopping || self.restart.load(std::sync::atomic::Ordering::SeqCst);
        let mut relink = !matches!(ended, Ok(Ok(false))) && !stopping();
        let _starting = relink.then(|| self.holder_start.lock().unwrap_or_else(|p| p.into_inner()));
        relink &= !stopping(); // a stop that began meanwhile
        let mut st = self.lock();
        let Some(at) = st.holders.iter().position(|l| l.instance == instance) else { return };
        // Kept to the end: dropping it wakes a goodbye, which must find the sweep below counted.
        let link = st.holders.remove(at);
        let current = link.current;
        if relink {
            drop(st);
            // Fully closed first (its writer holds a copy): an older holder stuck
            // writing to us only lets go when our end is gone, not just shut down.
            drop(link);
            drop(stream);
            if let Ok(stream) = platform::connect(&socket) {
                return self.relink(stream, &instance, &socket);
            }
            st = self.lock();
        }
        // The holder is gone (drained, or died), and with it every program it ran.
        let theirs: Vec<u32> = st
            .reg
            .terminals
            .values()
            .filter(|t| t.is_live() && t.holder.as_deref() == Some(instance.as_str()))
            .map(|t| t.id())
            .collect();
        // Its programs may have outlived it (closing the PTY is only a hangup).
        self.sweep_in_background(theirs.iter().filter_map(|id| st.reg.terminals[id].scope.clone()).collect());
        for id in theirs {
            self.record_finish(&mut st, id, Exit { code: None, signal: None }, "holder_lost");
        }
        if current {
            for (_, tx) in st.pending.drain() {
                let _ = tx.send(Err("the holder exited".into()));
            }
        }
        self.save(&st);
        self.changed.notify_waiters();
    }

    /// Takes over a holder that still answers after our reading of it failed. If
    /// that fails too (a panic replaying its tails), or keeps happening, it is left
    /// alone: its programs run on, for the next core start to adopt.
    fn relink(self: &Arc<Self>, stream: UnixStream, instance: &str, socket: &std::path::Path) {
        let last = self.lock().relinked.insert(instance.into(), Instant::now());
        let ours = stream.try_clone();
        // A holder that doesn't answer must not keep `holder_start` (runs, stops) waiting:
        // past the deadline its link is cut, which ends the wait (no read timeouts on Windows).
        // Only the handshake is timed: what follows it is ours, however slow.
        let (answered, waiting) = mpsc::channel::<()>();
        if let Ok(watch) = stream.try_clone() {
            std::thread::spawn(move || {
                if waiting.recv_timeout(RELINK_WAIT) == Err(mpsc::RecvTimeoutError::Timeout) {
                    let _ = watch.shutdown(std::net::Shutdown::Both);
                }
            });
        }
        let taken = last.is_none_or(|at| at.elapsed() >= RELINK_GAP)
            && matches!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.take_over(
                    stream,
                    false,
                    socket,
                    Some(answered)
                ))),
                Ok(Ok(_))
            );
        if taken {
            eprintln!("core: lost the link to holder {instance}, reconnected");
            return;
        }
        eprintln!("core: lost the link to holder {instance} for good; its terminals wait for the next core start");
        // Closed, so the holder sees us leave instead of waiting on a link nobody reads.
        if let Ok(ours) = ours {
            let _ = ours.shutdown(std::net::Shutdown::Both);
        }
        let mut st = self.lock();
        st.holders.retain(|l| l.instance != instance);
        st.disconnected.insert(instance.into(), socket.to_path_buf());
    }

    /// Handles the holder's frames until it says goodbye (true) or the link ends (false).
    fn holder_frames(&self, stream: &mut UnixStream, instance: &str) -> io::Result<bool> {
        while let Some((kind, id, payload)) = proto::read(stream)? {
            let mut st = self.lock();
            match kind {
                proto::OUTPUT => self.output(&mut st, id, &payload),
                proto::SPAWNED | proto::SPAWN_FAILED => {
                    let result = if kind == proto::SPAWNED {
                        proto::parse::<proto::Spawned>(&payload).map(|s| s.pid).map_err(|e| e.to_string())
                    } else {
                        Err(proto::parse::<proto::SpawnFailed>(&payload).map_or_else(|e| e.to_string(), |f| f.error))
                    };
                    self.spawn_result(&mut st, instance, id, result);
                }
                proto::EXITED => {
                    if let Ok(exit) = proto::parse::<Exit>(&payload) {
                        self.finish(&mut st, id, exit);
                    }
                }
                proto::FOREGROUND_IS => {
                    let pgid = proto::parse::<proto::Foreground>(&payload).map_or(-1, |f| f.pgid);
                    for tx in st.foreground.remove(&id).unwrap_or_default() {
                        let _ = tx.send(pgid);
                    }
                }
                proto::BYE => {
                    // It said goodbye: it takes no more work from us.
                    if let Some(at) = st.holders.iter().position(|l| l.instance == instance) {
                        let _ = st.holders.remove(at).bye.send(());
                    }
                    return Ok(true);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn spawn_result(&self, st: &mut State, instance: &str, id: u32, result: Result<u32, String>) {
        // Recorded here, on the thread that also handles exits, so `created`
        // always comes before `finished`.
        let waiting = st.pending.get(&id).is_some_and(|tx| !tx.is_closed());
        let result = match (result, st.reg.terminals.get_mut(&id)) {
            (Ok(pid), Some(t)) if t.is_live() => {
                let restarted = t.reason.is_some();
                t.pid = Some(pid);
                t.program_started_ms = Some(now_ms());
                t.reason = None;
                t.exit_code = None;
                t.signal = None;
                let created = t.clone();
                let public = serde_json::to_value(crate::terminals::Public(created.clone())).unwrap_or_default();
                self.emit(st, if restarted { "restarted" } else { "created" }, id, public);
                self.save(st);
                Ok(created)
            }
            (Ok(_), _) => {
                // Its run gave up waiting: nobody can address it, so it must not live.
                if let Some(link) = st.holders.iter().find(|l| l.instance == instance) {
                    link.send(&proto::json_frame(proto::SIGNAL, id, &proto::Signal { signal: signal::KILL }));
                    link.send(&proto::frame(proto::RELEASE, id, &[]));
                }
                Err("removed while starting".into())
            }
            (Err(e), Some(t)) if t.is_live() && (t.reason.is_some() || !waiting) => {
                self.spawn_failed(st, id);
                Err(e)
            }
            (Err(e), _) => Err(e),
        };
        if let Some(tx) = st.pending.remove(&id) {
            let _ = tx.send(result);
        }
    }

    /// A program ended: record it, keep its final screen on disk, let the holder forget it.
    pub fn finish(&self, st: &mut State, id: u32, exit: Exit) {
        let reason = finish_reason(st.killing.contains(&id), st.stopping, platform::host_shutdown_in_progress());
        if let Some(link) = st.link(id) {
            link.send(&proto::frame(proto::RELEASE, id, &[]));
        }
        self.record_finish(st, id, exit, reason);
        self.save(st);
        self.changed.notify_waiters();
    }

    pub(crate) fn record_finish(&self, st: &mut State, id: u32, exit: Exit, reason: &str) {
        if !st.reg.terminals.get(&id).is_some_and(|t| t.is_live()) {
            return;
        }
        let reason = if st.killing.remove(&id) {
            "killed"
        } else if reason == "exited" && st.reg.terminals[&id].keep && exit.code.is_none() && exit.signal.is_some() {
            "signaled"
        } else {
            reason
        };
        if let Some(t) = st.reg.terminals.get_mut(&id) {
            t.first_task_turn = None;
            let change = crate::status::reset(t);
            self.status_changed(st, id, change);
        }
        st.reg.finish(id, exit.code, exit.signal, reason, now_ms());
        let finished_ms = st.reg.terminals.get(&id).and_then(|t| t.finished_ms);
        let end = serde_json::json!({"reason": reason, "exit_code": exit.code, "signal": exit.signal, "finished_ms": finished_ms});
        let pending = st.reg.terminals[&id].is_live();
        self.emit(st, if pending { "lost" } else { "finished" }, id, end.clone());
        if pending {
            let t = st.reg.terminals.get_mut(&id).expect("lost");
            t.news += 1;
            t.told_parent = false;
        } else {
            self.news(st, id);
            let _ = self.jobs.send(crate::notes::Job::Orphaned(id));
            crate::remote::orphaned_afar(self, st, id); // before `--rm` forgets where it reached
        }
        if let (Some(relay), Some(screen)) = (st.relays.get_mut(&id), st.screens.get_mut(&id)) {
            relay.flush(screen);
        }
        self.queue_resize_paints(st, id, None);
        for v in st.viewers.remove(&id).unwrap_or_default() {
            v.conn.with(|out| out.finish(v.channel, end.to_string().as_bytes()));
        }
        st.relays.remove(&id);
        if let Some(snapshot) = st.screens.remove(&id).and_then(|s| s.snapshot())
            && let Err(e) = write_atomic(&self.snapshot_path(id), &snapshot)
        {
            eprintln!("core: cannot save screen of {}: {e}", crate::terminals::address(id));
        }
        // `--rm`: gone now, unless its parent is waiting for the exit code.
        let rm = st.reg.terminals.get(&id).is_some_and(|t| {
            !t.is_live() && t.rm && !t.parent.as_ref().is_some_and(|p| st.waiting.contains_key(&(p.clone(), id)))
        });
        if rm {
            self.remove(st, id);
        }
        self.prune(st);
    }

    /// Forgets finished terminals past their retention. Runs when one finishes,
    /// at startup and on every list, so a quiet core keeps nothing too long.
    pub fn prune(&self, st: &mut State) {
        for gone in st.reg.expired(now_ms()) {
            self.remove(st, gone);
        }
    }

    /// The screen text of a live or finished terminal.
    pub fn screen_text(&self, st: &State, id: u32) -> Option<String> {
        match st.screens.get(&id) {
            Some(screen) => Some(screen.plain_text()),
            None => std::fs::read(self.snapshot_path(id))
                .ok()
                .and_then(|bytes| Screen::restore(&bytes, self.history_bytes()))
                .map(|s| s.plain_text()),
        }
    }

    pub fn remove(&self, st: &mut State, id: u32) {
        // Gone before it is announced: a fresh snapshot for a subscriber that
        // fell behind must not bring it back.
        let Some(was) = st.reg.terminals.remove(&id) else { return };
        self.emit_from(st, "removed", id, serde_json::json!({}), Some(&was));
        let _ = std::fs::remove_file(self.snapshot_path(id));
        let _ = std::fs::remove_dir_all(crate::put::dir(&self.paths.state, id));
        self.save(st);
    }

    pub fn signal(&self, st: &State, id: u32, signal: i32) {
        if let Some(link) = st.link(id) {
            link.send(&proto::json_frame(proto::SIGNAL, id, &proto::Signal { signal }));
        }
    }

    /// Waits until `done` holds or the timeout passes; returns whether it held.
    pub async fn wait_for(&self, timeout: Duration, done: impl Fn(&State) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if done(&self.lock()) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return done(&self.lock());
            }
        }
    }

    /// Hang up, then escalate. Returns once the program ended or KILL timed out.
    pub async fn kill(&self, ids: &[u32]) {
        let all_done = |st: &State| {
            ids.iter().all(|id| !st.reg.terminals.get(id).is_some_and(|t| t.is_live() && t.holder.is_some()))
        };
        {
            let mut st = self.lock();
            for &id in ids {
                if st
                    .reg
                    .terminals
                    .get(&id)
                    .is_some_and(|t| t.is_live() && (t.holder.is_none() || (t.reason.is_some() && t.pid.is_none())))
                {
                    let reason = if st.killing.contains(&id) { "killed" } else { "server_stop" };
                    if let Some(link) = st.link(id) {
                        link.send(&proto::json_frame(proto::SIGNAL, id, &proto::Signal { signal: signal::KILL }));
                        link.send(&proto::frame(proto::RELEASE, id, &[]));
                    }
                    self.record_finish(&mut st, id, Exit { code: None, signal: None }, reason);
                }
            }
            self.save(&st);
        }
        for (signal, wait) in [(signal::HUP, 500), (signal::TERM, 500), (signal::KILL, 2000)] {
            {
                let st = self.lock();
                for &id in ids {
                    self.signal(&st, id, signal);
                }
            }
            if self.wait_for(Duration::from_millis(wait), all_done).await {
                return;
            }
        }
    }

    /// Ends every link on purpose and waits until each holder confirms.
    fn goodbye(&self, mode: Goodbye) {
        let _starting = self.holder_start.lock().unwrap_or_else(|p| p.into_inner());
        let byes: Vec<mpsc::Receiver<()>> = {
            let mut st = self.lock();
            st.holders
                .iter_mut()
                .filter_map(|link| {
                    link.send(&proto::json_frame(proto::GOODBYE, 0, &mode));
                    link.bye_rx.take()
                })
                .collect()
        };
        if mode == Goodbye::Shutdown {
            let sockets: Vec<_> = self.lock().disconnected.drain().map(|(_, path)| path).collect();
            for path in sockets {
                if let Ok(mut stream) = platform::connect(&path) {
                    let Ok(watch) = stream.try_clone() else { continue };
                    let (done, waiting) = mpsc::channel::<()>();
                    std::thread::spawn(move || {
                        if waiting.recv_timeout(Duration::from_secs(6)) == Err(mpsc::RecvTimeoutError::Timeout) {
                            let _ = watch.shutdown(std::net::Shutdown::Both);
                        }
                    });
                    // The welcome may be unreadable (the reason relink failed).
                    // Shutdown needs no inventory: the holder ends its own programs.
                    let hello = proto::json_frame(proto::HELLO, 0, &proto::Hello { version: proto::VERSION });
                    if io::Write::write_all(&mut stream, &hello).is_ok()
                        && io::Write::write_all(&mut stream, &proto::json_frame(proto::GOODBYE, 0, &mode)).is_ok()
                    {
                        let _ = io::copy(&mut stream, &mut io::sink());
                    }
                    drop(done);
                }
            }
        }
        for rx in byes {
            // A holder told to shut down ends any unacknowledged programs first.
            let _ = rx.recv_timeout(Duration::from_secs(6));
        }
    }

    /// Crash-only cleanup after losing a holder, never a normal terminal end.
    /// Stop and restart wait for it. Tokens avoid signaling reused session IDs.
    fn sweep_in_background(&self, scopes: Vec<String>) {
        if scopes.is_empty() {
            return;
        }
        self.sweeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let sweeps = self.sweeps.clone();
        std::thread::spawn(move || {
            crate::sweep::sweep(&scopes);
            sweeps.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }

    fn wait_for_sweeps(&self) {
        let deadline = Instant::now() + Duration::from_secs(6);
        while self.sweeps.load(std::sync::atomic::Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Planned restart: the holder keeps programs; we save every screen exactly.
    pub async fn handover(&self) -> io::Result<()> {
        self.wait_for_sweeps();
        self.goodbye(Goodbye::Handover);
        self.wait_for_sweeps(); // a holder that died during goodbye left one
        {
            let mut st = self.lock();
            st.holders.clear();
            st.reg.drafts = st
                .gates
                .iter()
                .filter(|(id, _)| st.reg.terminals.get(*id).is_some_and(|t| t.is_live()))
                .filter_map(|(id, gate)| Some((*id, gate.snapshot()?)))
                .collect();
            let State { screens, relays, .. } = &mut *st;
            for (id, screen) in screens.iter_mut() {
                if let Some(relay) = relays.get_mut(id) {
                    relay.flush(screen);
                }
                if let Some(snapshot) = screen.snapshot() {
                    let _ = write_atomic(&self.snapshot_path(*id), &snapshot);
                }
            }
            self.save(&st);
        }
        self.registry_writer.flush().await
    }

    /// Server stop: end every program, then let the holder exit.
    pub async fn shutdown(&self) -> Result<(), crate::rpc::RpcError> {
        let live: Vec<u32> = {
            let mut st = self.lock();
            st.stopping = true;
            st.reg.terminals.values().filter(|t| t.is_live()).map(|t| t.id()).collect()
        };
        self.changed.notify_waiters(); // execs end
        self.kill(&live).await;
        crate::exec::wait_all().await;
        self.wait_for_sweeps();
        self.goodbye(Goodbye::Shutdown);
        self.wait_for_sweeps(); // a holder that died during goodbye left one
        {
            let mut st = self.lock();
            for id in live {
                self.record_finish(&mut st, id, Exit { code: None, signal: None }, "server_stop");
            }
            self.save(&st);
        }
        self.flush_saved().await
    }
}

#[cfg(test)]
mod tests;
