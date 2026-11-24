//! Viewer streams relayed to other cores, including reattachment.

use super::*;
use std::time::Instant;

/// A viewer here on a terminal there. It outlives a dropped line: when the
/// line is back, it is attached again on the same channel here.
#[derive(Clone)]
pub struct Relayed {
    pub automatic_input: bool,
    pub line: Arc<Line>,
    pub there: u32,
    pub conn: Arc<Conn>,
    pub machine: String,
    pub terminal: String,
    pub params: Value,
    pub active_at: Instant,
    pub size_pending: bool,
}

/// Why an opted-in stream was refused: a hop on the way lost resize support.
pub(super) const RESIZE_LOST: &str = "resize notifications lost";

/// Viewers left waiting by a dropped line to `m` get their terminal back.
pub(super) async fn reattach(core: Arc<Core>, m: Machine, line: Arc<Line>) {
    let mut waiting: Vec<_> = core
        .lock()
        .relayed
        .iter()
        .filter(|(_, r)| r.machine == m.id && !Arc::ptr_eq(&r.line, &line))
        .map(|(k, r)| (*k, r.active_at))
        .collect();
    // Resumed viewers tie there; the last attached wins unless another viewer stayed active.
    waiting.sort_by_key(|(_, active_at)| *active_at);
    for (key @ (_, here), was_active_at) in waiting {
        let Some(r) = core.lock().relayed.get(&key).cloned() else { continue };
        // The screen is painted again; the viewer already has its history.
        let mut params = r.params.clone();
        params["history"] = 0.into();
        params["resumed"] = true.into();
        negotiate_resize(&line, &mut params);
        let deadline = deadline_for(&core, "terminal.attach", &params);
        let sink = ((r.conn.clone(), here), r.params["resize_notifications"] == true);
        let reply = line.call_into(&m.name, "terminal.attach", params, Some(sink), deadline).await;
        let lost_resize = matches!(&reply, Err(e) if e.message == RESIZE_LOST);
        let mut st = core.lock();
        // Gone meanwhile (the viewer detached): nothing to resume.
        let Some(current) = st.relayed.get(&key).filter(|v| Arc::ptr_eq(&v.line, &r.line)) else {
            if let Ok(reply) = reply {
                close_remote_viewer(&line, &reply);
            }
            continue;
        };
        let reply = reply.ok();
        if r.automatic_input && reply.as_ref().is_some_and(|reply| !automatic_input(&line, reply)) {
            st.relayed.remove(&key);
            close_remote_viewer(&line, reply.as_ref().unwrap());
            r.conn.with(|out| out.close(here, b""));
            continue;
        }
        match reply.as_ref().and_then(|reply| reply["channel"].as_u64()) {
            Some(there) => {
                let there = there as u32;
                let terminal =
                    reply.as_ref().and_then(|r| r["terminal"]["address"].as_str()).unwrap_or(&current.terminal);
                let missed = current.active_at != was_active_at;
                let newer = missed
                    && st.relayed.values().any(|v| {
                        Arc::ptr_eq(&v.line, &line)
                            && v.terminal == terminal
                            && v.params["observe"] != true
                            && v.active_at > current.active_at
                    });
                let terminal = terminal.to_owned();
                let current = st.relayed.get_mut(&key).expect("still attached");
                current.line = line.clone();
                current.there = there;
                current.terminal = terminal;
                current.size_pending =
                    current.params["cols"] != r.params["cols"] || current.params["rows"] != r.params["rows"];
                // A resize also claims size control. Replay only missed activity,
                // and leave a newer viewer on the same terminal in control.
                if !newer && (missed || current.size_pending) {
                    resend_size(current);
                    current.size_pending = false;
                }
                if current.params["display"] != r.params["display"] {
                    let _ = line.frames.send(wire::encode(
                        there,
                        wire::FACTS,
                        current.params["display"].to_string().as_bytes(),
                    ));
                }
            }
            None => {
                st.relayed.remove(&key);
                // A downgraded path must not deliver paints sized for the old
                // contract; any other failure still hands over what arrived.
                if lost_resize {
                    r.conn.with(|out| out.close(here, b""));
                } else {
                    r.conn.with(|out| out.finish(here, b""));
                }
            }
        }
    }
}

fn resend_size(viewer: &Relayed) {
    let cols = viewer.params["cols"].as_u64().unwrap_or_default() as u16;
    let rows = viewer.params["rows"].as_u64().unwrap_or_default() as u16;
    let size = [cols.to_le_bytes(), rows.to_le_bytes()].concat();
    let _ = viewer.line.frames.send(wire::encode(viewer.there, wire::RESIZE, &size));
}

fn close_remote_viewer(line: &Line, reply: &Value) {
    if let Some(there) = reply["channel"].as_u64() {
        lock(&line.streams).remove(&(there as u32));
        let _ = line.frames.send(wire::encode(there as u32, wire::CLOSE, b""));
    }
}

pub(super) fn automatic_input(line: &Line, reply: &Value) -> bool {
    line.features.iter().any(|f| f == "automatic_input") && reply["automatic_input"] == true
}

pub(super) fn negotiate_resize(line: &Line, params: &mut Value) {
    if params["resize_notifications"] == true && !line.features.iter().any(|f| f == "resize_notifications") {
        params["resize_notifications"] = false.into();
    }
}

/// Opens a viewer here on a terminal there: the stream channel there is
/// relayed to a new channel on this connection, both ways.
pub async fn attach(core: &Core, conn: &Arc<Conn>, m: &Machine, mut params: Value) -> Result<Value, RpcError> {
    let line = line(core, m).await?;
    negotiate_resize(&line, &mut params);
    let here = conn.new_channel();
    conn.with(|out| out.open_paused(here));
    let deadline = deadline_for(core, "terminal.attach", &params);
    match line
        .call_into(&m.name, "terminal.attach", params.clone(), Some(((conn.clone(), here), false)), deadline)
        .await
    {
        Ok(mut r) => {
            let there = r["channel"].as_u64().unwrap_or_default() as u32;
            let enabled = params["observe"] == true
                && params["resize_notifications"] == true
                && r["resize_notifications"] == true;
            r["resize_notifications"] = enabled.into();
            params["resize_notifications"] = enabled.into();
            let automatic_input = automatic_input(&line, &r);
            r["automatic_input"] = automatic_input.into();
            let viewer = Relayed {
                automatic_input,
                line,
                there,
                conn: conn.clone(),
                machine: m.id.clone(),
                terminal: r["terminal"]["address"]
                    .as_str()
                    .or_else(|| params["terminal"].as_str())
                    .unwrap_or_default()
                    .into(),
                params,
                active_at: Instant::now(),
                size_pending: false,
            };
            conn.outbox().set_relayed(here, true);
            core.lock().relayed.insert((conn_key(conn), here), viewer);
            r["channel"] = here.into();
            Ok(localize(core, &m.name, &[], r))
        }
        Err(e) => {
            conn.with(|out| out.close(here, b""));
            Err(e)
        }
    }
}

/// A viewer's frame for a relayed stream goes there. False: not relayed.
pub fn relay_in(core: &Core, conn: &Arc<Conn>, here: u32, kind: u8, payload: &[u8]) -> bool {
    // Local channels never relay: their credit must not wait while output is parsed.
    if !conn.outbox().is_relayed(here) {
        return false;
    }
    let mut st = core.lock();
    let Some(r) = st.relayed.get_mut(&(conn_key(conn), here)) else { return false };
    let resize = kind == wire::RESIZE && payload.len() == 4;
    if r.params["observe"] != true
        && (kind == wire::DATA || resize || (kind == wire::FOCUS && payload.first() == Some(&1)))
    {
        r.active_at = Instant::now();
        if r.size_pending && !resize {
            resend_size(r);
            r.size_pending = false;
        }
    }
    // What a reattach after a dropped line brings back.
    if resize {
        r.params["cols"] = u16::from_le_bytes([payload[0], payload[1]]).into();
        r.params["rows"] = u16::from_le_bytes([payload[2], payload[3]]).into();
        r.size_pending = false;
    } else if kind == wire::FACTS
        && let Ok(display) = serde_json::from_slice::<Value>(payload)
    {
        r.params["display"] = display;
    }
    // Read here: our stream to the viewer may send more, and so may the one there to us.
    if kind == wire::CREDIT && payload.len() == 4 {
        conn.with(|out| out.credit(here, u32::from_le_bytes(payload.try_into().unwrap()) as usize));
    }
    // While the line is down this goes nowhere: typing into a gap is lost, not queued.
    let _ = r.line.frames.send(wire::encode(r.there, kind, payload));
    true
}

/// A viewer's connection closed: its relayed streams there end.
pub fn conn_gone(core: &Core, conn: &Arc<Conn>) {
    let key = conn_key(conn);
    let mut st = core.lock();
    let gone: Vec<(u32, Arc<Line>, u32)> = st
        .relayed
        .iter()
        .filter(|((k, _), _)| *k == key)
        .map(|((_, here), r)| (*here, r.line.clone(), r.there))
        .collect();
    for (here, line, there) in gone {
        st.relayed.remove(&(key, here));
        lock(&line.streams).remove(&there);
        let _ = line.frames.send(wire::encode(there, wire::CLOSE, b""));
    }
}
