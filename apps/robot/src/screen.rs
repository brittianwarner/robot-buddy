//! Loopback HTML panel for setup and registration (QR codes, owner confirmation).
//! The ESP32 face is a separate output; see `face.rs`.
use anyhow::Result;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use robot_buddy::auth;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

const FACE_ADDRESS: &str = "127.0.0.1:8090";
/// `Host` values the loopback panel answers. Anything else (DNS rebinding) is refused.
const FACE_HOSTS: [&str; 2] = ["127.0.0.1:8090", "localhost:8090"];
const FACE_ORIGINS: [&str; 2] = ["http://127.0.0.1:8090", "http://localhost:8090"];
/// Shared by the face panel and the `SoftAP` setup form.
pub const LOCAL_CSP: &str =
    "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

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

#[derive(Default)]
struct Panel {
    message: String,
    qr: String,
    code: String,
    confirm: bool,
    /// Rotated whenever the pending confirmation code changes.
    csrf: String,
}

#[derive(Clone)]
pub struct Screen {
    panel: Arc<RwLock<Panel>>,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            panel: Arc::new(RwLock::new(Panel {
                csrf: auth::random(),
                ..Panel::default()
            })),
        }
    }

    /// Show a message and optionally a QR code for `qr`.
    pub async fn show(&self, message: &str, qr: &str) -> Result<()> {
        let svg = if qr.is_empty() {
            String::new()
        } else {
            svg(qr)?
        };
        let mut panel = self.panel.write().await;
        panel.message = message.into();
        panel.qr = svg;
        Ok(())
    }

    /// Show the ready message and clear registration details.
    pub async fn ready(&self) {
        let mut panel = self.panel.write().await;
        panel.message = "Ready — hello!".into();
        panel.qr.clear();
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

    pub async fn serve(&self) -> Result<()> {
        let router = Router::new()
            .route("/", get(page))
            .route("/confirm", post(confirm))
            .layer(DefaultBodyLimit::max(4096))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind(FACE_ADDRESS).await?;
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
    if !header_in(&headers, header::HOST, &FACE_HOSTS) {
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
    let local = header_in(&headers, header::HOST, &FACE_HOSTS)
        && header_in(&headers, header::ORIGIN, &FACE_ORIGINS);
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
        let screen = Screen::new();
        let response = page(State(screen.clone()), headers(FACE_ADDRESS, None)).await;
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
        let screen = Screen::new();
        let origin = Some("http://127.0.0.1:8090");
        // No pending code yet: nothing to confirm.
        let token = csrf(&screen).await;
        let status = press(&screen, &token, FACE_ADDRESS, origin).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        assert!(!screen.confirmation("123456").await);
        let first = csrf(&screen).await;
        assert_ne!(first, token, "a new pending claim rotates the token");
        for (csrf, host, origin) in [
            (first.as_str(), FACE_ADDRESS, None),
            (
                first.as_str(),
                FACE_ADDRESS,
                Some("http://attacker.example"),
            ),
            (first.as_str(), "attacker.example:8090", origin),
            ("forged", FACE_ADDRESS, origin),
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
        let status = press(&screen, &first, FACE_ADDRESS, origin).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(!screen.confirmation("654321").await, "stale press dropped");
        let status = press(&screen, &first, FACE_ADDRESS, origin).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
