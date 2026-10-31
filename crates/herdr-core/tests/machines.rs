#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::json;

#[test]
fn a_machine_is_added_renamed_kept_and_forgotten() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    let peer = rohan.json(&["server", "status"])["core"]["id"].clone();

    let added = s.json(&["machine", "add", "can@rohan"]);
    assert_eq!((&added["name"], &added["id"], &added["target"]), (&json!("rohan"), &peer, &json!("can@rohan")));
    assert_eq!((&added["via"], &added["temporary"]), (&json!(false), &json!(false)), "ssh, kept: {added}");

    s.json(&["machine", "rename", "rohan", "vps"]);
    s.hc(&["server", "restart"]);
    let machines = s.json(&["machine", "ls"])["machines"].clone();
    assert_eq!((machines[0]["name"].as_str(), machines.as_array().unwrap().len()), (Some("vps"), 1), "{machines}");

    s.json(&["machine", "rm", "vps"]);
    assert_eq!(s.json(&["machine", "ls"])["machines"], json!([]));
}

#[test]
fn long_host_labels_get_short_names_and_numbered_collisions() {
    let first = Session::new();
    let second = Session::new();
    let third = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("node-07", &first), ("node-08", &second), ("node-09", &third)]));
    for (host, expected) in [
        ("alice@build-runner-staging-eu-west-3-node-07.example.com", "build-runner-staging-eu-west-3-n"),
        ("alice@build-runner-staging-eu-west-3-node-08.example.com", "build-runner-staging-eu-west-3-2"),
        ("alice@build-runner-staging-eu-west-3-node-09.example.com", "build-runner-staging-eu-west-3-3"),
    ] {
        let added = s.json(&["machine", "add", host, "--session", "test"]);
        assert_eq!(added["name"], expected, "{added}");
        assert_eq!(added["target"], host, "the connection target is never truncated");
        assert_eq!(added["session"], "test");
    }
    assert_eq!(s.json(&["machine", "ls"])["machines"].as_array().unwrap().len(), 3);
    let (_, error) = s.err(&["--json", "machine", "add", "other", "--name", "build-runner-staging-eu-west-3-n"]);
    assert_eq!(error["error"]["kind"], "name_taken", "explicit names still reject collisions");
}

#[test]
fn concurrent_machine_adds_allocate_names_from_the_registered_machines() {
    let first = Session::new();
    let second = Session::new();
    let mut s = Session::new();
    let (_, ssh) = fake_ssh(&s, &[("node-07", &first), ("node-08", &second)]);
    let gate = s.root.join("gated-ssh");
    std::fs::write(
        &gate,
        format!(
            "#!/bin/sh\ntouch {root}/\"$1\"\nwhile [ ! -e {root}/release ]; do sleep 0.02; done\nexec {ssh} \"$@\"\n",
            root = s.root.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gate, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), gate.display().to_string()));
    let hosts = ["build-runner-staging-eu-west-3-node-07", "build-runner-staging-eu-west-3-node-08"];
    let children: Vec<_> = hosts
        .iter()
        .map(|host| {
            s.cmd()
                .args(["--json", "machine", "add", host])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    wait_until("both adds connecting", || hosts.iter().all(|host| s.root.join(host).exists()));
    touch(&s, "release");
    let mut names: Vec<_> = children
        .into_iter()
        .map(|child| {
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["name"].as_str().unwrap().to_string()
        })
        .collect();
    names.sort();
    assert_eq!(names, ["build-runner-staging-eu-west-3-2", "build-runner-staging-eu-west-3-n"]);
}

#[test]
fn a_windows_shell_with_sh_on_path_retries_the_native_line() {
    let rohan = Session::new();
    let mut s = Session::new();
    let (_, ssh) = fake_ssh(&s, &[("rohan", &rohan)]);
    let wrapper = s.root.join("msys-ssh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \"$2\" in sh\\ *) exit 0;; esac\nexec {ssh} rohan '{} connect'\n",
            rohan.bin.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), wrapper.display().to_string()));
    let peer = rohan.json(&["server", "status"])["core"]["id"].clone();
    let added = s.json(&["machine", "add", "windows-msys"]);
    assert_eq!(added["id"], peer, "{added}");
    s.json(&["machine", "rm", "windows-msys"]);
}

#[test]
fn powershell_with_git_sh_retries_after_a_mangled_script() {
    let rohan = Session::new();
    let mut s = Session::new();
    let (_, ssh) = fake_ssh(&s, &[("rohan", &rohan)]);
    let wrapper = s.root.join("powershell-ssh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \"$2\" in sh\\ *) echo \"is: -c: line 2: syntax error: unexpected end of file from '{{' command on line 1\" >&2; exit 2;; esac\nexec {ssh} rohan '{} connect'\n",
            rohan.bin.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), wrapper.display().to_string()));
    let added = s.json(&["machine", "add", "windows-powershell"]);
    assert_eq!(added["id"], rohan.json(&["server", "status"])["core"]["id"], "{added}");
    s.json(&["machine", "rm", "windows-powershell"]);
}

#[test]
fn a_missing_unix_core_does_not_try_the_windows_line() {
    let mut s = Session::new();
    let wrapper = s.root.join("missing-unix-ssh");
    let calls = s.root.join("calls");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho call >> '{}'\necho 'herdr-core is not installed here' >&2\nexit 127\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), wrapper.display().to_string()));
    let (_, missing) = s.err(&["--json", "machine", "add", "unix-no-core"]);
    assert_eq!(missing["error"]["kind"], "hc_missing", "{missing}");
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "call\n");
}

#[test]
fn a_windows_probe_keeps_a_native_protocol_mismatch() {
    let mut s = Session::new();
    let wrapper = s.root.join("incompatible-ssh");
    std::fs::write(&wrapper, "#!/bin/sh\ncase \"$2\" in sh\\ *) exit 0;; esac\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"data\":{\"kind\":\"unsupported_generation\"}}}'\n").unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), wrapper.display().to_string()));
    let (_, error) = s.err(&["--json", "machine", "add", "old-windows"]);
    assert_eq!(error["error"]["kind"], "incompatible", "{error}");
}

#[test]
fn a_windows_probe_keeps_native_core_errors() {
    let mut s = Session::new();
    let wrapper = s.root.join("unavailable-ssh");
    std::fs::write(&wrapper, "#!/bin/sh\ncase \"$2\" in sh\\ *) exit 0;; esac\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"native startup failed\",\"data\":{\"kind\":\"unavailable\"}}}'\n").unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), wrapper.display().to_string()));
    let (_, error) = s.err(&["--json", "machine", "add", "unavailable-windows"]);
    assert_eq!(error["error"]["kind"], "unavailable", "{error}");
    assert_eq!(error["error"]["message"], "native startup failed", "{error}");
}

#[test]
fn a_windows_machine_with_git_sh_installs_the_native_build_when_missing() {
    for uname in ["MINGW64_NT-10.0 x86_64", "MSYS_NT-10.0 x86_64"] {
        let rohan = Session::new();
        let mut s = Session::new();
        let (_, ssh) = fake_ssh(&s, &[("rohan", &rohan)]);
        let bin = s.root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let releases = s.root.join(format!("releases/core/{}", env!("CARGO_PKG_VERSION")));
        std::fs::create_dir_all(&releases).unwrap();
        std::fs::write(releases.join("windows.zip"), b"windows test build").unwrap();
        std::fs::write(
            releases.join("manifest.json"),
            json!({
                "version": env!("CARGO_PKG_VERSION"), "generation": 1,
                "targets": {"windows-x86_64": {
                    "url": format!("file://{}/windows.zip", releases.display()),
                    "sha256": "2ab258378fc8f9fb3de76603fe051843b98403eb35246087e05a6912ab85b88d"
                }}
            })
            .to_string(),
        )
        .unwrap();
        let wrapper = bin.join("ssh");
        std::fs::write(&wrapper, format!(r#"#!/bin/sh
case "$2" in
  sh\ *) exit 0;;
  'uname -sm') echo '{uname}'; exit 0;;
  powershell\ *) [ -f '{root}/copied' ] || exit 1; touch '{root}/installed'; exit 0;;
  *--version) echo 'herdr-core test'; exit 0;;
esac
if [ ! -f '{root}/installed' ]; then
  echo "'.\\AppData\\Local\\Programs\\herdr-core\\herdr-core.exe' is not recognized as an internal or external command" >&2
  exit 1
fi
exec {ssh} rohan '{core} connect'
"#, root = s.root.display(), core = rohan.bin.display())).unwrap();
        let scp = bin.join("scp");
        std::fs::write(
            &scp,
            format!(
                r#"#!/bin/sh
for arg do
  case "$arg" in
    */hc-install.zip) [ "$(cat "$arg")" = 'windows test build' ] || exit 1; zip=yes;;
    */hc-install.ps1) [ -s "$arg" ] || exit 1; script=yes;;
  esac
done
[ "$zip:$script" = yes:yes ] || exit 1
touch '{root}/copied'
"#,
                root = s.root.display()
            ),
        )
        .unwrap();
        for file in [&wrapper, &scp] {
            std::fs::set_permissions(file, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        s.env.push(("PATH".into(), format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())));
        s.env.push(("HC_SSH".into(), wrapper.display().to_string()));
        s.env.push(("HERDR_RELEASES_URL".into(), format!("file://{}", s.root.join("releases").display())));
        let (_, missing) = s.err(&["--json", "machine", "add", "windows-msys"]);
        assert_eq!(missing["error"]["kind"], "hc_missing", "{missing}");
        let added = s.json(&["machine", "add", "windows-msys", "--install"]);
        assert_eq!(added["id"], rohan.json(&["server", "status"])["core"]["id"], "{added}");
        assert!(s.root.join("installed").exists(), "the native installer ran");
        s.json(&["machine", "rm", "windows-msys"]);
    }
}

#[test]
fn a_failed_windows_probe_keeps_the_original_posix_error() {
    let mut s = Session::new();
    let wrapper = s.root.join("closed-ssh");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\ncase \"$2\" in sh\\ *) exit 0;; esac\necho 'Windows retry error' >&2; exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push(("HC_SSH".into(), wrapper.display().to_string()));
    let (_, error) = s.err(&["--json", "machine", "add", "posix"]);
    assert_eq!(error["error"]["kind"], "unreachable", "{error}");
    assert!(error["error"]["message"].as_str().unwrap().ends_with("the connection closed"), "{error}");
    assert!(!error.to_string().contains("Windows retry error"), "{error}");
}

#[test]
fn a_machine_shows_the_version_its_line_last_saw() {
    // Updated there since it was added (hc machine update, or by hand).
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    s.hc(&["server", "stop"]);
    let saved = s.root.join("state/herdr-core/test/machines.json");
    let stale = std::fs::read_to_string(&saved).unwrap().replace(env!("CARGO_PKG_VERSION"), "0.0.1");
    std::fs::write(&saved, stale).unwrap();
    wait_until("version refreshed", || {
        s.json(&["machine", "ls"])["machines"][0]["version"] == env!("CARGO_PKG_VERSION")
    });
}

#[test]
fn another_session_on_the_same_box_is_another_machine() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    let first = s.json(&["machine", "add", "rohan"]);
    let other = s.json(&["machine", "add", "rohan", "--session", "scratch", "--name", "rohan-scratch"]);
    s.json(&["machine", "rm", "rohan-scratch"]); // else its line starts that core again
    // A dial already on its way when the machine was removed can still start
    // that core (the fake ssh's `hc connect` outlives it): stop until none comes back.
    let stop = || {
        let out = rohan.cmd().env("HC_SESSION", "scratch").args(["server", "stop"]).output().unwrap();
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["stopped"] == true
    };
    wait_until("no scratch core comes back", || {
        stop();
        std::thread::sleep(std::time::Duration::from_millis(300));
        !stop()
    });
    assert_ne!(other["id"], first["id"]);
    assert_eq!(other["session"], "scratch");
}

#[test]
fn a_box_that_asks_for_a_code_is_added_from_a_terminal() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    let (_, piped) = s.err(&["--json", "machine", "add", "otp-rohan"]);
    assert_eq!(piped["error"]["kind"], "needs_login", "{piped}");

    let t = run(&s, &["sh", "-c", &format!("{BIN} machine add otp-rohan; echo added-$?; sleep 100")]);
    wait_until("the code prompt", || text(&s, &t).contains("Verification code:"));
    s.json(&["send", &t, "123456"]);
    wait_until("added", || text(&s, &t).contains("added-0"));
    assert_eq!(s.json(&["machine", "ls"])["machines"][0]["name"], "otp-rohan");

    // The login lapses and the line drops: the line waits for a person.
    // (Connected first: the line's first dial is what starts hc there.)
    wait_until("connected", || s.json(&["machine", "ls"])["machines"][0]["state"] == "connected");
    std::fs::remove_file(s.root.join("otp-ok")).unwrap();
    rohan.hc(&["server", "restart"]);
    wait_until("needs login", || s.json(&["machine", "ls"])["machines"][0]["state"] == "needs_login");
    let t = run(&s, &["sh", "-c", &format!("{BIN} machine login otp-rohan; sleep 100")]);
    wait_until("the code prompt", || text(&s, &t).contains("Verification code:"));
    s.json(&["send", &t, "123456"]);
    wait_until("the line back at once", || text(&s, &t).contains("logged in to otp-rohan: connected"));
}

#[test]
fn a_box_without_hc_gets_this_one_when_you_say_yes() {
    let mut bare = Session::new();
    bare.bin = bare.root.join("bin/herdr-core"); // nothing there: no hc on that box
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("bare", &bare)]));
    let (_, piped) = s.err(&["--json", "machine", "add", "bare"]);
    assert_eq!(piped["error"]["kind"], "hc_missing");
    assert!(piped["error"]["hint"].as_str().unwrap().contains("in a terminal"), "{piped}");

    let t = run(&s, &["sh", "-c", &format!("{BIN} machine add bare; echo added-$?; sleep 100")]);
    wait_until("the offer", || text(&s, &t).contains("[Y/n]"));
    s.json(&["send", &t, "y"]);
    wait_until("added", || text(&s, &t).contains("added-0"));
    bare.bin = bare.root.join(".local/bin/herdr-core");
    assert!(bare.bin.exists() && bare.root.join(".local/bin/hc").exists(), "installed, with hc beside it");
    wait_until("connected", || s.json(&["machine", "ls"])["machines"][0]["state"] == "connected");

    let before = bare.json(&["server", "status"])["pid"].clone();
    let updated = s.json(&["machine", "update", "bare"]);
    assert!(updated["version"].as_str().is_some_and(|v| v.starts_with("hc ")), "{updated}");
    assert_ne!(bare.json(&["server", "status"])["pid"], before, "its core restarted on the new binary");
    wait_until("connected again", || s.json(&["machine", "ls"])["machines"][0]["state"] == "connected");
}

#[test]
fn a_machine_reached_through_any_command_works_like_one_over_ssh() {
    let rohan = Session::new();
    let mut bare = Session::new();
    bare.bin = bare.root.join("bin/herdr-core"); // no hc there
    let s = Session::new();
    // The fake ssh stands in for a provider's own command; hc's ssh is not involved.
    let (_, command) = fake_ssh(&s, &[("rohan", &rohan), ("bare", &bare)]);
    let via = format!("{command} rohan");
    let (_, nameless) = s.err(&["--json", "machine", "add", "--via", &via]);
    assert_eq!(nameless["error"]["kind"], "invalid_params", "{nameless}");

    let added = s.json(&["machine", "add", "--via", &via, "--name", "box", "--temp"]);
    let reach = |m: &serde_json::Value| (m["target"].clone(), m["via"].clone(), m["temporary"].clone());
    assert_eq!(reach(&added), (json!(via), json!(true), json!(true)), "{added}");
    let out = s.hc(&["exec", "--on", "box", "echo", "hi there"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi there\n");
    s.hc(&["server", "restart"]);
    wait_until("back, as it was", || {
        let m = s.json(&["machine", "ls"])["machines"][0].clone();
        m["state"] == "connected" && reach(&m) == reach(&added)
    });

    // Some connection wrappers collapse all remote failures to exit code 1.
    // A box without hc still gets this one through that command; --install approves the copy.
    let bare_via = format!("f() {{ {command} bare \"$@\" || exit 1; }}; f");
    let (_, missing) = s.err(&["--json", "machine", "add", "--via", &bare_via, "--name", "bare"]);
    assert_eq!(missing["error"]["kind"], "hc_missing", "{missing}");
    s.json(&["machine", "add", "--via", &bare_via, "--name", "bare", "--install"]);
    bare.bin = bare.root.join(".local/bin/herdr-core"); // so it is stopped at the end
    assert!(bare.bin.exists());
    s.json(&["machine", "rm", "box"]);
}

#[test]
fn a_login_banner_does_not_get_in_the_way() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "noisy-rohan"]);
}

#[test]
fn adding_says_what_went_wrong_and_saves_nothing() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan), ("self", &s)]));
    s.json(&["machine", "add", "rohan"]);
    let kind = |args: &[&str]| s.err(&[&["--json", "machine"], args].concat()).1["error"]["kind"].clone();

    assert_eq!(kind(&["add", "nohc"]), "hc_missing");
    let (_, down) = s.err(&["--json", "machine", "add", "down"]);
    assert_eq!(down["error"]["kind"], "unreachable");
    assert!(down["error"]["message"].as_str().unwrap().contains("Could not resolve"), "{down}");
    assert_eq!(kind(&["add", "self"]), "invalid_params");
    assert_eq!(kind(&["add", "rohan"]), "already_added");
    assert_eq!(kind(&["add", "rohan", "--name", "again"]), "already_added");
    // The same core reached at another address: a copy of rohan's disk.
    let (_, copy) = s.err(&["--json", "machine", "add", "can@rohan-copy", "--name", "copy"]);
    assert!(copy["error"]["message"].as_str().unwrap().contains("looks like a copy of rohan"), "{copy}");
    for bad in ["Rohan", "t7", "a b", "-x", ""] {
        assert_eq!(kind(&["add", "rohan", &format!("--name={bad}")]), "invalid_params", "{bad:?}");
    }
    assert_eq!(kind(&["rename", "nope", "x"]), "machine_unknown");
    assert_eq!(kind(&["rm", "nope"]), "machine_unknown");
    assert_eq!(s.json(&["machine", "ls"])["machines"].as_array().unwrap().len(), 1);
}
