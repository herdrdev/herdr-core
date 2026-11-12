//! Holder startup and terminal adoption after a core restart.

use super::*;

impl Core {
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
    pub(super) fn adopt(
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
            let replies = st.reg.replies.remove(&id).unwrap_or_default();
            if handover {
                let mut held = st.gates.entry(id).or_default().set_busy(false);
                held.extend(replies);
                if let Some(link) = st.link(id) {
                    link.input(id, &held);
                }
            }
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
    pub(super) fn nudge(&self, terminals: Vec<(u32, u16, u16)>) {
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
}
