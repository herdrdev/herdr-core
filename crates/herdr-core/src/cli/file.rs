//! `hc file …`: read, write and edit files here or on a machine.

use super::*;
use crate::api::{Edit, FileAt, FileEditParams, FileReadParams, FileWriteParams};

#[derive(Subcommand)]
pub(super) enum FileCmd {
    /// Print a text file, or lines of it (--offset, --limit).
    Read(FileReadParams),
    /// Write what is piped in as the file, making missing folders.
    Write(FileWriteParams),
    /// Replace exact text found once (--old X --new Y, repeatable); all or none.
    Edit(FileEditParams),
}

/// What is piped into hc, at most `max` bytes.
pub(super) fn piped(max: usize) -> Result<Vec<u8>, RpcError> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(RpcError::new("usage", "nothing piped in", "pipe it in: hc ... < file"));
    }
    let mut bytes = Vec::new();
    stdin.lock().take(max as u64 + 1).read_to_end(&mut bytes).map_err(|e| RpcError::internal(e.to_string()))?;
    if bytes.len() > max {
        return Err(RpcError::invalid_params(format!("at most {} MB piped in", max >> 20)));
    }
    Ok(bytes)
}

/// Bytes for one call: inline when they fit one message, else staged in parts.
pub(super) enum Payload {
    Inline(Vec<u8>),
    Upload(String),
}

/// Parts this size share a machine's line with terminal output without
/// holding it up for long, and arrive within `machine_timeout` on slow links.
const PART: usize = 2 << 20;

/// All of `from`: inline up to 8 MB (any core takes that), past it sent in
/// parts with `file.upload` to `on` (the machine that will use it).
pub(super) fn payload(
    mut from: impl Read,
    on: Option<&str>,
    call: &impl Fn(&str, Value) -> Result<Value, RpcError>,
) -> Result<Payload, RpcError> {
    let read = |from: &mut dyn Read, max: usize| {
        let mut buf = Vec::new();
        from.take(max as u64).read_to_end(&mut buf).map(|_| buf).map_err(|e| RpcError::internal(e.to_string()))
    };
    let mut buf = read(&mut from, crate::put::MAX_BYTES + 1)?;
    if buf.len() <= crate::put::MAX_BYTES {
        return Ok(Payload::Inline(buf));
    }
    let (mut upload, mut offset) = (None::<String>, 0u64);
    loop {
        let rest = if buf.len() > PART { buf.split_off(PART) } else { Vec::new() };
        let mut p = json!({"offset": offset, "data": crate::put::encode(&buf)});
        p["on"] = on.into();
        p["upload"] = upload.clone().into();
        let r = call("file.upload", p)?;
        upload = r["upload"].as_str().map(String::from);
        offset = r["bytes"].as_u64().unwrap_or_default();
        buf = if rest.is_empty() { read(&mut from, PART)? } else { rest };
        if buf.is_empty() {
            return upload.map(Payload::Upload).ok_or_else(|| RpcError::internal("no upload id"));
        }
    }
}

/// The caller's folder for a path here (the core's may be elsewhere).
fn here_unless_on(at: &mut FileAt) {
    if at.on.is_none() {
        at.cwd = from_here(at.cwd.take());
    }
}

pub(super) fn file(
    action: FileCmd,
    asked: bool,
    call: impl Fn(&str, Value) -> Result<Value, RpcError>,
) -> Result<Option<Out>, RpcError> {
    Ok(Some(match action {
        FileCmd::Read(mut p) => {
            here_unless_on(&mut p.at);
            let r = call("file.read", to(&p))?;
            if asked {
                return Ok(Some(Out { json: r, human: String::new() }));
            }
            // Text, even when piped, as `hc read` does.
            use std::io::Write;
            let _ = std::io::stdout().write_all(r["text"].as_str().unwrap_or_default().as_bytes());
            let (first, last, lines) = (r["first"].as_u64(), r["last"].as_u64(), r["lines"].as_u64());
            if first > Some(1) || last < lines {
                eprintln!("[hc] lines {}-{} of {}", r["first"], r["last"], r["lines"]);
            }
            return Ok(None);
        }
        FileCmd::Write(mut p) => {
            here_unless_on(&mut p.at);
            if std::io::stdin().is_terminal() {
                return Err(RpcError::new("usage", "nothing piped in", "pipe it in: hc ... < file"));
            }
            match payload(std::io::stdin().lock(), p.at.on.as_deref(), &call)? {
                Payload::Inline(bytes) => p.data = crate::put::encode(&bytes),
                Payload::Upload(id) => p.upload = Some(id),
            }
            let r = call("file.write", to(&p))?;
            let human = format!("wrote {} ({} bytes)", r["path"].as_str().unwrap_or_default(), r["bytes"]);
            Out { json: r, human }
        }
        FileCmd::Edit(mut p) => {
            here_unless_on(&mut p.at);
            if p.old.len() != p.new.len() {
                return Err(RpcError::new("usage", "give a --new for each --old", "--old X --new Y"));
            }
            p.edits = p.old.drain(..).zip(p.new.drain(..)).map(|(old, new)| Edit { old, new }).collect();
            let r = call("file.edit", to(&p))?;
            let human = format!("edited {}", r["path"].as_str().unwrap_or_default());
            Out { json: r, human }
        }
    }))
}
