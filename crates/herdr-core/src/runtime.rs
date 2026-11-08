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

mod holders;
mod persistence;
use persistence::RegistryWriter;

pub struct Core {
    pub info: CoreInfo,
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
