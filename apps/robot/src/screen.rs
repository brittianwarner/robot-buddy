//! Loopback HTML panel for setup and registration (QR codes, owner confirmation).
//! The ESP32 face is a separate output; see `face.rs`.
use anyhow::Result;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Default)]
struct Panel {
    message: String,
    qr: String,
    code: String,
    confirm: bool,
}

#[derive(Clone)]
pub struct Screen {
    panel: Arc<RwLock<Panel>>,
    csrf: String,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            panel: Arc::new(RwLock::new(Panel::default())),
            csrf: robot_buddy::auth::random(),
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
    /// Confirm since the last call.
    pub async fn confirmation(&self, code: &str) -> bool {
        let mut panel = self.panel.write().await;
        panel.code = code.into();
        std::mem::take(&mut panel.confirm)
    }

    pub async fn serve(&self) -> Result<()> {
        let router = Router::new()
            .route("/", get(page))
            .route("/confirm", post(confirm))
            .layer(DefaultBodyLimit::max(4096))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:8090").await?;
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

async fn page(State(screen): State<Screen>) -> Html<String> {
    let panel = screen.panel.read().await;
    let form = if panel.code.is_empty() {
        String::new()
    } else {
        format!(
            "<form method=post action=/confirm>\
             <input type=hidden name=csrf value=\"{}\">\
             <button>Confirm owner</button></form>",
            screen.csrf
        )
    };
    Html(format!(
        "{HEAD}<h1>{}</h1>{}<p>{}</p>{form}</body></html>",
        escape(&panel.message),
        panel.qr,
        escape(&panel.code),
    ))
}

#[derive(Deserialize)]
struct Confirm {
    csrf: String,
}

async fn confirm(State(screen): State<Screen>, Form(form): Form<Confirm>) -> Response {
    if form.csrf != screen.csrf {
        return (StatusCode::FORBIDDEN, "invalid confirmation").into_response();
    }
    screen.panel.write().await.confirm = true;
    Redirect::to("/").into_response()
}
