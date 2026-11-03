//! Released builds on releases.herdr.dev: the list (`core/latest.json`, or
//! `core/<version>/manifest.json` for one version) and its checked
//! downloads. curl, sha256sum/shasum (Windows: its own crypto) and tar do
//! the work: no HTTP or crypto code in hc for something run this rarely.

use crate::rpc::RpcError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Deserialize)]
pub struct Manifest {
    pub version: String,
    pub generation: u32,
    pub targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
pub struct Target {
    pub url: String,
    pub sha256: String,
}

fn base() -> String {
    std::env::var("HERDR_RELEASES_URL")
        .unwrap_or_else(|_| "https://releases.herdr.dev".into())
        .trim_end_matches('/')
        .into()
}

fn unavailable(why: String) -> RpcError {
    RpcError::new("unavailable", why, "check your connection; the releases are at releases.herdr.dev")
}

fn curl(url: &str, to: &Path) -> Result<(), RpcError> {
    // Quiet: our error says what failed, and piped stderr is one JSON error.
    let ok = Command::new("curl")
        .args(["-fsL", "--retry", "2", "--connect-timeout", "10", "-o"])
        .arg(to)
        .arg(url)
        .stderr(std::process::Stdio::null())
        .status();
    match ok {
        Ok(s) if s.success() => Ok(()),
        _ => Err(unavailable(format!("cannot download {url}"))),
    }
}

/// The latest release's list, or one version's.
pub fn manifest(version: Option<&str>, scratch: &Path) -> Result<Manifest, RpcError> {
    let url = match version {
        Some(v) => format!("{}/core/{v}/manifest.json", base()),
        None => format!("{}/core/latest.json", base()),
    };
    let file = scratch.join("manifest.json");
    curl(&url, &file)?;
    let bytes = std::fs::read(&file).map_err(|e| unavailable(e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| unavailable(format!("{url} is not a release list: {e}")))
}

/// Windows' own tar opens zips; Git Bash's GNU tar, often first on PATH there, can't.
fn tar_program() -> PathBuf {
    match std::env::var_os("SystemRoot") {
        Some(root) if cfg!(windows) => Path::new(&root).join(r"System32\tar.exe"),
        _ => "tar".into(),
    }
}

#[cfg(windows)]
fn sha256(file: &Path) -> Option<String> {
    crate::platform::sha256(&std::fs::read(file).ok()?)
}

#[cfg(unix)]
fn sha256(file: &Path) -> Option<String> {
    let run =
        |cmd: &str, args: &[&str]| Command::new(cmd).args(args).arg(file).output().ok().filter(|o| o.status.success());
    let out = run("sha256sum", &[]).or_else(|| run("shasum", &["-a", "256"]))?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(String::from)
}

/// Downloads `m`'s build for `target` (`linux-x86_64`) into `scratch`,
/// checks it against the list, unpacks it: the path of its herdr-core.
/// Windows builds are a zip (the exe and its ConPTY); Windows' own tar
/// unpacks it.
pub fn download(m: &Manifest, target: &str, scratch: &Path, bundle: bool) -> Result<PathBuf, RpcError> {
    let tar = fetch(m, target, scratch)?;
    let exe = if target.starts_with("windows") { "herdr-core.exe" } else { "herdr-core" };
    let ok =
        Command::new(tar_program()).arg("-xf").arg(&tar).arg("-C").arg(scratch).status().is_ok_and(|s| s.success());
    // Root executable remains in macOS archives for old updaters. New ones
    // carry the entire signed bundle through installation instead.
    let bundled = scratch.join("Herdr Core.app/Contents/MacOS/herdr-core");
    let bin = if bundle && target.starts_with("macos-") && bundled.is_file() { bundled } else { scratch.join(exe) };
    if !ok || !bin.is_file() {
        return Err(RpcError::new(
            "refused",
            format!("hc {}'s download for {target} holds no herdr-core", m.version),
            "",
        ));
    }
    crate::platform::verify_release_bundle(&bin)
        .map_err(|e| RpcError::new("refused", format!("invalid release signature: {e}"), ""))?;
    Ok(bin)
}

/// Downloads `m`'s archive for `target` into `scratch` and checks it against
/// the list: its path, still packed.
pub fn fetch(m: &Manifest, target: &str, scratch: &Path) -> Result<PathBuf, RpcError> {
    let t = m.targets.get(target).ok_or_else(|| {
        RpcError::new("refused", format!("hc {} has no build for {target}", m.version), "install hc there yourself")
    })?;
    let archive = if target.starts_with("windows") { "zip" } else { "tar.gz" };
    let file = scratch.join(format!("herdr-core-{target}.{archive}"));
    curl(&t.url, &file)?;
    if sha256(&file).as_deref() != Some(t.sha256.as_str()) {
        return Err(RpcError::new(
            "refused",
            format!("the download of hc {} for {target} is damaged (checksum mismatch)", m.version),
            "try again",
        ));
    }
    Ok(file)
}

/// A private folder for one download, removed when dropped.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Result<Scratch, RpcError> {
        let dir =
            std::env::temp_dir().join(format!("hc-download-{}-{}", std::process::id(), crate::platform::random_hex()));
        std::fs::create_dir_all(&dir).map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// This machine as release targets name it: `linux-x86_64`, `macos-aarch64`.
pub fn this_target() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}
