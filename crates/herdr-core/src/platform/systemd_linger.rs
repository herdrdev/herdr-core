use std::collections::BTreeMap;
use std::io;
use std::iter::Peekable;
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::Bytes;

use super::linux_session::resolve_tool;
use super::systemd::{SESSION, user_service_runner};

static LOGOUT_LINGER: std::sync::Mutex<(Option<bool>, Option<String>)> = std::sync::Mutex::new((None, None));
static LINGER_CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub fn logout_linger() -> (Option<bool>, Option<String>) {
    LOGOUT_LINGER.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

pub fn start_logout_linger() {
    tokio::spawn(async {
        loop {
            let observed = observe_logout_linger().await;
            *LOGOUT_LINGER.lock().unwrap_or_else(|p| p.into_inner()) = observed;
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {},
                _ = LINGER_CHANGED.notified() => {},
            }
        }
    });
    if user_service_runner().is_ok() {
        linger();
    }
}

async fn observe_logout_linger() -> (Option<bool>, Option<String>) {
    if user_service_runner().is_err() {
        return (None, None);
    }
    let Some(user) = login_user() else { return (None, None) };
    let linger = linger_enabled(Path::new("/var/lib/systemd/linger"), &user);
    let survives = match linger {
        Some(false) => logout_survival(linger, stop_delay().await.as_deref()),
        other => other,
    };
    (survives, Some(user))
}

fn linger_enabled(directory: &Path, user: &str) -> Option<bool> {
    let escaped: String = user
        .bytes()
        .map(|byte| match byte {
            b'\x07' => "\\a".into(),
            b'\x08' => "\\b".into(),
            b'\x0c' => "\\f".into(),
            b'\n' => "\\n".into(),
            b'\r' => "\\r".into(),
            b'\t' => "\\t".into(),
            b'\x0b' => "\\v".into(),
            b'\\' => "\\\\".into(),
            b'\'' => "\\'".into(),
            b'"' => "\\\"".into(),
            b' '..=b'~' => char::from(byte).to_string(),
            _ => format!("\\{:03o}", byte),
        })
        .collect();
    directory.join(escaped).try_exists().ok()
}

async fn stop_delay() -> Option<String> {
    let busctl = resolve_tool("busctl")?;
    let mut command = tokio::process::Command::new(busctl);
    command.args([
        "--timeout=1",
        "get-property",
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        "UserStopDelayUSec",
    ]);
    stop_delay_command(command).await
}

async fn stop_delay_command(mut command: tokio::process::Command) -> Option<String> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).process_group(0);
    let child = command.spawn().ok()?;
    // Kill the private group too, including helpers inheriting the output pipe.
    struct ProcessGroup(u32);
    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            unsafe { libc::kill(-(self.0 as i32), libc::SIGKILL) };
        }
    }
    let group = ProcessGroup(child.id()?);
    let output = child.wait_with_output();
    tokio::pin!(output);
    let out = match tokio::time::timeout(std::time::Duration::from_secs(1), &mut output).await {
        Ok(out) => out.ok()?,
        Err(_) => {
            drop(group);
            let _ = output.await;
            return None;
        }
    };
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn logout_survival(linger: Option<bool>, stop_delay: Option<&str>) -> Option<bool> {
    if linger != Some(false) {
        return linger;
    }
    // Per-user finite delays override the global value; infinity means unset.
    // Only a finite global delay proves that logind will stop this manager.
    let delay = stop_delay?.strip_prefix("t ")?.trim().parse::<u64>().ok()?;
    (delay != u64::MAX).then_some(false)
}

fn login_user() -> Option<String> {
    // getpwuid_r keeps simultaneous status requests from sharing libc's buffer.
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0u8; 16384];
    let mut result = std::ptr::null_mut();
    let error = unsafe {
        libc::getpwuid_r(libc::getuid(), entry.as_mut_ptr(), buffer.as_mut_ptr().cast(), buffer.len(), &mut result)
    };
    if error != 0 || result.is_null() {
        return None;
    }
    Some(unsafe { std::ffi::CStr::from_ptr((*result).pw_name) }.to_string_lossy().into_owned())
}

/// Linger keeps the user manager alive independently of logind's stop delay.
pub(super) fn linger() {
    let Some(user) = login_user() else { return };
    if linger_enabled(Path::new("/var/lib/systemd/linger"), &user) == Some(true) {
        return;
    }
    // Polkit can leave loginctl waiting for 25 seconds, longer than a
    // holder's handshake deadline. Try once per process, off the startup path.
    static ATTEMPT: std::sync::Once = std::sync::Once::new();
    ATTEMPT.call_once(|| {
        if let Some(loginctl) = resolve_tool("loginctl") {
            let _ = enable_linger(Command::new(loginctl), user);
        } else {
            eprintln!(
                "hc: the core may stop when your last login ends; `sudo loginctl enable-linger -- {user}` keeps it"
            );
        }
    });
}

fn enable_linger(mut command: Command, user: String) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("hc-linger".into()).spawn(move || {
        let enabled = command
            .args(["--no-ask-password", "enable-linger", "--", &user])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        LINGER_CHANGED.notify_one();
        if enabled {
            eprintln!("hc: turned on linger for {user}, so the core outlives your logins");
        } else {
            eprintln!(
                "hc: the core may stop when your last login ends; `sudo loginctl enable-linger -- {user}` keeps it"
            );
        }
    })
}

/// The user manager's environment now (a core may have started before the
/// desktop did). Blocks up to a second: call it off the core's thread.
pub fn manager_env() -> Option<BTreeMap<String, String>> {
    let mgr = user_service_runner().ok()?;
    let busctl = resolve_tool("busctl")?;
    let out = Command::new(busctl)
        .env("XDG_RUNTIME_DIR", mgr.runtime_dir)
        .args(["--user", "--timeout=1", "get-property", "org.freedesktop.systemd1"])
        .args(["/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager", "Environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    parse_busctl_environment(&out.stdout)
}

fn parse_busctl_environment(reply_bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let reply_text = std::str::from_utf8(reply_bytes).ok()?;
    let mut header_fields = reply_text.trim().splitn(3, ' ');
    let signature = header_fields.next()?;
    if signature != "as" {
        return None;
    }
    let environment_entry_count = header_fields.next()?.parse::<usize>().ok()?;
    let mut remaining_reply = header_fields.next().unwrap_or_default().bytes().peekable();
    let mut environment = BTreeMap::new();
    for _ in 0..environment_entry_count {
        let environment_entry = parse_busctl_quoted_string(&mut remaining_reply)?;
        if let Some((variable_name, variable_value)) = environment_entry.split_once('=') {
            environment.insert(variable_name.into(), variable_value.into());
        }
    }
    if remaining_reply.any(|byte| !byte.is_ascii_whitespace()) {
        return None;
    }
    Some(environment)
}

fn parse_busctl_quoted_string(remaining_reply: &mut Peekable<Bytes<'_>>) -> Option<String> {
    while remaining_reply.peek().is_some_and(u8::is_ascii_whitespace) {
        remaining_reply.next();
    }
    if remaining_reply.next()? != b'"' {
        return None;
    }
    let mut decoded_bytes = Vec::new();
    loop {
        match remaining_reply.next()? {
            b'"' => break,
            b'\\' => decoded_bytes.push(decode_busctl_escape(remaining_reply)?),
            byte if byte.is_ascii_control() => return None,
            byte => decoded_bytes.push(byte),
        }
    }
    let has_invalid_delimiter = remaining_reply.peek().is_some_and(|byte| !byte.is_ascii_whitespace());
    if has_invalid_delimiter {
        return None;
    }
    // Octal escapes can encode individual bytes of a multibyte UTF-8 character.
    String::from_utf8(decoded_bytes).ok()
}

fn decode_busctl_escape(remaining_reply: &mut Peekable<Bytes<'_>>) -> Option<u8> {
    let escape_code = remaining_reply.next()?;
    let decoded_byte = match escape_code {
        b'a' => b'\x07',
        b'b' => b'\x08',
        b'f' => b'\x0c',
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'v' => b'\x0b',
        b'\\' => b'\\',
        b'\'' => b'\'',
        b'"' => b'"',
        b'0'..=b'3' => {
            let first_octal_digit = escape_code - b'0';
            let second_octal_digit = remaining_reply.next()?.checked_sub(b'0')?;
            let third_octal_digit = remaining_reply.next()?.checked_sub(b'0')?;
            if second_octal_digit > 7 || third_octal_digit > 7 {
                return None;
            }
            first_octal_digit * 64 + second_octal_digit * 8 + third_octal_digit
        }
        _ => return None,
    };
    if decoded_byte == b'\0' {
        return None;
    }
    Some(decoded_byte)
}

/// Puts the login session's variables the manager has into `env`.
pub fn apply_session(env: &mut BTreeMap<String, String>, manager: &BTreeMap<String, String>) {
    for &k in SESSION {
        if let Some(v) = manager.get(k) {
            env.insert(k.into(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn linger_marker_is_rechecked_after_admin_changes() {
        let root = std::env::temp_dir().join(format!("hc-linger-{}", super::super::random_hex()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(super::linger_enabled(&root, "test"), Some(false));
        std::fs::write(root.join("test"), "").unwrap();
        assert_eq!(super::linger_enabled(&root, "test"), Some(true));
        std::fs::remove_file(root.join("test")).unwrap();
        assert_eq!(super::linger_enabled(&root, "test"), Some(false));
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn escaped_account_marker_is_rechecked() {
        let root = std::env::temp_dir().join(format!("hc-linger-{}", super::super::random_hex()));
        std::fs::create_dir_all(&root).unwrap();
        let user = "DOMAIN\\o'né";
        let marker = root.join(r"DOMAIN\\o\'n\303\251");
        assert_eq!(super::linger_enabled(&root, user), Some(false));
        std::fs::write(&marker, "").unwrap();
        assert_eq!(super::linger_enabled(&root, user), Some(true));
        std::fs::remove_file(marker).unwrap();
        assert_eq!(super::linger_enabled(&root, user), Some(false));
        std::fs::remove_dir(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_policy_query_does_not_block_and_is_reaped() {
        let directory = Path::new("/var/tmp").join(format!("hc-policy-{}", super::super::random_hex()));
        std::fs::create_dir(&directory).unwrap();
        let pid_file = directory.join("pid");
        use std::os::unix::fs::PermissionsExt;
        let fake_busctl = directory.join("busctl");
        std::fs::write(&fake_busctl, "#!/bin/sh\necho $$ > \"$1\"\nexec /bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&fake_busctl, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = tokio::process::Command::new("busctl");
        command.env("PATH", &directory).arg(&pid_file);
        let query = tokio::spawn(super::stop_delay_command(command));
        tokio::time::timeout(std::time::Duration::from_millis(250), async {
            while !pid_file.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("query blocked the executor");
        let start = std::time::Instant::now();
        for _ in 0..100 {
            super::logout_linger();
            tokio::task::yield_now().await;
        }
        assert!(start.elapsed() < std::time::Duration::from_millis(250));
        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert_eq!(query.await.unwrap(), None);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "policy child leaked");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_linger_warns_only_with_a_known_finite_stop_delay() {
        for (delay, expected) in [
            (Some("t 10000000\n"), Some(false)),
            (Some("t 0\n"), Some(false)),
            (Some("t 18446744073709551615\n"), None),
            (Some("not a property"), None),
            (None, None),
        ] {
            assert_eq!(super::logout_survival(Some(false), delay), expected);
            assert_eq!(super::logout_survival(Some(true), delay), Some(true));
            assert_eq!(super::logout_survival(None, delay), None);
        }
    }

    #[test]
    fn linger_does_not_wait_for_loginctl() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 1"]);
        let start = std::time::Instant::now();
        let worker = super::enable_linger(command, "hc-test".into()).unwrap();
        let elapsed = start.elapsed();
        worker.join().unwrap();
        assert!(elapsed < std::time::Duration::from_millis(500), "loginctl blocked startup");
    }

    #[test]
    fn missing_loginctl_is_best_effort() {
        let worker = super::enable_linger(Command::new("/no-such-hc-test-loginctl"), "hc-test".into()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn manager_environment_decodes_busctl_strings_without_evaluating_values() {
        let busctl_reply = br#"as 6 "EMPTY=" "PATH=/home/u/my tools" "SPECIAL=\"double\" \'single\' \\slash $dollar `ticks`=tail" "LINES=one\ntwo\r\tend" "UTF8=\303\251\342\230\203" "CONTROLS=\a\b\f\v\177""#;
        let expected_environment = map(&[
            ("EMPTY", ""),
            ("PATH", "/home/u/my tools"),
            ("SPECIAL", "\"double\" 'single' \\slash $dollar `ticks`=tail"),
            ("LINES", "one\ntwo\r\tend"),
            ("UTF8", "é☃"),
            ("CONTROLS", "\x07\x08\x0c\x0b\x7f"),
        ]);
        let parsed_environment = super::parse_busctl_environment(busctl_reply);
        assert_eq!(parsed_environment, Some(expected_environment));
        assert_eq!(super::parse_busctl_environment(b"as 0\n"), Some(BTreeMap::new()));
        let raw_utf8_reply = "as 1 \"UTF8=é☃\"".as_bytes();
        assert_eq!(super::parse_busctl_environment(raw_utf8_reply), Some(map(&[("UTF8", "é☃")])));
    }

    #[test]
    fn manager_environment_rejects_malformed_or_partial_replies() {
        for malformed_reply in [
            &b"s \"KEY=value\""[..],
            b"as nope",
            b"as 1",
            b"as 1 \"KEY=unterminated",
            b"as 2 \"KEY=only-one\"",
            b"as 0 \"KEY=extra\"",
            b"as 2 \"A=1\"\"B=2\"",
            b"as 1 \"KEY=\\q\"",
            b"as 1 \"KEY=\\40\"",
            b"as 1 \"KEY=\\400\"",
            b"as 1 \"KEY=\\300\"",
            b"as 1 \"KEY=\\000\"",
        ] {
            assert_eq!(super::parse_busctl_environment(malformed_reply), None, "{malformed_reply:?}");
        }
    }

    #[test]
    fn programs_get_the_desktop_the_manager_has_now() {
        let mut env = map(&[("SSH_AUTH_SOCK", "/tmp/ssh-x/agent.1"), ("DISPLAY", ":1"), ("PATH", "/usr/bin")]);
        super::apply_session(&mut env, &map(&[("PATH", "/x"), ("SSH_AUTH_SOCK", "/run/user/1/my keys/ssh")]));
        assert_eq!(env["SSH_AUTH_SOCK"], "/run/user/1/my keys/ssh");
        assert_eq!((env["PATH"].as_str(), env["DISPLAY"].as_str()), ("/usr/bin", ":1"), "only what the manager has");
    }
}
