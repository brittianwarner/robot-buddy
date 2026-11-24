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
//! session connects in the background, takes over once no call is in flight
//! and no turn is live (or shortly before expiry), and audio already
//! playing finishes on the old speaker. The old connection is then closed.
use crate::{
    audio::{PlaybackEvent, Speaker},
    backoff::{Backoff, HEALTHY_SESSION, LINK_CHECK_AFTER, LinkAction, LinkWatch},
    cache::CacheStore,
    config::EventDetail,
    controller::{Claim, Controller, Done, Lost, Registration, Remote, RobotState},
    face::Face,
    input::{Control, Input},
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

#[derive(Clone, Copy)]
pub struct Options {
    pub detail: EventDetail,
    /// How long the network must stay down before Wi-Fi setup reopens.
    pub wifi_grace: Duration,
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
    let controller = Controller::new(face, store, state, options.detail, auth::now());
    let mut device = Device::new(connector, controller, options);
    device.status("Connecting…");
    device.connecting = Some(device.connector.connect().boxed_local());
    tokio::pin!(shutdown);
    loop {
        device.tick().await;
        let wake = device.wake_at();
        let accepts = device.controller.accepts_events();
        let background = !device.background.is_empty();
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            control = controls.next() => device.controller.control(control).await,
            event = device.controller.next_playback() => device.controller.playback(event),
            done = wait(&mut device.cancel_lane) => device.finish(done).await,
            done = wait(&mut device.main_lane) => device.finish(done).await,
            Some(event) = recv(&mut device.events), if accepts => device.pushed(event).await,
            () = closed(device.closed.as_ref()) => device.lose("connection closed").await,
            registration = wait(&mut device.register_lane) => device.registered(registration).await,
            persisted = wait(&mut device.persist_lane) => {
                device.controller.persisted(persisted, Instant::now());
            }
            session = wait(&mut device.connecting) => device.connected(session)?,
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
    persist_lane: Slot<crate::controller::Persisted>,
    connecting: Slot<Opened<C>>,
    refreshing: Slot<Opened<C>>,
    checking: Slot<Option<bool>>,
    setup: Slot<()>,
    /// Disconnects and panel updates nothing waits for.
    background: FuturesUnordered<LocalBoxFuture<'static, ()>>,
}

impl<'a, C: Connector> Device<'a, C> {
    fn new(connector: C, controller: Controller<'a, C::Speaker>, options: Options) -> Self {
        Self {
            connector,
            controller,
            options,
            backoff: Backoff::new(RETRY_BASE, RETRY_MAX),
            link: LinkWatch::new(BOOT_GRACE.min(options.wifi_grace)),
            connected_once: false,
            live: None,
            events: None,
            closed: None,
            next: None,
            retry_at: None,
            cancel_lane: None,
            main_lane: None,
            register_lane: None,
            persist_lane: None,
            connecting: None,
            refreshing: None,
            checking: None,
            setup: None,
            background: FuturesUnordered::new(),
        }
    }

    /// Start whatever is due. Never waits on I/O.
    async fn tick(&mut self) {
        let now = Instant::now();
        self.controller.expire(now);
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
            self.refreshing = Some(self.connector.connect().boxed_local());
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

    fn connected(&mut self, opened: Opened<C>) -> Result<()> {
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
        });
        self.controller.go_online(session.speaker, session.playback);
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
        self.refreshing = None;
        self.events = None;
        self.closed = None;
        self.teardown(live.remote);
        if let Some(next) = self.next.take() {
            self.teardown(next.remote);
        }
        self.controller.go_offline().await;
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
        if let Some(claim) = registration.claim {
            self.controller.update_cache(|cache| {
                cache.claim_url = claim.url;
                cache.claim_expires = claim.expires;
            });
        }
        if let Some(live) = self.live.as_mut() {
            live.owned = registration.owned;
            let every = if live.owned {
                REGISTER_OWNED
            } else {
                REGISTER_UNOWNED
            };
            live.register_at = Instant::now() + every;
        }
    }

    /// Silence, then close the connections briefly; the caller flushes the cache.
    async fn stop(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cache::Cache,
        controller::{CallError, Submitted, tests::Log, tests::Server, tests::event},
        face::{self, LineSink, Mood, Opener},
    };
    use std::{io, sync::Mutex};
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
            self.world.server.submit(request_id, text)
        }
        async fn cancel(&self, turn_id: &str) -> Result<(), CallError> {
            self.world.network("cancel").await;
            self.world.server.cancel(turn_id)
        }
        async fn replay(&self, after: i64) -> Result<Vec<OutputEvent>, CallError> {
            self.world.network("replay").await;
            Ok(self.world.server.replay(after))
        }
        async fn open_turns(&self) -> Result<Vec<String>, CallError> {
            self.world.network("open_turns").await;
            Ok(self.world.server.open_turns())
        }
        async fn register(&self, _: Claim) -> Result<Registration> {
            self.world.network("register").await;
            Ok(Registration {
                owned: *self.world.owned.lock().unwrap(),
                claim: None,
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
        async fn stop(&mut self) {
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
        async fn open(&mut self) -> io::Result<PanelSink> {
            Ok(PanelSink(self.0.clone()))
        }
    }
    impl LineSink for PanelSink {
        async fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            let line = String::from_utf8_lossy(line).trim().to_owned();
            self.0
                .lock()
                .unwrap()
                .push((std::time::Instant::now(), line));
            Ok(())
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

    const OPTIONS: Options = Options {
        detail: EventDetail::Compact,
        wifi_grace: Duration::from_mins(5),
    };

    /// Run the device until `script` finishes.
    macro_rules! run_with {
        ($rig:ident, $script:expr) => {{
            let connector = FakeConnector($rig.world.clone());
            run(
                connector,
                &mut $rig.input,
                &$rig.face,
                &$rig.store,
                &mut $rig.state,
                OPTIONS,
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

    /// "Hey Pip" while every actor call hangs and each disk write takes
    /// 100 ms: the speaker stops and the listening LED is written at once,
    /// and the cancel still goes out alongside the stalled replay.
    #[tokio::test]
    async fn barge_in_while_calls_are_stalled_stops_within_a_bound() {
        let slow_disk: crate::cache::WriteFn = Arc::new(|_| {
            std::thread::sleep(Duration::from_millis(100));
            Ok(())
        });
        let mut rig = Rig::new(slow_disk);
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
                world.stall(false);
                until("cancelled", SOON, || world.log.count(&cancel) == 1).await;
                controls.send(Control::Listening(false)).await.unwrap();
            }
        });
        stops.sort();
        leds.sort();
        eprintln!("wake -> player stopped: {stops:?}");
        eprintln!("wake -> listening LED written: {leds:?}");
        let bound = Duration::from_millis(50);
        assert!(
            stops.iter().chain(&leds).all(|d| *d < bound),
            "{stops:?} {leds:?}"
        );
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
    async fn an_utterance_nobody_can_answer_expires() {
        let mut rig = Rig::quick();
        *rig.world.owned.lock().unwrap() = false;
        let world = rig.world.clone();
        let (face, controls) = (rig.face.clone(), rig.controls.clone());
        run_with!(rig, async {
            until("online", SOON, || world.entered.count("register") == 1).await;
            controls.send(Control::Say("hi".into())).await.unwrap();
            until("thinking", SOON, || face.steady() == Mood::Thinking).await;
            tokio::time::sleep(Duration::from_secs(59)).await;
            assert_eq!(face.steady(), Mood::Thinking);
            until("expired", SOON, || face.flashing() == Some(Mood::Sorry)).await;
            assert_eq!(face.steady(), Mood::Idle);
            // Registered later: the stale utterance is not submitted.
            *world.owned.lock().unwrap() = true;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        assert_eq!(rig.world.log.count("submit"), 0);
        assert!(rig.state.cache.pending.is_none());
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
            OPTIONS,
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
}
