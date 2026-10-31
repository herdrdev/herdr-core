//! Other machines: their profiles, and the pipe to the core on each.

use crate::addr::check_name;
use crate::rpc::RpcError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Machine {
    /// What this core calls it: `rohan` in `rohan/t7`.
    pub name: String,
    /// How it is reached: an ssh target, or (`via`) a command.
    pub target: String,
    /// `target` is a command that runs a shell line there, given as its last
    /// argument, like ssh (a provider's own CLI, ssh with its options).
    #[serde(default)]
    pub via: bool,
    /// Short-lived (a sandbox for a task): clients may show it apart from the
    /// machines that stay. hc treats it like any other.
    #[serde(default)]
    pub temporary: bool,
    /// The hc session there (null: its default).
    pub session: Option<String>,
    /// The other core's permanent id.
    pub id: String,
    /// What that core calls itself (its hostname).
    pub host: String,
    /// Its hc version when last seen.
    pub version: String,
}

impl Machine {
    pub fn reach(&self) -> Reach<'_> {
        Reach { target: &self.target, via: self.via }
    }
}

/// How hc gets there: `ssh <target> <line>`, or `<target> <line>` (`via`).
#[derive(Clone, Copy)]
pub struct Reach<'a> {
    pub target: &'a str,
    pub via: bool,
}

impl Reach<'_> {
    /// Says how to add it again, for hints.
    fn add(&self) -> String {
        match self.via {
            true => format!("hc machine add --via '{}' --name <name>", self.target.replace('\'', r"'\''")),
            false => format!("hc machine add {}", self.target),
        }
    }

    /// The command that runs `line` there.
    fn command(&self, paths: &crate::paths::Paths, extra: &[&str], line: &str) -> std::process::Command {
        if self.via {
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg(format!("{} \"$1\"", self.target)).arg("sh").arg(line);
            return cmd;
        }
        let mut cmd = ssh(paths, self.target, extra);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // A pipe from the detached core must not open a console of its own.
            cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        if std::env::var_os("HC_SSH").is_none() {
            cmd.arg("-T");
        }
        cmd.arg(self.target).arg(line);
        cmd
    }
}

/// No machine by that name: the error lists the ones there are.
pub fn unknown(machines: &[Machine], name: &str) -> crate::rpc::RpcError {
    let names: Vec<&str> = machines.iter().map(|m| m.name.as_str()).collect();
    let hint = if names.is_empty() {
        "no machines yet: hc machine add <ssh-target>".into()
    } else {
        format!("machines: {}", names.join(", "))
    };
    crate::rpc::RpcError::new("machine_unknown", format!("unknown machine {name:?}"), hint)
}

pub fn save(state: &Path, machines: &[Machine]) {
    let bytes = serde_json::to_vec_pretty(machines).expect("machines serialize");
    if let Err(e) = crate::paths::write_atomic(&state.join("machines.json"), &bytes) {
        eprintln!("core: cannot save machines: {e}");
    }
}

/// A free name from the target's host: `can@rohan.lan` -> `rohan`; IPs stay whole.
pub fn default_name(target: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let t = target.strip_prefix("ssh://").unwrap_or(target);
    let host = t.rsplit_once('@').map_or(t, |(_, host)| host);
    let host = host.split(':').next()?.to_ascii_lowercase();
    let ip = host.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    let mut name = if ip { host } else { host.split('.').next()?.to_string() };
    if name.len() > 32 {
        name = name.chars().take(32).collect::<String>().trim_end_matches('-').into();
    }
    check_name(&name).ok()?;
    let base = name.clone();
    let mut number = 1;
    while taken(&name) {
        number += 1;
        let suffix = format!("-{number}");
        name = format!("{}{}", base[..base.len().min(32 - suffix.len())].trim_end_matches('-'), suffix);
    }
    Some(name)
}

/// An ssh target goes to ssh as one argument: never an option, never two
/// words. A command (`via`) is a shell line here, on one line.
pub fn check_target(to: Reach) -> Result<(), RpcError> {
    let target = to.target;
    if to.via {
        if cfg!(windows) || target.trim().is_empty() || target.chars().any(char::is_control) {
            return Err(RpcError::invalid_params(format!("bad command {target:?}: one line, run by sh here")));
        }
        return Ok(());
    }
    if target.is_empty() || target.starts_with('-') || target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(RpcError::invalid_params(format!("bad ssh target {target:?}")));
    }
    Ok(())
}

/// One login per target, shared: a person logs in once in a terminal (codes,
/// passwords), and the core's connections ride that login. Short, private path.
#[cfg(unix)]
fn control_path(paths: &crate::paths::Paths, target: &str) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    target.hash(&mut h);
    paths.socket.parent().expect("socket has a dir").join(format!("ssh-{:012x}", h.finish() & 0xffff_ffff_ffff))
}

/// `ssh` riding the shared login when there is one, with `extra` options; or
/// `$HC_SSH` in its place (tests), which takes only the target and the remote
/// command. Only `login` creates the shared login: one made by the line (or
/// an install) would end with it, and take every connection riding it along
/// (an update restarting hc there ended its own upload that way).
fn ssh(paths: &crate::paths::Paths, target: &str, extra: &[&str]) -> std::process::Command {
    if let Some(fake) = std::env::var_os("HC_SSH") {
        return std::process::Command::new(fake);
    }
    let mut ssh = std::process::Command::new("ssh");
    #[cfg(unix)]
    {
        let master = if extra.iter().any(|o| o.starts_with("ControlPersist")) { "auto" } else { "no" };
        ssh.arg("-S").arg(control_path(paths, target)).args(["-o", &format!("ControlMaster={master}")]);
    }
    #[cfg(windows)]
    let _ = (paths, target); // Windows OpenSSH has no ControlMaster support.
    for option in extra {
        ssh.args(["-o", option]);
    }
    ssh
}

/// Logs in from this terminal, so ssh can ask for a password or a code. The
/// login stays up for the core's connections (10 minutes once unused). A
/// command (`via`) logs in its own way: nothing to do.
pub fn login(paths: &crate::paths::Paths, to: Reach) -> Result<(), RpcError> {
    check_target(to)?;
    let target = to.target;
    if to.via || cfg!(windows) {
        return Ok(());
    }
    paths.create_dirs().map_err(|e| RpcError::internal(e.to_string()))?;
    let mut cmd = ssh(paths, target, &["BatchMode=no", "ControlPersist=600"]);
    cmd.env_remove("SSH_ASKPASS").env("SSH_ASKPASS_REQUIRE", "never").arg(target).arg("exit");
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(RpcError::new("needs_login", format!("logging in to {target} failed"), "ssh said why, above")),
        Err(e) => Err(unreachable(to, &format!("cannot run ssh: {e}"))),
    }
}

/// Ends the shared login to `target`, if one is up (a forgotten machine).
pub fn logout(paths: &crate::paths::Paths, to: Reach) {
    let target = to.target;
    if cfg!(unix) && std::env::var_os("HC_SSH").is_none() && !to.via {
        let mut cmd = ssh(paths, target, &[]);
        cmd.args(["-O", "exit", target])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = cmd.status();
    }
}

/// Targets found to be Windows, so redials go straight.
static WINDOWS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn is_windows(target: &str) -> bool {
    WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|t| t == target)
}

/// cmd and PowerShell, asked to run `sh`.
fn no_sh(err: &str) -> bool {
    err.contains("'sh' is not recognized") || err.contains("The term 'sh' is not recognized")
}

/// Where install.ps1 puts hc, from the home folder an ssh session starts in.
/// Not PATH: sessions riding a shared login keep the PATH from before an
/// install. `.\` reads the same in cmd and PowerShell.
const WINDOWS_HC: &str = r".\AppData\Local\Programs\herdr-core\herdr-core.exe";

/// The pipe to the core there: `herdr-core connect`, found on PATH, else in
/// ~/.local/bin. On Windows, whose shell may be cmd or PowerShell, one line
/// both read alike; the installer put hc on PATH. Never prompts: a login a
/// person must type fails fast.
fn pipe(paths: &crate::paths::Paths, to: Reach, session: Option<&str>, windows: bool) -> tokio::process::Command {
    let remote = if windows {
        format!("{WINDOWS_HC} connect{}", session.map(|s| format!(" --session {s}")).unwrap_or_default())
    } else {
        let session = session.map(|s| format!("HC_SESSION={s} ")).unwrap_or_default();
        format!(
            "sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x \"$p\" ] || {{ echo \"{NOT_HERE}\" >&2; exit 127; }}; {session}exec \"$p\" connect'"
        )
    };
    let options = ["BatchMode=yes", "ConnectTimeout=10", "ServerAliveInterval=15", "ServerAliveCountMax=4"];
    let mut cmd = tokio::process::Command::from(to.command(paths, &options, &remote));
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    cmd
}

/// What the pipe's line says when there is no hc there.
const NOT_HERE: &str = "herdr-core is not installed here";

fn unreachable(to: Reach, why: &str) -> RpcError {
    let target = to.target;
    let hint = match to.via {
        true => format!("check that `{target} echo ok` prints ok without asking anything"),
        false => format!("check that `ssh {target}` logs in without a password prompt (keys or an agent)"),
    };
    RpcError::new("unreachable", format!("cannot reach {target}: {why}"), hint)
}

/// ssh refused us a login only a person can give (code, password, key).
fn wants_login(err: &str) -> bool {
    err.contains("Permission denied")
        && ["publickey", "keyboard-interactive", "password"].iter().any(|k| err.contains(k))
}

/// An open pipe to the core there, past its hello.
pub struct Dialed {
    pub child: tokio::process::Child,
    pub stdin: tokio::process::ChildStdin,
    /// Framed from here on (when dialed framed); may hold bytes already read.
    pub stdout: BufReader<tokio::process::ChildStdout>,
    pub stderr: tokio::process::ChildStderr,
    pub hello: Value,
}

/// Dials, says hello and hangs up: the other core's hello, or why not.
pub async fn hello(paths: &crate::paths::Paths, to: Reach<'_>, session: Option<&str>) -> Result<Value, RpcError> {
    Ok(dial(paths, to, session, json!({})).await?.hello)
}

/// Dials and says hello, with `hello` added to its params (`framed`, who we are).
pub async fn dial(
    paths: &crate::paths::Paths,
    to: Reach<'_>,
    session: Option<&str>,
    hello: Value,
) -> Result<Dialed, RpcError> {
    let mut probe_windows = false;
    match dial_once(paths, to, session, hello.clone(), is_windows(to.target), &mut probe_windows).await {
        Err(e) if e.kind == "windows" => {
            WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(to.target.to_string());
            dial_once(paths, to, session, hello, true, &mut probe_windows).await
        }
        Err(e) if probe_windows => match dial_once(paths, to, session, hello, true, &mut probe_windows).await {
            Err(_) if !is_windows(to.target) => Err(e),
            result => result,
        },
        other => other,
    }
}

async fn dial_once(
    paths: &crate::paths::Paths,
    to: Reach<'_>,
    session: Option<&str>,
    hello: Value,
    windows: bool,
    probe_windows: &mut bool,
) -> Result<Dialed, RpcError> {
    let target = to.target;
    let mut child =
        pipe(paths, to, session, windows).spawn().map_err(|e| unreachable(to, &format!("cannot run it: {e}")))?;
    let (mut stdin, stdout, mut stderr) =
        (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let mut params = json!({"generation": crate::api::GENERATION});
    params.as_object_mut().unwrap().extend(hello.as_object().cloned().unwrap_or_default());
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": params});
    let _ = stdin.write_all(format!("{req}\n").as_bytes()).await;
    // A login may print a banner first: the reply is the first line that is one.
    let mut lines = BufReader::new(stdout);
    let reply = async {
        let mut line = String::new();
        loop {
            line.clear();
            match lines.read_line(&mut line).await {
                Ok(1..) => match serde_json::from_str::<Value>(&line) {
                    Ok(reply) if reply["id"] == 1 => return Some(reply),
                    _ => continue,
                },
                _ => return None,
            }
        }
    };
    let reply = tokio::time::timeout(Duration::from_secs(20), reply).await;
    let reply = match reply {
        Err(_) => return Err(unreachable(to, "no answer within 20 s")),
        Ok(Some(reply)) => reply,
        Ok(None) => {
            // Bounded: something it started may hold stderr open, and a dial
            // that never ends leaves the machine "connecting" for good.
            let mut err = String::new();
            let status = tokio::time::timeout(Duration::from_secs(5), async {
                let _ = stderr.read_to_string(&mut err).await;
                child.wait().await.ok().and_then(|s| s.code())
            })
            .await
            .ok()
            .flatten();
            if !windows {
                if no_sh(&err) {
                    return Err(RpcError::new("windows", "", "")); // dial picks the Windows line
                }
                // Windows may find Git's sh but fail in its shim or mangle
                // the script's quoting. Only our marker proves the script ran.
                *probe_windows = !err.contains(NOT_HERE) && !wants_login(&err);
            }
            // cmd and PowerShell, when hc isn't where install.ps1 puts it.
            let missing_windows =
                windows && (err.contains("is not recognized") || err.contains("cannot find the path"));
            if missing_windows && !is_windows(target) {
                WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
            }
            // Some connection wrappers collapse all remote failures to exit code 1.
            if status == Some(127) || err.contains(NOT_HERE) || missing_windows {
                return Err(RpcError::new(
                    "hc_missing",
                    format!("hc is not installed on {target}"),
                    format!("in a terminal, `{}` offers to copy this hc there", to.add()),
                ));
            }
            if wants_login(&err) {
                if cfg!(windows) && !to.via {
                    return Err(RpcError::new(
                        "needs_login",
                        format!("SSH authentication to {target} failed"),
                        format!("check that `ssh {target}` logs in without a password prompt (keys or an agent)"),
                    ));
                }
                return Err(RpcError::new(
                    "needs_login",
                    format!("{target} wants a login typed by a person (a password or a code)"),
                    format!("run `{}` in a terminal", to.add()),
                ));
            }
            let why = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("the connection closed");
            return Err(unreachable(to, why.trim()));
        }
    };
    if windows && !is_windows(target) {
        WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
    }
    match reply.get("error") {
        Some(e) if e["data"]["kind"] == "unsupported_generation" => Err(RpcError::new(
            "incompatible",
            format!("hc on {target} speaks another protocol generation"),
            format!("in a terminal, `{}` offers to update it", to.add()),
        )),
        Some(e) => Err(RpcError::from_json(e)),
        None => Ok(Dialed { child, stdin, stdout: lines, stderr, hello: reply["result"].clone() }),
    }
}

/// `uname -sm` as this binary names platforms: `linux x86_64`, `macos aarch64`.
fn platform(uname: &str) -> String {
    let mut words = uname.split_whitespace();
    let os = match words.next() {
        Some("Linux") => "linux",
        Some("Darwin") => "macos",
        Some(os) if os.starts_with("MINGW") || os.starts_with("MSYS") => "windows",
        other => other.unwrap_or("?"),
    };
    let arch = match words.next() {
        Some("arm64" | "aarch64") => "aarch64",
        Some("x86_64" | "amd64") => "x86_64",
        other => other.unwrap_or("?"),
    };
    format!("{os} {arch}")
}

/// Runs `command` there as its login shell takes it (riding the shared
/// login), feeding it `input`: the last line it printed.
fn run_there(paths: &crate::paths::Paths, to: Reach, command: &str, input: &[u8]) -> Result<String, RpcError> {
    let out = run_raw(paths, to, command, input)?;
    match out.status.success() {
        true => Ok(last_line(&out.stdout)),
        false => Err(RpcError::new(
            "install_failed",
            format!("on {}: {}", to.target, last_line(&out.stderr)),
            "nothing was changed there",
        )),
    }
}

fn last_line(b: &[u8]) -> String {
    String::from_utf8_lossy(b).lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_string()
}

fn run_raw(
    paths: &crate::paths::Paths,
    to: Reach,
    command: &str,
    input: &[u8],
) -> Result<std::process::Output, RpcError> {
    use std::io::Write;
    let mut cmd = to.command(paths, &["BatchMode=yes", "ConnectTimeout=10"], command);
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| unreachable(to, &format!("cannot run it: {e}")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(|e| unreachable(to, &e.to_string()))?;
    let _ = feeder.join();
    Ok(out)
}

/// Copies this very hc to `target` (same OS and CPU only), swaps it in at
/// ~/.local/bin/herdr-core (adding `hc` beside it only if there is none),
/// and restarts hc there in `session`, so terminals there keep running.
/// Returns what the installed one says it is.
pub fn install(paths: &crate::paths::Paths, to: Reach, session: Option<&str>) -> Result<String, RpcError> {
    check_target(to)?;
    let target = to.target;
    if let Some(s) = session {
        check_name(s)?;
    }
    let here = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let probe = run_raw(paths, to, "uname -sm", b"")?;
    let err = String::from_utf8_lossy(&probe.stderr);
    let there = platform(&last_line(&probe.stdout));
    if err.contains("'uname' is not recognized")
        || err.contains("The term 'uname' is not recognized")
        || (probe.status.success() && there.starts_with("windows "))
    {
        if to.via {
            return Err(RpcError::new("install_failed", format!("{target} runs Windows"), "install hc there yourself"));
        }
        WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
        return install_windows(paths, target, session);
    }
    if !probe.status.success() {
        return Err(RpcError::new(
            "install_failed",
            format!("on {target}: {}", last_line(&probe.stderr)),
            "nothing was changed there",
        ));
    }
    // Same OS and CPU: this very hc. Else this version's released build for it.
    let exe = if there == here {
        std::env::current_exe()
            .and_then(std::fs::read)
            .map_err(|e| RpcError::internal(format!("cannot read this hc: {e}")))?
    } else {
        let scratch = crate::releases::Scratch::new()?;
        let version = env!("CARGO_PKG_VERSION");
        let not_released = |_| {
            RpcError::new(
                "install_failed",
                format!("{target} is {there}, and hc {version} has no release to download"),
                "install hc there yourself",
            )
        };
        let m = crate::releases::manifest(Some(version), &scratch.0).map_err(not_released)?;
        let bin = crate::releases::download(&m, &there.replace(' ', "-"), &scratch.0)?;
        std::fs::read(bin).map_err(|e| RpcError::internal(e.to_string()))?
    };
    let n = exe.len();
    let session = session.map(|s| format!("HC_SESSION={s} ")).unwrap_or_default();
    // Into a temp file beside the target, checked whole and running, then swapped.
    let script = format!(
        "set -e; d=$HOME/.local/bin; mkdir -p $d; rm -f $d/.herdr-core.new.*; t=$d/.herdr-core.new.$$; \
         head -c {n} > $t; [ $(($(wc -c < $t))) -eq {n} ] || {{ rm -f $t; echo \"the copy arrived incomplete\" >&2; exit 3; }}; \
         chmod 755 $t; $t --version > /dev/null || {{ rm -f $t; echo \"this hc does not run there\" >&2; exit 4; }}; \
         mv -f $t $d/herdr-core; command -v hc > /dev/null 2>&1 || [ -e $d/hc ] || ln -s herdr-core $d/hc; \
         {session}$d/herdr-core server restart > /dev/null; $d/herdr-core --version"
    );
    run_there(paths, to, &format!("sh -c '{script}'"), &exe)
}

/// Windows there: this version's release zip and the installer go to its
/// home folder, and the installer runs from them (no download there). It
/// swaps an hc already there the planned way, so its terminals keep running.
fn install_windows(paths: &crate::paths::Paths, target: &str, session: Option<&str>) -> Result<String, RpcError> {
    let scratch = crate::releases::Scratch::new()?;
    let version = env!("CARGO_PKG_VERSION");
    let m = crate::releases::manifest(Some(version), &scratch.0).map_err(|_| {
        RpcError::new(
            "install_failed",
            format!("{target} runs Windows, and hc {version} has no release to download"),
            "install hc there yourself",
        )
    })?;
    let zip = crate::releases::fetch(&m, "windows-x86_64", &scratch.0)?;
    let script = scratch.0.join("hc-install.ps1");
    std::fs::write(&script, include_str!("../../../distribution/install.ps1"))
        .map_err(|e| RpcError::internal(e.to_string()))?;
    std::fs::copy(&zip, scratch.0.join("hc-install.zip")).map_err(|e| RpcError::internal(e.to_string()))?;
    let mut scp = std::process::Command::new("scp");
    scp.arg("-q");
    #[cfg(unix)]
    scp.arg("-o")
        .arg(format!("ControlPath={}", control_path(paths, target).display()))
        .args(["-o", "ControlMaster=no"]);
    scp.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    scp.arg(scratch.0.join("hc-install.zip")).arg(&script).arg(format!("{target}:"));
    let to = Reach { target, via: false };
    let out = scp.output().map_err(|e| unreachable(to, &format!("cannot run scp: {e}")))?;
    if !out.status.success() {
        return Err(RpcError::new(
            "install_failed",
            format!("copying hc to {target}: {}", String::from_utf8_lossy(&out.stderr).trim()),
            "nothing was changed there",
        ));
    }
    // No quotes or $ in it: cmd and PowerShell read it the same.
    let session = session.map(|s| format!(" {s}")).unwrap_or_default();
    run_there(
        paths,
        to,
        &format!("powershell -NoProfile -ExecutionPolicy Bypass -File hc-install.ps1 hc-install.zip{session}"),
        b"",
    )?;
    run_there(paths, to, &format!("{WINDOWS_HC} --version"), b"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_comes_from_the_target_host() {
        let default_name = |target| super::default_name(target, |_| false);
        assert_eq!(default_name("rohan").as_deref(), Some("rohan"));
        assert_eq!(default_name("can@Rohan.lan").as_deref(), Some("rohan"));
        assert_eq!(default_name("ssh://can@rohan:2222").as_deref(), Some("rohan"));
        assert_eq!(default_name("root@10.0.0.7").as_deref(), Some("10.0.0.7"));
        assert_eq!(default_name("[::1]"), None);
        assert_eq!(default_name(&format!("{}-extra.example.com", "a".repeat(31))), Some("a".repeat(31)));
        assert_eq!(super::default_name("rohan", |name| name == "rohan"), Some("rohan-2".into()));
        let ssh = |target| Reach { target, via: false };
        assert!(check_target(ssh("-oProxyCommand=x")).is_err() && check_target(ssh("a b")).is_err());
        assert!(check_target(Reach { target: "fly ssh console -C", via: true }).is_ok() || cfg!(windows));
        assert_eq!(platform("Darwin arm64"), "macos aarch64");
        assert_eq!(platform("Linux x86_64"), "linux x86_64");
        assert_eq!(platform("MINGW64_NT-10.0 x86_64"), "windows x86_64");
        assert_eq!(platform("MSYS_NT-10.0 x86_64"), "windows x86_64");
    }

    #[test]
    fn a_windows_box_gets_one_line_cmd_and_powershell_read_alike() {
        assert!(no_sh("'sh' is not recognized as an internal or external command,"));
        assert!(no_sh("sh : The term 'sh' is not recognized as the name of a cmdlet"));
        assert!(!no_sh("sh: herdr-core: not found"));
        let paths = crate::paths::Paths::from_env().unwrap();
        let cmd = pipe(&paths, Reach { target: "winbox", via: false }, Some("s1"), true);
        let line = cmd.as_std().get_args().last().unwrap().to_string_lossy().into_owned();
        assert_eq!(line, r".\AppData\Local\Programs\herdr-core\herdr-core.exe connect --session s1");
    }
}
