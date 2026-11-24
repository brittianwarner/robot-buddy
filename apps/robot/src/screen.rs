//! Loopback HTML panel for setup and registration (QR codes, owner confirmation).
//! The ESP32 face is a separate output; see `face.rs`.
use anyhow::{Result, bail};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use robot_buddy::auth;
use serde::Deserialize;
use std::{
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, RwLock, Semaphore},
    time::Sleep,
};

/// Where the panel listens unless `BUDDY_SCREEN_ADDR` says otherwise.
const DEFAULT_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8090);
/// Shared by the face panel and the `SoftAP` setup form.
pub const LOCAL_CSP: &str =
    "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Local pages are tiny: a few connections at once, each closed after a
/// fixed time whatever it is doing. `axum::serve` has no header read
/// timeout of its own, so a client that sends half a request would
/// otherwise hold its connection (and a task) forever.
pub const MAX_CONNECTIONS: usize = 16;
pub const CONNECTION_LIFETIME: Duration = Duration::from_secs(10);

/// A TCP listener for local pages that bounds open connections and their
/// lifetime. Shared by the face panel and the `SoftAP` setup form.
pub struct Bounded {
    inner: TcpListener,
    slots: Arc<Semaphore>,
    lifetime: Duration,
}

impl Bounded {
    pub fn new(inner: TcpListener, connections: usize, lifetime: Duration) -> Self {
        Self {
            inner,
            slots: Arc::new(Semaphore::new(connections)),
            lifetime,
        }
    }

    /// The bounds every local page uses.
    pub fn local(inner: TcpListener) -> Self {
        Self::new(inner, MAX_CONNECTIONS, CONNECTION_LIFETIME)
    }
}

impl axum::serve::Listener for Bounded {
    type Io = Limited;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Limited, SocketAddr) {
        // Wait for a free slot before accepting: extra clients queue in the
        // kernel backlog instead of holding tasks here.
        let Ok(slot) = self.slots.clone().acquire_owned().await else {
            unreachable!("the semaphore is never closed");
        };
        // Retries accept errors with axum's back-off.
        let (io, address) = axum::serve::Listener::accept(&mut self.inner).await;
        let io = Limited {
            io,
            deadline: Box::pin(tokio::time::sleep(self.lifetime)),
            _slot: slot,
        };
        (io, address)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// One accepted connection. Reads and writes fail with `TimedOut` once its
/// lifetime is over, which makes hyper drop it and free its slot.
pub struct Limited {
    io: TcpStream,
    deadline: Pin<Box<Sleep>>,
    _slot: OwnedSemaphorePermit,
}

impl Limited {
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.deadline.as_mut().poll(cx).is_ready()
    }
}

fn timed_out<T>() -> Poll<io::Result<T>> {
    Poll::Ready(Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "local page connection lifetime over",
    )))
}

impl AsyncRead for Limited {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return timed_out();
        }
        Pin::new(&mut this.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Limited {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return timed_out();
        }
        Pin::new(&mut this.io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return timed_out();
        }
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Closing is always allowed.
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// Adds the headers every local page carries. `same-origin` (not `no-referrer`) so form
/// POSTs still send a real `Origin`.
pub fn local_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(LOCAL_CSP),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
}

/// The header's value when it is one of `allowed`.
pub fn header_in(headers: &HeaderMap, name: header::HeaderName, allowed: &[&str]) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| allowed.contains(&v))
}

/// Where the panel listens, from `BUDDY_SCREEN_ADDR`: unset means
/// `127.0.0.1:8090`; empty or `off` turns the panel off; anything else must be a
/// loopback `ip:port`, because the panel has no login and confirms owners.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    Off,
    At(SocketAddr),
    /// Refused by `serve`, so a typo fails at startup instead of silently.
    Invalid(String),
}

impl Address {
    pub fn from_env() -> Self {
        Self::parse(std::env::var("BUDDY_SCREEN_ADDR").ok().as_deref())
    }

    pub fn parse(value: Option<&str>) -> Self {
        let Some(value) = value.map(str::trim) else {
            return Self::At(DEFAULT_ADDRESS);
        };
        if value.is_empty() || value.eq_ignore_ascii_case("off") {
            return Self::Off;
        }
        match value.parse::<SocketAddr>() {
            Ok(address) if address.ip().is_loopback() && address.port() != 0 => Self::At(address),
            Ok(_) => Self::Invalid("BUDDY_SCREEN_ADDR must be a loopback ip:port".into()),
            Err(_) => Self::Invalid("BUDDY_SCREEN_ADDR is not ip:port or off".into()),
        }
    }
}

/// `Host` and `Origin` values the panel answers. Anything else (DNS
/// rebinding, cross-site POSTs) is refused.
struct Allowed {
    hosts: [String; 2],
    origins: [String; 2],
}

impl Allowed {
    fn for_address(address: &Address) -> Self {
        let address = match address {
            Address::At(address) => *address,
            Address::Off | Address::Invalid(_) => DEFAULT_ADDRESS,
        };
        let hosts = [address.to_string(), format!("localhost:{}", address.port())];
        let origins = hosts.clone().map(|host| format!("http://{host}"));
        Self { hosts, origins }
    }

    fn host(&self, headers: &HeaderMap) -> bool {
        header_in(
            headers,
            header::HOST,
            &self.hosts.each_ref().map(String::as_str),
        )
    }

    fn origin(&self, headers: &HeaderMap) -> bool {
        header_in(
            headers,
            header::ORIGIN,
            &self.origins.each_ref().map(String::as_str),
        )
    }
}

#[derive(Default)]
struct Panel {
    message: String,
    /// The value `qr` was rendered from, so an unchanged code is not redrawn.
    qr_value: String,
    qr: String,
    code: String,
    confirm: bool,
    /// Rotated whenever the pending confirmation code changes.
    csrf: String,
}

#[derive(Clone)]
pub struct Screen {
    panel: Arc<RwLock<Panel>>,
    address: Arc<Address>,
    allowed: Arc<Allowed>,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    /// A panel at `BUDDY_SCREEN_ADDR` (see [`Address`]).
    pub fn new() -> Self {
        Self::at(Address::from_env())
    }

    pub fn at(address: Address) -> Self {
        Self {
            panel: Arc::new(RwLock::new(Panel {
                csrf: auth::random(),
                ..Panel::default()
            })),
            allowed: Arc::new(Allowed::for_address(&address)),
            address: Arc::new(address),
        }
    }

    /// Show a message and optionally a QR code for `qr`. The session repeats
    /// the same code every few seconds while unclaimed; it is rendered once.
    pub async fn show(&self, message: &str, qr: &str) -> Result<()> {
        let rendered = {
            let panel = self.panel.read().await;
            panel.qr_value == qr
        };
        let svg = match (rendered, qr.is_empty()) {
            (true, _) => None,
            (false, true) => Some(String::new()),
            (false, false) => Some(svg(qr)?),
        };
        let mut panel = self.panel.write().await;
        if message != panel.message {
            message.clone_into(&mut panel.message);
        }
        if let Some(svg) = svg {
            panel.qr = svg;
            qr.clone_into(&mut panel.qr_value);
        }
        Ok(())
    }

    /// Show the ready message and clear registration details.
    pub async fn ready(&self) {
        let mut panel = self.panel.write().await;
        panel.message = "Ready — hello!".into();
        panel.qr.clear();
        panel.qr_value.clear();
        panel.code.clear();
    }

    /// Display the owner confirmation code. Returns whether the owner pressed
    /// Confirm for this code since the last call. A new code (a new pending claim) gets a
    /// fresh CSRF token and drops a press made for the previous one.
    pub async fn confirmation(&self, code: &str) -> bool {
        let mut panel = self.panel.write().await;
        if panel.code != code {
            panel.code = code.into();
            panel.csrf = auth::random();
            panel.confirm = false;
        }
        std::mem::take(&mut panel.confirm)
    }

    /// Start the panel in the background, unless `BUDDY_SCREEN_ADDR` is off.
    pub async fn serve(&self) -> Result<()> {
        let address = match &*self.address {
            Address::Off => {
                tracing::info!("screen panel off");
                return Ok(());
            }
            Address::Invalid(reason) => bail!("{reason}"),
            Address::At(address) => *address,
        };
        let router = Router::new()
            .route("/", get(page))
            .route("/confirm", post(confirm))
            .layer(DefaultBodyLimit::max(4096))
            .with_state(self.clone());
        let listener = Bounded::local(TcpListener::bind(address).await?);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "face server failed");
            }
        });
        Ok(())
    }
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn svg(value: &str) -> Result<String> {
    Ok(qrcode::QrCode::new(value)?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(320, 320)
        .build())
}

const HEAD: &str = "<!doctype html><html><head>\
    <meta name=viewport content='width=device-width,initial-scale=1'>\
    <meta http-equiv=refresh content=3>\
    <title>Robot Buddy</title></head><body>";

async fn page(State(screen): State<Screen>, headers: HeaderMap) -> Response {
    if !screen.allowed.host(&headers) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }
    let panel = screen.panel.read().await;
    let form = if panel.code.is_empty() {
        String::new()
    } else {
        format!(
            "<form method=post action=/confirm>\
             <input type=hidden name=csrf value=\"{}\">\
             <button>Confirm owner</button></form>",
            escape(&panel.csrf)
        )
    };
    let mut response = Html(format!(
        "{HEAD}<h1>{}</h1>{}<p>{}</p>{form}</body></html>",
        escape(&panel.message),
        panel.qr,
        escape(&panel.code),
    ))
    .into_response();
    local_headers(&mut response);
    response
}

#[derive(Deserialize)]
struct Confirm {
    csrf: String,
}

async fn confirm(
    State(screen): State<Screen>,
    headers: HeaderMap,
    Form(form): Form<Confirm>,
) -> Response {
    let local = screen.allowed.host(&headers) && screen.allowed.origin(&headers);
    let mut panel = screen.panel.write().await;
    if !local || panel.code.is_empty() || !auth::ct_eq(form.csrf.as_bytes(), panel.csrf.as_bytes())
    {
        return (StatusCode::FORBIDDEN, "invalid confirmation").into_response();
    }
    panel.confirm = true;
    let mut response = Redirect::to("/").into_response();
    local_headers(&mut response);
    response
}

#[cfg(test)]
mod tests {
    //! Handlers are called directly with their extractors; no socket is bound.
    use super::*;

    const DEFAULT: &str = "127.0.0.1:8090";

    fn panel() -> Screen {
        Screen::at(Address::parse(None))
    }

    async fn csrf(screen: &Screen) -> String {
        screen.panel.read().await.csrf.clone()
    }

    fn headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        headers
    }

    async fn press(screen: &Screen, csrf: &str, host: &str, origin: Option<&str>) -> StatusCode {
        let form = Form(Confirm { csrf: csrf.into() });
        confirm(State(screen.clone()), headers(host, origin), form)
            .await
            .status()
    }

    #[tokio::test]
    async fn page_is_locked_down() {
        let screen = panel();
        let response = page(State(screen.clone()), headers(DEFAULT, None)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            LOCAL_CSP
        );
        let rebound = page(State(screen), headers("attacker.example:8090", None)).await;
        assert_eq!(rebound.status(), StatusCode::MISDIRECTED_REQUEST);
    }

    #[tokio::test]
    async fn confirm_checks_origin_host_and_rotating_csrf() {
        let screen = panel();
        let origin = Some("http://127.0.0.1:8090");
        // No pending code yet: nothing to confirm.
        let token = csrf(&screen).await;
        let status = press(&screen, &token, DEFAULT, origin).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        assert!(!screen.confirmation("123456").await);
        let first = csrf(&screen).await;
        assert_ne!(first, token, "a new pending claim rotates the token");
        for (csrf, host, origin) in [
            (first.as_str(), DEFAULT, None),
            (first.as_str(), DEFAULT, Some("http://attacker.example")),
            (first.as_str(), "attacker.example:8090", origin),
            ("forged", DEFAULT, origin),
        ] {
            assert_eq!(
                press(&screen, csrf, host, origin).await,
                StatusCode::FORBIDDEN
            );
        }
        assert!(!screen.confirmation("123456").await);
        assert_eq!(csrf(&screen).await, first, "same claim keeps its token");
        let status = press(
            &screen,
            &first,
            "localhost:8090",
            Some("http://localhost:8090"),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(screen.confirmation("123456").await);

        // A press for the old claim does not carry over, and its token is dead.
        let status = press(&screen, &first, DEFAULT, origin).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(!screen.confirmation("654321").await, "stale press dropped");
        let status = press(&screen, &first, DEFAULT, origin).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn the_address_is_loopback_configurable_or_off() {
        let at = |s: &str| Address::At(s.parse().unwrap());
        assert_eq!(Address::parse(None), at(DEFAULT));
        assert_eq!(Address::parse(Some("127.0.0.1:8123")), at("127.0.0.1:8123"));
        assert_eq!(Address::parse(Some("[::1]:8090")), at("[::1]:8090"));
        assert_eq!(Address::parse(Some("")), Address::Off);
        assert_eq!(Address::parse(Some(" OFF ")), Address::Off);
        for bad in [
            "0.0.0.0:8090",
            "192.168.1.5:8090",
            "127.0.0.1:0",
            "8090",
            "localhost:8090",
        ] {
            assert!(
                matches!(Address::parse(Some(bad)), Address::Invalid(_)),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn a_moved_panel_answers_only_its_own_host_and_origin() {
        let screen = Screen::at(Address::parse(Some("127.0.0.1:8123")));
        let ok = page(State(screen.clone()), headers("localhost:8123", None)).await;
        assert_eq!(ok.status(), StatusCode::OK);
        let old = page(State(screen.clone()), headers(DEFAULT, None)).await;
        assert_eq!(old.status(), StatusCode::MISDIRECTED_REQUEST);
        assert!(!screen.confirmation("123456").await);
        let token = csrf(&screen).await;
        let status = press(
            &screen,
            &token,
            "127.0.0.1:8123",
            Some("http://127.0.0.1:8090"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let status = press(
            &screen,
            &token,
            "127.0.0.1:8123",
            Some("http://127.0.0.1:8123"),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }

    /// Real sockets, real time: small bounds keep it quick, and the only
    /// latency bound is a hang detector.
    async fn bounded(connections: usize, lifetime: Duration) -> (SocketAddr, Bounded) {
        let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = inner.local_addr().unwrap();
        (address, Bounded::new(inner, connections, lifetime))
    }

    const HANG: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn the_listener_holds_new_connections_while_every_slot_is_taken() {
        use axum::serve::Listener;
        let (address, mut listener) = bounded(2, HANG).await;
        let clients: Vec<_> =
            futures_util::future::join_all((0..3).map(|_| tokio::net::TcpStream::connect(address)))
                .await;
        assert!(clients.iter().all(Result::is_ok));
        let first = listener.accept().await.0;
        let _second = listener.accept().await.0;
        let mut third = Box::pin(listener.accept());
        assert!(
            futures_util::poll!(third.as_mut()).is_pending(),
            "no slot is free"
        );
        drop(first);
        tokio::time::timeout(HANG, third)
            .await
            .expect("a freed slot accepts the waiting client");
    }

    #[tokio::test]
    async fn a_half_sent_request_is_cut_off_and_its_slot_freed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let lifetime = Duration::from_millis(200);
        let (address, listener) = bounded(1, lifetime).await;
        let router = Router::new().route("/", get(|| async { "ok" }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let started = std::time::Instant::now();
        let mut slow = tokio::net::TcpStream::connect(address).await.unwrap();
        slow.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n")
            .await
            .unwrap();
        let mut rest = Vec::new();
        // Closed (EOF or reset) without an answer.
        let _closed = tokio::time::timeout(HANG, slow.read_to_end(&mut rest))
            .await
            .expect("the half-sent request is cut off");
        assert!(started.elapsed() >= lifetime);
        assert!(rest.is_empty(), "{}", String::from_utf8_lossy(&rest));
        // The only slot is free again: a whole request is answered.
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(HANG, client.read_to_end(&mut answer))
            .await
            .expect("answered")
            .unwrap();
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        server.abort();
    }

    #[tokio::test]
    async fn an_off_or_invalid_panel_binds_nothing() {
        Screen::at(Address::Off).serve().await.unwrap();
        let invalid = Screen::at(Address::parse(Some("0.0.0.0:8090")))
            .serve()
            .await;
        assert!(invalid.is_err());
    }

    #[tokio::test]
    async fn an_unchanged_qr_code_is_rendered_once() {
        let screen = panel();
        screen
            .show("Scan to claim", "https://example.test/claim#a")
            .await
            .unwrap();
        let first = screen.panel.read().await.qr.clone();
        assert!(first.starts_with("<?xml") || first.contains("<svg"));
        // Mark the cached render; an unchanged code must keep it.
        screen.panel.write().await.qr = "cached".into();
        screen
            .show("Still waiting", "https://example.test/claim#a")
            .await
            .unwrap();
        assert_eq!(screen.panel.read().await.qr, "cached");
        assert_eq!(screen.panel.read().await.message, "Still waiting");
        screen
            .show("New code", "https://example.test/claim#b")
            .await
            .unwrap();
        assert!(screen.panel.read().await.qr.contains("<svg"));
        screen.show("Connecting…", "").await.unwrap();
        assert!(screen.panel.read().await.qr.is_empty());
        screen
            .show("Scan to claim", "https://example.test/claim#b")
            .await
            .unwrap();
        assert!(
            screen.panel.read().await.qr.contains("<svg"),
            "cleared, then redrawn"
        );
    }
}
