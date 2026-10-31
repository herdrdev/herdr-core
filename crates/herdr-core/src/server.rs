//! The core daemon: one per session, guarded by a lock, serving a local socket
//! (a named pipe on Windows).

use crate::api::{self, CoreInfo};
use crate::paths::Paths;
use crate::platform::{self, AsyncListener};
use crate::rpc::{self, RpcError};
use crate::runtime::Core;
use crate::wire::{self, Conn};
use serde_json::Value;
use std::io;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type Reader = BufReader<platform::AsyncRead>;
type Writer = platform::AsyncWrite;

pub fn serve() -> io::Result<()> {
    let paths = Paths::from_env().map_err(|e| io::Error::other(e.message))?;
    paths.create_dirs()?;
    let mut lock =
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths.state.join("core.lock"))?;
    // Someone else holds the lock: they serve (or are handing over to whoever
    // starts the next core). Waiting here would start an unwanted core after a stop.
    if !platform::try_lock(&lock) {
        return Ok(());
    }
    // Where we listen, for clients whose env points elsewhere.
    lock.set_len(0)?;
    io::Write::write_all(&mut lock, paths.socket.as_os_str().as_encoded_bytes())?;
    let info = load_identity(&paths.state, platform::machine_id())?;
    let socket = paths.socket.clone();
    let _shutdown_watch = platform::monitor_host_shutdown();
    // Adopt the holder's terminals before anyone can talk to us.
    let core = Core::start(paths, info)?;
    let bound = AsyncListener::bind(&socket)?;

    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let mut listener = AsyncListener::serve(bound)?;
        crate::restart::consumed(&core.paths)?;
        let kept = core.clone();
        tokio::spawn(async move {
            kept.clone().recover_kept(true).await;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                kept.clone().recover_kept(false).await;
            }
        });
        if let Some(jobs) = core.jobs_rx.lock().unwrap_or_else(|p| p.into_inner()).take() {
            tokio::spawn(core.clone().run_jobs(jobs));
        }
        crate::remote::start_all(&core);
        tokio::spawn(core.clone().watch_foreground());
        tokio::spawn(core.clone().save_when_due());
        let term = platform::termination();
        tokio::pin!(term);
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (read, write) = accepted?;
                    tokio::spawn(connection(core.clone(), read, write));
                }
                _ = core.stop.notified() => break,
                // SIGTERM is a planned restart: programs stay, screens are saved.
                _ = &mut term => {
                    if let Err(error) = crate::restart::prepare(&core).await {
                        eprintln!("core: cannot restart: {error}");
                        term.set(platform::termination());
                        continue;
                    }
                    break;
                }
            }
        }
        platform::remove_endpoint(&socket);
        if core.restart.load(std::sync::atomic::Ordering::SeqCst) {
            core.handover();
        }
        Ok(())
    })
}

/// core.json: the identity, and the system's machine id it was made on.
#[derive(serde::Serialize, serde::Deserialize)]
struct Identity {
    #[serde(flatten)]
    info: CoreInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    machine_id: Option<String>,
}

/// What a copy of another machine's disk must not bring along: that
/// machine's terminals, screens, files, execs and the cores that dialed it.
/// Kept: the machines it reaches and the clients' docs (settings, not life).
const CLONE_FORGETS: &[&str] = &["terminals.json", "peers.json", "screens", "files", "execs"];

/// Permanent core identity: created on first start, kept until state is
/// wiped, or until the disk turns up under another machine id (a snapshot,
/// an image): then this is a new machine, with a new id and none of the
/// old one's life. A hostname change is no clone: people rename machines.
/// A file from before machine ids adopts the current one; with none
/// readable, the id stays.
fn load_identity(state: &Path, machine_id: Option<String>) -> io::Result<CoreInfo> {
    let path = state.join("core.json");
    let saved: Option<Identity> = crate::paths::read_state(&path)?;
    let info = match saved {
        Some(saved) if saved.machine_id.is_some() && machine_id.is_some() && saved.machine_id != machine_id => {
            eprintln!("core: this disk came from another machine: a new identity, its terminals forgotten");
            // All gone before the new identity is written, or the next start tries again.
            for name in CLONE_FORGETS {
                let p = state.join(name);
                let gone = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                if let Err(e) = gone.or_else(|e| if e.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(e) }) {
                    return Err(io::Error::new(e.kind(), format!("forgetting the copied {name}: {e}")));
                }
            }
            CoreInfo { id: platform::random_hex(), name: hostname() }
        }
        Some(saved) if saved.machine_id == machine_id || machine_id.is_none() => return Ok(saved.info),
        Some(saved) => saved.info,
        None => CoreInfo { id: platform::random_hex(), name: hostname() },
    };
    let identity = Identity { info, machine_id };
    crate::paths::write_atomic(&path, &serde_json::to_vec_pretty(&identity).map_err(io::Error::other)?)?;
    Ok(identity.info)
}

fn hostname() -> String {
    let name = platform::hostname();
    crate::addr::clean_name(name.split('.').next().unwrap_or_default())
}

/// A connection starts in plain mode (one JSON-RPC message per line);
/// a successful `hello` with `framed: true` switches it to frames.
async fn connection(core: Arc<Core>, read: platform::AsyncRead, mut write: Writer) -> io::Result<()> {
    let mut read = BufReader::new(read);
    let mut line = String::new();
    let plain = std::sync::Mutex::new(None);
    let mut ahead = Vec::new(); // what came while an exec ran
    loop {
        line.clear();
        match ahead.iter().position(|b| *b == b'\n') {
            Some(at) => line = String::from_utf8_lossy(&ahead.drain(..=at).collect::<Vec<u8>>()).into_owned(),
            None => {
                if read.read_line(&mut line).await? == 0 && ahead.is_empty() {
                    return Ok(());
                }
                line.insert_str(0, &String::from_utf8_lossy(&std::mem::take(&mut ahead)));
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let handling = handle(&core, None, Some(&plain), line.as_bytes());
        // An exec ends when its caller goes away.
        let handled = match is_exec(line.as_bytes()) {
            true => tokio::select! {
                handled = handling => handled,
                () = closed(&mut read, &mut ahead) => return Ok(()),
            },
            false => handling.await,
        };
        let wrote = match &handled.reply {
            Some(reply) => write.write_all(format!("{reply}\n").as_bytes()).await,
            None => Ok(()),
        };
        // Even if the caller is gone: `hc server stop` typed in one of our
        // terminals ends its own caller before the reply.
        handled.after(&core);
        wrote?;
        let sub = plain.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(sub) = sub {
            return plain_events(&core, sub, read, write).await;
        }
        if handled.framed {
            break;
        }
    }
    let conn = Arc::new(Conn::default());
    let mut writer = tokio::spawn(write_frames(conn.clone(), write));
    // Another core dialing in says who it is in its hello.
    let hello: Value = serde_json::from_str(&line).unwrap_or_default();
    crate::remote::peer_joined(&core, &conn, &hello["params"]);
    // It ends when it closes, can't be written to, or stopped reading so long
    // that its replies passed `MAX_CONTROL`.
    let result = tokio::select! {
        result = read_frames(&core, &conn, &mut read) => result,
        _ = &mut writer => Ok(()),
        () = conn.overflowed.notified() => {
            eprintln!("core: a client stopped reading its replies: disconnected");
            Ok(())
        }
    };
    core.detach(&conn, None);
    crate::remote::conn_gone(&core, &conn);
    crate::remote::peer_left(&core, &conn);
    writer.abort();
    result
}

fn is_exec(message: &[u8]) -> bool {
    serde_json::from_slice::<Value>(message).is_ok_and(|m| m["method"] == "machine.exec")
}

/// Returns once a plain connection is closed; what it sends meanwhile is
/// kept in `ahead` (up to a frame's worth, then it stops looking).
async fn closed(read: &mut Reader, ahead: &mut Vec<u8>) {
    while ahead.len() <= wire::MAX_PAYLOAD {
        let n = match read.fill_buf().await {
            Ok([]) | Err(_) => return,
            Ok(bytes) => {
                ahead.extend_from_slice(bytes);
                bytes.len()
            }
        };
        read.consume(n);
    }
    std::future::pending().await
}

pub fn heartbeat_every() -> std::time::Duration {
    let ms = std::env::var("HC_HEARTBEAT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(15_000);
    std::time::Duration::from_millis(ms)
}

/// A plain connection after `events.subscribe`: notifications only, until it closes.
async fn plain_events(core: &Arc<Core>, mut sub: api::PlainSub, mut read: Reader, mut write: Writer) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    let stuck = std::time::Duration::from_secs(30);
    let mut tick = tokio::time::interval(heartbeat_every());
    tick.tick().await;
    let mut ignored = String::new();
    loop {
        let line = tokio::select! {
            line = sub.rx.recv() => match line {
                Some(line) => line,
                None => return Ok(()),
            },
            _ = tick.tick() => crate::events::heartbeat_line(core.lock().events.seq),
            n = read.read_line(&mut ignored) => {
                ignored.clear();
                if n? == 0 { return Ok(()) }
                continue;
            }
        };
        let mut lines = vec![line];
        if sub.behind.swap(false, Ordering::SeqCst) {
            while sub.rx.try_recv().is_ok() {}
            lines = vec![core.missed_line(&sub.filter)];
        }
        for line in lines {
            // A client that stops reading for this long is dropped.
            match tokio::time::timeout(stuck, write.write_all(line.as_bytes())).await {
                Ok(result) => result?,
                Err(_) => return Ok(()),
            }
        }
    }
}

/// Heartbeats on a framed event channel until it is gone.
pub fn heartbeat(core: Arc<Core>, conn: Arc<Conn>, channel: u32) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(heartbeat_every()).await;
            let st = core.lock();
            if !core.subscribed(&st, &conn, channel) {
                return;
            }
            let line = crate::events::heartbeat_line(st.events.seq);
            // Only into an empty queue: queued events already say it's alive, and a
            // full queue must overflow on an event (which brings `missed`).
            conn.with(|out| {
                if out.queued(channel) == 0 {
                    out.data(channel, line.as_bytes());
                }
            });
        }
    });
}

/// Sends whatever the outbox hands out: control first, then streams by credit.
async fn write_frames(conn: Arc<Conn>, mut write: Writer) -> io::Result<()> {
    loop {
        conn.wake.notified().await;
        loop {
            let next = conn.outbox().next_frame();
            match next {
                Some(frame) => write.write_all(&frame).await?,
                None => break,
            }
        }
    }
}

async fn read_frames(core: &Arc<Core>, conn: &Arc<Conn>, read: &mut Reader) -> io::Result<()> {
    let u16_at = |p: &[u8], i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
    // Execs running for this connection by request id: they end when it
    // closes, or when it gives up on one (`$/cancelRequest`).
    let mut execs = Execs::default();
    while let Some((channel, kind, payload)) = wire::read_frame(read).await? {
        if channel != 0 && crate::remote::relay_in(core, conn, channel, kind, &payload) {
            continue;
        }
        match (channel, kind) {
            // Calls run side by side so a slow one (kill, wait) never holds up typing.
            (0, wire::DATA) => {
                // A peer's reply to a call of ours, not a call.
                if let Ok(reply) = serde_json::from_slice::<Value>(&payload)
                    && reply.get("method").is_none()
                    && crate::remote::reply_in(core, conn, reply)
                {
                    continue;
                }
                let request = serde_json::from_slice::<Value>(&payload).unwrap_or_default();
                if request["method"] == "$/cancelRequest" {
                    let id = request["params"]["id"].to_string();
                    execs.0.iter().filter(|(of, _)| *of == id).for_each(|(_, exec)| exec.abort());
                    continue;
                }
                let (core, conn) = (core.clone(), conn.clone());
                let task = tokio::spawn(async move {
                    let handled = handle(&core, Some(&conn), None, &payload).await;
                    if let Some(reply) = &handled.reply {
                        conn.with(|out| out.control(wire::encode(0, wire::DATA, reply.to_string().as_bytes())));
                    }
                    if handled.stop {
                        // Give the writer a moment to put the reply on the wire before we exit.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    handled.after(&core);
                });
                if request["method"] == "machine.exec" {
                    execs.0.retain(|(_, exec)| !exec.is_finished());
                    execs.0.push((request["id"].to_string(), task.abort_handle()));
                }
            }
            (0, _) => {}
            (ch, wire::DATA) => core.viewer_input(conn, ch, &payload),
            (ch, wire::CREDIT) if payload.len() == 4 => {
                let n = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
                conn.with(|out| out.credit(ch, n));
            }
            (ch, wire::RESIZE) if payload.len() == 4 => {
                core.viewer_resize(conn, ch, u16_at(&payload, 0), u16_at(&payload, 2))
            }
            (ch, wire::FOCUS) => core.viewer_focus(conn, ch, payload.first() == Some(&1)),
            (ch, wire::FACTS) => {
                if let Ok(display) = serde_json::from_slice(&payload) {
                    core.viewer_facts(conn, ch, display);
                }
            }
            (ch, wire::CLOSE) => {
                core.detach(conn, Some(ch));
                conn.with(|out| out.close(ch, b""));
            }
            _ => {}
        }
    }
    Ok(())
}

/// A connection's running execs by request id, ended with it.
#[derive(Default)]
struct Execs(Vec<(String, tokio::task::AbortHandle)>);

impl Drop for Execs {
    fn drop(&mut self) {
        self.0.iter().for_each(|(_, exec)| exec.abort());
    }
}

struct Handled {
    reply: Option<Value>,
    framed: bool,
    stop: bool,
}

impl Handled {
    /// Effects that must follow the reply on the wire.
    fn after(&self, core: &Core) {
        if self.stop {
            core.stop.notify_one();
        }
    }
}

async fn handle(
    core: &Arc<Core>,
    conn: Option<&Arc<Conn>>,
    plain: Option<&std::sync::Mutex<Option<api::PlainSub>>>,
    message: &[u8],
) -> Handled {
    let reply = |reply| Handled { reply: Some(reply), framed: false, stop: false };
    let request: Value = match serde_json::from_slice(message) {
        Ok(v) => v,
        Err(e) => return reply(rpc::response(Value::Null, Err(RpcError::parse_error(e.to_string())))),
    };
    let Some(method) = request["method"].as_str() else {
        let err = RpcError::invalid_request("missing method");
        return reply(rpc::response(request["id"].clone(), Err(err)));
    };
    let call = api::Call { core, conn, plain };
    let params = crate::remote::from_peer(core, conn, request["params"].clone());
    let result = match crate::remote::route(core, method, &params) {
        _ if method == "$/orphaned" => crate::remote::orphaned(core, conn, &params).await,
        Ok(Some((crate::remote::To::Machine(machine), params))) if method == "terminal.attach" => match conn {
            Some(conn) => crate::remote::attach(core, conn, &machine, params).await,
            None => Err(RpcError::new(
                "needs_framed",
                "attach needs a framed connection",
                "send hello with framed: true first, or use `hc attach`",
            )),
        },
        Ok(Some((to, params))) => crate::remote::forward_to(core, &to, method, params).await,
        Ok(None) => api::dispatch(&call, method, params).await,
        Err(e) => Err(e),
    };
    let ok = result.is_ok();
    let framed = ok && method == "hello" && result.as_ref().is_ok_and(|r| r["framed"] == true);
    let stop = ok && (method == "server.stop" || method == "server.restart");
    // Requests without an id are notifications: no reply.
    let reply = request.get("id").map(|id| rpc::response(id.clone(), result));
    Handled { reply, framed, stop }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disk_under_another_machine_id_is_a_new_machine_without_the_old_ones_life() {
        let dir = std::env::temp_dir().join(format!("hc-identity-{}", platform::random_hex()));
        std::fs::create_dir_all(dir.join("screens")).unwrap();
        let id = |m: Option<&str>| load_identity(&dir, m.map(String::from)).unwrap().id;
        let first = id(Some("a"));
        assert_eq!(id(Some("a")), first, "the same machine keeps its id");
        assert_eq!(id(None), first, "none readable: kept");

        for f in ["terminals.json", "peers.json", "machines.json", "docs.json"] {
            std::fs::write(dir.join(f), "{}").unwrap();
        }
        let copy = id(Some("b"));
        assert_ne!(copy, first, "a copy is another machine");
        assert_eq!(id(Some("b")), copy);
        let left = |f: &str| dir.join(f).exists();
        assert!(!left("terminals.json") && !left("peers.json") && !left("screens"), "its life forgotten");
        assert!(left("machines.json") && left("docs.json"), "its settings kept");

        // A core.json from before machine ids adopts the current one, same id.
        std::fs::write(dir.join("core.json"), r#"{"id":"old","name":"x"}"#).unwrap();
        assert_eq!(id(Some("c")), "old");
        assert_eq!(id(Some("c")), "old");
        assert_ne!(id(Some("d")), "old", "and from then on a copy is told apart");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
