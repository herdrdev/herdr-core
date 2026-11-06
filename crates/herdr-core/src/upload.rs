//! `file.upload`: a file too big for one message, sent in parts and staged
//! on the machine that will have it; `terminal.put` or `file.write` then
//! takes it whole by its id.

use crate::api::{UploadParams, UploadResult};
use crate::rpc::RpcError;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A staged upload nobody took is removed after this long.
const UNCLAIMED: Duration = Duration::from_secs(24 * 3600);

fn dir(state: &Path) -> PathBuf {
    state.join("uploads")
}

/// Where upload `id` is staged; ids are this core's own hex, nothing else.
pub fn staged(state: &Path, id: &str) -> Result<PathBuf, RpcError> {
    let gone = || RpcError::new("not_found", format!("no upload {id:?}"), "start it again: it was taken or expired");
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(gone());
    }
    let path = dir(state).join(id);
    if path.is_file() { Ok(path) } else { Err(gone()) }
}

/// Appends a part at `offset`, which must be what is staged so far: a part
/// sent twice or lost is refused, never written twice or skipped.
pub fn append(state: &Path, p: UploadParams) -> Result<UploadResult, RpcError> {
    let bytes = crate::put::decode(&p.data).ok_or_else(|| RpcError::invalid_params("data is not base64"))?;
    if bytes.len() > crate::put::MAX_BYTES {
        return Err(RpcError::invalid_params(format!("at most {} MB per part", crate::put::MAX_BYTES >> 20)));
    }
    let io = |e: std::io::Error| RpcError::new("io", format!("cannot stage the upload: {e}"), "");
    let (id, path) = match p.upload {
        Some(id) => (id.clone(), staged(state, &id)?),
        None => {
            if p.offset != 0 {
                return Err(RpcError::invalid_params("a new upload starts at offset 0"));
            }
            std::fs::create_dir_all(dir(state)).map_err(io)?;
            expire(&dir(state));
            let id = crate::platform::random_hex();
            let path = dir(state).join(&id);
            std::fs::File::create_new(&path).map_err(io)?;
            (id, path)
        }
    };
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).map_err(io)?;
    let has = f.metadata().map_err(io)?.len();
    if has != p.offset {
        return Err(RpcError::invalid_params(format!(
            "upload {id} has {has} bytes, not {}: send from there",
            p.offset
        )));
    }
    f.write_all(&bytes).map_err(io)?;
    Ok(UploadResult { upload: id, bytes: has + bytes.len() as u64 })
}

/// Removes uploads untouched for `UNCLAIMED` (a sender that gave up).
fn expire(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let idle = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok());
        if idle.is_some_and(|idle| idle > UNCLAIMED) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(upload: Option<&str>, offset: u64, bytes: &[u8]) -> UploadParams {
        UploadParams { on: None, upload: upload.map(String::from), offset, data: crate::put::encode(bytes) }
    }

    #[test]
    fn parts_append_only_at_the_staged_size() {
        let state = std::env::temp_dir().join(format!("hc-upload-{}", crate::platform::random_hex()));
        let first = append(&state, part(None, 0, b"hello ")).unwrap();
        assert_eq!(first.bytes, 6);
        let id = first.upload.as_str();
        assert_eq!(append(&state, part(Some(id), 6, b"world")).unwrap().bytes, 11);
        let again = append(&state, part(Some(id), 6, b"world")).err().unwrap();
        assert_eq!((again.kind.as_str(), again.message.contains("has 11 bytes")), ("invalid_params", true), "resent");
        assert_eq!(std::fs::read(staged(&state, id).unwrap()).unwrap(), b"hello world");
        assert_eq!(append(&state, part(None, 3, b"x")).err().unwrap().kind, "invalid_params");
        for id in ["../uploads", "", "zz", "0123"] {
            assert_eq!(staged(&state, id).unwrap_err().kind, "not_found", "{id:?}");
        }
        std::fs::remove_dir_all(&state).unwrap();
    }
}
