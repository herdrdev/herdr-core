//! Core ↔ holder wire: `[kind u8][terminal u32 LE][len u32 LE][payload]`.
//! Control payloads are JSON, terminal bytes are raw. `VERSION` is checked in
//! `Hello`/`Welcome` and is separate from ownership (the newest `Hello` owns).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Read};

pub const VERSION: u32 = 1;
const MAX_PAYLOAD: usize = 16 << 20;

// core → holder
pub const HELLO: u8 = 1;
pub const SPAWN: u8 = 2;
pub const INPUT: u8 = 3;
pub const RESIZE: u8 = 4;
pub const SIGNAL: u8 = 5;
pub const RELEASE: u8 = 6;
pub const GOODBYE: u8 = 7;
/// Which process group has the terminal now (a shell got its prompt back?).
pub const FOREGROUND: u8 = 8;
/// An older build: take no new terminals, exit once the last one is gone.
pub const DRAIN: u8 = 9;
// holder → core
pub const WELCOME: u8 = 101;
pub const TAIL: u8 = 102;
pub const OUTPUT: u8 = 103;
pub const SPAWNED: u8 = 104;
pub const SPAWN_FAILED: u8 = 105;
pub const EXITED: u8 = 106;
pub const BYE: u8 = 107;
pub const FOREGROUND_IS: u8 = 108;

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Spawn {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Serialize, Deserialize)]
pub struct Signal {
    pub signal: i32,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Goodbye {
    /// The core is restarting on purpose; its screens are saved.
    Handover,
    /// Server stop: the holder exits.
    Shutdown,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Serialize, Deserialize)]
pub struct Welcome {
    pub version: u32,
    /// Random per holder process: a different one means the old holder is gone.
    pub instance: String,
    pub pid: u32,
    /// Which binary it runs (see client::build_id); a core of another build
    /// starts a new holder for new terminals and lets this one drain.
    #[serde(default)]
    pub build: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_session: Option<crate::platform::LoginSession>,
    #[serde(default)]
    pub draining: bool,
    /// How the previous core left: "none", "handover" or "crash".
    pub previous: String,
    pub terminals: Vec<HeldTerminal>,
}

/// One terminal the holder runs; a `TAIL` frame with its recent output follows the welcome.
#[derive(Serialize, Deserialize)]
pub struct HeldTerminal {
    pub id: u32,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    pub exit: Option<Exit>,
}

#[derive(Serialize, Deserialize)]
pub struct Foreground {
    /// -1 when there is none or the terminal is gone.
    pub pgid: i32,
}

#[derive(Serialize, Deserialize)]
pub struct Spawned {
    pub pid: u32,
}

#[derive(Serialize, Deserialize)]
pub struct SpawnFailed {
    pub error: String,
}

pub fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + payload.len());
    out.push(kind);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn json_frame(kind: u8, id: u32, msg: &impl Serialize) -> Vec<u8> {
    frame(kind, id, &serde_json::to_vec(msg).expect("serializable"))
}

/// Reads one frame; `Ok(None)` at a clean end of stream.
pub fn read(r: &mut impl Read) -> io::Result<Option<(u8, u32, Vec<u8>)>> {
    let mut header = [0u8; 9];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let id = u32::from_le_bytes(header[1..5].try_into().unwrap());
    let len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::other(format!("holder frame of {len} bytes")));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok(Some((header[0], id, payload)))
}

pub fn parse<T: serde::de::DeserializeOwned>(payload: &[u8]) -> io::Result<T> {
    serde_json::from_slice(payload).map_err(io::Error::other)
}
