//! Waiting: on one terminal, or on children here and on other machines.

use super::*;

/// The given seconds, else config `wait_timeout`, else 180.
pub(super) fn wait_timeout(core: &Core, given: Option<f64>) -> Result<Duration, RpcError> {
    let secs = match given {
        Some(s) => s,
        None => crate::config::delivery(&core.paths.config).wait_timeout.unwrap_or(180.0),
    };
    Ok(seconds(Some(secs))?.unwrap_or_default())
}

pub(super) async fn terminal_wait(core: Ctx<'_>, p: WaitParams) -> Result<WaitResult, RpcError> {
    let timeout = wait_timeout(core, p.timeout)?;
    if let Some(mode) = p.children {
        let result = wait_children_everywhere(core, &p, &word(&mode), timeout).await;
        core.flush_saved().await?;
        return result;
    }
    let terminal = p.terminal.as_deref().ok_or_else(|| RpcError::invalid_params("give a terminal, or children"))?;
    let id = resolve(core, terminal)?;
    let bare = p.output.is_none();
    // The turn the caller's last send started: kept through timeouts (a retry
    // still waits for it), dropped once a wait ends unless a newer send replaced it.
    let key = p.caller.clone().filter(|_| bare).map(|c| (c, id));
    let after_turn = key.as_ref().and_then(|k| core.lock().last_send.get(k).copied());
    let spec = WaitSpec { output: p.output, timeout, after_turn };
    let r = watched(core, p.caller.clone(), id, spec).await;
    if r.ended
        && let Some(k) = key
    {
        let mut st = core.lock();
        if st.last_send.get(&k).copied() == after_turn {
            st.last_send.remove(&k);
        }
    }
    core.flush_saved().await?;
    Ok(r)
}

/// A caller's wait in progress: while it runs, its parent needs no note about it.
pub(super) struct Watching {
    core: Arc<Core>,
    key: Option<(String, u32)>,
}

impl Drop for Watching {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            let mut st = self.core.lock();
            if let Some(n) = st.waiting.get_mut(key) {
                *n -= 1;
                if *n == 0 {
                    st.waiting.remove(key);
                }
            }
        }
    }
}

/// A wait the caller is watching; afterwards it has seen the result, and a
/// `--rm` child it was waiting for can go.
pub(super) async fn watched(core: &Arc<Core>, caller: Option<String>, id: u32, spec: WaitSpec) -> WaitResult {
    let result = watch(core, caller.clone(), id, spec).await;
    has_seen(core, caller.as_deref(), id);
    result
}

/// `watched` without removing a finished `--rm` child: while it runs, the
/// caller needs no note, and what it returns is seen before that ends (an
/// exit it reports never becomes a note too).
pub(super) async fn watch(core: &Arc<Core>, caller: Option<String>, id: u32, spec: WaitSpec) -> WaitResult {
    let key = caller.clone().map(|c| (c, id));
    if let Some(key) = &key {
        *core.lock().waiting.entry(key.clone()).or_default() += 1;
    }
    let guard = Watching { core: core.clone(), key };
    let waiter = caller.as_deref().and_then(crate::terminals::parse_id);
    let result = core.wait(id, spec, waiter).await;
    core.saw(caller.as_deref(), id);
    drop(guard);
    result
}

/// The caller has this terminal's news now; a finished `--rm` child of its can go.
pub(super) fn has_seen(core: &Core, caller: Option<&str>, id: u32) {
    core.saw(caller, id);
    let mut st = core.lock();
    if st.reg.terminals.get(&id).is_some_and(|t| !t.is_live() && t.rm && t.parent.as_deref() == caller) {
        core.remove(&mut st, id);
    }
}

/// A child as it is now, for `--children` replies.
pub(super) fn as_is(t: &Terminal, ended: bool) -> WaitResult {
    let reason = match (ended, t.is_live() && !t.program_lost()) {
        (false, _) => "running",
        (true, true) => "status",
        (true, false) => "exit",
    };
    WaitResult {
        terminal: t.address.clone(),
        ended,
        reason: reason.into(),
        status: t.status.as_ref().map(|s| s.state.clone()),
        turn: t.turn,
        exit_code: t.exit_code,
        signal: t.signal,
        children: Vec::new(),
        from: None,
    }
}

/// Resolve whose children: a local terminal or a parent on a peer.
pub(super) fn children_of(core: &Core, p: &WaitParams) -> Result<(Option<u32>, String), RpcError> {
    match (&p.terminal, &p.caller) {
        (Some(t), _) => match crate::remote::peer_address(core, t) {
            Some(afar) => Ok((None, afar)),
            None => resolve(core, t).map(|id| (Some(id), crate::terminals::address(id))),
        },
        (None, Some(caller)) => {
            crate::terminals::parse_id(caller).map(|id| (Some(id), crate::terminals::address(id))).ok_or_else(|| {
                RpcError::new("needs_target", "whose children?", "run it inside a terminal, or name the parent")
            })
        }
        (None, None) => Err(RpcError::new("needs_target", "whose children?", "name the parent")),
    }
}

/// Children here and on every machine the parent started work on, waited on
/// together: `any` ends with the first news anywhere, `all` once none is
/// working anywhere. A machine that can't be reached is left out.
pub(super) async fn wait_children_everywhere(
    core: &Arc<Core>,
    p: &WaitParams,
    mode: &str,
    timeout: Duration,
) -> Result<WaitResult, RpcError> {
    let (parent, _) = children_of(core, p)?;
    let afar: Vec<crate::machines::Machine> = {
        let st = core.lock();
        let ids = parent.and_then(|id| st.reg.terminals.get(&id)).map(|t| t.afar.clone()).unwrap_or_default();
        st.machines.iter().filter(|m| ids.contains(&m.id)).cloned().collect()
    };
    let Some(parent) = parent.filter(|_| !afar.is_empty()) else {
        return wait_children(core, p, mode, timeout, false).await;
    };
    let mut sides = tokio::task::JoinSet::new();
    let local = WaitParams {
        terminal: p.terminal.clone(),
        children: p.children,
        output: None,
        timeout: p.timeout,
        caller: p.caller.clone(),
    };
    let (c, m) = (core.clone(), mode.to_string());
    sides.spawn(async move { wait_children(&c, &local, &m, timeout, true).await });
    for machine in afar {
        let params = json!({
            "terminal": format!("{}/{}", core.info.id, crate::terminals::address(parent)),
            "children": mode,
            "timeout": timeout.as_secs_f64(),
            "caller": p.caller.as_ref().map(|c| format!("{}/{c}", core.info.id)),
        });
        let c = core.clone();
        sides.spawn(async move {
            let r = crate::remote::forward(&c, &machine, "terminal.wait", params).await?;
            serde_json::from_value::<WaitResult>(r).map_err(|e| RpcError::internal(e.to_string()))
        });
    }
    let mut merged: Option<WaitResult> = None;
    while let Some(side) = sides.join_next().await {
        let Ok(Ok(r)) = side else { continue };
        // News or a message anywhere ends `any` (dropping the others cancels them there).
        if r.reason == "message" || (mode == "any" && r.ended && !r.children.is_empty()) {
            return Ok(WaitResult { terminal: crate::terminals::address(parent), ..r });
        }
        merged = Some(match merged {
            None => WaitResult { terminal: crate::terminals::address(parent), ..r },
            Some(mut m) => {
                m.children.extend(r.children);
                if !r.ended {
                    (m.ended, m.reason) = (false, r.reason);
                }
                m
            }
        });
    }
    merged.ok_or_else(|| RpcError::new("unavailable", "no answer from any machine", ""))
}

/// Decided from the children's current facts, again on every change:
/// `all`: none is working (unreported children and sent first tasks still pending count too);
/// the reply lists every child, finished ones too. `any`: a child with news
/// (for the parent itself: news it hasn't seen, even from before the wait), or
/// a message typed into the waiting parent (it sits in its queue until this
/// call ends); idle children are waited on too.
/// `others`: children on other machines count too, so none here is no answer.
pub(super) async fn wait_children(
    core: &Arc<Core>,
    p: &WaitParams,
    mode: &str,
    timeout: Duration,
    others: bool,
) -> Result<WaitResult, RpcError> {
    if p.output.is_some() {
        return Err(RpcError::invalid_params("children waits take no output; wait on one terminal for that"));
    }
    let (parent, address) = children_of(core, p)?;
    let done = |children: Vec<WaitResult>| WaitResult {
        terminal: address.clone(),
        ended: true,
        reason: "children".into(),
        status: None,
        turn: 0,
        exit_code: None,
        signal: None,
        children,
        from: None,
    };
    let any = mode == "any";
    let caller = p.caller.clone();
    let listen = caller.as_deref() == Some(address.as_str());
    let kids_of = |st: &crate::runtime::State| -> Vec<Terminal> {
        st.reg.terminals.values().filter(|t| t.parent.as_deref() == Some(address.as_str())).cloned().collect()
    };
    let busy = |t: &Terminal| {
        t.is_live()
            && !t.program_lost()
            && (!t.reported || t.first_task_turn.is_some() || t.status.as_ref().is_some_and(|s| s.state == "working"))
    };
    // While the caller waits on them, it needs no notes about them.
    let (start, _watching): (std::collections::HashMap<u32, u64>, Vec<Watching>) = {
        let mut st = core.lock();
        let kids = kids_of(&st);
        let watching = kids
            .iter()
            .filter_map(|t| caller.clone().map(|c| (c, t.id())))
            .map(|key| {
                *st.waiting.entry(key.clone()).or_default() += 1;
                Watching { core: core.clone(), key: Some(key) }
            })
            .collect();
        (kids.iter().map(|t| (t.id(), t.news)).collect(), watching)
    };
    let news = |t: &Terminal| match listen {
        true => t.news > t.seen.max(t.noted),
        false => t.news > start.get(&t.id()).copied().unwrap_or(0),
    };
    // The caller has these now; a `--rm` child it was waiting for can go.
    let seen = |ids: &[u32]| ids.iter().for_each(|&id| has_seen(core, caller.as_deref(), id));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let changed = core.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let (kids, message) = {
            let mut st = core.lock();
            (kids_of(&st), parent.filter(|_| listen).and_then(|id| st.messages.remove(&id)))
        };
        let ready: Option<Vec<&Terminal>> = match any {
            true => {
                let fresh: Vec<&Terminal> =
                    kids.iter().filter(|t| news(t) && (!busy(t) || t.first_task_turn.is_none())).collect();
                let nothing_can_happen = !others && !kids.iter().any(Terminal::is_live) && message.is_none();
                (!fresh.is_empty() || message.is_some() || nothing_can_happen).then_some(fresh)
            }
            false => (!kids.iter().any(busy)).then(|| kids.iter().collect()),
        };
        if let (None, Some(from)) = (&ready, &message) {
            let children = kids.iter().map(|t| as_is(t, !busy(t))).collect();
            return Ok(WaitResult { ended: false, reason: "message".into(), from: from.clone(), ..done(children) });
        }
        if let Some(results) = ready {
            seen(&results.iter().map(|t| t.id()).collect::<Vec<_>>());
            let mut r = done(results.iter().map(|t| as_is(t, true)).collect());
            if let Some(from) = message {
                if r.children.is_empty() {
                    r.reason = "message".into();
                }
                r.from = from;
            }
            return Ok(r);
        }
        if tokio::time::Instant::now() >= deadline {
            let children = kids.iter().filter(|t| t.is_live()).map(|t| as_is(t, !busy(t))).collect();
            return Ok(WaitResult { ended: false, reason: "timeout".into(), ..done(children) });
        }
        let _ = tokio::time::timeout_at(deadline, changed).await;
    }
}
