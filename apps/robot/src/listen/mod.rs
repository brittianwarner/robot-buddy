//! Hands-free local input. ALSA feeds one selected beam into bounded RMS endpointing;
//! whisper.cpp decodes completed utterances for both transcript wake and dialogue.
//! No temporary recordings, transcripts in logs, or direct network calls.
mod io;
mod policy;
mod vad;
mod wake;

use self::{
    io::{Alsa, Microphone, Recognizer},
    policy::Policy,
    vad::{Clip, Vad},
};
use crate::input::Control;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    future::pending,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

const DROP_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Published by the controller, including while playback outlives a completed turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub busy: bool,
    pub confirming: bool,
    pub muted: bool,
}

pub struct Config {
    model: PathBuf,
    whisper: OsString,
    device: OsString,
    channels: usize,
    channel: usize,
    barge_in: bool,
    wake: Option<wake::Models>,
}

impl Config {
    /// An unset/empty `BUDDY_LISTEN_MODEL` leaves the external control input alone.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_env_with(|key| std::env::var_os(key))
    }

    fn from_env_with(get: impl Fn(&str) -> Option<OsString>) -> Result<Option<Self>> {
        let Some(model) = get("BUDDY_LISTEN_MODEL").filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let number = |key, default: usize| -> Result<usize> {
            get(key).map_or(Ok(default), |s| {
                s.to_str()
                    .context("non-UTF8 mic setting")?
                    .parse()
                    .with_context(|| format!("invalid {key}"))
            })
        };
        let channels = number("BUDDY_LISTEN_CHANNELS", 1)?;
        let channel = number("BUDDY_LISTEN_CHANNEL", 0)?;
        let barge_env = get("BUDDY_LISTEN_BARGE_IN");
        let barge = match barge_env {
            Some(ref s) => s
                .to_str()
                .context("non-UTF8 mic setting")?
                .parse::<usize>()
                .context("invalid BUDDY_LISTEN_BARGE_IN")?,
            None => 1,
        };
        ensure!(
            (1..=8).contains(&channels) && channel < channels,
            "microphone needs 1..8 channels and a zero-based channel index"
        );
        ensure!(barge <= 1, "BUDDY_LISTEN_BARGE_IN must be 0 or 1");
        let wake = get("BUDDY_LISTEN_WAKE_DIR")
            .filter(|s| !s.is_empty())
            .map(|dir| {
                let level = |key, default| -> Result<f32> {
                    get(key).map_or(Ok(default), |v| {
                        Ok(v.to_str().context("invalid wake threshold")?.parse()?)
                    })
                };
                wake::Models::new(
                    std::path::Path::new(&dir),
                    level("BUDDY_LISTEN_WAKE_THRESHOLD", 0.5)?,
                    level("BUDDY_LISTEN_WAKE_BUSY_THRESHOLD", 0.8)?,
                )
            })
            .transpose()?;
        let model = PathBuf::from(model);
        ensure!(
            model.is_file(),
            "BUDDY_LISTEN_MODEL must name a local whisper.cpp model file"
        );
        let barge_in = mode_for(barge == 1, wake.is_some()).barge_in;
        if barge == 1 && wake.is_none() && barge_env.is_some() {
            tracing::info!(
                "barge-in disabled in transcript mode because local neural wake is not configured"
            );
        }
        Ok(Some(Self {
            model,
            whisper: get("BUDDY_LISTEN_WHISPER").unwrap_or_else(|| "whisper-cli".into()),
            device: get("BUDDY_LISTEN_DEVICE").unwrap_or_else(|| "default".into()),
            channels,
            channel,
            barge_in,
            wake,
        }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    acoustic: bool,
    barge_in: bool,
}

const fn mode_for(configured_barge_in: bool, wake_ready: bool) -> Mode {
    Mode {
        acoustic: wake_ready,
        barge_in: configured_barge_in && wake_ready,
    }
}

const fn capture_paused(state: State, barge_in: bool) -> bool {
    state.muted || (state.busy && !barge_in && !state.confirming)
}

const fn capture_must_restart(current: State, next: State, barge_in: bool) -> bool {
    next.muted || next.confirming != current.confirming || capture_paused(next, barge_in)
}

const fn detector_boundary(old: State, next: State) -> bool {
    old.muted || next.muted || old.confirming || next.confirming
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropKind {
    Clip,
    Wake,
}

impl DropKind {
    const COUNT: usize = 2;

    const fn index(self) -> usize {
        match self {
            Self::Clip => 0,
            Self::Wake => 1,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Clip => "clip",
            Self::Wake => "wake",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropReason {
    Overflow,
    Replaced,
    Stale,
    Echo,
    Muted,
    Confirming,
    Window,
}

impl DropReason {
    const COUNT: usize = 7;

    const fn index(self) -> usize {
        match self {
            Self::Overflow => 0,
            Self::Replaced => 1,
            Self::Stale => 2,
            Self::Echo => 3,
            Self::Muted => 4,
            Self::Confirming => 5,
            Self::Window => 6,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Overflow => "overflow",
            Self::Replaced => "replaced",
            Self::Stale => "stale",
            Self::Echo => "echo",
            Self::Muted => "muted",
            Self::Confirming => "confirming",
            Self::Window => "window",
        }
    }

    const fn is_routine(self) -> bool {
        matches!(self, Self::Muted | Self::Confirming | Self::Window)
    }
}

#[derive(Clone, Copy, Default)]
struct DropBucket {
    last_logged: Option<Instant>,
    suppressed: u64,
}

struct DropLog {
    buckets: [DropBucket; DropKind::COUNT * DropReason::COUNT],
}

impl Default for DropLog {
    fn default() -> Self {
        Self {
            buckets: [DropBucket::default(); DropKind::COUNT * DropReason::COUNT],
        }
    }
}

impl DropLog {
    fn record(
        &mut self,
        kind: DropKind,
        reason: DropReason,
        state: State,
        acoustic: bool,
        now: Instant,
    ) -> Option<u64> {
        let bucket = &mut self.buckets[kind.index() * DropReason::COUNT + reason.index()];
        if bucket
            .last_logged
            .is_some_and(|last| now.saturating_duration_since(last) < DROP_LOG_INTERVAL)
        {
            bucket.suppressed = bucket.suppressed.saturating_add(1);
            return None;
        }
        let suppressed = std::mem::take(&mut bucket.suppressed);
        bucket.last_logged = Some(now);
        if reason.is_routine() {
            tracing::debug!(
                kind = kind.as_str(),
                reason = reason.as_str(),
                suppressed,
                busy = state.busy,
                confirming = state.confirming,
                muted = state.muted,
                acoustic,
                "listen input dropped"
            );
        } else {
            tracing::info!(
                kind = kind.as_str(),
                reason = reason.as_str(),
                suppressed,
                busy = state.busy,
                confirming = state.confirming,
                muted = state.muted,
                acoustic,
                "listen input dropped"
            );
        }
        Some(suppressed)
    }
}

struct ClipRecording {
    clip: Clip,
    epoch: u64,
    // The receiver carries the watch version at capture, including same-value ABA edges.
    generation: watch::Receiver<State>,
}

#[derive(Clone)]
struct WakeEvent {
    at: Instant,
    epoch: u64,
    // The receiver carries the watch version at capture, including same-value ABA edges.
    generation: watch::Receiver<State>,
}

enum WakeSend {
    Sent { replaced: bool },
    Closed,
}

struct CaptureTx {
    clips: mpsc::Sender<ClipRecording>,
    wakes: watch::Sender<Option<WakeEvent>>,
    epoch: AtomicU64,
    consumed_wake_epoch: Arc<AtomicU64>,
}

struct CaptureRx {
    clips: mpsc::Receiver<ClipRecording>,
    wakes: Option<watch::Receiver<Option<WakeEvent>>>,
    consumed_wake_epoch: Arc<AtomicU64>,
}

fn channel(clips: usize) -> (CaptureTx, CaptureRx) {
    let (clip_tx, clip_rx) = mpsc::channel(clips);
    let (wake_tx, wake_rx) = watch::channel(None);
    let consumed_wake_epoch = Arc::new(AtomicU64::new(0));
    (
        CaptureTx {
            clips: clip_tx,
            wakes: wake_tx,
            epoch: AtomicU64::new(0),
            consumed_wake_epoch: Arc::clone(&consumed_wake_epoch),
        },
        CaptureRx {
            clips: clip_rx,
            wakes: Some(wake_rx),
            consumed_wake_epoch,
        },
    )
}

impl CaptureTx {
    /// Dedicated latest-wins wake path: never blocked by a full clip queue.
    fn send_wake(&self, at: Instant, generation: watch::Receiver<State>) -> WakeSend {
        let prev = self.epoch.fetch_add(1, Ordering::SeqCst);
        let epoch = prev + 1;
        let replaced = prev > self.consumed_wake_epoch.load(Ordering::SeqCst);
        if self
            .wakes
            .send(Some(WakeEvent {
                at,
                epoch,
                generation,
            }))
            .is_ok()
        {
            WakeSend::Sent { replaced }
        } else {
            WakeSend::Closed
        }
    }

    fn try_send_clip(
        &self,
        clip: Clip,
        generation: watch::Receiver<State>,
    ) -> std::result::Result<(), mpsc::error::TrySendError<ClipRecording>> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        self.clips.try_send(ClipRecording {
            clip,
            epoch,
            generation,
        })
    }

    #[cfg(test)]
    async fn send_clip(
        &self,
        clip: Clip,
        generation: watch::Receiver<State>,
    ) -> std::result::Result<(), mpsc::error::SendError<ClipRecording>> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        self.clips
            .send(ClipRecording {
                clip,
                epoch,
                generation,
            })
            .await
    }
}

enum Event {
    Wake(WakeEvent),
    Clip(ClipRecording),
}

async fn record(
    mic: &mut impl Microphone,
    detector: &mut impl wake::WakeDetector,
    mut generation: watch::Receiver<State>,
    tx: &CaptureTx,
    acoustic: bool,
    drops: &mut DropLog,
) -> Result<()> {
    let mut vad = Vad::default();
    let mut state = *generation.borrow_and_update();
    loop {
        let frame = mic.frame().await?;
        if generation.has_changed()? {
            let next = *generation.borrow_and_update();
            let reset_detector = detector_boundary(state, next);
            state = next;
            if reset_detector {
                vad = Vad::default();
                detector.reset();
            } else {
                vad.reset();
            }
            // This frame straddles a mode change. Keep the mic open so the next complete
            // frame following a barge-in is available without a USB/ALSA reopen delay.
            continue;
        }
        if !state.confirming && !state.muted && detector.fired(&frame, state.busy).await? {
            if generation.has_changed()? {
                let next = *generation.borrow_and_update();
                let reset_detector = detector_boundary(state, next);
                state = next;
                if reset_detector {
                    vad = Vad::default();
                    detector.reset();
                } else {
                    vad.reset();
                }
                drops.record(
                    DropKind::Wake,
                    DropReason::Stale,
                    state,
                    acoustic,
                    Instant::now(),
                );
                continue;
            }
            match tx.send_wake(Instant::now(), generation.clone()) {
                WakeSend::Sent { replaced: true } => {
                    drops.record(
                        DropKind::Wake,
                        DropReason::Replaced,
                        state,
                        acoustic,
                        Instant::now(),
                    );
                }
                WakeSend::Sent { replaced: false } => {}
                WakeSend::Closed => return Ok(()),
            }
        }
        if state.muted || (state.busy && !state.confirming) {
            continue;
        }
        if let Some(clip) = vad.feed(&frame, Instant::now(), state.busy) {
            // One clip decoding and one waiting. Never block the mic or accumulate a backlog.
            match tx.try_send_clip(clip, generation.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    drops.record(
                        DropKind::Clip,
                        DropReason::Overflow,
                        state,
                        acoustic,
                        Instant::now(),
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
            }
        }
    }
}

async fn capture(
    cfg: &Config,
    mode: Mode,
    #[cfg(feature = "onnx-wake")] worker: Option<(&wake::Worker, &wake::Models)>,
    mut state: watch::Receiver<State>,
    tx: CaptureTx,
) {
    let mut drops = DropLog::default();
    loop {
        let current = *state.borrow_and_update();
        if capture_paused(current, mode.barge_in) {
            if state.changed().await.is_err() {
                return;
            }
            continue;
        }
        let generation = state.clone();
        let mut mic = match Alsa::open(cfg) {
            Ok(mic) => mic,
            Err(error) => {
                tracing::warn!(%error,"microphone capture unavailable; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let session = async {
            #[cfg(feature = "onnx-wake")]
            if let Some((engine, models)) = worker {
                return record(
                    &mut mic,
                    &mut wake::Stream::new(engine, models),
                    generation,
                    &tx,
                    mode.acoustic,
                    &mut drops,
                )
                .await;
            }
            record(
                &mut mic,
                &mut wake::Transcript,
                generation,
                &tx,
                mode.acoustic,
                &mut drops,
            )
            .await
        };
        let result = tokio::select! {
            biased;
            closed = async {
                state
                    .wait_for(|s| capture_must_restart(current, *s, mode.barge_in))
                    .await
                    .is_err()
            } => closed.then_some(Ok(())),
            res = session => Some(res),
        };
        // Drop the frame read first, then release the old device before a new
        // claim or an immediate unmute can start a second arecord process.
        if let Err(error) = mic.stop().await {
            tracing::error!(%error,"microphone release not confirmed; capture stopped");
            return;
        }
        match result {
            Some(Ok(())) => return,
            Some(Err(error)) => {
                tracing::warn!(%error, "microphone capture unavailable; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            None => {}
        }
    }
}

async fn face_timer(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t.into()).await,
        None => pending().await,
    }
}

fn valid_generation(generation: &watch::Receiver<State>, current: State) -> bool {
    matches!(generation.has_changed(), Ok(false)) && *generation.borrow() == current
}

fn classify_wake(wake: &WakeEvent, current: State) -> std::result::Result<(), DropReason> {
    if !valid_generation(&wake.generation, current) {
        return Err(DropReason::Stale);
    }
    if current.muted {
        return Err(DropReason::Muted);
    }
    if current.confirming {
        return Err(DropReason::Confirming);
    }
    Ok(())
}

fn classify_clip(
    record: &ClipRecording,
    min_clip_epoch: u64,
    current: State,
    acoustic: bool,
    policy: &Policy,
) -> std::result::Result<(), DropReason> {
    if !valid_generation(&record.generation, current) {
        return Err(DropReason::Stale);
    }
    if record.epoch < min_clip_epoch {
        return Err(DropReason::Replaced);
    }
    if current.muted {
        return Err(DropReason::Muted);
    }
    if current.busy && !current.confirming {
        return Err(DropReason::Echo);
    }
    if acoustic && !current.confirming && !policy.acoustic_clip(record.clip.started) {
        return Err(DropReason::Window);
    }
    Ok(())
}

async fn recv_wake(
    wakes: &mut Option<watch::Receiver<Option<WakeEvent>>>,
    consumed_wake_epoch: &AtomicU64,
    current: State,
    acoustic: bool,
    drops: &mut DropLog,
) -> WakeEvent {
    let Some(rx) = wakes.as_mut() else {
        return pending().await;
    };
    loop {
        if rx.changed().await.is_err() {
            *wakes = None;
            return pending().await;
        }
        let wake = rx.borrow_and_update().clone();
        if let Some(wake) = wake {
            consumed_wake_epoch.store(wake.epoch, Ordering::SeqCst);
            match classify_wake(&wake, current) {
                Ok(()) => return wake,
                Err(reason) => {
                    drops.record(DropKind::Wake, reason, current, acoustic, Instant::now());
                }
            }
        }
    }
}

async fn recv_clip(
    buffered: &mut Option<ClipRecording>,
    clips: &mut mpsc::Receiver<ClipRecording>,
) -> Option<ClipRecording> {
    if let Some(clip) = buffered.take() {
        return Some(clip);
    }
    clips.recv().await
}

fn drain_clips(
    clips: &mut mpsc::Receiver<ClipRecording>,
    buffered: &mut Option<ClipRecording>,
    keep: impl Fn(&ClipRecording) -> std::result::Result<(), DropReason>,
    current: State,
    acoustic: bool,
    drops: &mut DropLog,
) {
    if let Some(clip) = buffered.as_ref()
        && let Err(reason) = keep(clip)
    {
        *buffered = None;
        drops.record(DropKind::Clip, reason, current, acoustic, Instant::now());
    }
    if buffered.is_none() {
        while let Ok(clip) = clips.try_recv() {
            match keep(&clip) {
                Ok(()) => {
                    *buffered = Some(clip);
                    break;
                }
                Err(reason) => {
                    drops.record(DropKind::Clip, reason, current, acoustic, Instant::now());
                }
            }
        }
    }
}

struct Processor<'a> {
    acoustic: bool,
    state: watch::Receiver<State>,
    current: State,
    policy: Policy,
    drops: DropLog,
    min_clip_epoch: u64,
    buffered_clip: Option<ClipRecording>,
    pending_wake: Option<WakeEvent>,
    rx: CaptureRx,
    out: &'a mpsc::Sender<Control>,
}

impl<'a> Processor<'a> {
    fn new(
        acoustic: bool,
        mut state: watch::Receiver<State>,
        rx: CaptureRx,
        out: &'a mpsc::Sender<Control>,
    ) -> Self {
        let current = *state.borrow_and_update();
        Self {
            acoustic,
            state,
            current,
            policy: Policy::default(),
            drops: DropLog::default(),
            min_clip_epoch: 0,
            buffered_clip: None,
            pending_wake: None,
            rx,
            out,
        }
    }

    async fn next_event(&mut self) -> Option<Event> {
        loop {
            if let Some(wake) = self.pending_wake.take() {
                if self.state.has_changed().unwrap_or(true) {
                    self.drops.record(
                        DropKind::Wake,
                        DropReason::Stale,
                        self.current,
                        self.acoustic,
                        Instant::now(),
                    );
                    continue;
                }
                return Some(Event::Wake(wake));
            }
            tokio::select! {
                biased;
                changed = self.state.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    let next = *self.state.borrow_and_update();
                    self.policy.changed(self.current, next, Instant::now());
                    self.current = next;
                    drain_clips(
                        &mut self.rx.clips,
                        &mut self.buffered_clip,
                        |clip| if valid_generation(&clip.generation, self.current) { Ok(()) } else { Err(DropReason::Stale) },
                        self.current,
                        self.acoustic,
                        &mut self.drops,
                    );
                },
                wake = recv_wake(&mut self.rx.wakes, &self.rx.consumed_wake_epoch, self.current, self.acoustic, &mut self.drops) => {
                    return Some(Event::Wake(wake));
                },
                () = face_timer(self.policy.face_deadline) => {
                    self.policy.face_deadline = None;
                    if self.out.send(Control::Listening(false)).await.is_err() {
                        return None;
                    }
                },
                next = recv_clip(&mut self.buffered_clip, &mut self.rx.clips) => {
                    return next.map(Event::Clip);
                },
            }
        }
    }

    async fn handle_wake(&mut self, wake: WakeEvent) -> bool {
        if let Err(reason) = classify_wake(&wake, self.current) {
            self.drops.record(
                DropKind::Wake,
                reason,
                self.current,
                self.acoustic,
                Instant::now(),
            );
            return true;
        }
        self.min_clip_epoch = self.min_clip_epoch.max(wake.epoch);
        let min_epoch = self.min_clip_epoch;
        drain_clips(
            &mut self.rx.clips,
            &mut self.buffered_clip,
            |clip| {
                if clip.epoch < min_epoch {
                    Err(DropReason::Replaced)
                } else {
                    Ok(())
                }
            },
            self.current,
            self.acoustic,
            &mut self.drops,
        );
        let permit = tokio::select! {
            biased;
            () = changed_since(&self.state) => {
                self.drops.record(DropKind::Wake, DropReason::Stale, self.current, self.acoustic, Instant::now());
                return true;
            }
            result = self.out.reserve() => match result {
                Ok(p) => p,
                Err(_) => return false,
            },
        };
        self.policy.acoustic_wake(wake.at);
        permit.send(Control::Wake);
        true
    }

    async fn handle_clip(&mut self, r: &impl Recognizer, record: ClipRecording) -> bool {
        if let Err(reason) = classify_clip(
            &record,
            self.min_clip_epoch,
            self.current,
            self.acoustic,
            &self.policy,
        ) {
            self.drops.record(
                DropKind::Clip,
                reason,
                self.current,
                self.acoustic,
                Instant::now(),
            );
            return true;
        }
        let text = tokio::select! {
            biased;
            // wait_for/changed would mark the version seen; has_changed at loop entry must
            // still observe this edge. Use a clone exclusively as the cancellation signal.
            () = changed_since(&self.state) => {
                self.drops.record(DropKind::Clip, DropReason::Stale, self.current, self.acoustic, Instant::now());
                return true;
            }
            wake = recv_wake(&mut self.rx.wakes, &self.rx.consumed_wake_epoch, self.current, self.acoustic, &mut self.drops) => {
                self.drops.record(DropKind::Clip, DropReason::Replaced, self.current, self.acoustic, Instant::now());
                self.pending_wake = Some(wake);
                return true;
            }
            result = r.transcript(&record.clip.pcm) => match result {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(%error, "local speech recognition failed");
                    return true;
                }
            }
        };
        let mut next_policy = self.policy;
        let mut commands =
            next_policy.heard(&text, self.current, record.clip.started, Instant::now());
        if self.acoustic {
            commands.retain(|c| c != &Control::Wake);
        }
        if commands.is_empty() {
            self.policy = next_policy;
            return true;
        }
        // A wake+say/stop pair stays contiguous on the shared input channel.
        let permits = tokio::select! {
            biased;
            () = changed_since(&self.state) => {
                self.drops.record(DropKind::Clip, DropReason::Stale, self.current, self.acoustic, Instant::now());
                return true;
            }
            wake = recv_wake(&mut self.rx.wakes, &self.rx.consumed_wake_epoch, self.current, self.acoustic, &mut self.drops) => {
                self.drops.record(DropKind::Clip, DropReason::Replaced, self.current, self.acoustic, Instant::now());
                self.pending_wake = Some(wake);
                return true;
            }
            result = self.out.reserve_many(commands.len()) => match result {
                Ok(p) => p,
                Err(_) => return false,
            },
        };
        self.policy = next_policy;
        for (permit, command) in permits.zip(commands) {
            permit.send(command);
        }
        true
    }
}

async fn process(
    r: &impl Recognizer,
    acoustic: bool,
    state: watch::Receiver<State>,
    rx: CaptureRx,
    out: &mpsc::Sender<Control>,
) {
    let mut processor = Processor::new(acoustic, state, rx, out);
    while let Some(event) = processor.next_event().await {
        let keep_going = match event {
            Event::Wake(wake) => processor.handle_wake(wake).await,
            Event::Clip(record) => processor.handle_clip(r, record).await,
        };
        if !keep_going {
            return;
        }
    }
}

async fn changed_since(state: &watch::Receiver<State>) {
    let _ = state.clone().changed().await;
}

#[cfg(any(feature = "onnx-wake", test))]
async fn wait_state_closed(state: &mut watch::Receiver<State>) {
    while state.changed().await.is_ok() {}
}

#[cfg(any(feature = "onnx-wake", test))]
async fn resolve_worker<W, Fut>(
    configured: bool,
    start: impl FnOnce() -> Fut,
    state: &watch::Receiver<State>,
    out: &mpsc::Sender<Control>,
) -> Option<Option<W>>
where
    Fut: std::future::Future<Output = Result<W>>,
{
    if !configured {
        return Some(None);
    }
    let mut closed_state = state.clone();
    tokio::select! {
        biased;
        () = out.closed() => None,
        () = wait_state_closed(&mut closed_state) => None,
        res = start() => match res {
            Ok(worker) => Some(Some(worker)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "local wake model unavailable; falling back to transcript mode with barge-in disabled"
                );
                Some(None)
            }
        }
    }
}

/// Owns capture and recognition subprocess lifetimes. Dropping this future, closing the
/// state watch, or dropping the input receiver stops both, even if a helper is stalled.
pub async fn run(cfg: Config, out: mpsc::Sender<Control>, state: watch::Receiver<State>) {
    #[cfg(feature = "onnx-wake")]
    let Some(worker) = resolve_worker(
        cfg.wake.is_some(),
        || async {
            let models = cfg.wake.as_ref().context("missing wake models")?;
            wake::Worker::start(&models.dir).await
        },
        &state,
        &out,
    )
    .await
    else {
        return;
    };
    #[cfg(feature = "onnx-wake")]
    let mode = mode_for(cfg.barge_in, worker.is_some());
    #[cfg(not(feature = "onnx-wake"))]
    let mode = mode_for(cfg.barge_in, cfg.wake.is_some());
    let (tx, rx) = channel(1);
    let capture_state = state.clone();
    let capturing = async {
        #[cfg(feature = "onnx-wake")]
        capture(
            &cfg,
            mode,
            worker.as_ref().zip(cfg.wake.as_ref()),
            capture_state,
            tx,
        )
        .await;
        #[cfg(not(feature = "onnx-wake"))]
        capture(&cfg, mode, capture_state, tx).await;
    };
    tokio::select! {
        () = capturing => (),
        () = process(&cfg, mode.acoustic, state, rx, &out) => (),
        () = out.closed() => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listen::vad::FRAME;
    use std::{
        collections::BTreeMap,
        sync::{Mutex, atomic::AtomicUsize},
    };
    use tokio::sync::Notify;

    struct TestMic(usize);
    impl Microphone for TestMic {
        async fn frame(&mut self) -> Result<[i16; FRAME]> {
            self.0 += 1;
            match self.0 {
                1..=12 => Ok([1000; FRAME]),
                13..=52 => Ok([0; FRAME]),
                _ => pending().await,
            }
        }
    }

    struct TestStt;
    impl Recognizer for TestStt {
        fn transcript(&self, pcm: &[i16]) -> impl std::future::Future<Output = Result<String>> {
            assert_eq!(pcm.len(), 52 * FRAME);
            std::future::ready(Ok("Hey Pip, what time is it?".into()))
        }
    }

    fn make_clip(started: Instant, pcm: &[i16]) -> Clip {
        Clip {
            started,
            pcm: pcm.to_vec(),
        }
    }

    struct DropNotify(Arc<Notify>);
    impl Drop for DropNotify {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    struct HoldOnceStt {
        hold: Vec<i16>,
        expect_next: Vec<i16>,
        reply: &'static str,
        seen: Arc<Mutex<Vec<Vec<i16>>>>,
        started: Arc<Notify>,
        dropped: Arc<Notify>,
    }

    impl Recognizer for HoldOnceStt {
        async fn transcript(&self, pcm: &[i16]) -> Result<String> {
            self.seen.lock().unwrap().push(pcm.to_vec());
            if pcm == self.hold {
                let _guard = DropNotify(Arc::clone(&self.dropped));
                self.started.notify_one();
                return pending().await;
            }
            assert_eq!(pcm, self.expect_next, "unexpected clip decoded by Whisper");
            Ok(self.reply.into())
        }
    }

    struct GatedStt {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl Recognizer for GatedStt {
        async fn transcript(&self, pcm: &[i16]) -> Result<String> {
            self.started.notify_one();
            self.release.notified().await;
            match pcm {
                [10] => Ok("what time is it?".into()),
                [20] => Ok("012 345".into()),
                other => panic!("unexpected clip {other:?}"),
            }
        }
    }

    struct ScriptedMic(mpsc::Receiver<[i16; FRAME]>);
    impl Microphone for ScriptedMic {
        async fn frame(&mut self) -> Result<[i16; FRAME]> {
            self.0
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("mic ended"))
        }
    }

    struct CountingDetector {
        resets: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        tick: Arc<Notify>,
        reset_seen: Arc<Notify>,
    }

    impl wake::WakeDetector for CountingDetector {
        fn reset(&mut self) {
            self.resets.fetch_add(1, Ordering::SeqCst);
            self.reset_seen.notify_one();
        }
        fn fired(
            &mut self,
            _: &[i16; FRAME],
            _: bool,
        ) -> impl std::future::Future<Output = Result<bool>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.tick.notify_one();
            std::future::ready(Ok(false))
        }
    }

    type CapturedEvents = Arc<Mutex<Vec<BTreeMap<String, String>>>>;
    struct FieldVisitor(BTreeMap<String, String>);
    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }
    }

    struct CaptureLayer(CapturedEvents);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = FieldVisitor(BTreeMap::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    #[tokio::test]
    async fn mock_beam_reaches_existing_control_channel_in_order() {
        let (_state, rx) = watch::channel(State::default());
        let generation = rx.clone();
        let (tx, capture_rx) = channel(1);
        let (out, mut controls) = mpsc::channel(2);
        let worker =
            tokio::spawn(async move { process(&TestStt, false, rx, capture_rx, &out).await });
        tokio::task::yield_now().await;
        let mic = tokio::spawn(async move {
            let mut drops = DropLog::default();
            record(
                &mut TestMic(0),
                &mut wake::Transcript,
                generation,
                &tx,
                false,
                &mut drops,
            )
            .await
        });
        let pair = tokio::time::timeout(Duration::from_secs(3), async {
            (controls.recv().await, controls.recv().await)
        })
        .await
        .unwrap();
        assert_eq!(
            pair,
            (
                Some(Control::Wake),
                Some(Control::Say("what time is it?".into()))
            )
        );
        mic.abort();
        worker.abort();
    }

    #[tokio::test]
    async fn busy_acoustic_wake_accepts_immediate_idle_clip_but_not_old_playback() {
        struct Stt(Arc<AtomicUsize>);
        impl Recognizer for Stt {
            fn transcript(&self, pcm: &[i16]) -> impl std::future::Future<Output = Result<String>> {
                assert_eq!(pcm, [2]);
                self.0.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok("what time is it?".into()))
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let stt = Stt(calls.clone());
        let (state_tx, rx) = watch::channel(State {
            busy: true,
            ..State::default()
        });
        let busy = rx.clone();
        let (tx, capture_rx) = channel(3);
        let (out, mut controls) = mpsc::channel(4);
        let worker = tokio::spawn(async move { process(&stt, true, rx, capture_rx, &out).await });
        tokio::task::yield_now().await;
        let at = Instant::now();
        tx.send_clip(
            Clip {
                started: at,
                pcm: vec![1],
            },
            busy.clone(),
        )
        .await
        .unwrap();
        assert!(matches!(
            tx.send_wake(at, busy.clone()),
            WakeSend::Sent { .. }
        ));
        assert_eq!(controls.recv().await, Some(Control::Wake));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        state_tx.send(State::default()).unwrap();
        tx.send_clip(
            Clip {
                started: at,
                pcm: vec![3],
            },
            busy,
        )
        .await
        .unwrap(); // queued before the interruption
        tx.send_clip(
            Clip {
                started: at,
                pcm: vec![2],
            },
            state_tx.subscribe(),
        )
        .await
        .unwrap(); // no settle/reopen wait
        assert_eq!(
            controls.recv().await,
            Some(Control::Say("what time is it?".into()))
        );
        drop(tx);
        worker.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn leaving_claim_mode_cancels_inflight_recognition() {
        struct Slow(Arc<Notify>);
        impl Recognizer for Slow {
            async fn transcript(&self, _: &[i16]) -> Result<String> {
                self.0.notify_one();
                pending().await
            }
        }
        let started = Arc::new(Notify::new());
        let stt = Slow(started.clone());
        let state = State {
            confirming: true,
            ..State::default()
        };
        let (state_tx, rx) = watch::channel(state);
        let generation = rx.clone();
        let (tx, capture_rx) = channel(1);
        let (out, mut controls) = mpsc::channel(4);
        let worker = tokio::spawn(async move { process(&stt, true, rx, capture_rx, &out).await });
        tokio::task::yield_now().await;
        tx.send_clip(
            Clip {
                started: Instant::now(),
                pcm: vec![1],
            },
            generation,
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        state_tx.send(State::default()).unwrap();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(controls.recv().await.is_none());
    }

    #[tokio::test]
    async fn neural_wake_interrupts_held_followup_decode_with_full_queue_and_skips_prewake_clip() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let stt = HoldOnceStt {
            hold: vec![1],
            expect_next: vec![3],
            reply: "what time is it?",
            seen: Arc::clone(&seen),
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        };
        let (state_tx, rx) = watch::channel(State {
            busy: true,
            ..State::default()
        });
        let (tx, capture_rx) = channel(1);
        let (out, mut controls) = mpsc::channel(4);
        let worker = tokio::spawn(async move { process(&stt, true, rx, capture_rx, &out).await });
        tokio::task::yield_now().await;

        // Open the post-playback follow-up window so clip [1] enters Whisper.
        state_tx.send(State::default()).unwrap();
        let idle = state_tx.subscribe();
        let followup_onset = Instant::now() + Duration::from_millis(500);
        tx.send_clip(make_clip(followup_onset, &[1]), idle.clone())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();

        // Fill the single-slot clip queue with a pre-wake clip whose onset would otherwise
        // fall inside the upcoming wake window [at - 2s, at + 4s].
        let wake_at = followup_onset + Duration::from_millis(100);
        tx.try_send_clip(make_clip(followup_onset, &[2]), idle.clone())
            .unwrap();
        assert!(matches!(
            tx.try_send_clip(make_clip(followup_onset, &[99]), idle.clone()),
            Err(mpsc::error::TrySendError::Full(_))
        ));

        // Priority wake still reaches controller, interrupts clip [1], and evicts clip [2].
        assert!(matches!(
            tx.send_wake(wake_at, idle.clone()),
            WakeSend::Sent { replaced: false }
        ));
        tokio::time::timeout(Duration::from_secs(3), dropped.notified())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), controls.recv())
                .await
                .unwrap(),
            Some(Control::Wake)
        );

        // Fresh post-wake follow-up fits into the freed one-slot queue and decodes cleanly.
        tx.try_send_clip(make_clip(wake_at, &[3]), idle).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), controls.recv())
                .await
                .unwrap(),
            Some(Control::Say("what time is it?".into()))
        );
        drop(tx);
        worker.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![vec![1], vec![3]]);
    }

    #[tokio::test]
    async fn transcript_mode_disables_barge_in_even_with_legacy_env_and_preserves_enrollment() {
        struct ConfirmStt;
        impl Recognizer for ConfirmStt {
            fn transcript(&self, pcm: &[i16]) -> impl std::future::Future<Output = Result<String>> {
                assert_eq!(pcm, [42]);
                std::future::ready(Ok("012 345".into()))
            }
        }
        let fixture_model = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/listen/tests/whisper-fixture.sh"
        );
        let cfg = Config::from_env_with(|key| match key {
            "BUDDY_LISTEN_MODEL" => Some(fixture_model.into()),
            "BUDDY_LISTEN_BARGE_IN" => Some("1".into()),
            _ => None,
        })
        .unwrap()
        .unwrap();
        assert!(!cfg.barge_in);
        assert!(cfg.wake.is_none());

        let idle = State::default();
        let busy = State { busy: true, ..idle };
        let confirming_busy = State {
            busy: true,
            confirming: true,
            ..idle
        };
        let muted = State {
            muted: true,
            ..idle
        };
        assert!(capture_paused(busy, false));
        assert!(!capture_paused(confirming_busy, false));
        assert!(capture_paused(muted, false));
        assert!(capture_must_restart(idle, busy, false));
        assert!(!capture_must_restart(idle, busy, true));
        assert!(capture_must_restart(idle, confirming_busy, true));
        assert!(capture_must_restart(idle, muted, true));

        // Enrollment confirmation still reaches Whisper even when busy in transcript mode.
        let (_state_tx, rx) = watch::channel(confirming_busy);
        let generation = rx.clone();
        let (tx, capture_rx) = channel(1);
        let (out, mut controls) = mpsc::channel(2);
        let worker =
            tokio::spawn(async move { process(&ConfirmStt, false, rx, capture_rx, &out).await });
        tokio::task::yield_now().await;
        tx.send_clip(make_clip(Instant::now(), &[42]), generation)
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), controls.recv())
                .await
                .unwrap(),
            Some(Control::Confirm("012345".into()))
        );
        drop(tx);
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn transcript_mode_blocks_busy_echo_cancels_stale_decode_and_accepts_post_playback_wake()
    {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let stt = HoldOnceStt {
            hold: vec![1],
            expect_next: vec![4],
            reply: "Hey Pip, what time is it?",
            seen: Arc::clone(&seen),
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        };
        let (state_tx, rx) = watch::channel(State::default());
        let idle_before = rx.clone();
        let (tx, capture_rx) = channel(1);
        let (out, mut controls) = mpsc::channel(4);
        let worker = tokio::spawn(async move { process(&stt, false, rx, capture_rx, &out).await });
        tokio::task::yield_now().await;

        // Hold Whisper on an earlier idle clip, then enter busy playback -> stale decode cancels.
        tx.send_clip(make_clip(Instant::now(), &[1]), idle_before)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        state_tx
            .send(State {
                busy: true,
                ..State::default()
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), dropped.notified())
            .await
            .unwrap();

        // Busy playback/echo clips never start Whisper or clog the queue.
        let busy_gen = state_tx.subscribe();
        tx.send_clip(make_clip(Instant::now(), &[2]), busy_gen.clone())
            .await
            .unwrap();
        tx.send_clip(make_clip(Instant::now(), &[3]), busy_gen)
            .await
            .unwrap();

        // Playback finishes: first valid post-playback "Hey Pip" is neither lost nor blocked.
        state_tx.send(State::default()).unwrap();
        let idle_after = state_tx.subscribe();
        tx.send_clip(make_clip(Instant::now(), &[4]), idle_after)
            .await
            .unwrap();
        let pair = tokio::time::timeout(Duration::from_secs(3), async {
            (controls.recv().await, controls.recv().await)
        })
        .await
        .unwrap();
        assert_eq!(
            pair,
            (
                Some(Control::Wake),
                Some(Control::Say("what time is it?".into()))
            )
        );
        drop(tx);
        worker.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![vec![1], vec![4]]);
    }

    #[tokio::test]
    async fn priority_wake_preserves_aba_generation_mute_and_claim_confirmation() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let stt = GatedStt {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        };
        let (state_tx, rx) = watch::channel(State {
            muted: true,
            ..State::default()
        });
        let muted_gen = rx.clone();
        let (tx, capture_rx) = channel(1);
        let (out, mut controls) = mpsc::channel(4);
        let worker = tokio::spawn(async move { process(&stt, true, rx, capture_rx, &out).await });
        tokio::task::yield_now().await;

        // 1. Muted: wake with current muted generation is dropped.
        assert!(matches!(
            tx.send_wake(Instant::now(), muted_gen),
            WakeSend::Sent { .. }
        ));
        tokio::task::yield_now().await;
        assert!(controls.try_recv().is_err());

        // 2. ABA: idle (A) -> busy (B) -> idle (A'). Wake carrying old A must not cancel A' decode.
        state_tx.send(State::default()).unwrap();
        let gen_a = state_tx.subscribe();
        state_tx
            .send(State {
                busy: true,
                ..State::default()
            })
            .unwrap();
        state_tx.send(State::default()).unwrap();
        let gen_a2 = state_tx.subscribe();
        let onset = Instant::now();
        assert!(matches!(
            tx.send_wake(onset, gen_a2.clone()),
            WakeSend::Sent { .. }
        ));
        assert_eq!(controls.recv().await, Some(Control::Wake));
        tx.send_clip(make_clip(onset, &[10]), gen_a2).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        assert!(matches!(tx.send_wake(onset, gen_a), WakeSend::Sent { .. }));
        tokio::task::yield_now().await;
        release.notify_one();
        assert_eq!(
            controls.recv().await,
            Some(Control::Say("what time is it?".into()))
        );

        // 3. Confirmation: wake with current confirming generation must not interrupt or emit Wake.
        state_tx
            .send(State {
                confirming: true,
                ..State::default()
            })
            .unwrap();
        let confirming_gen = state_tx.subscribe();
        tx.send_clip(make_clip(Instant::now(), &[20]), confirming_gen.clone())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        assert!(matches!(
            tx.send_wake(Instant::now(), confirming_gen),
            WakeSend::Sent { .. }
        ));
        tokio::task::yield_now().await;
        release.notify_one();
        assert_eq!(
            controls.recv().await,
            Some(Control::Confirm("012345".into()))
        );
        drop(tx);
        worker.await.unwrap();
        assert!(controls.try_recv().is_err());
    }

    #[tokio::test]
    async fn wake_worker_load_failure_falls_back_to_transcript_without_barge_in_and_keeps_claims() {
        struct FallbackStt;
        impl Recognizer for FallbackStt {
            fn transcript(&self, pcm: &[i16]) -> impl std::future::Future<Output = Result<String>> {
                let text = match pcm {
                    [1] => "Hey Pip, hello",
                    [2] => "012 345",
                    other => panic!("unexpected clip in fallback mode: {other:?}"),
                };
                std::future::ready(Ok(text.into()))
            }
        }
        let (state_tx, rx) = watch::channel(State::default());
        let (out, mut controls) = mpsc::channel(4);
        let resolved: Option<Option<()>> = resolve_worker(
            true,
            || async { Err(anyhow::anyhow!("missing onnx runtime")) },
            &rx,
            &out,
        )
        .await;
        assert_eq!(resolved, Some(None));
        let mode = mode_for(true, resolved.flatten().is_some());
        assert!(!mode.acoustic);
        assert!(!mode.barge_in);

        let (tx, capture_rx) = channel(1);
        let worker = tokio::spawn(async move {
            process(&FallbackStt, mode.acoustic, rx, capture_rx, &out).await;
        });
        tokio::task::yield_now().await;

        // Transcript wake works.
        tx.send_clip(make_clip(Instant::now(), &[1]), state_tx.subscribe())
            .await
            .unwrap();
        assert_eq!(
            (controls.recv().await, controls.recv().await),
            (Some(Control::Wake), Some(Control::Say("hello".into())))
        );

        // Busy echo is blocked, while busy spoken claim still works.
        state_tx
            .send(State {
                busy: true,
                ..State::default()
            })
            .unwrap();
        tx.send_clip(make_clip(Instant::now(), &[99]), state_tx.subscribe())
            .await
            .unwrap();
        state_tx
            .send(State {
                busy: true,
                confirming: true,
                ..State::default()
            })
            .unwrap();
        tx.send_clip(make_clip(Instant::now(), &[2]), state_tx.subscribe())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), controls.recv())
                .await
                .unwrap(),
            Some(Control::Confirm("012345".into()))
        );
        drop(tx);
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn wake_detector_stays_warm_across_busy_and_resets_on_privacy_or_claim_boundaries() {
        let resets = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let tick = Arc::new(Notify::new());
        let reset_seen = Arc::new(Notify::new());
        let (frame_tx, frame_rx) = mpsc::channel(8);
        let (state_tx, state_rx) = watch::channel(State::default());
        let (tx, mut capture_rx) = channel(4);
        let detector = CountingDetector {
            resets: Arc::clone(&resets),
            calls: Arc::clone(&calls),
            tick: Arc::clone(&tick),
            reset_seen: Arc::clone(&reset_seen),
        };
        let task = tokio::spawn(async move {
            let mut drops = DropLog::default();
            let mut detector = detector;
            let _ = record(
                &mut ScriptedMic(frame_rx),
                &mut detector,
                state_rx,
                &tx,
                true,
                &mut drops,
            )
            .await;
        });

        // Ordinary idle -> busy -> idle keeps the wake detector warm (0 resets) and emits no busy VAD clips.
        frame_tx.send([0; FRAME]).await.unwrap();
        tick.notified().await;
        state_tx
            .send(State {
                busy: true,
                ..State::default()
            })
            .unwrap();
        frame_tx.send([0; FRAME]).await.unwrap(); // straddling frame
        for sample in [2000; 12].into_iter().chain([0; 42]) {
            frame_tx.send([sample; FRAME]).await.unwrap();
            tick.notified().await;
        }
        state_tx.send(State::default()).unwrap();
        frame_tx.send([0; FRAME]).await.unwrap(); // straddling frame
        frame_tx.send([0; FRAME]).await.unwrap();
        tick.notified().await;
        assert_eq!(resets.load(Ordering::SeqCst), 0);
        assert!(capture_rx.clips.try_recv().is_err());

        // Claim and privacy (mute) boundaries each reset the detector on entry and exit.
        for next in [
            State {
                confirming: true,
                ..State::default()
            },
            State::default(),
            State {
                muted: true,
                ..State::default()
            },
            State::default(),
        ] {
            state_tx.send(next).unwrap();
            frame_tx.send([0; FRAME]).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), reset_seen.notified())
                .await
                .unwrap();
        }
        assert_eq!(resets.load(Ordering::SeqCst), 4);
        assert!(calls.load(Ordering::SeqCst) > 0);

        drop(frame_tx);
        task.await.unwrap();
    }

    #[test]
    fn drop_log_is_rate_limited_and_emits_only_safe_metadata() {
        use tracing_subscriber::prelude::*;

        // Callsite interest is process-global. With one scoped dispatcher,
        // tracing-core's JustOne fast path uses the *registering thread's*
        // default. A parallel producer without our subscriber can register
        // this exact INFO callsite as disabled even while our rate limiter
        // correctly admits all three entries. Exercise the real macros and
        // subscriber in a single-purpose child, once, with no retries.
        const CHILD: &str = "BUDDY_TEST_DROP_LOG_CHILD";
        const TEST: &str = "listen::tests::drop_log_is_rate_limited_and_emits_only_safe_metadata";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed; 0 failed"),
                "isolated log test: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let captured: CapturedEvents = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(Arc::clone(&captured)));
        tracing::subscriber::set_global_default(subscriber).unwrap();
        {
            let mut log = DropLog::default();
            let t0 = Instant::now();
            let idle = State::default();
            assert_eq!(
                log.record(DropKind::Clip, DropReason::Overflow, idle, true, t0),
                Some(0)
            );
            for i in 1..=5 {
                assert_eq!(
                    log.record(
                        DropKind::Clip,
                        DropReason::Overflow,
                        idle,
                        true,
                        t0 + Duration::from_millis(i * 100),
                    ),
                    None
                );
            }
            assert_eq!(
                log.record(
                    DropKind::Clip,
                    DropReason::Overflow,
                    idle,
                    true,
                    t0 + DROP_LOG_INTERVAL,
                ),
                Some(5)
            );
            assert_eq!(
                log.record(
                    DropKind::Clip,
                    DropReason::Echo,
                    State { busy: true, ..idle },
                    false,
                    t0,
                ),
                Some(0)
            );
        }

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 3);
        let allowed_keys = [
            "acoustic",
            "busy",
            "confirming",
            "kind",
            "message",
            "muted",
            "reason",
            "suppressed",
        ];
        for event in events.iter() {
            let keys: Vec<&str> = event.keys().map(String::as_str).collect();
            assert_eq!(keys, allowed_keys);
        }
        assert_eq!(events[0]["reason"], "overflow");
        assert_eq!(events[0]["suppressed"], "0");
        assert_eq!(events[1]["reason"], "overflow");
        assert_eq!(events[1]["suppressed"], "5");
        assert_eq!(events[2]["reason"], "echo");
        assert_eq!(events[2]["busy"], "true");
    }
}
