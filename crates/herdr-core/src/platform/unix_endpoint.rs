//! Filesystem ownership and non-blocking probes for terminal endpoints.

use super::{AsyncListener, Listener};
use crate::endpoints::{self, EndpointRecord};
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

fn endpoint_folder(socket: &Path) -> io::Result<PathBuf> {
    let name = socket.file_name().ok_or_else(|| io::Error::other("the shared socket has no file name"))?;
    let parent = socket.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let mut folder = name.to_os_string();
    folder.push(".e");
    Ok(parent.canonicalize()?.join(folder))
}

pub(crate) fn terminal_endpoint_path(socket: &Path, random: &str) -> io::Result<PathBuf> {
    Ok(endpoint_folder(socket)?.join(format!("{random}.sock")))
}

pub(crate) fn valid_terminal_endpoint(socket: &Path, endpoint: &Path) -> bool {
    endpoint_folder(socket).is_ok_and(|folder| endpoint.parent() == Some(folder.as_path()))
        && endpoint.file_name().and_then(|name| name.to_str()).is_some_and(endpoints::valid_name)
}

pub(crate) fn verify_terminal_endpoint(path: &Path, create: bool) -> io::Result<()> {
    let folder = path.parent().ok_or_else(|| io::Error::other("the endpoint has no folder"))?;
    if create {
        match DirBuilder::new().mode(0o700).create(folder) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let metadata = fs::symlink_metadata(folder)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7777 != 0o700 {
        return Err(io::Error::other(format!(
            "{} must be a directory owned by this user with mode 0700",
            folder.display()
        )));
    }
    Ok(())
}

pub(crate) fn terminal_endpoint_identity(path: &Path) -> io::Result<Option<(u64, u64)>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => Ok(Some((metadata.dev(), metadata.ino()))),
        Ok(_) => Err(io::Error::other("the endpoint path is not a socket")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn bind_terminal_endpoint(path: &Path) -> io::Result<(AsyncListener, EndpointRecord)> {
    // Unlike the shared listener, this bind must leave any existing name alone.
    let listener = Listener::bind(path)?;
    let (dev, ino) =
        terminal_endpoint_identity(path)?.ok_or_else(|| io::Error::other("the new endpoint disappeared"))?;
    let record = EndpointRecord { path: path.into(), dev, ino };
    match listener.set_nonblocking(true).and_then(|()| AsyncListener::serve(listener)) {
        Ok(listener) => Ok((listener, record)),
        Err(e) => {
            let _ = cleanup_terminal_endpoint(&record);
            Err(e)
        }
    }
}

pub(crate) fn cleanup_terminal_endpoint(record: &EndpointRecord) -> bool {
    verify_terminal_endpoint(&record.path, false).is_ok()
        && terminal_endpoint_identity(&record.path).is_ok_and(|identity| identity == Some((record.dev, record.ino)))
        && fs::remove_file(&record.path).is_ok()
}

pub(crate) async fn probe_terminal_endpoint(path: &Path) -> io::Result<()> {
    tokio::net::UnixStream::connect(path).await.map(drop)
}
