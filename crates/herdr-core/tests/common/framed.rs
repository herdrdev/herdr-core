//! A framed-mode test client and a viewer that renders what it receives.

use super::Session;
use herdr_core::wire;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub type Frame = (u32, u8, Vec<u8>);

pub struct Framed {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// Frames read while waiting for something else.
    pub stash: VecDeque<Frame>,
}

impl Framed {
    pub fn open(s: &Session) -> Framed {
        s.json(&["server", "start"]);
        let stream = UnixStream::connect(s.socket()).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        writeln!(writer, r#"{{"jsonrpc":"2.0","id":0,"method":"hello","params":{{"framed":true}}}}"#).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        Framed { reader, writer, next_id: 1, stash: VecDeque::new() }
    }

    pub fn send(&mut self, channel: u32, kind: u8, payload: &[u8]) {
        self.writer.write_all(&wire::encode(channel, kind, payload)).unwrap();
    }

    /// The next frame from the wire, or None after `timeout`.
    pub fn read(&mut self, timeout: Duration) -> Option<Frame> {
        self.reader.get_ref().set_read_timeout(Some(timeout)).unwrap();
        let mut header = [0u8; wire::HEADER];
        if self.reader.read_exact(&mut header).is_err() {
            return None;
        }
        self.reader.get_ref().set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (channel, kind, len) = wire::decode_header(&header).unwrap();
        let mut payload = vec![0; len];
        self.reader.read_exact(&mut payload).unwrap();
        Some((channel, kind, payload))
    }

    /// One call on channel 0; other frames that arrive meanwhile are stashed.
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(0, wire::DATA, req.to_string().as_bytes());
        loop {
            let frame = self.read(Duration::from_secs(10)).expect("reply");
            if frame.0 == 0 && frame.1 == wire::DATA {
                let reply: Value = serde_json::from_slice(&frame.2).unwrap();
                if reply["id"] == id {
                    return reply;
                }
            }
            self.stash.push_back(frame);
        }
    }
}

pub struct Viewer {
    pub conn: Framed,
    pub channel: u32,
    pub screen: ghostty_vt::Terminal,
    pub received: usize,
    pub raw: Vec<u8>,
    pub closed: Option<Value>,
    /// Send credit back as bytes arrive (a well-behaved viewer).
    pub credit: bool,
}

impl Viewer {
    pub fn attach(s: &Session, terminal: &str, cols: u16, rows: u16, extra: Value) -> Viewer {
        let mut conn = Framed::open(s);
        let mut params = json!({"terminal": terminal, "cols": cols, "rows": rows});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let reply = conn.call("terminal.attach", params);
        let channel = reply["result"]["channel"].as_u64().unwrap_or_else(|| panic!("{reply}")) as u32;
        let screen = ghostty_vt::Terminal::new(cols, rows, 1 << 20);
        Viewer { conn, channel, screen, received: 0, raw: Vec::new(), closed: None, credit: true }
    }

    fn take(&mut self, (channel, kind, payload): Frame) {
        if channel != self.channel {
            return;
        }
        match kind {
            wire::DATA => {
                self.screen.write(&payload);
                self.received += payload.len();
                if self.raw.len() < 1 << 20 {
                    self.raw.extend_from_slice(&payload);
                }
                if self.credit {
                    self.conn.send(self.channel, wire::CREDIT, &(payload.len() as u32).to_le_bytes());
                }
            }
            wire::CLOSE => self.closed = Some(serde_json::from_slice(&payload).unwrap_or(Value::Null)),
            wire::RESIZE if payload.len() == 4 => {
                let side = |at: usize| u16::from_le_bytes([payload[at], payload[at + 1]]);
                self.screen.resize(side(0), side(2));
            }
            _ => {}
        }
    }

    /// Reads stream frames until `done` holds (10 s at most).
    pub fn until(&mut self, what: &str, done: impl Fn(&Viewer) -> bool) {
        let start = Instant::now();
        while let Some(frame) = self.conn.stash.pop_front() {
            self.take(frame);
        }
        while !done(self) {
            assert!(start.elapsed() < Duration::from_secs(10), "viewer timed out waiting for {what}:\n{}", self.text());
            if let Some(frame) = self.conn.read(Duration::from_millis(50)) {
                self.take(frame);
            }
        }
    }

    /// Reads until nothing arrives for `quiet`.
    pub fn drain(&mut self, quiet: Duration) {
        while let Some(frame) = self.conn.stash.pop_front() {
            self.take(frame);
        }
        while let Some(frame) = self.conn.read(quiet) {
            self.take(frame);
        }
    }

    pub fn shows(&mut self, needle: &str) {
        let needle = needle.to_string();
        self.until(&needle.clone(), move |v| v.text().contains(&needle));
    }

    pub fn text(&self) -> String {
        self.screen.plain_text()
    }

    pub fn typed(&mut self, bytes: &[u8]) {
        self.conn.send(self.channel, wire::DATA, bytes);
    }
}
