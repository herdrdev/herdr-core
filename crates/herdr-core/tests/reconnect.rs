#![cfg(unix)]
mod common;

use common::Session;
use herdr_core::holder_proto as proto;
use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A real holder behind a proxy that drops a spawn acknowledgement, forcing a
/// reconnect while the run is waiting. Optionally corrupt the next welcome too.
struct Fault {
    streams: Arc<Mutex<Vec<UnixStream>>>,
    spawned: std::sync::mpsc::Receiver<u32>,
    listener: Option<std::thread::JoinHandle<()>>,
    path: std::path::PathBuf,
    actual: std::path::PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Fault {
    fn new(s: &Session, fail_handshake: bool) -> Self {
        let dir = s.root.join("run/herdr-core");
        let path = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.to_string_lossy().contains(".holder."))
            .unwrap();
        let actual = s.root.join("actual.sock");
        std::fs::rename(&path, &actual).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let streams = Arc::new(Mutex::new(Vec::new()));
        let kept = streams.clone();
        let (spawned, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let endpoint = actual.clone();
        let thread = std::thread::spawn(move || {
            let mut injected = false;
            let start = Instant::now();
            // Startup takeover, failed reader's relink, and explicit stop.
            for _ in 0..3 {
                let client = loop {
                    if stopping.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((c, _)) => break c,
                        Err(e)
                            if e.kind() == io::ErrorKind::WouldBlock && start.elapsed() < Duration::from_secs(15) =>
                        {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        _ => return,
                    }
                };
                // macOS inherits the listener's nonblocking mode on accepted sockets.
                client.set_nonblocking(false).unwrap();
                let upstream = UnixStream::connect(&endpoint).unwrap();
                let reject = injected && fail_handshake;
                // Only the first connection can contain the test's spawn.
                let corrupt_spawn = !injected;
                injected = true;
                kept.lock().unwrap().extend([client.try_clone().unwrap(), upstream.try_clone().unwrap()]);
                let mut c = client.try_clone().unwrap();
                let mut u = upstream.try_clone().unwrap();
                std::thread::spawn(move || {
                    let _ = io::copy(&mut c, &mut u);
                    let _ = u.shutdown(std::net::Shutdown::Write);
                });
                let spawned = spawned.clone();
                std::thread::spawn(move || {
                    let (mut client, mut upstream) = (client, upstream);
                    while let Ok(Some((kind, id, payload))) = proto::read(&mut upstream) {
                        if kind == proto::SPAWNED {
                            let pid = proto::parse::<proto::Spawned>(&payload).unwrap().pid;
                            let _ = spawned.send(pid);
                        }
                        let frame = if (corrupt_spawn && kind == proto::SPAWNED) || (reject && kind == proto::WELCOME) {
                            vec![proto::OUTPUT, 1, 0, 0, 0, 255, 255, 255, 255]
                        } else {
                            proto::frame(kind, id, &payload)
                        };
                        if client.write_all(&frame).is_err() {
                            break;
                        }
                    }
                    let _ = client.shutdown(std::net::Shutdown::Write);
                });
            }
        });
        Self { streams, spawned: rx, listener: Some(thread), path, actual, stop }
    }
}

impl Drop for Fault {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.listener.take().unwrap().join().unwrap();
        for stream in self.streams.lock().unwrap().iter() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::rename(&self.actual, &self.path);
    }
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn reconnect_completes_a_launch_whose_acknowledgement_was_lost() {
    let mut s = Session::new();
    s.env.push(("HC_SPAWN_MS".into(), "2000".into()));
    s.json(&["run", "--bg", "sleep", "100"]);
    let fault = Fault::new(&s, false);
    s.json(&["server", "restart"]);
    let mut events = s.connect();
    events.call(1, "events.subscribe", serde_json::json!({}));
    let run = s.hc(&["--json", "run", "--bg", "sleep", "100"]);
    let pid = fault.spawned.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(s.json(&["get", "t2"])["pid"], pid);
    let event = common::next_event(&mut events, |e| e["params"]["terminal"] == "t2");
    assert_eq!(event["params"]["type"], "created");
    assert!(alive(pid));
    s.json(&["server", "stop"]);
    assert!(!alive(pid));
}

#[test]
fn reconnect_completes_a_kept_replacement_whose_acknowledgement_was_lost() {
    let mut s = Session::new();
    s.env.push(("SHELL".into(), "/bin/sh".into()));
    s.json(&["run", "--bg", "--keep", "sleep", "100"]);
    let original = s.json(&["get", "t1"])["pid"].as_u64().unwrap() as u32;
    let fault = Fault::new(&s, false);
    s.json(&["server", "restart"]);
    let mut events = s.connect();
    events.call(1, "events.subscribe", serde_json::json!({}));
    assert_eq!(unsafe { libc::kill(original as i32, libc::SIGTERM) }, 0);
    let pid = fault.spawned.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    let terminal = loop {
        let terminal = s.json(&["get", "t1"]);
        if terminal["pid"] == pid {
            break terminal;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "replacement was not adopted");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(terminal["reason"].is_null(), "{terminal}");
    assert!(terminal["signal"].is_null(), "{terminal}");
    let lost = common::next_event(&mut events, |e| e["params"]["type"] == "lost");
    assert_eq!(lost["params"]["terminal"], "t1");
    let restarted = common::next_event(&mut events, |e| e["params"]["type"] == "restarted");
    assert_eq!(restarted["params"]["terminal"], "t1");
    assert_eq!(restarted["params"]["data"]["pid"], pid);
    assert!(restarted["params"]["data"]["reason"].is_null());
    assert!(alive(pid));
    s.json(&["server", "stop"]);
    assert!(!alive(pid));
}

#[test]
fn explicit_stop_ends_programs_and_holder_after_a_failed_relink() {
    let mut s = Session::new();
    s.env.push(("HC_SPAWN_MS".into(), "2000".into()));
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "60000".into()));
    s.json(&["run", "--bg", "sleep", "100"]);
    let original = s.json(&["get", "t1"])["pid"].as_u64().unwrap() as u32;
    let holder = s.json(&["server", "status"])["holder"]["pid"].as_u64().unwrap() as u32;
    let fault = Fault::new(&s, true);
    s.json(&["server", "restart"]);
    let run = s.hc(&["--json", "run", "--bg", "sleep", "100"]);
    assert!(!run.status.success());
    let pending = fault.spawned.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(s.json(&["server", "status"])["holders"].as_array().unwrap().is_empty());
    assert!(alive(original), "relink failure must not end programs");
    assert!(alive(pending));
    s.json(&["server", "stop"]);
    assert!(!alive(original), "acknowledged program outlived stop");
    assert!(!alive(pending), "unacknowledged program outlived stop");
    let start = Instant::now();
    while alive(holder) && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!alive(holder), "holder can still restart core after stop");
}
