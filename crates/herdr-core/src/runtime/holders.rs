//! Holders: starting one, adopting what they run, reading their frames, relinking when a link drops.

use super::*;

impl Core {
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
    pub(super) fn attach_all(self: &Arc<Self>) -> io::Result<()> {
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
    pub(super) fn take_over(
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
        let current = welcome.build == build;
        if require_current && !current {
            io::Write::write_all(&mut stream, &proto::frame(proto::DRAIN, 0, &[]))?;
            return Err(io::Error::other(format!("holder build {} differs from core build {build}", welcome.build)));
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
            // Another build: it keeps what it runs, takes nothing new, exits when empty.
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
}
