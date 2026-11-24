//! Durable robot cache and its single ordered writer.
//!
//! The session loop never touches the disk. It hands snapshots to one writer
//! task, which delays an ordinary cursor checkpoint by the coalescing interval
//! (so a burst of events costs one write), writes urgent checkpoints (terminal
//! turn events) right away, and confirms `flush` only after the data is on
//! disk. Each write is temp file + fsync + rename + parent directory fsync, so
//! a crash leaves the old or new file, never a partial one. The parent
//! directory is created once, when the store starts.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    time::Instant,
};

/// Default path for the robot cache when `BUDDY_CACHE_FILE` is unset.
pub const DEFAULT_CACHE_FILE: &str = "/var/lib/robot-buddy/cache.json";

/// `BUDDY_CACHE_FILE`, else [`DEFAULT_CACHE_FILE`].
pub fn path() -> PathBuf {
    std::env::var_os("BUDDY_CACHE_FILE")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_CACHE_FILE), PathBuf::from)
}

/// `robot-buddy-robot claim-url`: print the active claim URL from the cache,
/// or say why none is active. Nothing is logged.
pub fn print_claim_url() -> Result<()> {
    write_claim_url(
        &path(),
        robot_buddy::auth::now(),
        &mut std::io::stdout().lock(),
    )
}

fn write_claim_url(path: &Path, now: i64, out: &mut impl Write) -> Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            writeln!(out, "No active claim URL yet.")?;
            out.flush()?;
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    let cache: Cache = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a valid cache file", path.display()))?;
    if !cache.claim_url.is_empty() {
        if cache.claim_expires > 0 && cache.claim_expires <= now {
            writeln!(
                out,
                "Claim URL expired; waiting for the robot to start a new claim."
            )?;
        } else {
            writeln!(out, "{}", cache.claim_url)?;
        }
    } else if !cache.bot_id.is_empty() {
        writeln!(
            out,
            "No active claim URL (robot is already owned or waiting to start a claim)."
        )?;
    } else {
        writeln!(out, "No active claim URL yet.")?;
    }
    out.flush()?;
    Ok(())
}

/// Request IDs kept for cancellation at most. Each is an utterance replaced
/// while its submit's answer was unknown; more than a few means the server
/// has been unreachable through several interruptions, and the oldest go.
pub const MAX_CANCEL_REQUESTS: usize = 8;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// Request IDs of replaced utterances whose submit may have reached the
    /// server without an answer. Each is cancelled by request ID until the
    /// server acknowledges it. Oldest first, at most [`MAX_CANCEL_REQUESTS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cancel_requests: Vec<String>,
    /// The last turn this robot started and has not yet seen finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<String>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub request_id: String,
    pub text: String,
    /// Wall-clock creation time (Unix seconds). An utterance is only worth
    /// submitting for a short while; caches written before this field existed
    /// read as 0, which is always expired.
    #[serde(default)]
    pub created: i64,
}

/// Never prints the claim URL (it carries the claim token) or utterance text.
impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("bot_id", &self.bot_id)
            .field("cursor", &self.cursor)
            .field("claim_url", &redacted(&self.claim_url))
            .field("claim_expires", &self.claim_expires)
            .field("pending", &self.pending)
            .field("cancel", &self.cancel)
            .field("cancel_requests", &self.cancel_requests)
            .field("active_turn", &self.active_turn)
            .finish()
    }
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("request_id", &self.request_id)
            .field("text", &redacted(&self.text))
            .field("created", &self.created)
            .finish()
    }
}

fn redacted(value: &str) -> &'static str {
    if value.is_empty() { "" } else { "[redacted]" }
}

impl Cache {
    /// Remember `request_id` for cancellation, dropping the oldest beyond the bound.
    pub fn cancel_request(&mut self, request_id: String) {
        if self.cancel_requests.contains(&request_id) {
            return;
        }
        if self.cancel_requests.len() >= MAX_CANCEL_REQUESTS {
            self.cancel_requests.remove(0);
            tracing::info!("oldest request cancellation dropped");
        }
        self.cancel_requests.push(request_id);
    }

    /// Forget a request cancellation; whether it was kept.
    pub fn cancelled_request(&mut self, request_id: &str) -> bool {
        let before = self.cancel_requests.len();
        self.cancel_requests.retain(|id| id != request_id);
        self.cancel_requests.len() != before
    }
}

/// Read the cache, or start empty when it is missing or unreadable. Also
/// removes temp files left by an interrupted write. A file that exists but
/// cannot be read or parsed is reported: starting empty replays the whole
/// history (silently), so it should not go unnoticed.
pub fn load(path: &Path) -> Cache {
    remove_stale_temps(path);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Cache::default(),
        Err(error) => {
            tracing::warn!(%error, "cache file unreadable; starting from an empty cache");
            return Cache::default();
        }
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        // Never print the content: it can hold a pending utterance.
        tracing::warn!(%error, "cache file corrupt; starting from an empty cache");
        Cache::default()
    })
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

/// Atomically replace `path` with `bytes`. The parent directory must exist.
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
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(%error, "cache directory unavailable");
        }
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
        self.flush_later(cache).await
    }

    /// Snapshot `cache` now and return a future that writes it and resolves
    /// once it is durable. The future borrows nothing, so the session loop
    /// can keep serving input while the disk works.
    pub fn flush_later(&self, cache: &Cache) -> impl Future<Output = Result<()>> + 'static {
        let snapshot = self.snapshot(cache, true);
        let messages = self.messages.clone();
        async move {
            let snapshot = snapshot.context("cache encoding")?;
            let (done, wait) = oneshot::channel();
            messages
                .send(Message::Flush(snapshot, done))
                .await
                .ok()
                .context("cache writer stopped")?;
            wait.await.context("cache writer stopped")??;
            Ok(())
        }
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
    let mut due: Option<Instant> = None;
    loop {
        tokio::select! {
            message = inbox.recv() => match message {
                Some(Message::Flush(snapshot, done)) => {
                    let result = store(&write, &snapshot, &mut written).await;
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
                // An ordinary checkpoint waits a full interval from the first
                // change, so a turn's events share one write with its terminal
                // event. Urgent ones go now.
                let next = if urgent { Instant::now() } else { Instant::now() + interval };
                due = Some(due.map_or(next, |d| d.min(next)));
            },
            () = sleep_until(due), if due.is_some() => {
                due = None;
                write_latest(&shared, &write, &mut written).await;
            },
        }
    }
}

async fn sleep_until(due: Option<Instant>) {
    if let Some(due) = due {
        tokio::time::sleep_until(due).await;
    }
}

async fn write_latest(shared: &Shared, write: &WriteFn, written: &mut u64) {
    let snapshot = shared
        .latest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(snapshot) = snapshot
        && let Err(error) = store(write, &snapshot, written).await
    {
        tracing::warn!(%error, "cache checkpoint failed");
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

    #[tokio::test(start_paused = true)]
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
        assert!((1..=2).contains(&writes), "{writes} writes for 1000 events");
        assert_eq!(seen.lock().unwrap().last().unwrap().cursor, 1000);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_checkpoint_is_written_without_waiting_for_the_interval() {
        let (write, count, seen) = counting();
        let store = CacheStore::spawn_with(write, Duration::from_secs(30));
        store.checkpoint(&at(1), false); // ordinary: due in 30 s
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(count.load(Ordering::SeqCst), 0, "a turn's deltas wait");
        store.checkpoint(&at(2), false);
        store.checkpoint(&at(3), true); // terminal event: due now, carries the rest
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1, "one write per short turn");
        assert_eq!(seen.lock().unwrap().last().unwrap().cursor, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_checkpoints_are_written_after_the_interval() {
        let (write, count, seen) = counting();
        let store = CacheStore::spawn_with(write, Duration::from_secs(30));
        store.checkpoint(&at(1), false);
        tokio::time::sleep(Duration::from_secs(29)).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        store.checkpoint(&at(2), false);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(seen.lock().unwrap().last().unwrap().cursor, 2);
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
            created: 1,
        });
        store.flush(&pending).await.unwrap();
        assert_eq!(seen.lock().unwrap().last(), Some(&pending));
        // The older coalesced checkpoint must not overwrite the flushed state.
        store.shutdown().await;
        assert_eq!(seen.lock().unwrap().last(), Some(&pending));
        assert!(count.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_writes_the_outstanding_checkpoint() {
        let (write, count, seen) = counting();
        let store = CacheStore::spawn_with(write, Duration::from_secs(30));
        store.checkpoint(&at(1), false);
        tokio::time::sleep(Duration::from_millis(20)).await;
        store.checkpoint(&at(7), false);
        store.shutdown().await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "both coalesced into the shutdown write"
        );
        assert_eq!(seen.lock().unwrap().last().unwrap().cursor, 7);
    }

    #[test]
    fn debug_output_never_shows_the_claim_token_or_utterance_text() {
        let cache = Cache {
            claim_url: "https://buddy.example/claim?bot=b&token=SENTINEL-CLAIM".into(),
            pending: Some(Pending {
                request_id: "r".into(),
                text: "SENTINEL-PROMPT my question".into(),
                created: 1,
            }),
            ..Cache::default()
        };
        let shown = format!("{cache:?} {:#?}", cache.pending);
        assert!(!shown.contains("SENTINEL"), "{shown}");
        assert!(shown.contains("request_id: \"r\""));
    }

    #[test]
    fn request_cancellations_are_bounded_and_deduplicated() {
        let mut cache = Cache::default();
        for id in 0..MAX_CANCEL_REQUESTS + 3 {
            cache.cancel_request(format!("r{id}"));
        }
        cache.cancel_request(format!("r{}", MAX_CANCEL_REQUESTS + 2));
        assert_eq!(cache.cancel_requests.len(), MAX_CANCEL_REQUESTS);
        assert_eq!(cache.cancel_requests[0], "r3", "oldest dropped");
        assert!(cache.cancelled_request("r3"));
        assert!(!cache.cancelled_request("r3"));
        // Absent from the file until used, so old robots read new caches.
        let empty = serde_json::to_value(Cache::default()).unwrap();
        assert!(empty.get("cancel_requests").is_none());
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
        let pending = cache.pending.unwrap();
        assert_eq!(pending.text, "t");
        assert_eq!(pending.created, 0, "an undated utterance reads as expired");
    }

    #[tokio::test]
    async fn store_writes_real_files() {
        let dir = temp_dir();
        // The store creates the directory once at start, not on every write.
        let path = dir.join("state").join("cache.json");
        // The ordinary checkpoint below is not due before shutdown, so
        // nothing is mid-write while the test reads the file.
        let store = CacheStore::spawn(path.clone(), Duration::from_mins(1));
        store.flush(&at(5)).await.unwrap();
        assert_eq!(load(&path).cursor, 5);
        let later = store.flush_later(&at(6));
        store.checkpoint(&at(7), false);
        later.await.unwrap();
        assert_eq!(
            load(&path).cursor,
            6,
            "the snapshot is taken when the flush is requested"
        );
        // The later checkpoint is written before the store stops.
        store.shutdown().await;
        assert_eq!(load(&path).cursor, 7);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claim_url_helper_reports_active_expired_owned_and_missing_states() {
        let dir = temp_dir();
        let path = dir.join("cache.json");

        // Missing cache file.
        let mut out = Vec::new();
        write_claim_url(&path, 1_000, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "No active claim URL yet.\n"
        );

        // Active claim URL.
        let active = Cache {
            bot_id: "bot-1".into(),
            claim_url: "https://buddy.example/claim?bot=bot-1&token=tok".into(),
            claim_expires: 2_000,
            ..Cache::default()
        };
        persist(&path, &serde_json::to_vec(&active).unwrap()).unwrap();
        let mut out = Vec::new();
        write_claim_url(&path, 1_000, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "https://buddy.example/claim?bot=bot-1&token=tok\n"
        );

        // Expired claim URL.
        let mut out = Vec::new();
        write_claim_url(&path, 2_000, &mut out).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("Claim URL expired")
        );

        // Owned robot (bot_id present, claim_url cleared).
        let owned = Cache {
            bot_id: "bot-1".into(),
            ..Cache::default()
        };
        persist(&path, &serde_json::to_vec(&owned).unwrap()).unwrap();
        let mut out = Vec::new();
        write_claim_url(&path, 1_000, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("already owned"));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
