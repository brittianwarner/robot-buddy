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
    wake: Option<wake::Models>,
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
        let wake = std::env::var_os("BUDDY_LISTEN_WAKE_DIR")
            .filter(|s| !s.is_empty())
            .map(|dir| {
                let level = |key, default| -> Result<f32> {
                    std::env::var_os(key).map_or(Ok(default), |v| {
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
        Ok(Some(Self {
            model,
            whisper: std::env::var_os("BUDDY_LISTEN_WHISPER")
                .unwrap_or_else(|| "whisper-cli".into()),
            device: std::env::var_os("BUDDY_LISTEN_DEVICE").unwrap_or_else(|| "default".into()),
            channels,
            channel,
            barge_in: barge == 1,
            wake,
        }))
    }
}

struct Recording {
    content: Content,
    // The receiver carries the watch version at capture, including same-value ABA edges.
    generation: watch::Receiver<State>,
}

enum Content {
    Wake(Instant),
    Clip(Clip),
}

async fn record(
    mic: &mut impl Microphone,
    detector: &mut impl wake::WakeDetector,
    mut generation: watch::Receiver<State>,
    clips: &mpsc::Sender<Recording>,
) -> Result<()> {
    let mut vad = Vad::default();
    let mut state = *generation.borrow_and_update();
    loop {
        let frame = mic.frame().await?;
        if generation.has_changed()? {
            state = *generation.borrow_and_update();
            vad = Vad::default();
            detector.reset();
            // This frame straddles a mode change. Keep the mic open so the next complete
            // frame following a barge-in is available without a USB/ALSA reopen delay.
            continue;
        }
        if !state.confirming && detector.fired(&frame, state.busy).await? {
            let _ = clips.try_send(Recording {
                generation: generation.clone(),
                content: Content::Wake(Instant::now()),
            });
        }
        if let Some(clip) = vad.feed(&frame, Instant::now(), state.busy) {
            // One clip decoding and one waiting. Never block the mic or accumulate a backlog.
            if let Err(mpsc::error::TrySendError::Closed(_)) = clips.try_send(Recording {
                content: Content::Clip(clip),
                generation: generation.clone(),
            }) {
                return Ok(());
            }
        }
    }
}

async fn capture(cfg: &Config, mut state: watch::Receiver<State>, clips: mpsc::Sender<Recording>) {
    #[cfg(feature = "onnx-wake")]
    let worker = if let Some(models) = &cfg.wake {
        match wake::Worker::start(&models.dir).await {
            Ok(worker) => Some((worker, models)),
            Err(e) => {
                tracing::warn!(error=%e,"local wake model unavailable");
                return;
            }
        }
    } else {
        None
    };
    loop {
        let current = *state.borrow_and_update();
        if current.muted || (current.busy && !cfg.barge_in && !current.confirming) {
            if state.changed().await.is_err() {
                return;
            }
            continue;
        }
        let generation = state.clone();
        let session = async {
            let mut mic = Alsa::open(cfg)?;
            #[cfg(feature = "onnx-wake")]
            if let Some((engine, models)) = &worker {
                return record(
                    &mut mic,
                    &mut wake::Stream::new(engine, models),
                    generation,
                    &clips,
                )
                .await;
            }
            record(&mut mic, &mut wake::Transcript, generation, &clips).await
        };
        tokio::select! {
            biased;
            closed = async {state.wait_for(|s| s.muted || s.confirming != current.confirming || (s.busy && !cfg.barge_in && !s.confirming)).await.is_err()} => if closed { return; },
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
    acoustic: bool,
    mut state: watch::Receiver<State>,
    mut clips: mpsc::Receiver<Recording>,
    out: &mpsc::Sender<Control>,
) {
    let mut current = *state.borrow_and_update();
    let mut policy = Policy::default();
    loop {
        let record = tokio::select! {
            biased;
            changed = state.changed() => {
                if changed.is_err() { return; }
                let next = *state.borrow_and_update();
                policy.changed(current, next, Instant::now());
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
        if !matches!(record.generation.has_changed(), Ok(false))
            || *record.generation.borrow() != current
        {
            continue;
        }
        let commands = match record.content {
            Content::Wake(at) => {
                if current.confirming || current.muted {
                    continue;
                }
                policy.acoustic_wake(at);
                vec![Control::Wake]
            }
            Content::Clip(clip) => {
                if acoustic && !current.confirming && !policy.acoustic_clip(clip.started) {
                    continue;
                }
                let text = tokio::select! {
                    biased;
                    // wait_for/changed would mark the version seen; has_changed at loop entry must
                    // still observe this edge. Use a clone exclusively as the cancellation signal.
                    () = changed_since(&state) => continue,
                    result = r.transcript(&clip.pcm) => if let Ok(text) = result { text } else { tracing::warn!("local speech recognition failed"); continue; }
                };
                let mut commands = policy.heard(&text, current, clip.started, Instant::now());
                if acoustic {
                    commands.retain(|c| c != &Control::Wake);
                }
                commands
            }
        };
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
        () = process(&cfg, cfg.wake.is_some(), state, rx, &out) => (),
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
        let generation = rx.clone();
        let (tx, clips) = mpsc::channel(1);
        let (out, mut controls) = mpsc::channel(2);
        let worker = tokio::spawn(async move { process(&TestStt, false, rx, clips, &out).await });
        tokio::task::yield_now().await;
        let mic = tokio::spawn(async move {
            record(&mut TestMic(0), &mut wake::Transcript, generation, &tx).await
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
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Stt(Arc<AtomicUsize>);
        impl Recognizer for Stt {
            async fn transcript(&self, pcm: &[i16]) -> Result<String> {
                assert_eq!(pcm, [2]);
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok("what time is it?".into())
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let stt = Stt(calls.clone());
        let (state_tx, rx) = watch::channel(State {
            busy: true,
            ..State::default()
        });
        let busy = rx.clone();
        let (tx, clips) = mpsc::channel(3);
        let (out, mut controls) = mpsc::channel(4);
        let worker = tokio::spawn(async move { process(&stt, true, rx, clips, &out).await });
        tokio::task::yield_now().await;
        let at = Instant::now();
        let clip = |v, generation| Recording {
            generation,
            content: Content::Clip(Clip {
                started: at,
                pcm: vec![v],
            }),
        };
        tx.send(clip(1, busy.clone())).await.unwrap();
        tx.send(Recording {
            generation: busy.clone(),
            content: Content::Wake(at),
        })
        .await
        .unwrap();
        assert_eq!(controls.recv().await, Some(Control::Wake));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        state_tx.send(State::default()).unwrap();
        tx.send(clip(3, busy)).await.unwrap(); // queued before the interruption
        tx.send(clip(2, state_tx.subscribe())).await.unwrap(); // no settle/reopen wait
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
        use std::sync::Arc;
        struct Slow(Arc<tokio::sync::Notify>);
        impl Recognizer for Slow {
            async fn transcript(&self, _: &[i16]) -> Result<String> {
                self.0.notify_one();
                pending().await
            }
        }
        let started = Arc::new(tokio::sync::Notify::new());
        let stt = Slow(started.clone());
        let state = State {
            confirming: true,
            ..State::default()
        };
        let (state_tx, rx) = watch::channel(state);
        let generation = rx.clone();
        let (tx, clips) = mpsc::channel(1);
        let (out, mut controls) = mpsc::channel(4);
        let worker = tokio::spawn(async move { process(&stt, true, rx, clips, &out).await });
        tokio::task::yield_now().await;
        tx.send(Recording {
            generation,
            content: Content::Clip(Clip {
                started: Instant::now(),
                pcm: vec![1],
            }),
        })
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
}
