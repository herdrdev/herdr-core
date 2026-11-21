//! Terminal endpoints are first-instance local named pipes.

use super::AsyncListener;
use crate::endpoints::{self, EndpointRecord};
use std::io;
use std::path::{Path, PathBuf};

const PIPE_PREFIX: &str = r"\\.\pipe\herdr-core-endpoint-";

pub(crate) fn terminal_endpoint_path(_socket: &Path, random: &str) -> io::Result<PathBuf> {
    Ok(format!("{PIPE_PREFIX}{random}.sock").into())
}

pub(crate) fn valid_terminal_endpoint(_socket: &Path, endpoint: &Path) -> bool {
    endpoint.to_str().and_then(|path| path.strip_prefix(PIPE_PREFIX)).is_some_and(endpoints::valid_name)
}

pub(crate) fn verify_terminal_endpoint(_path: &Path, _create: bool) -> io::Result<()> {
    Ok(())
}

pub(crate) fn terminal_endpoint_identity(_path: &Path) -> io::Result<Option<(u64, u64)>> {
    // There is no socket file. Attempt a first-instance pipe when reconciling.
    Ok(None)
}

pub(crate) fn bind_terminal_endpoint(path: &Path) -> io::Result<(AsyncListener, EndpointRecord)> {
    Ok((AsyncListener::serve(path.into())?, EndpointRecord { path: path.into(), dev: 0, ino: 0 }))
}

pub(crate) fn cleanup_terminal_endpoint(_record: &EndpointRecord) -> bool {
    // The name goes away when the remaining handles close.
    false
}

pub(crate) async fn probe_terminal_endpoint(_path: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
