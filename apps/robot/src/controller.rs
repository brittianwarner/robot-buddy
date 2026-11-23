//! Conversation behaviour on the robot, independent of the Rivet transport.
//!
//! Rules this module enforces:
//! - Events apply in sequence order exactly once per cursor; a pushed event
//!   that is not the next sequence triggers a replay instead.
//! - Barge-in silences the speaker first, records the cancel and the new
//!   utterance durably, and only then talks to the server.
//! - Speech auto-plays only for a turn this connection submitted and the
//!   server acknowledged as new. Replayed or deduplicated turns stay silent.
use crate::{
    audio::{PlaybackEvent, Speaker},
    cache::{Cache, CacheStore, Pending},
    config::EventDetail,
    face::{self, Face, Mood, Signals},
    input::Control,
    journal,
};
use anyhow::Result;
use robot_buddy::protocol::OutputEvent;
use std::future::Future;

pub struct Submitted {
    pub turn_id: String,
    pub duplicate: bool,
}

/// A queued or running turn on the server.
pub struct OpenTurn {
    pub id: String,
    /// Used only to recognise our own submission; never logged.
    pub text: String,
}

/// Conversation actor actions the robot uses.
pub trait Remote {
    fn submit(&self, request_id: &str, text: &str) -> impl Future<Output = Result<Submitted>>;
    fn cancel(&self, turn_id: &str) -> impl Future<Output = Result<()>>;
    fn replay(&self, after: i64) -> impl Future<Output = Result<Vec<OutputEvent>>>;
    fn open_turns(&self) -> impl Future<Output = Result<Vec<OpenTurn>>>;
}

/// Robot state that outlives one connection.
#[derive(Default)]
pub struct RobotState {
    pub cache: Cache,
    pub muted: bool,
    pub listening: bool,
}

const TERMINAL: [&str; 4] = [
    "turn.completed",
    "turn.cancelled",
    "turn.failed",
    "turn.interrupted",
];
/// Most unknown open turns cancelled to make room for a new utterance.
const MAX_FOREIGN_CANCELS: usize = 4;

struct Playing {
    generation: u64,
    audible: bool,
}

pub struct Controller<'a, R, S> {
    remote: R,
    speaker: S,
    face: &'a Face,
    store: &'a CacheStore,
    state: &'a mut RobotState,
    detail: EventDetail,
    /// The turn this connection submitted and has not seen finish.
    live_turn: Option<String>,
    playing: Option<Playing>,
    /// Audio for the live turn has reached the speaker at least once.
    heard: bool,
    speech_retried: bool,
    pub needs_replay: bool,
}

impl<'a, R: Remote, S: Speaker> Controller<'a, R, S> {
    pub fn new(
        remote: R,
        speaker: S,
        face: &'a Face,
        store: &'a CacheStore,
        state: &'a mut RobotState,
        detail: EventDetail,
    ) -> Self {
        let controller = Self {
            remote,
            speaker,
            face,
            store,
            state,
            detail,
            live_turn: None,
            playing: None,
            heard: false,
            speech_retried: false,
            needs_replay: true,
        };
        controller.refresh();
        controller
    }

    pub fn cache_mut(&mut self) -> &mut Cache {
        &mut self.state.cache
    }

    /// A turn is in flight or audio is playing.
    pub fn is_live(&self) -> bool {
        self.live_turn.is_some() || self.playing.is_some()
    }

    pub async fn control(&mut self, control: Control) {
        match control {
            Control::Wake => {
                self.silence().await;
                self.state.listening = true;
                self.record(None).await;
            }
            Control::Listening(on) => self.state.listening = on,
            Control::Mute(on) => {
                self.state.muted = on;
                if on {
                    self.silence().await;
                }
            }
            Control::Say(text) => {
                self.silence().await;
                self.state.listening = false;
                self.record(Some(text)).await;
            }
        }
        self.refresh();
    }

    async fn silence(&mut self) {
        self.playing = None;
        self.speaker.stop().await;
    }

    /// Durably record an interruption (and the replacement utterance, if any)
    /// before any network I/O, so a disconnect cannot lose it.
    async fn record(&mut self, text: Option<String>) {
        let cache = &mut self.state.cache;
        if let Some(turn) = cache.active_turn.take() {
            cache.cancel = Some(turn);
        }
        self.live_turn = None;
        let replacing = text.is_some();
        if let Some(text) = text {
            cache.pending = Some(Pending {
                request_id: robot_buddy::auth::random(),
                text,
            });
        }
        if !replacing && cache.cancel.is_none() {
            return;
        }
        if let Err(error) = self.store.flush(cache).await {
            tracing::warn!(%error, "could not persist the interruption; continuing");
        }
    }

    /// Send recorded intents: the cancel first, then the utterance (only when
    /// `may_submit`, i.e. the robot has an owner). Safe to repeat after a
    /// reconnect: cancelling a finished turn is harmless and the request ID
    /// deduplicates the submission.
    pub async fn send_intents(&mut self, may_submit: bool) -> Result<()> {
        if let Some(turn) = self.state.cache.cancel.clone() {
            self.remote.cancel(&turn).await?;
            self.state.cache.cancel = None;
            self.store.checkpoint(&self.state.cache, false);
        }
        let Some(pending) = self.state.cache.pending.clone().filter(|_| may_submit) else {
            return Ok(());
        };
        let submitted = match self.remote.submit(&pending.request_id, &pending.text).await {
            Ok(submitted) => submitted,
            // Usually "busy": a turn we no longer know about (for example
            // from before a reboot) is still running. Replace it.
            Err(error) => {
                if !self.cancel_unknown_turns(&pending.text).await? {
                    return Err(error);
                }
                self.remote
                    .submit(&pending.request_id, &pending.text)
                    .await?
            }
        };
        self.state.cache.pending = None;
        if !submitted.duplicate {
            self.state.cache.active_turn = Some(submitted.turn_id.clone());
            self.live_turn = Some(submitted.turn_id.clone());
            self.heard = false;
            self.speech_retried = false;
            // The server waits for the first chunk, so ask for audio now.
            self.speak(&submitted.turn_id);
        }
        self.store.checkpoint(&self.state.cache, false);
        self.refresh();
        Ok(())
    }

    async fn cancel_unknown_turns(&mut self, text: &str) -> Result<bool> {
        let open = self.remote.open_turns().await?;
        let mut cancelled = false;
        for turn in open
            .iter()
            .filter(|t| t.text != text)
            .take(MAX_FOREIGN_CANCELS)
        {
            self.remote.cancel(&turn.id).await?;
            cancelled = true;
        }
        Ok(cancelled)
    }

    fn speak(&mut self, turn_id: &str) {
        if self.state.muted {
            return;
        }
        self.playing = self.speaker.start(turn_id).map(|generation| Playing {
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
            self.apply(event).await;
        } else {
            self.needs_replay = true;
        }
    }

    /// Fetch and apply one replay page after the cursor.
    pub async fn replay_page(&mut self) -> Result<()> {
        let events = self.remote.replay(self.state.cache.cursor).await?;
        self.needs_replay = !events.is_empty();
        for event in events {
            if event.sequence > self.state.cache.cursor {
                self.apply(event).await;
            }
        }
        Ok(())
    }

    async fn apply(&mut self, event: OutputEvent) {
        journal::print(&event, self.detail);
        let kind = event.kind.as_str();
        let terminal = TERMINAL.contains(&kind);
        if self.live_turn.as_deref() == Some(event.turn_id.as_str()) {
            match kind {
                "speech.ready" if event.payload["index"] == 0 => self.retry_speech(&event.turn_id),
                "turn.completed" => self.live_turn = None,
                "turn.cancelled" | "turn.failed" | "turn.interrupted" => {
                    self.silence().await;
                    self.live_turn = None;
                    if kind != "turn.cancelled" {
                        self.face.flash(Mood::Sorry, face::SORRY);
                    }
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
    /// only when it already ended without any audio.
    fn retry_speech(&mut self, turn_id: &str) {
        if self.playing.is_none() && !self.heard && !self.speech_retried {
            self.speech_retried = true;
            self.speak(turn_id);
        }
    }

    pub fn playback(&mut self, event: PlaybackEvent) {
        match event {
            PlaybackEvent::Started { generation } => {
                if let Some(playing) = self.playing.as_mut()
                    && playing.generation == generation
                {
                    playing.audible = true;
                    self.heard = true;
                }
            }
            PlaybackEvent::Finished { generation, .. } => {
                if self
                    .playing
                    .as_ref()
                    .is_some_and(|p| p.generation == generation)
                {
                    self.playing = None;
                }
            }
        }
        self.refresh();
    }

    fn refresh(&self) {
        self.face.set(face::mood(Signals {
            connected: true,
            muted: self.state.muted,
            listening: self.state.listening,
            speaking: self.playing.as_ref().is_some_and(|p| p.audible),
            thinking: self.is_live(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use serde_json::json;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);
    impl Log {
        fn push(&self, entry: String) {
            self.0.lock().unwrap().push(entry);
        }
        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
        fn position(&self, prefix: &str) -> usize {
            self.entries()
                .iter()
                .position(|e| e.starts_with(prefix))
                .unwrap_or_else(|| panic!("missing {prefix}: {:?}", self.entries()))
        }
        fn count(&self, prefix: &str) -> usize {
            self.entries()
                .iter()
                .filter(|e| e.starts_with(prefix))
                .count()
        }
    }

    #[derive(Default)]
    struct Fake {
        log: Log,
        submits: Mutex<VecDeque<Result<Submitted>>>,
        cancels_fail: Mutex<usize>,
        pages: Mutex<VecDeque<Vec<OutputEvent>>>,
        open: Vec<OpenTurn>,
    }
    impl Remote for &Fake {
        async fn submit(&self, request_id: &str, text: &str) -> Result<Submitted> {
            self.log.push(format!("submit {text} {request_id}"));
            self.submits.lock().unwrap().pop_front().unwrap_or_else(|| {
                Ok(Submitted {
                    turn_id: format!("turn-{text}"),
                    duplicate: false,
                })
            })
        }
        async fn cancel(&self, turn_id: &str) -> Result<()> {
            self.log.push(format!("cancel {turn_id}"));
            let mut fail = self.cancels_fail.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(anyhow!("disconnected"));
            }
            Ok(())
        }
        async fn replay(&self, after: i64) -> Result<Vec<OutputEvent>> {
            self.log.push(format!("replay {after}"));
            Ok(self.pages.lock().unwrap().pop_front().unwrap_or_default())
        }
        async fn open_turns(&self) -> Result<Vec<OpenTurn>> {
            self.log.push("open_turns".into());
            Ok(self
                .open
                .iter()
                .map(|t| OpenTurn {
                    id: t.id.clone(),
                    text: t.text.clone(),
                })
                .collect())
        }
    }

    struct FakeSpeaker {
        log: Log,
        generation: u64,
    }
    impl Speaker for FakeSpeaker {
        fn start(&mut self, turn_id: &str) -> Option<u64> {
            self.generation += 1;
            self.log.push(format!("start {turn_id}"));
            Some(self.generation)
        }
        async fn stop(&mut self) {
            self.log.push("stop".into());
        }
    }

    fn store(log: &Log) -> CacheStore {
        let log = log.clone();
        CacheStore::spawn_with(
            Arc::new(move |bytes| {
                let cache: Cache = serde_json::from_slice(bytes).unwrap();
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

    fn event(sequence: i64, turn: &str, kind: &str) -> OutputEvent {
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
    }
    impl Rig {
        fn new() -> Self {
            let log = Log::default();
            Self {
                store: store(&log),
                face: Face::new().0,
                state: RobotState::default(),
                log,
            }
        }
        fn controller<'a>(&'a mut self, fake: &'a Fake) -> Controller<'a, &'a Fake, FakeSpeaker> {
            let speaker = FakeSpeaker {
                log: self.log.clone(),
                generation: 0,
            };
            Controller::new(
                fake,
                speaker,
                &self.face,
                &self.store,
                &mut self.state,
                EventDetail::Compact,
            )
        }
    }

    #[tokio::test]
    async fn barge_in_silences_then_persists_then_cancels_then_submits() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            ..Fake::default()
        };
        let mut c = rig.controller(&fake);
        c.control(Control::Say("first".into())).await;
        c.send_intents(true).await.unwrap();
        assert!(c.is_live());
        c.control(Control::Say("second".into())).await;
        c.send_intents(true).await.unwrap();
        let log = rig.log.clone();
        let stop = log
            .0
            .lock()
            .unwrap()
            .iter()
            .rposition(|e| e == "stop")
            .unwrap();
        let persist = log.position("persist cancel=Some(\"turn-first\") pending=Some(\"second\")");
        let cancel = log.position("cancel turn-first");
        let submit = log.position("submit second");
        assert!(
            stop < persist && persist < cancel && cancel < submit,
            "{:?}",
            log.entries()
        );
        assert_eq!(log.count("start turn-second"), 1);
    }

    #[tokio::test]
    async fn replacement_survives_a_disconnect_and_resumes_idempotently() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            cancels_fail: Mutex::new(1),
            ..Fake::default()
        };
        {
            let mut c = rig.controller(&fake);
            c.control(Control::Say("first".into())).await;
            c.send_intents(true).await.unwrap();
            c.control(Control::Say("second".into())).await;
            assert!(
                c.send_intents(true).await.is_err(),
                "cancel lost in disconnect"
            );
        }
        let pending = rig.state.cache.pending.clone().expect("replacement kept");
        assert_eq!(pending.text, "second");
        assert_eq!(rig.state.cache.cancel.as_deref(), Some("turn-first"));
        // Next connection: the same request ID goes out after the cancel.
        let mut c = rig.controller(&fake);
        c.send_intents(true).await.unwrap();
        let entries = rig.log.entries();
        let tail: Vec<_> = entries
            .iter()
            .filter(|e| e.starts_with("cancel") || e.starts_with("submit"))
            .collect();
        assert_eq!(tail.len(), 4, "{tail:?}");
        assert_eq!(tail[2], "cancel turn-first");
        assert_eq!(*tail[3], format!("submit second {}", pending.request_id));
        assert!(rig.state.cache.pending.is_none() && rig.state.cache.cancel.is_none());
    }

    #[tokio::test]
    async fn speech_starts_at_acknowledgement_only_for_new_turns() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            submits: Mutex::new(VecDeque::from([
                Ok(Submitted {
                    turn_id: "old".into(),
                    duplicate: true,
                }),
                Ok(Submitted {
                    turn_id: "new".into(),
                    duplicate: false,
                }),
            ])),
            ..Fake::default()
        };
        let mut c = rig.controller(&fake);
        c.control(Control::Say("again".into())).await;
        c.send_intents(true).await.unwrap();
        c.pushed(event(1, "old", "speech.ready")).await;
        assert_eq!(rig_log_starts(&c), 0, "duplicate turns stay silent");
        c.control(Control::Say("hello".into())).await;
        c.send_intents(true).await.unwrap();
        assert_eq!(rig_log_starts(&c), 1, "speech requested before any event");
        drop(c);
        assert_eq!(rig.log.count("start new"), 1);
    }

    fn rig_log_starts<R, S>(c: &Controller<'_, R, S>) -> usize {
        usize::from(c.playing.is_some())
    }

    #[tokio::test]
    async fn not_owned_robots_keep_the_utterance_until_registration() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            ..Fake::default()
        };
        let mut c = rig.controller(&fake);
        c.control(Control::Say("hi".into())).await;
        c.send_intents(false).await.unwrap();
        assert_eq!(rig.log.count("submit"), 0);
        assert!(rig.state.cache.pending.is_some());
    }

    #[tokio::test]
    async fn next_sequence_applies_directly_and_gaps_fall_back_to_replay() {
        let mut rig = Rig::new();
        rig.state.cache.cursor = 10;
        let fake = Fake {
            log: rig.log.clone(),
            pages: Mutex::new(VecDeque::from([vec![
                event(12, "t", "text.delta"),
                event(13, "t", "text.delta"),
                event(14, "t", "turn.completed"),
            ]])),
            ..Fake::default()
        };
        let mut c = rig.controller(&fake);
        c.needs_replay = false;
        c.pushed(event(11, "t", "turn.started")).await;
        assert_eq!(c.state.cache.cursor, 11);
        assert!(!c.needs_replay);
        c.pushed(event(11, "t", "turn.started")).await; // duplicate push
        c.pushed(event(9, "t", "turn.started")).await; // stale
        assert_eq!(c.state.cache.cursor, 11);
        c.pushed(event(14, "t", "turn.completed")).await; // gap
        assert_eq!(c.state.cache.cursor, 11);
        assert!(c.needs_replay);
        c.replay_page().await.unwrap();
        assert_eq!(c.state.cache.cursor, 14);
        c.replay_page().await.unwrap();
        assert!(!c.needs_replay);
        drop(c);
        assert_eq!(rig.log.count("replay"), 2);
        assert_eq!(rig.log.position("replay 11"), rig.log.position("replay"));
    }

    #[tokio::test]
    async fn replayed_history_never_auto_speaks() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            pages: Mutex::new(VecDeque::from([vec![
                event(1, "earlier", "turn.started"),
                event(2, "earlier", "speech.ready"),
                event(3, "earlier", "turn.completed"),
            ]])),
            ..Fake::default()
        };
        let mut c = rig.controller(&fake);
        c.replay_page().await.unwrap();
        drop(c);
        assert_eq!(rig.log.count("start"), 0);
        assert_eq!(rig.state.cache.cursor, 3);
    }

    #[tokio::test]
    async fn busy_server_turns_are_cancelled_before_resubmitting() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            submits: Mutex::new(VecDeque::from([Err(anyhow!("busy"))])),
            open: vec![
                OpenTurn {
                    id: "before-reboot".into(),
                    text: "older question".into(),
                },
                OpenTurn {
                    id: "maybe-ours".into(),
                    text: "new question".into(),
                },
            ],
            ..Fake::default()
        };
        let mut c = rig.controller(&fake);
        c.control(Control::Say("new question".into())).await;
        c.send_intents(true).await.unwrap();
        drop(c);
        assert_eq!(rig.log.count("cancel"), 1);
        assert!(rig.log.position("cancel before-reboot") < rig.log.entries().len());
        assert_eq!(rig.log.count("submit new question"), 2);
        assert_eq!(
            rig.state.cache.active_turn.as_deref(),
            Some("turn-new question")
        );
    }

    #[tokio::test]
    async fn face_follows_wake_think_speak_and_failure() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            ..Fake::default()
        };
        let face = rig.face.clone();
        let mut c = rig.controller(&fake);
        c.needs_replay = false;
        assert_eq!(face.steady(), Mood::Idle);
        c.control(Control::Wake).await;
        assert_eq!(face.steady(), Mood::Listening);
        c.control(Control::Listening(false)).await;
        assert_eq!(face.steady(), Mood::Idle);
        c.control(Control::Say("hi".into())).await;
        c.send_intents(true).await.unwrap();
        assert_eq!(face.steady(), Mood::Thinking);
        c.playback(PlaybackEvent::Started { generation: 1 });
        assert_eq!(face.steady(), Mood::Speaking);
        c.playback(PlaybackEvent::Finished {
            generation: 1,
            played: true,
        });
        assert_eq!(face.steady(), Mood::Thinking, "turn not finished yet");
        c.pushed(event(1, "turn-hi", "turn.completed")).await;
        assert_eq!(face.steady(), Mood::Idle);
        c.control(Control::Say("again".into())).await;
        c.send_intents(true).await.unwrap();
        c.pushed(event(2, "turn-again", "turn.failed")).await;
        assert_eq!(face.steady(), Mood::Idle);
        assert_eq!(face.flashing(), Some(Mood::Sorry));
        c.control(Control::Mute(true)).await;
        assert_eq!(face.steady(), Mood::Muted);
        c.control(Control::Say("quiet".into())).await;
        c.send_intents(true).await.unwrap();
        assert!(c.playing.is_none(), "muted robots do not auto-play");
        c.control(Control::Mute(false)).await;
        assert_eq!(face.steady(), Mood::Thinking);
    }

    #[tokio::test]
    async fn stale_playback_events_are_ignored_and_speech_retries_once_without_audio() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            ..Fake::default()
        };
        let mut c = rig.controller(&fake);
        c.needs_replay = false;
        c.control(Control::Say("hi".into())).await;
        c.send_intents(true).await.unwrap();
        c.playback(PlaybackEvent::Started { generation: 99 });
        assert_eq!(c.face.steady(), Mood::Thinking);
        // The early request ended with no audio; speech.ready retries once.
        c.playback(PlaybackEvent::Finished {
            generation: 1,
            played: false,
        });
        c.pushed(event(1, "turn-hi", "speech.ready")).await;
        assert!(c.playing.is_some());
        c.playback(PlaybackEvent::Finished {
            generation: 2,
            played: false,
        });
        c.pushed(OutputEvent {
            payload: json!({"index": 0}),
            ..event(2, "turn-hi", "speech.ready")
        })
        .await;
        assert!(c.playing.is_none(), "only one retry");
        drop(c);
        assert_eq!(rig.log.count("start turn-hi"), 2);
    }

    #[tokio::test]
    async fn wake_cancels_the_live_turn_durably() {
        let mut rig = Rig::new();
        let fake = Fake {
            log: rig.log.clone(),
            ..Fake::default()
        };
        let mut c = rig.controller(&fake);
        c.control(Control::Say("tell me a story".into())).await;
        c.send_intents(true).await.unwrap();
        c.control(Control::Wake).await;
        assert!(!c.is_live());
        c.send_intents(true).await.unwrap();
        drop(c);
        let stop = rig
            .log
            .0
            .lock()
            .unwrap()
            .iter()
            .rposition(|e| e == "stop")
            .unwrap();
        let persist = rig
            .log
            .position("persist cancel=Some(\"turn-tell me a story\")");
        let cancel = rig.log.position("cancel turn-tell me a story");
        assert!(stop < persist && persist < cancel);
    }
}
