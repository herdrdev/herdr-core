#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use herdr_core::wire;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;

#[test]
fn autostarts_and_keeps_its_identity_across_restarts() {
    let s = Session::new();
    let first = s.json(&["server", "status"]);
    let id = first["core"]["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 32);
    assert!(!first["core"]["name"].as_str().unwrap().is_empty());
    let pid = first["pid"].as_i64().unwrap();

    s.json(&["server", "stop"]);
    wait_until("core exit", || !pid_alive(pid));
    assert!(!s.socket().exists());

    let second = s.json(&["server", "status"]);
    assert_eq!(second["core"]["id"], id.as_str());
    assert_ne!(second["pid"].as_i64().unwrap(), pid);
}

#[test]
fn concurrent_commands_start_one_core() {
    let s = Session::new();
    let children: Vec<_> =
        (0..6).map(|_| s.cmd().args(["server", "status"]).stdout(Stdio::piped()).spawn().unwrap()).collect();
    let pids: Vec<i64> = children
        .into_iter()
        .map(|c| {
            let out = c.wait_with_output().unwrap();
            assert!(out.status.success());
            serde_json::from_slice::<Value>(&out.stdout).unwrap()["pid"].as_i64().unwrap()
        })
        .collect();
    assert!(pids.iter().all(|p| *p == pids[0]), "{pids:?}");
}

#[test]
fn plain_mode_speaks_json_rpc_lines() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = s.connect();

    let hello = conn.call(1, "hello", json!({}));
    let result = &hello["result"];
    assert_eq!(hello["id"], 1);
    assert_eq!(result["generation"], 1);
    assert_eq!(result["core"]["id"].as_str().unwrap().len(), 32);

    let discover = conn.call(2, "rpc.discover", json!({}));
    let discovered: Vec<&str> =
        discover["result"]["methods"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap()).collect();
    let hello_methods: Vec<&str> = result["methods"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
    assert_eq!(discovered, hello_methods);
    for name in ["hello", "server.status", "server.stop", "rpc.discover"] {
        assert!(discovered.contains(&name), "{name} missing");
    }
    for m in discover["result"]["methods"].as_array().unwrap() {
        assert!(!m["description"].as_str().unwrap().is_empty(), "{m}");
        assert!(m["params"].is_array() && m["result"]["schema"].is_object(), "{m}");
    }

    let unknown = conn.call(3, "no.such", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(unknown["error"]["data"]["kind"], "unknown_method");

    let bad = conn.call(4, "hello", json!({"framed": "yes"}));
    assert_eq!(bad["error"]["code"], -32602);
    assert_eq!(bad["error"]["data"]["kind"], "invalid_params");

    writeln!(conn.writer, "not json").unwrap();
    let mut line = String::new();
    conn.reader.read_line(&mut line).unwrap();
    let parse: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parse["error"]["code"], -32700);
}

#[test]
fn the_schema_is_openrpc_and_the_committed_file_matches_the_code() {
    let s = Session::new();
    let full = s.json(&["schema"]);
    assert!(full["openrpc"].as_str().unwrap().starts_with("1."), "{}", full["openrpc"]);
    let text = full.to_string();
    let defs = full["components"]["schemas"].as_object().unwrap();
    assert!(defs.contains_key("Terminal"));
    // Every reference resolves, and the terminal record appears once.
    assert!(!text.contains("#/$defs/"));
    for r in text.split("\"$ref\":\"#/components/schemas/").skip(1) {
        let name = &r[..r.find('"').unwrap()];
        assert!(defs.contains_key(name), "dangling $ref {name}");
    }
    assert_eq!(text.matches("\"first_message\"").count(), 1, "RunResult once");
    let run = full["methods"].as_array().unwrap().iter().find(|m| m["name"] == "terminal.run").unwrap();
    let argv = run["params"].as_array().unwrap().iter().find(|p| p["name"] == "argv").unwrap();
    assert_eq!(argv["required"], false, "none: that machine's own shell: {argv}");

    // The committed schema is the code's: an API change is always a visible diff.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/hc.openrpc.json");
    let generated = serde_json::to_string_pretty(&herdr_core::api::openrpc()).unwrap() + "\n";
    if std::env::var_os("HC_UPDATE_SCHEMA").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &generated).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "schema/hc.openrpc.json is stale: review the API change, then HC_UPDATE_SCHEMA=1 cargo test"
    );
    assert_eq!(full, serde_json::from_str::<Value>(&committed).unwrap(), "the core serves the same document");
}

#[test]
fn hello_switches_to_framed_mode() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = s.connect();

    let hello = conn.call(1, "hello", json!({"framed": true}));
    assert_eq!(hello["result"]["framed"], true);

    let req = json!({"jsonrpc": "2.0", "id": 2, "method": "server.status"});
    conn.writer.write_all(&wire::encode(0, wire::DATA, req.to_string().as_bytes())).unwrap();
    let (channel, kind, payload) = read_frame(&mut conn.reader);
    assert_eq!((channel, kind), (0, wire::DATA));
    let reply: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reply["id"], 2);
    assert!(reply["result"]["pid"].is_number());
}

#[test]
fn connect_carries_the_protocol_over_stdio() {
    let s = Session::new();
    let mut child = s.cmd().arg("connect").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"hello","params":{{"framed":true}}}}"#).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let hello: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(hello["result"]["framed"], true);

    let req = json!({"jsonrpc": "2.0", "id": 2, "method": "server.status"});
    stdin.write_all(&wire::encode(0, wire::DATA, req.to_string().as_bytes())).unwrap();
    let (_, _, payload) = read_frame(&mut stdout);
    let reply: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reply["result"]["core"], hello["result"]["core"]);

    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn help_is_for_people_and_points_agents_to_their_own() {
    let s = Session::new();
    let out = |args: &[&str]| String::from_utf8(s.hc(args).stdout).unwrap();
    let help = out(&["--help"]);
    assert!(help.trim_end().ends_with("Agent? Run: hc --skill"), "{help}");
    assert_eq!(out(&["help"]), help);
    assert_eq!(out(&["-h"]), help);
    let agent = out(&["--skill"]);
    assert!(agent.lines().count() < 500, "skill must stay under 500 lines");
    // The manual is read, not installed: only its embedded stub is a skill.
    assert!(!agent.starts_with("---"), "manual has no skill frontmatter");
    let names =
        |text: &str, name: &str| text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-').any(|word| word == name);
    let for_people = ["events", "connect", "server", "schema", "update"];
    for name in herdr_core::cli::command_names() {
        assert!(names(&help, &name), "help misses `{name}`");
        assert!(for_people.contains(&name.as_str()) || names(&agent, &name), "skill misses `{name}`");
    }
    // Per-command help is still there.
    assert!(String::from_utf8(s.hc(&["run", "--help"]).stdout).unwrap().contains("--outlive-parent"));
}

#[test]
fn hc_carries_the_license_of_everything_it_includes() {
    let s = Session::new();
    let notices = String::from_utf8(s.hc(&["--licenses"]).stdout).unwrap();
    assert!(notices.contains("Mitchell Hashimoto, Ghostty contributors"), "libghostty-vt's own");
    let inputs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../distribution/notices");
    for entry in std::fs::read_dir(inputs).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "txt") {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(notices.contains(&text), "{} missing or stale: run just notices", path.display());
        }
    }
    for component in ["uucode", "Jacob Sandlund", "1991-2025 Unicode", "Rich Felker", "Rust 1.95.0"] {
        assert!(notices.contains(component), "{component} missing: run just notices");
    }
    assert!(!notices.contains("the crate ships no license text"), "license texts must be supplied");
    // Every crate in Cargo.lock, at its locked version: `scripts/notices` after a dependency change.
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")).unwrap();
    for package in lock.split("[[package]]").filter(|p| p.contains("source = ")) {
        let field =
            |key: &str| package.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(" = \"")?.strip_suffix('"'));
        let (name, version) = (field("name").unwrap(), field("version").unwrap());
        assert!(notices.contains(&format!("==== {name} {version} (")), "{name} {version} missing: run scripts/notices");
    }
}

#[test]
fn piped_errors_are_json_with_stable_exit_codes() {
    let s = Session::new();
    let (code, err) = s.err(&["no-such-command"]);
    assert_eq!(code, 2);
    assert_eq!(err["error"]["kind"], "usage");
    // What is missing, not just that something is.
    let (_, err) = s.err(&["send", "t1"]);
    assert!(err["error"]["message"].as_str().unwrap().contains("<TEXT>"), "{err}");
}

fn read_frame(r: &mut impl Read) -> (u32, u8, Vec<u8>) {
    let mut header = [0u8; wire::HEADER];
    r.read_exact(&mut header).unwrap();
    let (channel, kind, len) = wire::decode_header(&header).unwrap();
    let mut payload = vec![0; len];
    r.read_exact(&mut payload).unwrap();
    (channel, kind, payload)
}
