//! `terminal.run`: start a program or template, and with `wait` see it through like a shell.

use super::*;

pub(super) async fn terminal_run(core: Ctx<'_>, mut p: RunParams) -> Result<RunResult, RpcError> {
    // A template is found only by template lookup, never by terminal name.
    let template = match p.argv.first() {
        Some(name) => crate::templates::load(&core.paths, name)?,
        None => None,
    };
    let first = template.as_ref().map(|t| crate::templates::first_message(t, &p.argv[1..].join(" ")));
    match (&template, p.line.take(), p.argv.is_empty()) {
        // A shell line (VAR=value prefixes, quotes, ~); sh replaces itself with a single command.
        (Some(t), None, _) => p.argv = vec!["sh".into(), "-c".into(), t.command.clone()],
        // The shell is this machine's: the caller may be on another kind.
        (None, Some(line), true) => p.argv = crate::platform::shell_line(&line),
        (None, None, true) => p.argv = crate::config::load(&core.paths.config)?.shell()?,
        (None, None, false) => {}
        _ => return Err(RpcError::invalid_params("give argv or line, not both")),
    }
    let quiet_ready = template.as_ref().is_some_and(|t| t.ready_when_quiet);
    let caller = p.caller.clone();
    let timeout = if p.wait { Some(wait_timeout(core, p.timeout)?) } else { None }; // checked before it starts
    if p.wait {
        // A pager in a real terminal waits for a person: a waited run would hang on it.
        for pager in ["PAGER", "GIT_PAGER", "GH_PAGER"] {
            p.env.insert(pager.into(), "cat".into());
        }
    }
    let started = start(core, p, template).await?;
    let id = started.id();
    let first_message = match first.filter(|text| !text.is_empty()) {
        Some(text) => Some(core.first_message(id, quiet_ready, &text, caller.as_deref()).await),
        None => None,
    };
    let (wait, output) = match timeout {
        None => (None, None),
        Some(timeout) => {
            // A template's task: wait for the turn it started, not the idle before it.
            let after_turn = first_message.as_ref().and_then(|f| f.turn);
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
    };
    Ok(RunResult { terminal: Public(started), first_message, wait, output })
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

pub(super) async fn start(
    core: &Arc<Core>,
    p: RunParams,
    template: Option<crate::templates::Template>,
) -> Result<Terminal, RpcError> {
    not_stopping(&core.lock())?;
    core.ensure_holder().await.map_err(|e| {
        RpcError::new(
            "unavailable",
            format!("no holder: {e}"),
            "run hc server restart on this machine, then retry; see holder.log and core.log in the state directory",
        )
    })?;
    let settings = crate::config::load(&core.paths.config)?;
    let (cols, rows) = settings.headless()?;
    // A caller on another machine has no folder here: home, then.
    let from_afar = p.caller.as_deref().is_some_and(|c| c.contains('/'));
    let cwd = crate::exec::folder(p.cwd.as_deref(), from_afar);
    let env = child_env(crate::platform::program_env().await, p.env.clone());
    let identity = format!(
        "{} {}",
        env.get("TERM_PROGRAM").map_or("", String::as_str),
        env.get("TERM_PROGRAM_VERSION").map_or("", String::as_str)
    )
    .trim()
    .to_string();
    // Set once, at start: an explicit parent, else the calling terminal.
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
    let (id, rx) = {
        let mut st = core.lock();
        not_stopping(&st)?;
        let instance = st
            .current()
            .ok_or_else(|| {
                RpcError::new(
                    "unavailable",
                    "no holder: the holder disconnected before the terminal could start",
                    "retry; if it keeps failing, run hc server restart on this machine and check holder.log",
                )
            })?
            .instance
            .clone();
        let new = NewTerminal {
            template,
            die_with_parent: parent.is_some() && !p.outlive_parent,
            parent,
            notify: !p.no_notify,
            rm: p.rm,
            keep: p.keep,
            name: p.name,
            labels: labels(&p.labels),
            argv: p.argv.clone(),
            cwd: cwd.clone(),
            cols,
            rows,
            identity: identity.clone(),
        };
        let id = st.reg.create(new, now_ms())?;
        let mut screen = ghostty_vt::Terminal::new(cols, rows, settings.history_bytes());
        screen.set_xtversion(&identity);
        st.screens.insert(id, screen);
        let (env, scope) = terminal_env(core, id, &cwd, env);
        st.reg.terminals.get_mut(&id).expect("just created").scope = Some(scope);
        let (tx, rx) = tokio::sync::oneshot::channel();
        st.pending.insert(id, tx);
        st.reg.terminals.get_mut(&id).expect("just created").restart_env = p.env;
        let spawn = proto::Spawn { argv: p.argv, env, cwd, cols, rows };
        st.reg.terminals.get_mut(&id).expect("just created").holder = Some(instance);
        core.save(&st);
        st.link(id).expect("assigned holder").send(&proto::json_frame(proto::SPAWN, id, &spawn));
        (id, rx)
    };
    let spawn_ms = std::env::var("HC_SPAWN_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let spawned = tokio::time::timeout(Duration::from_millis(spawn_ms), rx).await;
    let mut st = core.lock();
    match spawned {
        // As it was when it started: with --rm it may already be gone.
        Ok(Ok(Ok(started))) => Ok(started),
        other => {
            st.pending.remove(&id);
            let keep = st.reg.terminals.get(&id).is_some_and(|t| {
                t.keep && t.is_live() && (t.reason.is_some() || t.pid.is_some() || !matches!(&other, Ok(Ok(Err(_)))))
            });
            if keep {
                let t = st.reg.terminals.get_mut(&id).expect("kept live record");
                if t.pid.is_none() && t.reason.is_none() {
                    t.reason = Some("spawn_pending".into());
                    core.save(&st);
                }
            } else {
                st.screens.remove(&id);
                core.remove(&mut st, id);
            }
            let error = match other {
                Ok(Ok(Err(e))) => e,
                _ => "the holder did not answer".into(),
            };
            Err(RpcError::new("spawn_failed", error, "check the program name and cwd"))
        }
    }
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

/// The core's environment minus whoever started the core's identity, plus the
/// caller's identity and this terminal's address.
pub(crate) fn child_env(base: BTreeMap<String, String>, extra: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = base
        .into_iter()
        .filter(|(k, _)| {
            !IDENTITY_VARS.contains(&k.as_str())
                && !["HC_TERMINAL_ID", crate::sweep::VAR, crate::sweep::RUNTIME].contains(&k.as_str())
        })
        .collect();
    crate::platform::merge_env(&mut env, extra);
    if let Ok(session) = std::env::var("HC_SESSION") {
        env.insert("HC_SESSION".into(), session);
    }
    if let Some(lang) = crate::locale::for_programs(&env) {
        env.insert("LANG".into(), lang); // a core started from Finder has none
    }
    env.entry("TERM".into()).or_insert_with(|| "xterm-256color".into());
    env.entry("COLORTERM".into()).or_insert_with(|| "truecolor".into());
    if env.get("TERM_PROGRAM").is_none_or(|p| p == "herdr") {
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
