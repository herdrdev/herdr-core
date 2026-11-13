#![cfg(unix)]

mod common;

use common::*;
use herdr_core::wire;
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn session(enabled: bool) -> Session {
    session_with_timeout(enabled, 30.0, 15_000)
}

fn session_with_timeout(enabled: bool, timeout: f64, heartbeat_ms: u64) -> Session {
    let mut s = Session::new();
    // No login agent, launchd context, or real systemd user manager in these tests.
    s.env.push(("SSH_CONNECTION".into(), "test-direct".into()));
    s.env.push(("HC_HEARTBEAT_MS".into(), heartbeat_ms.to_string()));
    let config = s.root.join("config/herdr-core");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        format!("ssh_agent_forwarding = {enabled}\nmachine_timeout = {timeout}\n"),
    )
    .unwrap();
    s.json(&["server", "start"]);
    s
}

fn endpoint(s: &Session) -> PathBuf {
    PathBuf::from(format!("{}.ssh-agent", s.socket().display()))
}

/// A byte-level fake agent: no keys, signing, or dependence on a real SSH server.
struct Agent {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Agent {
    fn new(s: &Session, name: &str, identity: u8) -> Agent {
        let path = s.root.join(name);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let ended = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !ended.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let ended = ended.clone();
                        workers.push(std::thread::spawn(move || {
                            socket.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
                            while !ended.load(Ordering::Relaxed) {
                                match socket.read(&mut [0u8]) {
                                    Ok(0) => break,
                                    Ok(_) => {
                                        if socket.write_all(&[identity]).is_err() {
                                            break;
                                        }
                                    }
                                    Err(e)
                                        if matches!(
                                            e.kind(),
                                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                        ) => {}
                                    Err(_) => break,
                                }
                            }
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("fake agent accept: {e}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Agent { path, stop, thread: Some(thread) }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

/// Exercise the real `hc connect` bridge, not a manually injected socket offer.
struct Bridge {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Bridge {
    fn open(s: &Session, agent: &Agent) -> Bridge {
        Self::open_from(s, agent, true)
    }

    fn open_from(s: &Session, agent: &Agent, ssh: bool) -> Bridge {
        let mut cmd = s.cmd();
        if !ssh {
            for name in herdr_core::platform::SSH_LOGIN {
                cmd.env_remove(name);
            }
        }
        let mut child = cmd
            .env("SSH_AUTH_SOCK", &agent.path)
            .args(["connect"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0", "id":0, "method":"hello", "params":{
                "framed":true, "generation":1, "features":["resize_notifications"]
            }})
        )
        .unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["result"]["generation"], 1, "{reply}");
        let mut bridge = Bridge { child, input, output };
        // The framed reply synchronizes with post-hello registration.
        bridge.status();
        bridge
    }

    fn status(&mut self) {
        self.call("server.status", json!({}));
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params});
        self.input.write_all(&wire::encode(0, wire::DATA, request.to_string().as_bytes())).unwrap();
        loop {
            let (channel, kind, payload) = self.frame();
            if channel != 0 || kind != wire::DATA {
                self.credit(channel, payload.len());
                continue;
            }
            let reply: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert!(reply.get("result").is_some(), "{reply}");
            return reply["result"].clone();
        }
    }

    fn frame(&mut self) -> (u32, u8, Vec<u8>) {
        let mut header = [0; wire::HEADER];
        self.output.read_exact(&mut header).unwrap();
        let (channel, kind, len) = wire::decode_header(&header).unwrap();
        let mut payload = vec![0; len];
        self.output.read_exact(&mut payload).unwrap();
        (channel, kind, payload)
    }

    fn credit(&mut self, channel: u32, bytes: usize) {
        self.input.write_all(&wire::encode(channel, wire::CREDIT, &(bytes as u32).to_le_bytes())).unwrap();
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn probe(path: &Path) -> Option<u8> {
    let mut socket = UnixStream::connect(path).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    socket.write_all(b"?").ok()?;
    let mut answer = [0];
    socket.read_exact(&mut answer).ok()?;
    Some(answer[0])
}

#[test]
fn default_off_keeps_the_bridge_and_environment_unchanged() {
    let s = session(false);
    let agent = Agent::new(&s, "agent", b'A');
    let _bridge = Bridge::open(&s, &agent);
    assert!(!endpoint(&s).exists());
    let out = s.hc(&["exec", "printf '%s' \"${SSH_AUTH_SOCK-unset}\""]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"unset");
}

#[test]
fn enabled_programs_get_a_stable_endpoint_before_any_client_registers() {
    let s = session(true);
    let path = endpoint(&s);
    assert!(path.exists(), "enabled relay must listen before an agent connects");
    assert_eq!(probe(&path), None);
    let out = s.hc(&["exec", "printf '%s' \"$SSH_AUTH_SOCK\""]);
    assert_eq!(out.stdout, path.to_str().unwrap().as_bytes());
    let out = s.hc(&["exec", "--env", "SSH_AUTH_SOCK=/explicit", "printf '%s' \"$SSH_AUTH_SOCK\""]);
    assert_eq!(out.stdout, b"/explicit");
    let terminal = run(&s, &["sh", "-c", "printf 'agent=%s\\n' \"$SSH_AUTH_SOCK\"; sleep 100"]);
    wait_until("terminal gets the relay environment", || text(&s, &terminal).contains(path.to_str().unwrap()));
    let agent = Agent::new(&s, "first-agent", b'A');
    let bridge = Bridge::open(&s, &agent);
    assert_eq!(probe(&path), Some(b'A'));
    drop(bridge);
    wait_until("disconnect revokes the agent", || probe(&path).is_none());
    let other = Agent::new(&s, "new-agent", b'B');
    let _bridge = Bridge::open(&s, &other);
    assert_eq!(probe(&path), Some(b'B'));
    assert!(get(&s, &terminal)["pid"].is_number());
}

#[test]
fn newest_live_client_wins_and_disconnect_closes_its_existing_channels() {
    let s = session(true);
    let first = Agent::new(&s, "first", b'A');
    let second = Agent::new(&s, "second", b'B');
    let _older = Bridge::open(&s, &first);
    let newer = Bridge::open(&s, &second);
    let path = endpoint(&s);
    assert_eq!(probe(&path), Some(b'B'));
    let mut active = UnixStream::connect(&path).unwrap();
    active.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    active.write_all(b"?").unwrap();
    let mut answer = [0];
    active.read_exact(&mut answer).unwrap();
    assert_eq!(answer, [b'B']);
    drop(newer);
    match active.read(&mut answer) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("revocation must close the channel, not time out: {result:?}"),
    }
    wait_until("older live provider becomes usable", || probe(&path) == Some(b'A'));
}

#[test]
fn core_restart_keeps_terminal_pid_and_endpoint_but_not_the_old_registration() {
    let s = session(true);
    let agent = Agent::new(&s, "agent", b'A');
    let bridge = Bridge::open(&s, &agent);
    let terminal = run(&s, &["sleep", "100"]);
    let pid = get(&s, &terminal)["pid"].clone();
    let path = endpoint(&s);
    assert_eq!(probe(&path), Some(b'A'));
    s.json(&["server", "restart"]);
    drop(bridge);
    assert_eq!(get(&s, &terminal)["pid"], pid);
    assert_eq!(probe(&path), None);
    let _bridge = Bridge::open(&s, &agent);
    assert_eq!(probe(&path), Some(b'A'));
}

#[test]
fn local_non_ssh_bridges_never_offer_their_desktop_agent() {
    let s = session(true);
    let agent = Agent::new(&s, "desktop-agent", b'A');
    let _bridge = Bridge::open_from(&s, &agent, false);
    assert_eq!(probe(&endpoint(&s)), None);
}

#[test]
fn an_unreachable_newer_agent_falls_back_before_delivering_any_request() {
    let s = session(true);
    let first = Agent::new(&s, "working", b'A');
    let second = Agent::new(&s, "disappearing", b'B');
    let _older = Bridge::open(&s, &first);
    let _newer = Bridge::open(&s, &second);
    drop(second);
    assert_eq!(probe(&endpoint(&s)), Some(b'A'));
}

#[test]
fn idle_heartbeat_credits_keep_the_agent_alive_but_a_silent_peer_expires() {
    let s = session_with_timeout(true, 0.15, 20);
    let agent = Agent::new(&s, "agent", b'A');
    let mut bridge = Bridge::open(&s, &agent);
    let channel = bridge.call("events.subscribe", json!({}))["channel"].as_u64().unwrap() as u32;
    for _ in 0..15 {
        let (got, kind, payload) = bridge.frame();
        assert_eq!((got, kind), (channel, wire::DATA));
        bridge.credit(channel, payload.len());
        assert_eq!(probe(&endpoint(&s)), Some(b'A'));
    }
    // Keep both the process and its pipes open, but stop end-to-end peer traffic.
    wait_until("half-open provider expires", || probe(&endpoint(&s)).is_none());
    bridge.status(); // only the agent is revoked, not the terminal/control connection
    drop(bridge);
    let _fresh = Bridge::open(&s, &agent);
    assert_eq!(probe(&endpoint(&s)), Some(b'A'));
}

#[test]
fn a_real_terminal_program_can_talk_to_the_fake_agent() {
    let s = session(true);
    let agent = Agent::new(&s, "agent", b'A');
    let _bridge = Bridge::open(&s, &agent);
    let terminal = run(
        &s,
        &[
            "python3",
            "-c",
            "import os,socket,time; s=socket.socket(socket.AF_UNIX); s.connect(os.environ['SSH_AUTH_SOCK']); s.sendall(b'?'); print('agent_reply='+str(s.recv(1)[0]),flush=True); time.sleep(100)",
        ],
    );
    wait_until("PTY program receives its agent reply", || text(&s, &terminal).contains("agent_reply=65"));
}

#[test]
fn kept_terminal_recovery_gets_the_relay_environment() {
    let s = session(true);
    let terminal = s.json(&["run", "--bg", "--keep", "sleep", "100"])["address"].as_str().unwrap().to_string();
    let before = get(&s, &terminal)["pid"].as_i64().unwrap();
    assert_eq!(unsafe { libc::kill(before as i32, libc::SIGKILL) }, 0);
    wait_until("kept terminal gets a fresh shell", || {
        get(&s, &terminal)["pid"].as_i64().is_some_and(|pid| pid != before)
    });
    s.json(&["send", &terminal, "printf 'recovered=%s\\n' \"$SSH_AUTH_SOCK\""]);
    let expected = format!("recovered={}", endpoint(&s).display());
    wait_until("recovered shell has the stable endpoint", || text(&s, &terminal).contains(&expected));
}
