//! Speech admission: a process-wide stream cap and one live stream per bot.
//!
//! The newest request for a bot wins. It stops the older stream (for example one left
//! behind by barge-in whose TCP close the server has not seen yet) and takes its slot.
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// How long a replacing request waits for the stream it stopped to free capacity.
const HANDOVER: Duration = Duration::from_secs(1);

type Slots = Arc<Mutex<HashMap<String, Slot>>>;

struct Slot {
    generation: u64,
    stop: watch::Sender<bool>,
}

pub struct Admission {
    capacity: Arc<Semaphore>,
    slots: Slots,
    next: AtomicU64,
    #[cfg(test)]
    streams: usize,
}

/// Held by the pipeline task for the stream's whole life.
pub struct Lease {
    _slot: SlotGuard,
    stopped: watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
}

struct SlotGuard {
    bot: String,
    generation: u64,
    slots: Slots,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut slots = lock(&self.slots);
        if slots
            .get(&self.bot)
            .is_some_and(|slot| slot.generation == self.generation)
        {
            slots.remove(&self.bot);
        }
    }
}

fn lock(slots: &Slots) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
    slots
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Admission {
    pub fn new(streams: usize) -> Self {
        Self {
            capacity: Arc::new(Semaphore::new(streams)),
            slots: Arc::default(),
            next: AtomicU64::new(0),
            #[cfg(test)]
            streams,
        }
    }

    pub async fn lease(&self, bot: &str) -> Result<Lease> {
        let generation = self.next.fetch_add(1, Ordering::Relaxed);
        let (stop, stopped) = watch::channel(false);
        let replaced = self.claim(bot, generation, stop);
        let slot = SlotGuard {
            bot: bot.to_owned(),
            generation,
            slots: self.slots.clone(),
        };
        let permit = if replaced {
            tokio::time::timeout(HANDOVER, self.capacity.clone().acquire_owned())
                .await
                .context("speech capacity reached")??
        } else {
            self.capacity
                .clone()
                .try_acquire_owned()
                .context("speech capacity reached")?
        };
        Ok(Lease {
            _slot: slot,
            stopped,
            _permit: permit,
        })
    }

    /// Takes the bot's slot and stops its previous stream. Returns whether one existed.
    fn claim(&self, bot: &str, generation: u64, stop: watch::Sender<bool>) -> bool {
        let previous = lock(&self.slots).insert(bot.to_owned(), Slot { generation, stop });
        previous.map(|slot| slot.stop.send_replace(true)).is_some()
    }

    /// True when no stream holds a slot or capacity.
    #[cfg(test)]
    pub fn idle(&self) -> bool {
        lock(&self.slots).is_empty() && self.capacity.available_permits() == self.streams
    }
}

impl Lease {
    /// Resolves once a newer request for the same bot replaces this stream.
    pub async fn stopped(&self) {
        let mut stopped = self.stopped.clone();
        let _ = stopped.wait_for(|stop| *stop).await;
    }
}
