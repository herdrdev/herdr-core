//! file read, write, edit: an agent's hands on files, here or on a machine.

#![cfg(unix)]

mod common;

use common::*;
use std::os::unix::fs::PermissionsExt;

fn source() -> String {
    (1..=6000).map(|i| format!("fn f{i}() -> u32 {{\n    {i}\n}}\n")).collect()
}

#[test]
fn files_are_written_read_and_edited_here_and_on_a_machine() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    let src = source(); // ~150 KB: past one argument's limit
    let here = s.root.display().to_string();
    for (on, home) in [(&["--cwd", &here][..], &s), (&["--on", "rohan"][..], &rohan)] {
        let file = |args: &[&'static str]| [&["file"], &args[..1], on, &args[1..]].concat();
        let w = s.hc_input(&[&["--json"][..], &file(&["write", "src/lib.rs"])].concat(), src.as_bytes());
        assert!(w.status.success(), "{on:?}: {}", String::from_utf8_lossy(&w.stderr));
        let path = home.root.join("src/lib.rs");
        let w: serde_json::Value = serde_json::from_slice(&w.stdout).unwrap();
        assert_eq!((w["path"].as_str(), w["bytes"].as_u64()), (path.to_str(), Some(src.len() as u64)), "folder made");

        let read = s.hc(&file(&["read", "src/lib.rs"]));
        assert!(read.stdout == src.as_bytes() && read.stderr.is_empty(), "{on:?}: all of it, as is");
        let part = s.hc(&file(&["read", "src/lib.rs", "--offset", "4", "--limit", "3"]));
        assert_eq!(String::from_utf8_lossy(&part.stdout), "fn f2() -> u32 {\n    2\n}\n");
        assert_eq!(String::from_utf8_lossy(&part.stderr).trim(), "[hc] lines 4-6 of 18000");

        const NEW_FN: &str = "fn added() -> u32 {\n    0\n}\n\nfn f5000() -> u32 {";
        s.json(&file(&[
            "edit",
            "src/lib.rs",
            "--old",
            "fn f5000() -> u32 {",
            "--new",
            NEW_FN,
            "--old",
            "    7\n",
            "--new",
            "    70\n",
        ]));
        let edited = src.replace("fn f5000() -> u32 {", NEW_FN).replace("    7\n", "    70\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited, "{on:?}");

        // All or none: the second edit is ambiguous, so the first isn't made either.
        let (code, e) =
            s.err(&file(&["edit", "src/lib.rs", "--old", "fn f1(", "--new", "fn g1(", "--old", "u32", "--new", "u64"]));
        assert_eq!((code, e["error"]["kind"].as_str()), (1, Some("edit_failed")), "{e}");
        assert!(e["error"]["message"].as_str().unwrap().contains("6001 times"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited, "untouched");
    }
}

#[test]
fn a_write_swaps_the_file_in_whole_and_keeps_what_it_was() {
    let s = Session::new();
    let dir = s.root.join("w");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("run.sh");
    std::fs::write(&script, "old").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink("run.sh", dir.join("link")).unwrap();
    let cwd = dir.display().to_string();

    assert!(s.hc_input(&["file", "write", "--cwd", &cwd, "link"], b"#!/bin/sh\n").status.success());
    assert_eq!(std::fs::read_to_string(&script).unwrap(), "#!/bin/sh\n", "through the link");
    assert!(dir.join("link").is_symlink(), "the link stays a link");
    assert_eq!(std::fs::metadata(&script).unwrap().permissions().mode() & 0o777, 0o750, "its mode kept");
    let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 2, "no temp file left: {names:?}");
    std::os::unix::fs::symlink("later.sh", dir.join("dangling")).unwrap();
    assert!(s.hc_input(&["file", "write", "--cwd", &cwd, "dangling"], b"x").status.success());
    assert_eq!(std::fs::read_to_string(dir.join("later.sh")).unwrap(), "x", "made where the link points");
    assert!(dir.join("dangling").is_symlink());

    // A relative --cwd starts where hc runs, not where the core does.
    let read = s.cmd().current_dir(&s.root).args(["file", "read", "--cwd", "w", "run.sh"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&read.stdout), "#!/bin/sh\n");

    assert_eq!(s.err(&["--json", "file", "read", "--cwd", &cwd, "nope"]).0, 3, "not_found");
    std::fs::write(dir.join("bin"), b"\xff\xfe").unwrap();
    assert_eq!(s.err(&["--json", "file", "read", "--cwd", &cwd, "bin"]).1["error"]["kind"], "not_text");
    // Never what could block or never end, never past the limits.
    assert!(std::process::Command::new("mkfifo").arg(dir.join("fifo")).status().unwrap().success());
    std::fs::File::create(dir.join("huge")).unwrap().set_len(65 << 20).unwrap();
    for name in ["fifo", "huge"] {
        assert_eq!(s.err(&["--json", "file", "read", "--cwd", &cwd, name]).1["error"]["kind"], "invalid_params");
    }
    let big = s.hc_input(&["--json", "file", "write", "--cwd", &cwd, "big"], &vec![b'x'; (8 << 20) + 1]);
    assert!(String::from_utf8_lossy(&big.stderr).contains("invalid_params"), "8 MB at most");
}

#[test]
fn an_empty_write_is_refused_and_leaves_the_target_untouched() {
    let s = Session::new();
    let cwd = s.root.display().to_string();
    let missing = s.root.join("not-created.txt");

    let (code, e) = s.err(&["--json", "file", "write", "--cwd", &cwd, "not-created.txt"]);
    assert_eq!(code, 2, "{e}");
    assert_eq!(e["error"]["kind"], "usage", "{e}");
    assert!(!missing.exists(), "no empty file made");

    let existing = s.root.join("kept.txt");
    std::fs::write(&existing, b"keep me").unwrap();
    let (code, _) = s.err(&["--json", "file", "write", "--cwd", &cwd, "kept.txt"]);
    assert_eq!(code, 2);
    assert_eq!(std::fs::read(&existing).unwrap(), b"keep me", "not truncated");

    let allowed = s.hc_input(&["--json", "file", "write", "--allow-empty", "--cwd", &cwd, "not-created.txt"], b"");
    assert!(allowed.status.success(), "{}", String::from_utf8_lossy(&allowed.stderr));
    assert_eq!(std::fs::metadata(&missing).unwrap().len(), 0, "explicit empty write");
    let r: serde_json::Value = serde_json::from_slice(&allowed.stdout).unwrap();
    assert_eq!(r["bytes"], 0, "{r}");
}

#[test]
fn a_write_reports_the_bytes_the_target_holds() {
    let s = Session::new();
    let cwd = s.root.display().to_string();
    let content = b"exactly twelve";
    let out = s.hc_input(&["--json", "file", "write", "--cwd", &cwd, "counted.txt"], content);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["bytes"], content.len() as u64, "{r}");
    assert_eq!(std::fs::metadata(s.root.join("counted.txt")).unwrap().len(), content.len() as u64);
}
