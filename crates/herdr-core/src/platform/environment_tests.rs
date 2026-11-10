#[test]
fn a_restart_restores_settings_without_reusing_paths_or_lifecycle_marks() {
    let pairs =
        |pairs: &[(&str, &str)]| pairs.iter().map(|(key, value)| ((*key).into(), (*value).into())).collect::<Vec<_>>();
    let saved = pairs(&[
        ("PATH", "saved-path"),
        ("HC_VALUE", "saved"),
        ("HOME", "old-home"),
        ("HC_SESSION", "old-session"),
        ("HC_SCOPE", "old-sweep"),
        ("HC_RUNTIME", "old-runtime"),
        ("HC_LAUNCHD_DIRECT", "old-bypass"),
        ("INVOCATION_ID", "old-service"),
        ("HC_SOCKET", "old-socket"),
    ]);
    let current = pairs(&[
        ("PATH", "caller-path"),
        ("HC_CALLER_ONLY", "must-not-leak"),
        ("HOME", "new-home"),
        ("HC_SESSION", "new-session"),
        ("HC_SCOPE", "caller-sweep"),
        ("HC_LAUNCHD_DETACH", "caller-bypass"),
        ("INVOCATION_ID", "new-service"),
        ("HC_SOCKET", "new-socket"),
    ]);
    let env = super::restored_environment(saved, current, false);
    for (key, value) in [
        ("PATH", "saved-path"),
        ("HC_VALUE", "saved"),
        ("HOME", "new-home"),
        ("HC_SESSION", "new-session"),
        ("HC_SOCKET", "new-socket"),
        ("HC_RUNTIME", "1"),
    ] {
        assert_eq!(env.get(std::ffi::OsStr::new(key)), Some(&std::ffi::OsString::from(value)));
    }
    for key in ["HC_CALLER_ONLY", "HC_SCOPE", "HC_LAUNCHD_DIRECT", "HC_LAUNCHD_DETACH"] {
        assert!(!env.contains_key(std::ffi::OsStr::new(key)), "{key}");
    }
    #[cfg(target_os = "linux")]
    assert_eq!(env.get(std::ffi::OsStr::new("INVOCATION_ID")), Some(&std::ffi::OsString::from("new-service")));
}

#[cfg(target_os = "linux")]
#[test]
fn a_fresh_ssh_environment_uses_normal_service_filtering_but_direct_keeps_its_session() {
    use std::ffi::OsStr;
    let saved = [
        ("PATH", "/bin"),
        ("SSH_CONNECTION", "ssh"),
        ("SSH_AUTH_SOCK", "/forwarded-agent"),
        ("XDG_SESSION_ID", "ssh-session"),
        ("XAUTHORITY", "/forwarded-cookie"),
        ("BASH_FUNC_f%%", "() { :; }"),
        ("NO_COLOR", "1"),
    ]
    .map(|(k, v)| (k.into(), v.into()));
    let direct = super::restored_environment(
        saved.clone(),
        [("INVOCATION_ID".into(), "inherited".into()), ("XDG_RUNTIME_DIR".into(), "/custom/run".into())],
        true,
    );
    assert_eq!(direct.get(OsStr::new("SSH_AUTH_SOCK")).unwrap(), "/forwarded-agent");
    let service = super::restored_environment(
        saved,
        [
            ("INVOCATION_ID".into(), "service".into()),
            ("XDG_RUNTIME_DIR".into(), super::systemd::runtime_dir().into_os_string()),
            ("SSH_AUTH_SOCK".into(), "/manager-agent".into()),
        ],
        true,
    );
    assert_eq!(service.get(OsStr::new("SSH_AUTH_SOCK")).unwrap(), "/manager-agent");
    assert_eq!(service.get(OsStr::new("NO_COLOR")).unwrap(), "1");
    assert_eq!(service.get(OsStr::new("INVOCATION_ID")).unwrap(), "service");
    for key in ["SSH_CONNECTION", "XDG_SESSION_ID", "XAUTHORITY", "BASH_FUNC_f%%"] {
        assert!(!service.contains_key(OsStr::new(key)), "{key}");
    }
}

#[cfg(windows)]
#[test]
fn windows_path_keys_are_matched_without_case() {
    let env = super::restored_environment(
        [("home".into(), "old".into()), ("hc_scope".into(), "old".into()), ("Path".into(), "saved".into())],
        [("HOME".into(), "new".into()), ("Path".into(), "caller".into())],
        false,
    );
    assert!(!env.contains_key(std::ffi::OsStr::new("home")));
    assert!(!env.contains_key(std::ffi::OsStr::new("hc_scope")));
    assert_eq!(env.get(std::ffi::OsStr::new("HOME")), Some(&std::ffi::OsString::from("new")));
    assert_eq!(env.get(std::ffi::OsStr::new("Path")), Some(&std::ffi::OsString::from("saved")));
}

#[cfg(target_os = "macos")]
#[test]
fn a_restart_refreshes_only_a_dead_saved_ssh_agent_socket() {
    use std::ffi::OsStr;
    let path = std::path::PathBuf::from(format!("/var/tmp/hc-agent-{}", super::random_hex()));
    let socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let restore = |current| {
        super::restored_environment([("SSH_AUTH_SOCK".into(), path.clone().into_os_string())], current, false)
    };
    let launcher = vec![("SSH_AUTH_SOCK".into(), "/new-agent".into())];
    let live = restore(launcher.clone());
    assert_eq!(live.get(OsStr::new("SSH_AUTH_SOCK")), Some(&path.clone().into_os_string()));
    // The file stays behind, as after a logout, but nothing answers.
    drop(socket);
    let stale = restore(launcher.clone());
    assert_eq!(stale.get(OsStr::new("SSH_AUTH_SOCK")).unwrap(), "/new-agent");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(restore(launcher.clone()).get(OsStr::new("SSH_AUTH_SOCK")).unwrap(), "/new-agent");
    let over_ssh = launcher.into_iter().chain([("SSH_CONNECTION".into(), "1 2 3 4".into())]).collect::<Vec<_>>();
    assert!(!restore(over_ssh).contains_key(OsStr::new("SSH_AUTH_SOCK")), "a forwarded agent is not adopted");
    assert!(!restore(Vec::new()).contains_key(OsStr::new("SSH_AUTH_SOCK")));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn xpc_flags_belong_to_neither_a_replacement_core_nor_its_programs() {
    use std::ffi::OsStr;
    let env = super::restored_environment(
        [("XPC_FLAGS".into(), "0x2".into()), ("XPC_SERVICE_NAME".into(), "old-service".into())],
        [("XPC_FLAGS".into(), "0x0".into()), ("XPC_SERVICE_NAME".into(), "0".into())],
        false,
    );
    assert!(!env.contains_key(OsStr::new("XPC_FLAGS")));
    assert_eq!(env.get(OsStr::new("XPC_SERVICE_NAME")).unwrap(), "0");
    assert!(!super::program_env().await.contains_key("XPC_FLAGS"));
}
