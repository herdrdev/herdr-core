//! `hc update`: swap this hc for another build, keep the terminals running.

use super::*;

/// What a binary says it is (`hc 0.2.0`), if it is a working hc.
pub(super) fn version_of(path: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new(path).arg("--version").stdin(std::process::Stdio::null()).output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && v.starts_with("hc ")).then_some(v)
}

/// A release name for `--version`: `0.1.4` (it goes into a URL).
pub(super) fn release_version(s: &str) -> Result<String, String> {
    let parts: Vec<&str> = s.split('.').collect();
    let number = |p: &&str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if parts.len() == 3 && parts.iter().all(number) {
        Ok(s.into())
    } else {
        Err(format!("expected a release like 0.1.4, got {s:?}"))
    }
}

/// `hc 0.1.9` is older than `hc 0.2.0`; anything unparsed is not older.
pub(super) fn older(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Option<Vec<u64>> {
        v.strip_prefix("hc ")?.split(['.', '-']).take(3).map(|p| p.parse().ok()).collect()
    };
    matches!((parts(a), parts(b)), (Some(a), Some(b)) if a < b)
}

/// The installed binary can be current while a core started before it was
/// replaced is still running. Do not start a core just to check its version.
pub(super) fn up_to_date(paths: &Paths, release: &str, human: String) -> Result<Out, RpcError> {
    let this = format!("hc {}", env!("CARGO_PKG_VERSION"));
    let mut out = Out { json: json!({"release": release, "now": this, "updated": false}), human };
    if let Some(stream) = client::connect_existing(paths) {
        let status = client::call(&stream, "server.status", json!({}))?;
        if older(&format!("hc {}", status["version"].as_str().unwrap_or_default()), &this) {
            client::call(&stream, "server.restart", json!({}))?;
            wait_closed(&stream);
            let status = call(paths, "server.status", json!({}))?;
            out.json["restarted"] = true.into();
            out.json["pid"] = status["pid"].clone();
            out.human.push_str(&format!("; older core restarted (pid {}), terminals kept", status["pid"]));
        }
    }
    Ok(out)
}

/// Swaps the binary this hc runs from for `from`, then restarts the core
/// the planned way: every terminal keeps running. One swap path, for people
/// and for apps that bundle hc.
pub(super) fn update(paths: &Paths, from: &std::path::Path, force: bool) -> Result<Out, RpcError> {
    let refused = |why: String, hint: &str| RpcError::new("refused", why, hint);
    let dest = std::env::current_exe().and_then(|e| e.canonicalize()).map_err(|e| RpcError::internal(e.to_string()))?;
    let shown = dest.display().to_string();
    if let Some(manager) = client::installed_by() {
        let how = match manager {
            "homebrew" => "brew upgrade herdr-core",
            "nix" => "update it in your Nix configuration",
            "mise" => "mise upgrade herdr-core",
            _ => {
                return Err(refused(
                    format!("{} isn't writable by you", dest.parent().unwrap().display()),
                    "update herdr-core the way it was installed (package manager, or sudo)",
                ));
            }
        };
        return Err(refused(format!("{shown} belongs to {manager}"), how));
    }
    let was = format!("hc {}", env!("CARGO_PKG_VERSION"));
    let now = version_of(from).ok_or_else(|| refused(format!("{} is not a working hc", from.display()), ""))?;
    if older(&now, &was) && !force {
        return Err(refused(format!("{now} is older than this {was}"), "--force to go back anyway"));
    }
    // Staged beside it, checked, then swapped in: never a half-written hc.
    crate::platform::swap_in(from, &dest, |staged| version_of(staged).as_deref() == Some(now.as_str())).map_err(
        |e| match e.kind() {
            std::io::ErrorKind::InvalidData => refused(format!("the copy of {} does not run", from.display()), ""),
            _ => RpcError::internal(format!("cannot replace {shown}: {e}")),
        },
    )?;
    let restarted = match client::connect_existing(paths) {
        Some(stream) => {
            client::call(&stream, "server.restart", json!({}))?;
            wait_closed(&stream);
            true
        }
        None => false,
    };
    let status = call(paths, "server.status", json!({}))?;
    let human = format!("{shown}: {was} -> {now}; core running again (pid {}), terminals kept", status["pid"]);
    Ok(Out {
        json: json!({"path": shown, "was": was, "now": now, "restarted": restarted, "pid": status["pid"]}),
        human,
    })
}
