//! Robot Buddy server: device enrollment, owner claim, the speech media plane and the
//! optional lab, in front of a Rivet engine it may also supervise.
mod claim;
mod cli;
mod config;
mod engine;
mod enroll;
#[cfg(test)]
mod http_tests;
mod lab;
mod limit;
mod oidc;
mod serve;
mod speech;
mod web;
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    middleware::{from_fn, from_fn_with_state, map_response},
    response::Response,
    routing::{get, post},
};
use oidc::Oidc;
use rivetkit::{
    TypedClientExt,
    client::{Client, GetOptions},
};
use robot_buddy::{
    Bot,
    auth::{self, Secret, Session},
    protocol::{Call, Command},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;
use web::{Kind, Web, require};

struct App {
    key: Secret<Vec<u8>>,
    issuer: ed25519_dalek::VerifyingKey,
    client: Client,
    http: reqwest::Client,
    engine: String,
    admin: Secret<String>,
    public_engine: String,
    public_url: String,
    /// `scheme://host[:port]` of `public_url`, compared against `Origin`.
    public_origin: String,
    namespace: String,
    oidc: Option<Oidc>,
    lab_token: Option<Secret<String>>,
    speech: Option<Arc<speech::Speech>>,
    limiter: limit::RateLimiter,
    logins: claim::Logins,
    lab_sessions: lab::Sessions,
    readiness: Readiness,
}

/// Lifetime of a session the server mints for its own engine calls or for a device.
const SESSION_TTL: i64 = 900;

impl App {
    fn session(&self, bot: &str, role: &str) -> Result<String> {
        self.session_until(bot, role, auth::now() + SESSION_TTL)
    }
    fn session_until(&self, bot: &str, role: &str, expires: i64) -> Result<String> {
        auth::sign(
            self.key.expose(),
            "session",
            &Session {
                bot_id: bot.into(),
                role: role.into(),
                expires,
            },
        )
    }
    /// Cookies carry `Secure`, and responses HSTS, whenever the public URL is HTTPS.
    fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https:")
    }
    fn callback_url(&self) -> String {
        format!("{}/auth/callback", self.public_url.trim_end_matches('/'))
    }
    async fn call(&self, bot: &str, command: Command) -> Result<Value> {
        self.client
            .get_typed::<Bot>(
                "bot",
                [bot],
                GetOptions {
                    params: Some(json!({"session":self.session(bot,"admin")?})),
                },
            )?
            .send(Call {
                session: self.session(bot, "admin")?,
                command,
            })
            .await
    }
}

/// All HTTP routes and middleware. Starting the engine and registry is separate, so tests
/// can drive this with `tower::ServiceExt::oneshot`.
///
/// `/health` sits outside every limit, so a full pool never fails a liveness probe. Lab
/// routes exist only when the lab is enabled.
fn router(app: Arc<App>) -> Router {
    let mut limited = Router::new()
        .route("/ready", get(ready))
        .route("/v1/challenge", post(enroll::challenge))
        .route("/v1/enroll", post(enroll::enroll))
        .route("/v1/speech", post(speech::device))
        .route("/claim", get(claim::claim))
        .route("/login", post(claim::login))
        .route("/auth/callback", get(claim::callback));
    if app.lab_token.is_some() {
        limited = limited.merge(lab::routes(&app));
    }
    let limited = limited
        .layer(DefaultBodyLimit::max(web::BODY_LIMIT))
        .layer(from_fn_with_state(app.clone(), limit::rate_limit))
        .layer(from_fn_with_state(app.clone(), limit::limit_requests));
    let https = app.secure_cookies();
    Router::new()
        .route("/health", get(health))
        .merge(limited)
        .layer(map_response(move |mut response: Response| async move {
            web::security_headers(&mut response, https);
            response
        }))
        .layer(from_fn(serve::track))
        .with_state(app)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("provision") {
        return cli::provision(&args[2..]);
    }
    if args.get(1).map(String::as_str) == Some("init") {
        return cli::initialize(&args[2..]);
    }
    let _model = robot_buddy::model::Model::from_env()?;
    let app = Arc::new(App::from_env().await?);
    let listener = serve::Guarded::new(
        tokio::net::TcpListener::bind(
            std::env::var("BUDDY_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
        )
        .await?,
        app.limiter.trusted().to_vec(),
        serve::ConnLimits::default(),
    );
    let router = router(app);
    let mut engine_process = if let Ok(path) = std::env::var("BUDDY_ENGINE_BINARY") {
        Some(engine::spawn_engine(&path).await?)
    } else {
        None
    };
    let mut registry = tokio::spawn(robot_buddy::registry().start());
    let mut web = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<serve::Peer>(),
        )
        .await
    });
    // The registry handles SIGTERM/SIGINT and drains actors before the engine is stopped.
    let outcome = if let Some((child, _)) = engine_process.as_mut() {
        tokio::select! {
            r = &mut registry => r.context("registry join")?,
            r = child.wait() => Err(anyhow::anyhow!("engine stopped: {r:?}")),
            r = &mut web => Err(anyhow::anyhow!("HTTP server stopped: {r:?}")),
        }
    } else {
        tokio::select! {
            r = &mut registry => r.context("registry join")?,
            r = &mut web => Err(anyhow::anyhow!("HTTP server stopped: {r:?}")),
        }
    };
    web.abort();
    if let Some((mut child, config)) = engine_process {
        engine::terminate(&child);
        if tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .is_err()
        {
            child.kill().await?;
        }
        drop(config);
    }
    outcome
}

/// Liveness: the process answers HTTP. It touches nothing else, so the robot can use it
/// to warm its speech connection without loading the engine.
async fn health() -> Json<Value> {
    Json(json!({"status":"ok"}))
}

/// The last engine probe. Callers inside [`READY_FRESH`] share it; a caller that finds
/// it stale probes while holding the lock, so concurrent callers share that probe too.
#[derive(Default)]
struct Readiness(tokio::sync::Mutex<Option<(Instant, bool)>>);
const READY_FRESH: Duration = Duration::from_secs(2);
const READY_PROBE: Duration = Duration::from_secs(3);

/// Readiness: the Rivet engine answers. 503 without internal detail when it does not.
/// Rate-limited per client, and the engine sees at most one probe per [`READY_FRESH`].
async fn ready(State(app): State<Arc<App>>) -> Web<Json<Value>> {
    let mut last = app.readiness.0.lock().await;
    let up = match *last {
        Some((at, up)) if at.elapsed() < READY_FRESH => up,
        _ => {
            let probe = app
                .http
                .get(format!("{}/health", app.engine))
                .timeout(READY_PROBE)
                .send()
                .await;
            let up = probe.is_ok_and(|r| r.status().is_success());
            *last = Some((Instant::now(), up));
            up
        }
    };
    drop(last);
    require(up, Kind::Unavailable)?;
    Ok(Json(json!({"status":"ok"})))
}
