//! Machine management API handlers (`machine.add`, `list`, `connect`, `rename`, `rm`).

use super::*;

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

pub(super) async fn machine_add(core: Ctx<'_>, p: MachineAddParams) -> Result<crate::machines::Machine, RpcError> {
    let (target, via) = match (p.target, p.via) {
        (Some(target), None) => (crate::machines::ssh_target(target), false),
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
        keeps_running_after_logout: hello["keeps_running_after_logout"].as_bool(),
        logout_linger_user: hello["logout_linger_user"].as_str().map(String::from),
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

pub(super) async fn machine_list(core: Ctx<'_>, _: Empty) -> Result<MachineListResult, RpcError> {
    let st = core.lock();
    Ok(MachineListResult { machines: st.machines.iter().map(|m| crate::remote::view(&st, m)).collect() })
}

pub(super) async fn machine_connect(core: Ctx<'_>, p: MachineParams) -> Result<crate::remote::MachineView, RpcError> {
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

pub(super) async fn machine_rename(
    core: Ctx<'_>,
    p: MachineRenameParams,
) -> Result<crate::machines::Machine, RpcError> {
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

pub(super) async fn machine_rm(core: Ctx<'_>, p: MachineParams) -> Result<RemoveResult, RpcError> {
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
