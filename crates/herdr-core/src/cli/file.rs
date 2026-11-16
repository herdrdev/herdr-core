//! `hc file …`: read, write and edit files here or on a machine.

use super::*;
use crate::api::{Edit, FileAt, FileEditParams, FileReadParams, FileWriteParams};

#[derive(Subcommand)]
pub(super) enum FileCmd {
    /// Print a text file, or lines of it (--offset, --limit); --bytes for any file.
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
            // Text (or the bytes asked for), even when piped, as `hc read` does.
            use std::io::Write;
            if let Some(data) = r["data"].as_str() {
                let bytes = crate::put::decode(data).ok_or_else(|| RpcError::internal("data is not base64"))?;
                let _ = std::io::stdout().write_all(&bytes);
                return Ok(None);
            }
            let _ = std::io::stdout().write_all(r["text"].as_str().unwrap_or_default().as_bytes());
            let (first, last, lines) = (r["first"].as_u64(), r["last"].as_u64(), r["lines"].as_u64());
            if first > Some(1) || last < lines {
                eprintln!("[hc] lines {}-{} of {}", r["first"], r["last"], r["lines"]);
            }
            return Ok(None);
        }
        FileCmd::Write(mut p) => {
            here_unless_on(&mut p.at);
            p.data = crate::put::encode(&piped(crate::files::MAX_BYTES)?);
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
