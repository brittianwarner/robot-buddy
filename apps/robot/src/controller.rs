//! Conversation behaviour on the robot, independent of the transport.
//!
//! The controller never waits on the network or the disk. It owns the
//! conversation state, decides which actor call should go out next ([`Op`]),
//! and applies each call's outcome ([`Done`]). The device loop
//! (`device.rs`) runs those calls and the cache writes concurrently, so
//! control input is never queued behind them.
//!
//! Rules this module enforces:
//! - Wake, mute and say stop playback and update the face before anything
//!   else. The cancel for an interrupted turn goes out without waiting for
//!   the disk (cancelling twice is harmless); a new utterance is submitted
//!   only once it is on disk, so a crash cannot lose or duplicate it.
//! - Events apply in sequence order exactly once per cursor; a pushed event
//!   that is not the next sequence triggers a replay instead.
//! - Speech auto-plays only for a turn the server acknowledged as new to a
//!   submission this robot still wants. Replayed, deduplicated or superseded
//!   turns stay silent, and a superseded turn is cancelled.
//! - An utterance is worth submitting for [`PENDING_TTL`] only. Offline, a
//!   new utterance is refused at once with the Sorry face, never queued.
//! - An utterance replaced while its submit was unanswered is cancelled by
//!   request ID on the next connection, until the server acknowledges it.
//! - Audio must start within [`FIRST_AUDIO`] of `speech.ready`; a silent
//!   request is stopped and retried once.
use crate::{
    audio::{PlaybackEvent, Rewarm, Speaker},
    cache::{Cache, CacheStore, Pending},
    face::{self, Face, Mood, Signals},
    input::Control,
    journal::Journal,
};
use robot_buddy::{auth, protocol::OutputEvent};
use std::{fmt, future::Future, time::Duration};
use tokio::{sync::mpsc, time::Instant};

/// After this an utterance is dropped (with the Sorry face) instead of
/// submitted: an answer that late is worse than a short apology.
pub const PENDING_TTL: Duration = Duration::from_mins(1);
/// A cached utterance dated further in the future than this (the wall clock
/// stepped back) cannot be aged, so it counts as expired.
const CLOCK_SKEW_SECS: i64 = 5;
/// Delay before retrying a cache write that failed.
const PERSIST_RETRY: Duration = Duration::from_secs(1);
/// A replay sent while the robot waits on its own turn (Thinking, nothing
/// audible) gets this long before the connection is presumed dead: its pushes
/// have gone quiet, and the 15 s call deadline is too long to look stuck.
pub const WAITING_REPLAY_DEADLINE: Duration = Duration::from_secs(3);
/// Once the server has audio (`speech.ready`), the speaker must play it
/// within this long. A request that stays silent is stopped and the one
/// retry starts; the server keeps the audio, so a retry streams it again.
pub const FIRST_AUDIO: Duration = Duration::from_secs(5);
const REST_AFTER: Duration = Duration::from_mins(5);
const TERMINAL: [&str; 4] = [
    "turn.completed",
    "turn.cancelled",
    "turn.failed",
    "turn.interrupted",
];

pub struct Submitted {
    pub turn_id: String,
    pub duplicate: bool,
    pub cancelled: bool,
}

/// Why an actor call did not succeed.
#[derive(Debug)]
pub enum CallError {
    /// The server answered with an error. For a submit this means no open
    /// turn carries the request ID: the server checks for a prior request
    /// before it checks whether it is busy.
    Rejected(String),
    /// A submit was refused because this turn (another request's) is still
    /// open. Servers that predate the typed refusal answer [`Self::Rejected`].
    Busy(String),
    /// No answer (timeout or closed connection): the call may or may not
    /// have run.
    Uncertain(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(e) => write!(f, "rejected: {e}"),
            Self::Busy(turn) => write!(f, "busy with {turn}"),
            Self::Uncertain(e) => write!(f, "unanswered: {e}"),
        }
    }
}

/// The claim QR the panel shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Claim {
    pub url: String,
    pub expires: i64,
}

pub struct Registration {
    /// Only owned robots submit utterances.
    pub owned: bool,
    /// A new claim to remember, when one was started.
    pub claim: Option<Claim>,
    /// An owner signed in and the claim waits for the code on their phone to
    /// be confirmed at the robot.
    pub confirming: bool,
}

/// The actor calls the robot uses. Clones share one connection.
pub trait Remote: Clone + 'static {
    fn submit(
        &self,
        request_id: &str,
        text: &str,
    ) -> impl Future<Output = Result<Submitted, CallError>>;
    fn cancel(&self, turn_id: &str) -> impl Future<Output = Result<(), CallError>>;
    /// Cancel whatever turn `request_id` started, or will start: the server
    /// keeps a tombstone when no turn has it yet.
    fn cancel_request(&self, request_id: &str) -> impl Future<Output = Result<(), CallError>>;
    fn replay(&self, after: i64) -> impl Future<Output = Result<Vec<OutputEvent>, CallError>>;
    /// One cheap authenticated conversation call, proving a new connection
    /// works before it replaces the current one.
    fn probe(&self) -> impl Future<Output = Result<(), CallError>>;
    /// Confirm the pending claim with the code the owner spoke. The server
    /// counts wrong codes and clears the claim after a few.
    fn confirm(&self, code: &str) -> impl Future<Output = anyhow::Result<()>>;
    /// Check ownership and keep the panel's claim QR current. `shown` is the
    /// claim this robot last displayed.
    fn register(&self, shown: Claim) -> impl Future<Output = anyhow::Result<Registration>>;
    /// Close the connection cleanly.
    fn disconnect(&self) -> impl Future<Output = ()>;
}

/// One conversation call the device loop runs for the controller.
#[derive(Debug)]
pub enum Op {
    Cancel(String),
    /// Cancel by the request ID of a replaced utterance.
    CancelRequest(String),
    Submit(Pending),
    /// One replay page after `after`. `waiting`: under
    /// [`WAITING_REPLAY_DEADLINE`] instead of the call deadline.
    Replay {
        after: i64,
        waiting: bool,
    },
}

pub enum Done {
    Cancelled(String, Result<(), CallError>),
    CancelledRequest(String, Result<(), CallError>),
    Submitted(Pending, Result<Submitted, CallError>),
    Replayed(Result<Vec<OutputEvent>, CallError>),
}

impl Op {
    pub async fn run<R: Remote>(self, remote: R) -> Done {
        match self {
            Self::Cancel(turn) => {
                let result = remote.cancel(&turn).await;
                Done::Cancelled(turn, result)
            }
            Self::CancelRequest(request) => {
                let result = remote.cancel_request(&request).await;
                Done::CancelledRequest(request, result)
            }
            Self::Submit(pending) => {
                let result = remote.submit(&pending.request_id, &pending.text).await;
                Done::Submitted(pending, result)
            }
            Self::Replay { after, waiting } => {
                let replay = remote.replay(after);
                let result = if waiting {
                    tokio::time::timeout(WAITING_REPLAY_DEADLINE, replay)
                        .await
                        .unwrap_or_else(|_| Err(CallError::Uncertain("timed out".into())))
                } else {
                    replay.await
                };
                Done::Replayed(result)
            }
        }
    }
}

/// The connection can no longer be trusted. Every intent stays recorded and
/// is retried on the next connection.
#[derive(Debug)]
pub struct Lost(pub String);

/// Events from audio playback owned by the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioEvent {
    Playback(PlaybackEvent),
    OfflineVoiceFinished,
}

/// The outcome of a cache write started by [`Controller::take_persist`].
pub struct Persisted {
    request_id: Option<String>,
    result: anyhow::Result<()>,
}

/// Robot state that outlives one connection.
#[derive(Default)]
pub struct RobotState {
    pub cache: Cache,
    pub muted: bool,
    pub listening: bool,
}

struct Playing {
    generation: u64,
    audible: bool,
}

/// Speech for the turn this robot is waiting to hear. Outlives the turn's
/// `turn.completed`, because audio can still be on its way.
struct Voice {
    turn: String,
    /// `speech.ready` index 0 arrived: the server has audio.
    ready: bool,
    /// Audio reached the speaker at least once.
    heard: bool,
    retried: bool,
    /// The current request must be audible by then (see [`FIRST_AUDIO`]).
    first_audio_by: Option<Instant>,
}

struct Out<S> {
    speaker: S,
    events: mpsc::Receiver<PlaybackEvent>,
}

enum Main {
    Submit(String),
    Replay,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of separate lanes; an enum would only rename them"
)]
pub struct Controller<'a, S> {
    face: &'a Face,
    pub peripherals: crate::hardware::Peripherals,
    offline_voice: Option<crate::piper::Announcement>,
    rest_after: Option<Instant>,
    rest: Option<Mood>,
    #[cfg(test)]
    neck_log: Option<tests::Log>,
    store: &'a CacheStore,
    state: &'a mut RobotState,
    journal: Journal,
    online: bool,
    out: Option<Out<S>>,
    /// A rotated session's speaker, waiting for the current audio to end.
    next_out: Option<Out<S>>,
    /// The turn this robot submitted and has not seen finish.
    live_turn: Option<String>,
    voice: Option<Voice>,
    playing: Option<Playing>,
    pub needs_replay: bool,
    cancelling: Option<String>,
    main: Option<Main>,
    /// Request ID of the pending utterance whose submit was refused as busy
    /// once; the open turn is being cancelled and a second refusal drops it.
    busy: Option<String>,
    /// When the pending utterance was created, on the monotonic clock.
    pending_born: Option<Instant>,
    /// Request ID of the pending utterance as last written to disk.
    durable: Option<String>,
    /// Request ID of the pending utterance once a submit for it went out
    /// (or may have, before a restart): replacing it then needs a cancel.
    attempted: Option<String>,
    /// A claim waits for the owner's confirmation code.
    claiming: bool,
    persist_wanted: bool,
    persist_inflight: bool,
    persist_retry_at: Option<Instant>,
}

impl<'a, S: Speaker> Controller<'a, S> {
    /// `wall_now` (Unix seconds) ages an utterance loaded from the cache.
    pub fn new(
        face: &'a Face,
        store: &'a CacheStore,
        state: &'a mut RobotState,
        journal: Journal,
        wall_now: i64,
    ) -> Self {
        let mut controller = Self {
            face,
            peripherals: crate::hardware::Peripherals::default(),
            offline_voice: None,
            rest_after: None,
            rest: None,
            #[cfg(test)]
            neck_log: None,
            store,
            state,
            journal,
            online: false,
            out: None,
            next_out: None,
            live_turn: None,
            voice: None,
            playing: None,
            needs_replay: true,
            cancelling: None,
            main: None,
            busy: None,
            pending_born: None,
            durable: None,
            attempted: None,
            claiming: false,
            persist_wanted: false,
            persist_inflight: false,
            persist_retry_at: None,
        };
        controller.age_loaded_pending(wall_now, Instant::now());
        controller.refresh();
        controller
    }

    /// An utterance from the cache is on disk already; give it whatever is
    /// left of its time to live.
    fn age_loaded_pending(&mut self, wall_now: i64, now: Instant) {
        let Some(pending) = &self.state.cache.pending else {
            return;
        };
        let ttl = i64::try_from(PENDING_TTL.as_secs()).unwrap_or(i64::MAX);
        let age = wall_now.saturating_sub(pending.created);
        if pending.created <= 0 || age < -CLOCK_SKEW_SECS || age >= ttl {
            self.state.cache.pending = None;
            self.persist_wanted = true;
            tracing::info!("cached utterance expired; dropped");
            return;
        }
        let age = Duration::from_secs(u64::try_from(age.max(0)).unwrap_or(0));
        self.pending_born = Some(now.checked_sub(age).unwrap_or(now));
        self.durable = Some(pending.request_id.clone());
        // It may have been submitted before the restart.
        self.attempted = Some(pending.request_id.clone());
    }

    pub fn cache(&self) -> &Cache {
        &self.state.cache
    }

    /// Change the cache outside the conversation (bot ID, claim) and
    /// checkpoint it right away.
    pub fn update_cache(&mut self, change: impl FnOnce(&mut Cache)) {
        change(&mut self.state.cache);
        self.store.checkpoint(&self.state.cache, true);
    }

    /// A turn is in flight or audio is playing.
    pub fn is_live(&self) -> bool {
        self.live_turn.is_some() || self.playing.is_some()
    }

    /// This robot's turn is open and nothing is audible yet: the face reads
    /// Thinking until an event arrives, so silence must be noticed fast. Once
    /// audio plays the answer is already reaching the listener, and a
    /// reconnect would only cut it off.
    fn waiting(&self) -> bool {
        self.live_turn.is_some() && !self.playing.as_ref().is_some_and(|p| p.audible)
    }

    /// No conversation call is in flight.
    pub fn lanes_idle(&self) -> bool {
        self.cancelling.is_none() && self.main.is_none()
    }

    /// Show (or stop showing) that a claim waits for the owner's code.
    pub fn set_claiming(&mut self, claiming: bool) {
        if self.claiming != claiming {
            if claiming && self.rest == Some(Mood::Resting) {
                self.rest = None;
                self.end_cue();
            }
            self.claiming = claiming;
            self.refresh();
        }
    }

    /// Drop any cached utterance once registration confirms the robot is
    /// unowned or entering a claim, so pre-claim speech can never submit after
    /// ownership changes.
    pub fn clear_pending_unowned(&mut self) {
        self.attempted = None;
        self.pending_born = None;
        self.busy = None;
        self.durable = None;
        if self.state.cache.pending.take().is_some() {
            self.persist_wanted = false;
            self.persist_retry_at = None;
            self.store.checkpoint(&self.state.cache, true);
            self.refresh();
        }
    }

    /// Refuse an online utterance without queuing or persisting it (unowned or
    /// claiming), while leaving Listening and flashing Sorry.
    pub async fn refuse_say(&mut self, reason: &'static str) {
        self.rest = None;
        self.rest_after = None;
        self.end_cue();
        self.state.listening = false;
        self.playing = None;
        self.interrupt(None);
        // Leave Listening first: flashes are ignored over active moods.
        self.refresh_face();
        self.face.flash(Mood::Sorry, face::SORRY);
        tracing::info!(reason, "utterance refused");
        self.stop_speaker(Rewarm::No).await;
        self.stop_offline_voice().await;
        self.refresh();
    }

    /// Flash the apology face: a refused confirmation, or one with no claim.
    pub fn apologize(&self) {
        self.face.flash(Mood::Sorry, face::SORRY);
    }

    /// A replay holds the main lane while an utterance is ready to submit.
    /// The device drops that replay (it reads nothing it cannot read again)
    /// and calls [`Self::preempt_replay`], so a question never waits behind
    /// a slow catch-up.
    pub fn submit_waits_on_replay(&self, owned: bool) -> bool {
        matches!(self.main, Some(Main::Replay)) && self.submit_ready(owned)
    }

    /// The replay in flight was dropped; it runs again after the submit.
    pub fn preempt_replay(&mut self) {
        if matches!(self.main, Some(Main::Replay)) {
            self.main = None;
            self.needs_replay = true;
        }
    }

    /// The pending utterance is on disk and nothing it must wait for is left.
    fn submit_ready(&self, owned: bool) -> bool {
        let cache = &self.state.cache;
        let cancelled =
            self.cancelling.is_none() && cache.cancel.is_none() && cache.cancel_requests.is_empty();
        owned
            && cancelled
            && cache
                .pending
                .as_ref()
                .is_some_and(|p| self.durable.as_ref() == Some(&p.request_id))
    }

    /// Pushed events wait while a submit is in flight, so the new turn's
    /// first events are applied after its acknowledgement.
    pub fn accepts_events(&self) -> bool {
        !matches!(self.main, Some(Main::Submit(_)))
    }

    pub async fn control(&mut self, control: Control) {
        if matches!(
            control,
            Control::Wake | Control::Say(_) | Control::Listening(true)
        ) {
            self.rest = None;
            self.rest_after = None;
        }
        if matches!(control, Control::Mute(_)) && self.rest == Some(Mood::Resting) {
            self.rest = None;
        }
        if matches!(
            control,
            Control::Wake | Control::Say(_) | Control::Mute(true) | Control::Listening(true)
        ) {
            self.end_cue();
        }
        let stop_offline = !matches!(control, Control::Listening(false) | Control::Confirm(_));
        match control {
            Control::Sleep => {
                self.end_cue();
                self.interrupt(None);
                self.playing = None;
                self.state.listening = false;
                self.rest = Some(Mood::Asleep);
                self.rest_after = None;
                self.express_neck("asleep", 1.);
                self.refresh_face();
                self.stop_speaker(Rewarm::No).await;
            }
            Control::Wake => {
                self.state.listening = true;
                self.playing = None;
                self.interrupt(None);
                // Face::set clears any error flash for an active mood.
                self.refresh_face();
                self.stop_speaker(Rewarm::Yes).await;
            }
            Control::Listening(on) => {
                self.state.listening = on;
                if on {
                    self.refresh_face();
                }
            }
            Control::Mute(on) => {
                self.state.muted = on;
                if on {
                    self.playing = None;
                    self.refresh_face();
                    self.stop_speaker(Rewarm::Yes).await;
                }
            }
            // Claim confirmations go to the bot actor; the device loop takes them.
            Control::Confirm(_) => {}
            Control::Say(text) => {
                self.state.listening = false;
                self.playing = None;
                if self.online && !self.claiming {
                    self.interrupt(Some(text));
                    self.refresh_face();
                } else {
                    // Refused now rather than queued behind a reconnect or claim.
                    self.interrupt(None);
                    // Leave Listening first: flashes are ignored over active moods.
                    self.refresh_face();
                    self.face.flash(Mood::Sorry, face::SORRY);
                    if self.online {
                        tracing::info!("utterance refused: claim waiting for confirmation");
                    } else {
                        tracing::info!("utterance dropped: offline");
                    }
                }
                // The replacing utterance's own speech request follows, so no
                // warm-up races it. (Offline there is no speaker to stop.)
                self.stop_speaker(Rewarm::No).await;
            }
        }
        if stop_offline {
            self.stop_offline_voice().await;
        }
        self.refresh();
    }

    /// Record an interruption in memory: the live turn becomes a cancel
    /// intent and `text` (if any) replaces the pending utterance. Wake clears
    /// a pending utterance. The disk write follows through `take_persist`.
    fn interrupt(&mut self, text: Option<String>) {
        self.abandon_attempt();
        let cache = &mut self.state.cache;
        let mut changed = cache.pending.is_some() || text.is_some();
        if let Some(turn) = cache.active_turn.take() {
            changed = true;
            queue_cancel(cache, turn);
        }
        self.live_turn = None;
        self.voice = None;
        self.busy = None;
        cache.pending = text.map(|text| Pending {
            request_id: auth::random(),
            text,
            created: auth::now(),
        });
        self.pending_born = cache.pending.as_ref().map(|_| Instant::now());
        if changed {
            self.persist_wanted = true;
            self.persist_retry_at = None;
        }
    }

    async fn stop_speaker(&mut self, rewarm: Rewarm) {
        if let Some(out) = self.out.as_mut() {
            out.speaker.stop(rewarm).await;
        }
        self.adopt_next_speaker();
    }

    async fn stop_offline_voice(&mut self) {
        if let Some(voice) = self.offline_voice.take() {
            voice.stop().await;
        }
    }

    fn adopt_next_speaker(&mut self) {
        if self.playing.is_none()
            && let Some(next) = self.next_out.take()
        {
            self.out = Some(next);
        }
    }

    /// Start the disk write for the latest interruption, if one is due. The
    /// future borrows nothing; hand its output to [`Self::persisted`].
    pub fn take_persist(
        &mut self,
        now: Instant,
    ) -> Option<impl Future<Output = Persisted> + 'static> {
        if !self.persist_wanted
            || self.persist_inflight
            || self.persist_retry_at.is_some_and(|at| now < at)
        {
            return None;
        }
        self.persist_wanted = false;
        self.persist_inflight = true;
        self.persist_retry_at = None;
        let request_id = self
            .state
            .cache
            .pending
            .as_ref()
            .map(|p| p.request_id.clone());
        let write = self.store.flush_later(&self.state.cache);
        Some(async move {
            Persisted {
                request_id,
                result: write.await,
            }
        })
    }

    /// A failed write keeps the intent in memory and retries; the speaker
    /// and face already reacted, and nothing is submitted until it lands.
    pub fn persisted(&mut self, done: Persisted, now: Instant) {
        self.persist_inflight = false;
        match done.result {
            Ok(()) => {
                if done.request_id.is_some()
                    && self.state.cache.pending.as_ref().map(|p| &p.request_id)
                        == done.request_id.as_ref()
                {
                    self.durable = done.request_id;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not persist the interruption; retrying");
                self.persist_wanted = true;
                self.persist_retry_at = Some(now + PERSIST_RETRY);
            }
        }
    }

    /// Drop the pending utterance once it is too old to answer (one being
    /// submitted waits for the server's answer instead), and stop a speech
    /// request that stayed silent past [`FIRST_AUDIO`].
    pub async fn expire(&mut self, now: Instant) {
        if self.rest_deadline().is_some_and(|at| now >= at) {
            self.rest = Some(Mood::Resting);
            self.rest_after = None;
            self.express_neck("sleepy", 0.9);
            self.refresh();
        }
        self.silent_speech(now).await;
        let Some(born) = self.pending_born else {
            return;
        };
        let submitting = matches!(&self.main, Some(Main::Submit(id))
            if self.state.cache.pending.as_ref().is_some_and(|p| p.request_id == *id));
        if now >= born + PENDING_TTL && !submitting {
            self.drop_pending("expired");
            self.refresh();
        }
    }

    /// The server has audio but the speaker played none of it in time: stop
    /// that request, and start the one retry if it is still unused.
    async fn silent_speech(&mut self, now: Instant) {
        let Some(voice) = self.voice.as_mut() else {
            return;
        };
        if voice.first_audio_by.is_none_or(|by| now < by) || voice.heard {
            return;
        }
        voice.first_audio_by = None;
        let retry = !voice.retried;
        if self.playing.take().is_none() {
            return;
        }
        if retry {
            tracing::info!("no audio after speech.ready; retrying once");
            self.stop_speaker(Rewarm::No).await;
            self.retry_speech();
        } else {
            tracing::warn!("no audio after the retry; staying silent");
            self.stop_speaker(Rewarm::Yes).await;
        }
        self.refresh();
    }

    /// The next time [`Self::expire`] or [`Self::take_persist`] has work.
    pub fn deadline(&self) -> Option<Instant> {
        let expiry = self.pending_born.map(|born| born + PENDING_TTL);
        let retry = self
            .persist_retry_at
            .filter(|_| self.persist_wanted && !self.persist_inflight);
        let audio = self.voice.as_ref().and_then(|v| v.first_audio_by);
        expiry
            .into_iter()
            .chain(retry)
            .chain(audio)
            .chain(self.rest_deadline())
            .min()
    }

    fn can_rest(&self) -> bool {
        self.online
            && self.rest.is_none()
            && !self.is_live()
            && self.state.cache.pending.is_none()
            && !self.claiming
            && !self.state.muted
            && !self.state.listening
            && self.offline_voice.is_none()
    }

    fn rest_deadline(&self) -> Option<Instant> {
        self.can_rest().then_some(self.rest_after).flatten()
    }

    /// The pending utterance is being given up. If a submit for it went out
    /// and its answer is unknown, it may have started a turn: remember its
    /// request ID until the server confirms the cancellation.
    fn abandon_attempt(&mut self) {
        let cache = &mut self.state.cache;
        if let Some(pending) = &cache.pending
            && self.attempted.as_ref() == Some(&pending.request_id)
        {
            cache.cancel_request(pending.request_id.clone());
            self.persist_wanted = true;
            self.persist_retry_at = None;
        }
        self.attempted = None;
    }

    fn drop_pending(&mut self, reason: &'static str) {
        // Only expiry gives up an utterance whose submit may be unanswered;
        // the other reasons are the server's own answers.
        if reason == "expired" {
            self.abandon_attempt();
        }
        self.attempted = None;
        if self.state.cache.pending.take().is_some() {
            tracing::info!(reason, "utterance dropped");
            self.face.flash(Mood::Sorry, face::SORRY);
            self.persist_wanted = true;
            self.persist_retry_at = None;
        }
        self.pending_born = None;
        self.busy = None;
    }

    /// The next cancel to send, if any. Cancels have their own lane so a
    /// stalled submit or replay never holds one back.
    pub fn next_cancel(&mut self) -> Option<Op> {
        if !self.online || self.cancelling.is_some() {
            return None;
        }
        let cache = &self.state.cache;
        if let Some(turn) = cache.cancel.clone() {
            self.cancelling = Some(turn.clone());
            return Some(Op::Cancel(turn));
        }
        let request = cache.cancel_requests.first()?.clone();
        self.cancelling = Some(request.clone());
        Some(Op::CancelRequest(request))
    }

    /// The next submit or replay page. A submit waits until the utterance is
    /// on disk and the interrupted (or busy) turn is cancelled.
    pub fn next_main(&mut self, owned: bool) -> Option<Op> {
        if !self.online || self.main.is_some() {
            return None;
        }
        if self.submit_ready(owned)
            && let Some(pending) = self.state.cache.pending.clone()
        {
            self.main = Some(Main::Submit(pending.request_id.clone()));
            self.attempted = Some(pending.request_id.clone());
            return Some(Op::Submit(pending));
        }
        if self.needs_replay {
            self.main = Some(Main::Replay);
            return Some(Op::Replay {
                after: self.state.cache.cursor,
                waiting: self.waiting(),
            });
        }
        None
    }

    /// Apply a finished call. `Err` means the connection must be replaced.
    pub async fn finish(&mut self, done: Done) -> Result<(), Lost> {
        let result = match done {
            Done::Cancelled(turn, result) => {
                self.cancelling = None;
                self.cancelled(&turn, result)
            }
            Done::CancelledRequest(request, result) => {
                self.cancelling = None;
                self.cancelled_request(&request, result)
            }
            Done::Submitted(sent, result) => {
                self.main = None;
                self.submitted(&sent, result)
            }
            Done::Replayed(result) => {
                self.main = None;
                match result {
                    Ok(events) => {
                        self.needs_replay = !events.is_empty();
                        for event in events {
                            if event.sequence > self.state.cache.cursor {
                                self.apply(event).await;
                            }
                        }
                        Ok(())
                    }
                    Err(error) => Err(Lost(format!("replay {error}"))),
                }
            }
        };
        self.refresh();
        result
    }

    fn cancelled(&mut self, turn: &str, result: Result<(), CallError>) -> Result<(), Lost> {
        match result {
            Ok(()) => {}
            Err(CallError::Rejected(error)) => {
                tracing::warn!(%error, "cancel refused; not retrying");
            }
            Err(error) => return Err(Lost(format!("cancel {error}"))),
        }
        let cache = &mut self.state.cache;
        if cache.cancel.as_deref() == Some(turn) {
            cache.cancel = None;
            self.store.checkpoint(cache, false);
        }
        Ok(())
    }

    /// Old servers refuse the command outright; nothing more can be done.
    fn cancelled_request(
        &mut self,
        request: &str,
        result: Result<(), CallError>,
    ) -> Result<(), Lost> {
        match result {
            Ok(()) => {}
            Err(CallError::Rejected(error)) => {
                tracing::warn!(%error, "request cancel refused; not retrying");
            }
            Err(error) => return Err(Lost(format!("request cancel {error}"))),
        }
        if self.state.cache.cancelled_request(request) {
            self.store.checkpoint(&self.state.cache, false);
        }
        Ok(())
    }

    fn submitted(
        &mut self,
        sent: &Pending,
        result: Result<Submitted, CallError>,
    ) -> Result<(), Lost> {
        // A busy or refused submit settles that this request started no turn.
        if matches!(result, Err(CallError::Busy(_) | CallError::Rejected(_)))
            && self.state.cache.cancelled_request(&sent.request_id)
        {
            self.store.checkpoint(&self.state.cache, false);
        }
        let current = self
            .state
            .cache
            .pending
            .as_ref()
            .is_some_and(|p| p.request_id == sent.request_id);
        // Error text is not logged: nothing about an utterance reaches the journal.
        let submitted = match result {
            Ok(submitted) => submitted,
            Err(CallError::Uncertain(_)) => {
                // Kept: the next connection resubmits the same request ID and
                // the server deduplicates it.
                return Err(Lost("submit unanswered".into()));
            }
            Err(CallError::Busy(turn)) if current => {
                if self.busy.as_ref() == Some(&sent.request_id) {
                    self.drop_pending("busy");
                } else {
                    // A turn this robot no longer tracks (from before a reboot,
                    // or a second interruption) is open. The server keeps one
                    // open turn, so cancelling it makes room; the submit waits
                    // for that cancel.
                    tracing::info!("server busy; cancelling its open turn");
                    self.busy = Some(sent.request_id.clone());
                    queue_cancel(&mut self.state.cache, turn);
                    self.store.checkpoint(&self.state.cache, true);
                }
                return Ok(());
            }
            Err(CallError::Rejected(_)) if current => {
                self.drop_pending("refused");
                return Ok(());
            }
            Err(CallError::Rejected(_) | CallError::Busy(_)) => return Ok(()),
        };
        let cache = &mut self.state.cache;
        if current {
            cache.pending = None;
            self.pending_born = None;
            self.busy = None;
            self.attempted = None;
            if submitted.cancelled {
                tracing::info!("submit already cancelled");
            } else {
                cache.active_turn = Some(submitted.turn_id.clone());
                if !submitted.duplicate {
                    self.live_turn = Some(submitted.turn_id.clone());
                    self.voice = Some(Voice {
                        turn: submitted.turn_id.clone(),
                        ready: false,
                        heard: false,
                        retried: false,
                        first_audio_by: None,
                    });
                    // The server waits for the first chunk, so ask for audio now.
                    self.speak(&submitted.turn_id);
                }
            }
        } else if submitted.cancelled {
            // A prior CancelRequest already cancelled this turn on the server.
            cache.cancelled_request(&sent.request_id);
        } else {
            // Wake or a newer utterance replaced this one while it was in
            // flight: its answer must not play. Cancelled by turn ID when that
            // slot is free; otherwise its request ID stays queued instead.
            tracing::info!("superseded turn cancelled");
            if cache.cancel.is_none() || cache.cancel.as_ref() == Some(&submitted.turn_id) {
                cache.cancelled_request(&sent.request_id);
                queue_cancel(cache, submitted.turn_id);
            } else {
                cache.cancel_request(sent.request_id.clone());
            }
        }
        self.store.checkpoint(&self.state.cache, true);
        Ok(())
    }

    fn speak(&mut self, turn_id: &str) {
        if self.state.muted {
            return;
        }
        let Some(out) = self.out.as_mut() else {
            return;
        };
        self.playing = out.speaker.start(turn_id).map(|generation| Playing {
            generation,
            audible: false,
        });
    }

    /// An event pushed by the subscription: apply it when it is the next
    /// sequence, ignore it when already applied, replay on a gap.
    pub async fn pushed(&mut self, event: OutputEvent) {
        let cursor = self.state.cache.cursor;
        if event.sequence <= cursor {
            return;
        }
        if event.sequence == cursor + 1 {
            if self.online
                && !self.state.muted
                && !self.state.listening
                && self.live_turn.as_deref() == Some(event.turn_id.as_str())
                && let Some(cue) = event.expression()
            {
                crate::hardware::apply(self.face, self.peripherals.motion.as_ref(), cue);
            }
            self.apply(event).await;
        } else {
            self.needs_replay = true;
        }
    }

    async fn apply(&mut self, event: OutputEvent) {
        self.journal.record(&event);
        let kind = event.kind.as_str();
        let terminal = TERMINAL.contains(&kind);
        if kind == "speech.ready"
            && event.payload["index"] == 0
            && let Some(voice) = self.voice.as_mut().filter(|v| v.turn == event.turn_id)
        {
            voice.ready = true;
            // The request already out must now produce audio promptly.
            if self.playing.as_ref().is_some_and(|p| !p.audible) && !voice.heard {
                voice.first_audio_by = Some(Instant::now() + FIRST_AUDIO);
            }
            self.retry_speech();
        }
        if self.live_turn.as_deref() == Some(event.turn_id.as_str()) {
            match kind {
                "turn.completed" => self.live_turn = None,
                "turn.cancelled" | "turn.failed" | "turn.interrupted" => {
                    self.end_cue();
                    self.live_turn = None;
                    self.voice = None;
                    self.playing = None;
                    if kind != "turn.cancelled" {
                        self.face.flash(Mood::Sorry, face::SORRY);
                    }
                    self.stop_speaker(Rewarm::Yes).await;
                }
                _ => {}
            }
        }
        let cache = &mut self.state.cache;
        if terminal && cache.active_turn.as_deref() == Some(event.turn_id.as_str()) {
            cache.active_turn = None;
        }
        cache.cursor = event.sequence;
        self.store.checkpoint(cache, terminal);
        self.refresh();
    }

    /// The early speech request normally covers the turn. Start once more
    /// when the server has audio but the speaker got none, whichever of
    /// `speech.ready` and the silent end of the first request comes first.
    fn retry_speech(&mut self) {
        let Some(voice) = self.voice.as_mut() else {
            return;
        };
        if self.playing.is_none() && voice.ready && !voice.heard && !voice.retried {
            voice.retried = true;
            let turn = voice.turn.clone();
            self.speak(&turn);
            if let Some(voice) = self.voice.as_mut() {
                voice.first_audio_by = self.playing.as_ref().map(|_| Instant::now() + FIRST_AUDIO);
            }
        }
    }

    /// The next event from the current speaker. Pends without one.
    #[cfg(test)]
    pub async fn next_playback(&mut self) -> PlaybackEvent {
        if let Some(out) = self.out.as_mut()
            && let Some(event) = out.events.recv().await
        {
            return event;
        }
        std::future::pending().await
    }

    pub fn playback(&mut self, event: PlaybackEvent) {
        match event {
            PlaybackEvent::Started { generation } => {
                if let Some(playing) = self.playing.as_mut()
                    && playing.generation == generation
                {
                    playing.audible = true;
                    if let Some(voice) = self.voice.as_mut() {
                        voice.heard = true;
                        voice.first_audio_by = None;
                    }
                }
            }
            PlaybackEvent::Finished { generation, played } => {
                if self
                    .playing
                    .as_ref()
                    .is_some_and(|p| p.generation == generation)
                {
                    self.playing = None;
                    if let Some(voice) = self.voice.as_mut() {
                        voice.heard |= played;
                        voice.first_audio_by = None;
                    }
                    self.adopt_next_speaker();
                    self.retry_speech();
                }
            }
        }
        self.refresh();
    }

    /// The next event from the current speaker or offline announcement.
    pub async fn next_audio(&mut self) -> AudioEvent {
        let out = &mut self.out;
        let offline = &mut self.offline_voice;
        tokio::select! {
            biased;
            event = async {
                if let Some(out) = out.as_mut()
                    && let Some(event) = out.events.recv().await
                {
                    return event;
                }
                std::future::pending().await
            } => AudioEvent::Playback(event),
            () = async {
                if let Some(voice) = offline.as_mut() {
                    voice.finished().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => AudioEvent::OfflineVoiceFinished,
        }
    }

    pub fn audio(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Playback(event) => self.playback(event),
            AudioEvent::OfflineVoiceFinished => self.finish_offline_voice(),
        }
    }

    /// Resolves when the running offline announcement finishes on its own.
    #[cfg(test)]
    pub async fn next_offline_voice(&mut self) {
        if let Some(voice) = self.offline_voice.as_mut() {
            voice.finished().await;
        } else {
            std::future::pending::<()>().await;
        }
    }

    /// Clear a finished offline announcement and republish `listen::State`.
    pub fn finish_offline_voice(&mut self) {
        self.offline_voice = None;
        self.refresh();
    }

    /// A new session is up: speak through its speaker and catch up.
    pub async fn go_online(&mut self, speaker: S, events: mpsc::Receiver<PlaybackEvent>) {
        self.stop_offline_voice().await;
        self.online = true;
        self.out = Some(Out { speaker, events });
        self.next_out = None;
        self.needs_replay = true;
        self.refresh();
    }

    /// Credentials rotated. Audio that is playing finishes on the old
    /// speaker; the new one takes over when it ends.
    pub fn rotate(&mut self, speaker: S, events: mpsc::Receiver<PlaybackEvent>) {
        self.needs_replay = true;
        let out = Out { speaker, events };
        if self.playing.is_some() {
            self.next_out = Some(out);
        } else {
            self.out = Some(out);
        }
    }

    /// The connection is gone. Recorded intents stay for the next one.
    pub async fn go_offline(&mut self) {
        self.end_cue();
        let announce = self.online && !self.state.muted && !self.state.listening;
        self.online = false;
        self.playing = None;
        self.live_turn = None;
        self.voice = None;
        self.cancelling = None;
        self.main = None;
        self.rest_after = None;
        if self.rest == Some(Mood::Resting) {
            self.rest = None;
        }
        self.refresh_face();
        if let Some(mut out) = self.out.take() {
            out.speaker.stop(Rewarm::No).await;
        }
        self.next_out = None;
        self.stop_offline_voice().await;
        if announce && let Some(voice) = self.peripherals.piper.clone() {
            self.offline_voice = Some(voice.start());
        }
        self.refresh();
    }

    pub async fn shutdown(&mut self) {
        self.online = false;
        self.playing = None;
        self.rest_after = None;
        self.face.set(Mood::Offline);
        if let Some(mut out) = self.out.take() {
            out.speaker.stop(Rewarm::No).await;
        }
        self.next_out = None;
        self.stop_offline_voice().await;
        self.refresh_listen();
    }

    fn refresh(&mut self) {
        if self.can_rest() {
            if self.rest_after.is_none() {
                self.rest_after = Some(Instant::now() + REST_AFTER);
            }
        } else {
            self.rest_after = None;
        }
        self.refresh_listen();
        self.refresh_face();
    }

    fn refresh_listen(&self) {
        let speaking = self.playing.as_ref().is_some_and(|p| p.audible);
        let thinking = self.online && (self.is_live() || self.state.cache.pending.is_some());
        // A claim waiting for the spoken code has its own face. Like Thinking
        // it still shows the apology for a refused code.
        let confirming = self.online && self.claiming;
        if let Some(listen) = &self.peripherals.listen {
            let state = crate::listen::State {
                busy: thinking || speaking || self.offline_voice.is_some(),
                confirming,
                muted: self.state.muted,
            };
            listen.send_if_modified(|old| {
                if *old == state {
                    false
                } else {
                    *old = state;
                    true
                }
            });
        }
    }

    fn refresh_face(&self) {
        let speaking = self.playing.as_ref().is_some_and(|p| p.audible);
        let thinking = self.online && (self.is_live() || self.state.cache.pending.is_some());
        let confirming = self.online && self.claiming;
        // Mute, listening and audio already playing show without a
        // connection; only an otherwise idle robot reads Offline.
        let connected = self.online || self.state.muted || self.state.listening || speaking;
        let mood = face::mood(Signals {
            connected,
            muted: self.state.muted,
            listening: self.state.listening,
            speaking,
            thinking,
            confirming,
        });
        self.face.set(if mood == Mood::Idle {
            self.rest.unwrap_or(mood)
        } else {
            mood
        });
    }

    fn express_neck(&self, emotion: &str, intensity: f32) {
        #[cfg(test)]
        if let Some(log) = &self.neck_log {
            log.push(format!("neck express {emotion} {intensity}"));
        }
        if let Some(neck) = &self.peripherals.motion {
            neck.express(emotion, intensity);
        }
    }

    fn end_cue(&self) {
        self.face.clear_expression();
        #[cfg(test)]
        if let Some(log) = &self.neck_log {
            log.push("neck gesture none 0".into());
        }
        if let Some(neck) = &self.peripherals.motion {
            neck.gesture("none", 0.);
        }
        self.express_neck("neutral", 0.);
    }
}

/// One cancel intent is kept. A second interrupted turn (rare: it needs a
/// cancel still unsent) is named by the server's busy refusal of the next
/// submit, and cancelled then.
fn queue_cancel(cache: &mut Cache, turn: String) {
    if cache.cancel.is_none() {
        cache.cancel = Some(turn);
    } else if cache.cancel.as_deref() != Some(turn.as_str()) {
        tracing::debug!("cancel already queued; a busy refusal names the other turn");
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        collections::{HashMap, VecDeque},
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    pub struct Log(pub Arc<Mutex<Vec<String>>>);
    impl Log {
        pub fn push(&self, entry: String) {
            self.0.lock().unwrap().push(entry);
        }
        pub fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
        pub fn position(&self, prefix: &str) -> usize {
            self.entries()
                .iter()
                .position(|e| e.starts_with(prefix))
                .unwrap_or_else(|| panic!("missing {prefix}: {:?}", self.entries()))
        }
        pub fn last_position(&self, prefix: &str) -> usize {
            self.entries()
                .iter()
                .rposition(|e| e.starts_with(prefix))
                .unwrap_or_else(|| panic!("missing {prefix}: {:?}", self.entries()))
        }
        pub fn count(&self, prefix: &str) -> usize {
            self.entries()
                .iter()
                .filter(|e| e.starts_with(prefix))
                .count()
        }
    }

    /// A conversation server in memory: deduplicates request IDs and
    /// refuses a submit while a turn is open, like the real one.
    #[derive(Default)]
    pub struct Server {
        pub log: Log,
        pub turns: Mutex<Vec<(String, bool)>>,
        pub requests: Mutex<HashMap<String, String>>,
        pub replies: Mutex<VecDeque<Result<(), CallError>>>,
        pub pages: Mutex<VecDeque<Vec<OutputEvent>>>,
        pub drop_next_ack: Mutex<bool>,
        pub cancel_next_submit: Mutex<bool>,
    }

    impl Server {
        pub fn open(&self, id: &str) {
            self.turns.lock().unwrap().push((id.into(), true));
        }
        fn close(&self, id: &str) {
            for turn in self.turns.lock().unwrap().iter_mut() {
                if turn.0 == id {
                    turn.1 = false;
                }
            }
        }
        fn scripted(&self) -> Option<Result<(), CallError>> {
            self.replies.lock().unwrap().pop_front()
        }
        pub fn submit(&self, request_id: &str, text: &str) -> Result<Submitted, CallError> {
            self.log.push(format!("submit {text} {request_id}"));
            if let Some(Err(error)) = self.scripted() {
                return Err(error);
            }
            if let Some(turn) = self.requests.lock().unwrap().get(request_id) {
                return Ok(Submitted {
                    turn_id: turn.clone(),
                    duplicate: true,
                    cancelled: false,
                });
            }
            if std::mem::take(&mut *self.cancel_next_submit.lock().unwrap()) {
                return Ok(Submitted {
                    turn_id: format!("turn-{text}"),
                    duplicate: false,
                    cancelled: true,
                });
            }
            if let Some(open) = self.turns.lock().unwrap().iter().find(|t| t.1) {
                return Err(CallError::Busy(open.0.clone()));
            }
            let turn = format!("turn-{text}");
            self.open(&turn);
            self.requests
                .lock()
                .unwrap()
                .insert(request_id.into(), turn.clone());
            if std::mem::take(&mut *self.drop_next_ack.lock().unwrap()) {
                return Err(CallError::Uncertain("ack lost".into()));
            }
            Ok(Submitted {
                turn_id: turn,
                duplicate: false,
                cancelled: false,
            })
        }
        pub fn cancel(&self, turn_id: &str) -> Result<(), CallError> {
            self.log.push(format!("cancel {turn_id}"));
            if let Some(Err(error)) = self.scripted() {
                return Err(error);
            }
            self.close(turn_id);
            Ok(())
        }
        pub fn cancel_request(&self, request_id: &str) -> Result<(), CallError> {
            self.log.push(format!("cancel_request {request_id}"));
            if let Some(Err(error)) = self.scripted() {
                return Err(error);
            }
            let turn = self.requests.lock().unwrap().get(request_id).cloned();
            if let Some(turn) = turn {
                self.close(&turn);
            }
            Ok(())
        }
        pub fn replay(&self, after: i64) -> Vec<OutputEvent> {
            self.log.push(format!("replay {after}"));
            self.pages.lock().unwrap().pop_front().unwrap_or_default()
        }
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Server>);
    impl Remote for Fake {
        fn submit(
            &self,
            request_id: &str,
            text: &str,
        ) -> impl Future<Output = Result<Submitted, CallError>> {
            std::future::ready(self.0.submit(request_id, text))
        }
        fn cancel(&self, turn_id: &str) -> impl Future<Output = Result<(), CallError>> {
            std::future::ready(self.0.cancel(turn_id))
        }
        fn cancel_request(&self, request_id: &str) -> impl Future<Output = Result<(), CallError>> {
            std::future::ready(self.0.cancel_request(request_id))
        }
        fn replay(&self, after: i64) -> impl Future<Output = Result<Vec<OutputEvent>, CallError>> {
            std::future::ready(Ok(self.0.replay(after)))
        }
        fn probe(&self) -> impl Future<Output = Result<(), CallError>> {
            std::future::ready(Ok(()))
        }
        fn confirm(&self, _: &str) -> impl Future<Output = anyhow::Result<()>> {
            std::future::ready(Ok(()))
        }
        fn register(&self, _: Claim) -> impl Future<Output = anyhow::Result<Registration>> {
            std::future::ready(Ok(Registration {
                owned: true,
                claim: None,
                confirming: false,
            }))
        }
        fn disconnect(&self) -> impl Future<Output = ()> {
            std::future::ready(())
        }
    }

    pub struct FakeSpeaker {
        pub log: Log,
        pub generation: u64,
    }
    impl Speaker for FakeSpeaker {
        fn start(&mut self, turn_id: &str) -> Option<u64> {
            self.generation += 1;
            self.log.push(format!("start {turn_id}"));
            Some(self.generation)
        }
        async fn stop(&mut self, rewarm: Rewarm) {
            self.log.push(match rewarm {
                Rewarm::Yes => "stop".into(),
                Rewarm::No => "stop cold".into(),
            });
        }
    }

    /// A store whose writes are logged (and can be made to fail).
    pub fn store(log: &Log, fail: Arc<Mutex<usize>>) -> CacheStore {
        let log = log.clone();
        CacheStore::spawn_with(
            Arc::new(move |bytes| {
                let cache: Cache = serde_json::from_slice(bytes).unwrap();
                let mut fail = fail.lock().unwrap();
                if *fail > 0 {
                    *fail -= 1;
                    return Err(std::io::Error::other("disk full"));
                }
                log.push(format!(
                    "persist cancel={:?} pending={:?}",
                    cache.cancel,
                    cache.pending.map(|p| p.text)
                ));
                Ok(())
            }),
            Duration::from_secs(1),
        )
    }

    /// The real journal writer, writing nowhere.
    pub fn quiet() -> Journal {
        Journal::to(crate::config::EventDetail::Compact, std::io::sink()).unwrap()
    }

    pub fn event(sequence: i64, turn: &str, kind: &str) -> OutputEvent {
        OutputEvent {
            sequence,
            turn_id: turn.into(),
            kind: kind.into(),
            payload: json!({"index": 0}),
        }
    }

    struct Rig {
        log: Log,
        face: Face,
        store: CacheStore,
        state: RobotState,
        fail: Arc<Mutex<usize>>,
        server: Fake,
    }
    impl Rig {
        fn new() -> Self {
            let log = Log::default();
            let fail = Arc::new(Mutex::new(0));
            Self {
                store: store(&log, fail.clone()),
                face: Face::new().0,
                state: RobotState::default(),
                server: Fake(Arc::new(Server {
                    log: log.clone(),
                    ..Server::default()
                })),
                fail,
                log,
            }
        }
        async fn controller(&mut self) -> Controller<'_, FakeSpeaker> {
            let mut c = Controller::new(
                &self.face,
                &self.store,
                &mut self.state,
                quiet(),
                auth::now(),
            );
            let speaker = FakeSpeaker {
                log: self.log.clone(),
                generation: 0,
            };
            c.go_online(speaker, mpsc::channel(1).1).await;
            c.needs_replay = false;
            c
        }
    }

    /// Run the controller's calls (and disk writes) until it has none left
    /// or the connection is lost.
    async fn drive<S: Speaker>(c: &mut Controller<'_, S>, remote: &Fake) -> Result<(), Lost> {
        loop {
            if let Some(write) = c.take_persist(Instant::now()) {
                let done = write.await;
                c.persisted(done, Instant::now());
                continue;
            }
            let Some(op) = c.next_cancel().or_else(|| c.next_main(true)) else {
                return Ok(());
            };
            let done = op.run(remote.clone()).await;
            c.finish(done).await?;
        }
    }

    fn starts(log: &Log, turn: &str) -> usize {
        log.count(&format!("start {turn}"))
    }

    #[tokio::test]
    async fn barge_in_silences_first_and_submits_only_after_persisting() {
        let mut rig = Rig::new();
        let log = rig.log.clone();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("first".into())).await;
        drive(&mut c, &remote).await.unwrap();
        assert!(c.is_live());
        c.control(Control::Say("second".into())).await;
        // Silenced before the interruption touched the disk or the network.
        let stop = log.last_position("stop");
        assert_eq!(log.count("cancel"), 0);
        assert_eq!(log.count("persist cancel=Some"), 0);
        // The cancel goes out before the write lands; the submit waits for it.
        let cancel = c.next_cancel().unwrap();
        assert!(matches!(&cancel, Op::Cancel(t) if t == "turn-first"));
        let done = cancel.run(remote.clone()).await;
        c.finish(done).await.unwrap();
        assert!(c.next_main(true).is_none(), "not on disk yet");
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        // The cancel finished before the write, so the write already shows it done.
        let cancelled = log.position("cancel turn-first");
        let persist = log.last_position("persist cancel=None pending=Some(\"second\")");
        let submit = log.position("submit second");
        assert!(
            stop < cancelled && stop < persist && persist < submit,
            "{:?}",
            log.entries()
        );
        assert_eq!(starts(&log, "turn-second"), 1);
    }

    #[tokio::test]
    async fn a_failed_write_keeps_the_utterance_and_blocks_only_its_submit() {
        let mut rig = Rig::new();
        let log = rig.log.clone();
        *rig.fail.lock().unwrap() = 1;
        let remote = rig.server.clone();
        let shown = rig.face.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("hello".into())).await;
        assert_eq!(log.count("stop"), 1, "silenced before any write");
        assert_eq!(shown.steady(), Mood::Thinking);
        let now = Instant::now();
        let write = c.take_persist(now).unwrap();
        c.persisted(write.await, now);
        assert!(
            c.next_main(true).is_none(),
            "never submitted before it is durable"
        );
        assert!(c.take_persist(now).is_none(), "retried after a pause");
        assert_eq!(c.deadline(), Some(now + PERSIST_RETRY));
        assert_eq!(c.cache().pending.as_ref().unwrap().text, "hello");
        let later = now + PERSIST_RETRY;
        let write = c.take_persist(later).unwrap();
        c.persisted(write.await, later);
        drive(&mut c, &remote).await.unwrap();
        assert!(c.is_live());
        drop(c);
        assert_eq!(log.count("submit hello"), 1);
    }

    #[tokio::test]
    async fn an_unanswered_submit_is_retried_with_the_same_id_and_stays_silent() {
        let mut rig = Rig::new();
        *rig.server.0.drop_next_ack.lock().unwrap() = true;
        let remote = rig.server.clone();
        {
            let mut c = rig.controller().await;
            c.control(Control::Say("first".into())).await;
            assert!(drive(&mut c, &remote).await.is_err(), "ack lost");
            c.go_offline().await;
        }
        let pending = rig.state.cache.pending.clone().expect("kept");
        let mut c = rig.controller().await;
        drive(&mut c, &remote).await.unwrap();
        assert!(!c.is_live(), "a deduplicated turn never auto-plays");
        assert_eq!(c.cache().active_turn.as_deref(), Some("turn-first"));
        // The next utterance cancels it first and is heard.
        c.control(Control::Say("next".into())).await;
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        let submits = rig
            .log
            .count(&format!("submit first {}", pending.request_id));
        assert_eq!(submits, 2);
        assert_eq!(starts(&rig.log, "turn-first"), 0);
        assert_eq!(starts(&rig.log, "turn-next"), 1);
        assert!(rig.log.position("cancel turn-first") < rig.log.position("submit next"));
    }

    #[tokio::test]
    async fn speech_starts_at_acknowledgement_only_for_new_turns() {
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("hello".into())).await;
        drive(&mut c, &remote).await.unwrap();
        assert!(c.playing.is_some(), "speech requested before any event");
        drop(c);
        assert_eq!(starts(&rig.log, "turn-hello"), 1);
    }

    #[tokio::test]
    async fn not_owned_robots_keep_the_utterance_until_registration() {
        let mut rig = Rig::new();
        let mut c = rig.controller().await;
        c.control(Control::Say("hi".into())).await;
        let write = c.take_persist(Instant::now()).unwrap();
        c.persisted(write.await, Instant::now());
        assert!(c.next_main(false).is_none());
        assert!(c.cache().pending.is_some());
        assert!(matches!(c.next_main(true), Some(Op::Submit(_))));
    }

    #[tokio::test]
    async fn next_sequence_applies_directly_and_gaps_fall_back_to_replay() {
        let mut rig = Rig::new();
        rig.state.cache.cursor = 10;
        rig.server.0.pages.lock().unwrap().push_back(vec![
            event(12, "t", "text.delta"),
            event(13, "t", "text.delta"),
            event(14, "t", "turn.completed"),
        ]);
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.pushed(event(11, "t", "turn.started")).await;
        assert_eq!(c.cache().cursor, 11);
        assert!(!c.needs_replay);
        c.pushed(event(11, "t", "turn.started")).await; // duplicate push
        c.pushed(event(9, "t", "turn.started")).await; // stale
        assert_eq!(c.cache().cursor, 11);
        c.pushed(event(14, "t", "turn.completed")).await; // gap
        assert_eq!(c.cache().cursor, 11);
        assert!(c.needs_replay);
        drive(&mut c, &remote).await.unwrap();
        assert_eq!(c.cache().cursor, 14);
        assert!(!c.needs_replay);
        drop(c);
        assert_eq!(rig.log.count("replay"), 2);
        assert_eq!(rig.log.position("replay 11"), rig.log.position("replay"));
    }

    #[tokio::test]
    async fn replayed_history_never_auto_speaks() {
        let mut rig = Rig::new();
        rig.server.0.pages.lock().unwrap().push_back(vec![
            event(1, "earlier", "turn.started"),
            event(2, "earlier", "speech.ready"),
            event(3, "earlier", "turn.completed"),
        ]);
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.needs_replay = true;
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        assert_eq!(rig.log.count("start"), 0);
        assert_eq!(rig.state.cache.cursor, 3);
    }

    #[tokio::test]
    async fn the_busy_turn_is_cancelled_by_the_id_the_server_names() {
        let mut rig = Rig::new();
        // A turn this robot no longer tracks.
        rig.server.0.open("before-reboot");
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("new question".into())).await;
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        let log = &rig.log;
        assert_eq!(log.count("cancel"), 1);
        assert!(log.position("cancel before-reboot") < log.last_position("submit new question"));
        assert_eq!(log.count("submit new question"), 2);
        assert_eq!(
            rig.state.cache.active_turn.as_deref(),
            Some("turn-new question")
        );
    }

    #[tokio::test]
    async fn refusals_drop_the_utterance_with_an_apology() {
        // A second busy refusal (the turn would not close), and an untyped
        // refusal (any other error, or a server from before the busy outcome).
        let busy = || Err(CallError::Busy("stuck".into()));
        for (replies, submits) in [
            // Refused, the named turn cancelled, refused again.
            (vec![busy(), Ok(()), busy()], 2),
            (vec![Err(CallError::Rejected("no".into()))], 1),
        ] {
            let mut rig = Rig::new();
            rig.server.0.replies.lock().unwrap().extend(replies);
            let remote = rig.server.clone();
            let shown = rig.face.clone();
            let mut c = rig.controller().await;
            c.control(Control::Say("hi".into())).await;
            drive(&mut c, &remote).await.unwrap();
            assert!(c.cache().pending.is_none());
            assert_eq!(shown.flashing(), Some(Mood::Sorry));
            assert_eq!(shown.steady(), Mood::Idle);
            drop(c);
            assert_eq!(rig.log.count("submit hi"), submits);
        }
    }

    #[tokio::test]
    async fn only_a_stop_into_idle_rewarms_the_speaker() {
        let mut rig = Rig::new();
        let log = rig.log.clone();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("first".into())).await;
        drive(&mut c, &remote).await.unwrap();
        c.control(Control::Say("second".into())).await;
        assert_eq!(
            log.count("stop cold"),
            2,
            "a replacing utterance warms nothing"
        );
        assert_eq!(log.count("stop"), 2);
        c.control(Control::Wake).await;
        c.control(Control::Mute(true)).await;
        assert_eq!(log.count("stop") - log.count("stop cold"), 2);
        c.go_offline().await;
        assert_eq!(log.count("stop cold"), 3);
    }

    #[tokio::test]
    async fn only_a_replay_while_waiting_on_the_turn_has_the_short_deadline() {
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        let replay = |c: &mut Controller<'_, FakeSpeaker>| {
            c.needs_replay = true;
            let op = c.next_main(true);
            c.main = None;
            match op {
                Some(Op::Replay { waiting, .. }) => waiting,
                _ => panic!("replay expected"),
            }
        };
        assert!(!replay(&mut c), "idle");
        c.control(Control::Say("hi".into())).await;
        drive(&mut c, &remote).await.unwrap();
        assert!(replay(&mut c), "submitted, nothing audible");
        c.playback(PlaybackEvent::Started { generation: 1 });
        assert!(!replay(&mut c), "speaking");
        c.playback(PlaybackEvent::Finished {
            generation: 1,
            played: true,
        });
        assert!(replay(&mut c), "audio ended, the turn still open");
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_replay_that_hangs_is_unanswered_after_its_deadline() {
        #[derive(Clone)]
        struct Hung;
        impl Remote for Hung {
            async fn submit(&self, _: &str, _: &str) -> Result<Submitted, CallError> {
                std::future::pending().await
            }
            async fn cancel(&self, _: &str) -> Result<(), CallError> {
                std::future::pending().await
            }
            async fn cancel_request(&self, _: &str) -> Result<(), CallError> {
                std::future::pending().await
            }
            async fn replay(&self, _: i64) -> Result<Vec<OutputEvent>, CallError> {
                std::future::pending().await
            }
            async fn probe(&self) -> Result<(), CallError> {
                std::future::pending().await
            }
            async fn confirm(&self, _: &str) -> anyhow::Result<()> {
                std::future::pending().await
            }
            async fn register(&self, _: Claim) -> anyhow::Result<Registration> {
                std::future::pending().await
            }
            async fn disconnect(&self) {}
        }
        let started = Instant::now();
        let op = Op::Replay {
            after: 0,
            waiting: true,
        };
        let Done::Replayed(Err(CallError::Uncertain(_))) = op.run(Hung).await else {
            panic!("expected an unanswered replay");
        };
        assert_eq!(started.elapsed(), WAITING_REPLAY_DEADLINE);
        let idle = Op::Replay {
            after: 0,
            waiting: false,
        };
        let pending = tokio::time::timeout(Duration::from_mins(1), idle.run(Hung)).await;
        assert!(
            pending.is_err(),
            "an idle replay waits for the call deadline"
        );
    }

    #[tokio::test]
    async fn an_acknowledgement_for_a_replaced_utterance_is_cancelled_silently() {
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("first".into())).await;
        let write = c.take_persist(Instant::now()).unwrap();
        c.persisted(write.await, Instant::now());
        let Some(submit) = c.next_main(true) else {
            panic!("submit expected")
        };
        assert!(!c.accepts_events(), "events wait for the acknowledgement");
        // "Hey Pip" while the submit is in flight.
        c.control(Control::Wake).await;
        assert!(c.cache().pending.is_none(), "wake clears the utterance");
        let done = submit.run(remote.clone()).await;
        c.finish(done).await.unwrap();
        assert!(!c.is_live());
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        assert_eq!(rig.log.count("start"), 0);
        assert_eq!(rig.log.count("cancel turn-first"), 1);
    }

    #[tokio::test]
    async fn only_live_contiguous_cues_for_our_turn_move_the_face() {
        use robot_buddy::protocol::{Emotion, FACE_EXPRESS};
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let face = rig.face.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("hello".into())).await;
        drive(&mut c, &remote).await.unwrap();
        let cue = |seq, turn: &str| {
            let mut e = event(seq, turn, FACE_EXPRESS);
            e.payload = serde_json::json!({"emotion":"love","intensity":0.6,"gesture":"blink"});
            e
        };
        c.apply(cue(1, "turn-hello")).await; // the replay path, even with an actionable body
        c.pushed(cue(3, "turn-hello")).await; // gap
        c.pushed(cue(2, "other")).await;
        assert_eq!(face.override_emotion(), None);
        c.pushed(cue(3, "turn-hello")).await;
        assert_eq!(face.override_emotion(), Some(Emotion::Love));
        c.control(Control::Wake).await;
        c.pushed(cue(3, "turn-hello")).await;
        c.pushed(cue(4, "turn-hello")).await;
        assert_eq!(face.override_emotion(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_rest_and_explicit_sleep_end_on_wake() {
        let mut rig = Rig::new();
        let face = rig.face.clone();
        let mut c = rig.controller().await;
        c.expire(Instant::now() + Duration::from_secs(301)).await;
        assert_eq!(face.steady(), Mood::Resting);
        c.control(Control::Sleep).await;
        assert_eq!(face.steady(), Mood::Asleep);
        c.control(Control::Wake).await;
        assert_eq!(face.steady(), Mood::Listening);
        c.control(Control::Listening(false)).await;
        assert_eq!(face.steady(), Mood::Idle);
    }

    #[tokio::test]
    async fn face_follows_wake_think_speak_and_failure() {
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let shown = rig.face.clone();
        let mut c = rig.controller().await;
        assert_eq!(shown.steady(), Mood::Idle);
        c.control(Control::Wake).await;
        assert_eq!(shown.steady(), Mood::Listening);
        c.control(Control::Listening(false)).await;
        assert_eq!(shown.steady(), Mood::Idle);
        c.control(Control::Say("hi".into())).await;
        assert_eq!(shown.steady(), Mood::Thinking, "before any I/O");
        drive(&mut c, &remote).await.unwrap();
        assert_eq!(shown.steady(), Mood::Thinking);
        c.playback(PlaybackEvent::Started { generation: 1 });
        assert_eq!(shown.steady(), Mood::Speaking);
        c.playback(PlaybackEvent::Finished {
            generation: 1,
            played: true,
        });
        assert_eq!(shown.steady(), Mood::Thinking, "turn not finished yet");
        c.pushed(event(1, "turn-hi", "turn.completed")).await;
        assert_eq!(shown.steady(), Mood::Idle);
        c.control(Control::Say("again".into())).await;
        drive(&mut c, &remote).await.unwrap();
        c.pushed(event(2, "turn-again", "turn.failed")).await;
        assert_eq!(shown.steady(), Mood::Idle);
        assert_eq!(shown.flashing(), Some(Mood::Sorry));
        c.control(Control::Wake).await;
        assert_eq!(shown.steady(), Mood::Listening);
        assert_eq!(shown.flashing(), None, "the Sorry flash ends");
        c.control(Control::Mute(true)).await;
        assert_eq!(shown.steady(), Mood::Muted);
        c.control(Control::Say("quiet".into())).await;
        drive(&mut c, &remote).await.unwrap();
        assert!(c.playing.is_none(), "muted robots do not auto-play");
        c.control(Control::Mute(false)).await;
        assert_eq!(shown.steady(), Mood::Thinking);
    }

    #[tokio::test]
    async fn offline_mute_and_say_answer_at_once() {
        let mut rig = Rig::new();
        let shown = rig.face.clone();
        let mut c = rig.controller().await;
        c.go_offline().await;
        assert_eq!(shown.steady(), Mood::Offline);
        c.control(Control::Mute(true)).await;
        assert_eq!(
            shown.steady(),
            Mood::Muted,
            "mute shows without a connection"
        );
        c.control(Control::Mute(false)).await;
        c.control(Control::Wake).await;
        assert_eq!(shown.steady(), Mood::Listening);
        c.control(Control::Say("anyone there".into())).await;
        assert_eq!(shown.flashing(), Some(Mood::Sorry), "refused, not queued");
        assert_eq!(shown.steady(), Mood::Offline);
        assert!(c.cache().pending.is_none());
        assert!(c.next_main(true).is_none());
    }

    #[tokio::test]
    async fn speech_retries_once_whichever_of_ready_and_silence_comes_first() {
        // speech.ready first, then the early request ends without audio.
        let mut rig = Rig::new();
        let log = rig.log.clone();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("hi".into())).await;
        drive(&mut c, &remote).await.unwrap();
        c.playback(PlaybackEvent::Started { generation: 99 });
        assert_eq!(c.face.steady(), Mood::Thinking, "stale generation ignored");
        c.pushed(event(1, "turn-hi", "speech.ready")).await;
        assert_eq!(starts(&log, "turn-hi"), 1, "audio still on its way");
        c.playback(PlaybackEvent::Finished {
            generation: 1,
            played: false,
        });
        assert_eq!(starts(&log, "turn-hi"), 2, "retried after the silent end");
        c.playback(PlaybackEvent::Finished {
            generation: 2,
            played: false,
        });
        c.pushed(event(2, "turn-hi", "speech.ready")).await;
        assert!(c.playing.is_none(), "only one retry");
        drop(c);
        assert_eq!(starts(&log, "turn-hi"), 2);

        // Silent end first, then speech.ready, after turn.completed.
        let mut rig = Rig::new();
        let log = rig.log.clone();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("yo".into())).await;
        drive(&mut c, &remote).await.unwrap();
        c.playback(PlaybackEvent::Finished {
            generation: 1,
            played: false,
        });
        c.pushed(event(1, "turn-yo", "turn.completed")).await;
        c.pushed(event(2, "turn-yo", "speech.ready")).await;
        drop(c);
        assert_eq!(starts(&log, "turn-yo"), 2);
    }

    #[tokio::test]
    async fn wake_cancels_the_live_turn_and_clears_the_utterance() {
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("tell me a story".into())).await;
        drive(&mut c, &remote).await.unwrap();
        c.control(Control::Say("unsent".into())).await;
        c.control(Control::Wake).await;
        assert!(!c.is_live());
        assert!(c.cache().pending.is_none());
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        assert_eq!(rig.log.count("submit unsent"), 0);
        assert!(rig.log.position("stop") < rig.log.position("cancel turn-tell me a story"));
        assert!(rig.state.cache.cancel.is_none());
    }

    #[tokio::test]
    async fn a_stale_cached_utterance_is_never_submitted() {
        let log = Log::default();
        let store = store(&log, Arc::default());
        let face = Face::new().0;
        let now = auth::now();
        for (created, kept) in [
            (now - 10, true),
            (now - 61, false),
            (0, false),
            (now + 60, false),
        ] {
            let mut state = RobotState::default();
            state.cache.pending = Some(Pending {
                request_id: "r".into(),
                text: "t".into(),
                created,
            });
            let c = Controller::<FakeSpeaker>::new(&face, &store, &mut state, quiet(), now);
            let left = c.deadline().map(|d| d.duration_since(Instant::now()));
            drop(c);
            assert_eq!(state.cache.pending.is_some(), kept, "created {created}");
            if kept {
                assert!(left.unwrap() <= Duration::from_secs(50), "{left:?}");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_utterance_expires_after_its_time_to_live() {
        let mut rig = Rig::new();
        let shown = rig.face.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("hello".into())).await;
        let born = Instant::now();
        c.expire(born + PENDING_TTL - Duration::from_millis(1))
            .await;
        assert!(c.cache().pending.is_some());
        assert_eq!(c.deadline(), Some(born + PENDING_TTL));
        c.expire(born + PENDING_TTL).await;
        assert!(c.cache().pending.is_none());
        assert_eq!(shown.flashing(), Some(Mood::Sorry));
        assert!(
            c.take_persist(Instant::now()).is_some(),
            "the drop is written"
        );
    }

    /// The C1 order: the submit goes out, "Hey Pip" replaces the utterance,
    /// the connection drops before any answer, and the robot reconnects with
    /// nothing new to say. The unanswered request is cancelled by its ID on
    /// the new connection, persisted until then, and nothing is resubmitted.
    #[tokio::test]
    async fn an_unanswered_submit_replaced_by_wake_is_cancelled_by_request_after_reconnect() {
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let request = {
            let mut c = rig.controller().await;
            c.control(Control::Say("first".into())).await;
            let write = c.take_persist(Instant::now()).unwrap();
            c.persisted(write.await, Instant::now());
            let Some(Op::Submit(sent)) = c.next_main(true) else {
                panic!("submit expected");
            };
            // The submit reaches the server; its answer never comes back.
            let _ = remote.0.submit(&sent.request_id, &sent.text);
            c.control(Control::Wake).await;
            assert_eq!(c.cache().cancel_requests, vec![sent.request_id.clone()]);
            assert!(c.cache().pending.is_none());
            // The intent is written before the connection is given up.
            let write = c.take_persist(Instant::now()).unwrap();
            c.persisted(write.await, Instant::now());
            c.go_offline().await;
            sent.request_id
        };
        assert_eq!(rig.state.cache.cancel_requests, vec![request.clone()]);
        let mut c = rig.controller().await;
        drive(&mut c, &remote).await.unwrap();
        assert!(c.cache().cancel_requests.is_empty(), "acknowledged");
        drop(c);
        let log = &rig.log;
        let submit = log.position(&format!("submit first {request}"));
        let cancel = log.position(&format!("cancel_request {request}"));
        assert!(submit < cancel, "{:?}", log.entries());
        assert_eq!(log.count("submit"), 1, "nothing resubmitted");
        assert_eq!(log.count("cancel_request"), 1);
        assert_eq!(log.count("start"), 0, "nothing spoken");
        assert!(
            rig.server.0.turns.lock().unwrap().iter().all(|t| !t.1),
            "the turn it started is closed"
        );
    }

    #[tokio::test]
    async fn a_request_cancel_an_old_server_refuses_is_dropped() {
        let mut rig = Rig::new();
        rig.state.cache.cancel_requests = vec!["r1".into()];
        rig.server
            .0
            .replies
            .lock()
            .unwrap()
            .push_back(Err(CallError::Rejected("unknown variant".into())));
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        drive(&mut c, &remote).await.unwrap();
        assert!(c.cache().cancel_requests.is_empty());
        drop(c);
        assert_eq!(rig.log.count("cancel_request r1"), 1);
    }

    #[tokio::test]
    async fn a_submit_waits_for_request_cancels_and_never_queues_unsent_ones() {
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        // Replaced before its submit went out: nothing to cancel.
        c.control(Control::Say("unsent".into())).await;
        c.control(Control::Say("sent".into())).await;
        assert!(c.cache().cancel_requests.is_empty());
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        assert_eq!(rig.log.count("cancel_request"), 0);
        assert_eq!(rig.log.count("submit sent"), 1);
        // A queued request cancel goes out before the next submit.
        let mut rig = Rig::new();
        rig.state.cache.cancel_requests = vec!["old".into()];
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("next".into())).await;
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        assert!(rig.log.position("cancel_request old") < rig.log.position("submit next"));
    }

    #[tokio::test]
    async fn an_answered_replaced_submit_is_cancelled_by_turn_not_by_request() {
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("first".into())).await;
        let write = c.take_persist(Instant::now()).unwrap();
        c.persisted(write.await, Instant::now());
        let submit = c.next_main(true).unwrap();
        c.control(Control::Wake).await;
        assert_eq!(c.cache().cancel_requests.len(), 1, "unknown until answered");
        let done = submit.run(remote.clone()).await;
        c.finish(done).await.unwrap();
        assert!(
            c.cache().cancel_requests.is_empty(),
            "the answer settles it"
        );
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        assert_eq!(rig.log.count("cancel turn-first"), 1);
        assert_eq!(rig.log.count("cancel_request"), 0);
    }

    /// `speech.ready` arrived but the speaker stays silent: after
    /// [`FIRST_AUDIO`] the request is stopped and retried once, then the
    /// robot gives up quietly.
    #[tokio::test(start_paused = true)]
    async fn silent_speech_after_ready_is_retried_once_at_the_deadline() {
        let mut rig = Rig::new();
        let log = rig.log.clone();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("hi".into())).await;
        drive(&mut c, &remote).await.unwrap();
        assert_eq!(starts(&log, "turn-hi"), 1);
        assert_eq!(
            c.deadline(),
            None,
            "no deadline before the server has audio"
        );
        c.pushed(event(1, "turn-hi", "speech.ready")).await;
        let ready = Instant::now();
        assert_eq!(c.deadline(), Some(ready + FIRST_AUDIO));
        c.expire(ready + FIRST_AUDIO - Duration::from_millis(1))
            .await;
        assert_eq!(starts(&log, "turn-hi"), 1, "still within the deadline");
        let stops = log.count("stop");
        c.expire(ready + FIRST_AUDIO).await;
        assert!(log.count("stop") > stops, "the silent request is stopped");
        assert_eq!(starts(&log, "turn-hi"), 2, "and retried");
        // The stopped request ending later changes nothing.
        c.playback(PlaybackEvent::Finished {
            generation: 1,
            played: false,
        });
        assert!(c.playing.is_some());
        let retried = c.deadline().unwrap();
        c.expire(retried).await;
        assert!(c.playing.is_none(), "gives up after one retry");
        assert_eq!(c.deadline(), None);
        drop(c);
        assert_eq!(starts(&log, "turn-hi"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn audible_speech_clears_the_first_audio_deadline() {
        let mut rig = Rig::new();
        let log = rig.log.clone();
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("hi".into())).await;
        drive(&mut c, &remote).await.unwrap();
        c.pushed(event(1, "turn-hi", "speech.ready")).await;
        c.playback(PlaybackEvent::Started { generation: 1 });
        assert_eq!(c.deadline(), None);
        c.expire(Instant::now() + FIRST_AUDIO * 2).await;
        drop(c);
        assert_eq!(starts(&log, "turn-hi"), 1);
    }

    /// The deadline above against the real speaker and a real socket: the
    /// request still waiting for its headers at the deadline is cancelled (its
    /// connection closes, it reports nothing), the one retry plays, and the
    /// retry's `Started`, sent on its first pipe write, clears the deadline.
    #[tokio::test]
    async fn a_request_silent_past_the_deadline_is_cancelled_and_the_retry_plays() {
        use crate::audio::{Media, Playback, PlayerFactory};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        /// Liveness only; nothing here waits on wall-clock time.
        const GUARD: Duration = Duration::from_secs(10);
        /// Read one request: its header block and `content-length` body.
        async fn request(socket: &mut tokio::net::TcpStream) {
            let mut read = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0, "client left mid-request");
                read.extend_from_slice(&buffer[..n]);
                let Some(end) = read.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&read[..end]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                if read.len() >= end + 4 + length {
                    return;
                }
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let (seen_tx, seen) = tokio::sync::oneshot::channel();
        let (closed_tx, closed) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            // The first request never gets headers; note when the client goes.
            let (mut first, _) = listener.accept().await.unwrap();
            request(&mut first).await;
            let _ = seen_tx.send(());
            let mut buffer = [0u8; 64];
            while let Ok(1..) = first.read(&mut buffer).await {}
            let _ = closed_tx.send(());
            // The retry is answered with audio for the turn.
            let (mut second, _) = listener.accept().await.unwrap();
            request(&mut second).await;
            let pcm = vec![0u8; 4800];
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: audio/pcm;rate=24000;channels=1\r\n\
                 x-audio-encoding: pcm_s16le\r\nx-audio-turn-id: turn-hi\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                pcm.len()
            );
            second.write_all(head.as_bytes()).await.unwrap();
            second.write_all(&pcm).await.unwrap();
            while let Ok(1..) = second.read(&mut buffer).await {}
        });
        let player: PlayerFactory = Arc::new(|_| {
            let mut command = tokio::process::Command::new("cat");
            command.stdout(std::process::Stdio::null());
            command
        });
        let media = Arc::new(Media::for_test(&server, player, Duration::from_secs(2)));
        let (events, playback) = mpsc::channel(8);
        let mut rig = Rig::new();
        let remote = rig.server.clone();
        let mut c = Controller::new(&rig.face, &rig.store, &mut rig.state, quiet(), auth::now());
        c.go_online(Playback::new(media, "s".into(), events), playback)
            .await;
        c.needs_replay = false;
        c.control(Control::Say("hi".into())).await;
        drive(&mut c, &remote).await.unwrap();
        tokio::time::timeout(GUARD, seen).await.unwrap().unwrap();
        c.pushed(event(1, "turn-hi", "speech.ready")).await;
        let by = c.voice.as_ref().and_then(|v| v.first_audio_by);
        assert!(by.is_some(), "speech.ready arms the deadline");
        assert_eq!(c.deadline(), by);
        c.expire(by.unwrap()).await;
        tokio::time::timeout(GUARD, closed)
            .await
            .expect("the silent request's connection is closed")
            .unwrap();
        // Nothing from the cancelled generation: the retry speaks first.
        let started = tokio::time::timeout(GUARD, c.next_playback())
            .await
            .unwrap();
        assert_eq!(started, PlaybackEvent::Started { generation: 2 });
        c.playback(started);
        assert_eq!(c.deadline(), None, "first audio clears the deadline");
        assert_eq!(c.face.steady(), Mood::Speaking);
        let finished = tokio::time::timeout(GUARD, c.next_playback())
            .await
            .unwrap();
        assert_eq!(
            finished,
            PlaybackEvent::Finished {
                generation: 2,
                played: true
            }
        );
        c.playback(finished);
        assert!(c.voice.as_ref().is_some_and(|v| v.retried && v.heard));
        drop(c);
        assert_eq!(
            rig.log.count("submit hi"),
            1,
            "spoken twice, submitted once"
        );
    }

    #[tokio::test]
    async fn a_ready_submit_preempts_a_replay_in_flight() {
        let mut rig = Rig::new();
        let mut c = rig.controller().await;
        c.needs_replay = true;
        assert!(matches!(c.next_main(true), Some(Op::Replay { .. })));
        assert!(!c.submit_waits_on_replay(true), "nothing to submit");
        c.control(Control::Say("now".into())).await;
        assert!(!c.submit_waits_on_replay(true), "not on disk yet");
        let write = c.take_persist(Instant::now()).unwrap();
        c.persisted(write.await, Instant::now());
        assert!(!c.submit_waits_on_replay(false), "not owned");
        assert!(c.submit_waits_on_replay(true));
        c.preempt_replay();
        assert!(c.needs_replay, "the replay runs again afterwards");
        assert!(matches!(c.next_main(true), Some(Op::Submit(p)) if p.text == "now"));
    }

    /// The journal's reader stops (journald or a terminal paused): event
    /// lines are dropped and counted, and "Hey Pip" still stops the speaker.
    #[tokio::test]
    async fn wake_never_waits_on_a_paused_journal_reader() {
        let (reader, writer) = std::io::pipe().unwrap();
        let journal = Journal::to(crate::config::EventDetail::Full, writer).unwrap();
        let mut rig = Rig::new();
        let log = rig.log.clone();
        let mut c = Controller::new(&rig.face, &rig.store, &mut rig.state, journal, auth::now());
        let speaker = FakeSpeaker {
            log: log.clone(),
            generation: 0,
        };
        c.go_online(speaker, mpsc::channel(1).1).await;
        let mut big = event(0, "t", "text.delta");
        big.payload = json!({ "delta": "x".repeat(4096) });
        // Far more than the pipe buffer and the journal queue together hold.
        for sequence in 1..=1_000 {
            big.sequence = sequence;
            c.pushed(big.clone()).await;
        }
        c.control(Control::Wake).await;
        assert_eq!(c.face.steady(), Mood::Listening);
        assert_eq!(log.count("stop"), 1);
        drop(c);
        drop(reader);
    }

    #[tokio::test]
    async fn a_cancelled_submit_reply_does_not_start_speech_or_live_turn() {
        let mut rig = Rig::new();
        *rig.server.0.cancel_next_submit.lock().unwrap() = true;
        let remote = rig.server.clone();
        let shown = rig.face.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("already cancelled".into())).await;
        drive(&mut c, &remote).await.unwrap();
        assert!(!c.is_live(), "must not be tracked as a live turn");
        assert!(c.playing.is_none(), "must not start speech");
        assert!(c.voice.is_none(), "must not await audio");
        assert!(c.cache().pending.is_none());
        assert!(c.cache().active_turn.is_none());
        assert_eq!(shown.steady(), Mood::Idle);
        drop(c);
        assert_eq!(starts(&rig.log, "turn-already cancelled"), 0);
        assert_eq!(rig.log.count("cancel "), 0);
    }

    #[tokio::test]
    async fn a_superseded_cancelled_submit_needs_no_followup_cancel() {
        let mut rig = Rig::new();
        *rig.server.0.cancel_next_submit.lock().unwrap() = true;
        let remote = rig.server.clone();
        let mut c = rig.controller().await;
        c.control(Control::Say("first".into())).await;
        let write = c.take_persist(Instant::now()).unwrap();
        c.persisted(write.await, Instant::now());
        let submit = c.next_main(true).unwrap();
        c.control(Control::Wake).await;
        let done = submit.run(remote.clone()).await;
        c.finish(done).await.unwrap();
        assert!(c.cache().cancel.is_none());
        assert!(c.cache().cancel_requests.is_empty());
        drive(&mut c, &remote).await.unwrap();
        drop(c);
        assert_eq!(rig.log.count("start"), 0);
        assert_eq!(rig.log.count("cancel "), 0);
        assert_eq!(rig.log.count("cancel_request"), 0);
    }

    #[tokio::test]
    async fn busy_stays_true_until_online_speaker_stops_on_wake_sleep_and_offline() {
        use tokio::sync::{Notify, watch};
        struct GatedSpeaker {
            generation: u64,
            entered: Arc<Notify>,
            release: Arc<Notify>,
            busy_seen_during_stop: Arc<Mutex<Vec<bool>>>,
            listen_rx: watch::Receiver<crate::listen::State>,
            face: Face,
            mood_seen_during_stop: Arc<Mutex<Vec<Mood>>>,
        }
        impl Speaker for GatedSpeaker {
            fn start(&mut self, _: &str) -> Option<u64> {
                self.generation += 1;
                Some(self.generation)
            }
            async fn stop(&mut self, _: Rewarm) {
                self.busy_seen_during_stop
                    .lock()
                    .unwrap()
                    .push(self.listen_rx.borrow().busy);
                self.mood_seen_during_stop
                    .lock()
                    .unwrap()
                    .push(self.face.steady());
                self.entered.notify_one();
                self.release.notified().await;
            }
        }
        for (label, expected_mood) in [
            ("wake", Mood::Listening),
            ("sleep", Mood::Asleep),
            ("offline", Mood::Offline),
        ] {
            let mut rig = Rig::new();
            let remote = rig.server.clone();
            let (listen_tx, listen_rx) = watch::channel(crate::listen::State::default());
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let busy_during = Arc::new(Mutex::new(Vec::new()));
            let mood_during = Arc::new(Mutex::new(Vec::new()));
            let mut c =
                Controller::new(&rig.face, &rig.store, &mut rig.state, quiet(), auth::now());
            c.peripherals.listen = Some(listen_tx);
            c.go_online(
                GatedSpeaker {
                    generation: 0,
                    entered: entered.clone(),
                    release: release.clone(),
                    busy_seen_during_stop: busy_during.clone(),
                    listen_rx: listen_rx.clone(),
                    face: rig.face.clone(),
                    mood_seen_during_stop: mood_during.clone(),
                },
                mpsc::channel(1).1,
            )
            .await;
            c.needs_replay = false;
            // Release the stop from Control::Say immediately.
            release.notify_one();
            c.control(Control::Say("hello".into())).await;
            drive(&mut c, &remote).await.unwrap();
            c.playback(PlaybackEvent::Started { generation: 1 });
            assert!(listen_rx.borrow().busy, "{label}: busy while speaking");
            busy_during.lock().unwrap().clear();
            mood_during.lock().unwrap().clear();

            let release_after = release.clone();
            let entered_wait = entered.clone();
            let listen_probe = listen_rx.clone();
            let watcher = tokio::spawn(async move {
                entered_wait.notified().await;
                assert!(
                    listen_probe.borrow().busy,
                    "{label}: busy dropped to false before speaker stop completed"
                );
                release_after.notify_one();
            });
            match label {
                "wake" => c.control(Control::Wake).await,
                "sleep" => c.control(Control::Sleep).await,
                "offline" => c.go_offline().await,
                _ => unreachable!(),
            }
            watcher.await.unwrap();
            assert_eq!(*busy_during.lock().unwrap(), vec![true], "{label}");
            assert_eq!(
                *mood_during.lock().unwrap(),
                vec![expected_mood],
                "{label}: face updates immediately while busy waits for reap"
            );
            assert!(
                !listen_rx.borrow().busy,
                "{label}: busy clears after speaker stop completes"
            );
        }
    }

    #[tokio::test]
    async fn offline_announcement_participates_in_busy_and_reaps_before_wake_or_reconnect() {
        use crate::piper::{OfflineVoice, Processes};
        use robot_buddy::audio::PcmFormat;
        use tokio::sync::watch;

        struct Scripted {
            synth: String,
            player: String,
        }
        impl Processes for Scripted {
            fn synth(&self) -> tokio::process::Command {
                let mut cmd = tokio::process::Command::new("sh");
                cmd.args(["-c", &self.synth]);
                cmd
            }
            fn player(&self, _: PcmFormat) -> tokio::process::Command {
                let mut cmd = tokio::process::Command::new("sh");
                cmd.args(["-c", &self.player])
                    .stdout(std::process::Stdio::null());
                cmd
            }
        }
        let dir = std::env::temp_dir().join(format!("buddy-ctrl-off-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Part A: natural completion flips busy true -> false when finish_offline_voice runs.
        let mut rig = Rig::new();
        let (listen_tx, listen_rx) = watch::channel(crate::listen::State::default());
        let mut c = rig.controller().await;
        c.peripherals.listen = Some(listen_tx);
        c.peripherals.piper = Some(OfflineVoice::for_test(
            22050,
            Arc::new(Scripted {
                synth: "printf '1234'".into(),
                player: "cat >/dev/null".into(),
            }),
        ));
        assert!(!listen_rx.borrow().busy);
        c.go_offline().await;
        assert!(listen_rx.borrow().busy, "offline announcement marks busy");
        tokio::time::timeout(Duration::from_secs(5), c.next_offline_voice())
            .await
            .unwrap();
        c.finish_offline_voice();
        assert!(!listen_rx.borrow().busy, "busy clears after finish");
        drop(c);

        // Part B & C: cancel + reap before returning on Wake and on go_online.
        for mode in ["wake", "online"] {
            let pid_file = dir.join(format!("{mode}.pid"));
            let mut rig = Rig::new();
            let log = rig.log.clone();
            let (listen_tx, listen_rx) = watch::channel(crate::listen::State::default());
            let mut c = rig.controller().await;
            c.peripherals.listen = Some(listen_tx);
            c.peripherals.piper = Some(OfflineVoice::for_test(
                22050,
                Arc::new(Scripted {
                    synth: "printf '1234'".into(),
                    player: format!("echo $$ > '{}'; exec sleep 30", pid_file.display()),
                }),
            ));
            c.go_offline().await;
            assert!(listen_rx.borrow().busy, "{mode}: busy during announcement");
            let deadline = Instant::now() + Duration::from_secs(5);
            let pid: i32 = loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                assert!(
                    Instant::now() < deadline,
                    "{mode}: offline player pid missing"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            if mode == "wake" {
                c.control(Control::Wake).await;
            } else {
                let speaker = FakeSpeaker {
                    log: log.clone(),
                    generation: 0,
                };
                c.go_online(speaker, mpsc::channel(1).1).await;
            }
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(
                !alive.success(),
                "{mode}: offline player pid {pid} still alive"
            );
            assert!(!listen_rx.borrow().busy, "{mode}: busy cleared after reap");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn long_reply_claim_offline_and_mute_reset_the_rest_timer() {
        let mut rig = Rig::new();
        let log = rig.log.clone();
        let remote = rig.server.clone();
        let face = rig.face.clone();
        let mut c = rig.controller().await;

        // 1. Long reply (> 5 min): does not prematurely sleep when playback ends.
        c.control(Control::Say("long story".into())).await;
        drive(&mut c, &remote).await.unwrap();
        c.playback(PlaybackEvent::Started { generation: 1 });
        tokio::time::sleep(Duration::from_mins(6)).await;
        c.pushed(event(1, "turn-long story", "turn.completed"))
            .await;
        c.playback(PlaybackEvent::Finished {
            generation: 1,
            played: true,
        });
        assert_eq!(face.steady(), Mood::Idle);
        c.expire(Instant::now() + Duration::from_secs(1)).await;
        assert_eq!(
            face.steady(),
            Mood::Idle,
            "must not sleep right after long reply"
        );
        c.expire(Instant::now() + REST_AFTER - Duration::from_secs(1))
            .await;
        assert_eq!(face.steady(), Mood::Idle);
        c.expire(Instant::now() + REST_AFTER).await;
        assert_eq!(face.steady(), Mood::Resting);

        // 2. Long claim (> 5 min): returning from claiming resets the rest timer.
        c.set_claiming(true);
        assert_eq!(face.steady(), Mood::Confirming);
        tokio::time::sleep(Duration::from_mins(6)).await;
        c.set_claiming(false);
        assert_eq!(face.steady(), Mood::Idle);
        c.expire(Instant::now() + Duration::from_secs(1)).await;
        assert_eq!(
            face.steady(),
            Mood::Idle,
            "must not sleep right after long claim"
        );
        c.expire(Instant::now() + REST_AFTER).await;
        assert_eq!(face.steady(), Mood::Resting);

        // 3. Long offline (> 5 min): reconnecting resets the rest timer.
        c.go_offline().await;
        assert_eq!(face.steady(), Mood::Offline);
        tokio::time::sleep(Duration::from_mins(6)).await;
        let speaker = FakeSpeaker {
            log: log.clone(),
            generation: 0,
        };
        c.go_online(speaker, mpsc::channel(1).1).await;
        assert_eq!(face.steady(), Mood::Idle);
        c.expire(Instant::now() + Duration::from_secs(1)).await;
        assert_eq!(
            face.steady(),
            Mood::Idle,
            "must not sleep right after long offline"
        );
        c.expire(Instant::now() + REST_AFTER).await;
        assert_eq!(face.steady(), Mood::Resting);

        // 4. Long mute (> 5 min): unmuting resets the rest timer.
        c.control(Control::Mute(true)).await;
        assert_eq!(face.steady(), Mood::Muted);
        tokio::time::sleep(Duration::from_mins(6)).await;
        c.control(Control::Mute(false)).await;
        assert_eq!(face.steady(), Mood::Idle);
        c.expire(Instant::now() + Duration::from_secs(1)).await;
        assert_eq!(
            face.steady(),
            Mood::Idle,
            "must not sleep right after long mute"
        );
        c.expire(Instant::now() + REST_AFTER).await;
        assert_eq!(face.steady(), Mood::Resting);
    }

    #[tokio::test(start_paused = true)]
    async fn listening_true_clears_sleeping_and_resting_neck() {
        let mut rig = Rig::new();
        let face = rig.face.clone();
        let neck_log = Log::default();
        let mut c = rig.controller().await;
        c.peripherals.motion = Some(crate::motion::MotionInput::default());
        c.neck_log = Some(neck_log.clone());

        // From explicit Sleep -> Listening(true) clears sleeping neck.
        c.control(Control::Sleep).await;
        assert_eq!(face.steady(), Mood::Asleep);
        assert_eq!(
            neck_log.entries().last().map(String::as_str),
            Some("neck express asleep 1")
        );
        c.control(Control::Listening(true)).await;
        assert_eq!(face.steady(), Mood::Listening);
        assert_eq!(
            neck_log.entries().last().map(String::as_str),
            Some("neck express neutral 0")
        );
        c.control(Control::Listening(false)).await;
        assert_eq!(face.steady(), Mood::Idle);

        // From idle Resting -> Listening(true) clears sleepy neck.
        c.expire(Instant::now() + REST_AFTER + Duration::from_secs(1))
            .await;
        assert_eq!(face.steady(), Mood::Resting);
        assert_eq!(
            neck_log.entries().last().map(String::as_str),
            Some("neck express sleepy 0.9")
        );
        c.control(Control::Listening(true)).await;
        assert_eq!(face.steady(), Mood::Listening);
        assert_eq!(
            neck_log.entries().last().map(String::as_str),
            Some("neck express neutral 0")
        );
        c.control(Control::Listening(false)).await;
        assert_eq!(face.steady(), Mood::Idle);
    }
}
