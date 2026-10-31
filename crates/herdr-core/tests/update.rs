#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

#[test]
fn an_up_to_date_binary_restarts_only_an_older_running_core() {
    let mut s = Session::new();
    let version = env!("CARGO_PKG_VERSION");
    std::fs::create_dir_all(s.root.join("core")).unwrap();
    std::fs::write(
        s.root.join("core/latest.json"),
        json!({"version": version, "generation": herdr_core::api::GENERATION, "targets": {}}).to_string(),
    )
    .unwrap();
    s.env.push(("HERDR_RELEASES_URL".into(), format!("file://{}", s.root.display())));
    let args = ["update", "--version", version];
    assert_eq!(s.json(&args)["updated"], false);
    assert!(!s.connect_ok(), "checking an absent core must not start one");
    std::fs::create_dir_all(s.socket().parent().unwrap()).unwrap();
    for (running, latest) in
        [(version, false), ("999.0.0", false), ("unknown", false), ("0.0.0", false), ("0.0.0", true)]
    {
        let listener = UnixListener::bind(s.socket()).unwrap();
        let socket = s.socket();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["method"], "server.status");
            writeln!(stream, "{}", json!({"id": 1, "result": {"version": running}})).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            if running == "0.0.0" {
                let req: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(req["method"], "server.restart");
                writeln!(stream, "{}", json!({"id": 1, "result": {}})).unwrap();
            } else {
                assert!(line.is_empty(), "must leave {running} running");
            }
            drop(listener);
            std::fs::remove_file(socket).unwrap();
        });
        let out = s.json(if latest { &["update"] } else { &args });
        server.join().unwrap();
        assert_eq!(out["updated"], false);
        if running == "0.0.0" {
            assert_eq!(out["restarted"], true);
            assert_eq!(s.json(&["server", "status"])["version"], version);
            s.json(&["server", "stop"]);
        } else {
            assert_ne!(out["restarted"], true);
        }
    }
}

#[test]
fn an_install_in_a_read_only_folder_explains_how_to_update_it() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // Root can replace files in read-only directories.
    }
    let mut s = Session::new();
    let dir = s.root.join("bin");
    std::fs::create_dir(&dir).unwrap();
    s.bin = dir.join("herdr-core");
    std::fs::copy(BIN, &s.bin).unwrap();
    s.json(&["server", "start"]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let status = s.cmd().args(["server", "status"]).output().unwrap();
    let update = s.cmd().args(["update", "--from", BIN]).output().unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["installed_by"], "system package manager");
    let error: Value = serde_json::from_slice(&update.stderr).unwrap();
    assert!(!update.status.success());
    assert_eq!(error["error"]["kind"], "refused", "{error}");
    assert_eq!(error["error"]["message"], format!("{} isn't writable by you", dir.canonicalize().unwrap().display()));
    assert_eq!(error["error"]["hint"], "update herdr-core the way it was installed (package manager, or sudo)");
}

#[test]
fn update_from_a_file_swaps_hc_and_keeps_everything_running() {
    let mut s = Session::new();
    s.env.push(("HC_SESSION_VALUE".into(), "original".into()));
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("herdr-core")).unwrap();
    std::os::unix::fs::symlink("herdr-core", dir.join("hc")).unwrap();
    s.bin = dir.join("hc"); // run as `hc`, the way people do
    let t = run(&s, &["sh", "-c", "echo before; while read l; do echo got-$l; done"]);
    let pid = s.json(&["server", "status"])["pid"].clone();

    let out = s
        .cmd()
        .env("NO_COLOR", "1")
        .env("HC_SESSION_VALUE", "updater")
        .env("HC_UPDATER_ONLY", "leaked")
        .args(["update", "--from", BIN])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    let real = std::fs::canonicalize(dir.join("herdr-core")).unwrap(); // macOS: /var is /private/var
    assert_eq!(r["path"], real.display().to_string(), "the file hc runs from, not the link: {r}");
    assert!(r["was"].as_str().unwrap().starts_with("hc ") && r["now"].as_str().unwrap().starts_with("hc "), "{r}");
    assert_eq!(r["restarted"], true);
    assert_ne!(s.json(&["server", "status"])["pid"], pid, "a new core runs");
    assert!(dir.join("hc").is_symlink(), "the link stays a link");
    s.json(&["send", &t, "after"]);
    wait_until("the terminal still answers", || text(&s, &t).contains("got-after"));
    let probe = "printf 'ENV:%s|%s|%s\\n' \"${NO_COLOR-unset}\" \"$HC_SESSION_VALUE\" \"${HC_UPDATER_ONLY-unset}\"";
    let out = s.hc(&["run", "sh", "-c", probe]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("ENV:unset|original|unset"), "{out:?}");
    let stream = herdr_core::platform::connect(&s.socket()).unwrap();
    let terminal = herdr_core::client::call(
        &stream,
        "terminal.run",
        json!({
            "argv": ["sh", "-c", probe],
            "env": {"NO_COLOR": "1", "HC_SESSION_VALUE": "override"},
        }),
    )
    .unwrap();
    wait_until("per-terminal environment override", || {
        text(&s, terminal["address"].as_str().unwrap()).contains("ENV:1|override|unset")
    });

    let older = s.root.join("older");
    std::fs::write(&older, "#!/bin/sh\necho 'hc 0.0.1'\n").unwrap();
    std::fs::set_permissions(&older, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (_, e) = s.err(&["--json", "update", "--from", older.to_str().unwrap()]);
    assert_eq!(e["error"]["kind"], "refused", "never older by accident: {e}");
    let junk = s.root.join("junk");
    std::fs::write(&junk, "not a program").unwrap();
    assert_eq!(s.err(&["--json", "update", "--from", junk.to_str().unwrap()]).1["error"]["kind"], "refused");
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".new")),
        "no leftovers"
    );
}
