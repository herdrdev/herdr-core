//! Viewers: raw attach streams on framed connections. First paint, live
//! bytes (minus answered queries), input, and who sets the size.

use crate::holder_proto as proto;
use crate::relay::Relay;
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use crate::wire::Conn;
use ghostty_vt::Rgb;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

#[cfg(test)]
mod tests;

/// What a viewer's own terminal looks like, reported by the client.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default, Debug, PartialEq)]
pub struct Display {
    /// Default foreground as `#rrggbb`.
    pub fg: Option<String>,
    /// Default background as `#rrggbb`.
    pub bg: Option<String>,
    /// Cursor color as `#rrggbb`.
    pub cursor: Option<String>,
    /// Cell size in pixels: `[width, height]`.
    pub cell: Option<[u32; 2]>,
}

pub struct Viewer {
    pub conn: Arc<Conn>,
    pub channel: u32,
    pub observe: bool,
    resize_notifications: bool,
    repaint_pending: bool,
    size: (u16, u16),
    /// None: not active since it came back after a dropped line.
    active_at: Option<Instant>,
    /// Whether the person's input is in this viewer: its window has the OS
    /// focus and this is the pane typed in. None: it never said, so it counts
    /// as the person's own viewer (the CLI, an agent, a first attach).
    focused: Option<bool>,
    /// The person has typed in this viewer since it attached (or since the
    /// line dropped and came back). A viewer that only arrived keeps its
    /// provisional leadership until it says it is in the background; a viewer
    /// the person has typed in keeps leadership through unfocus, so moving
    /// the OS focus between two windows does not move the program's size.
    spoke: bool,
    display: Display,
    /// An unfinished sequence exceeded continuation tracking; skip its tail.
    pub(crate) recovering: bool,
}

impl Display {
    /// Answer the program's color and cell-size queries with these facts.
    pub(crate) fn apply(&self, screen: &mut ghostty_vt::Terminal) {
        screen.set_display(rgb(&self.fg), rgb(&self.bg), rgb(&self.cursor), self.cell.map(|[w, h]| (w, h)));
    }
}

fn rgb(hex: &Option<String>) -> Option<Rgb> {
    let hex = hex.as_deref()?.strip_prefix('#')?;
    let n = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6)?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

pub struct AttachRequest {
    pub observe: bool,
    pub resize_notifications: bool,
    pub history: usize,
    pub size: (u16, u16),
    pub display: Display,
    pub resumed: bool,
}

/// The viewer whose size and colors count: the most recently active one. A
/// viewer that has not claimed activity since it reconnected ties with the
/// rest, and the one watching longest keeps the program, so a window that
/// arrives later cannot take over by being last in line.
fn leader(viewers: &[Viewer]) -> Option<&Viewer> {
    viewers.iter().filter(|v| !v.observe).rev().max_by_key(|v| v.active_at)
}

impl Core {
    /// Adds a viewer: queues the first paint, then every live byte follows in order.
    pub fn attach(&self, conn: &Arc<Conn>, id: u32, req: AttachRequest) -> Result<u32, RpcError> {
        let mut st = self.lock();
        if let Some(t) = st.reg.terminals.get(&id) {
            t.check_program()?;
        }
        if !st.screens.contains_key(&id) {
            return Err(RpcError::new(
                "finished",
                format!("{} has finished", crate::terminals::address(id)),
                format!("read what it left: hc read {}", crate::terminals::address(id)),
            ));
        }
        let channel = conn.new_channel();
        let recovering = st.screens[&id].continuation().is_none();
        st.viewers.entry(id).or_default().push(Viewer {
            conn: conn.clone(),
            channel,
            observe: req.observe,
            resize_notifications: req.observe && req.resize_notifications,
            repaint_pending: false,
            size: req.size,
            active_at: (!req.resumed).then(Instant::now),
            focused: None,
            spoke: false,
            display: req.display,
            recovering,
        });
        self.apply_size(&mut st, id);
        let paint = first_paint(&st, id, req.history);
        st.viewers.get_mut(&id).unwrap().last_mut().unwrap().repaint_pending = false;
        conn.with(|out| {
            out.open_paused(channel);
            if req.observe && req.resize_notifications {
                let t = &st.reg.terminals[&id];
                let size = [t.cols.to_le_bytes(), t.rows.to_le_bytes()].concat();
                out.resize(channel, &size);
            }
            out.data(channel, &paint);
        });
        Ok(channel)
    }

    /// Program output: through the relay to the screen and every viewer,
    /// then whatever status or title it reported.
    pub fn output(&self, st: &mut State, id: u32, bytes: &[u8]) {
        st.last_output.insert(id, Instant::now());
        *st.output_seq.entry(id).or_default() += 1;
        if let Some(t) = st.reg.terminals.get_mut(&id) {
            t.printed += bytes.iter().filter(|b| **b == b'\n').count() as u64;
        }
        // While a read scrolls the app, its frames are ours alone: viewers get one repaint after.
        let scrolling = st.stitching.contains(&id);
        let instance = st.reg.terminals.get(&id).and_then(|t| t.holder.clone());
        let State { screens, relays, viewers, holders, output_watch, .. } = st;
        let holder = holders.iter().find(|l| Some(&l.instance) == instance.as_ref());
        let Some(screen) = screens.get_mut(&id) else { return };
        let relay = relays.entry(id).or_default();
        let recovering = viewers.get(&id).into_iter().flatten().any(|v| v.recovering);
        let (out, recovered) =
            if recovering { relay.feed_to_ground(screen, bytes) } else { (relay.feed(screen, bytes), None) };
        let win32_input = relay.win32_input.take();
        if let Some(watchers) = output_watch.get_mut(&id).filter(|_| !scrolling) {
            watchers.retain(|w| w.send(out.clone()).is_ok());
        }
        if let Some(link) = holder {
            link.input(id, &Relay::take_responses(screen));
        }
        let reports = screen.take_status_reports();
        let title = screen.take_title_changed().then(|| screen.title());
        let full_screen = screen.alt_screen();
        // Before any repaint below, so it carries the mode this output just set.
        if let Some(on) = win32_input
            && let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.win32_input != on)
        {
            t.win32_input = on;
            self.save(st);
        }
        let recovered = recovered.map(|(at, paint)| {
            let mut paint = paint_state(st, id, paint);
            paint.extend_from_slice(&out[at..]);
            paint
        });
        let mut behind = Vec::new();
        for (index, v) in st.viewers.get_mut(&id).into_iter().flatten().enumerate().filter(|_| !scrolling) {
            if v.repaint_pending {
                continue;
            }
            let bytes = if v.recovering {
                let Some(paint) = &recovered else { continue };
                v.recovering = false;
                paint
            } else {
                &out
            };
            v.conn.with(|ob| {
                if !ob.data(v.channel, bytes) {
                    behind.push(index);
                }
            });
        }
        if !behind.is_empty() {
            let paint = first_paint(st, id, 0);
            let recovering = st.screens[&id].continuation().is_none();
            let viewers = st.viewers.get_mut(&id).expect("viewers with queued output");
            for index in behind {
                let v = &mut viewers[index];
                v.recovering = recovering;
                v.conn.with(|ob| {
                    ob.data(v.channel, &paint);
                });
            }
        }
        if full_screen && let Some(t) = st.reg.terminals.get_mut(&id) {
            t.full_screen = true;
        }
        if !reports.is_empty() {
            self.report_status(st, id, &reports);
        }
        if let Some(title) = title {
            st.reg.terminals.get_mut(&id).expect("live terminal").title = title.clone();
            self.save_soon();
            self.emit(st, "title", id, serde_json::json!({"title": title}));
        }
    }

    /// Applies 7501 reports and tells subscribers what changed.
    pub fn report_status(&self, st: &mut State, id: u32, reports: &[ghostty_vt::StatusReport]) {
        let word = |st: &State| st.reg.terminals[&id].status.as_ref().map(|s| s.state.clone());
        let mut word_changed = false;
        for r in reports {
            let was = word(st);
            let t = st.reg.terminals.get_mut(&id).expect("live terminal");
            let change = crate::status::apply(t, r);
            self.status_changed(st, id, change);
            word_changed |= word(st) != was;
        }
        // Losing the holder too (a reboot) leaves no tail to replay: queue a new word,
        // and with it a turn or news, for immediate persistence. Messages and progress can wait.
        if word_changed { self.save(st) } else { self.save_soon() }
    }

    pub fn status_changed(&self, st: &mut State, id: u32, change: crate::status::Change) {
        let t = st.reg.terminals.get_mut(&id).expect("reported terminal");
        let first_task_finished = t.first_task_turn.is_some_and(|turn| {
            t.turn > turn
                || (change.root_report
                    && t.status.as_ref().is_some_and(|s| matches!(s.state.as_str(), "done" | "blocked" | "error")))
        });
        if first_task_finished {
            t.first_task_turn = None;
        }
        let (status, records, turn, reported) = (t.status.clone(), t.records.clone(), t.turn, t.reported);
        if change.status {
            self.emit(
                st,
                "status",
                id,
                serde_json::json!({"status": status, "records": records, "reported": reported}),
            );
        }
        if change.turn_ended {
            let state = status.map(|s| s.state);
            self.emit(st, "turn", id, serde_json::json!({"turn": turn, "state": state}));
        }
        if (change.turn_ended || change.stuck || first_task_finished) && st.reg.terminals[&id].is_live() {
            self.news(st, id);
        }
    }

    fn viewer<'a>(st: &'a mut State, conn: &Arc<Conn>, channel: u32) -> Option<(u32, &'a mut Viewer)> {
        st.viewers.iter_mut().find_map(|(id, vs)| {
            vs.iter_mut().find(|v| Arc::ptr_eq(&v.conn, conn) && v.channel == channel).map(|v| (*id, v))
        })
    }

    /// Typing, and the size a viewer may ask for, makes it the one whose
    /// size and colors count; see `viewer_resize` and `viewer_focus` for who
    /// may ask, and when focus alone must not.
    fn touch(&self, st: &mut State, conn: &Arc<Conn>, channel: u32, f: impl FnOnce(&mut Viewer)) -> Option<u32> {
        let (id, v) = Self::viewer(st, conn, channel)?;
        f(v);
        if v.observe {
            return None;
        }
        v.active_at = Some(Instant::now());
        self.apply_size(st, id);
        Some(id)
    }

    pub fn viewer_input(&self, conn: &Arc<Conn>, channel: u32, bytes: &[u8], automatic: bool) {
        let mut st = self.lock();
        let id = if automatic {
            Self::viewer(&mut st, conn, channel).filter(|(_, v)| !v.observe).map(|(id, _)| id)
        } else {
            self.touch(&mut st, conn, channel, |v| v.spoke = true)
        };
        if let Some(id) = id {
            let now = st.gates.entry(id).or_default().viewer(bytes, !automatic);
            if let Some(link) = st.link(id) {
                link.input(id, &now);
            }
        }
    }

    /// The viewer's window was laid out, so record its grid. Only a viewer
    /// that has said it is the one the person is in takes the program's
    /// size from the last one. A viewer that never reports focus — an idle
    /// second window, a watcher — keeps its grid to itself, so a background
    /// window's layout, tab switches, and window-manager animations never
    /// re-wrap the program being watched; a viewer that is first in line
    /// still sizes the program through its attach, and `hc attach` in a
    /// bare terminal is the only viewer and needs no introduction.
    pub fn viewer_resize(&self, conn: &Arc<Conn>, channel: u32, cols: u16, rows: u16) {
        let mut st = self.lock();
        let Some((id, _)) = Self::viewer(&mut st, conn, channel) else { return };
        let (_, v) = Self::viewer(&mut st, conn, channel).expect("the viewer is there");
        v.size = (cols, rows);
        if v.observe {
            return;
        }
        if v.focused == Some(true) {
            v.active_at = Some(Instant::now());
        }
        self.apply_size(&mut st, id);
    }

    pub fn viewer_facts(&self, conn: &Arc<Conn>, channel: u32, display: Display) {
        let mut st = self.lock();
        let Some((id, v)) = Self::viewer(&mut st, conn, channel) else { return };
        v.display.fg = display.fg.or(v.display.fg.take());
        v.display.bg = display.bg.or(v.display.bg.take());
        v.display.cursor = display.cursor.or(v.display.cursor.take());
        v.display.cell = display.cell.or(v.display.cell);
        let display = leader(&st.viewers[&id])
            .filter(|v| Arc::ptr_eq(&v.conn, conn) && v.channel == channel)
            .map(|v| v.display.clone());
        if let Some(display) = display {
            self.apply_display(&mut st, id, Some(&display));
        }
    }

    /// Focus reaches programs that asked for it, and tells the program who
    /// the person is in. Gaining focus does not claim the program's size:
    /// two windows on one machine showing the same program rebuilt it at
    /// every alt-tab while the size ping-ponged between their grids. Typing
    /// is what claims it. A viewer that only arrived — laid out, never typed
    /// in — hands its provisional leadership back when it reports the
    /// background, so a window opening in the background cannot re-wrap the
    /// program; a viewer the person typed in keeps leadership through
    /// unfocus, so leaving a window for one you type in moves the size
    /// exactly once, at the keystroke.
    pub fn viewer_focus(&self, conn: &Arc<Conn>, channel: u32, focused: bool) {
        let mut st = self.lock();
        let Some((id, _)) = Self::viewer(&mut st, conn, channel) else { return };
        let leads = leader(&st.viewers[&id]).is_some_and(|v| Arc::ptr_eq(&v.conn, conn) && v.channel == channel);
        let left = st.viewers[&id].iter().filter(|v| !v.observe).count() > 1;
        let (_, v) = Self::viewer(&mut st, conn, channel).expect("the viewer is there");
        if v.observe {
            return;
        }
        v.focused = Some(focused);
        if !focused && !v.spoke && leads && left {
            v.active_at = None;
        }
        self.apply_size(&mut st, id);
        if st.screens.get(&id).is_some_and(|s| s.dec_mode(1004))
            && let Some(link) = st.link(id)
        {
            link.input(id, if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// A viewer left (`channel` = None: the whole connection went away).
    pub fn detach(&self, conn: &Arc<Conn>, channel: Option<u32>) {
        let mut st = self.lock();
        self.unsubscribe(&mut st, conn, channel);
        let mut touched = Vec::new();
        for (id, vs) in st.viewers.iter_mut() {
            let before = vs.len();
            vs.retain(|v| !(Arc::ptr_eq(&v.conn, conn) && channel.is_none_or(|c| c == v.channel)));
            if vs.len() != before {
                touched.push(*id);
            }
        }
        st.viewers.retain(|_, vs| !vs.is_empty());
        for id in touched {
            self.apply_size(&mut st, id);
        }
    }

    /// The most recently active typing viewer sets size and display facts;
    /// with none, the terminal runs headless.
    fn apply_size(&self, st: &mut State, id: u32) {
        let leader = st.viewers.get(&id).and_then(|vs| leader(vs));
        let (cols, rows) = leader.map_or_else(|| self.headless(), |v| v.size);
        let display = leader.map(|v| v.display.clone());
        if !st.screens.contains_key(&id) {
            return;
        }
        self.apply_display(st, id, display.as_ref());
        let rec = st.reg.terminals.get_mut(&id).expect("live terminal");
        if (rec.cols, rec.rows) == (cols, rows) || cols == 0 || rows == 0 {
            return;
        }
        st.screens.get_mut(&id).expect("live screen").resize(cols, rows);
        rec.cols = cols;
        rec.rows = rows;
        if let Some(link) = st.link(id) {
            link.send(&proto::json_frame(proto::RESIZE, id, &proto::Size { cols, rows }));
        }
        let size = [cols.to_le_bytes(), rows.to_le_bytes()].concat();
        for v in st.viewers.get_mut(&id).into_iter().flatten().filter(|v| v.resize_notifications) {
            v.repaint_pending = true;
            v.conn.with(|out| out.resize(v.channel, &size));
        }
        self.save_soon();
        self.emit(st, "size", id, serde_json::json!({"cols": cols, "rows": rows}));
    }

    /// Serialize only the latest grid, when a viewer has credit to receive it.
    pub fn prepare_repaints(&self, conn: &Arc<Conn>) {
        let mut st = self.lock();
        let ids: Vec<u32> = st.viewers.keys().copied().collect();
        for id in ids {
            self.queue_resize_paints(&mut st, id, Some(conn));
        }
    }

    /// On exit there will be no later writer pass over this screen.
    pub(crate) fn queue_resize_paints(&self, st: &mut State, id: u32, conn: Option<&Arc<Conn>>) {
        let ready = |v: &Viewer| {
            v.repaint_pending && conn.is_none_or(|conn| Arc::ptr_eq(&v.conn, conn) && conn.outbox().can_send(v.channel))
        };
        if !st.viewers.get(&id).into_iter().flatten().any(ready) {
            return;
        }
        let paint = first_paint(st, id, 0);
        let recovering = st.screens[&id].continuation().is_none();
        for v in st.viewers.get_mut(&id).into_iter().flatten().filter(|v| ready(v)) {
            v.conn.with(|out| {
                out.data(v.channel, &paint);
            });
            v.repaint_pending = false;
            v.recovering = recovering;
        }
    }

    fn apply_display(&self, st: &mut State, id: u32, display: Option<&Display>) {
        let Some(screen) = st.screens.get_mut(&id) else { return };
        if let Some(d) = display {
            d.apply(screen);
        }
        // Display changes can notify an idle program, even without a resize or output.
        let replies = Relay::take_responses(screen);
        if let Some(link) = st.link(id) {
            link.input(id, &replies);
        }
    }
}

/// The screen, then the latest 7501 report of every record and whether the
/// program wants win32 key events (both pass through to viewers, so a new one
/// must see them too; a viewer repainted after falling behind gets the mode
/// either way).
pub(crate) fn first_paint(st: &State, id: u32, history: usize) -> Vec<u8> {
    #[cfg(test)]
    tests::PAINTS.with(|n| n.set(n.get() + 1));
    let screen = &st.screens[&id];
    let mut paint = paint_state(st, id, screen.paint(history));
    if let Some(continuation) = screen.continuation() {
        paint.extend(continuation);
    }
    paint
}

fn paint_state(st: &State, id: u32, screen: Vec<u8>) -> Vec<u8> {
    // A backlog may have ended in any parser state, not at an escape boundary.
    let mut paint = vec![0x18];
    paint.extend(screen);
    let t = &st.reg.terminals[&id];
    paint.extend(if t.win32_input { b"\x1b[?9001h" } else { b"\x1b[?9001l" });
    let root = t.status.iter().map(|s| ("", s));
    for (rid, status) in root.chain(t.records.iter().map(|(k, s)| (k.as_str(), s))) {
        paint.extend(status.encode(rid).as_bytes());
    }
    paint
}
