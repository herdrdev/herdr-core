//! `hc machine …`: adding, listing, logging in to, updating other machines.

use super::*;
use crate::api::{MachineAddParams, MachineParams, MachineRenameParams};

#[derive(Subcommand)]
pub(super) enum MachineCmd {
    /// Add a machine you reach with ssh: hc machine add vps.
    Add(MachineAddParams),
    /// List the machines added here.
    Ls,
    /// Log in again (a password or code) after the machine says it needs one.
    /// On Windows: reconnect once its SSH keys work.
    Login(MachineParams),
    /// Copy this hc there and restart hc there (its terminals keep running).
    Update(MachineParams),
    /// Give a machine a new name here.
    Rename(MachineRenameParams),
    /// Forget a machine (its terminals keep running there).
    Rm(MachineParams),
}

/// A person who can answer ssh's questions (a password, a code, a host key).
fn at_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// How a listed machine is reached.
fn reach_of(m: &Value) -> crate::machines::Reach<'_> {
    crate::machines::Reach { target: m["target"].as_str().unwrap_or_default(), via: m["via"] == true }
}

pub(super) fn machine(
    paths: &Paths,
    action: MachineCmd,
    call: impl Fn(&str, Value) -> Result<Value, RpcError>,
) -> Result<Out, RpcError> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    Ok(match action {
        MachineCmd::Add(p) => {
            let target = p.via.as_ref().or(p.target.as_ref()).cloned().unwrap_or_default();
            let reach = crate::machines::Reach { target: &target, via: p.via.is_some() };
            if at_terminal() {
                crate::machines::login(paths, reach)?;
            }
            let m = match call("machine.add", to(&p)) {
                Err(e) if (p.install || at_terminal()) && matches!(e.kind.as_str(), "hc_missing" | "incompatible") => {
                    let what = if e.kind == "hc_missing" { "has no hc" } else { "has an hc too old to talk to" };
                    if !p.install {
                        eprint!("{target} {what}. Copy this hc ({}) there? [Y/n] ", env!("CARGO_PKG_VERSION"));
                        let mut answer = String::new();
                        std::io::stdin().read_line(&mut answer).map_err(|err| RpcError::internal(err.to_string()))?;
                        if answer.trim().to_lowercase().starts_with('n') {
                            return Err(e);
                        }
                    }
                    let version = crate::machines::install(paths, reach, p.session.as_deref())?;
                    eprintln!("{target} now has {version}");
                    call("machine.add", to(&p))?
                }
                other => other?,
            };
            let human = format!("added {}: {} answers, hc {}", s(&m["name"]), s(&m["target"]), s(&m["version"]));
            Out { json: m, human }
        }
        MachineCmd::Ls => {
            let r = call("machine.list", json!({}))?;
            let rows: Vec<String> = r["machines"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| {
                    let session = m["session"].as_str().map(|x| format!(" session {x}")).unwrap_or_default();
                    let state = line_state(m);
                    format!(
                        "{:<16} {state}  \x1b[2m{}{session} \u{b7} hc {}\x1b[22m",
                        s(&m["name"]),
                        s(&m["target"]),
                        s(&m["version"])
                    )
                })
                .collect();
            let human =
                if rows.is_empty() { "no machines yet: hc machine add <ssh-target>".into() } else { rows.join("\n") };
            Out { json: r, human }
        }
        MachineCmd::Login(p) => {
            if !at_terminal() {
                return Err(RpcError::new(
                    "usage",
                    "logging in needs a terminal",
                    "a person types the password or code",
                ));
            }
            let r = call("machine.list", json!({}))?;
            let found = r["machines"].as_array().into_iter().flatten().find(|m| m["name"] == p.machine.as_str());
            let Some(m) = found else {
                return Err(RpcError::new(
                    "machine_unknown",
                    format!("unknown machine {:?}", p.machine),
                    "see `hc machine ls`",
                ));
            };
            crate::machines::login(paths, reach_of(m))?;
            let m = call("machine.connect", to(&p))?;
            let human = format!("logged in to {}: {}", p.machine, line_state(&m));
            Out { json: m, human }
        }
        MachineCmd::Update(p) => {
            let r = call("machine.list", json!({}))?;
            let found =
                r["machines"].as_array().into_iter().flatten().find(|m| m["name"] == p.machine.as_str()).cloned();
            let Some(m) = found else {
                return Err(RpcError::new(
                    "machine_unknown",
                    format!("unknown machine {:?}", p.machine),
                    "see `hc machine ls`",
                ));
            };
            let version = crate::machines::install(paths, reach_of(&m), m["session"].as_str())?;
            let human = format!("{} now runs {version}; its terminals kept running", p.machine);
            Out { json: json!({"machine": p.machine, "version": version}), human }
        }
        MachineCmd::Rename(p) => {
            let m = call("machine.rename", to(&p))?;
            let human = format!("{} is now {}", p.machine, s(&m["name"]));
            Out { json: m, human }
        }
        MachineCmd::Rm(p) => {
            let r = call("machine.rm", to(&p))?;
            let human = format!("forgot {}; its terminals keep running there", s(&r["removed"]));
            Out { json: r, human }
        }
    })
}
