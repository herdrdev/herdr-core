#![cfg(windows)]
//! Windows end to end: real cmd and PowerShell programs on ConPTY, through
//! the CLI. The core's logic is the same code everywhere and tested on Unix;
//! these cover what Windows does differently: pipes, ConPTY, job objects,
//! Ctrl+C, handle inheritance, the console attach runs in.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-core");
static LIVE: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());

struct Session {
    name: String,
    root: PathBuf,
}

impl Session {
    fn new() -> Session {
        static N: AtomicU32 = AtomicU32::new(0);
        static WATCHDOG: std::sync::Once = std::sync::Once::new();
        // Abort a hung test binary after the watchdog deadline.
        WATCHDOG.call_once(|| {
            std::thread::spawn(|| {
                let limit = std::env::var("HC_TEST_WATCHDOG_S").ok().and_then(|s| s.parse().ok()).unwrap_or(90);
                std::thread::sleep(Duration::from_secs(limit));
                eprintln!("test binary still running after {limit} s: stopping its cores and aborting");
                // Each stop gets 10 s: the hang may be in stop itself.
                let live = LIVE.lock().unwrap_or_else(|p| p.into_inner()).clone();
                for (name, root) in live {
                    let Ok(mut stop) = cmd(&name, &root).args(["server", "stop"]).spawn() else { continue };
                    let start = std::time::Instant::now();
                    while stop.try_wait().ok().flatten().is_none() && start.elapsed() < Duration::from_secs(10) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    let _ = stop.kill();
                }
                std::process::abort();
            });
        });
        let name = format!("test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let root = std::env::temp_dir().join(format!("hc-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        LIVE.lock().unwrap().push((name.clone(), root.clone()));
        Session { name, root }
    }

    /// The exit code and stdout. `output()` waits for stdout to close: a core
    /// the CLI started that kept the pipe would hang here.
    fn out(&self, args: &[&str]) -> (i32, String) {
        let out = cmd(&self.name, &self.root).args(args).output().unwrap();
        if !out.stderr.is_empty() {
            eprintln!("{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn json(&self, args: &[&str]) -> Value {
        let (_, text) = self.out(args);
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{args:?}: {e}: {text}"))
    }

    /// Starts a program in the background; its address.
    fn bg(&self, program: &[&str]) -> String {
        let args: Vec<&str> = ["run", "--bg"].iter().chain(program).copied().collect();
        self.json(&args)["address"].as_str().unwrap().to_string()
    }

    fn read(&self, t: &str) -> String {
        self.out(&["read", t]).1
    }

    fn shows(&self, t: &str, text: &str) {
        wait_until(&format!("{t} shows {text:?}"), || self.read(t).contains(text));
    }

    /// Keys one by one, like a person typing.
    fn typed(&self, t: &str, text: &str) {
        let keys: Vec<String> = text.chars().map(|c| if c == ' ' { "space".into() } else { c.to_string() }).collect();
        let mut args = vec!["keys", t];
        args.extend(keys.iter().map(String::as_str));
        args.push("Enter");
        self.out(&args);
    }

    fn file(&self, name: &str, body: &str) -> String {
        let path = self.root.join(name);
        std::fs::write(&path, body).unwrap();
        path.display().to_string()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = cmd(&self.name, &self.root).args(["server", "stop"]).output();
        LIVE.lock().unwrap().retain(|(n, _)| *n != self.name);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn cmd(session: &str, root: &Path) -> Command {
    cmd_at(Path::new(BIN), session, root)
}

fn cmd_at(exe: &Path, session: &str, root: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.current_dir(root)
        .env("HC_SESSION", session)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HC_SETTLE_MS", "0")
        .env_remove("HC_TERMINAL_ID")
        .env_remove("HC_SOCKET")
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_CLIENT")
        .env_remove("SSH_TTY");
    cmd
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn alive(pid: u64) -> bool {
    let out = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

#[test]
fn sessions_select_distinct_pipes_even_with_the_same_state_home() {
    let a = Session::new();
    let b = Session::new();
    let status = |name: &str| {
        let out = cmd(name, &a.root).args(["server", "start"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let first = status(&a.name);
    let second = status(&b.name);
    assert_ne!(first["pid"], second["pid"], "HC_SESSION must select the pipe, not just state");
    assert_eq!(status(&a.name)["pid"], first["pid"]);
}

#[test]
fn ssh_reaches_the_host_without_a_control_socket() {
    use std::net::TcpListener;
    use std::sync::{Arc, atomic::AtomicBool};

    let s = Session::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = format!("ssh://127.0.0.1:{}", listener.local_addr().unwrap().port());
    listener.set_nonblocking(true).unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let stop = finished.clone();
    let server = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok(_) => return true,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept: {e}"),
            }
        }
        false
    });
    let output = cmd(&s.name, &s.root).env_remove("HC_SSH").args(["machine", "add", &target]).output().unwrap();
    finished.store(true, Ordering::Relaxed);
    assert!(server.join().unwrap(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn ssh_auth_failure_points_to_keys_instead_of_repeating_login() {
    let s = Session::new();
    let ssh =
        s.file("ssh-denied.cmd", "@echo off\r\necho Permission denied (publickey,password). 1>&2\r\nexit /b 255\r\n");
    let output = cmd(&s.name, &s.root).env("HC_SSH", &ssh).args(["machine", "add", "example"]).output().unwrap();
    let error: Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(error["error"]["kind"], "needs_login", "{error}");
    let hint = error["error"]["hint"].as_str().unwrap();
    assert!(hint.contains("keys or an agent") && !hint.contains("hc machine add"), "{hint}");
}

#[test]
fn ssh_transport_has_no_console_and_keeps_its_pipes() {
    let s = Session::new();
    let probe = s.file(
        "ssh-console.ps1",
        r#"Add-Type 'using System; using System.Runtime.InteropServices; public static class ConsoleProbe {
            [DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow(); }'
[IO.File]::WriteAllText((Join-Path $PSScriptRoot 'console-window.txt'), [ConsoleProbe]::GetConsoleWindow().ToInt64().ToString())
$request = [Console]::ReadLine() | ConvertFrom-Json
if ($request.method -ne 'hello') { exit 2 }
[Console]::WriteLine('{"id":1,"error":{"code":-32000,"message":"ssh-probe","data":{"kind":"probe","hint":"done"}}}')
"#,
    );
    let ssh = s.file(
        "ssh-console.cmd",
        &format!("@echo off\r\npowershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{probe}\"\r\n"),
    );
    let output = cmd(&s.name, &s.root).env("HC_SSH", &ssh).args(["machine", "add", "example"]).output().unwrap();
    let error: Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(error["error"]["kind"], "probe", "{error}");
    assert_eq!(std::fs::read_to_string(s.root.join("console-window.txt")).unwrap(), "0");
}

/// A release folder like the zip: herdr-core.exe + conpty, the exe marked
/// with `mark` at its end (a PE runs with bytes appended).
fn release(dir: &Path, mark: &str) -> PathBuf {
    let built = Path::new(BIN).parent().unwrap();
    for rel in [r"conpty\conpty.dll", r"conpty\x64\OpenConsole.exe"] {
        std::fs::create_dir_all(dir.join(rel).parent().unwrap()).unwrap();
        std::fs::copy(built.join(rel), dir.join(rel)).expect("run scripts\\fetch-conpty.ps1 first");
    }
    let mut exe = std::fs::read(BIN).unwrap();
    exe.extend_from_slice(mark.as_bytes());
    std::fs::write(dir.join("herdr-core.exe"), exe).unwrap();
    dir.join("herdr-core.exe")
}

fn marked(file: &Path, mark: &str) -> bool {
    std::fs::read(file).unwrap().ends_with(mark.as_bytes())
}

/// An installed hc, with `hc.exe` linked to it.
fn installed(s: &Session) -> PathBuf {
    let exe = release(&s.root.join("inst"), "old-build");
    std::fs::hard_link(&exe, s.root.join(r"inst\hc.exe")).unwrap();
    exe
}

fn hc_at(s: &Session, exe: &Path, args: &[&str]) -> std::process::Output {
    cmd_at(exe, &s.name, &s.root).args(args).output().unwrap()
}

#[test]
fn update_swaps_the_whole_release_while_its_terminals_run() {
    let s = Session::new();
    let exe = installed(&s);
    let run = hc_at(&s, &exe, &["run", "--bg", "ping", "-n", "120", "127.0.0.1"]);
    let t = serde_json::from_slice::<Value>(&run.stdout).unwrap()["address"].as_str().unwrap().to_string();
    // Not hc's, even when named like hc's files: never cleaned up. An old
    // update's own leftover is.
    let theirs = [s.root.join(r"inst\settings.old-backup"), s.root.join(r"inst\hc.exe.old-backup")];
    for f in &theirs {
        std::fs::write(f, "keep").unwrap();
    }
    let leftover = s.root.join(r"inst\herdr-core.exe.old-123-0123456789abcdef0123456789abcdef");
    std::fs::write(&leftover, "old").unwrap();
    let new = release(&s.root.join("new"), "new-build");
    let out = hc_at(&s, &exe, &["update", "--from", new.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(theirs.iter().all(|f| f.is_file()), "update deleted a file that isn't hc's");
    assert!(!leftover.exists(), "an old update's leftover stays");
    assert!(marked(&exe, "new-build"), "herdr-core.exe swapped");
    assert!(marked(&s.root.join(r"inst\hc.exe"), "new-build"), "hc.exe follows it");
    assert_eq!(s.json(&["get", &t])["state"], "live", "its terminal kept running");
}

#[test]
fn an_update_that_cannot_finish_puts_everything_back() {
    let s = Session::new();
    let exe = installed(&s);
    let new = release(&s.root.join("new"), "new-build");
    // Something holds the last file without letting it move (a scanner).
    let held = std::os::windows::fs::OpenOptionsExt::share_mode(std::fs::OpenOptions::new().read(true), 0)
        .open(s.root.join(r"inst\conpty\x64\OpenConsole.exe"))
        .unwrap();
    let out = hc_at(&s, &exe, &["update", "--from", new.to_str().unwrap()]);
    drop(held);
    assert!(!out.status.success(), "the update should have failed");
    assert!(marked(&exe, "old-build"), "herdr-core.exe back");
    assert!(marked(&s.root.join(r"inst\hc.exe"), "old-build"), "hc.exe back");
    assert!(s.root.join(r"inst\conpty\conpty.dll").is_file());
    assert!(hc_at(&s, &exe, &["--version"]).status.success(), "hc still runs");
}

#[test]
fn attach_types_non_ascii_text_unchanged() {
    // The console attach runs in reads keys in its code page (437 here).
    let s = Session::new();
    let got = s.root.join("got.txt");
    let script = format!(
        "Write-Host ready; $l = [Console]::ReadLine(); [IO.File]::WriteAllText('{}', (($l.ToCharArray() | % {{ [int]$_ }}) -join ','))",
        got.display()
    );
    let inner = s.bg(&["powershell", "-NoProfile", "-Command", &script]);
    s.shows(&inner, "ready");
    let outer = s.bg(&[BIN, "attach", &inner]);
    s.shows(&outer, "ready");
    s.json(&["send", &outer, "é"]);
    assert_eq!(s.json(&["wait", &inner, "--timeout", "15"])["exit_code"], 0, "the line was read and written");
    assert_eq!(std::fs::read_to_string(&got).unwrap(), "233");
}

#[test]
fn a_cmd_shim_gets_its_arguments_literally() {
    // npm installs agents as .cmd files; a prompt is an argument.
    let s = Session::new();
    let shim = s.file("agent.cmd", "@echo off\r\nsetlocal EnableDelayedExpansion\r\nset \"a=%~1\"\r\nset \"b=%~2\"\r\nset \"c=%~3\"\r\necho 1=[!a!] 2=[!b!] 3=[!c!]\r\n");
    let (_, out) = s.out(&["run", "--timeout", "20", &shim, "fix a & echo INJECTED", "100%PATH%", "x y"]);
    assert!(out.contains("1=[fix a & echo INJECTED] 2=[100%PATH%] 3=[x y]"), "{out}");
    assert!(!out.lines().any(|l| l.trim() == "INJECTED"), "{out}");
}

#[test]
fn the_exe_needs_no_visual_cpp_runtime_installed() {
    // Imported DLL names sit in the exe as plain text.
    let exe = String::from_utf8_lossy(&std::fs::read(BIN).unwrap()).to_ascii_lowercase();
    for dll in ["vcruntime140", "msvcp140", "api-ms-win-crt-"] {
        assert!(!exe.contains(dll), "herdr-core.exe imports {dll}: a clean Windows can't run it");
    }
}

#[test]
fn only_this_account_may_open_the_cores_pipe() {
    let s = Session::new();
    s.out(&["server", "start"]);
    let user: String = std::env::var("USERNAME").unwrap().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let pipe = format!(r"\\.\pipe\herdr-core-{user}-{}.sock", s.name);
    // From Windows itself: Get-Acl sometimes answers for the pipe root instead.
    let sddl = unsafe {
        use windows_sys::Win32::Security::Authorization::*;
        use windows_sys::Win32::Security::*;
        let name: Vec<u16> = pipe.encode_utf16().chain(Some(0)).collect();
        use std::ptr::null_mut;
        let mut sd = null_mut();
        let err = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        );
        assert_eq!(err, 0, "reading {pipe}'s permissions");
        let mut text = null_mut();
        assert_ne!(
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                null_mut()
            ),
            0
        );
        String::from_utf16_lossy(std::slice::from_raw_parts(text, (0..).take_while(|&i| *text.add(i) != 0).count()))
    };
    // SYSTEM and exactly one account: this one (a SID, or an alias such as
    // LA when running as the built-in Administrator, as CI does).
    let account = sddl
        .strip_prefix("D:P(A;;FA;;;SY)(A;;FA;;;")
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or_else(|| panic!("{sddl}"));
    assert!(!account.contains('('), "more than one account: {sddl}");
    for group in ["WD", "AN", "BA", "BU", "AU", "IU"] {
        assert_ne!(account, group, "{group} has access: {sddl}");
    }
}

#[test]
fn status_does_not_start_a_core() {
    let s = Session::new();
    let out = cmd(&s.name, &s.root).args(["server", "status"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("hc server start"));
    assert!(!s.root.join("state").exists(), "status created runtime state");
}

#[test]
fn new_terminals_refresh_windows_settings_without_losing_runtime_variables() {
    let s = Session::new();
    let out = cmd(&s.name, &s.root)
        .env("PATH", r"C:\Windows\System32;HC_STALE_PATH")
        .env("HC_PROCESS_ONLY", "preserved")
        .args(["server", "start"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (code, output) =
        s.out(&["run", "--timeout", "20", "cmd", "/c", "echo %PATH% & echo %HC_PROCESS_ONLY% & echo %HC_SESSION%"]);
    assert_eq!(code, 0, "{output}");
    assert!(!output.contains("HC_STALE_PATH"), "{output}");
    assert!(output.contains("preserved"), "{output}");
    assert!(output.contains(&s.name), "{output}");
}

#[test]
fn an_ssh_client_uses_an_existing_core_instead_of_starting_another() {
    let s = Session::new();
    let first = s.json(&["server", "start"]);
    let out = cmd(&s.name, &s.root).env("SSH_CONNECTION", "10.0.0.1 50000 10.0.0.2 22").args(["ls"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.json(&["server", "status"])["pid"], first["pid"]);
}

#[test]
fn implicit_ssh_start_reports_desktop_conflict_or_starts_headless() {
    let s = Session::new();
    let out = cmd(&s.name, &s.root).env("SSH_CLIENT", "10.0.0.1 50000 22").args(["ls"]).output().unwrap();
    if out.status.success() {
        assert!(s.json(&["server", "status"])["pid"].is_number());
    } else {
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert!(error["error"]["message"].as_str().unwrap().contains("desktop session"), "{error}");
        assert!(error["error"]["hint"].as_str().unwrap().contains("open Herdr on that Windows machine"), "{error}");
        assert_ne!(s.out(&["server", "status"]).0, 0, "refused startup left a core");
    }
}

#[test]
fn explicit_start_is_allowed_even_from_an_ssh_client() {
    let s = Session::new();
    // SSH_CLIENT triggers startup policy without the distinct kill-on-close job guard.
    let out = cmd(&s.name, &s.root).env("SSH_CLIENT", "10.0.0.1 50000 22").args(["server", "start"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn registry_changes_after_start_are_seen_by_each_new_terminal() {
    struct Setting(String);
    impl Setting {
        fn set(&self, value: Option<&str>) {
            let script = match value {
                Some(value) => format!(
                    "$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true); $k.SetValue('{}', '{}', [Microsoft.Win32.RegistryValueKind]::ExpandString); $k.Close()",
                    self.0, value
                ),
                None => format!(
                    "$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true); $k.DeleteValue('{}', $false); $k.Close()",
                    self.0
                ),
            };
            assert!(Command::new("powershell").args(["-NoProfile", "-Command", &script]).status().unwrap().success());
        }
    }
    impl Drop for Setting {
        fn drop(&mut self) {
            self.set(None);
        }
    }
    let s = Session::new();
    let setting = Setting(format!("HC_ENV_{}", s.name.replace('-', "_")));
    s.json(&["server", "start"]);
    for value in ["first", "second"] {
        setting.set(Some(&format!("%SystemRoot%\\{value}")));
        let (code, output) = s.out(&["run", "--timeout", "20", "cmd", "/c", &format!("echo %{}%", setting.0)]);
        assert_eq!(code, 0, "{output}");
        assert!(output.contains(&format!("{}\\{value}", std::env::var("SystemRoot").unwrap())), "{output}");
        assert!(!output.contains("%SystemRoot%"), "REG_EXPAND_SZ was not expanded: {output}");
    }
}

#[test]
fn a_core_that_would_die_at_ssh_logout_is_refused_not_started() {
    // Cargo's job kills on close and won't let hc leave, like some ssh
    // sessions: there, logging out would end every terminal.
    let s = Session::new();
    let out = cmd(&s.name, &s.root).env("SSH_CONNECTION", "10.0.0.1 50000 10.0.0.2 22").args(["ls"]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    assert!(err.contains("log out") || err.contains("desktop session"), "{err}");
}

#[test]
fn run_waits_and_returns_the_output_and_exit_code() {
    let s = Session::new();
    let (code, out) = s.out(&["run", "--timeout", "20", "cmd", "/c", "echo hello & exit 3"]);
    assert!(out.contains("hello"), "{out}");
    assert_eq!(code, 3, "{out}");
}

#[test]
fn exec_returns_exact_bytes_and_ends_what_it_started_at_its_timeout() {
    let s = Session::new();
    let out = cmd(&s.name, &s.root).args(["exec", "echo a\tb& exit 3"]).output().unwrap();
    assert_eq!((out.stdout.as_slice(), out.status.code()), (&b"a\tb\r\n"[..], Some(3)), "cmd's line, tab kept");

    let pid_file = s.root.join("exec-pid.txt");
    let child =
        s.file("exec.ps1", &format!("$PID | Out-File -Encoding ascii '{}'; Start-Sleep 60", pid_file.display()));
    let (code, _) = s.out(&["exec", "--timeout", "5", "cmd", "/c", &format!("powershell -NoProfile -File {child}")]);
    assert_eq!(code, 124);
    let pid: u64 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    wait_until("grandchild gone", || !alive(pid));
}

#[test]
fn a_shell_line_runs_in_cmd_quotes_and_all_and_no_program_is_powershell() {
    let s = Session::new();
    let (code, out) = s.out(&["run", "--timeout", "20", "echo \"hi  there\" & exit 3"]);
    assert!(out.contains("\"hi  there\""), "{out}");
    assert_eq!(code, 3, "{out}");
    let out = cmd(&s.name, &s.root).args(["exec", "echo \"a  b\""]).output().unwrap();
    assert_eq!(out.stdout, b"\"a  b\"\r\n");
    let out = cmd(&s.name, &s.root).args(["exec", r"echo %OS% a^^b C:\dir\"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Windows_NT a^b C:\\dir\\\r\n", "cmd's own rules");

    let mut conn = cmd(&s.name, &s.root)
        .arg("connect")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (mut input, output) = (conn.stdin.take().unwrap(), conn.stdout.take().unwrap());
    std::io::Write::write_all(
        &mut input,
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"terminal.run\",\"params\":{}}\n",
    )
    .unwrap();
    let mut reply = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(output), &mut reply).unwrap();
    drop(input);
    let _ = conn.wait();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    let shell = reply["result"]["argv"][0].as_str().unwrap_or_default().to_string();
    assert!(shell == "pwsh.exe" || shell == "powershell.exe", "{reply}");
}

#[test]
fn send_types_a_line_into_a_program_waiting_for_one() {
    let s = Session::new();
    let t = s.bg(&["powershell", "-NoProfile", "-Command", "$x = Read-Host 'name'; \"got-$x\""]);
    s.shows(&t, "name");
    s.json(&["send", &t, "bob"]);
    let end = s.json(&["wait", &t, "--timeout", "15"]);
    assert_eq!(end["reason"], "exit", "{end}");
    assert!(s.read(&t).contains("got-bob"));
}

#[test]
fn a_program_reporting_7501_has_a_status_and_send_wait_waits_for_its_turn() {
    let s = Session::new();
    let agent = s.file(
        "agent.ps1",
        r#"$e = [char]27
function report($s) { [Console]::Out.Write("$e]7501;$s$e\"); [Console]::Out.Flush() }
report "state=idle"
while ($true) {
  $line = [Console]::In.ReadLine()
  report "state=working"
  Start-Sleep 1
  [Console]::Out.WriteLine("answer to: $line")
  report "state=done"
}
"#,
    );
    let t = s.bg(&["powershell", "-NoProfile", "-File", &agent]);
    wait_until("idle", || s.json(&["get", &t])["status"]["state"] == "idle");
    let sent = s.json(&["send", &t, "hi", "--wait", "--timeout", "15"]);
    assert_eq!(sent["wait"]["status"], "done", "{sent}");
    assert_eq!(sent["wait"]["turn"], 1, "{sent}");
    assert!(s.read(&t).contains("answer to: hi"));
}

#[test]
fn kill_ends_what_the_program_started_too() {
    let s = Session::new();
    let pid_file = s.root.join("pid.txt");
    let child =
        s.file("child.ps1", &format!("$PID | Out-File -Encoding ascii '{}'; Start-Sleep 60", pid_file.display()));
    let t = s.bg(&["cmd", "/c", &format!("powershell -NoProfile -File {child}")]);
    wait_until("grandchild started", || std::fs::read_to_string(&pid_file).is_ok_and(|p| !p.trim().is_empty()));
    let pid: u64 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    assert!(alive(pid));
    s.json(&["kill", &t]);
    wait_until("grandchild gone", || !alive(pid));
}

#[test]
fn interrupt_is_ctrl_c_the_program_takes() {
    // Started from a context that ignores Ctrl+C (an ssh session, a detached
    // parent), a program must still get it.
    let s = Session::new();
    let t = s.bg(&["ping", "-t", "127.0.0.1"]);
    s.shows(&t, "Reply from");
    s.json(&["interrupt", &t, "--timeout", "5"]);
    wait_until("ping stopped", || s.json(&["get", &t])["state"] == "finished");
}

#[test]
fn restart_and_a_core_crash_keep_terminals_and_stop_leaves_nothing() {
    let s = Session::new();
    let t = s.bg(&["ping", "-n", "120", "127.0.0.1"]);
    let program = s.json(&["get", &t])["pid"].as_u64().unwrap();
    let status = s.json(&["server", "status"]);
    let holder = status["holder"]["pid"].as_u64().unwrap();

    s.json(&["server", "restart"]);
    assert_eq!(s.json(&["get", &t])["state"], "live");

    let core = s.json(&["server", "status"])["pid"].as_u64().unwrap();
    Command::new("taskkill").args(["/F", "/PID", &core.to_string()]).output().unwrap();
    wait_until("core back", || !alive(core) && s.out(&["server", "status"]).0 == 0);
    let after = s.json(&["server", "status"]);
    assert_eq!(after["holder"]["pid"].as_u64(), Some(holder), "same holder: {after}");
    assert_eq!(s.json(&["get", &t])["state"], "live");

    let core = after["pid"].as_u64().unwrap();
    s.json(&["server", "stop"]);
    wait_until("nothing left", || ![program, holder, core].into_iter().any(alive));
}

#[test]
fn a_shell_behind_a_launcher_reports_its_own_folder_and_arguments() {
    let s = Session::new();
    let dir = s.root.join("child folder");
    std::fs::create_dir(&dir).unwrap();
    let expected = std::fs::canonicalize(&dir).unwrap();
    let t = s.bg(&["cmd.exe", "/d", "/c", "cmd.exe", "/d", "/q"]);
    s.out(&["send", &t, &format!("cd /d \"{}\"", dir.display())]);
    wait_until("the child shell's folder and command line", || {
        let terminal = s.json(&["get", &t]);
        let f = &terminal["foreground"];
        std::fs::canonicalize(f["cwd"].as_str().unwrap_or_default()).ok().as_ref() == Some(&expected)
            && f["pid"] != terminal["pid"]
            && f["argv"].as_array().is_some_and(|args| args.iter().any(|a| a == "/q"))
    });
}

#[test]
fn a_shell_in_another_console_does_not_supply_this_terminals_folder() {
    let s = Session::new();
    let here = s.root.join("this console");
    let there = s.root.join("other console");
    std::fs::create_dir(&here).unwrap();
    std::fs::create_dir(&there).unwrap();
    let script = s.file(
        "separate-console.ps1",
        &format!(
            r#"
Start-Process cmd.exe -ArgumentList '/d','/q','/k' -WorkingDirectory '{}' -WindowStyle Hidden
[Environment]::CurrentDirectory = '{}'
Write-Output 'separate-console-ready'
while ($true) {{ Start-Sleep 1 }}
"#,
            there.display(),
            here.display()
        ),
    );
    let t = s.bg(&["powershell.exe", "-NoProfile", "-File", &script]);
    s.shows(&t, "separate-console-ready");
    let expected = std::fs::canonicalize(&here).unwrap();
    wait_until("only this console supplies foreground metadata", || {
        let terminal = s.json(&["get", &t]);
        let f = &terminal["foreground"];
        f["pid"] == terminal["pid"]
            && std::fs::canonicalize(f["cwd"].as_str().unwrap_or_default()).ok().as_ref() == Some(&expected)
    });
}

#[test]
fn a_32_bit_shell_reports_its_folder_and_arguments() {
    let s = Session::new();
    let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"SysWOW64\cmd.exe");
    let dir = s.root.join("x86 folder");
    std::fs::create_dir(&dir).unwrap();
    let expected = std::fs::canonicalize(&dir).unwrap();
    let t = s.bg(&[shell.to_str().unwrap(), "/d", "/q"]);
    s.out(&["send", &t, &format!("cd /d \"{}\"", dir.display())]);
    wait_until("the 32-bit shell's folder and command line", || {
        let terminal = s.json(&["get", &t]);
        let f = &terminal["foreground"];
        std::fs::canonicalize(f["cwd"].as_str().unwrap_or_default()).ok().as_ref() == Some(&expected)
            && f["argv"].as_array().is_some_and(|args| args.iter().any(|a| a == "/q"))
    });
}

#[test]
fn the_folder_a_shell_moves_to_is_seen_and_a_lost_kept_shell_returns_there() {
    let s = Session::new();
    let dir = s.root.join("moved here");
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.display().to_string();
    // TEMP may use an 8.3 name; PowerShell expands it to the same folder's long name.
    let expected = std::fs::canonicalize(&dir).unwrap();
    let cwd =
        |t: &str| std::fs::canonicalize(s.json(&["get", t])["foreground"]["cwd"].as_str().unwrap_or_default()).ok();

    let cmd = s.bg(&["cmd.exe"]);
    s.out(&["send", &cmd, &format!("cd /d \"{dir}\"")]);
    wait_until("cmd's folder", || cwd(&cmd).as_ref() == Some(&expected));

    // PowerShell keeps its location to itself: hc's own shell tells Windows.
    let shell = s.bg(&["--keep"]);
    s.out(&["send", &shell, &format!("Set-Location '{dir}'")]);
    wait_until("PowerShell's folder", || cwd(&shell).as_ref() == Some(&expected));

    let old = s.json(&["get", &shell])["pid"].as_u64().unwrap();
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_u64().unwrap();
    Command::new("taskkill").args(["/F", "/PID", &holder.to_string()]).output().unwrap();
    wait_until("replacement shell", || {
        let r = s.json(&["get", &shell]);
        r["state"] == "live" && r["pid"].as_u64().is_some_and(|p| p != old)
    });
    assert_eq!(std::fs::canonicalize(s.json(&["get", &shell])["cwd"].as_str().unwrap()).unwrap(), expected);
}

#[test]
fn programs_get_the_cores_answers_to_their_questions() {
    let conpty = Path::new(BIN).parent().unwrap().join(r"conpty\conpty.dll");
    assert!(
        conpty.is_file(),
        "Windows' own ConPTY swallows answers: run scripts\\fetch-conpty.ps1 -Dest {}",
        Path::new(BIN).parent().unwrap().display()
    );
    let s = Session::new();
    let query = s.file(
        "query.ps1",
        r#"$e = [char]27
[Console]::Out.Write("$e]7501;?$e\"); [Console]::Out.Flush()
Start-Sleep 2
$got = ""
while ([Console]::KeyAvailable) { $got += [Console]::ReadKey($true).KeyChar }
[Console]::Out.WriteLine("GOT:" + ($got -replace "$e", "ESC"))
"#,
    );
    let (_, out) = s.out(&["run", "--timeout", "20", "powershell", "-NoProfile", "-File", &query]);
    assert!(out.contains("GOT:ESC]7501;?:states="), "{out}");
}

#[test]
fn a_terminal_started_from_inside_a_terminal_has_it_as_parent() {
    let s = Session::new();
    let parent = s.bg(&["powershell", "-NoProfile"]);
    s.shows(&parent, "PS ");
    s.json(&["send", &parent, &format!("& '{BIN}' run --bg --name kid ping -n 30 127.0.0.1")]);
    wait_until("kid started", || s.out(&["get", "kid"]).0 == 0);
    assert_eq!(s.json(&["get", "kid"])["parent"], parent.as_str());
}

#[test]
fn attach_types_into_the_program_and_detaches_on_its_key() {
    // hc attach runs in a ConPTY console of its own, like over ssh.
    let s = Session::new();
    let inner = s.bg(&["cmd"]);
    s.shows(&inner, ">");
    let outer = s.bg(&[BIN, "attach", &inner]);
    s.shows(&outer, ">");
    s.typed(&outer, "echo typed-through");
    wait_until("inner ran it", || s.read(&inner).lines().any(|l| l.trim() == "typed-through"));
    s.out(&["keys", &outer, "ctrl+\\"]);
    wait_until("attach ended", || s.json(&["get", &outer])["state"] == "finished");
    assert!(s.read(&outer).contains("detached"), "{}", s.read(&outer));
    let screen = s.read(&inner);
    assert!(!screen.contains("[6;") && !screen.contains("^["), "a reply reached the program: {screen}");
    assert_eq!(s.json(&["get", &inner])["state"], "live");
}

#[test]
fn configured_shell_uses_argv_and_is_read_live_for_new_and_recovered_terminals() {
    let s = Session::new();
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let exe = s.root.join("shell with spaces.exe");
    std::fs::copy(std::env::var("COMSPEC").unwrap(), &exe).unwrap();
    let set_shell = |marker: &str| {
        let argv = serde_json::json!([exe, "/d", "/k", format!("echo {marker}")]);
        std::fs::write(&config, format!("shell = {argv}\n")).unwrap();
    };
    s.json(&["server", "start"]);
    set_shell("configured-first");
    let t = s.bg(&["--keep"]);
    s.shows(&t, "configured-first");
    let explicit = s.bg(&["cmd.exe", "/d", "/k", "echo explicit-program"]);
    s.shows(&explicit, "explicit-program");
    assert!(!s.read(&explicit).contains("configured-first"));
    let old = s.json(&["get", &t])["pid"].as_u64().unwrap();
    set_shell("configured-recovery");
    s.json(&["server", "stop"]);
    wait_until("configured shell restored", || s.json(&["get", &t])["pid"].as_u64().is_some_and(|p| p != old));
    s.shows(&t, "configured-recovery");
    let pid = s.json(&["get", &t])["pid"].as_u64().unwrap();
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_u64().unwrap();
    Command::new("taskkill").args(["/F", "/PID", &holder.to_string()]).output().unwrap();
    wait_until("configured keep replacement", || s.json(&["get", &t])["pid"].as_u64().is_some_and(|p| p != pid));
    s.shows(&t, "configured-recovery");
    for (value, kind) in [("[]", "config"), (r#"["hc-missing-shell-24"]"#, "spawn_failed")] {
        std::fs::write(&config, format!("shell = {value}\n")).unwrap();
        let out = cmd(&s.name, &s.root).args(["run", "--bg"]).output().unwrap();
        assert!(!out.status.success());
        let err: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["kind"], kind, "{err}");
    }
}
