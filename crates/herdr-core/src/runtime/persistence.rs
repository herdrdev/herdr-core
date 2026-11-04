//! Ordered registry persistence, without disk IO on the core state lock.
use crate::paths::write_atomic;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use tokio::sync::watch;

pub(super) struct RegistryWriter {
    queue: Arc<(Mutex<Pending>, Condvar)>,
    completed: watch::Receiver<u64>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Pending {
    latest: Option<(u64, Vec<u8>)>,
    submitted: u64,
    closed: bool,
}

impl RegistryWriter {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        Self::start(move |bytes| write_atomic(&path, bytes))
    }

    pub(super) fn start(mut write: impl FnMut(&[u8]) -> io::Result<()> + Send + 'static) -> io::Result<Self> {
        let queue = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let (done, completed) = watch::channel(0);
        let work = queue.clone();
        let worker = std::thread::Builder::new().name("registry-writer".into()).spawn(move || {
            let (mutex, changed) = &*work;
            let mut pending = mutex.lock().unwrap();
            loop {
                if let Some((version, bytes)) = pending.latest.take() {
                    drop(pending);
                    if let Err(e) = write(&bytes) {
                        eprintln!("core: cannot save terminals: {e}");
                    }
                    done.send_replace(version);
                    pending = mutex.lock().unwrap();
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
        completed
            .wait_for(|written| *written >= version)
            .await
            .map_err(|_| io::Error::other("registry writer stopped"))?;
        Ok(())
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
    async fn a_flush_can_be_cancelled_and_a_failed_write_does_not_fail_the_barrier() {
        let (started, writes) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let writer = RegistryWriter::start(move |_| {
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Err(io::Error::other("disk failed"))
        })
        .unwrap();
        writer.save(vec![1]);
        writes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(10), writer.flush()).await.is_err());
        release.send(()).unwrap();
        writer.flush().await.unwrap();
        writer.flush().await.unwrap();
    }
}
