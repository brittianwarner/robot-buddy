//! Speech sequencing: committed sentences in, one ordered PCM stream out.
//!
//! One spawned task per stream owns the lease, an absolute deadline, the authorization
//! guard and at most two upstream syntheses: the sentence playing and the next one.
//! Audio leaves through a bounded channel, so a slow or vanished listener never makes
//! the task buffer without limit, and the task stops even if the body is never polled.
//!
//! Every actor call re-authorizes the session. A successful chunk read therefore counts
//! as an authorization check: the guard only calls when no read has proved access for
//! `check_every`, and it starts once chunk 0 has been read.
use super::admission::Lease;
use anyhow::{Context, Result, anyhow, bail, ensure};
use axum::body::Bytes;
use futures_util::{
    Stream, StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use robot_buddy::audio::PcmFormat;
use serde_json::Value;
use std::{collections::VecDeque, future::Future, sync::Arc, time::Duration};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::Instant,
};

/// One committed sentence and the voice pinned for its turn.
pub struct Spoken {
    pub text: String,
    pub voice: Value,
}

pub enum Chunk {
    Ready(Spoken),
    Pending,
    Done,
}

/// Committed speech for one turn.
pub trait ChunkSource: Send + Sync + 'static {
    /// Looks up chunk `index`. An error means the turn stopped or access was revoked.
    /// `Ready` and `Pending` answers come from an authorized actor call.
    fn chunk(&self, index: u32) -> impl Future<Output = Result<Chunk>> + Send;
    /// Re-authorizes the session and confirms the turn may still be spoken. Reads turn
    /// status only.
    fn check(&self) -> impl Future<Output = Result<()>> + Send;
    /// Resolves when a new chunk may have been committed. The pipeline also polls.
    fn changed(&self) -> impl Future<Output = ()> + Send;
}

pub struct Synthesis {
    pub format: PcmFormat,
    pub audio: BoxStream<'static, Result<Bytes>>,
}

/// One paid text-to-speech request per sentence. Never retried automatically.
pub trait Synthesizer: Send + Sync + 'static {
    /// The turn's pinned voice, decoded once per stream.
    type Voice: Send + Sync;
    fn voice(&self, pinned: &Value) -> Result<Self::Voice>;
    fn synthesize(
        &self,
        voice: &Self::Voice,
        text: &str,
    ) -> impl Future<Output = Result<Synthesis>> + Send;
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Absolute stream deadline, independent of how the body is polled.
    pub deadline: Duration,
    pub max_chunks: u32,
    pub audio_budget: usize,
    /// PCM buffered for the next sentence while the current one plays.
    pub prefetch_budget: usize,
    /// Largest piece sent through the channel.
    pub piece_bytes: usize,
    pub queue_depth: usize,
    /// Fallback poll for a pending chunk when no event arrives.
    pub poll: Duration,
    pub check_every: Duration,
    /// Forwarding continues this long after a successful check was issued.
    pub authorized_for: Duration,
    pub check_timeout: Duration,
    pub actor_timeout: Duration,
    /// Start the next sentence while the current one streams. Off restores the
    /// sequential behavior; kept for the gap eval.
    pub lookahead: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            deadline: Duration::from_mins(5),
            max_chunks: 64,
            audio_budget: 16 * 1024 * 1024,
            prefetch_budget: 2 * 1024 * 1024,
            piece_bytes: 16 * 1024,
            queue_depth: 8,
            poll: Duration::from_millis(250),
            check_every: Duration::from_millis(500),
            authorized_for: Duration::from_secs(2),
            check_timeout: Duration::from_secs(5),
            actor_timeout: Duration::from_secs(10),
            lookahead: true,
        }
    }
}

/// Response metadata, known once the first sentence's synthesis has started.
#[derive(Debug)]
pub struct Head {
    pub format: PcmFormat,
    pub voice_id: String,
    pub revision: i64,
}

impl Head {
    fn new(spoken: &Spoken, format: PcmFormat) -> Result<Self> {
        Ok(Self {
            format,
            voice_id: spoken.voice["voice_id"]
                .as_str()
                .context("voice missing")?
                .into(),
            revision: spoken.voice["revision"]
                .as_i64()
                .context("voice revision missing")?,
        })
    }
}

pub struct Job<S, T> {
    pub source: S,
    pub synth: Arc<T>,
    pub upstream: Arc<Semaphore>,
    pub lease: Lease,
    pub limits: Limits,
}

/// Receiving half of the stream. A stopped stream ends with an error, never a clean
/// end of body, so clients cannot mistake a cut stream for a finished one.
///
/// A stop wins over queued audio: once the task fails (replaced, revoked, cut off) the
/// next item is the error and queued PCM is discarded. A finished stream drains.
pub struct AudioBody {
    rx: mpsc::Receiver<Bytes>,
    outcome: oneshot::Receiver<Result<()>>,
}

impl AudioBody {
    pub fn into_stream(self) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
        stream::unfold(Some((self, false)), |state| async move {
            let (mut body, finished) = state?;
            if finished {
                // The task ended cleanly; what is queued is the end of the answer.
                return body
                    .rx
                    .recv()
                    .await
                    .map(|bytes| (Ok(bytes), Some((body, true))));
            }
            let outcome = tokio::select! {
                biased;
                outcome = &mut body.outcome => outcome,
                bytes = body.rx.recv() => match bytes {
                    Some(bytes) => return Some((Ok(bytes), Some((body, false)))),
                    None => (&mut body.outcome).await,
                },
            };
            match outcome {
                Ok(Ok(())) => body
                    .rx
                    .recv()
                    .await
                    .map(|bytes| (Ok(bytes), Some((body, true)))),
                Ok(Err(error)) => Some((Err(std::io::Error::other(error.to_string())), None)),
                Err(_) => Some((Err(std::io::Error::other("speech stream stopped")), None)),
            }
        })
    }
}

/// Spawns the stream task. The head resolves once the first sentence is synthesizing.
pub fn spawn<S: ChunkSource, T: Synthesizer>(
    job: Job<S, T>,
) -> (oneshot::Receiver<Result<Head>>, AudioBody) {
    let (tx, rx) = mpsc::channel(job.limits.queue_depth);
    let (head_tx, head_rx) = oneshot::channel();
    let (outcome_tx, outcome) = oneshot::channel();
    tokio::spawn(async move {
        let mut head = Some(head_tx);
        let result = run(&job, &tx, &mut head).await;
        // Release the lease, upstream requests and actor subscription before reporting.
        drop(job);
        match (result, head) {
            (Err(error), Some(head)) => {
                let _ = head.send(Err(error));
            }
            (result, _) => {
                let _ = outcome_tx.send(result);
            }
        }
    });
    (head_rx, AudioBody { rx, outcome })
}

type HeadSender = oneshot::Sender<Result<Head>>;

async fn run<S: ChunkSource, T: Synthesizer>(
    job: &Job<S, T>,
    tx: &mpsc::Sender<Bytes>,
    head: &mut Option<HeadSender>,
) -> Result<()> {
    // When an authorized call last went out. Nothing is forwarded before chunk 0's read.
    let authorized = watch::channel(None).0;
    let ctx = Ctx {
        source: &job.source,
        synth: &*job.synth,
        upstream: &job.upstream,
        authorized: &authorized,
        limits: job.limits,
    };
    let deadline = Instant::now() + job.limits.deadline;
    tokio::select! {
        biased;
        () = job.lease.stopped() => bail!("speech stream replaced"),
        () = tx.closed() => bail!("listener disconnected"),
        () = tokio::time::sleep_until(deadline) => bail!("speech deadline exceeded"),
        result = ctx.guard() => result,
        result = ctx.speak(tx, head) => result,
    }
}

struct Ctx<'a, S, T> {
    source: &'a S,
    synth: &'a T,
    upstream: &'a Arc<Semaphore>,
    /// Issue time of the latest actor call that proved access.
    authorized: &'a watch::Sender<Option<Instant>>,
    limits: Limits,
}

impl<S, T> Clone for Ctx<'_, S, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S, T> Copy for Ctx<'_, S, T> {}

/// The turn's voice, decoded by the first sentence and reused by the rest.
type Voice<T> = Arc<<T as Synthesizer>::Voice>;

/// A synthesis in flight, holding one global upstream permit.
struct Upstream {
    format: PcmFormat,
    audio: BoxStream<'static, Result<Bytes>>,
    _permit: OwnedSemaphorePermit,
}

impl<S: ChunkSource, T: Synthesizer> Ctx<'_, S, T> {
    /// Records an authorized call issued at `issued`. Older proofs never win.
    fn authorized_at(self, issued: Instant) {
        self.authorized.send_if_modified(|last| {
            let newer = last.is_none_or(|at| issued > at);
            if newer {
                *last = Some(issued);
            }
            newer
        });
    }

    /// Re-checks authorization off the audio path once no read has done so for
    /// `check_every`. Starts after chunk 0's read. A failed or timed-out check stops
    /// the stream.
    async fn guard(self) -> Result<()> {
        let mut seen = self.authorized.subscribe();
        loop {
            let last = *seen.borrow_and_update();
            let due = last.map(|at| at + self.limits.check_every);
            if due.is_none_or(|due| Instant::now() < due) {
                let wait = due.map_or_else(
                    || futures_util::future::Either::Left(std::future::pending()),
                    |due| futures_util::future::Either::Right(tokio::time::sleep_until(due)),
                );
                tokio::select! {
                    () = wait => {}
                    changed = seen.changed() => {
                        changed.map_err(|_| anyhow!("speech authorization stopped"))?;
                    }
                }
                continue;
            }
            let issued = Instant::now();
            tokio::time::timeout(self.limits.check_timeout, self.source.check())
                .await
                .context("speech authorization timed out")?
                .context("speech authorization stopped")?;
            self.authorized_at(issued);
        }
    }

    async fn speak(self, tx: &mpsc::Sender<Bytes>, head: &mut Option<HeadSender>) -> Result<()> {
        let spoken = self.wait_chunk(0).await?.context("no spoken chunks")?;
        let voice = Arc::new(self.synth.voice(&spoken.voice)?);
        let first = self.open(&voice, &spoken).await?;
        let value = Head::new(&spoken, first.format)?;
        head.take()
            .context("speech head already sent")?
            .send(Ok(value))
            .map_err(|_| anyhow!("listener disconnected"))?;
        self.forward(tx, first, voice).await
    }

    /// Waits for chunk `index`; `None` when the turn has no more speech.
    async fn wait_chunk(self, index: u32) -> Result<Option<Spoken>> {
        if index >= self.limits.max_chunks {
            return Ok(None);
        }
        loop {
            let issued = Instant::now();
            let chunk = tokio::time::timeout(self.limits.actor_timeout, self.source.chunk(index))
                .await
                .context("speech actor deadline")??;
            match chunk {
                Chunk::Ready(spoken) => {
                    self.authorized_at(issued);
                    return Ok(Some(spoken));
                }
                Chunk::Done => return Ok(None),
                Chunk::Pending => {
                    self.authorized_at(issued);
                    tokio::select! {
                        () = self.source.changed() => {}
                        () = tokio::time::sleep(self.limits.poll) => {}
                    }
                }
            }
        }
    }

    async fn open(self, voice: &T::Voice, spoken: &Spoken) -> Result<Upstream> {
        let permit = self.upstream.clone().acquire_owned().await?;
        let synthesis = self.synth.synthesize(voice, &spoken.text).await?;
        Ok(Upstream {
            format: synthesis.format,
            audio: synthesis.audio,
            _permit: permit,
        })
    }

    /// Finds and starts the next sentence. The outer error (lookup) stops the stream
    /// at once; the inner error (synthesis) waits until the current sentence ends.
    async fn look(self, voice: Voice<T>, index: u32) -> Result<Option<Result<Upstream>>> {
        let Some(spoken) = self.wait_chunk(index).await? else {
            return Ok(None);
        };
        Ok(Some(self.open(&voice, &spoken).await))
    }

    /// Whether audio may be forwarded now: within `authorized_for` of the last proof.
    fn authorized_now(self) -> bool {
        self.authorized
            .borrow()
            .is_some_and(|at| Instant::now() < at + self.limits.authorized_for)
    }

    fn count(self, total: usize, bytes: usize) -> Result<usize> {
        let total = total + bytes;
        ensure!(total <= self.limits.audio_budget, "audio exceeds budget");
        Ok(total)
    }

    async fn forward(
        self,
        tx: &mpsc::Sender<Bytes>,
        first: Upstream,
        voice: Voice<T>,
    ) -> Result<()> {
        let mut authorization = self.authorized.subscribe();
        let format = first.format;
        let (piece, budget) = (self.limits.piece_bytes, self.limits.prefetch_budget);
        let mut total = 0;
        let mut index = 0;
        let mut playing = Playing::new(first);
        let mut ahead = Ahead::Idle;
        loop {
            if playing.finished() {
                playing.ensure_complete(format)?;
                match std::mem::replace(&mut ahead, Ahead::Idle) {
                    Ahead::Done => return Ok(()),
                    Ahead::Failed(error) => return Err(error),
                    Ahead::Ready(next) => {
                        ensure!(
                            next.upstream.format == format,
                            "speech format changed between chunks"
                        );
                        playing = next.into_playing();
                        index += 1;
                        continue;
                    }
                    waiting => ahead = waiting,
                }
            }
            if matches!(ahead, Ahead::Idle) && (self.limits.lookahead || playing.eof) {
                ahead = Ahead::Looking(Box::pin(self.look(voice.clone(), index + 1)));
            }
            authorization.mark_unchanged();
            let authorized = self.authorized_now();
            let queued = !playing.outbox.is_empty();
            tokio::select! {
                biased;
                permit = tx.reserve(), if queued && authorized => {
                    let permit = permit.map_err(|_| anyhow!("listener disconnected"))?;
                    // The client may have been slow; authorization can lapse meanwhile.
                    if self.authorized_now()
                        && let Some(bytes) = playing.outbox.pop_front()
                    {
                        permit.send(bytes);
                    }
                }
                changed = authorization.changed(), if queued && !authorized => {
                    changed.map_err(|_| anyhow!("speech authorization stopped"))?;
                }
                item = playing.upstream.audio.next(), if !queued && !playing.eof => {
                    match item {
                        Some(bytes) => {
                            let bytes = bytes?;
                            total = self.count(total, bytes.len())?;
                            playing.bytes += bytes.len();
                            enqueue(&mut playing.outbox, bytes, piece);
                        }
                        None => playing.eof = true,
                    }
                }
                received = ahead.advance(piece), if ahead.wants_poll(budget) => {
                    total = self.count(total, received?)?;
                }
                else => bail!("speech pipeline stalled"),
            }
        }
    }
}

fn enqueue(queue: &mut VecDeque<Bytes>, mut bytes: Bytes, piece: usize) {
    while bytes.len() > piece {
        queue.push_back(bytes.split_to(piece));
    }
    if !bytes.is_empty() {
        queue.push_back(bytes);
    }
}

/// The sentence being forwarded.
struct Playing {
    upstream: Upstream,
    outbox: VecDeque<Bytes>,
    bytes: usize,
    eof: bool,
}

impl Playing {
    fn new(upstream: Upstream) -> Self {
        Self {
            upstream,
            outbox: VecDeque::new(),
            bytes: 0,
            eof: false,
        }
    }

    fn finished(&self) -> bool {
        self.eof && self.outbox.is_empty()
    }

    fn ensure_complete(&self, format: PcmFormat) -> Result<()> {
        let frame = usize::from(format.channels) * 2;
        ensure!(
            self.bytes > 0 && self.bytes.is_multiple_of(frame),
            "incomplete PCM chunk"
        );
        Ok(())
    }
}

/// The next sentence, buffered up to the prefetch budget.
struct Prefetch {
    upstream: Upstream,
    buffer: VecDeque<Bytes>,
    buffered: usize,
    eof: bool,
}

impl Prefetch {
    fn into_playing(self) -> Playing {
        Playing {
            upstream: self.upstream,
            outbox: self.buffer,
            bytes: self.buffered,
            eof: self.eof,
        }
    }
}

type Lookup<'a> = BoxFuture<'a, Result<Option<Result<Upstream>>>>;

enum Ahead<'a> {
    Idle,
    Looking(Lookup<'a>),
    Ready(Prefetch),
    Failed(anyhow::Error),
    Done,
}

impl Ahead<'_> {
    fn wants_poll(&self, budget: usize) -> bool {
        match self {
            Self::Looking(_) => true,
            Self::Ready(next) => !next.eof && next.buffered < budget,
            _ => false,
        }
    }

    /// Advances the lookup or buffers one piece of the next sentence. Cancel-safe.
    /// Returns the PCM bytes received.
    async fn advance(&mut self, piece: usize) -> Result<usize> {
        match self {
            Self::Looking(lookup) => {
                *self = match lookup.await? {
                    None => Self::Done,
                    Some(Ok(upstream)) => Self::Ready(Prefetch {
                        upstream,
                        buffer: VecDeque::new(),
                        buffered: 0,
                        eof: false,
                    }),
                    Some(Err(error)) => Self::Failed(error),
                };
                Ok(0)
            }
            Self::Ready(next) => match next.upstream.audio.next().await {
                Some(Ok(bytes)) => {
                    let received = bytes.len();
                    next.buffered += received;
                    enqueue(&mut next.buffer, bytes, piece);
                    Ok(received)
                }
                Some(Err(error)) => {
                    *self = Self::Failed(error);
                    Ok(0)
                }
                None => {
                    next.eof = true;
                    Ok(0)
                }
            },
            _ => std::future::pending().await,
        }
    }
}
