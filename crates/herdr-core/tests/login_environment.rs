#![cfg(unix)]

mod common;
use common::*;

#[test]
fn a_login_profile_running_hc_captures_only_once() {
    use std::os::unix::fs::PermissionsExt;
    let mut s = Session::new();
    let shell = s.root.join("login-shell");
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\necho capture >> \"$HOME/captures\"\n\
             if [ \"$(wc -l < \"$HOME/captures\")\" -lt 4 ]; then '{BIN}' ls >/dev/null; fi\n\
             [ \"$1\" != -l ] || shift\nexec /bin/sh \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.env.extend([("SSH_CONNECTION".into(), "test".into()), ("SHELL".into(), shell.to_str().unwrap().into())]);
    s.json(&["ls"]);
    assert_eq!(std::fs::read_to_string(s.root.join("captures")).unwrap(), "capture\n");
}

#[test]
fn failed_login_profile_cleans_up_its_background_child() {
    let s = ssh_session("sleep 60 &\necho $! > \"$HOME/profile-child\"\nexit 17\n");
    assert_eq!(value(&s), "");
    let pid = std::fs::read_to_string(s.root.join("profile-child")).unwrap();
    let pid: i32 = pid.trim().parse().unwrap();
    let dead = || {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z")
        }
        #[cfg(not(target_os = "linux"))]
        unsafe {
            libc::kill(pid, 0) != 0
        }
    };
    for _ in 0..100 {
        if dead() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    panic!("profile child survived fallback");
}

#[test]
fn login_profile_output_is_bounded_while_it_runs() {
    for redirect in ["", " >&2"] {
        let s = ssh_session(&format!("head -c 4194304 /dev/zero{redirect}\ntouch \"$HOME/overrun\"\n"));
        assert_eq!(value(&s), "");
        assert!(!s.root.join("overrun").exists(), "writer ran beyond the capture budget");
        let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
        assert!(log.contains("exceeded 1 MiB"), "missing output-limit diagnostic");
        assert!(log.len() < (1 << 20) + 1024);
    }
}

#[test]
fn login_profile_exit_chatter_preserves_exports() {
    let s = ssh_session("trap 'echo goodbye' EXIT\nexport LOGIN_TEST=loaded\n");
    assert_eq!(value(&s), "loaded");
}

#[test]
fn holder_environment_does_not_reload_login_profile() {
    let mut s = ssh_session("export LOGIN_TEST=\"x$LOGIN_TEST\"\n");
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "100".into()));
    assert_eq!(value(&s), "x");
    let terminal = run(&s, &["sleep", "60"]);
    let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until("holder starts a replacement core", || {
        let Ok(stream) = herdr_core::platform::connect(&s.socket()) else { return false };
        herdr_core::client::call(&stream, "server.status", serde_json::json!({}))
            .is_ok_and(|status| status["pid"].as_i64() != Some(pid as i64))
    });
    assert_eq!(value(&s), "x", "holder recovery must keep the already loaded environment");
    s.json(&["kill", &terminal]);
}

fn ssh_session(profile: &str) -> Session {
    let mut s = Session::new();
    s.env.extend([("SSH_CONNECTION".into(), "test".into()), ("SHELL".into(), "/bin/bash".into())]);
    std::fs::write(s.root.join(".bash_profile"), profile).unwrap();
    s
}

fn value(s: &Session) -> String {
    let out = s.hc(&["exec", "/bin/sh", "-c", "printf '%s' \"$LOGIN_TEST\""]);
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn ssh_start_loads_login_exports_once_and_keeps_them_across_restart() {
    let s = ssh_session("echo profile chatter\nexport LOGIN_TEST='login value'\n");
    assert_eq!(value(&s), "login value");
    std::fs::write(s.root.join(".bash_profile"), "export LOGIN_TEST=changed\n").unwrap();
    assert_eq!(value(&s), "login value", "reconnecting must not reload profiles");
    s.json(&["server", "restart"]);
    assert_eq!(value(&s), "login value", "a restart keeps the settled environment");
}

#[test]
fn local_unix_start_keeps_its_inherited_environment() {
    let mut s = ssh_session("export LOGIN_TEST=profile\n");
    s.env.retain(|(key, _)| key != "SSH_CONNECTION");
    s.env.push(("LOGIN_TEST".into(), "desktop".into()));
    assert_eq!(value(&s), "desktop");
}

#[test]
fn ssh_start_keeps_profile_prepends_ahead_of_incoming_path_and_retains_caller_extras() {
    use std::os::unix::fs::PermissionsExt;
    let guard =
        "case \":$PATH:\" in *\":$HOME/profile_bin:\"*) ;; *) export PATH=\"$HOME/profile_bin:$PATH\" ;; esac\n";
    let check_shell = |shell: &str, profile: &str, label: &str| {
        let mut s = Session::new();
        let profile_bin = s.root.join("profile_bin");
        let shadow_bin = s.root.join("shadow_bin");
        let caller_only_bin = s.root.join("caller_only_bin");
        for dir in [&profile_bin, &shadow_bin, &caller_only_bin] {
            std::fs::create_dir(dir).unwrap();
        }
        let write_tool = |path: std::path::PathBuf, out: &str| {
            std::fs::write(&path, format!("#!/bin/sh\nprintf '{out}'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        write_tool(profile_bin.join("tool"), "from-profile");
        write_tool(shadow_bin.join("tool"), "from-shadow");
        write_tool(caller_only_bin.join("caller-helper"), "from-caller");
        std::fs::write(s.root.join(profile), guard).unwrap();
        let incoming =
            format!("{}:{}:{}:/usr/bin:/bin", shadow_bin.display(), profile_bin.display(), caller_only_bin.display());
        s.env.extend([
            ("SSH_CONNECTION".into(), "test".into()),
            ("SHELL".into(), shell.into()),
            ("PATH".into(), incoming),
        ]);
        let tool = s.hc(&["exec", "--", "tool"]);
        assert!(tool.status.success(), "{label}: {tool:?}");
        assert_eq!(String::from_utf8_lossy(&tool.stdout), "from-profile", "{label}");
        let caller = s.hc(&["exec", "--", "caller-helper"]);
        assert!(caller.status.success(), "{label}: {caller:?}");
        assert_eq!(String::from_utf8_lossy(&caller.stdout), "from-caller", "{label}");
        let path_out = s.hc(&["exec", "printf '%s' \"$PATH\""]);
        assert!(path_out.status.success(), "{label}: {path_out:?}");
        let path = String::from_utf8_lossy(&path_out.stdout);
        let pos = |needle: &str| path.split(':').position(|entry| entry == needle);
        let profile_pos = pos(profile_bin.to_str().unwrap());
        assert!(profile_pos.is_some(), "{label}: missing profile_bin in {path}");
        assert!(profile_pos < pos("/usr/bin"), "{label}: {path}");
        assert!(profile_pos < pos(shadow_bin.to_str().unwrap()), "{label}: {path}");
    };
    check_shell("/bin/bash", ".bash_profile", "bash");
    #[cfg(target_os = "macos")]
    check_shell("/bin/zsh", ".zprofile", "zsh");
}

#[test]
fn failing_login_profile_falls_back_and_logs_the_shell_and_reason() {
    let mut s = ssh_session("echo profile failed\nexit 17\n");
    s.env.push(("LOGIN_TEST".into(), "inherited".into()));
    assert_eq!(value(&s), "inherited");
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
    assert!(log.contains("/bin/bash") && log.contains("17") && log.contains("using inherited environment"), "{log}");
}

#[test]
fn a_login_profile_cannot_block_machine_start_forever() {
    let mut s = ssh_session("sleep 30\n");
    s.env.push(("LOGIN_TEST".into(), "inherited".into()));
    let start = std::time::Instant::now();
    assert_eq!(value(&s), "inherited");
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
    assert!(log.contains("/bin/bash") && log.contains("exceeded 5 s"), "{log}");
}
