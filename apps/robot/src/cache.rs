//! Durable robot cache and its single ordered writer.
//!
//! The session loop never touches the disk. It hands snapshots to one writer
//! task, which coalesces cursor checkpoints to at most one write per interval,
//! writes urgent checkpoints (terminal turn events) right away, and confirms
//! `flush` only after the data is on disk. Each write is temp file + fsync +
//! rename + parent directory fsync, so a crash leaves the old or new file,
//! never a partial one.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    time::Instant,
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cache {
    pub bot_id: String,
    /// Highest applied conversation event sequence (at least once: events after
    /// it may be applied again and are deduplicated).
    pub cursor: i64,
    #[serde(default)]
    pub claim_url: String,
    #[serde(default)]
    pub claim_expires: i64,
    /// An utterance that must be submitted, recorded before any network I/O.
    #[serde(default)]
    pub pending: Option<Pending>,
    /// A turn the user interrupted that must be cancelled before submitting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<String>,
    /// The last turn this robot started and has not yet seen finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub request_id: String,
    pub text: String,
}

/// Read the cache, or start empty when it is missing or unreadable. Also
/// removes temp files left by an interrupted write.
pub fn load(path: &Path) -> Cache {
    remove_stale_temps(path);
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn temp_prefix(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    Some(format!("{stem}."))
}

fn remove_stale_temps(path: &Path) {
    let (Some(dir), Some(prefix)) = (path.parent(), temp_prefix(path)) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Atomically replace `path` with `bytes`.
pub fn persist(path: &Path, bytes: &[u8]) -> io::Result<()> {
    persist_with(path, bytes, Write::write_all)
}

fn persist_with(
    path: &Path,
    bytes: &[u8],
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.tmp", robot_buddy::auth::random()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        write(&mut file, bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    std::fs::rename(&temp, path)?;
    // The rename is durable only once the directory entry is on disk.
    std::fs::File::open(parent.unwrap_or(Path::new(".")))?.sync_all()
}

pub(crate) type WriteFn = Arc<dyn Fn(&[u8]) -> io::Result<()> + Send + Sync>;

struct Snapshot {
    version: u64,
    bytes: Vec<u8>,
    urgent: bool,
}

struct Shared {
    latest: Mutex<Option<Snapshot>>,
    wake: Notify,
}

enum Message {
    Flush(Snapshot, oneshot::Sender<io::Result<()>>),
    Shutdown(oneshot::Sender<()>),
}

/// Handle to the writer task. Cheap to call from the session loop.
pub struct CacheStore {
    shared: Arc<Shared>,
    messages: mpsc::Sender<Message>,
    version: std::sync::atomic::AtomicU64,
}

impl CacheStore {
    pub fn spawn(path: PathBuf, interval: Duration) -> Self {
        Self::spawn_with(Arc::new(move |bytes| persist(&path, bytes)), interval)
    }

    pub(crate) fn spawn_with(write: WriteFn, interval: Duration) -> Self {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            wake: Notify::new(),
        });
        let (messages, inbox) = mpsc::channel(8);
        tokio::spawn(writer(shared.clone(), inbox, write, interval));
        Self {
            shared,
            messages,
            version: std::sync::atomic::AtomicU64::default(),
        }
    }

    fn snapshot(&self, cache: &Cache, urgent: bool) -> Option<Snapshot> {
        let version = 1 + self
            .version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match serde_json::to_vec(cache) {
            Ok(bytes) => Some(Snapshot {
                version,
                bytes,
                urgent,
            }),
            Err(error) => {
                tracing::error!(%error, "cache encoding failed");
                None
            }
        }
    }

    /// Record a checkpoint. Written within the coalescing interval, or right
    /// away when `urgent`. Never blocks.
    pub fn checkpoint(&self, cache: &Cache, urgent: bool) {
        let Some(snapshot) = self.snapshot(cache, urgent) else {
            return;
        };
        let mut latest = self
            .shared
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let urgent = urgent || latest.as_ref().is_some_and(|s| s.urgent);
        *latest = Some(Snapshot { urgent, ..snapshot });
        drop(latest);
        self.shared.wake.notify_one();
    }

    /// Write `cache` now and wait until it is durable.
    pub async fn flush(&self, cache: &Cache) -> Result<()> {
        let snapshot = self.snapshot(cache, true).context("cache encoding")?;
        let (done, wait) = oneshot::channel();
        self.messages
            .send(Message::Flush(snapshot, done))
            .await
            .ok()
            .context("cache writer stopped")?;
        wait.await.context("cache writer stopped")??;
        Ok(())
    }

    /// Write any outstanding checkpoint and stop the writer.
    pub async fn shutdown(&self) {
        let (done, wait) = oneshot::channel();
        if self.messages.send(Message::Shutdown(done)).await.is_ok() {
            let _ = wait.await;
        }
    }
}

async fn writer(
    shared: Arc<Shared>,
    mut inbox: mpsc::Receiver<Message>,
    write: WriteFn,
    interval: Duration,
) {
    let mut written = 0u64;
    let mut last_write: Option<Instant> = None;
    let mut due: Option<Instant> = None;
    loop {
        tokio::select! {
            message = inbox.recv() => match message {
                Some(Message::Flush(snapshot, done)) => {
                    let result = store(&write, &snapshot, &mut written).await;
                    last_write = Some(Instant::now());
                    let _ = done.send(result);
                }
                Some(Message::Shutdown(done)) => {
                    write_latest(&shared, &write, &mut written).await;
                    let _ = done.send(());
                    return;
                }
                None => {
                    write_latest(&shared, &write, &mut written).await;
                    return;
                }
            },
            () = shared.wake.notified() => {
                let urgent = shared.latest.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref().is_some_and(|s| s.urgent);
                let earliest = last_write.map_or_else(Instant::now, |t| t + interval);
                let next = if urgent { Instant::now() } else { earliest.max(Instant::now()) };
                due = Some(due.map_or(next, |d| d.min(next)));
            },
            () = sleep_until(due), if due.is_some() => {
                due = None;
                if write_latest(&shared, &write, &mut written).await {
                    last_write = Some(Instant::now());
                }
            },
        }
    }
}

async fn sleep_until(due: Option<Instant>) {
    if let Some(due) = due {
        tokio::time::sleep_until(due).await;
    }
}

async fn write_latest(shared: &Shared, write: &WriteFn, written: &mut u64) -> bool {
    let snapshot = shared
        .latest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    match snapshot {
        Some(snapshot) => {
            if let Err(error) = store(write, &snapshot, written).await {
                tracing::warn!(%error, "cache checkpoint failed");
            }
            true
        }
        None => false,
    }
}

/// Write one snapshot unless a newer one is already on disk.
async fn store(write: &WriteFn, snapshot: &Snapshot, written: &mut u64) -> io::Result<()> {
    if snapshot.version <= *written {
        return Ok(());
    }
    let write = write.clone();
    let bytes = snapshot.bytes.clone();
    tokio::task::spawn_blocking(move || write(&bytes))
        .await
        .map_err(io::Error::other)??;
    *written = snapshot.version;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("buddy-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn counting() -> (WriteFn, Arc<AtomicUsize>, Arc<Mutex<Vec<Cache>>>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (c, s) = (count.clone(), seen.clone());
        let write: WriteFn = Arc::new(move |bytes| {
            c.fetch_add(1, Ordering::SeqCst);
            s.lock()
                .unwrap()
                .push(serde_json::from_slice(bytes).unwrap());
            Ok(())
        });
        (write, count, seen)
    }

    fn at(cursor: i64) -> Cache {
        Cache {
            cursor,
            ..Cache::default()
        }
    }

    #[tokio::test]
    async fn coalesces_a_burst_of_checkpoints_into_few_fsyncs() {
        let (write, count, seen) = counting();
        let store = CacheStore::spawn_with(write, Duration::from_millis(200));
        for cursor in 1..=1000 {
            store.checkpoint(&at(cursor), false);
            if cursor % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }
        tokio::time::sleep(Duration::from_millis(450)).await;
        let writes = count.load(Ordering::SeqCst);
        assert!((1..=3).contains(&writes), "{writes} writes for 1000 events");
        assert_eq!(seen.lock().unwrap().last().unwrap().cursor, 1000);
    }

    #[tokio::test]
    async fn terminal_checkpoint_is_written_without_waiting_for_the_interval() {
        let (write, count, seen) = counting();
        let store = CacheStore::spawn_with(write, Duration::from_secs(30));
        store.checkpoint(&at(1), false); // first write after a quiet period
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.checkpoint(&at(2), false); // coalesced: due in 30 s
        store.checkpoint(&at(3), true); // terminal event: due now
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(seen.lock().unwrap().last().unwrap().cursor, 3);
    }

    #[tokio::test]
    async fn flush_is_durable_before_returning_and_never_rewinds() {
        let (write, count, seen) = counting();
        let store = CacheStore::spawn_with(write, Duration::from_secs(30));
        store.checkpoint(&at(1), false);
        let mut pending = at(2);
        pending.pending = Some(Pending {
            request_id: "r".into(),
            text: "hi".into(),
        });
        store.flush(&pending).await.unwrap();
        assert_eq!(seen.lock().unwrap().last(), Some(&pending));
        // The older coalesced checkpoint must not overwrite the flushed state.
        store.shutdown().await;
        assert_eq!(seen.lock().unwrap().last(), Some(&pending));
        assert!(count.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn shutdown_writes_the_outstanding_checkpoint() {
        let (write, count, seen) = counting();
        let store = CacheStore::spawn_with(write, Duration::from_secs(30));
        store.checkpoint(&at(1), false);
        tokio::time::sleep(Duration::from_millis(20)).await;
        store.checkpoint(&at(7), false);
        store.shutdown().await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(seen.lock().unwrap().last().unwrap().cursor, 7);
    }

    #[test]
    fn interrupted_write_never_replaces_the_cache_with_partial_content() {
        let dir = temp_dir();
        let path = dir.join("cache.json");
        let good = serde_json::to_vec(&at(41)).unwrap();
        persist(&path, &good).unwrap();
        let next = serde_json::to_vec(&at(42)).unwrap();
        let failed = persist_with(&path, &next, |file, bytes| {
            file.write_all(&bytes[..bytes.len() / 2])?;
            Err(io::Error::other("power cut"))
        });
        assert!(failed.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), good);
        assert_eq!(load(&path).cursor, 41);
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1, "temp file removed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_tolerates_corruption_and_removes_stale_temps() {
        let dir = temp_dir();
        let path = dir.join("cache.json");
        std::fs::write(&path, b"{not json").unwrap();
        std::fs::write(dir.join("cache.0123.tmp"), b"{\"bot_id\":").unwrap();
        std::fs::write(dir.join("other.tmp"), b"keep").unwrap();
        assert_eq!(load(&path), Cache::default());
        assert!(!dir.join("cache.0123.tmp").exists());
        assert!(dir.join("other.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_caches_written_before_cancel_and_active_turn_existed() {
        let old = br#"{"bot_id":"b","cursor":9,"pending":{"request_id":"r","text":"t"}}"#;
        let cache: Cache = serde_json::from_slice(old).unwrap();
        assert_eq!(cache.cursor, 9);
        assert!(cache.cancel.is_none() && cache.active_turn.is_none());
        assert_eq!(cache.pending.unwrap().text, "t");
    }

    #[tokio::test]
    async fn store_writes_real_files() {
        let dir = temp_dir();
        let path = dir.join("cache.json");
        let store = CacheStore::spawn(path.clone(), Duration::from_millis(10));
        store.flush(&at(5)).await.unwrap();
        assert_eq!(load(&path).cursor, 5);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
