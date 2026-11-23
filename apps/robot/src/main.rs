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
use tokio::{io::BufReader, time::Instant};

/// Cursor checkpoints are coalesced to at most one write per interval.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
/// How long the setup hotspot stays up when reopened after a Wi-Fi loss.
const SETUP_WINDOW: Duration = Duration::from_secs(600);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
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
    let stdin = BufReader::with_capacity(input::MAX_LINE, tokio::io::stdin());
    let mut input = Input::spawn(stdin);
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
    let run = reconnect(
        &settings, &identity, &control, &shared, &mut state, &mut input,
    );
    tokio::select! {
        result = run => result,
        () = shutdown_signal() => {
            tracing::info!("stopping");
            face.set(Mood::Offline);
            store.shutdown().await;
            Ok(())
        }
    }
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
        match session::bootstrap(control, &settings.server, identity).await {
            Ok(bundle) => {
                let known = &state.cache.bot_id;
                ensure!(
                    known.is_empty() || *known == bundle.bot_id,
                    "server returned a different bot identity"
                );
                if *known != bundle.bot_id {
                    state.cache.bot_id = bundle.bot_id.clone();
                    shared.store.flush(&state.cache).await?;
                }
                if session::run(&bundle, shared, state, input).await.is_err() {
                    tracing::warn!("connection interrupted; recovering same bot");
                }
            }
            Err(_) => tracing::warn!("bootstrap unavailable"),
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
    let up = match wifi::connected().await {
        Ok(up) => up,
        Err(_) => {
            tracing::debug!("Wi-Fi state unavailable");
            return;
        }
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
