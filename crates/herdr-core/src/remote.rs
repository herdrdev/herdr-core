//! Lines to other machines: the core keeps one framed connection open to each
//! machine it added, and calls naming `machine/…` go over it. The other way,
//! a core that dialed in (a peer) may have parents of terminals here: they
//! hear from them over the same line, or later from an outbox kept here.

use crate::machines::Machine;
use crate::rpc::RpcError;
use crate::runtime::{Core, State, now_ms};
use crate::wire::{self, Conn};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

/// A viewer here that a stream there goes to: its connection and channel.
type Sink = (Arc<Conn>, u32);
/// A call waiting for its reply, and where a stream it opens goes.
type Waiting = (oneshot::Sender<Value>, Option<Sink>);

#[cfg(test)]
mod tests;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// One open line: calls go out on channel 0, replies come back by id;
/// attach streams there are relayed to viewers here.
pub struct Line {
    frames: mpsc::UnboundedSender<Vec<u8>>,
    pending: Mutex<HashMap<u64, Waiting>>,
    /// Stream channel there -> the viewer here.
    streams: Mutex<HashMap<u32, Sink>>,
    next: AtomicU64,
    /// The events subscription that keeps this machine's mirror: its call id,
    /// then its channel, and a line of events not yet whole.
    mirror: Mutex<(u64, Option<u32>, Vec<u8>)>,
    /// What the core there said it can do (its `hello`).
    features: Vec<String>,
}

impl Line {
    fn new(frames: mpsc::UnboundedSender<Vec<u8>>, hello: &Value) -> Arc<Line> {
        let features = hello["features"].as_array().into_iter().flatten();
        Arc::new(Line {
            features: features.filter_map(|f| f.as_str().map(String::from)).collect(),
            frames,
            pending: Mutex::default(),
            streams: Mutex::default(),
            next: AtomicU64::new(2),
            mirror: Mutex::default(),
        })
    }

    /// A reply to one of our calls: to whoever waits for it.
    fn take_reply(&self, reply: Value) {
        let Some((waiter, sink)) = reply["id"].as_u64().and_then(|id| lock(&self.pending).remove(&id)) else { return };
        if let (Some(sink), Some(there)) = (sink, reply["result"]["channel"].as_u64()) {
            lock(&self.streams).insert(there as u32, sink);
        }
        let _ = waiter.send(reply);
    }

    fn message(&self, message: &Value) {
        let _ = self.frames.send(wire::encode(0, wire::DATA, message.to_string().as_bytes()));
    }

    /// A call that gets `deadline` to answer: a stuck machine never holds us.
    pub async fn call(
        &self,
        machine: &str,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Value, RpcError> {
        self.call_into(machine, method, params, None, deadline).await
    }

    /// A call whose reply may open a stream (`channel`): it goes to `sink`
    /// from its first byte.
    async fn call_into(
        &self,
        machine: &str,
        method: &str,
        params: Value,
        sink: Option<Sink>,
        deadline: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        // Too big for one frame would end the whole line: refuse just this call.
        if req.len() > wire::MAX_PAYLOAD {
            return Err(RpcError::invalid_params(format!(
                "too big to send to {machine} ({} MB at most)",
                wire::MAX_PAYLOAD >> 20
            )));
        }
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, (tx, sink));
        let _ = self.frames.send(wire::encode(0, wire::DATA, req.as_bytes()));
        let _asked = Asked { line: self, id };
        let Ok(reply) = tokio::time::timeout(deadline, rx).await else {
            return Err(RpcError::new(
                "unavailable",
                format!("{machine} didn't answer within {} s", deadline.as_secs()),
                "it may be busy or stuck; hc machine ls shows its line",
            ));
        };
        let reply = reply.map_err(|_| {
            RpcError::new(
                "unavailable",
                format!("lost the line to {machine} during the call"),
                "it may have happened there; check, then retry",
            )
        })?;
        match reply.get("error") {
            Some(e) => Err(RpcError::from_json(e)),
            None => Ok(reply["result"].clone()),
        }
    }
}

/// A call on its way: given up on (its deadline passed, or its caller went
/// away) before the reply came, the other side hears so and can stop it. A
/// call no longer pending was answered, or its line is gone.
struct Asked<'a> {
    line: &'a Line,
    id: u64,
}

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        if lock(&self.line.pending).remove(&self.id).is_some() {
            self.line.message(&json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": self.id}}));
        }
    }
}

pub enum LineState {
    Connecting,
    Connected(Arc<Line>),
    NeedsLogin {
        since_ms: u64,
    },
    /// Its hc speaks another protocol generation.
    NeedsUpdate {
        since_ms: u64,
    },
    Unreachable {
        since_ms: u64,
        why: String,
    },
}

/// A machine as `machine ls` and `ls` show it: its profile and its line now.
#[derive(Serialize, JsonSchema, Clone)]
pub struct MachineView {
    #[serde(flatten)]
    pub machine: Machine,
    /// connecting, connected, needs_login or unreachable.
    pub state: String,
    /// Since when it is not connected (ms since the epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

pub fn view(st: &State, m: &Machine) -> MachineView {
    let (state, since_ms, why) = match st.lines.get(&m.id) {
        None | Some(LineState::Connecting) => ("connecting", None, None),
        Some(LineState::Connected(_)) => ("connected", None, None),
        Some(LineState::NeedsLogin { since_ms }) => ("needs_login", Some(*since_ms), None),
        Some(LineState::NeedsUpdate { since_ms }) => ("needs_update", Some(*since_ms), None),
        Some(LineState::Unreachable { since_ms, why }) => ("unreachable", Some(*since_ms), Some(why.clone())),
    };
    MachineView { machine: m.clone(), state: state.into(), since_ms, why }
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
    let task = tokio::spawn(supervise(core.clone(), id.clone()));
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

async fn supervise(core: Arc<Core>, id: String) {
    let mut backoff = Duration::from_millis(250);
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
        let hello = json!({"framed": true, "core": core.info, "calls_you": m.name});
        match crate::machines::dial(&core.paths, m.reach(), m.session.as_deref(), hello).await {
            Ok(d) if d.hello["core"]["id"] != m.id.as_str() => {
                down(&mut core.lock(), format!("a different core answers there ({})", d.hello["core"]["id"]));
            }
            Ok(d) => {
                backoff = Duration::from_millis(250);
                // It may have been updated since it was added.
                if let Some(v) = d.hello["version"].as_str().filter(|v| *v != m.version) {
                    let mut st = core.lock();
                    if let Some(saved) = st.machines.iter_mut().find(|x| x.id == id) {
                        saved.version = v.into();
                    }
                    crate::machines::save(&core.paths.state, &st.machines);
                }
                hold(&core, &m, d).await;
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
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(reconnect_max());
    }
}

/// A viewer here on a terminal there. It outlives a dropped line: when the
/// line is back, it is attached again on the same channel here.
#[derive(Clone)]
pub struct Relayed {
    pub line: Arc<Line>,
    pub there: u32,
    pub conn: Arc<Conn>,
    pub machine: String,
    pub params: Value,
}

/// Viewers left waiting by a dropped line to `m` get their terminal back.
async fn reattach(core: Arc<Core>, m: Machine, line: Arc<Line>) {
    let waiting: Vec<((usize, u32), Relayed)> = core
        .lock()
        .relayed
        .iter()
        .filter(|(_, r)| r.machine == m.id && !Arc::ptr_eq(&r.line, &line))
        .map(|(k, r)| (*k, r.clone()))
        .collect();
    for ((key, here), r) in waiting {
        // The screen is painted again; the viewer already has its history.
        let mut params = r.params.clone();
        params["history"] = 0.into();
        params["resumed"] = true.into();
        let deadline = deadline_for(&core, "terminal.attach", &params);
        let reply = line.call_into(&m.name, "terminal.attach", params, Some((r.conn.clone(), here)), deadline).await;
        let mut st = core.lock();
        // Gone meanwhile (the viewer detached): nothing to resume.
        if !st.relayed.contains_key(&(key, here)) {
            continue;
        }
        match reply.ok().and_then(|reply| reply["channel"].as_u64()) {
            Some(there) => {
                st.relayed.insert((key, here), Relayed { line: line.clone(), there: there as u32, ..r });
            }
            None => {
                st.relayed.remove(&(key, here));
                r.conn.with(|out| out.finish(here, b""));
            }
        }
    }
}

/// Serves one open line until it closes.
async fn hold(core: &Arc<Core>, m: &Machine, mut d: crate::machines::Dialed) {
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
    // Silence is no bytes at all: a big frame on a slow link keeps flowing.
    let heard = wire::Heard::default();
    let mut stdout = heard.watch(&mut d.stdout);
    loop {
        let (channel, kind, payload) = tokio::select! {
            frame = wire::read_frame(&mut stdout) => match frame {
                Ok(Some(frame)) => frame,
                _ => break,
            },
            () = heard.silent_for(silence) => {
                eprintln!("core: {} sent nothing for {} s: line given up", m.name, silence.as_secs());
                break;
            }
        };
        match (channel, kind) {
            (0, wire::DATA) => {
                let Ok(message) = serde_json::from_slice::<Value>(&payload) else { continue };
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
                mirror_events(core, m, &line, &payload);
                let _ = line.frames.send(wire::encode(there, wire::CREDIT, &(payload.len() as u32).to_le_bytes()));
            }
            (there, wire::DATA) => {
                if let Some((conn, here)) = lock(&line.streams).get(&there) {
                    conn.with(|out| {
                        out.data(*here, &payload);
                    });
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

/// A machine's terminals now (after a connect, or after it says we missed
/// some): subscribers here hear what appeared, changed or went.
fn mirror_replace(core: &Core, m: &Machine, terminals: Vec<Value>) {
    let mut st = core.lock();
    let Some(name) = st.machines.iter().find(|current| current.id == m.id).map(|m| m.name.clone()) else { return };
    let fresh: std::collections::BTreeMap<String, Value> = terminals
        .into_iter()
        .map(|t| localize_in(core, &st, &name, &[], t))
        .map(|t| (t["address"].as_str().unwrap_or_default().to_string(), t))
        .collect();
    let old = st.mirrors.remove(&m.id).unwrap_or_default();
    for (address, record) in old.iter().filter(|(a, _)| !fresh.contains_key(*a)) {
        core.deliver(&mut st, "removed", address, json!({}), record, Some(record));
    }
    for (address, record) in fresh.iter().filter(|(a, r)| old.get(*a) != Some(*r)) {
        core.deliver(&mut st, "created", address, record.clone(), record, None);
    }
    st.mirrors.insert(m.id.clone(), fresh);
}

pub fn rename_mirror(core: &Core, st: &mut State, id: &str, old: &str, new: &str) {
    let Some(records) = st.mirrors.remove(id) else { return };
    let mut renamed = std::collections::BTreeMap::new();
    for record in records.values() {
        let mut record = record.clone();
        map_addresses(&mut record, &|s| match s.split_once('/') {
            Some((prefix, rest)) if prefix == old => format!("{new}/{rest}"),
            _ => s.to_owned(),
        });
        let address = record["address"].as_str().unwrap_or_default().to_owned();
        renamed.insert(address, record);
    }
    st.mirrors.insert(id.to_owned(), renamed.clone());
    for (address, record) in records {
        core.deliver(st, "removed", &address, json!({}), &record, Some(&record));
    }
    for (address, record) in renamed {
        core.deliver(st, "created", &address, record.clone(), &record, None);
    }
}

/// Event lines from a machine: applied to its mirror, passed on as seen from here.
fn mirror_events(core: &Core, m: &Machine, line: &Line, payload: &[u8]) {
    let lines: Vec<Vec<u8>> = {
        let mut mirror = lock(&line.mirror);
        mirror.2.extend_from_slice(payload);
        let whole = mirror.2.iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
        let done: Vec<u8> = mirror.2.drain(..whole).collect();
        done.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec).collect()
    };
    for l in lines {
        let Ok(msg) = serde_json::from_slice::<Value>(&l) else { continue };
        match msg["method"].as_str() {
            Some("event") => mirror_event(core, m, msg["params"].clone()),
            Some("missed") => {
                mirror_replace(core, m, msg["params"]["terminals"].as_array().cloned().unwrap_or_default());
                core.documents_missed(&mut core.lock());
            }
            _ => {}
        }
    }
}

fn mirror_event(core: &Core, m: &Machine, p: Value) {
    let mut st = core.lock();
    let Some(name) = st.machines.iter().find(|current| current.id == m.id).map(|m| m.name.clone()) else { return };
    let p = localize_in(core, &st, &name, &[], p);
    let (kind, address, data) =
        (p["type"].as_str().unwrap_or_default(), p["terminal"].as_str().unwrap_or_default(), p["data"].clone());
    if kind == "doc" {
        // Only this machine, not docs it in turn watches on a third core.
        if p.get("machine").is_none() {
            core.doc_event(&mut st, data, Some(&name));
        }
        return;
    }
    let mirror = st.mirrors.entry(m.id.clone()).or_default();
    let (record, was) = match kind {
        "created" | "restarted" => {
            mirror.insert(address.into(), data.clone());
            (data.clone(), None)
        }
        "removed" => match mirror.remove(address) {
            Some(record) => (record.clone(), Some(record)),
            None => return,
        },
        _ => {
            let Some(record) = mirror.get_mut(address) else { return };
            let was = record.clone();
            apply(record, kind, &data);
            (record.clone(), Some(was))
        }
    };
    core.deliver(&mut st, kind, address, data, &record, was.as_ref());
}

/// An event's change, applied to a terminal record (see schema/README.md).
fn apply(record: &mut Value, kind: &str, data: &Value) {
    let set = |record: &mut Value, keys: &[&str]| keys.iter().for_each(|k| record[*k] = data[*k].clone());
    match kind {
        "finished" | "lost" => {
            record["state"] = if kind == "lost" { "live" } else { "finished" }.into();
            if kind == "lost" {
                record["pid"] = Value::Null;
            }
            set(record, &["reason", "exit_code", "signal", "finished_ms"]);
            record.as_object_mut().map(|r| r.remove("foreground"));
        }
        "status" => set(record, &["status", "records", "reported"]),
        "turn" => set(record, &["turn"]),
        "title" => set(record, &["title"]),
        "labels" => set(record, &["labels"]),
        "keep" => set(record, &["keep"]),
        "size" => set(record, &["cols", "rows"]),
        "foreground" => record["foreground"] = data.clone(),
        _ => {}
    }
}

/// What a machine may ask of terminals here that are parents of its own.
const CALLS_BACK: &[&str] = &["terminal.send", "terminal.get", "terminal.read", "terminal.wait"];

/// A call or a note from a machine we dialed. It reaches only terminals here
/// that started work there: adding a machine gives it no say over the rest.
async fn from_machine(core: Arc<Core>, m: Machine, line: Arc<Line>, message: Value) {
    let method = message["method"].as_str().unwrap_or_default();
    let mut params = message["params"].clone();
    let key = if method == "notes" { "parent" } else { "terminal" };
    let parent = params[key].as_str().and_then(|t| {
        let st = core.lock();
        let id = st.reg.resolve(crate::addr::local(t, &core.info.id, &core.info.name).ok()?).ok()?;
        st.reg.terminals.get(&id).filter(|t| t.afar.contains(&m.id)).map(|_| id)
    });
    if method == "notes" {
        if let (Some(id), Some(text)) = (parent, params["text"].as_str()) {
            core.lock().afar_notes.entry(id).or_default().push(text.into());
            let _ = core.jobs.send(crate::notes::Job::Notes(id));
        }
        return;
    }
    let result = match parent.filter(|_| CALLS_BACK.contains(&method)) {
        None => Err(RpcError::new(
            "denied",
            format!("{} may only reach the terminals here that started its own", m.name),
            "",
        )),
        Some(id) => {
            params["terminal"] = crate::terminals::address(id).into();
            if let Some(caller) = params["caller"].as_str() {
                params["caller"] = format!("{}/{caller}", m.name).into();
            }
            let call = crate::api::Call { core: &core, conn: None, plain: None };
            crate::api::dispatch(&call, method, params).await
        }
    };
    if let Some(id) = message.get("id") {
        line.message(&crate::rpc::response(id.clone(), result));
    }
}

/// A core that dialed in, as this core remembers it (kept across restarts).
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct Peer {
    /// What it calls itself: its terminals are `name/t3` here.
    pub name: String,
    /// What it calls this core: terminals here are `calls_me/t7` there.
    pub calls_me: String,
    /// What it must hear and couldn't yet (notes for its terminals), oldest first.
    #[serde(default)]
    pub outbox: Vec<Value>,
}

fn save_peers(core: &Core, st: &State) {
    let bytes = serde_json::to_vec(&st.peers).expect("peers serialize");
    if let Err(e) = crate::paths::write_atomic(&core.paths.state.join("peers.json"), &bytes) {
        eprintln!("core: cannot save peers: {e}");
    }
}

/// A core dialed in on `conn`: calls to it go out on the same connection,
/// and whatever waited in its outbox goes now.
pub fn peer_joined(core: &Arc<Core>, conn: &Arc<Conn>, hello: &Value) {
    let (Some(id), Some(name)) = (hello["core"]["id"].as_str(), hello["core"]["name"].as_str()) else { return };
    let (frames, mut out) = mpsc::unbounded_channel::<Vec<u8>>();
    let line = Line::new(frames, &Value::Null); // it only calls back here, never runs
    let c = conn.clone();
    tokio::spawn(async move {
        while let Some(frame) = out.recv().await {
            c.with(|o| o.control(frame));
        }
    });
    let mut st = core.lock();
    // Its name must mean it alone here; else it goes by its id.
    let name = crate::addr::clean_name(name);
    let taken = name == core.info.name
        || st.machines.iter().any(|m| m.name == name)
        || st.peers.iter().any(|(other, p)| other != id && p.name == name);
    let peer = st.peers.entry(id.into()).or_default();
    peer.name = if taken { id.into() } else { name };
    peer.calls_me = hello["calls_you"].as_str().unwrap_or(&core.info.name).into();
    for message in std::mem::take(&mut peer.outbox) {
        line.message(&message);
    }
    st.peer_lines.insert(id.into(), (conn_key(conn), line));
    save_peers(core, &st);
}

/// A call from a peer: its callers (`<its id>/t3`) as this core calls them.
pub fn from_peer(core: &Core, conn: Option<&Arc<Conn>>, mut params: Value) -> Value {
    let Some(conn) = conn else { return params };
    let st = core.lock();
    let key = conn_key(conn);
    let Some(id) = st.peer_lines.iter().find(|(_, (k, _))| *k == key).map(|(id, _)| id) else { return params };
    if let (Some(rest), Some(peer)) =
        (params["caller"].as_str().and_then(|c| c.strip_prefix(&format!("{id}/"))), st.peers.get(id))
    {
        params["caller"] = format!("{}/{rest}", peer.name).into();
    }
    params
}

/// Whether `conn` is a core that dialed in.
pub fn is_peer(core: &Core, conn: &Arc<Conn>) -> bool {
    let key = conn_key(conn);
    core.lock().peer_lines.values().any(|(k, _)| *k == key)
}

pub fn peer_left(core: &Core, conn: &Arc<Conn>) {
    let key = conn_key(conn);
    core.lock().peer_lines.retain(|_, (k, _)| *k != key);
}

/// A reply from a peer to one of our calls. False: not a peer's connection.
pub fn reply_in(core: &Core, conn: &Arc<Conn>, reply: Value) -> bool {
    let key = conn_key(conn);
    let line = core.lock().peer_lines.values().find(|(k, _)| *k == key).map(|(_, l)| l.clone());
    line.map(|l| l.take_reply(reply)).is_some()
}

/// Notes for a parent on a peer: over its line now, else kept until it is back.
pub fn tell_peer(core: &Core, st: &mut State, parent: &str, notes: &[String]) {
    let Some((prefix, local)) = parent.split_once('/') else { return };
    let Some(id) = st.peers.iter().find(|(id, p)| *id == prefix || p.name == prefix).map(|(id, _)| id.clone()) else {
        return;
    };
    let message = json!({"jsonrpc": "2.0", "method": "notes", "params": {"parent": local, "text": notes.join("\n")}});
    match st.peer_lines.get(&id) {
        Some((_, line)) => line.message(&message),
        None => {
            st.peers.get_mut(&id).expect("found").outbox.push(message);
            save_peers(core, st);
        }
    }
}

/// A parent here finished: machines it started work on end its children that
/// asked to die with it (once the line is back, if it is down).
pub fn orphaned_afar(core: &Core, st: &mut State, parent: u32) {
    let afar = st.reg.terminals.get(&parent).map(|t| t.afar.clone()).unwrap_or_default();
    let message = json!({"jsonrpc": "2.0", "method": "$/orphaned", "params": {"parent": format!("{}/{}", core.info.id, crate::terminals::address(parent))}});
    for id in afar {
        match st.lines.get(&id) {
            Some(LineState::Connected(line)) => line.message(&message),
            _ => st.machine_outbox.entry(id).or_default().push(message.clone()),
        }
    }
}

/// From a peer: its terminal `parent` finished. Children here that asked to
/// die with it end.
pub async fn orphaned(core: &Arc<Core>, conn: Option<&Arc<Conn>>, params: &Value) -> Result<Value, RpcError> {
    let from_peer = conn.is_some_and(|c| core.lock().peer_lines.values().any(|(k, _)| *k == conn_key(c)));
    let Some(address) = params["parent"].as_str().filter(|_| from_peer).and_then(|p| peer_address(core, p)) else {
        return Err(RpcError::new("denied", "only a core that dialed in says its terminals finished", ""));
    };
    let kids: Vec<u32> = {
        let mut st = core.lock();
        let kids: Vec<u32> = st
            .reg
            .terminals
            .values()
            .filter(|t| t.is_live() && t.die_with_parent && t.parent.as_deref() == Some(address.as_str()))
            .map(|t| t.id())
            .collect();
        st.killing.extend(&kids);
        kids
    };
    core.kill(&kids).await;
    Ok(json!({}))
}

/// An address on a core that dialed in, as parents from there are kept here
/// (`<its name>/t3`); `None` for anything else.
pub fn peer_address(core: &Core, address: &str) -> Option<String> {
    let (prefix, rest) = address.split_once('/')?;
    let st = core.lock();
    if prefix == core.info.id
        || prefix == core.info.name
        || st.machines.iter().any(|m| m.name == prefix || m.id == prefix)
    {
        return None;
    }
    st.peers.iter().find(|(id, p)| *id == prefix || p.name == prefix).map(|(_, p)| format!("{}/{rest}", p.name))
}

/// How terminals here are called on the peer `parent` lives on (`rohan`).
pub fn called_there(st: &State, parent: &str) -> Option<String> {
    let prefix = parent.split_once('/')?.0;
    st.peers.iter().find(|(id, p)| *id == prefix || p.name == prefix).map(|(_, p)| p.calls_me.clone())
}

pub fn conn_key(conn: &Arc<Conn>) -> usize {
    Arc::as_ptr(conn) as usize
}

/// Opens a viewer here on a terminal there: the stream channel there is
/// relayed to a new channel on this connection, both ways.
pub async fn attach(core: &Core, conn: &Arc<Conn>, m: &Machine, params: Value) -> Result<Value, RpcError> {
    let line = line(core, m).await?;
    let here = conn.new_channel();
    conn.with(|out| out.open(here));
    let deadline = deadline_for(core, "terminal.attach", &params);
    match line.call_into(&m.name, "terminal.attach", params.clone(), Some((conn.clone(), here)), deadline).await {
        Ok(mut r) => {
            let there = r["channel"].as_u64().unwrap_or_default() as u32;
            let viewer = Relayed { line, there, conn: conn.clone(), machine: m.id.clone(), params };
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
    let r = {
        let mut st = core.lock();
        let Some(r) = st.relayed.get_mut(&(conn_key(conn), here)) else { return false };
        // What a reattach after a dropped line brings back.
        if kind == wire::RESIZE && payload.len() == 4 {
            r.params["cols"] = u16::from_le_bytes([payload[0], payload[1]]).into();
            r.params["rows"] = u16::from_le_bytes([payload[2], payload[3]]).into();
        } else if kind == wire::FACTS
            && let Ok(display) = serde_json::from_slice::<Value>(payload)
        {
            r.params["display"] = display;
        }
        r.clone()
    };
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

/// Methods whose `terminal` names the terminal they act on.
const ON_A_TERMINAL: &[&str] = &[
    "terminal.attach",
    "terminal.get",
    "terminal.kill",
    "terminal.rm",
    "terminal.label",
    "terminal.keep",
    "terminal.read",
    "terminal.send",
    "terminal.keys",
    "terminal.interrupt",
    "terminal.wait",
    "terminal.put",
];

/// Where a call goes when it isn't here.
pub enum To {
    /// A machine this core added.
    Machine(Machine),
    /// A core that dialed in: only to the parents of its terminals here.
    Peer { id: String, name: String },
}

/// What an address prefix names: this core (`None`), a machine, or a peer.
fn named(core: &Core, st: &State, prefix: &str) -> Result<Option<To>, RpcError> {
    if prefix == core.info.id || prefix == core.info.name {
        return Ok(None);
    }
    if let Some(m) = st.machines.iter().find(|m| m.name == prefix || m.id == prefix) {
        return Ok(Some(To::Machine(m.clone())));
    }
    match st.peers.iter().find(|(id, p)| *id == prefix || p.name == prefix) {
        Some((id, p)) => Ok(Some(To::Peer { id: id.clone(), name: p.name.clone() })),
        None => Err(crate::machines::unknown(&st.machines, prefix)),
    }
}

/// Where a call goes: here (`None`), or elsewhere with the params to send,
/// addresses as they are seen there.
pub fn route(core: &Core, method: &str, params: &Value) -> Result<Option<(To, Value)>, RpcError> {
    let st = core.lock();
    let mut params = params.clone();
    let to = if matches!(
        method,
        "terminal.run" | "machine.exec" | "file.read" | "file.write" | "file.edit" | "doc.get" | "doc.set"
    ) {
        let Some(on) = params["on"].as_str().map(String::from) else { return Ok(None) };
        params.as_object_mut().map(|p| p.remove("on"));
        match named(core, &st, &on)? {
            Some(To::Peer { name, .. }) => return Err(crate::machines::unknown(&st.machines, &name)),
            to => to,
        }
    } else if ON_A_TERMINAL.contains(&method) && params["children"].is_null() {
        let Some((prefix, local)) =
            params["terminal"].as_str().and_then(|t| t.split_once('/')).map(|(a, b)| (a.to_string(), b.to_string()))
        else {
            return Ok(None);
        };
        let to = named(core, &st, &prefix)?;
        if let Some(To::Peer { name, .. }) = &to
            && !CALLS_BACK.contains(&method)
        {
            return Err(RpcError::new("denied", format!("{name} only takes send, read, get and wait from here"), ""));
        }
        params["terminal"] = local.into();
        to
    } else {
        None
    };
    let Some(to) = to else { return Ok(None) };
    // Our callers travel by our id (the machine names them; see `from_peer`);
    // a peer names callers from here itself.
    if let (To::Machine(_), Some(caller)) = (&to, params["caller"].as_str()) {
        params["caller"] = format!("{}/{caller}", core.info.id).into();
    }
    Ok(Some((to, params)))
}

/// Returns once the machine's line is past dialing (or `machine_timeout` passed).
pub async fn dialed(core: &Core, id: &str) {
    let wait = crate::config::load(&core.paths.config).unwrap_or_default().machine_timeout();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let changed = core.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !matches!(core.lock().lines.get(id), Some(LineState::Connecting)) || tokio::time::Instant::now() >= deadline
        {
            return;
        }
        let _ = tokio::time::timeout_at(deadline, changed).await;
    }
}

/// The open line to `m` (waiting while it is first dialed), or why there is none.
async fn line(core: &Core, m: &Machine) -> Result<Arc<Line>, RpcError> {
    dialed(core, &m.id).await;
    let st = core.lock();
    match st.lines.get(&m.id) {
        Some(LineState::Connected(line)) => Ok(line.clone()),
        Some(LineState::NeedsLogin { .. }) => Err(RpcError::new(
            "unavailable",
            format!("{} needs a login typed by a person", m.name),
            if cfg!(windows) {
                format!("set up its SSH keys, then: hc machine login {}", m.name)
            } else {
                format!("in a terminal: hc machine login {}", m.name)
            },
        )),
        Some(LineState::NeedsUpdate { .. }) => Err(RpcError::new(
            "needs_update",
            format!("{}'s hc speaks another protocol generation", m.name),
            format!("hc machine update {}", m.name),
        )),
        Some(LineState::Unreachable { why, .. }) => Err(RpcError::new(
            "unavailable",
            format!("{} is unreachable: {why}", m.name),
            "hc machine ls shows when it comes back",
        )),
        _ => Err(RpcError::new("unavailable", format!("still connecting to {}", m.name), "try again in a moment")),
    }
}

/// Sends a call to `m` and returns the reply with addresses as this core sees them.
/// How long a call there may take: `machine_timeout`, plus the wait it asks
/// for when it waits (`wait`, a waited `run` or `send`).
fn deadline_for(core: &Core, method: &str, params: &Value) -> Duration {
    let config = crate::config::load(&core.paths.config).unwrap_or_default();
    if method == "machine.exec" {
        // Its own timeout, then the time to end everything it started.
        let secs = params["timeout"].as_f64().unwrap_or(crate::exec::DEFAULT_TIMEOUT).clamp(0.0, 600.0);
        return config.machine_timeout() + Duration::from_secs_f64(secs) + Duration::from_secs(5);
    }
    let waits =
        method == "terminal.wait" || (params["wait"] == true && matches!(method, "terminal.run" | "terminal.send"));
    let wait = match params["timeout"].as_f64() {
        Some(secs) => secs,
        None => config.wait_timeout.unwrap_or(180.0),
    };
    config.machine_timeout() + if waits { Duration::from_secs_f64(wait.clamp(0.0, 86_400.0)) } else { Duration::ZERO }
}

fn waits(method: &str, params: &Value) -> bool {
    method == "terminal.wait" || (params["wait"] == true && matches!(method, "terminal.run" | "terminal.send"))
}

fn line_lost(e: &RpcError) -> bool {
    e.message.starts_with("lost the line")
}

/// A wait there outlives a dropped line: once the line is back it waits
/// again, until its own deadline. A run is found again by a label it was
/// started with; its output is read once it is done.
async fn forward_waiting(core: &Core, m: &Machine, method: &str, mut params: Value) -> Result<Value, RpcError> {
    let deadline = tokio::time::Instant::now() + deadline_for(core, method, &params);
    let mark = (method == "terminal.run").then(crate::platform::random_hex);
    if let Some(mark) = &mark {
        params["labels"]["hc.run"] = mark.clone().into();
    }
    let first = match forward_once(core, m, method, params.clone()).await {
        Err(e) if line_lost(&e) => e,
        done => return done,
    };
    let caller = params["caller"].clone();
    let mut target = params["terminal"].as_str().map(String::from);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(first);
        }
        dialed(core, &m.id).await;
        if line(core, m).await.is_err() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        if target.is_none()
            && let Some(mark) = &mark
        {
            let found = forward_once(core, m, "terminal.list", json!({"all": true, "labels": {"hc.run": mark}})).await;
            target = found.ok().and_then(|r| r["terminals"][0]["address"].as_str().map(String::from));
            if target.is_none() {
                return Err(first); // it never started there
            }
        }
        let Some(terminal) = target.clone() else { return Err(first) };
        let local = terminal.split_once('/').map_or(terminal.as_str(), |(_, t)| t).to_string();
        let left = deadline.saturating_duration_since(tokio::time::Instant::now()).as_secs_f64();
        let wait = json!({"terminal": local, "timeout": left, "caller": caller});
        let r = match forward_once(core, m, "terminal.wait", wait).await {
            Err(e) if line_lost(&e) => continue,
            other => other?,
        };
        return Ok(match method {
            "terminal.run" => {
                let mut record = forward_once(core, m, "terminal.get", json!({"terminal": local})).await?;
                let read = json!({"terminal": local, "lines": crate::api::FINISHED_LINES});
                let text =
                    forward_once(core, m, "terminal.read", read).await.map(|r| r["text"].clone()).unwrap_or_default();
                let id = crate::terminals::parse_id(&local).unwrap_or_default();
                record["wait"] = r;
                record["output"] = crate::api::tail(text.as_str().unwrap_or_default().to_string(), id).into();
                record
            }
            "terminal.send" => json!({"status": r["status"], "turn": r["turn"], "wait": r}),
            _ => r,
        });
    }
}

pub async fn forward(core: &Core, m: &Machine, method: &str, params: Value) -> Result<Value, RpcError> {
    match waits(method, &params) {
        true => forward_waiting(core, m, method, params).await,
        false => forward_once(core, m, method, params).await,
    }
}

async fn forward_once(core: &Core, m: &Machine, method: &str, mut params: Value) -> Result<Value, RpcError> {
    if method == "terminal.run" {
        // An older core ends a child with its parent only when asked to.
        params["die_with_parent"] = json!(params["outlive_parent"] != true);
    }
    let deadline = deadline_for(core, method, &params);
    let line = line(core, m).await?;
    // An older core would read no program as "nothing to run".
    let own_shell = params["argv"].as_array().is_none_or(Vec::is_empty) || params.get("line").is_some();
    if method == "terminal.run" && own_shell && !line.features.iter().any(|f| f == "shell") {
        return Err(RpcError::new(
            "needs_update",
            format!("{}'s hc is older and can't pick its own shell", m.name),
            format!("hc machine update {}", m.name),
        ));
    }
    let reply = line.call(&m.name, method, params, deadline).await.map_err(|e| match e.kind.as_str() {
        "unknown_method" => RpcError::new(
            "needs_update",
            format!("{}'s hc is older and has no {method}", m.name),
            format!("hc machine update {}", m.name),
        ),
        _ => e,
    })?;
    // Document contents are opaque: address-shaped keys belong to the client.
    let reply = if method.starts_with("doc.") { reply } else { localize(core, &m.name, &[], reply) };
    // It started work there: its child may now call back and send it notes.
    if method == "terminal.run"
        && let Some(parent) = reply["parent"].as_str().and_then(crate::terminals::parse_id)
    {
        let mut st = core.lock();
        if let Some(t) = st.reg.terminals.get_mut(&parent) {
            t.afar.insert(m.id.clone());
            core.save(&st);
        }
    }
    Ok(reply)
}

pub async fn forward_to(core: &Core, to: &To, method: &str, params: Value) -> Result<Value, RpcError> {
    match to {
        To::Machine(m) => forward(core, m, method, params).await,
        To::Peer { id, name } => {
            let found = core.lock().peer_lines.get(id).map(|(_, l)| l.clone());
            let line = found
                .ok_or_else(|| RpcError::new("unavailable", format!("{name} is not connected"), "try again later"))?;
            let calls_me = core.lock().peers.get(id).map(|p| p.calls_me.clone()).unwrap_or_default();
            let deadline = deadline_for(core, method, &params);
            let reply = line.call(name, method, params, deadline).await?;
            Ok(if method.starts_with("doc.") { reply } else { localize(core, name, &[calls_me.as_str()], reply) })
        }
    }
}

/// Addresses from the core called `name`: its `t7` is `name/t7` here; ours
/// (by our id, our name, or an `alias` it calls us) are bare again.
pub fn localize(core: &Core, name: &str, aliases: &[&str], v: Value) -> Value {
    localize_in(core, &core.lock(), name, aliases, v)
}

fn map_addresses(v: &mut Value, map: &dyn Fn(&str) -> String) {
    match v {
        Value::Object(o) => {
            for (k, v) in o.iter_mut() {
                match v {
                    Value::String(s) if ["address", "parent", "terminal", "from", "removed"].contains(&k.as_str()) => {
                        *s = map(s)
                    }
                    _ => map_addresses(v, map),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| map_addresses(v, map)),
        _ => {}
    }
}

fn localize_in(core: &Core, st: &State, name: &str, aliases: &[&str], mut v: Value) -> Value {
    let map = |s: &str| match s.split_once('/') {
        None => format!("{name}/{s}"),
        Some((prefix, rest)) if prefix == core.info.id || prefix == core.info.name || aliases.contains(&prefix) => {
            rest.to_string()
        }
        Some((prefix, rest)) => match st.machines.iter().find(|m| m.id == prefix) {
            Some(m) => format!("{}/{rest}", m.name),
            None => s.to_string(),
        },
    };
    map_addresses(&mut v, &map);
    v
}
