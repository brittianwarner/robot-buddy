//! Robot Buddy device client: boots the identity, keeps a session to the
//! Bot and Conversation actors, and drives speech, the face and the panel.
mod audio;
mod backoff;
mod cache;
mod config;
mod controller;
mod device;
mod face;
mod input;
mod journal;
mod screen;
mod session;
mod systemd;
mod wifi;

use anyhow::Result;
use audio::Media;
use cache::CacheStore;
use config::Settings;
use controller::RobotState;
use face::Face;
use input::Input;
use robot_buddy::auth::Identity;
use screen::Screen;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

/// Ordinary cursor checkpoints wait this long so a whole turn shares one
/// write; terminal events, interruptions and the stop flush are written at
/// once. The cursor is at least once and replayed events never speak, so a
/// crash re-applies at most this much history, silently. At 1 s a streamed
/// answer cost an SD-card write per second.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);
/// The Pi 5 has four cores and the robot is one loop plus a few helpers;
/// two workers leave the rest to audio and the face.
const WORKER_THREADS: usize = 2;
/// After a stop signal and the final cache flush, how long runtime shutdown
/// may wait for blocking work (a stuck serial write, a DNS lookup) before
/// the process exits anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()?;
    let result = runtime.block_on(robot());
    // Dropping the runtime drops every task: an aborted playback task drops
    // its `aplay` child, which `kill_on_drop` SIGKILLs. Blocking work is not
    // awaited past the grace period.
    let stopping = std::time::Instant::now();
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    tracing::debug!(
        shutdown_ms = stopping.elapsed().as_millis(),
        "runtime stopped"
    );
    result
}

async fn robot() -> Result<()> {
    let stop = shutdown_signal()?;
    let settings = Settings::from_env()?;
    let identity: Identity = serde_json::from_slice(&std::fs::read(&settings.identity_file)?)?;
    let store = CacheStore::spawn(settings.cache_file.clone(), CHECKPOINT_INTERVAL);
    let mut state = RobotState {
        cache: cache::load(&settings.cache_file),
        ..RobotState::default()
    };
    let screen = Screen::new();
    screen.serve().await?;
    let (face, target) = Face::new();
    if let Some(path) = settings.face_serial.clone() {
        tokio::spawn(face::run(target, face::Device(path)));
    }
    let media = Arc::new(Media::new(
        &settings.server,
        settings.speech,
        settings.player.clone(),
    )?);
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let stdin = std::io::BufReader::with_capacity(input::MAX_LINE, std::io::stdin());
    let mut input = Input::spawn(stdin)?;
    // Type=notify: ready once input is served. Under WatchdogSec= the device
    // loop's heartbeat gates the watchdog pings.
    let pulse = systemd::ready();
    // No Wi-Fi gate here: the loop serves input at once, and reopens setup
    // only after a bounded time without any network.
    let link = session::Link {
        http,
        server: settings.server.clone(),
        identity: Arc::new(identity),
        screen,
        media,
        manage_wifi: !settings.simulate,
    };
    let options = device::Options {
        detail: settings.events,
        wifi_grace: settings.wifi_grace,
        pulse,
    };
    let result = device::run(link, &mut input, &face, &store, &mut state, options, stop).await;
    // The loop silenced the speaker and closed the connection. Persist the
    // in-memory cursor and intents, not only the last coalesced checkpoint.
    tracing::info!("stopping");
    let flushing = Instant::now();
    if let Err(error) = store.flush(&state.cache).await {
        tracing::warn!(%error, "final cache flush failed");
    }
    store.shutdown().await;
    tracing::debug!(flush_ms = flushing.elapsed().as_millis(), "cache flushed");
    result
}

/// Register SIGTERM and SIGINT handlers now, so a stop during startup is
/// still a clean stop (flushed cache) rather than the default kill. The
/// returned future resolves on the first signal.
fn shutdown_signal() -> Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
    })
}
