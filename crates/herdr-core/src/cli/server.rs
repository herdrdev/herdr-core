//! Core lifecycle commands and the client connection.

use super::*;
use std::io::Read;

pub(super) fn run(paths: &Paths, action: ServerCmd) -> Result<Out, RpcError> {
    if matches!(action, ServerCmd::Stop) {
        return stop(paths);
    }
    if let ServerCmd::Restart { fresh } = action {
        restart(paths, fresh)?;
    }
    let r = if crate::platform::status_may_start_core() {
        call(paths, "server.status", json!({}))?
    } else {
        let stream = if matches!(action, ServerCmd::Status) {
            client::require_existing(paths)?
        } else {
            client::connect_or_start_explicit(paths)?
        };
        client::call(&stream, "server.status", json!({}))?
    };
    let id = r["core"]["id"].as_str().unwrap_or("?");
    let mut human = format!(
        "core {} ({}) running, pid {}, up {}s",
        r["core"]["name"].as_str().unwrap_or("?"),
        id.get(..8).unwrap_or(id),
        r["pid"],
        r["uptime_ms"].as_u64().unwrap_or(0) / 1000
    );
    if let Some(status) = r["keep_awake"].as_str() {
        human.push_str(&format!("\nkeep_awake: {status}"));
    }
    Ok(Out { json: r, human })
}

fn restart(paths: &Paths, fresh: bool) -> Result<(), RpcError> {
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
fn stop(paths: &Paths) -> Result<Out, RpcError> {
    let Some(stream) = client::connect_existing(paths) else {
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

/// Copies bytes between stdin/stdout and the core: plain or framed, untouched.
pub(super) fn connect(paths: &Paths) -> Result<(), RpcError> {
    let stream = crate::platform::connect_for_app(paths)?;
    let mut to_core = stream.try_clone().map_err(|e| RpcError::internal(e.to_string()))?;
    std::thread::spawn(move || {
        pump(&mut *crate::platform::console::stdin(), &mut to_core);
        let _ = to_core.shutdown(std::net::Shutdown::Write);
    });
    // Unbuffered: std's stdout holds bytes until a newline, and frames have none.
    pump(&mut &stream, &mut *crate::platform::console::stdout());
    Ok(())
}

fn pump(from: &mut impl Read, to: &mut impl std::io::Write) {
    let mut buf = [0u8; 64 * 1024];
    while let Ok(n @ 1..) = from.read(&mut buf) {
        if to.write_all(&buf[..n]).is_err() {
            return;
        }
    }
}
/// Prints the snapshot, then every notification until the core goes away.
pub(super) fn follow_events(paths: &Paths, params: Value, json: bool) -> Result<(), RpcError> {
    use std::io::BufRead;
    let stream = client::connect_or_start(paths)?;
    let snapshot = client::call(&stream, "events.subscribe", params)?;
    if json {
        println!("{snapshot}");
    } else {
        println!("{}", table(&json!({"terminals": snapshot["terminals"]})));
    }
    for line in std::io::BufReader::new(&stream).lines() {
        let Ok(line) = line else { break };
        if json {
            println!("{line}");
            continue;
        }
        let msg: Value = serde_json::from_str(&line).unwrap_or_default();
        let p = &msg["params"];
        match msg["method"].as_str() {
            Some("event") if p["type"] == "doc" => {
                let name = p["data"]["name"].as_str().unwrap_or("?");
                let machine = p["machine"].as_str().map(|m| format!("{m}/")).unwrap_or_default();
                println!("{machine}{name}  doc  version {}", p["data"]["version"]);
            }
            Some("event") => println!(
                "{:<6} {:<10} {}",
                p["terminal"].as_str().unwrap_or("?"),
                p["type"].as_str().unwrap_or("?"),
                p["data"]
            ),
            Some("missed") => {
                println!("[missed some events; {} terminals now]", p["terminals"].as_array().map_or(0, Vec::len))
            }
            _ => {}
        }
    }
    Ok(())
}
