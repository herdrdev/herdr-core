//! Terminal-owned listeners and caller attribution for requests on them.

use crate::paths::Paths;
use crate::platform::{self, AsyncListener};
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use crate::terminals::{Binding, Registry, Terminal, address};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const CALLER_METHODS: &[&str] = &[
    "terminal.run",
    "terminal.get",
    "terminal.kill",
    "terminal.rm",
    "terminal.read",
    "terminal.attach",
    "terminal.send",
    "terminal.wait",
    "terminal.keys",
];

pub(crate) const LAUNCHD_SENTINEL: &str = "--hc-launchd";
pub(crate) const MODE: &str = "HC_ENDPOINT";

pub(crate) fn in_cli() -> bool {
    std::env::var_os(MODE).is_some_and(|value| value == "1")
}

pub(crate) fn refuses_entry(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .find(|arg| arg != "--json")
        .is_some_and(|word| matches!(word.to_str(), Some("serve" | "hold" | "--hc-install-bundle" | LAUNCHD_SENTINEL)))
}

pub(crate) fn lifecycle_refusal() -> RpcError {
    RpcError::new("usage", "this terminal talks to its own endpoint; run that outside it", "")
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EndpointRecord {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

pub(crate) struct Bound {
    id: u32,
    state: Arc<EndpointState>,
}

impl Bound {
    pub(crate) fn terminal(&self) -> String {
        address(self.id)
    }

    pub(crate) fn retired(&self) -> bool {
        self.state.retired.load(Ordering::SeqCst)
    }
}

pub(crate) struct EndpointState {
    record: EndpointRecord,
    retired: AtomicBool,
    accept: Mutex<Accept>,
}

enum Accept {
    Waiting,
    Ready(AsyncListener),
    Running(tokio::task::AbortHandle),
}

impl EndpointState {
    fn new(record: EndpointRecord, accept: Accept) -> Arc<Self> {
        Arc::new(Self { record, retired: AtomicBool::new(false), accept: Mutex::new(accept) })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.record.path
    }

    fn start(self: &Arc<Self>, core: &Arc<Core>, id: u32) {
        let mut accept = self.accept.lock().unwrap_or_else(|p| p.into_inner());
        let Accept::Ready(mut listener) = std::mem::replace(&mut *accept, Accept::Waiting) else {
            unreachable!("only a ready endpoint starts accepting")
        };
        let (core, state) = (core.clone(), self.clone());
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((read, write)) => {
                        let bound = Bound { id, state: state.clone() };
                        tokio::spawn(crate::server::connection(core.clone(), read, write, Some(bound)));
                    }
                    Err(e) => {
                        eprintln!("core: cannot accept endpoint for {}: {e}", address(id));
                        break;
                    }
                }
            }
        });
        *accept = Accept::Running(task.abort_handle());
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        let accept = std::mem::replace(&mut *self.accept.lock().unwrap_or_else(|p| p.into_inner()), Accept::Waiting);
        if let Accept::Running(task) = accept {
            task.abort();
        }
        let _ = platform::cleanup_terminal_endpoint(&self.record);
    }
}

pub(crate) fn new_path(paths: &Paths) -> Result<PathBuf, RpcError> {
    let random = platform::random_hex();
    let path = platform::terminal_endpoint_path(&paths.socket, &random[..16]).map_err(endpoint_error)?;
    if path.to_str().is_none() {
        return Err(RpcError::new("unavailable", "the endpoint path is not valid UTF-8", "use a UTF-8 socket path"));
    }
    if !fits_client(&path) {
        return Err(RpcError::new(
            "unavailable",
            "the endpoint path is too long here",
            "use a shorter HC_SESSION or HC_SOCKET",
        ));
    }
    Ok(path)
}

fn fits_client(path: &Path) -> bool {
    crate::paths::socket_path_len(path, &path.with_extension("holder.sock")) <= platform::MAX_SOCKET_PATH
}

fn endpoint_error(e: io::Error) -> RpcError {
    RpcError::new("unavailable", format!("cannot bind terminal endpoint: {e}"), "check the endpoint location")
}

pub(crate) fn valid_name(name: &str) -> bool {
    name.strip_suffix(".sock")
        .is_some_and(|name| name.len() == 16 && name.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

/// Owns the new listener and its name until the registry owns both.
pub(crate) struct Prepared(Option<(AsyncListener, EndpointRecord)>);

impl Prepared {
    pub(crate) fn bind(path: &Path) -> Result<Self, RpcError> {
        platform::verify_terminal_endpoint(path, true).map_err(endpoint_error)?;
        Ok(Self(Some(platform::bind_terminal_endpoint(path).map_err(endpoint_error)?)))
    }

    pub(crate) fn install(mut self, core: &Arc<Core>, st: &mut State, id: u32) {
        let (listener, record) = self.0.take().expect("prepared endpoint");
        install(st, id, listener, record).start(core, id);
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if let Some((_, record)) = &self.0 {
            let _ = platform::cleanup_terminal_endpoint(record);
        }
    }
}

fn install(st: &mut State, id: u32, listener: AsyncListener, record: EndpointRecord) -> Arc<EndpointState> {
    let row = st.reg.terminals.get_mut(&id).expect("endpoint row");
    row.endpoint = Some(record.clone());
    row.bound = Some(Binding::Bound);
    let endpoint = EndpointState::new(record, Accept::Ready(listener));
    st.endpoints.insert(id, endpoint.clone());
    endpoint
}

/// During adoption, a row can disappear before its persisted socket is reconciled.
pub(crate) fn load(reg: &Registry) -> HashMap<u32, Arc<EndpointState>> {
    reg.terminals
        .iter()
        .filter_map(|(&id, row)| Some((id, EndpointState::new(row.endpoint.clone()?, Accept::Waiting))))
        .collect()
}

pub(crate) fn retire(st: &mut State, id: u32) {
    if st.recovering_endpoints {
        return;
    }
    if let Some(endpoint) = st.endpoints.get(&id) {
        endpoint.retire();
        st.endpoints.remove(&id);
    }
    if let Some(row) = st.reg.terminals.get_mut(&id) {
        row.endpoint = None;
    }
}

pub(crate) fn shutdown(st: &mut State) {
    for (_, endpoint) in st.endpoints.drain() {
        endpoint.retire();
    }
}

#[derive(Clone, Copy, Debug)]
enum Location {
    Absent,
    OwnSocket,
    Occupied,
}

#[derive(Clone, Copy, Debug)]
enum Probe {
    Refused,
    Live,
    Indeterminate,
}

#[derive(Debug, PartialEq, Eq)]
enum Recovery {
    Bind,
    Reclaim,
    Lost,
}

fn recovery(location: Location, probe: Probe) -> Recovery {
    match (location, probe) {
        (Location::Absent, _) => Recovery::Bind,
        (Location::OwnSocket, Probe::Refused) => Recovery::Reclaim,
        _ => Recovery::Lost,
    }
}

async fn observe(record: &EndpointRecord) -> Recovery {
    let location = match platform::terminal_endpoint_identity(&record.path) {
        Ok(None) => Location::Absent,
        Ok(Some(identity)) if identity == (record.dev, record.ino) => Location::OwnSocket,
        _ => Location::Occupied,
    };
    let probe = match location {
        Location::OwnSocket => {
            let connection = platform::probe_terminal_endpoint(&record.path);
            match tokio::time::timeout(std::time::Duration::from_millis(200), connection).await {
                Ok(Ok(())) => Probe::Live,
                Ok(Err(e)) if e.kind() == io::ErrorKind::ConnectionRefused => Probe::Refused,
                _ => Probe::Indeterminate,
            }
        }
        _ => Probe::Indeterminate,
    };
    recovery(location, probe)
}

/// Recover under core.lock. Probe outside the state lock so holder readers can
/// finish rows meanwhile, then decide against the current rows under that lock.
pub(crate) async fn recover(core: &Arc<Core>) {
    let records: Vec<_> = {
        let st = core.lock();
        if !st.recovering_endpoints {
            return;
        }
        st.endpoints.iter().map(|(&id, endpoint)| (id, endpoint.record.clone())).collect()
    };
    let mut observed = Vec::with_capacity(records.len());
    for (id, record) in records {
        let valid = record.path.to_str().is_some()
            && fits_client(&record.path)
            && platform::valid_terminal_endpoint(&core.paths.socket, &record.path);
        let live = core.lock().reg.terminals.get(&id).is_some_and(Terminal::is_live);
        let action = if valid && live && platform::verify_terminal_endpoint(&record.path, true).is_ok() {
            observe(&record).await
        } else {
            Recovery::Lost
        };
        observed.push((id, record, valid, action));
    }

    {
        let mut st = core.lock();
        st.recovering_endpoints = false;
        for (id, record, valid, action) in observed {
            if !st.reg.terminals.get(&id).is_some_and(Terminal::is_live) {
                if valid {
                    let _ = platform::cleanup_terminal_endpoint(&record);
                }
                st.endpoints.remove(&id);
                if let Some(row) = st.reg.terminals.get_mut(&id) {
                    row.endpoint = None;
                }
                continue;
            }
            let rebind = match action {
                Recovery::Bind => platform::verify_terminal_endpoint(&record.path, false).is_ok(),
                Recovery::Reclaim => platform::cleanup_terminal_endpoint(&record),
                Recovery::Lost => false,
            };
            if rebind && let Ok((listener, record)) = platform::bind_terminal_endpoint(&record.path) {
                install(&mut st, id, listener, record);
            } else {
                st.endpoints.remove(&id);
                st.reg.terminals.get_mut(&id).expect("live endpoint row").bound = Some(Binding::Lost);
                eprintln!("core: endpoint for {} could not be recovered", address(id));
            }
        }
        core.save(&st);
    }
    if let Err(e) = core.sync_saved().await {
        eprintln!("core: cannot save recovered endpoints: {e}");
    }
    let st = core.lock();
    for (&id, endpoint) in &st.endpoints {
        endpoint.start(core, id);
    }
}

pub(crate) fn bind_params(method: &str, mut params: Value, bound: &Bound) -> Result<Value, RpcError> {
    if bound.retired() {
        return Err(RpcError::new("endpoint_retired", "this terminal's endpoint has retired", ""));
    }
    if params.is_null() {
        params = json!({});
    }
    let fields = params
        .as_object_mut()
        .ok_or_else(|| RpcError::invalid_params("params must be an object on a bound endpoint"))?;
    if method == "hello" && fields.contains_key("core") {
        return Err(RpcError::invalid_params("a peer hello cannot use a bound endpoint"));
    }
    if method == "$/orphaned" {
        return Err(RpcError::invalid_params("orphan notifications cannot use a bound endpoint"));
    }
    if CALLER_METHODS.contains(&method) {
        fields.insert("caller".into(), bound.terminal().into());
    }
    Ok(params)
}

pub(crate) fn check_run(bind: bool, on: bool, remote: bool) -> Result<(), RpcError> {
    if bind && on {
        return Err(RpcError::invalid_params("bind cannot be used with on"));
    }
    if bind && remote {
        return Err(RpcError::new("refused", "a forwarded run cannot bind a terminal", "run on the terminal's core"));
    }
    Ok(())
}

pub(crate) fn check_run_request(method: &str, params: &Value, remote: bool) -> Result<(), RpcError> {
    if method != "terminal.run" {
        return Ok(());
    }
    if remote
        && params.is_array()
        && let Ok(run) = crate::rpc::parse_params::<crate::api::RunParams>(params.clone())
    {
        return check_run(run.bind, run.on.is_some(), true);
    }
    check_run(params["bind"] == true, !params["on"].is_null(), remote)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_methods_match_the_schema() {
        let doc = crate::api::openrpc();
        let mut methods: Vec<_> = doc["methods"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|method| method["params"].as_array().unwrap().iter().any(|param| param["name"] == "caller"))
            .map(|method| method["name"].as_str().unwrap())
            .collect();
        methods.sort_unstable();
        let mut callers = CALLER_METHODS.to_vec();
        callers.sort_unstable();
        assert_eq!(callers, methods);
    }

    #[test]
    fn recovery_only_reclaims_its_own_refused_socket() {
        for location in [Location::Absent, Location::OwnSocket, Location::Occupied] {
            for probe in [Probe::Refused, Probe::Live, Probe::Indeterminate] {
                let expected = match (location, probe) {
                    (Location::Absent, _) => Recovery::Bind,
                    (Location::OwnSocket, Probe::Refused) => Recovery::Reclaim,
                    _ => Recovery::Lost,
                };
                assert_eq!(recovery(location, probe), expected);
            }
        }
    }
}
