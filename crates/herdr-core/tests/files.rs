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

        // Any file whole, as it is: an image to preview.
        let png: Vec<u8> = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".iter().copied().chain(0..=255).collect();
        assert!(s.hc_input(&file(&["write", "img.png"]), &png).status.success());
        let (code, e) = s.err(&file(&["read", "img.png"]));
        assert_eq!((code, e["error"]["kind"].as_str()), (1, Some("not_text")), "{e}");
        assert_eq!(e["error"]["hint"], "for its bytes: hc file read --bytes");
        let raw = s.hc(&file(&["read", "img.png", "--bytes"]));
        assert!(raw.stdout == png && raw.stderr.is_empty(), "{on:?}: its bytes, as they are");
        let r = s.json(&file(&["read", "img.png", "--bytes"]));
        assert_eq!(r["bytes"].as_u64(), Some(png.len() as u64), "{r}");
        assert!(r["data"].as_str().unwrap().starts_with("iVBORw0KGgo"), "base64: {r}");
        assert!(r.get("text").is_none(), "{r}");
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
    assert_eq!(
        s.err(&["--json", "file", "read", "--bytes", "--cwd", &cwd, "fifo"]).1["error"]["kind"],
        "invalid_params"
    );
    std::fs::File::create(dir.join("9mb")).unwrap().set_len(9 << 20).unwrap();
    let e = s.err(&["--json", "file", "read", "--bytes", "--cwd", &cwd, "9mb"]).1;
    assert!(e["error"]["message"].as_str().unwrap().ends_with("is over 8 MB"), "bytes fit one message: {e}");
}
