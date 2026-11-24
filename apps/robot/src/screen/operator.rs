//! Optional operator console. Only this listener may be forwarded over SSH;
//! the registration panel and hotspot portal are separate authorities.
use super::{Allowed, Screen, escape, local_headers};
use crate::guard;
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Form, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response, Sse, sse::Event},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::stream;
use robot_buddy::auth::{self, Secret};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    convert::Infallible,
    fmt::Write,
    net::SocketAddr,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore};
use tracing_subscriber::Layer;

const MAX_JPEG: usize = 512 * 1024;
const LOG_CAP: usize = 96;

pub enum Control {
    Pose { yaw: f32, pitch: f32, roll: f32 },
    Release,
    Gesture(String),
    Emotion(String),
    Enroll(String),
    Forget(String),
}

/// A local-only adapter. Motion must enforce calibrated limits; enrollment
/// must use a fresh frame with one face. Do not expose embeddings or filenames.
pub trait Hardware: Send + Sync + 'static {
    fn control(&self, command: Control) -> Result<()>;
    fn people(&self) -> Result<Vec<String>>;
    /// Latest fresh local match, never included in the event log.
    fn recognized(&self) -> Result<Option<String>> {
        Ok(None)
    }
    /// Latest bounded JPEG, requested only by an authenticated live viewer.
    fn jpeg(&self) -> Result<Option<Vec<u8>>>;
    /// Latest-state update (nonblocking); `None` clears both physical eyes.
    fn wifi_eyes(&self, pair: Option<(&str, &str)>);
}

/// Only fixed callsite metadata reaches the console. Messages and structured
/// fields can carry private transcripts, credentials or remote error text.
pub struct LogLayer;

#[derive(Default)]
struct Log {
    sequence: u64,
    entries: VecDeque<(u64, String)>,
}

static LOG: LazyLock<Mutex<Log>> = LazyLock::new(Mutex::default);

impl<S: tracing::Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        if !meta.target().starts_with("robot_buddy_robot::") {
            return;
        }
        let mut log = LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = log.sequence.saturating_add(1);
        log.sequence = seq;
        if log.entries.len() == LOG_CAP {
            log.entries.pop_front();
        }
        log.entries.push_back((
            seq,
            format!(
                "{} {}:{}",
                meta.level(),
                meta.target(),
                meta.line().unwrap_or(0)
            ),
        ));
    }
}

#[derive(Clone)]
struct Console {
    screen: Screen,
    allowed: Arc<Allowed>,
    authorization: Arc<Secret<String>>,
    work: Arc<Semaphore>,
    viewers: Arc<Semaphore>,
}

fn address(value: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = value
        .parse()
        .context("BUDDY_OPERATOR_ADDR must be a loopback ip:port")?;
    ensure!(
        addr.ip().is_loopback() && addr.port() != 0,
        "BUDDY_OPERATOR_ADDR must be a loopback ip:port"
    );
    Ok(addr)
}

pub(super) async fn serve(screen: &Screen) -> Result<()> {
    let token = match std::env::var("BUDDY_OPERATOR_TOKEN") {
        Err(std::env::VarError::NotPresent) => return Ok(()),
        Err(_) => anyhow::bail!("invalid operator token"),
        Ok(value) => Secret::new(value),
    };
    ensure!(
        (32..=128).contains(&token.expose().len())
            && token.expose().bytes().all(|b| b.is_ascii_graphic()),
        "BUDDY_OPERATOR_TOKEN must be 32 to 128 printable non-space ASCII bytes"
    );
    let addr =
        address(&std::env::var("BUDDY_OPERATOR_ADDR").unwrap_or_else(|_| "127.0.0.1:8091".into()))?;
    ensure!(
        screen.hardware.is_some(),
        "operator hardware adapter missing"
    );
    let plain = Secret::new(format!("operator:{}", token.expose()));
    let encoded = Secret::new(STANDARD.encode(plain.expose()));
    let console = Console {
        screen: screen.clone(),
        allowed: Arc::new(Allowed::for_address(&super::Address::At(addr))),
        authorization: Arc::new(Secret::new(format!("Basic {}", encoded.expose()))),
        work: Arc::new(Semaphore::new(4)),
        viewers: Arc::new(Semaphore::new(2)),
    };
    let router = routes(console);
    let listener = guard::guard(TcpListener::bind(addr).await?, guard::Limits::PAGE);
    tokio::spawn(async move {
        if axum::serve(listener, router).await.is_err() {
            tracing::error!("operator listener stopped");
        }
    });
    Ok(())
}

fn routes(console: Console) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/control", post(control))
        .route("/camera", get(camera))
        .route("/recognized", get(recognized))
        .route("/events", get(events))
        .route(
            "/console.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    include_str!("operator.js"),
                )
            }),
        )
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn_with_state(console.clone(), protect))
        .with_state(console)
}

fn authorize(console: &Console, headers: &HeaderMap, method: &Method) -> Result<(), StatusCode> {
    if !console.allowed.host(headers) {
        return Err(StatusCode::MISDIRECTED_REQUEST);
    }
    if (method == Method::POST || headers.contains_key(header::ORIGIN))
        && !console.allowed.origin(headers)
        || headers
            .get("sec-fetch-site")
            .is_some_and(|v| v != "same-origin" && v != "none")
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let supplied = headers
        .get(header::AUTHORIZATION)
        .map_or(&[][..], HeaderValue::as_bytes);
    if !auth::ct_eq(supplied, console.authorization.expose().as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

async fn protect(State(c): State<Console>, mut request: Request, next: Next) -> Response {
    let mut response = match authorize(&c, request.headers(), request.method()) {
        Err(StatusCode::UNAUTHORIZED) => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"Robot operator\"")],
        )
            .into_response(),
        Err(status) => status.into_response(),
        Ok(()) => {
            let epoch = c.screen.panel.read().await.operator_epoch.clone();
            if let Some(epoch) = epoch {
                request.extensions_mut().insert(Epoch(epoch));
                next.run(request).await
            } else {
                (
                    StatusCode::FORBIDDEN,
                    "Robot must be connected and claimed.",
                )
                    .into_response()
            }
        }
    };
    local_headers(&mut response);
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; img-src 'self'; script-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"));
    response.headers_mut().insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    response
}

#[derive(Clone)]
struct Epoch(String);

enum RunError {
    Status(StatusCode),
    Hardware(anyhow::Error),
}

impl Console {
    async fn active(&self, epoch: &Epoch) -> bool {
        self.screen.panel.read().await.operator_epoch.as_ref() == Some(&epoch.0)
    }

    /// Limit even disconnected callers: permits live with the synchronous
    /// adapter work. Check the epoch at dispatch, not just at HTTP admission.
    async fn run_raw<T: Send + 'static>(
        &self,
        epoch: &Epoch,
        f: impl FnOnce(&dyn Hardware) -> Result<T> + Send + 'static,
    ) -> Result<T, RunError> {
        let slot = self
            .work
            .clone()
            .try_acquire_owned()
            .map_err(|_| RunError::Status(StatusCode::TOO_MANY_REQUESTS))?;
        let screen = self.screen.clone();
        let epoch = epoch.clone();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let panel = screen.panel.blocking_read();
            if panel.operator_epoch.as_ref() != Some(&epoch.0) {
                return Err(RunError::Status(StatusCode::FORBIDDEN));
            }
            let hardware = screen
                .hardware
                .as_deref()
                .ok_or(RunError::Status(StatusCode::SERVICE_UNAVAILABLE))?;
            drop(panel);
            f(hardware).map_err(RunError::Hardware)
        })
        .await
        .map_err(|_| RunError::Status(StatusCode::SERVICE_UNAVAILABLE))?
    }

    async fn run<T: Send + 'static>(
        &self,
        epoch: &Epoch,
        f: impl FnOnce(&dyn Hardware) -> Result<T> + Send + 'static,
    ) -> Result<T, StatusCode> {
        self.run_raw(epoch, f).await.map_err(|err| match err {
            RunError::Status(status) => status,
            RunError::Hardware(_) => StatusCode::SERVICE_UNAVAILABLE,
        })
    }
}

const GESTURES: &[&str] = &[
    "nod",
    "shake",
    "tilt",
    "look_around",
    "dance",
    "blink",
    "ear_wiggle",
];
const EMOTIONS: &[&str] = &[
    "neutral",
    "happy",
    "curious",
    "thinking",
    "surprised",
    "sad",
    "angry",
    "sleepy",
    "love",
    "asleep",
];

async fn page(
    State(c): State<Console>,
    axum::Extension(epoch): axum::Extension<Epoch>,
) -> Html<String> {
    let mut html = String::from(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>Robot operator</title><h1>Robot operator</h1>",
    );
    let form = format!(
        "<form method=post action=/control><input type=hidden name=csrf value='{}'>",
        escape(&epoch.0)
    );
    let _ = write!(html, "<h2>Neck</h2>{form}");
    for axis in ["yaw", "pitch", "roll"] {
        let _ = write!(
            html,
            "<label>{axis} (degrees) <input type=range name={axis} min=-45 max=45 value=0></label>"
        );
    }
    html.push_str("<button name=action value=pose>Hold pose</button><button name=action value=release>Release</button></form>");
    for (action, names) in [("gesture", GESTURES), ("emotion", EMOTIONS)] {
        let _ = write!(html, "{form}<label>{action} <select name=name>");
        for name in names {
            let _ = write!(html, "<option>{name}</option>");
        }
        let _ = write!(
            html,
            "</select></label><button name=action value={action}>Apply</button></form>"
        );
    }
    let matched = recognized_label(&c, &epoch).await;
    let _ = write!(
        html,
        "<h2>People</h2><p>Latest match: <span id=recognized>{}</span></p>",
        escape(&matched)
    );
    match c.run(&epoch, Hardware::people).await {
        Ok(people) => {
            for name in people.into_iter().filter(|n| person_name(n)).take(256) {
                let name = escape(&name);
                let _ = write!(
                    html,
                    "{form}<input type=hidden name=name value=\"{name}\">{name} <button name=action value=forget>Forget</button></form>"
                );
            }
            let _ = write!(
                html,
                "{form}<label>Name <input name=name maxlength=32 required></label><button name=action value=enroll>Enroll face in view</button></form>"
            );
        }
        Err(_) => html.push_str("<p>Recognition unavailable.</p>"),
    }
    html.push_str("<h2>Camera</h2><button id=camera>Start camera</button><br><img id=preview alt='Camera off'><h2>Device events</h2><pre id=events></pre><script src=/console.js></script></html>");
    Html(html)
}

async fn recognized_label(c: &Console, epoch: &Epoch) -> String {
    c.run(epoch, Hardware::recognized)
        .await
        .ok()
        .flatten()
        .filter(|n| person_name(n))
        .unwrap_or_else(|| "Unknown".into())
}

async fn recognized(
    State(c): State<Console>,
    axum::Extension(epoch): axum::Extension<Epoch>,
) -> String {
    recognized_label(&c, &epoch).await
}

#[derive(Deserialize)]
struct CommandForm {
    csrf: String,
    action: String,
    #[serde(default)]
    name: String,
    yaw: Option<f32>,
    pitch: Option<f32>,
    roll: Option<f32>,
}

fn person_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.trim() == name
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b' ' | b'-' | b'\''))
}

impl CommandForm {
    fn command(self) -> Option<Control> {
        let angle = |v: Option<f32>| v.filter(|v| (-45.0..=45.0).contains(v));
        match self.action.as_str() {
            "pose" => Some(Control::Pose {
                yaw: angle(self.yaw)?,
                pitch: angle(self.pitch)?,
                roll: angle(self.roll)?,
            }),
            "release" => Some(Control::Release),
            "gesture" if GESTURES.contains(&self.name.as_str()) => {
                Some(Control::Gesture(self.name))
            }
            "emotion" if EMOTIONS.contains(&self.name.as_str()) => {
                Some(Control::Emotion(self.name))
            }
            "enroll" if person_name(&self.name) => Some(Control::Enroll(self.name)),
            "forget" if person_name(&self.name) => Some(Control::Forget(self.name)),
            _ => None,
        }
    }
}

/// Map only known static enrollment failures from `vision::Panel::enroll` and
/// `vision::gallery::Gallery::add`. Wrapped or unknown errors stay generic so
/// filesystem paths, `OpenCV` details and other internal state never leave the
/// process.
fn enroll_error(error: &anyhow::Error) -> (StatusCode, &'static str) {
    if error.source().is_some() {
        return (StatusCode::SERVICE_UNAVAILABLE, "Control unavailable.");
    }
    match error.to_string().as_str() {
        "vision is offline" => (StatusCode::SERVICE_UNAVAILABLE, "Vision is offline."),
        "recognition is off" => (StatusCode::SERVICE_UNAVAILABLE, "Recognition is off."),
        "no fresh camera frame" => (
            StatusCode::SERVICE_UNAVAILABLE,
            "No fresh camera frame available.",
        ),
        "show exactly one face at least 40 pixels across" => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Show exactly one face at least 40 pixels across.",
        ),
        "enrollment frame expired" => {
            (StatusCode::CONFLICT, "Enrollment frame expired; try again.")
        }
        "no usable face feature" => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "No usable face feature detected; try again.",
        ),
        "gallery full" => (StatusCode::CONFLICT, "Recognition gallery is full."),
        "person already has eight samples" => (
            StatusCode::CONFLICT,
            "Person already has the maximum eight samples.",
        ),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "Control unavailable."),
    }
}

async fn control(
    State(c): State<Console>,
    axum::Extension(epoch): axum::Extension<Epoch>,
    Form(form): Form<CommandForm>,
) -> Response {
    if !auth::ct_eq(form.csrf.as_bytes(), epoch.0.as_bytes()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(cmd) = form.command() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let is_enroll = matches!(cmd, Control::Enroll(_));
    match c.run_raw(&epoch, move |h| h.control(cmd)).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(RunError::Status(status)) => (status, "Control unavailable.").into_response(),
        Err(RunError::Hardware(error)) if is_enroll => enroll_error(&error).into_response(),
        Err(RunError::Hardware(_)) => {
            (StatusCode::SERVICE_UNAVAILABLE, "Control unavailable.").into_response()
        }
    }
}

fn jpeg_part(jpeg: &[u8]) -> Option<Bytes> {
    if jpeg.len() > MAX_JPEG || !jpeg.starts_with(&[0xff, 0xd8]) || !jpeg.ends_with(&[0xff, 0xd9]) {
        return None;
    }
    let mut buf = format!(
        "--buddy-frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        jpeg.len()
    )
    .into_bytes();
    buf.extend_from_slice(jpeg);
    buf.extend_from_slice(b"\r\n");
    Some(Bytes::from(buf))
}

async fn camera(
    State(c): State<Console>,
    axum::Extension(epoch): axum::Extension<Epoch>,
) -> Response {
    let Ok(slot) = c.viewers.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let first = match c.run(&epoch, Hardware::jpeg).await {
        Ok(Some(jpeg)) => jpeg_part(&jpeg),
        Ok(None) => None,
        Err(StatusCode::SERVICE_UNAVAILABLE) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "Camera unavailable.").into_response();
        }
        Err(status) => return status.into_response(),
    };
    if !c.active(&epoch).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(first) = first else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Camera unavailable.").into_response();
    };
    let feed = stream::unfold((c, epoch, slot, Some(first)), |mut state| async move {
        if let Some(part) = state.3.take() {
            return Some((Ok::<_, Infallible>(part), state));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (c, epoch, _, _) = &state;
        if !c.active(epoch).await {
            return None;
        }
        let jpeg = c.run(epoch, Hardware::jpeg).await.ok()??;
        if !c.active(epoch).await {
            return None;
        }
        let part = jpeg_part(&jpeg)?;
        Some((Ok::<_, Infallible>(part), state))
    });
    (
        [
            (
                header::CONTENT_TYPE,
                "multipart/x-mixed-replace; boundary=buddy-frame",
            ),
            (header::CONNECTION, "close"),
        ],
        Body::from_stream(feed),
    )
        .into_response()
}

fn log_batch(cursor: u64) -> (u64, String) {
    let log = LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rows: Vec<_> = log
        .entries
        .iter()
        .filter(|(id, _)| *id > cursor)
        .map(|(_, s)| s.as_str())
        .collect();
    (log.sequence, rows.join("\n"))
}

async fn events(
    State(c): State<Console>,
    axum::Extension(epoch): axum::Extension<Epoch>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let cursor = headers
        .get("last-event-id")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    Sse::new(stream::unfold(
        (c, epoch, cursor),
        |(c, epoch, cursor)| async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !c.active(&epoch).await {
                return None;
            }
            let (id, lines) = log_batch(cursor);
            let event = if lines.is_empty() {
                Event::default().comment("online")
            } else {
                Event::default().id(id.to_string()).data(lines)
            };
            Some((Ok::<_, Infallible>(event), (c, epoch, id)))
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing_subscriber::prelude::*;

    enum FrameReply {
        Valid,
        Offline,
        Invalid,
        Error(&'static str),
    }

    struct Mock {
        actions: Mutex<Vec<Control>>,
        recognized: Mutex<Option<String>>,
        seen: Mutex<Vec<Option<(String, String)>>>,
        jpeg_reads: AtomicUsize,
        frame: Mutex<FrameReply>,
        control_error: Mutex<Option<Box<dyn Fn() -> anyhow::Error + Send + Sync>>>,
    }

    impl Default for Mock {
        fn default() -> Self {
            Self {
                actions: Mutex::default(),
                recognized: Mutex::default(),
                seen: Mutex::default(),
                jpeg_reads: AtomicUsize::new(0),
                frame: Mutex::new(FrameReply::Valid),
                control_error: Mutex::default(),
            }
        }
    }

    impl Hardware for Mock {
        fn control(&self, command: Control) -> Result<()> {
            self.actions.lock().unwrap().push(command);
            if let Some(make_error) = self.control_error.lock().unwrap().as_ref() {
                return Err(make_error());
            }
            Ok(())
        }
        fn people(&self) -> Result<Vec<String>> {
            Ok(vec!["Ada".into()])
        }
        fn recognized(&self) -> Result<Option<String>> {
            Ok(self.recognized.lock().unwrap().clone())
        }
        fn jpeg(&self) -> Result<Option<Vec<u8>>> {
            self.jpeg_reads.fetch_add(1, Ordering::Relaxed);
            match *self.frame.lock().unwrap() {
                FrameReply::Valid => Ok(Some(vec![255, 216, 0, 255, 217])),
                FrameReply::Offline => Ok(None),
                FrameReply::Invalid => Ok(Some(b"not-a-jpeg".to_vec())),
                FrameReply::Error(message) => Err(anyhow::anyhow!(message)),
            }
        }
        fn wifi_eyes(&self, pair: Option<(&str, &str)>) {
            self.seen
                .lock()
                .unwrap()
                .push(pair.map(|(a, b)| (a.to_owned(), b.to_owned())));
        }
    }

    struct Fixture {
        url: String,
        auth: String,
        work: Arc<Semaphore>,
        viewers: Arc<Semaphore>,
        server: tokio::task::JoinHandle<std::io::Result<()>>,
    }

    async fn guarded_fixture(screen: &Screen, limits: guard::Limits) -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let auth = format!("Basic {}", STANDARD.encode("operator:fixture-password"));
        let work = Arc::new(Semaphore::new(4));
        let viewers = Arc::new(Semaphore::new(2));
        let router = routes(Console {
            screen: screen.clone(),
            allowed: Arc::new(Allowed::for_address(&super::super::Address::At(addr))),
            authorization: Arc::new(Secret::new(auth.clone())),
            work: work.clone(),
            viewers: viewers.clone(),
        });
        let listener = guard::guard(listener, limits);
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        Fixture {
            url,
            auth,
            work,
            viewers,
            server,
        }
    }

    async fn fixture(
        screen: &Screen,
    ) -> (String, String, tokio::task::JoinHandle<std::io::Result<()>>) {
        let f = guarded_fixture(screen, guard::Limits::PAGE).await;
        (f.url, f.auth, f.server)
    }

    #[tokio::test]
    async fn operator_requires_local_authenticated_owned_session_on_every_route() {
        let mock = Arc::new(Mock::default());
        let screen = Screen::at(super::super::Address::Off).with_hardware(mock.clone());
        let (url, auth, server) = fixture(&screen).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        for path in ["", "/events", "/camera", "/recognized", "/console.js"] {
            let response = client.get(format!("{url}{path}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = client
                .get(format!("{url}{path}"))
                .header(header::AUTHORIZATION, &auth)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        screen
            .show("PRIVATE CLAIM", "https://example.test/claim?token=fixture")
            .await
            .unwrap();
        screen.set_owned(true).await;
        let get = |path: &str| {
            client
                .get(format!("{url}{path}"))
                .header(header::AUTHORIZATION, &auth)
        };
        assert_eq!(
            get("")
                .header(header::HOST, "evil.example")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::MISDIRECTED_REQUEST
        );
        assert_eq!(
            get("/camera")
                .header("sec-fetch-site", "cross-site")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let html = get("").send().await.unwrap().text().await.unwrap();
        assert!(html.contains("Ada"));
        assert!(html.contains("<span id=recognized>Unknown</span>"));
        assert!(
            !html.contains("PRIVATE CLAIM") && !html.contains("/confirm") && !html.contains("<svg")
        );
        assert_eq!(mock.jpeg_reads.load(Ordering::Relaxed), 0);
        let token = screen.panel.read().await.operator_epoch.clone().unwrap();
        let post = |csrf: &str, origin: &str| {
            client
                .post(format!("{url}/control"))
                .header(header::AUTHORIZATION, &auth)
                .header(header::ORIGIN, origin)
                .form(&[("csrf", csrf), ("action", "gesture"), ("name", "nod")])
        };
        assert_eq!(
            post("wrong", &url).send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post(&token, "https://untrusted.test")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            post(&token, &url).send().await.unwrap().status(),
            StatusCode::SEE_OTHER
        );
        assert!(matches!(&mock.actions.lock().unwrap()[..], [Control::Gesture(g)] if g == "nod"));
        let mut camera = get("/camera").send().await.unwrap();
        assert!(
            camera
                .chunk()
                .await
                .unwrap()
                .unwrap()
                .starts_with(b"--buddy-frame\r\n")
        );
        screen.set_owned(false).await;
        screen.set_owned(true).await;
        assert!(
            camera.chunk().await.unwrap().is_none(),
            "reclaimed robot closes existing viewer"
        );
        assert_eq!(
            post(&token, &url).send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        server.abort();
    }

    #[tokio::test]
    async fn local_people_enrollment_and_recognition_use_the_device_adapter() {
        let mock = Arc::new(Mock::default());
        let screen = Screen::at(super::super::Address::Off).with_hardware(mock.clone());
        let (url, auth, server) = fixture(&screen).await;
        screen.set_owned(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let csrf = screen.panel.read().await.operator_epoch.clone().unwrap();
        let post = |action: &str, name: &str| {
            client
                .post(format!("{url}/control"))
                .header(header::AUTHORIZATION, &auth)
                .header(header::ORIGIN, &url)
                .form(&[("action", action), ("name", name), ("csrf", &csrf)])
        };
        let get = |path: &str| {
            client
                .get(format!("{url}{path}"))
                .header(header::AUTHORIZATION, &auth)
        };
        assert_eq!(
            post("enroll", "../bad").send().await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        for action in ["enroll", "forget"] {
            assert_eq!(
                post(action, "O'Brien").send().await.unwrap().status(),
                StatusCode::SEE_OTHER
            );
        }
        assert!(
            matches!(&mock.actions.lock().unwrap()[..],[Control::Enroll(a),Control::Forget(b)] if a == "O'Brien" && b == a)
        );
        *mock.recognized.lock().unwrap() = Some("O'Brien".into());
        let body = get("/").send().await.unwrap().text().await.unwrap();
        assert!(body.contains("<span id=recognized>O&#39;Brien</span>"));
        assert_eq!(
            get("/recognized")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "O'Brien"
        );
        *mock.recognized.lock().unwrap() = None;
        assert_eq!(
            get("/recognized")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "Unknown"
        );
        server.abort();
    }

    #[tokio::test]
    async fn aborted_setup_clears_physical_qr_pair() {
        let mock = Arc::new(Mock::default());
        let screen = Screen::at(super::super::Address::Off).with_hardware(mock.clone());
        let (start, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _eyes = screen.wifi_eyes("test-hotspot-qr", "http://10.42.0.1");
            let _ = start.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(
            &*mock.seen.lock().unwrap(),
            &[
                Some(("test-hotspot-qr".into(), "http://10.42.0.1".into())),
                None
            ]
        );
    }

    #[test]
    fn log_retains_only_bounded_callsite_metadata() {
        tracing::subscriber::with_default(tracing_subscriber::registry().with(LogLayer), || {
            for i in 0..2 * LOG_CAP {
                tracing::info!(target:"robot_buddy_robot::fixture", password="fixture-secret", "private text {i}");
            }
        });
        let (_, batch) = log_batch(0);
        assert_eq!(batch.lines().count(), LOG_CAP);
        assert!(batch.contains("INFO robot_buddy_robot::fixture:"));
        assert!(!batch.contains("fixture-secret") && !batch.contains("private text"));
    }

    #[test]
    fn limits_and_local_only_address() {
        assert!(address("127.0.0.1:8091").is_ok());
        for invalid in ["127.0.0.1:0", "0.0.0.0:8091", "192.168.1.5:8091"] {
            assert!(address(invalid).is_err());
        }
        for name in ["../home", "", "a/b", "<Ada>"] {
            assert!(!person_name(name));
        }
        for invalid in [f32::NAN, f32::INFINITY, 46.0] {
            assert!(
                CommandForm {
                    csrf: String::new(),
                    action: "pose".into(),
                    name: String::new(),
                    yaw: Some(invalid),
                    pitch: Some(0.),
                    roll: Some(0.)
                }
                .command()
                .is_none()
            );
        }
        assert!(jpeg_part(b"<html>").is_none());
        assert!(jpeg_part(&vec![0; MAX_JPEG + 1]).is_none());
    }

    #[tokio::test]
    async fn offline_camera_returns_503_immediately_and_preserves_caps() {
        let mock = Arc::new(Mock::default());
        *mock.frame.lock().unwrap() = FrameReply::Offline;
        let screen = Screen::at(super::super::Address::Off).with_hardware(mock.clone());
        let fixture = guarded_fixture(&screen, guard::Limits::PAGE).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let get = |path: &str| {
            client
                .get(format!("{}{path}", fixture.url))
                .header(header::AUTHORIZATION, &fixture.auth)
        };

        // Auth and ownership still gate /camera before touching hardware.
        assert_eq!(
            client
                .get(format!("{}/camera", fixture.url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get("/camera").send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(mock.jpeg_reads.load(Ordering::Relaxed), 0);

        screen.set_owned(true).await;
        assert_eq!(
            get("/camera")
                .header("sec-fetch-site", "cross-site")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(mock.jpeg_reads.load(Ordering::Relaxed), 0);

        // Offline camera returns 503 immediately without polling, releasing the viewer slot.
        for expected_reads in 1..=3 {
            let started = std::time::Instant::now();
            let response = get("/camera").send().await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                started.elapsed() < Duration::from_millis(150),
                "offline camera must not wait on the 200 ms stream loop"
            );
            assert_eq!(response.text().await.unwrap(), "Camera unavailable.");
            assert_eq!(mock.jpeg_reads.load(Ordering::Relaxed), expected_reads);
            assert_eq!(fixture.viewers.available_permits(), 2);
        }

        // Invalid JPEG and hardware errors also return 503 without leaking details.
        *mock.frame.lock().unwrap() = FrameReply::Invalid;
        let response = get("/camera").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.text().await.unwrap(), "Camera unavailable.");

        *mock.frame.lock().unwrap() = FrameReply::Error("/dev/video0: internal camera fault");
        let response = get("/camera").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.unwrap();
        assert_eq!(body, "Camera unavailable.");
        assert!(!body.contains("/dev/video0") && !body.contains("internal"));

        fixture.server.abort();
    }

    #[tokio::test]
    async fn camera_enforces_work_and_viewer_caps_and_closes_when_camera_drops() {
        let mock = Arc::new(Mock::default());
        let screen = Screen::at(super::super::Address::Off).with_hardware(mock.clone());
        screen.set_owned(true).await;
        let fixture = guarded_fixture(&screen, guard::Limits::PAGE).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let get = |path: &str| {
            client
                .get(format!("{}{path}", fixture.url))
                .header(header::AUTHORIZATION, &fixture.auth)
        };

        // Work cap returns 429 and releases the viewer slot.
        let held_work = fixture.work.acquire_many(4).await.unwrap();
        let reads_before = mock.jpeg_reads.load(Ordering::Relaxed);
        assert_eq!(
            get("/camera").send().await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(mock.jpeg_reads.load(Ordering::Relaxed), reads_before);
        assert_eq!(fixture.viewers.available_permits(), 2);
        drop(held_work);

        // Viewer cap allows two concurrent streams and rejects the third with 429.
        let mut first = get("/camera?t=1").send().await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(
            first
                .chunk()
                .await
                .unwrap()
                .unwrap()
                .starts_with(b"--buddy-frame\r\n")
        );
        let mut second = get("/camera?t=2").send().await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert!(
            second
                .chunk()
                .await
                .unwrap()
                .unwrap()
                .starts_with(b"--buddy-frame\r\n")
        );
        let reads_at_cap = mock.jpeg_reads.load(Ordering::Relaxed);
        assert_eq!(
            get("/camera?t=3").send().await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(mock.jpeg_reads.load(Ordering::Relaxed), reads_at_cap);

        // Going offline mid-stream closes the stream cleanly instead of polling forever.
        *mock.frame.lock().unwrap() = FrameReply::Offline;
        assert!(
            tokio::time::timeout(Duration::from_secs(2), first.chunk())
                .await
                .expect("stream closes when camera goes offline")
                .unwrap()
                .is_none()
        );
        drop(first);
        drop(second);
        fixture.server.abort();
    }

    #[tokio::test]
    async fn guarded_camera_and_routes_recover_across_guard_lifetime() {
        let mock = Arc::new(Mock::default());
        let screen = Screen::at(super::super::Address::Off).with_hardware(mock.clone());
        screen.set_owned(true).await;
        let limits = guard::Limits {
            max: 16,
            idle: Duration::from_secs(2),
            lifetime: Duration::from_millis(350),
        };
        let fixture = guarded_fixture(&screen, limits).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let get = |path: &str| {
            client
                .get(format!("{}{path}", fixture.url))
                .header(header::AUTHORIZATION, &fixture.auth)
        };

        // Guarded static/page/API routes carry local security headers.
        for path in ["/", "/recognized", "/console.js"] {
            let response = get(path).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert_eq!(
                response.headers()["cross-origin-resource-policy"],
                "same-origin"
            );
        }

        // Browser console script includes bounded watchdog and cache-busted reconnect.
        let script = get("/console.js")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(script.contains("WATCHDOG_MS"));
        assert!(script.contains("/camera?t="));
        assert!(script.contains("MAX_ERRORS"));
        assert!(script.contains("visibilitychange"));

        // Guarded SSE stream opens cleanly and carries security headers.
        let sse = get("/events").send().await.unwrap();
        assert_eq!(sse.status(), StatusCode::OK);
        assert_eq!(sse.headers()[header::CACHE_CONTROL], "no-store");
        drop(sse);

        // Guarded camera stream emits frames, closes at guard lifetime, frees its viewer
        // slot, and serves a fresh cache-busted reconnect.
        let mut first = get("/camera?t=100-1").send().await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers()[header::CONTENT_TYPE],
            "multipart/x-mixed-replace; boundary=buddy-frame"
        );
        assert_eq!(first.headers()[header::CONNECTION], "close");
        let chunk = first.chunk().await.unwrap().unwrap();
        assert!(chunk.starts_with(b"--buddy-frame\r\n"));

        let ended = tokio::time::timeout(Duration::from_secs(2), async {
            while let Ok(Some(_)) = first.chunk().await {}
        })
        .await;
        assert!(ended.is_ok(), "guard lifetime closes the camera stream");
        drop(first);

        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.viewers.available_permits() != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("viewer slot released after guard lifetime closes connection");

        let mut reconnected = get("/camera?t=200-2").send().await.unwrap();
        assert_eq!(reconnected.status(), StatusCode::OK);
        assert!(
            reconnected
                .chunk()
                .await
                .unwrap()
                .unwrap()
                .starts_with(b"--buddy-frame\r\n")
        );
        drop(reconnected);
        fixture.server.abort();
    }

    #[tokio::test]
    async fn enrollment_surfaces_sanitized_precondition_and_offline_errors_without_leaks() {
        let mock = Arc::new(Mock::default());
        let screen = Screen::at(super::super::Address::Off).with_hardware(mock.clone());
        screen.set_owned(true).await;
        let fixture = guarded_fixture(&screen, guard::Limits::PAGE).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let csrf = screen.panel.read().await.operator_epoch.clone().unwrap();
        let post = |action: &str, name: &str| {
            client
                .post(format!("{}/control", fixture.url))
                .header(header::AUTHORIZATION, &fixture.auth)
                .header(header::ORIGIN, &fixture.url)
                .form(&[("action", action), ("name", name), ("csrf", &csrf)])
        };

        let cases = [
            (
                "vision is offline",
                StatusCode::SERVICE_UNAVAILABLE,
                "Vision is offline.",
            ),
            (
                "recognition is off",
                StatusCode::SERVICE_UNAVAILABLE,
                "Recognition is off.",
            ),
            (
                "no fresh camera frame",
                StatusCode::SERVICE_UNAVAILABLE,
                "No fresh camera frame available.",
            ),
            (
                "show exactly one face at least 40 pixels across",
                StatusCode::UNPROCESSABLE_ENTITY,
                "Show exactly one face at least 40 pixels across.",
            ),
            (
                "enrollment frame expired",
                StatusCode::CONFLICT,
                "Enrollment frame expired; try again.",
            ),
            (
                "no usable face feature",
                StatusCode::UNPROCESSABLE_ENTITY,
                "No usable face feature detected; try again.",
            ),
            (
                "gallery full",
                StatusCode::CONFLICT,
                "Recognition gallery is full.",
            ),
            (
                "person already has eight samples",
                StatusCode::CONFLICT,
                "Person already has the maximum eight samples.",
            ),
        ];
        for (raw, expected_status, expected_body) in cases {
            *mock.control_error.lock().unwrap() = Some(Box::new(move || anyhow::anyhow!(raw)));
            let response = post("enroll", "Ada").send().await.unwrap();
            assert_eq!(response.status(), expected_status, "{raw}");
            assert_eq!(response.text().await.unwrap(), expected_body, "{raw}");
        }

        // Unrecognized internal errors never leak paths or diagnostics.
        *mock.control_error.lock().unwrap() = Some(Box::new(|| {
            anyhow::anyhow!("/var/lib/robot-buddy/faces/.pending: permission denied")
        }));
        let response = post("enroll", "Ada").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.unwrap();
        assert_eq!(body, "Control unavailable.");
        assert!(!body.contains("/var/lib") && !body.contains("permission"));

        // Wrapped IO errors stay generic even if an outer context matches a known string.
        *mock.control_error.lock().unwrap() = Some(Box::new(|| {
            Err::<(), _>(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "/secret/gallery.sf1",
            ))
            .context("vision is offline")
            .unwrap_err()
        }));
        let response = post("enroll", "Ada").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.unwrap();
        assert_eq!(body, "Control unavailable.");
        assert!(!body.contains("/secret"));

        // Non-enrollment controls stay generic and never leak adapter errors.
        *mock.control_error.lock().unwrap() =
            Some(Box::new(|| anyhow::anyhow!("vision is offline")));
        let response = post("gesture", "nod").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.text().await.unwrap(), "Control unavailable.");

        // Work cap still applies to enrollment control posts.
        let held_work = fixture.work.acquire_many(4).await.unwrap();
        let response = post("enroll", "Ada").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.text().await.unwrap(), "Control unavailable.");
        drop(held_work);

        fixture.server.abort();
    }
}
