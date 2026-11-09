use super::*;
use crate::push::send::{self, subscribe};
use crate::push::{Entry, check};

/// A webhook as listed: secrets are never echoed.
fn listed(h: &PushHook) -> PushHook {
    let mut h = h.clone();
    (h.spec.secret, h.spec.auth) = (None, None);
    h
}

fn unknown(name: &str) -> RpcError {
    RpcError::new("not_found", format!("no webhook {name:?}"), "see `hc push ls`")
}

fn hook(core: &Core, name: &str) -> Result<PushHook, RpcError> {
    core.push.hooks.lock().unwrap_or_else(|p| p.into_inner()).get(name).cloned().ok_or_else(|| unknown(name))
}

fn save(core: &Core, hooks: &BTreeMap<String, PushHook>) -> Result<(), RpcError> {
    core.push.save(hooks).map_err(|e| RpcError::internal(format!("cannot save webhooks: {e}")))
}

pub(super) async fn push_add(core: Ctx<'_>, p: PushAddParams) -> Result<PushHook, RpcError> {
    crate::addr::check_name(&p.name)?;
    if let Some(why) = check(&p) {
        return Err(RpcError::invalid_params(why));
    }
    let added = PushHook { spec: p, seen: Default::default() };
    {
        let mut hooks = core.push.hooks.lock().unwrap_or_else(|p| p.into_inner());
        let previous = hooks.insert(added.spec.name.clone(), added.clone());
        if let Err(e) = save(core, &hooks) {
            match previous {
                Some(h) => hooks.insert(h.spec.name.clone(), h),
                None => hooks.remove(&added.spec.name),
            };
            return Err(e);
        }
    }
    subscribe(core, &mut core.lock(), &added.spec);
    Ok(listed(&added))
}

pub(super) async fn push_list(core: Ctx<'_>, _: Empty) -> Result<PushListResult, RpcError> {
    let hooks = core.push.hooks.lock().unwrap_or_else(|p| p.into_inner());
    Ok(PushListResult { webhooks: hooks.values().map(listed).collect() })
}

pub(super) async fn push_rm(core: Ctx<'_>, p: PushNameParams) -> Result<RemoveResult, RpcError> {
    {
        let mut hooks = core.push.hooks.lock().unwrap_or_else(|p| p.into_inner());
        let Some(removed) = hooks.remove(&p.name) else { return Err(unknown(&p.name)) };
        if let Err(e) = save(core, &hooks) {
            hooks.insert(removed.spec.name.clone(), removed);
            return Err(e);
        }
    }
    core.unsubscribe_push(&mut core.lock(), &p.name);
    Ok(RemoveResult { removed: p.name })
}

pub(super) async fn push_test(core: Ctx<'_>, p: PushNameParams) -> Result<PushTestResult, RpcError> {
    let h = hook(core, &p.name)?;
    let ms = now_ms();
    let entry = Entry { seq: 0, ms, event: json!({"type": "test", "data": {}, "seq": 0, "ms": ms}) };
    let (seen, outcome) = send::post(core, &h, &entry).await;
    let result =
        PushTestResult { status: seen.last_status, outcome: outcome.word().into(), error: seen.last_error.clone() };
    core.push.record(&h.spec, seen);
    Ok(result)
}

pub(super) async fn push_events(core: Ctx<'_>, p: PushEventsParams) -> Result<PushEventsResult, RpcError> {
    let log = core.push.log.lock().unwrap_or_else(|p| p.into_inner());
    let entries = match (p.seq, p.ms, p.since) {
        (Some(seq), Some(ms), None) => vec![log.find(seq, ms).ok_or_else(|| {
            RpcError::new(
                "not_found",
                format!("no pushed event {seq}:{ms}"),
                "push events are kept push_retention_days (default 7); `hc get <terminal>` shows its current state",
            )
        })?],
        (None, None, Some(since)) => log.since(since.seq, since.ms, p.limit.unwrap_or(100)),
        _ => return Err(RpcError::invalid_params("give seq and ms, or since")),
    };
    Ok(PushEventsResult { events: entries.into_iter().map(|e| json!(e)).collect() })
}

pub(super) async fn push_key(core: Ctx<'_>, _: Empty) -> Result<PushKeyResult, RpcError> {
    let key = core.push.vapid().map_err(|e| RpcError::internal(format!("cannot load the VAPID key: {e}")))?;
    Ok(PushKeyResult { key: crate::push::vapid_key(&key) })
}
