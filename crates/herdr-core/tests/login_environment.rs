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

#[cfg(target_os = "linux")]
#[test]
fn failed_login_profile_cleans_up_its_background_child() {
    let s = ssh_session("sleep 60 &\necho $! > \"$HOME/profile-child\"\nexit 17\n");
    assert_eq!(value(&s), "");
    let pid = std::fs::read_to_string(s.root.join("profile-child")).unwrap();
    let pid: i32 = pid.trim().parse().unwrap();
    for _ in 0..100 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        if stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z") {
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
fn holder_recaptures_login_environment_after_a_crash() {
    let mut s = ssh_session("export LOGIN_TEST=x\n");
    s.env.extend([("BIRTH_ONLY".into(), "inherited".into()), ("HC_HOLDER_GRACE_MS".into(), "100".into())]);
    s.json(&["server", "start"]);
    let out = s.hc(&["exec", "printf '%s' \"$BIRTH_ONLY\""]);
    assert_eq!(out.stdout, b"inherited");
    let terminal = run(&s, &["sleep", "60"]);
    std::fs::write(s.root.join(".bash_profile"), "export LOGIN_TEST=y\n").unwrap();
    let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until("holder starts a replacement core", || {
        let Ok(stream) = herdr_core::platform::connect(&s.socket()) else { return false };
        herdr_core::client::call(&stream, "server.status", serde_json::json!({}))
            .is_ok_and(|status| status["pid"].as_i64() != Some(pid as i64))
    });
    assert_eq!(value(&s), "y");
    let out = s.hc(&["exec", "printf '%s' \"${BIRTH_ONLY-unset}\""]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"unset");
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
fn local_autostart_uses_login_exports_and_keeps_caller_tools_after_the_login_path() {
    use std::os::unix::fs::PermissionsExt;

    let mut s =
        ssh_session("export LOGIN_TEST=profile\nexport PROFILE_CWD=\"$PWD\"\nexport PATH=\":$HOME/login:$PATH:\"\n");
    s.env.retain(|(key, _)| key != "SSH_CONNECTION");
    let caller = s.root.join("caller");
    for (dir, tool) in [("login", "collision"), ("caller", "collision"), ("caller", "onlycaller")] {
        let dir_path = s.root.join(dir);
        std::fs::create_dir_all(&dir_path).unwrap();
        let tool = dir_path.join(tool);
        std::fs::write(&tool, format!("#!/bin/sh\necho {dir}\n")).unwrap();
        std::fs::set_permissions(tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::copy(caller.join("onlycaller"), caller.join("cwd-only")).unwrap();
    s.env.extend([
        ("PATH".into(), format!(":{}:/usr/bin:/bin:", caller.display())),
        ("FOO_LEAK".into(), "desktop".into()),
    ]);
    let out = s.cmd().current_dir(&caller).arg("ls").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = s.hc(&[
        "exec",
        "printf '%s|%s|%s\n' \"$LOGIN_TEST\" \"${FOO_LEAK-unset}\" \"$PROFILE_CWD\"; onlycaller; collision",
    ]);
    assert!(out.status.success(), "{out:?}");
    let output = String::from_utf8_lossy(&out.stdout);
    let cwd = output.strip_prefix("profile|unset|").and_then(|s| s.strip_suffix("\ncaller\nlogin\n")).unwrap();
    assert_eq!(std::path::Path::new(cwd).canonicalize().unwrap(), s.root.canonicalize().unwrap());

    let elsewhere = s.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::rename(caller.join("cwd-only"), elsewhere.join("cwd-only")).unwrap();
    let out = s.cmd().current_dir(&elsewhere).args(["exec", "cwd-only"]).output().unwrap();
    assert!(!out.status.success(), "an empty PATH segment searches the working directory");
}

#[test]
fn failing_login_profile_falls_back_and_logs_the_shell_and_reason() {
    let mut s = ssh_session("echo private-profile-output >&2\nexit 17\n");
    use std::os::unix::fs::PermissionsExt;

    let caller_tool = s.root.join("onlycaller");
    std::fs::write(&caller_tool, "#!/bin/sh\nprintf caller\n").unwrap();
    std::fs::set_permissions(&caller_tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.env.extend([
        ("LOGIN_TEST".into(), "inherited".into()),
        ("PATH".into(), format!("{}:/usr/bin:/bin", s.root.display())),
    ]);
    assert_eq!(value(&s), "");
    let out = s.hc(&["exec", "onlycaller"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"caller");
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
    assert!(log.contains("/bin/bash") && log.contains("17"), "{log}");
    assert!(!log.contains("private-profile-output"), "{log}");
}

#[test]
fn a_stale_login_capture_marker_does_not_block_a_new_start() {
    let mut s = ssh_session("export LOGIN_TEST=loaded\n");
    s.env.retain(|(key, _)| key != "SSH_CONNECTION");
    s.env.push(("HC_LOGIN_CAPTURE".into(), "stale".into()));
    s.json(&["server", "start"]);
    let out = s.hc(&["exec", "printf '%s' \"${HC_LOGIN_CAPTURE-unset}\""]);
    assert_eq!(out.stdout, b"unset");
    s.json(&["server", "stop"]);
    assert_eq!(value(&s), "loaded");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a disposable systemd user manager"]
fn systemd_autostart_overlays_login_exports_on_the_user_manager_environment() {
    use std::process::Command;

    struct ManagerExports([String; 2]);
    impl Drop for ManagerExports {
        fn drop(&mut self) {
            let _ = Command::new("systemctl").args(["--user", "unset-environment"]).args(&self.0).status();
        }
    }

    let exports = ManagerExports([
        format!("HC_MANAGER_ONLY_{}", std::process::id()),
        format!("HC_MANAGER_COLLISION_{}", std::process::id()),
    ]);
    let [only, collision] = &exports.0;
    let status = Command::new("systemctl")
        .args(["--user", "set-environment"])
        .args([format!("{only}=manager"), format!("{collision}=manager")])
        .status()
        .unwrap();
    assert!(status.success());

    let mut s = ssh_session(&format!("export {collision}=login\n"));
    s.env.retain(|(key, _)| key != "SSH_CONNECTION");
    s.env.extend([
        ("XDG_RUNTIME_DIR".into(), format!("/run/user/{}", unsafe { libc::getuid() })),
        ("HC_SOCKET".into(), s.root.join("core.sock").display().to_string()),
        ("HC_TEST_CALLER_ONLY".into(), "caller".into()),
    ]);
    s.json(&["ls"]);
    let pid = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    assert!(cgroup.contains("/hc-serve-"), "{cgroup}");
    let out = s.hc(&["exec", "env"]);
    assert!(out.status.success(), "{out:?}");
    let env = String::from_utf8(out.stdout).unwrap();
    assert!(env.lines().any(|line| line == format!("{only}=manager")), "{env}");
    assert!(env.lines().any(|line| line == format!("{collision}=login")), "{env}");
    assert!(!env.contains("HC_TEST_CALLER_ONLY="), "{env}");
}

#[test]
fn a_login_profile_cannot_block_machine_start_forever() {
    let mut s = ssh_session("sleep 30\n");
    s.env.push(("LOGIN_TEST".into(), "inherited".into()));
    let start = std::time::Instant::now();
    assert_eq!(value(&s), "");
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let log = std::fs::read_to_string(s.root.join("state/herdr-core/test/core.log")).unwrap();
    assert!(log.contains("/bin/bash") && log.contains("exceeded 5 s"), "{log}");
}
