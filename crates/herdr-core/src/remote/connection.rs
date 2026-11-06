//! Keeping one SSH line up, receiving frames, and recovering its viewers.

use super::*;

#[cfg(test)]
mod tests;

const WAKE_PROBE: Duration = Duration::from_secs(3);
const WAKE_RETRY_WINDOW: Duration = Duration::from_secs(30);

struct Retry {
    backoff: Duration,
    fast_until: Option<tokio::time::Instant>,
    // A failed wake probe must not ride the same dead ControlMaster again.
    fresh: bool,
}

impl Default for Retry {
    fn default() -> Self {
        Self { backoff: Duration::from_millis(250), fast_until: None, fresh: false }
    }
}

impl Retry {
    fn woke(&mut self) {
        self.backoff = Duration::from_millis(250);
        self.fast_until = Some(tokio::time::Instant::now() + WAKE_RETRY_WINDOW);
    }

    fn delay(&mut self) -> Duration {
        let fast = self.fast_until.is_some_and(|until| tokio::time::Instant::now() < until);
        let delay = if fast { self.backoff.min(Duration::from_secs(1)) } else { self.backoff };
        self.backoff = (self.backoff * 2).min(reconnect_max());
        delay
    }

    async fn wait(&mut self, wakes: &mut tokio::sync::watch::Receiver<u64>) {
        tokio::select! {
            biased;
            () = resumed(wakes) => { self.woke(); self.fresh = true; }
            () = tokio::time::sleep(self.delay()) => {}
        }
    }
}

async fn resumed(wakes: &mut tokio::sync::watch::Receiver<u64>) {
    if wakes.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn reconnect_max() -> Duration {
    Duration::from_millis(std::env::var("HC_RECONNECT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30_000))
}

/// Keeps the line to every added machine up.
pub fn start_all(core: &Arc<Core>) {
    let ids: Vec<String> = core.lock().machines.iter().map(|m| m.id.clone()).collect();
    for id in ids {
        keep_up(core, id);
    }
}

/// Dials the machine, holds the line, and dials again after a drop (backing
/// off to 30 s) until the machine is removed.
pub fn keep_up(core: &Arc<Core>, id: String) {
    let mut st = core.lock();
    st.lines.insert(id.clone(), LineState::Connecting);
    let task = tokio::spawn(supervise(core.clone(), id.clone(), crate::platform::wake_events()));
    if let Some(old) = st.line_tasks.insert(id, task.abort_handle()) {
        old.abort();
    }
}

pub fn forget(core: &Core, st: &mut State, id: &str) {
    for (address, record) in st.mirrors.remove(id).unwrap_or_default() {
        core.deliver(st, "removed", &address, json!({}), &record, Some(&record));
    }
    st.lines.remove(id);
    if let Some(task) = st.line_tasks.remove(id) {
        task.abort();
    }
    // Viewers waiting for it won't get it back.
    let waiting: Vec<(usize, u32)> = st.relayed.iter().filter(|(_, r)| r.machine == id).map(|(k, _)| *k).collect();
    for key in waiting {
        if let Some(r) = st.relayed.remove(&key) {
            r.conn.with(|out| out.finish(key.1, b""));
        }
    }
}

async fn supervise(core: Arc<Core>, id: String, mut wakes: tokio::sync::watch::Receiver<u64>) {
    let mut retry = Retry::default();
    loop {
        let Some(m) = core.lock().machines.iter().find(|m| m.id == id).cloned() else { return };
        let down = |st: &mut State, why: String| {
            let since_ms = match st.lines.get(&id) {
                Some(
                    LineState::Unreachable { since_ms, .. }
                    | LineState::NeedsLogin { since_ms }
                    | LineState::NeedsUpdate { since_ms },
                ) => *since_ms,
                _ => now_ms(),
            };
            let state = match why.as_str() {
                "needs_login" => LineState::NeedsLogin { since_ms },
                "needs_update" => LineState::NeedsUpdate { since_ms },
                _ => LineState::Unreachable { since_ms, why },
            };
            st.lines.insert(id.clone(), state);
            core.changed.notify_waiters();
        };
        let hello = json!({"framed": true, "core": core.info, "calls_you": m.name, "features": crate::api::FEATURES});
        let fast = retry.fast_until.is_some_and(|until| tokio::time::Instant::now() < until);
        let reuse_login = !retry.fresh;
        let dialed = tokio::select! {
            biased;
            () = resumed(&mut wakes) => {
                retry.woke();
                retry.fresh = true;
                continue;
            }
            dialed = async {
                let dial = crate::machines::dial_with_login(&core.paths, m.reach(), m.session.as_deref(), hello, reuse_login);
                if fast {
                    tokio::time::timeout(WAKE_PROBE, dial).await.unwrap_or_else(|_| Err(RpcError::new(
                        "unreachable", format!("{} did not finish connecting within 3 s after wake", m.name),
                        "core is retrying while the network returns",
                    )))
                } else { dial.await }
            } => dialed,
        };
        match dialed {
            Ok(d) if d.hello["core"]["id"] != m.id.as_str() => {
                down(&mut core.lock(), format!("a different core answers there ({})", d.hello["core"]["id"]));
            }
            Ok(d) => {
                retry.backoff = Duration::from_millis(250);
                // It may have been updated since it was added.
                if let Some(v) = d.hello["version"].as_str().filter(|v| *v != m.version) {
                    let mut st = core.lock();
                    if let Some(saved) = st.machines.iter_mut().find(|x| x.id == id) {
                        saved.version = v.into();
                    }
                    crate::machines::save(&core.paths.state, &st.machines);
                }
                hold(&core, &m, d, &mut wakes, &mut retry).await;
                // Dial again at once: most drops are a restart there. Calls meanwhile
                // wait for that dial instead of failing; only a failed one is "down".
                core.lock().lines.insert(id.clone(), LineState::Connecting);
                core.changed.notify_waiters();
                continue;
            }
            Err(e) if e.kind == "needs_login" => down(&mut core.lock(), "needs_login".into()),
            Err(e) if e.kind == "incompatible" => down(&mut core.lock(), "needs_update".into()),
            Err(e) => down(&mut core.lock(), e.message),
        }
        retry.wait(&mut wakes).await;
    }
}

/// Serves one open line until it closes.
async fn hold(
    core: &Arc<Core>,
    m: &Machine,
    mut d: crate::machines::Dialed,
    wakes: &mut tokio::sync::watch::Receiver<u64>,
    retry: &mut Retry,
) {
    let (frames, mut out) = mpsc::unbounded_channel::<Vec<u8>>();
    let line = Line::new(frames, &d.hello);
    {
        let mut st = core.lock();
        st.lines.insert(m.id.clone(), LineState::Connected(line.clone()));
        for message in st.machine_outbox.remove(&m.id).unwrap_or_default() {
            line.message(&message);
        }
    }
    core.changed.notify_waiters();
    tokio::spawn(reattach(core.clone(), m.clone(), line.clone()));
    // Everything that happens there, to keep its mirror here and tell subscribers.
    let id = line.next.fetch_add(1, Ordering::Relaxed);
    lock(&line.mirror).0 = id;
    line.message(&json!({"jsonrpc": "2.0", "id": id, "method": "events.subscribe", "params": {}}));
    let mut stdin = d.stdin;
    let writer = tokio::spawn(async move {
        while let Some(frame) = out.recv().await {
            if stdin.write_all(&frame).await.is_err() {
                return;
            }
        }
    });
    // ssh's own messages must not fill its pipe.
    let mut stderr = d.stderr;
    let drain = tokio::spawn(async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await });
    // It sends heartbeats: a machine silent this long is stuck, not quiet.
    let silence = crate::config::load(&core.paths.config).unwrap_or_default().machine_timeout()
        + crate::server::heartbeat_every();
    receive(core, m, &line, &mut d.stdout, wakes, retry, silence).await;
    writer.abort();
    drain.abort();
    // No longer the line to use, before anyone waiting hears it is lost (a
    // retry must not pick this dead line up again).
    {
        let mut st = core.lock();
        if matches!(st.lines.get(&m.id), Some(LineState::Connected(l)) if Arc::ptr_eq(l, &line)) {
            st.lines.insert(m.id.clone(), LineState::Connecting);
        }
    }
    // Calls still waiting on it fail now ("lost the line"), not at their deadline.
    lock(&line.pending).clear();
    // Viewers here hear that the line is gone and stay open; the programs keep
    // running there, and the viewers get them back with the line.
    let notice = format!("\r\n[line to {} lost; reconnecting...]\r\n", m.name);
    for (_, (conn, here)) in lock(&line.streams).drain() {
        conn.with(|out| {
            out.data(here, notice.as_bytes());
        });
    }
    // Killed, not waited for: a Mac core once never heard the old ssh had
    // died and stayed "connecting" for good. It is reaped in the background.
    let _ = d.child.start_kill();
}

/// Keep a partially read frame pinned across wake/probe notifications: cancelling
/// read_exact after consuming part of a header would corrupt the byte stream.
async fn receive(
    core: &Arc<Core>,
    m: &Machine,
    line: &Arc<Line>,
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    wakes: &mut tokio::sync::watch::Receiver<u64>,
    retry: &mut Retry,
    silence: Duration,
) {
    let heard = wire::Heard::default();
    let mut stdout = heard.watch(reader);
    let mut probe: Option<(u64, tokio::time::Instant)> = None;
    'receiving: loop {
        let reading = wire::read_frame(&mut stdout);
        tokio::pin!(reading);
        let (channel, kind, payload) = loop {
            let deadline = probe.map_or_else(tokio::time::Instant::now, |(_, deadline)| deadline);
            tokio::select! {
                biased;
                () = resumed(wakes) => {
                    retry.woke();
                    let id = line.next.fetch_add(1, Ordering::Relaxed);
                    line.message(&json!({"jsonrpc":"2.0", "id":id, "method":"server.status", "params":{}}));
                    probe = Some((id, tokio::time::Instant::now() + WAKE_PROBE));
                }
                () = tokio::time::sleep_until(deadline), if probe.is_some() => {
                    retry.fresh = true;
                    eprintln!("core: {} did not answer its wake probe; reconnecting", m.name);
                    break 'receiving;
                }
                frame = &mut reading => match frame {
                    Ok(Some(frame)) => break frame,
                    _ => {
                        if probe.is_some() { retry.fresh = true; }
                        break 'receiving;
                    }
                },
                () = heard.silent_for(silence), if probe.is_none() => {
                    eprintln!("core: {} sent nothing for {} s: line given up", m.name, silence.as_secs());
                    break 'receiving;
                }
            }
        };
        match (channel, kind) {
            (0, wire::DATA) => {
                let Ok(message) = serde_json::from_slice::<Value>(&payload) else { continue };
                if probe.is_some_and(|(id, _)| message["id"] == id && message.get("method").is_none()) {
                    probe = None;
                }
                let mirror_call = lock(&line.mirror).0;
                match message["method"].as_str() {
                    None if message["id"] == mirror_call => {
                        lock(&line.mirror).1 = message["result"]["channel"].as_u64().map(|c| c as u32);
                        mirror_replace(core, m, message["result"]["terminals"].as_array().cloned().unwrap_or_default());
                        if message["result"]["channel"].is_u64() {
                            // Docs are not in terminal snapshots: readers must refetch after a gap.
                            core.documents_missed(&mut core.lock());
                        }
                    }
                    None => line.take_reply(message),
                    Some(_) => drop(tokio::spawn(from_machine(core.clone(), m.clone(), line.clone(), message))),
                }
            }
            (0, _) => {}
            (there, wire::DATA) if lock(&line.mirror).1 == Some(there) => {
                mirror_events(core, m, line, &payload);
                let _ = line.frames.send(wire::encode(there, wire::CREDIT, &(payload.len() as u32).to_le_bytes()));
            }
            (there, wire::DATA) => {
                if let Some((conn, here)) = lock(&line.streams).get(&there) {
                    conn.with(|out| {
                        out.data(*here, &payload);
                    });
                }
            }
            (there, wire::RESIZE) if payload.len() == 4 => {
                if let Some((conn, here)) = lock(&line.streams).get(&there) {
                    conn.with(|out| out.ordered_resize(*here, &payload));
                }
            }
            (there, wire::CLOSE) => {
                if let Some((conn, here)) = lock(&line.streams).remove(&there) {
                    core.lock().relayed.remove(&(conn_key(&conn), here));
                    conn.with(|out| out.finish(here, &payload));
                }
            }
            _ => {}
        }
    }
}
