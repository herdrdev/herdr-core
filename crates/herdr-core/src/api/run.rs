//! `terminal.create`, `terminal.start`, and `terminal.run`: allocate a terminal,
//! idempotently start a program or template, and with `wait` see it through.

use super::*;
use crate::runtime::{StartOutcome, StartWaiter};
use crate::terminals::{ClaimOutcome, LaunchSpec};

struct PreparedCommand {
    template: Option<crate::templates::Template>,
    first: Option<String>,
    quiet_ready: bool,
    argv: Vec<String>,
    restart_env: BTreeMap<String, String>,
    spawn_env: BTreeMap<String, String>,
    spec: LaunchSpec,
}

struct PreparedEmpty {
    new: NewTerminal,
    display: Option<crate::viewers::Display>,
    history_bytes: usize,
}

fn env_identity(env: &BTreeMap<String, String>) -> String {
    format!(
        "{} {}",
        env.get("TERM_PROGRAM").map_or("", String::as_str),
        env.get("TERM_PROGRAM_VERSION").map_or("", String::as_str)
    )
    .trim()
    .to_string()
}

fn prepare_command(
    core: &Core,
    argv: Vec<String>,
    line: Option<String>,
    env: BTreeMap<String, String>,
    wait: bool,
) -> Result<PreparedCommand, RpcError> {
    // A template is found only by template lookup, never by terminal name.
    let template = match argv.first() {
        Some(name) => crate::templates::load(&core.paths, name)?,
        None => None,
    };
    let first = template
        .as_ref()
        .map(|t| crate::templates::first_message(t, &argv[1..].join(" ")))
        .filter(|text| !text.is_empty());
    let resolved_argv = match (&template, line, argv.is_empty()) {
        // A shell line (VAR=value prefixes, quotes, ~); sh replaces itself with a single command.
        (Some(t), None, _) => vec!["sh".into(), "-c".into(), t.command.clone()],
        // The shell is this machine's: the caller may be on another kind.
        (None, Some(line), true) => crate::platform::shell_line(&line),
        (None, None, true) => crate::config::load(&core.paths.config)?.shell()?,
        (None, None, false) => argv,
        _ => return Err(RpcError::invalid_params("give argv or line, not both")),
    };
    let quiet_ready = template.as_ref().is_some_and(|t| t.ready_when_quiet);
    // Exclude PAGER overrides so retrying `--bg` vs waited `terminal.start` matches.
    let spec = LaunchSpec { argv: resolved_argv.clone(), env: env.clone(), first: first.clone() };
    let mut spawn_env = env;
    if wait {
        // A pager in a real terminal waits for a person: a waited run would hang on it.
        for pager in ["PAGER", "GIT_PAGER", "GH_PAGER"] {
            spawn_env.insert(pager.into(), "cat".into());
        }
    }
    Ok(PreparedCommand {
        template,
        first,
        quiet_ready,
        argv: resolved_argv,
        restart_env: spec.env.clone(),
        spawn_env,
        spec,
    })
}

fn no_holder(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        "unavailable",
        format!("no holder: {e}"),
        "run hc server restart on this machine, then retry; see holder.log and core.log in the state directory",
    )
}

fn holder_disconnected() -> RpcError {
    RpcError::new(
        "unavailable",
        "no holder: the holder disconnected before the terminal could start",
        "retry; if it keeps failing, run hc server restart on this machine and check holder.log",
    )
}

async fn prepare_empty(core: &Core, p: CreateParams) -> Result<(PreparedEmpty, BTreeMap<String, String>), RpcError> {
    not_stopping(&core.lock())?;
    let settings = crate::config::load(&core.paths.config)?;
    let (cols, rows) = match p.size {
        Some(size) if size.cols > 0 && size.rows > 0 => (size.cols, size.rows),
        Some(_) => return Err(RpcError::invalid_params("size.cols and size.rows must be at least 1")),
        None => settings.headless()?,
    };
    // A caller on another machine has no folder here: home, then.
    let from_afar = p.caller.as_deref().is_some_and(|c| c.contains('/'));
    let cwd = crate::exec::folder(p.cwd.as_deref(), from_afar);
    let base_env = crate::platform::program_env().await;
    let env = child_env(base_env.clone(), BTreeMap::new());
    let identity = env_identity(&env);
    // Set once, at creation: an explicit parent, else the calling terminal.
    let parent = match (&p.parent, &p.caller) {
        (Some(parent), _) => Some(crate::terminals::address(resolve(core, parent)?)),
        // Started from a terminal on the machine that dialed in: `desktop/t3`.
        (None, Some(caller)) if !p.no_parent && from_afar => {
            caller.split_once('/').and_then(|(_, t)| crate::terminals::parse_id(t)).map(|_| caller.clone())
        }
        (None, Some(caller)) if !p.no_parent => crate::terminals::parse_id(caller)
            .filter(|id| core.lock().reg.terminals.contains_key(id))
            .map(crate::terminals::address),
        _ => None,
    };
    Ok((
        PreparedEmpty {
            new: NewTerminal {
                template: None,
                die_with_parent: parent.is_some() && !p.outlive_parent,
                parent,
                notify: !p.no_notify,
                rm: p.rm,
                keep: p.keep,
                name: p.name,
                labels: labels(&p.labels),
                argv: Vec::new(),
                cwd,
                cols,
                rows,
                identity,
            },
            display: p.display,
            history_bytes: settings.history_bytes(),
        },
        base_env,
    ))
}

fn allocate_empty_locked(st: &mut crate::runtime::State, empty: PreparedEmpty) -> Result<u32, RpcError> {
    let (cols, rows, identity) = (empty.new.cols, empty.new.rows, empty.new.identity.clone());
    let id = st.reg.create_empty(empty.new, now_ms())?;
    let mut screen = ghostty_vt::Terminal::new(cols, rows, empty.history_bytes);
    screen.set_xtversion(&identity);
    if let Some(display) = &empty.display {
        display.apply(&mut screen);
    }
    st.screens.insert(id, screen);
    Ok(id)
}

pub(super) async fn terminal_create(core: Ctx<'_>, p: CreateParams) -> Result<Public, RpcError> {
    let (prepared, _) = prepare_empty(core, p).await?;
    let public = {
        let mut st = core.lock();
        not_stopping(&st)?;
        let id = allocate_empty_locked(&mut st, prepared)?;
        let public = Public(st.reg.terminals.get(&id).expect("just created").clone());
        core.emit(&mut st, "created", id, serde_json::to_value(&public).unwrap_or_default());
        core.save(&st);
        public
    };
    core.flush_saved().await?;
    Ok(public)
}

pub(super) async fn terminal_start(core: Ctx<'_>, p: StartParams) -> Result<RunResult, RpcError> {
    let timeout = if p.wait { Some(wait_timeout(core, p.timeout)?) } else { None };
    let prep = prepare_command(core, p.argv, p.line, p.env, p.wait)?;
    let id = resolve(core, &p.terminal)?;
    let caller = p.caller.clone();
    enum Next {
        Wait(StartWaiter),
        Ready(Box<StartOutcome>),
    }
    let next = {
        let mut st = core.lock();
        match st.reg.claim_start(id, prep.spec.clone(), prep.template.clone(), prep.restart_env.clone())? {
            ClaimOutcome::Claimed => Next::Wait(spawn_start_task(core, &mut st, id, prep, caller.clone())),
            ClaimOutcome::IdempotentReplay => {
                if let Some((_, rx)) = st.starts.get(&id).cloned() {
                    Next::Wait(rx)
                } else {
                    let mut t = st.reg.terminals.get(&id).cloned().ok_or_else(|| {
                        RpcError::new(
                            "not_found",
                            format!("{} was removed", crate::terminals::address(id)),
                            "see `hc ls --all`",
                        )
                    })?;
                    if t.is_live() && t.reason.as_deref() == Some("spawn_pending") {
                        return Err(RpcError::new(
                            "spawn_failed",
                            "the holder did not answer",
                            "check the program name and cwd",
                        ));
                    }
                    let first_message = t.sent_first_message.clone().or_else(|| t.first_message.take());
                    Next::Ready(Box::new(StartOutcome { started: t, first_message }))
                }
            }
        }
    };
    let outcome = match next {
        Next::Wait(rx) => await_start(rx).await?,
        Next::Ready(outcome) => *outcome,
    };
    finish_start(core, caller, id, timeout, outcome).await
}

pub(super) async fn terminal_run(core: Ctx<'_>, p: RunParams) -> Result<RunResult, RpcError> {
    let timeout = if p.wait { Some(wait_timeout(core, p.timeout)?) } else { None }; // checked before it starts
    let prep = prepare_command(core, p.argv, p.line, p.env, p.wait)?;
    let caller = p.create.caller.clone();
    not_stopping(&core.lock())?;
    core.ensure_holder().await.map_err(no_holder)?;
    let (empty, base_env) = prepare_empty(core, p.create).await?;
    let rx = {
        let mut st = core.lock();
        not_stopping(&st)?;
        if st.current().is_none() {
            return Err(holder_disconnected());
        }
        let id_created = allocate_empty_locked(&mut st, empty)?;
        st.reg.claim_start(id_created, prep.spec.clone(), prep.template.clone(), prep.restart_env.clone())?;
        let env = child_env(base_env, prep.spawn_env);
        let spawn_rx = send_spawn_locked(core, &mut st, id_created, prep.argv, env)?;
        let (seq, tx, rx) = register_start(&mut st, id_created);
        let meta = SpawnMeta {
            seq,
            first: prep.first,
            quiet_ready: prep.quiet_ready,
            caller: caller.clone(),
            ephemeral_on_fail: true,
        };
        tokio::spawn({
            let core = core.core.clone();
            async move {
                let outcome = finish_spawn(core.clone(), id_created, spawn_rx, meta).await;
                clear_start(&mut core.lock(), id_created, seq);
                let _ = tx.send(Some(outcome));
            }
        });
        (id_created, rx)
    };
    let (id, rx) = rx;
    let outcome = await_start(rx).await?;
    finish_start(core, caller, id, timeout, outcome).await
}

async fn finish_start(
    core: Ctx<'_>,
    caller: Option<String>,
    id: u32,
    timeout: Option<Duration>,
    outcome: StartOutcome,
) -> Result<RunResult, RpcError> {
    let (wait, output) = wait_after_start(core, caller, id, timeout, outcome.first_message.as_ref()).await;
    core.flush_saved().await?;
    Ok(RunResult { terminal: Public(outcome.started), first_message: outcome.first_message, wait, output })
}

async fn wait_after_start(
    core: Ctx<'_>,
    caller: Option<String>,
    id: u32,
    timeout: Option<Duration>,
    first_message: Option<&crate::templates::FirstMessage>,
) -> (Option<WaitResult>, Option<String>) {
    let Some(timeout) = timeout else { return (None, None) };
    // A template's task: wait for the turn it started, not the idle before it.
    let after_turn = first_message.and_then(|f| f.turn);
    let r = watch(core, caller.clone(), id, WaitSpec { output: None, timeout, after_turn }).await;
    let full_screen = core.lock().screens.get(&id).is_some_and(|s| s.alt_screen());
    let read = ReadParams {
        terminal: crate::terminals::address(id),
        caller: None,
        lines: if full_screen { 0 } else { FINISHED_LINES },
    };
    let text = tail(terminal_read(core, read).await.map(|r| r.text).unwrap_or_default(), id);
    has_seen(core, caller.as_deref(), id);
    (Some(r), Some(text))
}

/// Output a run hands back: at most `RUN_OUTPUT` bytes, whole lines from the
/// end (errors are at the end), saying where the rest is.
pub(crate) fn tail(text: String, id: u32) -> String {
    const RUN_OUTPUT: usize = 50_000;
    if text.len() <= RUN_OUTPUT {
        return text;
    }
    let mut from = text.len() - RUN_OUTPUT;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let from = text[from..].find('\n').map_or(from, |at| from + at + 1);
    format!(
        "[hc] earlier output cut: hc read {} --lines {FINISHED_LINES}\n{}",
        crate::terminals::address(id),
        &text[from..]
    )
}

/// A stopping core starts nothing: whatever it started now would outlive the stop.
pub(crate) fn not_stopping(st: &crate::runtime::State) -> Result<(), RpcError> {
    match st.stopping {
        true => Err(RpcError::new("unavailable", "the core is stopping", "run it again once it has stopped")),
        false => Ok(()),
    }
}

fn clear_start(st: &mut crate::runtime::State, id: u32, seq: u64) {
    if st.starts.get(&id).is_some_and(|(s, _)| *s == seq) {
        st.starts.remove(&id);
    }
}

fn abort_unstarted_locked(core: &Core, st: &mut crate::runtime::State, id: u32, seq: u64) {
    clear_start(st, id, seq);
    st.reg.release_unstarted(id);
    core.save(st);
}

fn abort_unstarted(core: &Core, id: u32, seq: u64) {
    let mut st = core.lock();
    abort_unstarted_locked(core, &mut st, id, seq);
}

struct SpawnMeta {
    seq: u64,
    first: Option<String>,
    quiet_ready: bool,
    caller: Option<String>,
    ephemeral_on_fail: bool,
}

type StartSender = tokio::sync::watch::Sender<Option<Result<StartOutcome, RpcError>>>;

fn register_start(st: &mut crate::runtime::State, id: u32) -> (u64, StartSender, StartWaiter) {
    st.starts_seq += 1;
    let seq = st.starts_seq;
    let (tx, rx) = tokio::sync::watch::channel(None);
    st.starts.insert(id, (seq, rx.clone()));
    (seq, tx, rx)
}

fn spawn_start_task(
    core: &Arc<Core>,
    st: &mut crate::runtime::State,
    id: u32,
    prep: PreparedCommand,
    caller: Option<String>,
) -> StartWaiter {
    let (seq, tx, rx) = register_start(st, id);
    let core = core.clone();
    tokio::spawn(async move {
        let outcome = drive_start(core.clone(), id, seq, prep, caller).await;
        clear_start(&mut core.lock(), id, seq);
        let _ = tx.send(Some(outcome));
    });
    rx
}

async fn await_start(mut rx: StartWaiter) -> Result<StartOutcome, RpcError> {
    let done = rx.wait_for(|v| v.is_some()).await.map_err(|_| RpcError::internal("start task stopped"))?;
    done.as_ref().expect("populated").clone()
}

fn send_spawn_locked(
    core: &Core,
    st: &mut crate::runtime::State,
    id: u32,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
) -> Result<tokio::sync::oneshot::Receiver<Result<Terminal, String>>, RpcError> {
    not_stopping(st)?;
    let instance = st.current().ok_or_else(holder_disconnected)?.instance.clone();
    let Some((cwd, cols, rows)) =
        st.reg.terminals.get(&id).filter(|t| t.is_live()).map(|t| (t.cwd.clone(), t.cols, t.rows))
    else {
        return Err(RpcError::new(
            "not_found",
            format!("{} was removed", crate::terminals::address(id)),
            "see `hc ls --all`",
        ));
    };
    let identity = env_identity(&env);
    if let Some(screen) = st.screens.get_mut(&id) {
        screen.set_xtversion(&identity);
    }
    let (env, scope) = terminal_env(core, id, &cwd, env);
    let t = st.reg.terminals.get_mut(&id).expect("live terminal");
    t.identity = identity;
    t.scope = Some(scope);
    t.holder = Some(instance);
    let (tx, rx) = tokio::sync::oneshot::channel();
    st.pending.insert(id, tx);
    let spawn = proto::Spawn { argv, env, cwd, cols, rows };
    // Queue the snapshot before spawning; the next start kills any unrecorded spawn.
    core.save(st);
    st.link(id).expect("assigned holder").send(&proto::json_frame(proto::SPAWN, id, &spawn));
    Ok(rx)
}

async fn drive_start(
    core: Arc<Core>,
    id: u32,
    seq: u64,
    prep: PreparedCommand,
    caller: Option<String>,
) -> Result<StartOutcome, RpcError> {
    if let Err(e) = not_stopping(&core.lock()) {
        abort_unstarted(&core, id, seq);
        return Err(e);
    }
    if let Err(e) = core.ensure_holder().await {
        abort_unstarted(&core, id, seq);
        return Err(no_holder(e));
    }
    let base_env = crate::platform::program_env().await;
    let env = child_env(base_env, prep.spawn_env);
    let rx = {
        let mut st = core.lock();
        match send_spawn_locked(&core, &mut st, id, prep.argv, env) {
            Ok(rx) => rx,
            Err(e) => {
                abort_unstarted_locked(&core, &mut st, id, seq);
                return Err(e);
            }
        }
    };
    let meta = SpawnMeta { seq, first: prep.first, quiet_ready: prep.quiet_ready, caller, ephemeral_on_fail: false };
    finish_spawn(core, id, rx, meta).await
}

async fn finish_spawn(
    core: Arc<Core>,
    id: u32,
    rx: tokio::sync::oneshot::Receiver<Result<Terminal, String>>,
    meta: SpawnMeta,
) -> Result<StartOutcome, RpcError> {
    let SpawnMeta { seq, first, quiet_ready, caller, ephemeral_on_fail } = meta;
    let spawn_ms = std::env::var("HC_SPAWN_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let spawned = tokio::time::timeout(Duration::from_millis(spawn_ms), rx).await;
    let started = {
        let mut st = core.lock();
        match spawned {
            // As it was when it started: with --rm it may already be gone.
            Ok(Ok(Ok(started))) => started,
            other => {
                st.pending.remove(&id);
                let is_sync_err = matches!(&other, Ok(Ok(Err(_))));
                let alive = st.reg.terminals.get(&id).is_some_and(|t| t.is_live());
                let preserve_pending = st.reg.terminals.get(&id).is_some_and(|t| {
                    t.is_live()
                        && (t.reason.is_some() || t.pid.is_some() || (!is_sync_err && (!ephemeral_on_fail || t.keep)))
                });
                if !alive && ephemeral_on_fail {
                    st.screens.remove(&id);
                    core.remove(&mut st, id);
                } else if preserve_pending {
                    let t = st.reg.terminals.get_mut(&id).expect("live record");
                    if t.pid.is_none() && t.reason.is_none() {
                        t.reason = Some("spawn_pending".into());
                        core.save(&st);
                    }
                } else if ephemeral_on_fail {
                    st.screens.remove(&id);
                    core.remove(&mut st, id);
                } else if is_sync_err {
                    st.reg.release_unstarted(id);
                    core.save(&st);
                }
                clear_start(&mut st, id, seq);
                let error = match other {
                    Ok(Ok(Err(e))) => e,
                    _ => "the holder did not answer".into(),
                };
                return Err(RpcError::new("spawn_failed", error, "check the program name and cwd"));
            }
        }
    };
    let first_message = match first.filter(|text| !text.is_empty()) {
        Some(text) => Some(core.first_message(id, quiet_ready, &text, caller.as_deref()).await),
        None => None,
    };
    {
        let mut st = core.lock();
        if let Some(ref fm) = first_message
            && let Some(t) = st.reg.terminals.get_mut(&id)
        {
            if fm.sent {
                t.sent_first_message = Some(fm.clone());
            } else {
                t.first_message = Some(fm.clone());
            }
            core.save(&st);
        }
        clear_start(&mut st, id, seq);
    }
    Ok(StartOutcome { started, first_message })
}

/// Per-program runtime fields, shared by initial runs and replacement shells.
pub(crate) fn terminal_env(
    core: &Core,
    id: u32,
    cwd: &str,
    mut env: BTreeMap<String, String>,
) -> (BTreeMap<String, String>, String) {
    env.insert("HC_TERMINAL_ID".into(), crate::terminals::address(id));
    env.insert("HC_TERMINAL_SOCKET".into(), core.paths.socket.display().to_string());
    let scope = crate::sweep::token();
    env.insert(crate::sweep::VAR.into(), scope.clone());
    // Preserve the requested spelling, including symlinks.
    env.insert("PWD".into(), cwd.into());
    (env, scope)
}

/// The core's environment with its own terminal identity and explicit client overrides.
pub(crate) fn child_env(base: BTreeMap<String, String>, extra: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = base
        .into_iter()
        .filter(|(k, _)| {
            !IDENTITY_VARS.contains(&k.as_str())
                && !["HC_TERMINAL_ID", crate::sweep::VAR, crate::sweep::RUNTIME].contains(&k.as_str())
        })
        .collect();
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env.insert("TERM_PROGRAM".into(), "ghostty".into());
    crate::platform::merge_env(&mut env, extra);
    if let Ok(session) = std::env::var("HC_SESSION") {
        env.insert("HC_SESSION".into(), session);
    }
    if let Some(lang) = crate::locale::for_programs(&env) {
        env.insert("LANG".into(), lang); // a core started from Finder has none
    }
    if env.get("TERM_PROGRAM").is_some_and(|p| p == "herdr") {
        env.insert("TERM_PROGRAM".into(), "ghostty".into());
        env.remove("TERM_PROGRAM_VERSION"); // the version belonged to whatever we replaced
    }
    env
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_long_output_keeps_whole_lines_from_its_end_even_mid_character() {
        let text = "é".repeat(30_000) + "\nlast line";
        let cut = super::tail(text, 7);
        assert!(cut.starts_with("[hc] earlier output cut: hc read t7") && cut.ends_with("\nlast line"));
        assert_eq!(super::tail("short".into(), 7), "short");
    }
}
