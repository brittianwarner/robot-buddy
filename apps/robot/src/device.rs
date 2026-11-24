//! The robot's main loop.
//!
//! One task serves control input first and runs everything else as lanes it
//! never waits on: the conversation calls the controller asks for (cancels
//! on a lane of their own), registration, cache writes, connecting,
//! credential rotation and Wi-Fi recovery. "Hey Pip" therefore stops the
//! speaker and lights the listening LED however slow the network or the
//! disk is.
//!
//! Credentials are replaced ahead of expiry without going offline: the next
//! session connects in the background and must answer one authenticated
//! conversation call before it counts. It takes over once no call is in
//! flight and no turn is live (or shortly before expiry), and audio already
//! playing finishes on the old speaker. The old connection is then closed.
//! A replacement that fails its first call, or closes while it waits, is
//! dropped and retried; the current session is never touched.
use crate::{
    audio::{PlaybackEvent, Speaker},
    backoff::{Backoff, HEALTHY_SESSION, LINK_CHECK_AFTER, LinkAction, LinkWatch},
    cache::CacheStore,
    controller::{Claim, Controller, Done, Lost, Registration, Remote, RobotState},
    face::Face,
    input::{Control, Input},
    journal::Journal,
    systemd::Pulse,
};
use anyhow::{Result, bail};
use futures_util::{FutureExt, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
use robot_buddy::{auth, protocol::OutputEvent};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc},
    time::{Instant, timeout},
};

const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// Until the robot has connected once, Wi-Fi setup reopens after this long
/// without a network instead of the configured grace: a robot that never
/// joined a network has nothing to wait for.
pub const BOOT_GRACE: Duration = Duration::from_mins(1);
/// How long the setup hotspot stays up before the robot tries known networks again.
pub const SETUP_WINDOW: Duration = Duration::from_mins(10);
const REGISTER_UNOWNED: Duration = Duration::from_secs(3);
/// Ownership rarely changes once set; the bot is checked again this often.
const REGISTER_OWNED: Duration = Duration::from_mins(5);
/// Without pushed events, replay this often as a safety net.
const CATCH_UP_LIVE: Duration = Duration::from_secs(2);
const CATCH_UP_IDLE: Duration = Duration::from_secs(10);
/// Idle catch-up once this connection has delivered pushes.
const CATCH_UP_PUSHING: Duration = Duration::from_mins(1);
/// The next session connects this long before the credentials expire...
const ROTATE_BEFORE: Duration = Duration::from_mins(5);
/// ...and takes over even mid-turn this long before.
const SWAP_BY: Duration = Duration::from_mins(1);
/// A session is never rotated sooner than this after it connected.
const MIN_SESSION: Duration = Duration::from_mins(1);
const ROTATE_RETRY: Duration = Duration::from_secs(10);
/// A replacement connection must answer its first call within this long.
const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const DISCONNECT_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_DISCONNECT: Duration = Duration::from_millis(200);

/// One authenticated connection.
pub struct Session<R, S> {
    pub bot_id: String,
    pub remote: R,
    pub speaker: S,
    pub playback: mpsc::Receiver<PlaybackEvent>,
    pub events: mpsc::Receiver<OutputEvent>,
    /// Set when a pushed event could not be queued: replay to catch up.
    pub overflow: Arc<AtomicBool>,
    /// Notified when the connection closes or reports an error.
    pub closed: Arc<Notify>,
    /// When the credentials stop working, on the monotonic clock.
    pub expires: Instant,
}

/// Opens sessions and reaches the network around them. Every future is
/// `'static`, so the loop keeps serving input while it runs.
pub trait Connector {
    /// Revoke operator-panel capabilities immediately when registration is lost.
    fn panel_owned(&self, _owned: bool) -> impl Future<Output = ()> {
        std::future::ready(())
    }
    type Remote: Remote;
    type Speaker: Speaker + 'static;
    fn connect(
        &self,
    ) -> impl Future<Output = Result<Session<Self::Remote, Self::Speaker>>> + 'static;
    /// Whether a network is joined; `None` when unknown or not managed here.
    fn link_up(&self) -> impl Future<Output = Option<bool>> + 'static;
    /// Host Wi-Fi setup for `window`.
    fn setup(&self, window: Duration) -> impl Future<Output = ()> + 'static;
    /// Show a status message on the panel.
    fn status(&self, message: &'static str) -> impl Future<Output = ()> + 'static;
}

/// Where control input comes from.
pub trait Controls {
    fn next(&mut self) -> impl Future<Output = Control>;
}

impl Controls for Input {
    fn next(&mut self) -> impl Future<Output = Control> {
        Input::next(self)
    }
}

pub struct Options {
    pub peripherals: crate::hardware::Peripherals,
    /// Where conversation event lines go.
    pub journal: Journal,
    /// How long the network must stay down before Wi-Fi setup reopens.
    pub wifi_grace: Duration,
    /// Beaten every pass of the loop, for the systemd watchdog.
    pub pulse: Option<Pulse>,
}

/// Run until `shutdown` resolves (Ok) or the server names a different bot.
pub async fn run<C: Connector, I: Controls>(
    connector: C,
    controls: &mut I,
    face: &Face,
    store: &CacheStore,
    state: &mut RobotState,
    options: Options,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let mut controller = Controller::new(face, store, state, options.journal.clone(), auth::now());
    controller.peripherals = options.peripherals.clone();
    let mut device = Device::new(connector, controller, options);
    device.status("Connecting…");
    device.connecting = Some(device.connector.connect().boxed_local());
    tokio::pin!(shutdown);
    loop {
        device.tick().await;
        let mut wake = device.wake_at();
        if let Some(pulse) = &device.options.pulse {
            pulse.beat();
            wake = wake.min(Instant::now() + pulse.every());
        }
        let accepts = device.controller.accepts_events();
        let background = !device.background.is_empty();
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            control = controls.next() => device.control(control).await,
            event = device.controller.next_audio() => device.controller.audio(event),
            done = wait(&mut device.cancel_lane) => device.finish(done).await,
            done = wait(&mut device.main_lane) => device.finish(done).await,
            Some(event) = recv(&mut device.events), if accepts => device.pushed(event).await,
            () = closed(device.closed.as_ref()) => device.lose("connection closed").await,
            () = closed(device.next.as_ref().map(|next| &next.closed)) => device.discard_next(),
            registration = wait(&mut device.register_lane) => device.registered(registration).await,
            confirmed = wait(&mut device.confirm_lane) => device.confirmed(confirmed),
            persisted = wait(&mut device.persist_lane) => {
                device.controller.persisted(persisted, Instant::now());
            }
            session = wait(&mut device.connecting) => device.connected(session).await?,
            session = wait(&mut device.refreshing) => device.refreshed(session)?,
            up = wait(&mut device.checking) => device.checked(up),
            () = wait(&mut device.setup) => device.retry_at = Some(Instant::now()),
            Some(()) = device.background.next(), if background => {}
            () = tokio::time::sleep_until(wake) => {}
        }
    }
    device.stop().await;
    Ok(())
}

type Slot<T> = Option<LocalBoxFuture<'static, T>>;
type Opened<C> = Result<Session<<C as Connector>::Remote, <C as Connector>::Speaker>>;

/// Resolve the future in `slot`, emptying it. Pends while empty. Dropping
/// this before it resolves leaves the future in the slot to resume later.
async fn wait<T>(slot: &mut Slot<T>) -> T {
    let Some(future) = slot.as_mut() else {
        return std::future::pending().await;
    };
    let output = future.await;
    *slot = None;
    output
}

async fn recv<T>(receiver: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn closed(notify: Option<&Arc<Notify>>) {
    match notify {
        Some(notify) => notify.notified().await,
        None => std::future::pending().await,
    }
}

struct Live<R> {
    remote: R,
    overflow: Arc<AtomicBool>,
    expires: Instant,
    since: Instant,
    owned: bool,
    register_at: Instant,
    catch_up_at: Instant,
    /// This connection has delivered pushed events.
    pushing: bool,
    refresh_at: Instant,
    /// A claim waits for the owner's spoken confirmation code.
    claiming: bool,
}

fn refresh_at(now: Instant, expires: Instant) -> Instant {
    (now + MIN_SESSION).max(expires.checked_sub(ROTATE_BEFORE).unwrap_or(now))
}

fn catch_up(live: bool, pushing: bool) -> Duration {
    match (live, pushing) {
        (true, _) => CATCH_UP_LIVE,
        (false, true) => CATCH_UP_PUSHING,
        (false, false) => CATCH_UP_IDLE,
    }
}

struct Device<'a, C: Connector> {
    connector: C,
    controller: Controller<'a, C::Speaker>,
    options: Options,
    backoff: Backoff,
    link: LinkWatch,
    connected_once: bool,
    live: Option<Live<C::Remote>>,
    events: Option<mpsc::Receiver<OutputEvent>>,
    closed: Option<Arc<Notify>>,
    /// A rotated session waiting to take over.
    next: Option<Session<C::Remote, C::Speaker>>,
    retry_at: Option<Instant>,
    cancel_lane: Slot<Done>,
    main_lane: Slot<Done>,
    register_lane: Slot<Result<Registration>>,
    confirm_lane: Slot<Result<()>>,
    persist_lane: Slot<crate::controller::Persisted>,
    connecting: Slot<Opened<C>>,
    refreshing: Slot<Opened<C>>,
    checking: Slot<Option<bool>>,
    setup: Slot<()>,
    /// Disconnects and panel updates nothing waits for.
    background: FuturesUnordered<LocalBoxFuture<'static, ()>>,
    /// Claim URL last emitted to the journal, so 3 s polls do not repeat it.
    announced_claim: Option<String>,
}

impl<'a, C: Connector> Device<'a, C> {
    fn new(connector: C, controller: Controller<'a, C::Speaker>, options: Options) -> Self {
        let boot_grace = BOOT_GRACE.min(options.wifi_grace);
        Self {
            connector,
            controller,
            options,
            backoff: Backoff::new(RETRY_BASE, RETRY_MAX),
            link: LinkWatch::new(boot_grace),
            connected_once: false,
            live: None,
            events: None,
            closed: None,
            next: None,
            retry_at: None,
            cancel_lane: None,
            main_lane: None,
            register_lane: None,
            confirm_lane: None,
            persist_lane: None,
            connecting: None,
            refreshing: None,
            checking: None,
            setup: None,
            background: FuturesUnordered::new(),
            announced_claim: None,
        }
    }

    /// Start whatever is due. Never waits on I/O.
    async fn tick(&mut self) {
        let now = Instant::now();
        self.controller.expire(now).await;
        if self.persist_lane.is_none()
            && let Some(write) = self.controller.take_persist(now)
        {
            self.persist_lane = Some(write.boxed_local());
        }
        let Some(live) = self.live.as_ref() else {
            if self.retry_at.is_some_and(|at| now >= at)
                && self.connecting.is_none()
                && self.setup.is_none()
            {
                self.retry_at = None;
                self.connecting = Some(self.connector.connect().boxed_local());
            }
            return;
        };
        if now >= live.expires {
            self.lose("credentials expired").await;
            return;
        }
        if self.refreshing.is_none() && self.next.is_none() && now >= live.refresh_at {
            self.refreshing = Some(candidate(self.connector.connect()).boxed_local());
        }
        let swap = self.next.is_some()
            && self.controller.lanes_idle()
            && self.register_lane.is_none()
            && (!self.controller.is_live() || now + SWAP_BY >= live.expires);
        if swap {
            self.swap(now);
        }
        let Some(live) = self.live.as_mut() else {
            return;
        };
        if self.register_lane.is_none() && now >= live.register_at {
            let remote = live.remote.clone();
            let cache = self.controller.cache();
            let shown = Claim {
                url: cache.claim_url.clone(),
                expires: cache.claim_expires,
            };
            self.register_lane = Some(async move { remote.register(shown).await }.boxed_local());
        }
        if live.overflow.swap(false, Ordering::Relaxed) || now >= live.catch_up_at {
            self.controller.needs_replay = true;
            live.catch_up_at = now + catch_up(self.controller.is_live(), live.pushing);
        }
        if self.cancel_lane.is_none()
            && let Some(op) = self.controller.next_cancel()
        {
            self.cancel_lane = Some(op.run(live.remote.clone()).boxed_local());
        }
        // A question never waits behind a catch-up replay: the replay is
        // dropped (it changes nothing) and runs again after the submit.
        if self.main_lane.is_some() && self.controller.submit_waits_on_replay(live.owned) {
            self.main_lane = None;
            self.controller.preempt_replay();
        }
        if self.main_lane.is_none()
            && let Some(op) = self.controller.next_main(live.owned)
        {
            self.main_lane = Some(op.run(live.remote.clone()).boxed_local());
        }
    }

    /// The earliest future time something in `tick` comes due. Work that is
    /// due but waiting on a busy lane is picked up when that lane finishes.
    fn wake_at(&self) -> Instant {
        let now = Instant::now();
        let mut due = vec![self.retry_at, self.controller.deadline()];
        if let Some(live) = &self.live {
            due.push(Some(live.catch_up_at));
            due.push(Some(live.expires));
            if self.register_lane.is_none() {
                due.push(Some(live.register_at));
            }
            if self.refreshing.is_none() && self.next.is_none() {
                due.push(Some(live.refresh_at));
            }
            if self.next.is_some() {
                due.push(live.expires.checked_sub(SWAP_BY));
            }
        }
        due.into_iter()
            .flatten()
            .filter(|at| *at > now)
            .min()
            .unwrap_or(now + Duration::from_hours(1))
    }

    fn status(&mut self, message: &'static str) {
        self.background
            .push(self.connector.status(message).boxed_local());
    }

    fn teardown(&mut self, remote: C::Remote) {
        self.background.push(
            async move {
                if timeout(DISCONNECT_GRACE, remote.disconnect())
                    .await
                    .is_err()
                {
                    tracing::debug!("old connection did not close in time");
                }
            }
            .boxed_local(),
        );
    }

    fn check_bot(&mut self, bot_id: &str) -> Result<()> {
        let known = &self.controller.cache().bot_id;
        if !known.is_empty() && known != bot_id {
            bail!("server returned a different bot identity");
        }
        if known.is_empty() {
            self.controller
                .update_cache(|cache| bot_id.clone_into(&mut cache.bot_id));
        }
        Ok(())
    }

    async fn connected(&mut self, opened: Opened<C>) -> Result<()> {
        let session = match opened {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(%error, "bootstrap unavailable");
                self.backoff.record(false);
                self.schedule_retry();
                return Ok(());
            }
        };
        self.check_bot(&session.bot_id)?;
        let now = Instant::now();
        if !self.connected_once {
            self.connected_once = true;
            self.link = LinkWatch::new(self.options.wifi_grace);
        }
        // Reaching the server proves an uplink, whatever the interface.
        self.link.reset();
        self.events = Some(session.events);
        self.closed = Some(session.closed);
        self.live = Some(Live {
            remote: session.remote,
            overflow: session.overflow,
            expires: session.expires,
            since: now,
            owned: false,
            register_at: now,
            catch_up_at: now + CATCH_UP_IDLE,
            pushing: false,
            refresh_at: refresh_at(now, session.expires),
            claiming: false,
        });
        self.controller
            .go_online(session.speaker, session.playback)
            .await;
        Ok(())
    }

    fn refreshed(&mut self, opened: Opened<C>) -> Result<()> {
        match opened {
            Ok(session) => {
                self.check_bot(&session.bot_id)?;
                if self.live.is_some() {
                    self.next = Some(session);
                } else {
                    self.teardown(session.remote);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "credential refresh failed; retrying");
                if let Some(live) = self.live.as_mut() {
                    live.refresh_at = Instant::now() + ROTATE_RETRY;
                }
            }
        }
        Ok(())
    }

    /// The replacement closed before it took over: drop it and try again
    /// later. The current session carries on.
    fn discard_next(&mut self) {
        let Some(next) = self.next.take() else {
            return;
        };
        tracing::warn!("replacement connection closed before taking over; retrying");
        self.teardown(next.remote);
        if let Some(live) = self.live.as_mut() {
            live.refresh_at = Instant::now() + ROTATE_RETRY;
        }
    }

    /// Hand over to the rotated session. No Offline face, no backoff, and
    /// audio already playing finishes on the old speaker.
    fn swap(&mut self, now: Instant) {
        let (Some(next), Some(old)) = (self.next.take(), self.live.take()) else {
            return;
        };
        self.teardown(old.remote);
        self.events = Some(next.events);
        self.closed = Some(next.closed);
        self.live = Some(Live {
            remote: next.remote,
            overflow: next.overflow,
            expires: next.expires,
            since: old.since,
            owned: old.owned,
            register_at: old.register_at,
            catch_up_at: now + catch_up(self.controller.is_live(), false),
            pushing: false,
            refresh_at: refresh_at(now, next.expires),
            claiming: old.claiming,
        });
        self.controller.rotate(next.speaker, next.playback);
        tracing::info!("credentials rotated");
    }

    async fn lose(&mut self, reason: &str) {
        let Some(live) = self.live.take() else {
            return;
        };
        tracing::warn!(reason, "connection interrupted; recovering same bot");
        let healthy = live.since.elapsed() >= HEALTHY_SESSION;
        self.backoff.record(healthy);
        if healthy {
            self.link.reset();
        }
        self.cancel_lane = None;
        self.main_lane = None;
        self.register_lane = None;
        self.confirm_lane = None;
        self.refreshing = None;
        self.events = None;
        self.closed = None;
        self.teardown(live.remote);
        if let Some(next) = self.next.take() {
            self.teardown(next.remote);
        }
        self.controller.go_offline().await;
        self.connector.panel_owned(false).await;
        self.status("Connecting…");
        self.schedule_retry();
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.backoff.delay(&mut rand::thread_rng()));
        if self.backoff.failures() >= LINK_CHECK_AFTER
            && self.checking.is_none()
            && self.setup.is_none()
        {
            self.checking = Some(self.connector.link_up().boxed_local());
        }
    }

    /// Reopen Wi-Fi setup only when no network has been joined for the grace period.
    fn checked(&mut self, up: Option<bool>) {
        let Some(up) = up else {
            return;
        };
        if self.live.is_none() && self.link.observe(up, Instant::now()) == LinkAction::Setup {
            tracing::warn!("no network for the grace period; reopening Wi-Fi setup");
            self.retry_at = None;
            self.setup = Some(self.connector.setup(SETUP_WINDOW).boxed_local());
        }
    }

    async fn finish(&mut self, done: Done) {
        let replayed = matches!(done, Done::Replayed(_));
        if let Err(Lost(reason)) = self.controller.finish(done).await {
            self.lose(&reason).await;
            return;
        }
        let now = Instant::now();
        let live_turn = self.controller.is_live();
        if let Some(live) = self.live.as_mut() {
            let next = now + catch_up(live_turn, live.pushing);
            live.catch_up_at = if replayed {
                next
            } else {
                live.catch_up_at.min(next)
            };
        }
    }

    async fn pushed(&mut self, event: OutputEvent) {
        self.controller.pushed(event).await;
        let live_turn = self.controller.is_live();
        if let Some(live) = self.live.as_mut() {
            live.pushing = true;
            live.catch_up_at = Instant::now() + catch_up(live_turn, true);
        }
    }

    async fn registered(&mut self, registration: Result<Registration>) {
        let registration = match registration {
            Ok(registration) => registration,
            Err(error) => {
                self.lose(&format!("registration failed: {error}")).await;
                return;
            }
        };
        self.connector.panel_owned(registration.owned).await;
        if !registration.owned || registration.confirming || registration.claim.is_some() {
            self.controller.clear_pending_unowned();
        }
        if let Some(claim) = registration.claim {
            self.controller.update_cache(|cache| {
                cache.claim_url = claim.url;
                cache.claim_expires = claim.expires;
            });
        }
        // An owned robot has no use for its claim link, which carries a token.
        let cache = self.controller.cache();
        if registration.owned && (!cache.claim_url.is_empty() || cache.claim_expires != 0) {
            self.controller.update_cache(|cache| {
                cache.claim_url.clear();
                cache.claim_expires = 0;
            });
        }
        let active_claim = (!registration.owned)
            .then(|| self.controller.cache().claim_url.as_str())
            .filter(|url| !url.is_empty());
        match active_claim {
            Some(url) if self.announced_claim.as_deref() != Some(url) => {
                self.options.journal.claim(url);
                self.announced_claim = Some(url.to_owned());
            }
            Some(_) => {}
            None => self.announced_claim = None,
        }
        self.controller.set_claiming(registration.confirming);
        if let Some(live) = self.live.as_mut() {
            live.claiming = registration.confirming;
            live.owned = registration.owned;
            let every = if live.owned {
                REGISTER_OWNED
            } else {
                REGISTER_UNOWNED
            };
            live.register_at = Instant::now() + every;
        }
    }

    /// Control input. A confirmation code goes to the bot actor, only while a
    /// claim waits for one and one at a time. Utterances are admitted only once
    /// the current live registration verifies ownership and no claim is open;
    /// offline utterances go through the controller's offline apology path.
    async fn control(&mut self, control: Control) {
        match control {
            Control::Confirm(code) => {
                let Some(live) = self.live.as_ref().filter(|live| live.claiming) else {
                    // Never logged: the code is a credential while a claim is open.
                    tracing::info!("confirmation code ignored: no claim waiting");
                    self.controller.apologize();
                    return;
                };
                if self.confirm_lane.is_some() {
                    tracing::info!("confirmation code ignored: one is being checked");
                    return;
                }
                let remote = live.remote.clone();
                self.confirm_lane = Some(async move { remote.confirm(&code).await }.boxed_local());
            }
            Control::Say(_) if self.live.is_none() => {
                self.controller.control(control).await;
            }
            Control::Say(_)
                if !self
                    .live
                    .as_ref()
                    .is_some_and(|live| live.owned && !live.claiming) =>
            {
                let reason = if self.live.as_ref().is_some_and(|live| live.claiming) {
                    "claim waiting for confirmation"
                } else {
                    "robot not owned"
                };
                self.controller.refuse_say(reason).await;
            }
            other => {
                self.controller.control(other).await;
            }
        }
    }

    /// A confirmed claim makes the robot owned: register again at once. A
    /// refused code (the server counts it) shows the apology face.
    fn confirmed(&mut self, result: Result<()>) {
        match result {
            Ok(()) => {
                tracing::info!("owner confirmed");
                self.controller.set_claiming(false);
                if let Some(live) = self.live.as_mut() {
                    live.claiming = false;
                    live.register_at = Instant::now();
                }
            }
            Err(error) => {
                tracing::warn!(%error, "confirmation refused");
                self.controller.apologize();
            }
        }
    }

    /// Silence, then close the connections briefly; the caller flushes the cache.
    async fn stop(&mut self) {
        self.connector.panel_owned(false).await;
        self.controller.shutdown().await;
        let remotes: Vec<_> = self
            .live
            .take()
            .map(|live| live.remote)
            .into_iter()
            .chain(self.next.take().map(|next| next.remote))
            .collect();
        let closing = futures_util::future::join_all(remotes.iter().map(Remote::disconnect));
        if timeout(SHUTDOWN_DISCONNECT, closing).await.is_err() {
            tracing::debug!("connection did not close before shutdown");
        }
    }
}

/// Open a replacement session and prove it with one authenticated
/// conversation call. One that fails is closed here and never offered.
async fn candidate<R: Remote, S>(
    connect: impl Future<Output = Result<Session<R, S>>>,
) -> Result<Session<R, S>> {
    let session = connect.await?;
    let failure = match timeout(PROBE_DEADLINE, session.remote.probe()).await {
        Ok(Ok(())) => return Ok(session),
        Ok(Err(error)) => error.to_string(),
        Err(_) => "timed out".into(),
    };
    if timeout(DISCONNECT_GRACE, session.remote.disconnect())
        .await
        .is_err()
    {
        tracing::debug!("failed replacement did not close in time");
    }
    bail!("replacement connection failed its first call: {failure}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        audio::Rewarm,
        cache::Cache,
        controller::{CallError, Submitted, tests::Log, tests::Server, tests::event, tests::quiet},
        face::{self, LineSink, Mood, Opener},
    };
    use std::{
        io,
        sync::{Condvar, Mutex},
    };
    use tokio::sync::watch;

    impl Controls for mpsc::Receiver<Control> {
        async fn next(&mut self) -> Control {
            match self.recv().await {
                Some(control) => control,
                None => std::future::pending().await,
            }
        }
    }

    struct Wires {
        push: mpsc::Sender<OutputEvent>,
        playback: mpsc::Sender<PlaybackEvent>,
        closed: Arc<Notify>,
    }

    /// The network, the server and every connection the robot opened.
    struct World {
        server: Server,
        log: Log,
        stalled: watch::Sender<bool>,
        /// Replays (only) wait here while held.
        replays_held: watch::Sender<bool>,
        /// Submit answers wait here while held: the server has run them.
        acks_held: watch::Sender<bool>,
        /// Connections whose first call never answers.
        dead_probes: Mutex<Vec<usize>>,
        /// The claim the bot reports while unowned.
        claim: Mutex<Option<Claim>>,
        confirming: Mutex<bool>,
        /// Calls that reached the network, finished or not.
        entered: Log,
        wires: Mutex<Vec<Wires>>,
        lifetime: Mutex<Duration>,
        owned: Mutex<bool>,
        link: Mutex<Option<bool>>,
        stops: Mutex<Vec<std::time::Instant>>,
    }

    impl World {
        fn new() -> Arc<Self> {
            let log = Log::default();
            Arc::new(Self {
                server: Server {
                    log: log.clone(),
                    ..Server::default()
                },
                log,
                stalled: watch::channel(false).0,
                replays_held: watch::channel(false).0,
                acks_held: watch::channel(false).0,
                dead_probes: Mutex::default(),
                claim: Mutex::default(),
                confirming: Mutex::new(false),
                entered: Log::default(),
                wires: Mutex::default(),
                lifetime: Mutex::new(Duration::from_mins(15)),
                owned: Mutex::new(true),
                link: Mutex::new(None),
                stops: Mutex::default(),
            })
        }
        /// Every call waits here while the network is stalled.
        async fn network(&self, call: &str) {
            self.entered.push(call.into());
            let mut stalled = self.stalled.subscribe();
            let _ = stalled.wait_for(|s| !*s).await;
        }
        fn stall(&self, on: bool) {
            self.stalled.send_replace(on);
        }
        async fn gate(gate: &watch::Sender<bool>) {
            let mut held = gate.subscribe();
            let _ = held.wait_for(|h| !*h).await;
        }
        fn connections(&self) -> usize {
            self.wires.lock().unwrap().len()
        }
        fn playback(&self, connection: usize) -> mpsc::Sender<PlaybackEvent> {
            self.wires.lock().unwrap()[connection].playback.clone()
        }
        fn push(&self, connection: usize) -> mpsc::Sender<OutputEvent> {
            self.wires.lock().unwrap()[connection].push.clone()
        }
        fn close(&self, connection: usize) {
            self.wires.lock().unwrap()[connection].closed.notify_one();
        }
    }

    #[derive(Clone)]
    struct FakeRemote {
        world: Arc<World>,
        id: usize,
    }

    impl Remote for FakeRemote {
        async fn submit(&self, request_id: &str, text: &str) -> Result<Submitted, CallError> {
            self.world.network("submit").await;
            let answer = self.world.server.submit(request_id, text);
            World::gate(&self.world.acks_held).await;
            answer
        }
        async fn cancel(&self, turn_id: &str) -> Result<(), CallError> {
            self.world.network("cancel").await;
            self.world.server.cancel(turn_id)
        }
        async fn cancel_request(&self, request_id: &str) -> Result<(), CallError> {
            self.world.network("cancel_request").await;
            self.world.server.cancel_request(request_id)
        }
        async fn replay(&self, after: i64) -> Result<Vec<OutputEvent>, CallError> {
            self.world.network("replay").await;
            World::gate(&self.world.replays_held).await;
            Ok(self.world.server.replay(after))
        }
        async fn probe(&self) -> Result<(), CallError> {
            self.world.network("probe").await;
            if self.world.dead_probes.lock().unwrap().contains(&self.id) {
                return std::future::pending().await;
            }
            self.world.log.push(format!("probe {}", self.id));
            Ok(())
        }
        async fn confirm(&self, code: &str) -> Result<()> {
            self.world.network("confirm").await;
            self.world.log.push(format!("confirm {code}"));
            anyhow::ensure!(code == "123456", "confirmation expired or invalid");
            *self.world.owned.lock().unwrap() = true;
            *self.world.confirming.lock().unwrap() = false;
            Ok(())
        }
        async fn register(&self, _: Claim) -> Result<Registration> {
            self.world.network("register").await;
            let owned = *self.world.owned.lock().unwrap();
            Ok(Registration {
                owned,
                claim: if owned {
                    None
                } else {
                    self.world.claim.lock().unwrap().clone()
                },
                confirming: *self.world.confirming.lock().unwrap(),
            })
        }
        async fn disconnect(&self) {
            self.world.log.push(format!("disconnect {}", self.id));
        }
    }

    struct FakeSpeaker {
        world: Arc<World>,
        id: usize,
        generation: u64,
    }

    impl Speaker for FakeSpeaker {
        fn start(&mut self, turn_id: &str) -> Option<u64> {
            self.generation += 1;
            self.world
                .log
                .push(format!("start {turn_id} on {}", self.id));
            Some(self.generation)
        }
        async fn stop(&mut self, _: Rewarm) {
            self.world
                .stops
                .lock()
                .unwrap()
                .push(std::time::Instant::now());
            self.world.log.push(format!("stop {}", self.id));
        }
    }

    #[derive(Clone)]
    struct FakeConnector(Arc<World>);

    impl Connector for FakeConnector {
        type Remote = FakeRemote;
        type Speaker = FakeSpeaker;
        fn connect(
            &self,
        ) -> impl Future<Output = Result<Session<FakeRemote, FakeSpeaker>>> + 'static {
            let world = self.0.clone();
            async move {
                world.network("connect").await;
                let (push, events) = mpsc::channel(256);
                let (playback_tx, playback) = mpsc::channel(8);
                let closed = Arc::new(Notify::new());
                let id = {
                    let mut wires = world.wires.lock().unwrap();
                    wires.push(Wires {
                        push,
                        playback: playback_tx,
                        closed: closed.clone(),
                    });
                    wires.len() - 1
                };
                world.log.push(format!("connected {id}"));
                let expires = Instant::now() + *world.lifetime.lock().unwrap();
                Ok(Session {
                    bot_id: "bot".into(),
                    remote: FakeRemote {
                        world: world.clone(),
                        id,
                    },
                    speaker: FakeSpeaker {
                        world,
                        id,
                        generation: 0,
                    },
                    playback,
                    events,
                    overflow: Arc::default(),
                    closed,
                    expires,
                })
            }
        }
        fn link_up(&self) -> impl Future<Output = Option<bool>> + 'static {
            let up = *self.0.link.lock().unwrap();
            async move { up }
        }
        fn setup(&self, window: Duration) -> impl Future<Output = ()> + 'static {
            self.0.log.push(format!("setup {}", window.as_secs()));
            std::future::pending()
        }
        fn status(&self, _: &'static str) -> impl Future<Output = ()> + 'static {
            std::future::ready(())
        }
    }

    type Lines = Arc<Mutex<Vec<(std::time::Instant, String)>>>;

    /// The face serial port: records each line with the time it was written.
    struct Panel(Lines);
    struct PanelSink(Lines);
    impl Opener for Panel {
        type Sink = PanelSink;
        fn open(&mut self) -> impl Future<Output = io::Result<PanelSink>> + Send {
            std::future::ready(Ok(PanelSink(self.0.clone())))
        }
    }
    impl LineSink for PanelSink {
        fn write_frame(&mut self, frame: &[u8]) -> impl Future<Output = io::Result<()>> + Send {
            let now = std::time::Instant::now();
            let mut lines = self.0.lock().unwrap();
            for line in String::from_utf8_lossy(frame).lines() {
                lines.push((now, line.trim().to_owned()));
            }
            std::future::ready(Ok(()))
        }
    }

    const LISTENING_LED: &str = r#"{"led":"listening"}"#;
    const OFFLINE_FACE: &str = r#"{"e":"sleepy","i":0.5}"#;

    struct Rig {
        world: Arc<World>,
        face: Face,
        lines: Lines,
        store: CacheStore,
        state: RobotState,
        controls: mpsc::Sender<Control>,
        input: mpsc::Receiver<Control>,
    }

    impl Rig {
        fn new(write: crate::cache::WriteFn) -> Self {
            let (face, target) = Face::new();
            let lines = Lines::default();
            tokio::spawn(face::run(target, Panel(lines.clone())));
            let (controls, input) = mpsc::channel(16);
            Self {
                world: World::new(),
                face,
                lines,
                store: CacheStore::spawn_with(write, Duration::from_secs(30)),
                state: RobotState::default(),
                controls,
                input,
            }
        }
        fn quick() -> Self {
            Self::new(Arc::new(|_| Ok(())))
        }
    }

    fn options() -> Options {
        Options {
            peripherals: crate::hardware::Peripherals::default(),
            journal: quiet(),
            wifi_grace: Duration::from_mins(5),
            pulse: None,
        }
    }

    /// Run the device until `script` finishes.
    macro_rules! run_with {
        ($rig:ident, $script:expr) => {
            run_with!($rig, options(), $script)
        };
        ($rig:ident, $options:expr, $script:expr) => {{
            let connector = FakeConnector($rig.world.clone());
            run(
                connector,
                &mut $rig.input,
                &$rig.face,
                &$rig.store,
                &mut $rig.state,
                $options,
                $script,
            )
            .await
            .unwrap();
        }};
    }

    /// Wait (in test time) until `ready` holds.
    async fn until(what: &str, limit: Duration, ready: impl Fn() -> bool) {
        let give_up = Instant::now() + limit;
        while !ready() {
            assert!(Instant::now() < give_up, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    const SOON: Duration = Duration::from_secs(5);

    fn wrote_after(
        lines: &Lines,
        line: &str,
        after: std::time::Instant,
    ) -> Option<std::time::Instant> {
        let lines = lines.lock().unwrap();
        lines
            .iter()
            .find(|(at, l)| *at >= after && l == line)
            .map(|(at, _)| *at)
    }

    /// A disk the test can hold: writes wait until it is released.
    #[derive(Clone, Default)]
    struct Disk(Arc<(Mutex<bool>, Condvar)>, Log);
    impl Disk {
        fn hold(&self, on: bool) {
            *self.0.0.lock().unwrap() = on;
            self.0.1.notify_all();
        }
        fn write(&self) -> crate::cache::WriteFn {
            let disk = self.clone();
            Arc::new(move |_| {
                let (held, released) = &*disk.0;
                let _unused = released
                    .wait_while(held.lock().unwrap(), |held| *held)
                    .unwrap();
                disk.1.push("written".into());
                Ok(())
            })
        }
    }

    /// "Hey Pip" while every actor call hangs and the disk is held: the
    /// speaker stops and the listening LED is written before either
    /// recovers, and the cancel still goes out alongside the stalled
    /// replay. Real time on purpose (the disk is a real blocking write); the
    /// wake-to-silence latency is asserted under `BUDDY_STRICT_TIMING=1`.
    #[tokio::test]
    async fn barge_in_while_calls_and_the_disk_are_stalled_stops_first() {
        let disk = Disk::default();
        let mut rig = Rig::new(disk.write());
        let world = rig.world.clone();
        let (face, lines, controls) = (rig.face.clone(), rig.lines.clone(), rig.controls.clone());
        let mut stops = Vec::new();
        let mut leds = Vec::new();
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            for round in 0..5u64 {
                let turn = format!("turn-q{round}");
                controls
                    .send(Control::Say(format!("q{round}")))
                    .await
                    .unwrap();
                let started = format!("start {turn}");
                until("speech", SOON, || world.log.count(&started) == 1).await;
                world
                    .playback(0)
                    .send(PlaybackEvent::Started {
                        generation: round + 1,
                    })
                    .await
                    .unwrap();
                until("speaking", SOON, || face.steady() == Mood::Speaking).await;
                world.stall(true);
                // A gap forces a replay that cannot finish.
                let replays = world.entered.count("replay");
                world
                    .push(0)
                    .send(event(50, &turn, "text.delta"))
                    .await
                    .unwrap();
                until("held replay", SOON, || {
                    world.entered.count("replay") > replays
                })
                .await;
                disk.hold(true);
                let written = disk.1.count("written");
                let stopped = world.stops.lock().unwrap().len();
                let wake = std::time::Instant::now();
                controls.send(Control::Wake).await.unwrap();
                until("stop", SOON, || world.stops.lock().unwrap().len() > stopped).await;
                until("led", SOON, || {
                    wrote_after(&lines, LISTENING_LED, wake).is_some()
                })
                .await;
                stops.push(world.stops.lock().unwrap()[stopped] - wake);
                leds.push(wrote_after(&lines, LISTENING_LED, wake).unwrap() - wake);
                let cancel = format!("cancel {turn}");
                until("cancel sent while the replay is held", SOON, || {
                    world.entered.count("cancel") > usize::try_from(round).unwrap()
                })
                .await;
                assert_eq!(world.log.count(&cancel), 0, "the network is still stalled");
                assert_eq!(disk.1.count("written"), written, "the disk is still held");
                disk.hold(false);
                world.stall(false);
                until("cancelled", SOON, || world.log.count(&cancel) == 1).await;
                controls.send(Control::Listening(false)).await.unwrap();
            }
        });
        stops.sort();
        leds.sort();
        eprintln!("wake -> player stopped: {stops:?}");
        eprintln!("wake -> listening LED written: {leds:?}");
        if std::env::var("BUDDY_STRICT_TIMING").as_deref() == Ok("1") {
            let bound = Duration::from_millis(50);
            assert!(
                stops.iter().chain(&leds).all(|d| *d < bound),
                "{stops:?} {leds:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_submit_ack_never_replays_old_speech_and_the_next_turn_plays() {
        let mut rig = Rig::quick();
        *rig.world.server.drop_next_ack.lock().unwrap() = true;
        let world = rig.world.clone();
        let controls = rig.controls.clone();
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls.send(Control::Say("first".into())).await.unwrap();
            // The ack is lost; the robot reconnects and resubmits the same ID.
            until("reconnected", Duration::from_mins(1), || {
                world.connections() == 2
            })
            .await;
            until("resubmitted", SOON, || world.log.count("submit first") == 2).await;
            assert_eq!(world.log.count("disconnect 0"), 1, "old connection closed");
            // The old turn's audio is announced again: still silent.
            world
                .push(1)
                .send(event(1, "turn-first", "speech.ready"))
                .await
                .unwrap();
            world
                .push(1)
                .send(event(2, "turn-first", "turn.completed"))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(world.log.count("start turn-first"), 0);
            controls.send(Control::Say("next".into())).await.unwrap();
            until("new turn plays", SOON, || {
                world.log.count("start turn-next on 1") == 1
            })
            .await;
        });
        let submits: Vec<_> = rig
            .world
            .log
            .entries()
            .into_iter()
            .filter(|e| e.starts_with("submit first"))
            .collect();
        assert_eq!(submits[0], submits[1], "same request ID");
    }

    #[tokio::test(start_paused = true)]
    async fn rotation_mid_speech_keeps_talking_and_closes_the_old_connection() {
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        let (face, lines, controls) = (rig.face.clone(), rig.lines.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls.send(Control::Say("story".into())).await.unwrap();
            until("speech", SOON, || {
                world.log.count("start turn-story on 0") == 1
            })
            .await;
            world
                .playback(0)
                .send(PlaybackEvent::Started { generation: 1 })
                .await
                .unwrap();
            until("speaking", SOON, || face.steady() == Mood::Speaking).await;
            let online_at = std::time::Instant::now();
            let stops = world.stops.lock().unwrap().len();
            // Ten minutes in the next session connects; the turn is still live.
            tokio::time::sleep(Duration::from_secs(10 * 60 + 5)).await;
            assert_eq!(world.connections(), 2);
            assert_eq!(world.log.count("probe 1"), 1, "proven before it counts");
            assert_eq!(world.log.count("disconnect 0"), 0, "waits for the turn");
            // A minute before expiry it takes over even mid-turn.
            tokio::time::sleep(Duration::from_mins(4)).await;
            assert_eq!(world.log.count("disconnect 0"), 1);
            assert_eq!(world.stops.lock().unwrap().len(), stops, "speech never cut");
            assert_eq!(face.steady(), Mood::Speaking);
            assert!(
                wrote_after(&lines, OFFLINE_FACE, online_at).is_none(),
                "never shown Offline"
            );
            // The old speaker finishes; the next turn uses the new session.
            world
                .playback(0)
                .send(PlaybackEvent::Finished {
                    generation: 1,
                    played: true,
                })
                .await
                .unwrap();
            controls.send(Control::Say("again".into())).await.unwrap();
            until("new speaker", SOON, || {
                world.log.count("start turn-again on 1") == 1
            })
            .await;
        });
        assert_eq!(rig.world.connections(), 2, "one rotation, no reconnect");
    }

    #[tokio::test(start_paused = true)]
    async fn offline_mute_and_say_answer_at_once() {
        let mut rig = Rig::quick();
        rig.world.stall(true); // connecting hangs
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("connecting", SOON, || world.entered.count("connect") == 1).await;
            assert_eq!(face.steady(), Mood::Offline);
            controls.send(Control::Mute(true)).await.unwrap();
            until("muted", SOON, || face.steady() == Mood::Muted).await;
            controls.send(Control::Mute(false)).await.unwrap();
            until("unmuted", SOON, || face.steady() == Mood::Offline).await;
            controls.send(Control::Say("hello?".into())).await.unwrap();
            until("apology", SOON, || face.flashing() == Some(Mood::Sorry)).await;
            // Nothing is queued: when the network returns, nothing is sent.
            world.stall(false);
            until("online", SOON, || world.entered.count("register") == 1).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        assert_eq!(rig.world.log.count("submit"), 0);
        assert!(rig.state.cache.pending.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unowned_robot_refuses_utterances_without_queuing_them() {
        let mut rig = Rig::quick();
        *rig.world.owned.lock().unwrap() = false;
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls.send(Control::Say("hi".into())).await.unwrap();
            until("refused", SOON, || face.flashing() == Some(Mood::Sorry)).await;
            assert_eq!(face.steady(), Mood::Idle, "never queued into Thinking");
            // Registered later: the refused pre-claim utterance is never submitted.
            *world.owned.lock().unwrap() = true;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        assert_eq!(rig.world.log.count("submit"), 0);
        assert!(rig.state.cache.pending.is_none());
    }

    /// Pushes go silent and every call hangs (a half-open socket). While the
    /// robot waits on its own turn it reconnects within a few seconds; idle,
    /// it leaves the slow catch-up to notice.
    #[tokio::test(start_paused = true)]
    async fn a_silent_connection_is_replaced_fast_only_while_a_turn_is_awaited() {
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        let controls = rig.controls.clone();
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            world.stall(true);
            tokio::time::sleep(Duration::from_mins(1)).await;
            assert_eq!(world.entered.count("connect"), 1, "idle: no fast reconnect");
            world.stall(false);
            until("idle replay answered", SOON, || {
                world.log.count("replay") >= 1
            })
            .await;
            controls.send(Control::Say("q".into())).await.unwrap();
            until("speech requested", SOON, || {
                world.log.count("start turn-q on 0") == 1
            })
            .await;
            world.stall(true);
            let silent = Instant::now();
            until("reconnect", Duration::from_secs(10), || {
                world.entered.count("connect") == 2
            })
            .await;
            let recovered = silent.elapsed();
            assert!(
                recovered
                    <= CATCH_UP_LIVE + crate::controller::WAITING_REPLAY_DEADLINE + RETRY_BASE,
                "{recovered:?}"
            );
            world.stall(false);
        });
        assert_eq!(rig.world.log.count("disconnect 0"), 1);
    }

    /// The loop beats for the watchdog at least every `Pulse::every`, even
    /// with nothing to do.
    #[tokio::test(start_paused = true)]
    async fn an_idle_loop_keeps_beating() {
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        let pulse = Pulse::new(Duration::from_secs(15));
        let options = Options {
            pulse: Some(pulse.clone()),
            ..options()
        };
        run_with!(rig, options, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            let before = pulse.beats();
            tokio::time::sleep(Duration::from_mins(10)).await;
            assert!(pulse.beats() - before >= 40, "{}", pulse.beats() - before);
        });
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_connection_reconnects_and_owned_bots_poll_rarely() {
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            tokio::time::sleep(Duration::from_mins(4)).await;
            assert_eq!(world.entered.count("register"), 1, "owned: no 3 s polling");
            world.close(0);
            until("reconnected", Duration::from_mins(1), || {
                world.connections() == 2
            })
            .await;
            until("re-registered", SOON, || {
                world.entered.count("register") == 2
            })
            .await;
        });
        assert_eq!(rig.world.log.count("disconnect 0"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn wifi_setup_opens_after_the_boot_grace_while_input_still_works() {
        let mut rig = Rig::quick();
        *rig.world.link.lock().unwrap() = Some(false);
        rig.world.stall(true);
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        // Connect attempts fail at once instead of hanging.
        let failing = FailingConnector(rig.world.clone());
        let mut input = rig.input;
        run(
            failing,
            &mut input,
            &rig.face,
            &rig.store,
            &mut rig.state,
            options(),
            async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                assert_eq!(world.log.count("setup"), 0, "no eager setup at boot");
                until("setup", Duration::from_mins(3), || {
                    world.log.count("setup 600") == 1
                })
                .await;
                controls.send(Control::Mute(true)).await.unwrap();
                until("muted", SOON, || face.steady() == Mood::Muted).await;
            },
        )
        .await
        .unwrap();
    }

    struct FailingConnector(Arc<World>);

    impl Connector for FailingConnector {
        type Remote = FakeRemote;
        type Speaker = FakeSpeaker;
        fn connect(
            &self,
        ) -> impl Future<Output = Result<Session<FakeRemote, FakeSpeaker>>> + 'static {
            std::future::ready(Err(anyhow::anyhow!("no route to host")))
        }
        fn link_up(&self) -> impl Future<Output = Option<bool>> + 'static {
            let up = *self.0.link.lock().unwrap();
            async move { up }
        }
        fn setup(&self, window: Duration) -> impl Future<Output = ()> + 'static {
            self.0.log.push(format!("setup {}", window.as_secs()));
            std::future::pending()
        }
        fn status(&self, _: &'static str) -> impl Future<Output = ()> + 'static {
            std::future::ready(())
        }
    }

    /// Only the replacement connection is broken: its first call never
    /// answers. It is dropped and retried, the current session keeps
    /// talking and is never stopped, and a working replacement takes over.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_fails_its_first_call_never_touches_the_current_session() {
        let mut rig = Rig::quick();
        rig.world.dead_probes.lock().unwrap().extend([1, 2]);
        let world = rig.world.clone();
        let (face, lines, controls) = (rig.face.clone(), rig.lines.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls.send(Control::Say("story".into())).await.unwrap();
            until("speech", SOON, || {
                world.log.count("start turn-story on 0") == 1
            })
            .await;
            world
                .playback(0)
                .send(PlaybackEvent::Started { generation: 1 })
                .await
                .unwrap();
            until("speaking", SOON, || face.steady() == Mood::Speaking).await;
            let online_at = std::time::Instant::now();
            let stops = world.stops.lock().unwrap().len();
            tokio::time::sleep(Duration::from_mins(10) + PROBE_DEADLINE + SOON).await;
            assert_eq!(world.connections(), 2);
            assert_eq!(
                world.log.count("disconnect 1"),
                1,
                "the failed one is closed"
            );
            tokio::time::sleep(ROTATE_RETRY + PROBE_DEADLINE).await;
            assert_eq!(world.connections(), 3, "retried");
            assert_eq!(world.log.count("disconnect 2"), 1);
            assert_eq!(world.log.count("disconnect 0"), 0);
            assert_eq!(world.stops.lock().unwrap().len(), stops, "speech never cut");
            assert_eq!(face.steady(), Mood::Speaking);
            // The next replacement works and takes over before expiry.
            tokio::time::sleep(ROTATE_RETRY + SOON).await;
            assert_eq!(world.log.count("probe 3"), 1);
            tokio::time::sleep(Duration::from_mins(4)).await;
            assert_eq!(world.log.count("disconnect 0"), 1);
            assert_eq!(world.stops.lock().unwrap().len(), stops, "speech never cut");
            assert!(
                wrote_after(&lines, OFFLINE_FACE, online_at).is_none(),
                "never shown Offline"
            );
            world
                .playback(0)
                .send(PlaybackEvent::Finished {
                    generation: 1,
                    played: true,
                })
                .await
                .unwrap();
            controls.send(Control::Say("again".into())).await.unwrap();
            until("new speaker", SOON, || {
                world.log.count("start turn-again on 3") == 1
            })
            .await;
        });
    }

    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_closes_while_waiting_is_dropped() {
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls.send(Control::Say("story".into())).await.unwrap();
            until("speech", SOON, || {
                world.log.count("start turn-story on 0") == 1
            })
            .await;
            world
                .playback(0)
                .send(PlaybackEvent::Started { generation: 1 })
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(10 * 60 + 5)).await;
            assert_eq!(world.log.count("probe 1"), 1, "waiting for the turn to end");
            world.close(1);
            until("dropped", SOON, || world.log.count("disconnect 1") == 1).await;
            assert_eq!(face.steady(), Mood::Speaking);
            tokio::time::sleep(ROTATE_RETRY + SOON).await;
            assert_eq!(world.log.count("probe 2"), 1, "retried");
            tokio::time::sleep(Duration::from_mins(4)).await;
            assert_eq!(world.log.count("disconnect 0"), 1, "the retry took over");
        });
        assert_eq!(rig.world.connections(), 3);
    }

    /// A catch-up replay hangs (only replays): a new question is still
    /// submitted and its speech requested; the replay runs again after.
    #[tokio::test(start_paused = true)]
    async fn a_question_is_submitted_while_a_catch_up_replay_hangs() {
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        let controls = rig.controls.clone();
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            let replays = world.log.count("replay");
            let entered = world.entered.count("replay");
            world.replays_held.send_replace(true);
            world
                .push(0)
                .send(event(9, "old", "text.delta"))
                .await
                .unwrap();
            until("held replay", SOON, || {
                world.entered.count("replay") > entered
            })
            .await;
            controls.send(Control::Say("now".into())).await.unwrap();
            until("submitted", SOON, || world.log.count("submit now") == 1).await;
            until("speech", SOON, || {
                world.log.count("start turn-now on 0") == 1
            })
            .await;
            assert_eq!(
                world.log.count("replay"),
                replays,
                "the replay is still held"
            );
            world.replays_held.send_replace(false);
            until("replayed after", SOON, || {
                world.log.count("replay") > replays
            })
            .await;
        });
        let log = &rig.world.log;
        assert!(log.position("submit now") < log.last_position("replay"));
    }

    /// The C1 order end to end: the submit reaches the server, "Hey Pip"
    /// replaces it, the connection drops before the answer, and the robot
    /// reconnects with nothing new to say. The request is cancelled by ID.
    #[tokio::test(start_paused = true)]
    async fn a_request_replaced_mid_submit_is_cancelled_after_reconnect() {
        let mut rig = Rig::quick();
        rig.world.acks_held.send_replace(true);
        let world = rig.world.clone();
        let controls = rig.controls.clone();
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls.send(Control::Say("first".into())).await.unwrap();
            until("submit reached the server", SOON, || {
                world.log.count("submit first") == 1
            })
            .await;
            controls.send(Control::Wake).await.unwrap();
            world.close(0);
            until("reconnected", Duration::from_mins(1), || {
                world.connections() == 2
            })
            .await;
            world.acks_held.send_replace(false);
            until("request cancelled", SOON, || {
                world.log.count("cancel_request") == 1
            })
            .await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let log = &rig.world.log;
        let submit = log.entries()[log.position("submit first")].clone();
        let request = submit.rsplit(' ').next().unwrap().to_owned();
        assert_eq!(
            log.entries()[log.position("cancel_request")],
            format!("cancel_request {request}")
        );
        assert_eq!(log.count("submit"), 1, "never resubmitted");
        assert_eq!(log.count("start"), 0, "never spoken");
        assert!(rig.state.cache.cancel_requests.is_empty());
    }

    /// The owner speaks the code from their phone; the STT process sends it
    /// as a control line. Wrong codes go to the server (it counts them) and
    /// show the apology face; the right one registers the robot at once.
    #[tokio::test(start_paused = true)]
    async fn a_spoken_code_confirms_the_claim_and_registers_at_once() {
        let mut rig = Rig::quick();
        *rig.world.owned.lock().unwrap() = false;
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls
                .send(Control::Confirm("123456".into()))
                .await
                .unwrap();
            until("refused: no claim", SOON, || {
                face.flashing() == Some(Mood::Sorry)
            })
            .await;
            assert_eq!(world.log.count("confirm"), 0, "nothing to confirm yet");
            // An owner signed in on their phone.
            *world.confirming.lock().unwrap() = true;
            until("claim window", Duration::from_secs(10), || {
                face.steady() == Mood::Confirming
            })
            .await;
            // Wake and mute still show over the claim face, then it returns.
            controls.send(Control::Listening(true)).await.unwrap();
            until("listening over the claim", SOON, || {
                face.steady() == Mood::Listening
            })
            .await;
            controls.send(Control::Listening(false)).await.unwrap();
            until("back to the claim", SOON, || {
                face.steady() == Mood::Confirming
            })
            .await;
            tokio::time::sleep(face::SORRY).await;
            controls
                .send(Control::Confirm("000000".into()))
                .await
                .unwrap();
            until("wrong code", SOON, || {
                world.log.count("confirm 000000") == 1
            })
            .await;
            until("apology", SOON, || face.flashing() == Some(Mood::Sorry)).await;
            let registered = world.entered.count("register");
            controls
                .send(Control::Confirm("123456".into()))
                .await
                .unwrap();
            until("right code", SOON, || {
                world.log.count("confirm 123456") == 1
            })
            .await;
            until("registered at once", Duration::from_millis(100), || {
                world.entered.count("register") > registered
            })
            .await;
            until("ready", SOON, || face.steady() == Mood::Idle).await;
            controls.send(Control::Say("hello".into())).await.unwrap();
            until("owned robots talk", SOON, || {
                world.log.count("submit hello") == 1
            })
            .await;
        });
    }

    #[tokio::test(start_paused = true)]
    async fn an_owned_robot_forgets_its_claim_link() {
        let mut rig = Rig::quick();
        *rig.world.owned.lock().unwrap() = false;
        *rig.world.claim.lock().unwrap() = Some(Claim {
            url: "https://buddy.example/claim?bot=b&token=t".into(),
            expires: 99,
        });
        let world = rig.world.clone();
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            *world.owned.lock().unwrap() = true;
            until("registered again", SOON, || {
                world.entered.count("register") == 2
            })
            .await;
        });
        assert!(rig.state.cache.claim_url.is_empty());
        assert_eq!(rig.state.cache.claim_expires, 0);
    }

    #[test]
    fn rotation_is_scheduled_ahead_of_expiry_but_not_too_early() {
        let now = Instant::now();
        assert_eq!(
            refresh_at(now, now + Duration::from_mins(15)),
            now + Duration::from_mins(10)
        );
        assert_eq!(
            refresh_at(now, now + Duration::from_mins(2)),
            now + MIN_SESSION
        );
        let _ = Cache::default();
    }

    #[tokio::test(start_paused = true)]
    async fn claim_url_is_emitted_once_per_new_url_and_reset_when_owned() {
        use std::io::{BufRead, BufReader};
        let (reader, writer) = std::io::pipe().unwrap();
        let journal = Journal::to(crate::config::EventDetail::Compact, writer).unwrap();
        let mut rig = Rig::quick();
        *rig.world.owned.lock().unwrap() = false;
        *rig.world.claim.lock().unwrap() = Some(Claim {
            url: "https://buddy.example/claim?bot=b&token=t1".into(),
            expires: 99,
        });
        let world = rig.world.clone();
        let options = Options {
            journal,
            ..options()
        };
        run_with!(rig, options, async {
            // Multiple 3 s polls with the same claim URL emit it once.
            until("first poll", SOON, || world.entered.count("register") == 1).await;
            tokio::time::sleep(REGISTER_UNOWNED * 2 + Duration::from_secs(1)).await;
            assert!(world.entered.count("register") >= 3);
            // A newly issued claim URL is emitted once.
            *world.claim.lock().unwrap() = Some(Claim {
                url: "https://buddy.example/claim?bot=b&token=t2".into(),
                expires: 199,
            });
            let before = world.entered.count("register");
            until("new claim polled", Duration::from_secs(10), || {
                world.entered.count("register") >= before + 2
            })
            .await;
            // Becoming owned clears the dedup state; a subsequent claim re-emits.
            *world.owned.lock().unwrap() = true;
            let before = world.entered.count("register");
            until("owned polled", Duration::from_secs(10), || {
                world.entered.count("register") > before
            })
            .await;
            *world.owned.lock().unwrap() = false;
            *world.claim.lock().unwrap() = Some(Claim {
                url: "https://buddy.example/claim?bot=b&token=t2".into(),
                expires: 299,
            });
            let before = world.entered.count("register");
            until("revoked claim polled", REGISTER_OWNED + SOON, || {
                world.entered.count("register") > before
            })
            .await;
        });
        let lines: Vec<String> = BufReader::new(reader).lines().map(Result::unwrap).collect();
        assert_eq!(
            lines,
            [
                "claim url=https://buddy.example/claim?bot=b&token=t1",
                "claim url=https://buddy.example/claim?bot=b&token=t2",
                "claim url=https://buddy.example/claim?bot=b&token=t2",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wake_then_replacement_say_submits_before_a_held_replay_releases() {
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls.send(Control::Say("first".into())).await.unwrap();
            until("first speech", SOON, || {
                world.log.count("start turn-first on 0") == 1
            })
            .await;
            world
                .playback(0)
                .send(PlaybackEvent::Started { generation: 1 })
                .await
                .unwrap();
            until("speaking", SOON, || face.steady() == Mood::Speaking).await;

            // Hold an in-flight replay on the main lane.
            let replays = world.log.count("replay");
            let entered = world.entered.count("replay");
            world.replays_held.send_replace(true);
            world
                .push(0)
                .send(event(9, "turn-first", "text.delta"))
                .await
                .unwrap();
            until("held replay", SOON, || {
                world.entered.count("replay") > entered
            })
            .await;

            // Wake cancels the live turn alongside the held replay, then a
            // replacement Say preempts the held replay and submits immediately.
            controls.send(Control::Wake).await.unwrap();
            until("cancelled", SOON, || {
                world.log.count("cancel turn-first") == 1
            })
            .await;
            controls
                .send(Control::Say("replacement".into()))
                .await
                .unwrap();
            until("replacement submitted", SOON, || {
                world.log.count("submit replacement") == 1
            })
            .await;
            until("replacement speech", SOON, || {
                world.log.count("start turn-replacement on 0") == 1
            })
            .await;
            assert_eq!(
                world.log.count("replay"),
                replays,
                "replacement submitted before the held replay was released"
            );

            world.replays_held.send_replace(false);
            until("replayed after", SOON, || {
                world.log.count("replay") > replays
            })
            .await;
        });
        let log = &rig.world.log;
        assert!(log.position("cancel turn-first") < log.position("submit replacement"));
        assert!(log.position("submit replacement") < log.last_position("replay"));
    }

    #[tokio::test(start_paused = true)]
    async fn unowned_and_claiming_spoken_digits_are_never_persisted_or_submitted_after_confirm() {
        let persisted_pending = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let seen = persisted_pending.clone();
        let mut rig = Rig::new(Arc::new(move |bytes| {
            let cache: Cache = serde_json::from_slice(bytes).unwrap();
            seen.lock()
                .unwrap()
                .push(cache.pending.map(|pending| pending.text));
            Ok(())
        }));
        *rig.world.owned.lock().unwrap() = false;
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            // Spoken digits while unowned are refused fail-closed, never queued or persisted.
            controls.send(Control::Say("123456".into())).await.unwrap();
            until("unowned apology", SOON, || {
                face.flashing() == Some(Mood::Sorry)
            })
            .await;
            assert_eq!(face.steady(), Mood::Idle);
            tokio::time::sleep(face::SORRY + Duration::from_millis(50)).await;

            // Spoken digits as Say while claiming are also refused fail-closed.
            *world.confirming.lock().unwrap() = true;
            until("confirming", Duration::from_secs(10), || {
                face.steady() == Mood::Confirming
            })
            .await;
            controls.send(Control::Say("123456".into())).await.unwrap();
            until("claiming apology", SOON, || {
                face.flashing() == Some(Mood::Sorry)
            })
            .await;
            assert_eq!(face.steady(), Mood::Confirming);

            // Confirming and registering owned must NOT submit the prior spoken digits.
            controls
                .send(Control::Confirm("123456".into()))
                .await
                .unwrap();
            until("ready", SOON, || face.steady() == Mood::Idle).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            assert_eq!(world.log.count("submit"), 0);

            // Once owned is verified, ordinary utterances submit normally.
            controls
                .send(Control::Say("after claim".into()))
                .await
                .unwrap();
            until("owned submit", SOON, || {
                world.log.count("submit after claim") == 1
            })
            .await;
        });
        assert_eq!(rig.world.log.count("submit 123456"), 0);
        assert!(
            !persisted_pending
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.as_deref() == Some("123456")),
            "pre-claim digits must never be persisted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cached_pending_is_cleared_when_registration_confirms_unowned() {
        let mut rig = Rig::quick();
        *rig.world.owned.lock().unwrap() = false;
        rig.state.cache.pending = Some(crate::cache::Pending {
            request_id: "preclaim-req".into(),
            text: "123456".into(),
            created: auth::now(),
        });
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("unowned register", SOON, || {
                world.entered.count("register") == 1
            })
            .await;
            until("cleared from Thinking", SOON, || {
                face.steady() == Mood::Idle
            })
            .await;
            *world.confirming.lock().unwrap() = true;
            until("confirming", Duration::from_secs(10), || {
                face.steady() == Mood::Confirming
            })
            .await;
            controls
                .send(Control::Confirm("123456".into()))
                .await
                .unwrap();
            until("owned", SOON, || face.steady() == Mood::Idle).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        assert_eq!(rig.world.log.count("submit"), 0);
        assert!(rig.state.cache.pending.is_none());
    }

    struct ScriptedVoice {
        synth: String,
        player: String,
    }

    impl crate::piper::Processes for ScriptedVoice {
        fn synth(&self) -> tokio::process::Command {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.args(["-c", &self.synth]);
            cmd
        }
        fn player(&self, _: robot_buddy::audio::PcmFormat) -> tokio::process::Command {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.args(["-c", &self.player])
                .stdout(std::process::Stdio::null());
            cmd
        }
    }

    #[tokio::test]
    async fn offline_announcement_clears_busy_on_natural_finish() {
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        let (listen_tx, listen_rx) = watch::channel(crate::listen::State::default());
        let mut opts = options();
        opts.peripherals.listen = Some(listen_tx);
        opts.peripherals.piper = Some(crate::piper::OfflineVoice::for_test(
            22050,
            Arc::new(ScriptedVoice {
                synth: "printf '1234'".into(),
                player: "cat >/dev/null; sleep 0.05".into(),
            }),
        ));
        run_with!(rig, opts, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            world.stall(true);
            world.close(0);
            until("busy offline", SOON, || listen_rx.borrow().busy).await;
            until("idle after offline announcement finishes", SOON, || {
                !listen_rx.borrow().busy
            })
            .await;
            world.stall(false);
        });
    }

    #[tokio::test]
    async fn offline_announcement_reaps_before_online_playback_on_reconnect() {
        let dir = std::env::temp_dir().join(format!("buddy-dev-off-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("offline.pid");
        let mut rig = Rig::quick();
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        let (listen_tx, listen_rx) = watch::channel(crate::listen::State::default());
        let mut opts = options();
        opts.peripherals.listen = Some(listen_tx);
        opts.peripherals.piper = Some(crate::piper::OfflineVoice::for_test(
            22050,
            Arc::new(ScriptedVoice {
                synth: "printf '1234'".into(),
                player: format!("echo $$ > '{}'; exec sleep 30", pid_file.display()),
            }),
        ));
        run_with!(rig, opts, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            world.stall(true);
            world.close(0);
            until("busy during offline announcement", SOON, || {
                listen_rx.borrow().busy
            })
            .await;
            let deadline = Instant::now() + SOON;
            let pid: i32 = loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                assert!(Instant::now() < deadline, "offline player pid missing");
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            // Reconnect: go_online cancels and reaps offline player before returning.
            world.stall(false);
            until("reconnected", Duration::from_secs(10), || {
                world.connections() == 2 && face.steady() == Mood::Idle
            })
            .await;
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(
                !alive.success(),
                "offline player pid {pid} still alive after reconnect"
            );
            assert!(!listen_rx.borrow().busy);
            until("re-registered", SOON, || {
                world.entered.count("register") >= 2
            })
            .await;
            controls
                .send(Control::Say("hello again".into()))
                .await
                .unwrap();
            until("replacement online playback", SOON, || {
                world.log.count("start turn-hello again on 1") == 1
            })
            .await;
        });
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn long_claim_and_long_reply_do_not_prematurely_sleep_in_device_loop() {
        let mut rig = Rig::quick();
        *rig.world.owned.lock().unwrap() = false;
        *rig.world.confirming.lock().unwrap() = true;
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("confirming", SOON, || face.steady() == Mood::Confirming).await;
            tokio::time::sleep(Duration::from_mins(6)).await;
            controls
                .send(Control::Confirm("123456".into()))
                .await
                .unwrap();
            until("idle after long claim", SOON, || {
                face.steady() == Mood::Idle
            })
            .await;
            tokio::time::sleep(Duration::from_secs(10)).await;
            assert_eq!(
                face.steady(),
                Mood::Idle,
                "must not rest immediately after long claim"
            );

            controls
                .send(Control::Say("long reply".into()))
                .await
                .unwrap();
            until("speech started", SOON, || {
                world.log.count("start turn-long reply on 0") == 1
            })
            .await;
            world
                .playback(0)
                .send(PlaybackEvent::Started { generation: 1 })
                .await
                .unwrap();
            until("speaking", SOON, || face.steady() == Mood::Speaking).await;
            tokio::time::sleep(Duration::from_mins(6)).await;
            world
                .push(0)
                .send(event(1, "turn-long reply", "turn.completed"))
                .await
                .unwrap();
            world
                .playback(0)
                .send(PlaybackEvent::Finished {
                    generation: 1,
                    played: true,
                })
                .await
                .unwrap();
            until("idle after long reply", SOON, || {
                face.steady() == Mood::Idle
            })
            .await;
            tokio::time::sleep(Duration::from_secs(10)).await;
            assert_eq!(
                face.steady(),
                Mood::Idle,
                "must not rest immediately after long reply"
            );
            tokio::time::sleep(Duration::from_mins(5)).await;
            assert_eq!(face.steady(), Mood::Resting);
        });
    }
}
