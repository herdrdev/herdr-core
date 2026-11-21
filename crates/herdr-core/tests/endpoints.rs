#![cfg(unix)]

mod common;

use common::framed::Framed;
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn endpoint_script(s: &Session, ready: &str, body: &str) -> String {
    script(
        s,
        &format!(
            "printf '%s\\n' \"$HC_SOCKET\" \"$HC_ENDPOINT\" \"$HC_TERMINAL_SOCKET\" >'{ready}.tmp'; \
             mv '{ready}.tmp' '{ready}'; {body}"
        ),
    )
}

fn recorded_endpoint(ready: &str) -> PathBuf {
    wait_until("child endpoint environment", || Path::new(ready).exists());
    let env = fs::read_to_string(ready).unwrap();
    let mut lines = env.lines();
    let path = lines.next().unwrap();
    assert_eq!(lines.next(), Some("1"), "{env:?}");
    assert_eq!(lines.next(), Some(path), "{env:?}");
    path.into()
}

fn bound(s: &Session, args: &[&str], ready: &str, body: &str) -> (String, PathBuf) {
    let script = endpoint_script(s, ready, body);
    let addr = run(s, &[&["--bind"], args, &["sh", "-c", &script]].concat());
    (addr, recorded_endpoint(ready))
}

fn waiting(s: &Session) -> (String, PathBuf) {
    bound(s, &[], s.root.join("ready").to_str().unwrap(), "exec sleep 100")
}

fn hello(s: &Session, path: &Path) -> Value {
    result(s.connect_at(path).call(1, "hello", json!({})))
}

fn result(reply: Value) -> Value {
    assert!(reply.get("result").is_some(), "{reply}");
    reply["result"].clone()
}

fn kind(reply: Value, expected: &str) {
    assert_eq!(reply["error"]["data"]["kind"], expected, "{reply}");
}

fn count(s: &Session) -> usize {
    s.json(&["ls", "--all"])["terminals"].as_array().unwrap().len()
}

fn state(s: &Session, name: &str) -> PathBuf {
    s.root.join("state/herdr-core/test").join(name)
}

fn status(s: &Session) -> Value {
    s.json(&["server", "status"])
}

fn crash(s: &Session) {
    let pid = status(s)["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until("core socket stops accepting", || !s.connect_ok());
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let ty = fs::symlink_metadata(&path).unwrap().file_type();
        entries.insert(path.clone(), ty.is_file().then(|| fs::read(&path).unwrap()));
        if ty.is_dir() {
            entries.extend(tree(&path));
        }
    }
    entries
}

#[test]
fn bound_hello_names_the_terminal() {
    let s = Session::new();
    let (child, path) = waiting(&s);
    let reply = hello(&s, &path);
    let instance = status(&s)["instance"].clone();
    assert_eq!(instance.as_str().unwrap().len(), 32);
    assert_eq!(reply["instance"], instance);
    assert_eq!(reply["you"], json!({"terminal": child, "instance": instance}));
    assert!(hello(&s, &s.socket()).get("you").is_none());
}

#[test]
fn a_raw_run_naming_another_caller_is_attributed_to_the_endpoint() {
    let s = Session::new();
    let other = run(&s, &["sleep", "100"]);
    let (child, path) = waiting(&s);
    let params = json!({"caller": other, "argv": ["sleep", "100"]});
    let plain = s.connect_at(&path).call(1, "terminal.run", params.clone());
    let framed = Framed::open_at(&path).call("terminal.run", params);
    for reply in [plain, framed] {
        let address = result(reply)["address"].as_str().unwrap().to_owned();
        assert_eq!(get(&s, &address)["parent"], child);
    }
}

#[test]
fn omitted_and_null_params_bind_the_caller() {
    let s = Session::new();
    let (child, path) = waiting(&s);
    let mut plain = s.connect_at(&path);
    let mut framed = Framed::open_at(&path);
    let omitted = json!({"jsonrpc": "2.0", "id": 1, "method": "terminal.run"});
    let null = json!({"jsonrpc": "2.0", "id": 2, "method": "terminal.run", "params": null});
    let replies =
        [plain.request(omitted.clone()), plain.request(null.clone()), framed.request(omitted), framed.request(null)];
    for reply in replies {
        let address = result(reply)["address"].as_str().unwrap().to_owned();
        assert_eq!(get(&s, &address)["parent"], child);
    }
}

#[test]
fn positional_params_are_refused() {
    let s = Session::new();
    let (child, path) = waiting(&s);
    let params = json!([child, child]);
    assert_eq!(result(s.connect().call(1, "terminal.get", params.clone()))["address"], child);
    for reply in [
        s.connect_at(&path).call(1, "terminal.get", params.clone()),
        Framed::open_at(&path).call("terminal.get", params),
    ] {
        assert_eq!(reply["error"]["message"], "params must be an object on a bound endpoint");
        kind(reply, "invalid_params");
    }
    assert_eq!(count(&s), 1);
}

#[test]
fn real_cli_inside_a_bound_terminal() {
    let s = Session::new();
    let run_out = s.root.join("run.json");
    let raw_out = s.root.join("raw.json");
    let done = s.root.join("done");
    let body = format!(
        r#"'{BIN}' run --bg sleep 5 >'{}'; printf '%s\n' '{{"jsonrpc":"2.0","id":1,"method":"hello"}}' |
        '{BIN}' connect >'{}'; touch '{}'; exec sleep 100"#,
        run_out.display(),
        raw_out.display(),
        done.display(),
    );
    let (child, _) = bound(&s, &[], s.root.join("ready").to_str().unwrap(), &body);
    wait_until("both CLI replies", || done.exists());
    let read = |path: PathBuf| serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap();
    let spawned = read(run_out)["address"].as_str().unwrap().to_owned();
    assert_eq!(get(&s, &spawned)["parent"], child);
    assert_eq!(result(read(raw_out))["you"]["terminal"], child);
}

#[test]
fn retirement() {
    let s = Session::new();
    let (child, path) = bound(&s, &[], s.root.join("ready").to_str().unwrap(), "w exit");
    let mut conn = s.connect_at(&path);
    let mut framed = Framed::open_at(&path);
    assert_eq!(result(conn.call(1, "hello", json!({})))["you"]["terminal"], child);
    touch(&s, "exit");
    wait_until("endpoint unlinked", || !path.exists());
    kind(conn.call(2, "hello", json!({})), "endpoint_retired");
    kind(framed.call("hello", json!({})), "endpoint_retired");
    assert!(UnixStream::connect(path).is_err());
}

#[test]
fn no_autostart() {
    let s = Session::new();
    let (child, path) = waiting(&s);
    let pid = status(&s)["pid"].as_i64().unwrap();
    assert_eq!(result(s.connect_at(&path).call(1, "server.stop", json!({})))["stopped"], true);
    wait_until("original core exits", || !pid_alive(pid));
    let before = (tree(&s.root.join("run")), tree(&s.root.join("state")));
    let out = s
        .cmd()
        .env("HC_SOCKET", &path)
        .env("HC_TERMINAL_SOCKET", &path)
        .env("HC_TERMINAL_ID", child)
        .env("HC_ENDPOINT", "1")
        .arg("ls")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(!pid_alive(pid) && !s.connect_ok() && !path.exists());
    assert_eq!((tree(&s.root.join("run")), tree(&s.root.join("state"))), before);
}

struct PausedHolder(i32);

impl Drop for PausedHolder {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGCONT) };
    }
}

#[test]
fn late_spawn_keeps_its_endpoint() {
    let mut s = Session::new();
    s.env.push(("HC_SPAWN_MS".into(), "50".into()));
    run(&s, &["sleep", "100"]);
    let holder = status(&s)["holder"]["pid"].as_i64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(holder, libc::SIGSTOP) }, 0);
    let pause = PausedHolder(holder);
    let ready = s.root.join("ready");
    let ready = ready.to_str().unwrap();
    let script = endpoint_script(&s, ready, "exec sleep 100");
    s.err(&["run", "--bg", "--keep", "--bind", "sh", "-c", &script]);
    let row = get(&s, "t2");
    assert_eq!(row["reason"], "spawn_pending");
    assert!(row["pid"].is_null());
    let path = fs::read_dir(s.socket().with_file_name("test.sock.e")).unwrap().next().unwrap().unwrap().path();
    assert!(path.exists());
    drop(pause);
    wait_until("late spawn", || get(&s, "t2")["pid"].is_number());
    let child_endpoint = recorded_endpoint(ready);
    assert_eq!(child_endpoint, path.canonicalize().unwrap());
    assert_eq!(hello(&s, &child_endpoint)["you"]["terminal"], "t2");
}

#[test]
fn kept_respawn_keeps_the_endpoint() {
    let s = Session::new();
    let config = s.root.join("config/herdr-core/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let after = s.root.join("respawn");
    let shell = format!("printf '%s\n' \"$HC_SOCKET\" \"$HC_ENDPOINT\" >'{}'; exec sleep 100", after.display());
    fs::write(&config, format!("shell = {}", json!(["/bin/sh", "-c", &shell]))).unwrap();
    let (child, path) = bound(&s, &["--keep"], s.root.join("ready").to_str().unwrap(), "w exit; kill -TERM $$");
    let before = hello(&s, &path)["you"].clone();
    let pid = get(&s, &child)["pid"].as_i64().unwrap();
    touch(&s, "exit");
    wait_until("kept shell", || get(&s, &child)["pid"].as_i64().is_some_and(|p| p != pid) && after.exists());
    wait_until("kept shell environment", || {
        fs::read_to_string(&after).is_ok_and(|env| env == format!("{}\n1\n", path.display()))
    });
    assert_eq!(hello(&s, &path)["you"], before);
}

#[test]
fn restart_rebinds_reclaims_or_reports_lost() {
    let mut s = Session::new();
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "60000".into()));
    let (child, path) = waiting(&s);
    let (other, other_path) = bound(&s, &[], s.root.join("other").to_str().unwrap(), "exec sleep 100");
    let (live, live_path) = bound(&s, &[], s.root.join("live").to_str().unwrap(), "exec sleep 100");
    let first = hello(&s, &path);
    s.json(&["server", "restart"]);
    let restarted = hello(&s, &path);
    assert_eq!(restarted["you"]["terminal"], child);
    assert_ne!(restarted["instance"], first["instance"]);
    for _ in 0..2 {
        crash(&s);
        assert!(path.exists(), "crash leaves a stale socket");
        s.json(&["server", "start"]);
        assert_eq!(hello(&s, &path)["you"]["terminal"], child);
    }

    crash(&s);
    let displaced = other_path.with_extension("old");
    fs::rename(&other_path, &displaced).unwrap();
    let original_ino = fs::symlink_metadata(&displaced).unwrap().ino();
    drop(UnixListener::bind(&other_path).unwrap());
    let other_ino = fs::symlink_metadata(&other_path).unwrap().ino();
    assert_ne!(original_ino, other_ino);

    // Record the identity of a listening socket, so this case reaches the probe.
    fs::remove_file(&live_path).unwrap();
    let listener = UnixListener::bind(&live_path).unwrap();
    let metadata = fs::symlink_metadata(&live_path).unwrap();
    let snapshot = state(&s, "terminals.json");
    let mut saved: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    let key = live.strip_prefix('t').unwrap();
    saved["terminals"][key]["endpoint"]["dev"] = metadata.dev().into();
    saved["terminals"][key]["endpoint"]["ino"] = metadata.ino().into();
    fs::write(snapshot, serde_json::to_vec(&saved).unwrap()).unwrap();

    s.json(&["server", "start"]);
    assert_eq!(get(&s, &other)["bound"], "lost");
    assert_eq!(get(&s, &live)["bound"], "lost");
    assert_eq!(fs::symlink_metadata(&other_path).unwrap().ino(), other_ino);
    assert_eq!(fs::symlink_metadata(&live_path).unwrap().ino(), metadata.ino());
    assert_eq!(hello(&s, &path)["you"]["terminal"], child);
    drop(listener);
}

#[test]
fn recovery_cleans_an_endpoint_for_a_pruned_row() {
    let mut s = Session::new();
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "60000".into()));
    let (child, path) = bound(&s, &["--rm"], s.root.join("ready").to_str().unwrap(), "w exit");
    let pid = get(&s, &child)["pid"].as_i64().unwrap();
    crash(&s);
    touch(&s, "exit");
    wait_until("program exits while core is down", || !pid_alive(pid));
    assert!(path.exists());
    s.json(&["server", "start"]);
    assert!(!path.exists());
    assert_eq!(s.err(&["get", &child]).1["error"]["kind"], "not_found");
}

#[test]
fn restart_drops_endpoint_mode_from_the_replacement_environment() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut environment: BTreeMap<String, String> = s
        .cmd()
        .get_envs()
        .map(|(k, v)| (k.to_str().unwrap().to_owned(), v.unwrap().to_str().unwrap().to_owned()))
        .collect();
    environment.insert("HC_ENDPOINT".into(), "1".into());
    let mut conn = s.connect();
    result(conn.call(1, "server.restart", json!({"environment": environment})));
    assert_eq!(conn.reader.read(&mut [0]).unwrap(), 0);
    let child = run(&s, &["sh", "-c", "printf 'endpoint=<%s>\n' \"$HC_ENDPOINT\"; exec sleep 100"]);
    wait_until("fresh unbound program output", || text(&s, &child).contains("endpoint=<>"));
    assert!(status(&s)["holder"]["pid"].is_number());
    assert!(get(&s, &child).get("bound").is_none());
}

#[test]
fn custom_socket_and_child_cwd_use_an_absolute_endpoint() {
    let mut s = Session::new();
    let socket = s.root.join("x.e");
    s.env.push(("HC_SOCKET".into(), socket.display().to_string()));
    let started = s.cmd().current_dir(&s.root).env("HC_SOCKET", "./x.e").args(["server", "start"]).output().unwrap();
    assert!(started.status.success(), "{started:?}");
    let child_cwd = s.root.join("config");
    let (child, path) =
        bound(&s, &["--cwd", child_cwd.to_str().unwrap()], s.root.join("ready").to_str().unwrap(), "exec sleep 100");
    assert!(path.is_absolute());
    assert_eq!(path.parent().unwrap(), s.root.canonicalize().unwrap().join("x.e.e"));
    assert_eq!(get(&s, &child)["cwd"], child_cwd.to_str().unwrap());
    assert_eq!(hello(&s, &path)["you"]["terminal"], child);
}

#[test]
fn a_peer_hello_on_an_endpoint_is_refused() {
    let s = Session::new();
    let (_, path) = waiting(&s);
    let params = json!({"framed": true, "core": {"id": "peer-id", "name": "fake-peer"}});
    let peers = state(&s, "peers.json");
    let registered = Framed::hello_at(&s.socket(), params.clone());
    wait_until("registered peer is persisted", || peers.exists());
    let before = fs::read(&peers).unwrap();
    let machines = result(s.connect().call(1, "machine.list", json!({})));
    let mut endpoint = s.connect_at(&path);
    let mut impostor = params;
    impostor["core"]["id"] = "impostor-id".into();
    kind(endpoint.call(1, "hello", impostor), "invalid_params");
    kind(endpoint.call(2, "$/orphaned", json!({"parent": "peer-id/t1"})), "invalid_params");
    kind(Framed::open_at(&path).call("$/orphaned", json!({"parent": "peer-id/t1"})), "invalid_params");
    assert_eq!(fs::read(peers).unwrap(), before);
    assert_eq!(result(s.connect().call(2, "machine.list", json!({}))), machines);
    drop(registered);
}

#[test]
fn a_registered_peer_cannot_bind_a_terminal() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let params = json!({"framed": true, "core": {"id": "peer-id", "name": "fake-peer"}});
    let mut peer = Framed::hello_at(&s.socket(), params);
    kind(peer.call("terminal.run", json!({"bind": true, "argv": ["sleep", "100"]})), "refused");
    assert_eq!(count(&s), 0);
}

#[test]
fn bind_needs_the_feature() {
    let s = Session::new();
    let socket = s.root.join("fake.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let fake = std::thread::spawn(move || {
        let mut stream = None;
        wait_until("CLI opens fake socket", || {
            stream = listener.accept().ok().map(|(stream, _)| stream);
            stream.is_some()
        });
        let mut stream = stream.unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["method"], "hello", "{req}");
        writeln!(stream, "{}", json!({"jsonrpc": "2.0", "id": req["id"], "result": {"features": []}})).unwrap();
        line.clear();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0, "{line}");
        assert!(listener.accept().is_err());
    });
    let out = s.cmd().env("HC_SOCKET", socket).args(["run", "--bind", "--bg", "sleep", "100"]).output().unwrap();
    fake.join().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "refused");
    assert_eq!(error["error"]["message"], "this core cannot bind terminals; nothing was started");
}

#[test]
fn bind_with_on_is_refused() {
    let s = Session::new();
    let (code, error) = s.err(&["run", "--bind", "--on", "other", "--bg", "sleep", "100"]);
    assert_eq!(code, 2, "{error}");
    assert!(!s.connect_ok(), "usage must precede connection");
    s.json(&["server", "start"]);
    let mut conn = s.connect();
    kind(conn.call(1, "terminal.run", json!({"bind": true, "on": "other"})), "invalid_params");
    let positional = json!([
        ["sleep", "100"],
        null,
        null,
        null,
        null,
        null,
        "other",
        {},
        {},
        null,
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        null,
        null,
        true
    ]);
    let reply = conn.call(2, "terminal.run", positional);
    assert_eq!(reply["error"]["message"], "bind cannot be used with on", "{reply}");
    kind(reply, "invalid_params");
    assert_eq!(count(&s), 0);
}

#[test]
fn endpoint_path_too_long() {
    let mut s = Session::new();
    let max = herdr_core::platform::MAX_SOCKET_PATH;
    let holder_suffix = ".holder.sock".len() + 1 + herdr_core::paths::HOLDER_ID;
    let parent = s.root.join("run");
    let socket = parent.join(format!("{}.sock", "s".repeat(max - holder_suffix - parent.as_os_str().len() - 1)));
    s.env.push(("HC_SOCKET".into(), socket.display().to_string()));
    s.json(&["server", "start"]);
    let marker = s.root.join("program-ran");
    let (_, err) = s.err(&["run", "--bg", "--bind", "touch", marker.to_str().unwrap()]);
    assert_eq!(err["error"]["kind"], "unavailable", "{err}");
    assert_eq!(err["error"]["message"], "the endpoint path is too long here");
    assert_eq!(count(&s), 0);
    assert!(!marker.exists());
}

#[test]
fn endpoint_is_private() {
    let s = Session::new();
    let (child, path) = waiting(&s);
    let full = get(&s, &child);
    let short = s.json(&["ls", "--json"]);
    let snapshot = result(s.connect().call(1, "events.subscribe", json!({"terminal": child})));
    for row in [&full, &short["terminals"][0], &snapshot["terminals"][0]] {
        assert!(!row.to_string().contains(path.to_str().unwrap()), "{row}");
        assert!(row.get("endpoint").is_none(), "{row}");
    }
    assert_eq!(full["bound"], "bound");
    assert_eq!(snapshot["terminals"][0]["bound"], "bound");
    assert!(short["terminals"][0].get("bound").is_none());
    let schema: Value = serde_json::from_str(include_str!("../../../schema/hc.openrpc.json")).unwrap();
    let props = &schema["components"]["schemas"]["Terminal"]["properties"];
    assert!(props.get("endpoint").is_none() && props.get("bound").is_some(), "{props}");
}

#[test]
fn endpoint_mode_refuses_lifecycle_commands() {
    let s = Session::new();
    let state_dir = state(&s, "");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(state_dir.join("restart-env.json"), b"sentinel").unwrap();
    let before = (tree(&s.root.join("run")), tree(&s.root.join("state")));
    let socket = s.socket();
    for args in [
        &["serve"][..],
        &["--json", "serve"],
        &["hold", "--socket", socket.to_str().unwrap()],
        &["--hc-install-bundle", "x", "y"],
        &["--hc-launchd"],
        &["server", "start"],
        &["server", "restart"],
        &["server", "restart", "--fresh"],
        &["update"],
        &["update", "--from", "x"],
        &["connect", "--session", "other"],
    ] {
        let out = s.cmd().env("HC_ENDPOINT", "1").args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("this terminal talks to its own endpoint; run that outside it"),
            "{args:?}: {out:?}"
        );
    }
    let out = s.cmd().env("HC_ENDPOINT", "1").args(["server", "stop"]).output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "unavailable", "{err}");
    assert_eq!((tree(&s.root.join("run")), tree(&s.root.join("state"))), before);
}
