//! Unix SSH-agent forwarding is opt-in and unsupported on Windows destinations.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub struct SshAgent;

impl SshAgent {
    pub fn bind(_socket: &Path, enabled: bool, _expiry: Duration) -> io::Result<Arc<Self>> {
        if enabled {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "ssh_agent_forwarding requires a Unix destination"));
        }
        Ok(Arc::new(Self))
    }

    pub fn enabled(&self) -> bool {
        false
    }
    pub fn environment(&self, _env: &mut BTreeMap<String, String>) {}
    pub fn start(self: &Arc<Self>) -> io::Result<()> {
        Ok(())
    }
    pub fn register(self: &Arc<Self>, _conn: &Arc<crate::wire::Conn>, _path: &str) -> io::Result<()> {
        Ok(())
    }
    pub fn unregister(&self, _conn: &Arc<crate::wire::Conn>) {}
    pub fn stop(&self) {}
}

#[test]
fn disabled_is_a_noop_but_enabling_is_explicitly_unsupported() {
    assert!(!SshAgent::bind(Path::new("unused"), false, Duration::from_secs(1)).unwrap().enabled());
    let error = SshAgent::bind(Path::new("unused"), true, Duration::from_secs(1)).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
}
