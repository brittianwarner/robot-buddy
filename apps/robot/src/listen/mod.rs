//! Hands-free local input. ALSA feeds one selected beam into bounded RMS endpointing;
//! whisper.cpp decodes completed utterances for both transcript wake and dialogue.
//! No temporary recordings, transcripts in logs, or direct network calls.
mod io;
mod policy;
mod vad;

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
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

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
}

impl Config {
    /// An unset/empty `BUDDY_LISTEN_MODEL` leaves the external control input alone.
    pub fn from_env() -> Result<Option<Self>> {
        let Some(model) = std::env::var_os("BUDDY_LISTEN_MODEL").filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let number = |key, default: usize| -> Result<usize> {
            std::env::var_os(key).map_or(Ok(default), |s| {
                s.to_str()
                    .context("non-UTF8 mic setting")?
                    .parse()
                    .with_context(|| format!("invalid {key}"))
            })
        };
        let channels = number("BUDDY_LISTEN_CHANNELS", 1)?;
        let channel = number("BUDDY_LISTEN_CHANNEL", 0)?;
        let barge = number("BUDDY_LISTEN_BARGE_IN", 1)?;
        ensure!(
            (1..=8).contains(&channels) && channel < channels,
            "microphone needs 1..8 channels and a zero-based channel index"
        );
        ensure!(barge <= 1, "BUDDY_LISTEN_BARGE_IN must be 0 or 1");
        let model = PathBuf::from(model);
        ensure!(
            model.is_file(),
            "BUDDY_LISTEN_MODEL must name a local whisper.cpp model file"
        );
        Ok(Some(Self {
            model,
            whisper: std::env::var_os("BUDDY_LISTEN_WHISPER")
                .unwrap_or_else(|| "whisper-cli".into()),
            device: std::env::var_os("BUDDY_LISTEN_DEVICE").unwrap_or_else(|| "default".into()),
            channels,
            channel,
            barge_in: barge == 1,
        }))
    }
}

struct Recording {
    clip: Clip,
    state: State,
}

async fn record(
    mic: &mut impl Microphone,
    state: State,
    clips: &mpsc::Sender<Recording>,
) -> Result<()> {
    let mut vad = Vad::default();
    loop {
        let frame = mic.frame().await?;
        if let Some(clip) = vad.feed(frame, Instant::now(), state.busy) {
            // One clip decoding and one waiting. Never block the mic or accumulate a backlog.
            if let Err(mpsc::error::TrySendError::Closed(_)) =
                clips.try_send(Recording { clip, state })
            {
                return Ok(());
            }
        }
    }
}

async fn capture(cfg: &Config, mut state: watch::Receiver<State>, clips: mpsc::Sender<Recording>) {
    loop {
        let current = *state.borrow_and_update();
        if current.muted || (current.busy && !cfg.barge_in && !current.confirming) {
            if state.changed().await.is_err() {
                return;
            }
            continue;
        }
        let session = async {
            let mut mic = Alsa::open(cfg)?;
            record(&mut mic, current, &clips).await
        };
        tokio::select! {
            biased;
            res = state.changed() => if res.is_err() { return; },
            res = session => {
                if res.is_ok() { return; }
                tracing::warn!("microphone capture unavailable; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        // Dropping the old session kills arecord and purges its kernel/pipe/VAD buffers.
    }
}

async fn face_timer(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t.into()).await,
        None => pending().await,
    }
}

async fn process(
    r: &impl Recognizer,
    mut state: watch::Receiver<State>,
    mut clips: mpsc::Receiver<Recording>,
    out: &mpsc::Sender<Control>,
) {
    let mut current = *state.borrow_and_update();
    let mut since = Instant::now();
    let mut policy = Policy::default();
    loop {
        let record = tokio::select! {
            biased;
            changed = state.changed() => {
                if changed.is_err() { return; }
                let next = *state.borrow_and_update();
                since = Instant::now();
                policy.changed(current, next, since);
                current = next;
                continue;
            },
            () = face_timer(policy.face_deadline) => {
                policy.face_deadline = None;
                if out.send(Control::Listening(false)).await.is_err() { return; }
                continue;
            },
            next = clips.recv() => match next { Some(clip) => clip, None => return },
        };
        if record.state != current || record.clip.started < since {
            continue;
        }
        let text = tokio::select! {
            biased;
            // wait_for/changed would mark the version seen; has_changed at loop entry must
            // still observe this edge. Use a clone exclusively as the cancellation signal.
            () = changed_since(&state) => continue,
            result = r.transcript(&record.clip.pcm) => match result {
                Ok(text) => text,
                Err(_) => { tracing::warn!("local speech recognition failed"); continue; }
            }
        };
        let commands = policy.heard(&text, current, record.clip.started, Instant::now());
        if !commands.is_empty() {
            // A wake+say/stop pair stays contiguous on the shared input channel.
            let permits = tokio::select! {
                biased;
                () = changed_since(&state) => continue,
                result = out.reserve_many(commands.len()) => match result { Ok(p) => p, Err(_) => return },
            };
            for (permit, command) in permits.zip(commands) {
                permit.send(command);
            }
        }
    }
}

async fn changed_since(state: &watch::Receiver<State>) {
    let _ = state.clone().changed().await;
}

/// Owns capture and recognition subprocess lifetimes. Dropping this future, closing the
/// state watch, or dropping the input receiver stops both, even if a helper is stalled.
pub async fn run(cfg: Config, out: mpsc::Sender<Control>, state: watch::Receiver<State>) {
    let (tx, rx) = mpsc::channel(1);
    tokio::select! {
        () = capture(&cfg, state.clone(), tx) => (),
        () = process(&cfg, state, rx, &out) => (),
        () = out.closed() => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listen::vad::FRAME;
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
        async fn transcript(&self, pcm: &[i16]) -> Result<String> {
            assert_eq!(pcm.len(), 52 * FRAME);
            Ok("Hey Pip, what time is it?".into())
        }
    }
    #[tokio::test]
    async fn mock_beam_reaches_existing_control_channel_in_order() {
        let (_state, rx) = watch::channel(State::default());
        let (tx, clips) = mpsc::channel(1);
        let (out, mut controls) = mpsc::channel(2);
        let worker = tokio::spawn(async move { process(&TestStt, rx, clips, &out).await });
        tokio::task::yield_now().await;
        let mic = tokio::spawn(async move { record(&mut TestMic(0), State::default(), &tx).await });
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
}
