//! Where a core lives (socket, holder socket, state, config) and how its
//! state files are read and written.

use crate::rpc::RpcError;
use std::io;
use std::path::{Path, PathBuf};

/// Where one core keeps its socket and state. `HC_SESSION` names an isolated core.
pub struct Paths {
    pub socket: PathBuf,
    pub holder_socket: PathBuf,
    pub state: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Paths, RpcError> {
        let session = session(std::env::var("HC_SESSION").ok())?;
        let xdg = |var: &str, fallback: PathBuf| {
            std::env::var_os(var).filter(|v| !v.is_empty()).map_or(fallback, PathBuf::from)
        };
        let (state_home, config_home) = crate::platform::data_dirs();
        let state = xdg("XDG_STATE_HOME", state_home).join("herdr-core").join(&session);
        let config = xdg("XDG_CONFIG_HOME", config_home).join("herdr-core").join("config.toml");
        // Where sockets go (Windows: the named pipe namespace), and their name prefix.
        let (run, prefix) = crate::platform::endpoints(&state);
        // HC_SOCKET, else `socket` in config, else the runtime dir. The holder sits beside it.
        let chosen = std::env::var_os("HC_SOCKET")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::config::load(&config).ok()?.socket.map(PathBuf::from));
        let (socket, holder_socket) = match chosen {
            Some(socket) => (socket.clone(), socket.with_extension("holder.sock")),
            None => (run.join(format!("{prefix}{session}.sock")), run.join(format!("{prefix}{session}.holder.sock"))),
        };
        // Holders listen on `<holder_socket minus .sock>.<id>.sock`: the longest path.
        let longest = holder_socket.as_os_str().len() + 1 + HOLDER_ID;
        let max = crate::platform::MAX_SOCKET_PATH;
        if socket.as_os_str().len().max(longest) > max {
            return Err(RpcError::new(
                "usage",
                format!(
                    "the socket path for session {session:?} is too long here ({longest} bytes; this system allows {max})"
                ),
                "use a shorter HC_SESSION (or socket in the config)",
            ));
        }
        Ok(Paths { socket, holder_socket, config, state })
    }

    pub fn create_dirs(&self) -> std::io::Result<()> {
        use crate::platform::private_dir;
        private_dir(&self.state)?;
        private_dir(self.socket.parent().expect("socket has a parent"))?;
        private_dir(self.holder_socket.parent().expect("socket has a parent"))
    }
}

/// A state file, or the empty default only when it doesn't exist yet: an
/// unreadable or broken one is an error, never silently replaced.
pub fn read_state<T: serde::de::DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| io::Error::other(at(&e))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(io::Error::new(e.kind(), at(&e))),
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
    std::fs::rename(tmp, path)
}

/// Like `write_atomic`, for a file only this user may read (it holds secrets).
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = crate::platform::private_file(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, path)
}

/// Hex digits in a holder's socket name.
pub const HOLDER_ID: usize = 8;

/// The session name goes into folder and pipe names. Spaces around it are
/// dropped (cmd's `set X=a & ...` keeps the one before `&`); characters a
/// path can't hold are refused.
fn session(raw: Option<String>) -> Result<String, RpcError> {
    let name = raw.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("default");
    if name == "." || name == ".." || name.chars().any(|c| c.is_control() || r#"/\:*?"<>|"#.contains(c)) {
        return Err(RpcError::new(
            "usage",
            format!("HC_SESSION {name:?} can't be a folder name"),
            "use letters, digits, - _ .",
        ));
    }
    Ok(name.into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_missing_state_file_starts_fresh() {
        let dir = std::env::temp_dir().join(format!("hc-state-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("unreadable.json")).unwrap();
        assert_eq!(super::read_state::<Vec<u32>>(&dir.join("missing.json")).unwrap(), Vec::<u32>::new());
        let err = super::read_state::<Vec<u32>>(&dir.join("unreadable.json")).unwrap_err();
        assert!(err.to_string().contains("unreadable.json"), "{err}");
        super::write_atomic(&dir.join("broken.json"), b"[1,").unwrap();
        assert!(super::read_state::<Vec<u32>>(&dir.join("broken.json")).is_err());
        super::write_atomic(&dir.join("ok.json"), b"[1,2]").unwrap();
        assert_eq!(super::read_state::<Vec<u32>>(&dir.join("ok.json")).unwrap(), [1, 2]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_session_name_is_trimmed_and_must_fit_in_a_path() {
        let s = |raw: Option<&str>| super::session(raw.map(Into::into)).ok();
        assert_eq!(s(Some("smoke ")).as_deref(), Some("smoke"), "cmd's set keeps the space");
        assert_eq!(s(None).as_deref(), Some("default"));
        assert_eq!(s(Some("  ")).as_deref(), Some("default"));
        assert_eq!(s(Some("MyTest")).as_deref(), Some("MyTest"), "names that worked keep working");
        for bad in ["a/b", r"a\b", "a:b", "..", "a\tb"] {
            assert_eq!(s(Some(bad)), None, "{bad:?}");
        }
    }
}
