//! Ordered registry persistence, without disk IO on the core state lock.
use crate::paths::write_atomic;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::watch;

const FIRST_RETRY: Duration = Duration::from_millis(100);
const MAX_RETRY: Duration = Duration::from_secs(5);

pub(super) struct RegistryWriter {
    queue: Arc<(Mutex<Pending>, Condvar)>,
    completed: watch::Receiver<Completion>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Pending {
    latest: Option<(u64, Vec<u8>)>,
    submitted: u64,
    closed: bool,
}

#[derive(Default)]
struct Completion {
    saved: u64,
    failed: Option<(u64, String)>,
}

impl RegistryWriter {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        Self::start(move |bytes| write_atomic(&path, bytes))
    }

    pub(super) fn start(mut write: impl FnMut(&[u8]) -> io::Result<()> + Send + 'static) -> io::Result<Self> {
        let queue = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let (done, completed) = watch::channel(Completion::default());
        let work = queue.clone();
        let worker = std::thread::Builder::new().name("registry-writer".into()).spawn(move || {
            let (mutex, changed) = &*work;
            let mut pending = mutex.lock().unwrap();
            let mut retry = FIRST_RETRY;
            loop {
                if let Some((version, bytes)) = pending.latest.take() {
                    drop(pending);
                    let result = write(&bytes);
                    if let Err(e) = &result {
                        let _ = writeln!(io::stderr(), "core: cannot save terminals: {e}");
                    }
                    pending = mutex.lock().unwrap();
                    match result {
                        Ok(()) => {
                            done.send_replace(Completion { saved: version, failed: None });
                            retry = FIRST_RETRY;
                        }
                        Err(e) => {
                            done.send_modify(|done| done.failed = Some((version, e.to_string())));
                            if !pending.closed {
                                // A save submitted during the write already supersedes this one.
                                pending.latest.get_or_insert((version, bytes));
                                pending = changed.wait_timeout_while(pending, retry, |p| !p.closed).unwrap().0;
                                retry = (retry * 2).min(MAX_RETRY);
                            }
                        }
                    }
                } else if pending.closed {
                    break;
                } else {
                    pending = changed.wait(pending).unwrap();
                }
            }
        })?;
        Ok(Self { queue, completed, worker: Some(worker) })
    }

    /// Called under the core lock: snapshots enter the queue in state order.
    pub fn save(&self, bytes: Vec<u8>) {
        let (mutex, changed) = &*self.queue;
        let mut pending = mutex.lock().unwrap();
        pending.submitted += 1;
        pending.latest = Some((pending.submitted, bytes));
        changed.notify_one();
    }

    /// Wait for the current snapshot, not for unrelated future saves. Never hold the core lock.
    pub async fn flush(&self) -> io::Result<()> {
        let version = self.queue.0.lock().unwrap().submitted;
        let mut completed = self.completed.clone();
        let result = completed
            .wait_for(|done| done.saved >= version || done.failed.as_ref().is_some_and(|(v, _)| *v >= version))
            .await
            .map_err(|_| io::Error::other("registry writer stopped"))?;
        if result.saved >= version {
            Ok(())
        } else {
            Err(io::Error::other(result.failed.as_ref().expect("failed version").1.clone()))
        }
    }
}

impl Drop for RegistryWriter {
    fn drop(&mut self) {
        let (mutex, changed) = &*self.queue;
        mutex.lock().unwrap().closed = true;
        changed.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_slow_write_keeps_only_the_newest_pending_snapshot_and_drop_flushes_it() {
        let (started, writes) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let writer = RegistryWriter::start(move |bytes| {
            started.send(bytes.to_vec()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
        writer.save(vec![1]);
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [1]);
        writer.save(vec![2]);
        writer.save(vec![3]);
        let (finished, done) = mpsc::channel();
        let closing = std::thread::spawn(move || {
            drop(writer);
            finished.send(()).unwrap();
        });
        assert!(done.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [3]);
        assert!(done.try_recv().is_err());
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        closing.join().unwrap();
        assert!(writes.try_recv().is_err());
    }

    #[tokio::test]
    async fn flush_waits_for_the_atomic_replacement() {
        let root = std::env::temp_dir().join(format!("hc-writer-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("terminals.json");
        let writer = RegistryWriter::new(path.clone()).unwrap();
        writer.save(b"first".to_vec());
        writer.save(b"last".to_vec());
        writer.flush().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"last");
        assert!(!path.with_extension("tmp").exists());
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_flush_does_not_wait_for_later_snapshots() {
        let (started, writes) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let writer = RegistryWriter::start(move |bytes| {
            started.send(bytes.to_vec()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
        writer.save(vec![1]);
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [1]);
        {
            let waiting = writer.flush();
            tokio::pin!(waiting);
            assert!(tokio::time::timeout(Duration::from_millis(10), &mut waiting).await.is_err());
            writer.save(vec![2]);
            release.send(()).unwrap();
            assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [2]);
            tokio::time::timeout(Duration::from_secs(1), &mut waiting).await.unwrap().unwrap();
        }
        release.send(()).unwrap();
        drop(writer);
    }

    #[tokio::test]
    async fn a_failed_flush_reports_the_error_and_retries_the_newest_snapshot() {
        let (started, writes) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let writer = RegistryWriter::start(move |bytes| {
            started.send(bytes.to_vec()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap()
        })
        .unwrap();
        writer.save(vec![1]);
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [1]);
        let first = writer.flush();
        tokio::pin!(first);
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut first).await.is_err());
        writer.save(vec![2]);
        writer.save(vec![3]);
        release.send(Err(io::Error::other("disk failed"))).unwrap();
        assert_eq!(first.await.unwrap_err().to_string(), "disk failed");
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [3]);
        release.send(Ok(())).unwrap();
        writer.flush().await.unwrap();
    }
}
