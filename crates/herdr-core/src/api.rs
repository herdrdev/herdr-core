//! The method table: every method's description, errors and handler. Its
//! params and results are in `api/types.rs`; `run` and the waits have their
//! own files.

use crate::holder_proto as proto;
use crate::rpc::{RpcError, methods};
use crate::runtime::{Core, now_ms};
use crate::terminals::{NewTerminal, Public, Terminal};
use crate::wait::{WaitResult, WaitSpec};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub const GENERATION: u32 = 1;
pub const FEATURES: &[&str] = &["framed", "attach", "events", "status", "shell"];
pub const FINISHED_LINES: usize = 1000;
/// A terminal's identity: fixed at start from whoever starts it.
pub const IDENTITY_VARS: &[&str] = &["TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "COLORTERM"];

mod doc;
mod run;
mod types;
use doc::*;
mod waiting;
use run::*;
pub(crate) use run::{child_env, not_stopping, tail, terminal_env};
pub use types::*;
use waiting::*;

/// The whole API as an OpenRPC document, generated from the method table.
pub fn openrpc() -> serde_json::Value {
    let (methods, defs) = discover();
    crate::rpc::openrpc(methods, defs, &version(), GENERATION)
}

/// One call: the core plus the connection it came on.
pub struct Call<'a> {
    pub core: &'a Arc<Core>,
    /// The framed connection, if any.
    pub conn: Option<&'a Arc<crate::wire::Conn>>,
    /// Where a plain connection receives its subscription.
    pub plain: Option<&'a std::sync::Mutex<Option<PlainSub>>>,
}

impl std::ops::Deref for Call<'_> {
    type Target = Arc<Core>;
    fn deref(&self) -> &Arc<Core> {
        self.core
    }
}

type Ctx<'a> = &'a Call<'a>;

methods! { Ctx<'_>;
    "hello" => hello, HelloParams => HelloResult,
        "Handshake: protocol generation, core identity, methods and features. `framed: true` switches the connection to framed mode.",
        errors ["unsupported_generation"];
    "server.status" => server_status, Empty => StatusResult,
        "This core's identity, version, pid, uptime, holder and terminal counts.",
        errors [];
    "server.stop" => server_stop, Empty => StopResult,
        "Stop the core and its programs. Kept terminals remain pending and get new shells when the core next starts.",
        errors [];
    "server.restart" => server_restart, Empty => StopResult,
        "Planned restart (e.g. after an update): screens are saved, programs keep running, the next core adopts them.",
        errors [];
    "terminal.run" => terminal_run, RunParams => RunResult,
        "Start a program in a new terminal; or, when argv[0] names a template, start its command and send its body plus the rest of argv as the first message once the agent is ready. Returns once it runs (and the first message is sent or not).",
        errors ["invalid_params", "name_taken", "spawn_failed", "unavailable", "invalid_template", "not_found", "machine_unknown", "config"];
    "templates.list" => templates_list, Empty => TemplatesResult,
        "Templates by name and description, so you can pick a role without knowing its command.",
        errors [];
    "terminal.list" => terminal_list, ListParams => ListResult,
        "Live terminals (all with `all`), optionally filtered by labels.",
        errors [];
    "terminal.get" => terminal_get, TerminalParams => Public,
        "One terminal's facts, live or finished.",
        errors ["not_found", "machine_unknown"];
    "terminal.kill" => terminal_kill, TerminalParams => Public,
        "End the program: hangup, then terminate, then kill. Returns the observed result.",
        errors ["not_found", "machine_unknown"];
    "terminal.rm" => terminal_rm, TerminalParams => RemoveResult,
        "Remove a finished terminal.",
        errors ["not_found", "machine_unknown", "live"];
    "terminal.keep" => terminal_keep, KeepParams => Public,
        "Set whether a live terminal restarts as a shell after loss, including death by signal without an exit code. Exit with a code and explicit kill close it. Recovery runs on every OS after a grace delay, except while the core stops/restarts or Windows reports session shutdown. Streams end on loss; reattach after restarted.", errors ["not_found", "invalid_params"];
    "terminal.label" => terminal_label, LabelParams => Public,
        "Set or remove labels (free-form `key=value`, never interpreted).",
        errors ["not_found", "machine_unknown", "invalid_params"];
    "terminal.read" => terminal_read, ReadParams => ReadResult,
        "The screen plus `lines` of history as text, live or finished.",
        errors ["not_found", "machine_unknown"];
    "terminal.attach" => terminal_attach, AttachParams => AttachResult,
        "Open a raw stream to a live terminal on this framed connection (see `hello` with `framed`). Loss ends the stream even when kept; reattach after the restarted event.",
        errors ["not_found", "machine_unknown", "finished", "unavailable", "needs_framed"];
    "terminal.send" => terminal_send, SendParams => SendResult,
        "Paste text and press Enter, once nobody has typed for ~3 s; keys a person presses meanwhile are held, then delivered. Refuses a blocked program unless `force`.",
        errors ["not_found", "machine_unknown", "finished", "unavailable", "blocked", "status_mismatch", "invalid_params", "config"];
    "terminal.wait" => terminal_wait, WaitParams => crate::wait::WaitResult,
        "Wait until the program is not working (any word but working, or it exited/was lost); already not working returns at once. A program that never reported status is working until it exits. After a send from the same caller, waits for the turn that send started. A message to the caller also ends it: ended false, reason message, from = sender. A timeout is a check-in: ended false, current status.",
        errors ["not_found", "machine_unknown", "invalid_params", "needs_target", "config"];
    "terminal.keys" => terminal_keys, KeysParams => KeysResult,
        "Press named keys, encoded for the program's current keyboard modes. Same quiet rule as send.",
        errors ["not_found", "machine_unknown", "finished", "unavailable", "invalid_params"];
    "terminal.interrupt" => terminal_interrupt, InterruptParams => InterruptResult,
        "Press the stop key (esc if it reports status, else ctrl+c), then report what was observed. Never escalates.",
        errors ["not_found", "machine_unknown", "finished", "unavailable", "invalid_params"];
    "terminal.put" => terminal_put, PutParams => PutResult,
        "Write bytes to a file beside a live terminal, on its machine, for its program to read (a pasted image). Lands in a folder of its own, removed with the terminal; the name is kept (a suffix when taken). Types nothing: paste the path yourself.",
        errors ["not_found", "machine_unknown", "finished", "invalid_params"];
    "machine.exec" => machine_exec, ExecParams => ExecResult,
        "Run one program here or on a machine (`on`) without a terminal; return its exit code and exactly what it wrote (base64). `argv` runs as given, `line` in that machine's shell. Everything it started ends with it: at its exit, at `timeout` (default 60 s), or when the caller goes away. A nonzero exit or a timeout is a result, not an error.",
        errors ["invalid_params", "spawn_failed", "machine_unknown", "unavailable", "needs_update"];
    "file.read" => file_read, FileReadParams => FileReadResult,
        "Read a text file here or on a machine (`on`): lines from `offset`, at most `limit` and 8 MB, and how many it has.",
        errors ["invalid_params", "not_found", "not_text", "io", "machine_unknown", "unavailable", "needs_update"];
    "file.write" => file_write, FileWriteParams => FileResult,
        "Write a file here or on a machine (`on`), making missing folders. Swapped in whole: a reader sees old or new, never half. An existing file keeps its mode; a symlink is followed.",
        errors ["invalid_params", "io", "machine_unknown", "unavailable", "needs_update"];
    "file.edit" => file_edit, FileEditParams => FileResult,
        "Edit a text file here or on a machine (`on`): each `old` must be found exactly once in the file as it was, edits must not overlap; all are made or none. Written like file.write.",
        errors ["invalid_params", "edit_failed", "not_found", "not_text", "io", "machine_unknown", "unavailable", "needs_update"];
    "doc.get" => doc_get, DocGetParams => DocGetResult,
        "Read an opaque JSON document and its version (zero when never written). Optional on selects another machine.",
        errors ["invalid_params", "machine_unknown", "unavailable", "needs_update"];
    "doc.set" => doc_set, DocSetParams => DocSetResult,
        "Write an opaque JSON document (at most 1 MiB) if its version still matches. Null deletes the value but retains its version. Persists atomically and emits a doc event.",
        errors ["invalid_params", "conflict", "internal", "machine_unknown", "unavailable", "needs_update"];
    "events.subscribe" => events_subscribe, SubscribeParams => SubscribeResult,
        "Snapshot of matching terminals, then every fact change in order (created, lost, restarted, keep, finished, removed, status, turn, title, labels, size, doc) as `event` notifications, `heartbeat` every 15 s, and `missed` + fresh snapshot for a subscriber that fell behind.",
        errors ["not_found", "machine_unknown"];
    "machine.add" => machine_add, MachineAddParams => crate::machines::Machine,
        "Add another machine: dial it over ssh, check that hc answers there, and keep it under a name.",
        errors ["invalid_params", "name_taken", "already_added", "unreachable", "hc_missing", "incompatible"];
    "machine.list" => machine_list, Empty => MachineListResult,
        "The machines this core has added.",
        errors [];
    "machine.connect" => machine_connect, MachineParams => crate::remote::MachineView,
        "Dial a machine now instead of at its next retry (after a login), and report its line.",
        errors ["machine_unknown"];
    "machine.rename" => machine_rename, MachineRenameParams => crate::machines::Machine,
        "Give a machine a new name here. Only this core's name for it changes.",
        errors ["invalid_params", "name_taken", "machine_unknown"];
    "machine.rm" => machine_rm, MachineParams => RemoveResult,
        "Forget a machine. Its terminals keep running there.",
        errors ["machine_unknown"];
    "rpc.discover" => rpc_discover, Empty => serde_json::Value,
        "The whole API as an OpenRPC document: every method with its params, result and errors in JSON Schema.",
        errors [];
}

fn version() -> String {
    env!("CARGO_PKG_VERSION").into()
}

fn resolve(core: &Core, address: &str) -> Result<u32, RpcError> {
    let local = crate::addr::local(address, &core.info.id, &core.info.name)?;
    core.lock().reg.resolve(local)
}

fn record(core: &Core, id: u32) -> Result<Terminal, RpcError> {
    core.lock().reg.terminals.get(&id).cloned().ok_or_else(|| {
        RpcError::new("not_found", format!("{} was removed", crate::terminals::address(id)), "see `hc ls --all`")
    })
}

async fn hello(core: Ctx<'_>, p: HelloParams) -> Result<HelloResult, RpcError> {
    if let Some(g) = p.generation.filter(|g| *g != GENERATION) {
        return Err(RpcError::new(
            "unsupported_generation",
            format!("protocol generation {g} not supported"),
            format!("this core speaks generation {GENERATION}"),
        ));
    }
    Ok(HelloResult {
        generation: GENERATION,
        version: version(),
        core: core.info.clone(),
        framed: p.framed,
        methods: METHOD_NAMES.iter().map(|m| m.to_string()).collect(),
        features: FEATURES.iter().map(|f| f.to_string()).collect(),
    })
}

async fn server_status(core: Ctx<'_>, _: Empty) -> Result<StatusResult, RpcError> {
    let st = core.lock();
    let live = st.reg.terminals.values().filter(|t| t.is_live()).count();
    let holders: Vec<HolderInfo> = st
        .holders
        .iter()
        .map(|l| HolderInfo {
            pid: l.pid,
            instance: l.instance.clone(),
            current: l.current,
            terminals: st
                .reg
                .terminals
                .values()
                .filter(|t| t.is_live() && t.holder.as_ref() == Some(&l.instance))
                .count(),
        })
        .collect();
    Ok(StatusResult {
        core: core.info.clone(),
        version: version(),
        pid: std::process::id(),
        uptime_ms: core.started.elapsed().as_millis() as u64,
        holder: holders.iter().find(|h| h.current).cloned(),
        holders,
        live,
        finished: st.reg.terminals.len() - live,
        installed_by: crate::client::installed_by().map(String::from),
    })
}

async fn server_stop(core: Ctx<'_>, _: Empty) -> Result<StopResult, RpcError> {
    if core.restart.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(RpcError::new("unavailable", "the core is restarting", "run it again once it has restarted"));
    }
    core.shutdown().await;
    Ok(StopResult { stopped: true })
}

async fn server_restart(core: Ctx<'_>, _: Empty) -> Result<StopResult, RpcError> {
    crate::restart::prepare(core).await.map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(StopResult { stopped: true })
}

async fn templates_list(core: Ctx<'_>, _: Empty) -> Result<TemplatesResult, RpcError> {
    Ok(TemplatesResult { templates: crate::templates::list(&core.paths) })
}

async fn terminal_list(core: Ctx<'_>, p: ListParams) -> Result<ListResult, RpcError> {
    // Asked from another machine: only ours. Two machines that added each
    // other would otherwise ask each other back until the timeout.
    let afar = from_afar(core);
    let (mut terminals, mut hidden_finished, machines) = {
        let mut st = core.lock();
        core.prune(&mut st);
        let matching: Vec<&Terminal> = st
            .reg
            .terminals
            .values()
            .filter(|t| p.labels.iter().all(|Label(k, v)| t.labels.get(k) == Some(v)))
            .collect();
        let hidden = if p.all { 0 } else { matching.iter().filter(|t| !t.is_live()).count() };
        let terminals: Vec<serde_json::Value> =
            matching.into_iter().filter(|t| p.all || t.is_live()).cloned().map(|t| json!(Public(t))).collect();
        let machines = if afar { Vec::new() } else { st.machines.clone() };
        (terminals, hidden, machines)
    };
    // Each machine at once, each given a moment: a slow one never holds up the list.
    let asks: Vec<_> = machines
        .iter()
        .map(|m| {
            let (core, m, params) = (core.core.clone(), m.clone(), serde_json::to_value(&p).unwrap_or_default());
            tokio::spawn(async move {
                let ask = crate::remote::forward(&core, &m, "terminal.list", params);
                tokio::time::timeout(Duration::from_secs(3), ask).await.ok().and_then(Result::ok)
            })
        })
        .collect();
    for ask in asks {
        if let Ok(Some(r)) = ask.await {
            terminals.extend(r["terminals"].as_array().cloned().unwrap_or_default());
            hidden_finished += r["hidden_finished"].as_u64().unwrap_or(0) as usize;
        }
    }
    let st = core.lock();
    let machines = machines.iter().map(|m| crate::remote::view(&st, m)).collect();
    Ok(ListResult { terminals, hidden_finished, machines })
}

async fn terminal_get(core: Ctx<'_>, p: TerminalParams) -> Result<Public, RpcError> {
    let id = resolve(core, &p.terminal)?;
    core.saw(p.caller.as_deref(), id);
    record(core, id).map(Public)
}

async fn terminal_kill(core: Ctx<'_>, p: TerminalParams) -> Result<Public, RpcError> {
    let id = resolve(core, &p.terminal)?;
    if core.lock().reg.terminals.get(&id).is_some_and(|t| t.is_live()) {
        core.lock().killing.insert(id);
        core.kill(&[id]).await;
    }
    core.saw(p.caller.as_deref(), id); // covers the exit it caused
    record(core, id).map(Public)
}

async fn terminal_rm(core: Ctx<'_>, p: TerminalParams) -> Result<RemoveResult, RpcError> {
    let id = resolve(core, &p.terminal)?;
    let mut st = core.lock();
    if st.reg.terminals.get(&id).is_some_and(|t| t.is_live()) {
        return Err(RpcError::new(
            "live",
            format!("{} is still running", crate::terminals::address(id)),
            format!("kill it first: hc kill {}", crate::terminals::address(id)),
        ));
    }
    core.remove(&mut st, id);
    Ok(RemoveResult { removed: crate::terminals::address(id) })
}

async fn terminal_keep(core: Ctx<'_>, p: KeepParams) -> Result<Public, RpcError> {
    let id = resolve(core, &p.terminal)?;
    let mut st = core.lock();
    let t = st.reg.terminals.get_mut(&id).expect("resolved");
    if !t.is_live() {
        return Err(RpcError::invalid_params("only live terminals can be kept"));
    }
    t.keep = p.keep;
    let public = Public(t.clone());
    let lost = (!p.keep && t.holder.is_none()).then(|| t.reason.clone().unwrap_or_else(|| "holder_lost".into()));
    core.emit(&mut st, "keep", id, json!({"keep": p.keep}));
    if let Some(reason) = lost {
        core.record_finish(&mut st, id, proto::Exit { code: None, signal: None }, &reason);
        core.changed.notify_waiters();
    }
    core.save(&st);
    Ok(public)
}

async fn terminal_label(core: Ctx<'_>, p: LabelParams) -> Result<Public, RpcError> {
    let id = resolve(core, &p.terminal)?;
    {
        let mut st = core.lock();
        let was = st.reg.terminals.get(&id).cloned();
        st.reg.label(id, &labels(&p.labels))?;
        core.save(&st);
        let labels = serde_json::to_value(&st.reg.terminals[&id].labels).unwrap_or_default();
        core.emit_from(&mut st, "labels", id, json!({"labels": labels}), was.as_ref());
    }
    record(core, id).map(Public)
}

async fn terminal_read(core: Ctx<'_>, p: ReadParams) -> Result<ReadResult, RpcError> {
    let id = resolve(core, &p.terminal)?;
    core.saw(p.caller.as_deref(), id);
    // A full-screen app keeps its history to itself: scroll it to read it.
    let full_screen = core.lock().screens.get(&id).is_some_and(|s| s.alt_screen());
    if full_screen && p.lines > 0 {
        let lines = p.lines.min(FINISHED_LINES);
        return Ok(match core.stitch(id, lines).await {
            Some((text, complete)) => ReadResult { text, complete, how: "stitched".into(), why: None },
            None => {
                let st = core.lock();
                let text = st.screens.get(&id).map(|s| crate::stitch::text(&s.screen_rows())).unwrap_or_default();
                let working =
                    st.reg.terminals.get(&id).and_then(|t| t.status.as_ref()).is_some_and(|s| s.state == "working");
                ReadResult { text, complete: false, how: "screen".into(), why: working.then(|| "working".into()) }
            }
        });
    }
    let st = core.lock();
    let t = st.reg.terminals.get(&id).expect("resolved");
    let lines = if t.is_live() { p.lines } else { p.lines.min(FINISHED_LINES) };
    let rows = t.rows as usize;
    let exact = t.screen == "exact";
    let text = core.screen_text(&st, id);
    let mut complete = exact && text.is_some();
    let text = text.unwrap_or_default();
    let all: Vec<&str> = text.trim_end_matches('\n').lines().collect();
    let start = all.len().saturating_sub(rows + lines);
    // Asked for more than is kept, and more was printed than is kept: some is gone.
    let kept = all.len().saturating_sub(rows).min(lines);
    if p.lines > kept && t.printed > (kept + rows) as u64 {
        complete = false;
    }
    if p.lines > 0 && !t.is_live() && t.full_screen {
        complete = false;
    }
    Ok(ReadResult {
        text: all[start..].join("\n"),
        complete,
        how: if p.lines == 0 { "screen" } else { "history" }.into(),
        why: None,
    })
}

async fn terminal_attach(core: Ctx<'_>, p: AttachParams) -> Result<AttachResult, RpcError> {
    let Some(conn) = core.conn else {
        return Err(RpcError::new(
            "needs_framed",
            "attach needs a framed connection",
            "send hello with framed: true first, or use `hc attach`",
        ));
    };
    let id = resolve(core, &p.terminal)?;
    if p.caller.as_deref().and_then(crate::terminals::parse_id) == Some(id) {
        return Err(RpcError::new(
            "refused",
            format!(
                "{} is the terminal you are in: attached to itself, it would show its own output forever",
                crate::terminals::address(id)
            ),
            "attach from another terminal",
        ));
    }
    let req = crate::viewers::AttachRequest {
        observe: p.observe,
        history: p.history,
        size: (p.cols, p.rows),
        display: p.display,
        resumed: p.resumed,
    };
    let channel = core.attach(conn, id, req)?;
    Ok(AttachResult { channel, terminal: Public(record(core, id)?) })
}

async fn terminal_send(core: Ctx<'_>, p: SendParams) -> Result<SendResult, RpcError> {
    let timeout = wait_timeout(core, p.timeout)?; // checked before anything is typed
    let id = resolve(core, &p.terminal)?;
    let text = core.tagged(p.caller.as_deref(), id, &p.text, false);
    let sent = core.send(id, &text, p.force, p.only_if.map(|w| word(&w)).as_deref()).await?;
    if let Some(child) = p.caller.as_deref().and_then(crate::terminals::parse_id) {
        let mut st = core.lock();
        if let Some(c) = st
            .reg
            .terminals
            .get_mut(&child)
            .filter(|c| c.parent.as_deref() == Some(crate::terminals::address(id).as_str()))
        {
            c.told_parent = true;
        }
    }
    if sent.status.as_deref() == Some("working") {
        let mut st = core.lock();
        let from = p
            .caller
            .as_deref()
            .and_then(crate::terminals::parse_id)
            .and_then(|c| st.reg.terminals.get(&c))
            .map(|t| t.address.clone());
        let from = from.or_else(|| p.caller.clone().filter(|c| c.contains('/'))); // a child on another machine
        st.messages.insert(id, from);
        drop(st);
        core.changed.notify_waiters();
    }
    core.saw(p.caller.as_deref(), id);
    if let Some(caller) = &p.caller {
        core.lock().last_send.insert((caller.clone(), id), sent.turn);
    }
    let wait = if p.wait && sent.started == Some(false) {
        Some(core.wait_result(id, false, "not_started", None))
    } else if p.wait {
        let spec = WaitSpec { output: None, timeout, after_turn: Some(sent.turn) };
        Some(watched(core, p.caller.clone(), id, spec).await)
    } else {
        None
    };
    Ok(SendResult { status: sent.status, turn: sent.turn, started: sent.started, wait })
}

async fn machine_exec(core: Ctx<'_>, p: ExecParams) -> Result<ExecResult, RpcError> {
    crate::exec::run(core, p, from_afar(core)).await
}

fn from_afar(core: Ctx<'_>) -> bool {
    core.conn.is_some_and(|conn| crate::remote::is_peer(core, conn))
}

async fn file_read(core: Ctx<'_>, p: FileReadParams) -> Result<FileReadResult, RpcError> {
    let afar = from_afar(core);
    blocking(move || crate::files::read(p, afar)).await
}

async fn file_write(core: Ctx<'_>, p: FileWriteParams) -> Result<FileResult, RpcError> {
    let afar = from_afar(core);
    blocking(move || crate::files::write(p, afar)).await
}

async fn file_edit(core: Ctx<'_>, p: FileEditParams) -> Result<FileResult, RpcError> {
    let afar = from_afar(core);
    blocking(move || crate::files::edit(p, afar)).await
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, RpcError> + Send + 'static) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| RpcError::internal(e.to_string()))?
}

async fn terminal_put(core: Ctx<'_>, p: PutParams) -> Result<PutResult, RpcError> {
    let too_big = || RpcError::invalid_params(format!("at most {} MB per put", crate::put::MAX_BYTES >> 20));
    if p.data.len() / 4 * 3 > crate::put::MAX_BYTES + 3 {
        return Err(too_big());
    }
    let bytes = crate::put::decode(&p.data).ok_or_else(|| RpcError::invalid_params("data is not base64"))?;
    if bytes.len() > crate::put::MAX_BYTES {
        return Err(too_big());
    }
    let id = resolve(core, &p.terminal)?;
    if !record(core, id)?.is_live() {
        return Err(RpcError::new(
            "finished",
            format!("{} has finished", crate::terminals::address(id)),
            "nothing there to read it",
        ));
    }
    let path = crate::put::stage(&core.paths.state, id, &p.name, &bytes)
        .map_err(|e| RpcError::internal(format!("cannot write the file: {e}")))?;
    Ok(PutResult { path: path.display().to_string(), bytes: bytes.len() })
}

async fn terminal_keys(core: Ctx<'_>, p: KeysParams) -> Result<KeysResult, RpcError> {
    let id = resolve(core, &p.terminal)?;
    let (status, turn) = core.keys(id, &p.keys).await?;
    core.saw(p.caller.as_deref(), id);
    Ok(KeysResult { status, turn })
}

async fn terminal_interrupt(core: Ctx<'_>, p: InterruptParams) -> Result<InterruptResult, RpcError> {
    use crate::input::Interrupted;
    let timeout = seconds(p.timeout)?.unwrap_or(Duration::from_secs(10));
    let id = resolve(core, &p.terminal)?;
    let (outcome, key) = core.interrupt(id, timeout).await?;
    let outcome = match outcome {
        Interrupted::Stopped => "stopped",
        Interrupted::Exited => "exited",
        Interrupted::StillWorking => "still_working",
        Interrupted::Unknown => "unknown",
    };
    Ok(InterruptResult { outcome: outcome.into(), key })
}

async fn events_subscribe(core: Ctx<'_>, p: SubscribeParams) -> Result<SubscribeResult, RpcError> {
    use crate::events::{Filter, Sink};
    // A terminal on a machine is named as it is here; one here, by its id.
    let address = |t: &str| match t.split_once('/').filter(|(m, _)| core.lock().machines.iter().any(|x| x.name == *m)) {
        Some(_) => Ok(t.to_string()),
        None => resolve(core, t).map(crate::terminals::address),
    };
    let terminal = p.terminal.as_deref().map(address).transpose()?;
    let children_of = p.children_of.as_deref().map(address).transpose()?;
    let filter = Filter { terminal, children_of, labels: labels(&p.labels), here_only: from_afar(core) };
    let (sink, channel) = match (core.conn, core.plain) {
        (Some(conn), _) => {
            let channel = conn.new_channel();
            conn.with(|out| out.open(channel));
            (Sink::Framed { conn: conn.clone(), channel }, Some(channel))
        }
        (None, Some(slot)) => {
            let (tx, rx) = tokio::sync::mpsc::channel(crate::events::PLAIN_QUEUE);
            let behind = Arc::new(std::sync::atomic::AtomicBool::new(false));
            *slot.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(PlainSub { rx, behind: behind.clone(), filter: filter.clone() });
            (Sink::Plain { tx, behind }, None)
        }
        (None, None) => return Err(RpcError::internal("no connection to deliver events on")),
    };
    let (seq, terminals) = {
        let mut st = core.lock();
        let terminals = core.matching(&st, &filter);
        core.subscribe(&mut st, filter, sink);
        (st.events.seq, terminals)
    };
    if let Some(channel) = channel {
        crate::server::heartbeat(core.core.clone(), core.conn.expect("framed").clone(), channel);
    }
    Ok(SubscribeResult { seq, terminals, channel })
}

/// A machine name that is free: valid, not another machine's, not this core's own.
fn free_name(core: &Core, st: &crate::runtime::State, name: &str) -> Result<(), RpcError> {
    crate::addr::check_name(name)?;
    if name == core.info.name || name == core.info.id || st.machines.iter().any(|m| m.name == name) {
        return Err(RpcError::new(
            "name_taken",
            format!("{name:?} is already a machine here"),
            "pick another with --name",
        ));
    }
    Ok(())
}

async fn machine_add(core: Ctx<'_>, p: MachineAddParams) -> Result<crate::machines::Machine, RpcError> {
    let (target, via) = match (p.target, p.via) {
        (Some(target), None) => (target, false),
        (None, Some(command)) => (command, true),
        _ => return Err(RpcError::invalid_params("give either target or via")),
    };
    let to = crate::machines::Reach { target: &target, via };
    crate::machines::check_target(to)?;
    if let Some(session) = &p.session {
        crate::addr::check_name(session)?;
    }
    let name = |st: &crate::runtime::State| match &p.name {
        Some(name) => free_name(core, st, name).map(|_| name.clone()),
        None => (!via)
            .then(|| crate::machines::default_name(&target, |name| free_name(core, st, name).is_err()))
            .flatten()
            .ok_or_else(|| {
                RpcError::invalid_params(format!("can't make a name from {target:?}")).with_hint("give one with --name")
            }),
    };
    name(&core.lock())?;
    let hello = crate::machines::hello(&core.paths, to, p.session.as_deref()).await?;
    let text = |k: &str| hello["core"][k].as_str().unwrap_or_default().to_string();
    let mut st = core.lock();
    let machine = crate::machines::Machine {
        name: name(&st)?,
        target,
        via,
        temporary: p.temporary,
        session: p.session,
        id: text("id"),
        host: text("name"),
        version: hello["version"].as_str().unwrap_or_default().into(),
    };
    if machine.id == core.info.id {
        return Err(RpcError::invalid_params(format!("{} is this core", machine.target))
            .with_hint("add another machine, or another --session"));
    }
    if let Some(m) = st.machines.iter().find(|m| m.id == machine.id) {
        // Reached another way yet the same core: a copy of that machine's disk
        // whose system kept its machine id (some container images).
        if (m.target.as_str(), m.via) != (machine.target.as_str(), machine.via) {
            return Err(RpcError::new(
                "already_added",
                format!("{} answers as {}'s core: it looks like a copy of {}", machine.target, m.name, m.name),
                "on the copy, stop hc and delete its core.json in hc's state folder, then add it again",
            ));
        }
        return Err(RpcError::new(
            "already_added",
            format!("that core is already added as {}", m.name),
            format!("rename it: hc machine rename {} <name>", m.name),
        ));
    }
    st.machines.push(machine.clone());
    crate::machines::save(&core.paths.state, &st.machines);
    drop(st);
    crate::remote::keep_up(core.core, machine.id.clone());
    Ok(machine)
}

async fn machine_list(core: Ctx<'_>, _: Empty) -> Result<MachineListResult, RpcError> {
    let st = core.lock();
    Ok(MachineListResult { machines: st.machines.iter().map(|m| crate::remote::view(&st, m)).collect() })
}

async fn machine_connect(core: Ctx<'_>, p: MachineParams) -> Result<crate::remote::MachineView, RpcError> {
    let m = {
        let st = core.lock();
        st.machines
            .iter()
            .find(|m| m.name == p.machine)
            .cloned()
            .ok_or_else(|| crate::machines::unknown(&st.machines, &p.machine))?
    };
    let connected = matches!(core.lock().lines.get(&m.id), Some(crate::remote::LineState::Connected(_)));
    if !connected {
        crate::remote::keep_up(core.core, m.id.clone());
        crate::remote::dialed(core, &m.id).await;
    }
    Ok(crate::remote::view(&core.lock(), &m))
}

async fn machine_rename(core: Ctx<'_>, p: MachineRenameParams) -> Result<crate::machines::Machine, RpcError> {
    let mut st = core.lock();
    let i = st
        .machines
        .iter()
        .position(|m| m.name == p.machine)
        .ok_or_else(|| crate::machines::unknown(&st.machines, &p.machine))?;
    if p.new_name != p.machine {
        free_name(core, &st, &p.new_name)?;
    }
    let id = st.machines[i].id.clone();
    st.machines[i].name = p.new_name.clone();
    if p.new_name != p.machine {
        crate::remote::rename_mirror(core, &mut st, &id, &p.machine, &p.new_name);
    }
    crate::machines::save(&core.paths.state, &st.machines);
    Ok(st.machines[i].clone())
}

async fn machine_rm(core: Ctx<'_>, p: MachineParams) -> Result<RemoveResult, RpcError> {
    let mut st = core.lock();
    let i = st
        .machines
        .iter()
        .position(|m| m.name == p.machine)
        .ok_or_else(|| crate::machines::unknown(&st.machines, &p.machine))?;
    let gone = st.machines.remove(i);
    crate::remote::forget(core, &mut st, &gone.id);
    crate::machines::save(&core.paths.state, &st.machines);
    let last_on_target = !st.machines.iter().any(|m| m.target == gone.target);
    drop(st);
    if last_on_target {
        crate::machines::logout(&core.paths, gone.reach());
    }
    Ok(RemoveResult { removed: gone.name })
}

async fn rpc_discover(_: Ctx<'_>, _: Empty) -> Result<serde_json::Value, RpcError> {
    Ok(openrpc())
}
