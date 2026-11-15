//! Peer cores (machines that dialed into us) and bounded callbacks from added machines.

use super::*;

/// What a machine may ask of terminals here that are parents of its own.
pub(super) const CALLS_BACK: &[&str] = &["terminal.send", "terminal.get", "terminal.read", "terminal.wait"];

/// A call or a note from a machine we dialed. It reaches only terminals here
/// that started work there: adding a machine gives it no say over the rest.
pub(super) async fn from_machine(core: Arc<Core>, m: Machine, line: Arc<Line>, message: Value) {
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
        let _ = writeln!(io::stderr(), "core: cannot save peers: {e}");
    }
}

/// A core dialed in on `conn`: calls to it go out on the same connection,
/// and whatever waited in its outbox goes now.
pub fn peer_joined(core: &Arc<Core>, conn: &Arc<Conn>, hello: &Value) {
    let (Some(id), Some(name)) = (hello["core"]["id"].as_str(), hello["core"]["name"].as_str()) else { return };
    let (frames, mut out) = mpsc::unbounded_channel::<Vec<u8>>();
    let line = Line::new(frames, hello);
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
    if let Some((_, old)) = st.peer_lines.insert(id.into(), (conn_key(conn), line)) {
        old.close();
    }
    save_peers(core, &st);
}

/// A call from a peer: its callers (`<its id>/t3`) as this core calls them.
pub fn from_peer(core: &Core, conn: Option<&Arc<Conn>>, mut params: Value) -> Value {
    let Some(conn) = conn else { return params };
    let st = core.lock();
    let key = conn_key(conn);
    let Some((id, (_, line))) = st.peer_lines.iter().find(|(_, (k, _))| *k == key) else { return params };
    negotiate_resize(line, &mut params);
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
    core.lock().peer_lines.retain(|_, (k, line)| {
        if *k == key {
            line.close();
        }
        *k != key
    });
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
