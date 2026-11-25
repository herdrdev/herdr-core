//! Forward a call after granting a remote run access to its local parent.

use super::*;

#[cfg(test)]
mod tests;

/// A new grant is shared by concurrent runs until each launch has an outcome.
/// Cancellation or a lost reply may hide a started child, so retain by default.
struct RunGrant<'a> {
    core: &'a Core,
    key: (u32, String),
    keep: bool,
}

impl<'a> RunGrant<'a> {
    fn start(core: &'a Core, m: &Machine, method: &str, params: &Value) -> Option<Self> {
        // An explicit parent is resolved on the destination, not on this core.
        if method != "terminal.run" || params["no_parent"] == true || !params["parent"].is_null() {
            return None;
        }
        let caller = crate::addr::local(params["caller"].as_str()?, &core.info.id, &core.info.name).ok()?;
        let parent = crate::terminals::parse_id(caller)?;
        let key = (parent, m.id.clone());
        let mut st = core.lock();
        let t = st.reg.terminals.get(&parent)?;
        if t.afar.contains(&m.id) && !st.afar_runs.contains_key(&key) {
            return None; // A previous launch already made this grant permanent.
        }
        st.afar_runs.entry(key.clone()).or_default().0 += 1;
        st.reg.terminals.get_mut(&parent)?.afar.insert(m.id.clone());
        core.save(&st);
        Some(Self { core, key, keep: true })
    }

    fn answered(mut self, result: &Result<Value, RpcError>) {
        if let Err(e) = result {
            self.keep = !not_started(e);
        }
    }
}

impl Drop for RunGrant<'_> {
    fn drop(&mut self) {
        let mut st = self.core.lock();
        let Some((pending, keep)) = st.afar_runs.get_mut(&self.key) else { return };
        *keep |= self.keep;
        *pending -= 1;
        if *pending != 0 {
            return;
        }
        let keep = *keep;
        st.afar_runs.remove(&self.key);
        if !keep && let Some(t) = st.reg.terminals.get_mut(&self.key.0) {
            t.afar.remove(&self.key.1);
            self.core.save(&st);
        }
    }
}

/// Only errors that prove no child started permit revoking a new grant.
/// In particular, spawn timeouts and persistence errors can follow a real spawn.
fn not_started(e: &RpcError) -> bool {
    match e.kind.as_str() {
        "invalid_params" | "invalid_template" | "config" | "name_taken" | "not_found" | "machine_unknown"
        | "unknown_method" => true,
        "spawn_failed" => e.message != "the holder did not answer",
        "unavailable" => {
            e.message == "the core is stopping"
                || e.message.starts_with("no holder:")
                || e.message == "this core has used every terminal id"
        }
        _ => false,
    }
}

pub(super) async fn forward_once(core: &Core, m: &Machine, method: &str, mut params: Value) -> Result<Value, RpcError> {
    if method == "terminal.run" {
        // An older core ends a child with its parent only when asked to.
        params["die_with_parent"] = json!(params["outlive_parent"] != true);
    }
    let deadline = if method == "terminal.send" { Duration::MAX } else { deadline_for(core, method, &params) };
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
    let grant = RunGrant::start(core, m, method, &params);
    let result = line.call(&m.name, method, params, deadline).await;
    if let Some(grant) = grant {
        grant.answered(&result);
    }
    let reply = result.map_err(|e| match e.kind.as_str() {
        "unknown_method" => RpcError::new(
            "needs_update",
            format!("{}'s hc is older and has no {method}", m.name),
            format!("hc machine update {}", m.name),
        ),
        _ => e,
    })?;
    // Document contents are opaque: address-shaped keys belong to the client.
    Ok(if method.starts_with("doc.") { reply } else { localize(core, &m.name, &[], reply) })
}
