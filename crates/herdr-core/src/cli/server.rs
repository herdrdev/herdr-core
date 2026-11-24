//! `hc server …`: starting, stopping and restarting the core.

use super::*;

#[derive(Subcommand)]
pub(super) enum ServerCmd {
    Start,
    Stop,
    Status,
    /// Restart the core (e.g. after an update); programs keep running.
    Restart {
        /// Take this shell's environment for new terminals; run from a terminal outside Herdr.
        #[arg(long)]
        fresh: bool,
    },
}

pub(super) fn check_endpoint_command(cmd: Option<&Cmd>) -> Result<(), RpcError> {
    if crate::endpoints::in_cli()
        && matches!(
            cmd,
            Some(
                Cmd::Serve
                    | Cmd::Hold { .. }
                    | Cmd::Server { action: ServerCmd::Start | ServerCmd::Restart { .. } }
                    | Cmd::Update { .. }
                    | Cmd::Connect { session: Some(_) }
            )
        )
    {
        return Err(crate::endpoints::lifecycle_refusal());
    }
    Ok(())
}

pub(super) fn restart(paths: &Paths, fresh: bool) -> Result<(), RpcError> {
    if fresh && std::env::var_os("HC_TERMINAL_ID").is_some() {
        eprintln!(
            "hc: this shell carries Herdr's environment; run --fresh from a terminal outside Herdr or unset unwanted variables first"
        );
    }
    let environment: std::collections::BTreeMap<_, _> =
        crate::platform::utf8_vars().filter(|(k, _)| !k.starts_with('=')).collect();
    let Some(stream) = client::connect_existing(paths) else {
        if fresh {
            crate::restart::prepare_start(paths, environment).map_err(|e| {
                RpcError::new("unavailable", e.to_string(), "retry hc server restart --fresh once the core is running")
            })?;
        }
        return Ok(());
    };
    let params = if fresh {
        let hello = client::call(&stream, "hello", json!({}))?;
        if !hello["features"].as_array().is_some_and(|f| f.contains(&json!("restart_environment"))) {
            return Err(RpcError::new(
                "refused",
                "--fresh needs a newer running core",
                "run hc update, then retry hc server restart --fresh",
            ));
        }
        json!({"environment": environment})
    } else {
        json!({})
    };
    client::call(&stream, "server.restart", params)?;
    wait_closed(&stream);
    Ok(())
}

/// Stops the core and returns once it has exited (its connection closes).
pub(super) fn stop(paths: &Paths) -> Result<Out, RpcError> {
    let stream = if crate::endpoints::in_cli() {
        client::require_existing(paths)?
    } else if let Some(stream) = client::connect_existing(paths) {
        stream
    } else {
        crate::restart::discard_if_stopped(paths);
        return Ok(Out { json: json!({"stopped": false}), human: "no core running".into() });
    };
    let r = client::call(&stream, "server.stop", json!({}))?;
    wait_closed(&stream);
    Ok(Out { json: r, human: "core stopped".into() })
}

/// The core closes every connection when its process exits.
pub(super) fn wait_closed(stream: &crate::platform::Stream) {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
    let _ = (&*stream).read_to_end(&mut Vec::new());
}
