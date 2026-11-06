//! `terminal.put`: bytes into a file beside a terminal, for its program to read.

use std::path::{Path, PathBuf};

/// Raw bytes one put may carry (inline in one message).
pub const MAX_BYTES: usize = 8 << 20;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { B64[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text {
        acc = acc << 6 | B64.iter().position(|b| b == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Where a terminal's files go; removed with its record.
pub fn dir(state: &Path, id: u32) -> PathBuf {
    state.join("files").join(crate::terminals::address(id))
}

/// The basename, safe on any machine; `file` if nothing is left.
fn clean(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let safe: String =
        base.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let safe = safe.trim_start_matches('.').to_string();
    if safe.is_empty() { "file".into() } else { safe }
}

/// What a put lands: bytes it carried, or a staged `file.upload`.
pub enum Source {
    Bytes(Vec<u8>),
    Staged(PathBuf),
}

/// Lands `source` as `name` (or `name-2.ext`, ... when taken) beside terminal
/// `id`; returns where and how many bytes.
pub fn stage(state: &Path, id: u32, name: &str, source: Source) -> std::io::Result<(PathBuf, usize)> {
    let dir = dir(state, id);
    std::fs::create_dir_all(&dir)?;
    let name = clean(name);
    let (stem, ext) = match name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
        Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
        None => (name.clone(), String::new()),
    };
    for n in 1.. {
        let path = dir.join(if n == 1 { name.clone() } else { format!("{stem}-{n}{ext}") });
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                return match source {
                    Source::Bytes(bytes) => std::io::Write::write_all(&mut f, &bytes).map(|_| (path, bytes.len())),
                    // The name is held; the staged file replaces the empty one, same filesystem.
                    // Closed first: Windows won't replace a file that is open.
                    Source::Staged(staged) => {
                        drop(f);
                        let len = std::fs::metadata(&staged)?.len() as usize;
                        std::fs::rename(&staged, &path).map(|_| (path.clone(), len)).inspect_err(|_| {
                            let _ = std::fs::remove_file(&path);
                        })
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_and_names_are_safe() {
        for len in 0..10 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37) as u8).collect();
            assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(encode(b"hi!"), "aGkh");
        assert_eq!(decode("not base64!"), None);
        assert_eq!(clean("../../etc/pass wd.png"), "pass_wd.png");
        assert_eq!(clean(".."), "file");
    }
}
