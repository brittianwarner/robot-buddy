//! Robot Buddy device client: boots the identity, keeps a session to the
//! Bot and Conversation actors, and drives speech, the face and the panel.
mod audio;
mod backoff;
mod cache;
mod config;
mod controller;
mod face;
mod input;
mod journal;
mod screen;
mod session;
mod wifi;

use anyhow::{Result, ensure};
use audio::Media;
use backoff::{Backoff, HEALTHY_SESSION, LINK_CHECK_AFTER, LinkAction, LinkWatch};
use cache::CacheStore;
use config::Settings;
use controller::RobotState;
use face::{Face, Mood};
use input::Input;
use robot_buddy::auth::Identity;
use screen::Screen;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

/// Cursor checkpoints are coalesced to at most one write per interval.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
/// How long the setup hotspot stays up when reopened after a Wi-Fi loss.
const SETUP_WINDOW: Duration = Duration::from_mins(10);
/// After a stop signal and the final cache flush, how long runtime shutdown
/// may wait for blocking work (a stuck serial write, a DNS lookup) before
/// the process exits anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
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
    let control = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let stdin = std::io::BufReader::with_capacity(input::MAX_LINE, std::io::stdin());
    let mut input = Input::spawn(stdin)?;
    if !settings.simulate && !wifi::connected().await? {
        wifi::setup(&screen, None).await?;
    }
    let shared = session::Shared {
        server: &settings.server,
        screen: &screen,
        face: &face,
        store: &store,
        media: &media,
        detail: settings.events,
    };
    let stopped = {
        let run = reconnect(
            &settings, &identity, &control, &shared, &mut state, &mut input,
        );
        tokio::select! {
            result = run => Some(result),
            () = shutdown_signal() => None,
        }
    };
    // The session future is gone: its `Playback` aborted any player task.
    if let Some(result) = stopped {
        return result;
    }
    tracing::info!("stopping");
    face.set(Mood::Offline);
    // Persist the in-memory replay cursor, not only the last coalesced checkpoint.
    let flushing = Instant::now();
    if let Err(error) = store.flush(&state.cache).await {
        tracing::warn!(%error, "final cache flush failed");
    }
    store.shutdown().await;
    tracing::debug!(flush_ms = flushing.elapsed().as_millis(), "cache flushed");
    Ok(())
}

/// Connect, run a session, and reconnect with backoff for as long as the
/// process lives.
async fn reconnect(
    settings: &Settings,
    identity: &Identity,
    control: &reqwest::Client,
    shared: &session::Shared<'_>,
    state: &mut RobotState,
    input: &mut Input,
) -> Result<()> {
    let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
    let mut link = LinkWatch::new(settings.wifi_grace);
    loop {
        shared.face.set(Mood::Offline);
        shared.screen.show("Connecting…", "").await?;
        let started = Instant::now();
        if let Ok(bundle) = session::bootstrap(control, &settings.server, identity).await {
            let known = &state.cache.bot_id;
            ensure!(
                known.is_empty() || *known == bundle.bot_id,
                "server returned a different bot identity"
            );
            if *known != bundle.bot_id {
                state.cache.bot_id.clone_from(&bundle.bot_id);
                shared.store.flush(&state.cache).await?;
            }
            if session::run(&bundle, shared, state, input).await.is_err() {
                tracing::warn!("connection interrupted; recovering same bot");
            }
        } else {
            tracing::warn!("bootstrap unavailable");
        }
        shared.face.set(Mood::Offline);
        let healthy = started.elapsed() >= HEALTHY_SESSION;
        backoff.record(healthy);
        if healthy {
            link.reset();
        } else if !settings.simulate && backoff.failures() >= LINK_CHECK_AFTER {
            recover_wifi(&mut link, shared.screen).await;
        }
        tokio::time::sleep(backoff.delay(&mut rand::thread_rng())).await;
    }
}

/// Reopen Wi-Fi setup only when no network has been joined for the grace period.
async fn recover_wifi(link: &mut LinkWatch, screen: &Screen) {
    let Ok(up) = wifi::connected().await else {
        tracing::debug!("Wi-Fi state unavailable");
        return;
    };
    if link.observe(up, Instant::now()) == LinkAction::Setup {
        tracing::warn!("no Wi-Fi network for the grace period; reopening setup");
        if let Err(error) = wifi::setup(screen, Some(SETUP_WINDOW)).await {
            tracing::warn!(%error, "Wi-Fi setup failed");
        }
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = terminate.recv() => {},
        _ = tokio::signal::ctrl_c() => {},
    }
}
