#[cfg(test)]
mod http_tests;
mod lab;
mod limit;
mod oidc;
mod speech;
mod web;
use anyhow::{Context, Result, ensure};
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use oidc::Oidc;
use rivetkit::{
    TypedClientExt,
    client::{Client, ClientConfig, GetOptions, GetOrCreateOptions},
};
use robot_buddy::{
    Bot,
    auth::{self, Certificate, Challenge, Secret, Session},
    protocol::{Bundle, Call, Command, EnrollmentRequest},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use web::{CookieSpec, Kind, Reject, Web, cookie, require, same_origin};

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
    logins: oidc::UsedLogins,
}
fn env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} required"))
}
fn safe_url(raw: &str) -> Result<()> {
    let u = url::Url::parse(raw)?;
    ensure!(
        u.username().is_empty() && u.password().is_none() && u.fragment().is_none(),
        "URL contains credentials or fragment"
    );
    ensure!(
        u.scheme() == "https"
            || (std::env::var("BUDDY_ALLOW_INSECURE_LOCAL").as_deref() == Ok("true")
                && u.scheme() == "http"
                && matches!(u.host_str(), Some("localhost" | "127.0.0.1"))),
        "HTTPS required"
    );
    Ok(())
}
/// `RIVET_ENDPOINT` carries the engine admin token, so it must be HTTPS. Plaintext is
/// allowed only to loopback, or to the one host named by `RIVET_ENDPOINT_PLAINTEXT_HOST`
/// (a private Docker-network service name, for example).
fn engine_url(raw: &str, plaintext_host: Option<&str>) -> Result<()> {
    let u = url::Url::parse(raw)?;
    ensure!(
        u.username().is_empty() && u.password().is_none() && u.fragment().is_none(),
        "RIVET_ENDPOINT contains credentials or fragment"
    );
    let loopback = match u.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => {
            host == "localhost" || plaintext_host.is_some_and(|named| named == host)
        }
        None => false,
    };
    ensure!(
        u.scheme() == "https" || (u.scheme() == "http" && loopback),
        "RIVET_ENDPOINT requires HTTPS unless it is loopback or RIVET_ENDPOINT_PLAINTEXT_HOST"
    );
    Ok(())
}
fn origin_of(raw: &str) -> Result<String> {
    Ok(url::Url::parse(raw)?.origin().ascii_serialization())
}
impl App {
    fn session(&self, bot: &str, role: &str) -> Result<String> {
        auth::sign(
            self.key.expose(),
            "session",
            &Session {
                bot_id: bot.into(),
                role: role.into(),
                expires: auth::now() + 900,
            },
        )
    }
    /// Cookies carry `Secure` whenever the public URL is HTTPS.
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
    async fn from_env() -> Result<Self> {
        let key = robot_buddy::config::signing_key()?;
        let engine = env("RIVET_ENDPOINT")?;
        let plaintext_host = std::env::var("RIVET_ENDPOINT_PLAINTEXT_HOST").ok();
        engine_url(&engine, plaintext_host.as_deref().filter(|h| !h.is_empty()))?;
        let admin = Secret::new(env("RIVET_TOKEN")?);
        ensure!(
            admin.expose().len() >= 32,
            "RIVET_TOKEN must be random and at least 32 bytes"
        );
        let namespace = std::env::var("RIVET_NAMESPACE").unwrap_or_else(|_| "default".into());
        let public_url = env("BUDDY_PUBLIC_URL")?;
        let public_engine = env("BUDDY_PUBLIC_RIVET_URL")?;
        safe_url(&public_url)?;
        safe_url(&public_engine)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let oidc = match std::env::var("OIDC_ISSUER").ok().filter(|v| !v.is_empty()) {
            Some(issuer) => Some(Oidc::discover(&http, issuer).await?),
            None => None,
        };
        let lab_token = std::env::var("BUDDY_LAB_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Secret::new);
        if let Some(token) = &lab_token {
            ensure!(
                (32..=256).contains(&token.expose().len()),
                "BUDDY_LAB_TOKEN must be 32-256 bytes"
            );
        }
        Ok(Self {
            speech: speech::Speech::from_env()?,
            lab_token,
            key,
            issuer: auth::verifying_key(&env("DEVICE_ISSUER_PUBLIC_KEY")?)?,
            client: Client::new(
                ClientConfig::new(&engine)
                    .token(admin.expose())
                    .namespace(&namespace),
            ),
            http,
            engine,
            admin,
            public_engine,
            public_origin: origin_of(&public_url)?,
            public_url,
            namespace,
            oidc,
            limiter: limit::RateLimiter::new(limit::RateLimiter::trusted_from_env()?),
            logins: oidc::UsedLogins::default(),
        })
    }
}
/// All HTTP routes and middleware. Starting the engine and registry is separate, so tests
/// can drive this with `tower::ServiceExt::oneshot`.
fn router(app: Arc<App>) -> Router {
    Router::new()
        .merge(lab::routes())
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/challenge", post(challenge))
        .route("/v1/enroll", post(enroll))
        .route("/v1/speech", post(speech::device))
        .route("/claim", get(claim))
        .route("/login", post(login))
        .route("/auth/callback", get(callback))
        .layer(DefaultBodyLimit::max(16384))
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            limit::rate_limit,
        ))
        .layer(axum::middleware::from_fn(limit::limit_requests))
        .with_state(app)
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("provision") {
        return provision(&args[2..]);
    }
    if args.get(1).map(String::as_str) == Some("init") {
        return initialize(&args[2..]);
    }
    let _model = robot_buddy::model::Model::from_env()?;
    let app = Arc::new(App::from_env().await?);
    let router = router(app);
    let listener = tokio::net::TcpListener::bind(
        std::env::var("BUDDY_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
    )
    .await?;
    let mut engine_process = if let Ok(path) = std::env::var("BUDDY_ENGINE_BINARY") {
        Some(spawn_engine(&path).await?)
    } else {
        None
    };
    let mut registry = tokio::spawn(robot_buddy::registry().start());
    let mut web = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
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
        terminate(&child);
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
/// Ask the engine to drain with SIGTERM, sent directly rather than through `kill(1)`.
fn terminate(child: &tokio::process::Child) {
    use nix::{sys::signal, unistd::Pid};
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = signal::kill(Pid::from_raw(id), signal::Signal::SIGTERM);
    }
}
/// Liveness: the process answers HTTP. It touches nothing else, so the robot can use it
/// to warm its speech connection without loading the engine.
async fn health() -> Json<Value> {
    Json(json!({"status":"ok"}))
}
/// Readiness: the Rivet engine answers. 503 without internal detail when it does not.
async fn ready(State(app): State<Arc<App>>) -> Web<Json<Value>> {
    app.http
        .get(format!("{}/health", app.engine))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .reject(Kind::Unavailable)?;
    Ok(Json(json!({"status":"ok"})))
}
async fn challenge(State(app): State<Arc<App>>, Json(c): Json<Certificate>) -> Web<Json<Value>> {
    c.validate(&app.issuer).reject(Kind::Unauthorized)?;
    let value = Challenge {
        device_id: c.device_id,
        public_key: c.public_key,
        nonce: auth::random(),
        expires: auth::now() + 120,
    };
    let challenge = auth::sign(app.key.expose(), "challenge", &value).reject(Kind::Internal)?;
    Ok(Json(json!({ "challenge": challenge })))
}
async fn enroll(
    State(app): State<Arc<App>>,
    Json(r): Json<EnrollmentRequest>,
) -> Web<Json<Bundle>> {
    let key = r
        .certificate
        .validate(&app.issuer)
        .reject(Kind::Unauthorized)?;
    let challenge: Challenge =
        auth::verify(app.key.expose(), "challenge", &r.challenge).reject(Kind::Unauthorized)?;
    require(
        challenge.expires > auth::now()
            && challenge.device_id == r.certificate.device_id
            && challenge.public_key == r.certificate.public_key,
        Kind::Unauthorized,
    )?;
    auth::verify_proof(&key, &r.challenge, &r.proof).reject(Kind::Unauthorized)?;
    provision_bot(&app, r.certificate, challenge)
        .await
        .reject(Kind::BadGateway)
        .map(Json)
}
/// Engine work after the device proved possession. Actor rejections (a replayed nonce) and
/// engine failures are not distinguished; both surface as 502.
async fn provision_bot(
    app: &App,
    certificate: Certificate,
    challenge: Challenge,
) -> Result<Bundle> {
    let bot = certificate.bot_id();
    let handle = app.client.get_or_create_typed::<Bot>(
        "bot",
        [bot.clone()],
        GetOrCreateOptions {
            params: Some(json!({"session":app.session(&bot,"admin")?})),
            ..Default::default()
        },
    )?;
    handle
        .send(Call {
            session: app.session(&bot, "admin")?,
            command: Command::Enroll {
                device_id: certificate.device_id,
                public_key: certificate.public_key,
            },
        })
        .await?;
    handle
        .send(Call {
            session: app.session(&bot, "admin")?,
            command: Command::ConsumeChallenge {
                nonce: challenge.nonce,
                expires: challenge.expires,
            },
        })
        .await?;
    let actor_id = handle.inner().resolve().await?;
    let internal_session = app.session(&bot, "admin")?;
    let options = || GetOrCreateOptions {
        params: Some(json!({"session":internal_session})),
        ..Default::default()
    };
    let conversation = app
        .client
        .get_or_create_typed::<robot_buddy::conversation::Conversation>(
            "conversation",
            [bot.clone()],
            options(),
        )?;
    let memory = app
        .client
        .get_or_create_typed::<robot_buddy::memory::Memory>("memory", [bot.clone()], options())?;
    let conversation_actor_id = conversation.inner().resolve().await?;
    let memory_actor_id = memory.inner().resolve().await?;

    let grant = |id: &str| {
        json!({
            "resource": "actor_gateway",
            "target": {"id": id},
            "operations": ["read"],
        })
    };
    let token: Value = app
        .http
        .post(format!("{}/auth/tokens", app.engine.trim_end_matches('/')))
        .bearer_auth(app.admin.expose())
        .json(&json!({
            "namespace": app.namespace,
            "duration": 900,
            "grants": [grant(&actor_id), grant(&conversation_actor_id), grant(&memory_actor_id)],
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(Bundle {
        bot_id: bot.clone(),
        actor_id,
        conversation_actor_id,
        memory_actor_id,
        endpoint: app.public_engine.clone(),
        namespace: app.namespace.clone(),
        gateway_token: token["token"].as_str().context("token missing")?.into(),
        session: app.session(&bot, "device")?,
        expires: auth::now() + 900,
    })
}
#[derive(Deserialize)]
struct ClaimQuery {
    bot: String,
    token: String,
}
#[derive(Deserialize)]
struct LoginForm {
    bot: String,
    token: String,
    csrf: String,
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn page(body: &str) -> Response {
    let mut r = Html(format!(
        "<!doctype html><html><head>\
         <meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>Robot Buddy</title></head>\
         <body><main><h1>Robot Buddy</h1>{body}</main></body></html>"
    ))
    .into_response();
    let headers = r.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    r
}
const CLAIM_TTL: i64 = 600;
/// Lifetime of one sign-in transaction (login cookie, OAuth `state`), spent once.
const LOGIN_TTL: i64 = 300;
/// Binds `GET /claim` to `POST /login`: the signed value names the claim it was issued
/// for, and its CSRF value must come back in the form.
#[derive(Serialize, Deserialize)]
struct ClaimForm {
    bot: String,
    /// Digest only: the cookie never carries the QR claim token itself.
    token_digest: String,
    csrf: String,
    expires: i64,
}
fn claim_cookie(app: &App) -> CookieSpec {
    CookieSpec {
        name: "buddy_claim",
        path: "/login",
        same_site: "Strict",
        max_age: CLAIM_TTL,
        secure: app.secure_cookies(),
    }
}
fn login_cookie(app: &App) -> CookieSpec {
    // Lax: the identity provider returns with a cross-site top-level GET.
    CookieSpec {
        name: "buddy_login",
        path: "/auth/callback",
        same_site: "Lax",
        max_age: LOGIN_TTL,
        secure: app.secure_cookies(),
    }
}
fn valid_claim(bot: &str, token: &str) -> bool {
    (1..=128).contains(&bot.len()) && (1..=128).contains(&token.len())
}
async fn claim(State(app): State<Arc<App>>, Query(q): Query<ClaimQuery>) -> Web<Response> {
    require(valid_claim(&q.bot, &q.token), Kind::BadRequest)?;
    let Some(oidc) = app.oidc.as_ref() else {
        return Ok(page(
            "<p>The server administrator must configure sign-in before registration can finish. \
             Your robot will retain its identity.</p>",
        ));
    };
    let form = ClaimForm {
        bot: q.bot,
        token_digest: auth::digest(&q.token),
        csrf: auth::random(),
        expires: auth::now() + CLAIM_TTL,
    };
    let mut response = page(&format!(
        "<p>Connect this robot to your account. Keep the robot nearby to confirm.</p>\
         <form method=post action=/login>\
         <input type=hidden name=bot value=\"{}\">\
         <input type=hidden name=token value=\"{}\">\
         <input type=hidden name=csrf value=\"{}\">\
         <button>Sign in to register</button></form>",
        escape(&form.bot),
        escape(&q.token),
        escape(&form.csrf)
    ));
    // Browsers apply form-action to the login POST's redirect chain too.
    let authorize_origin = origin_of(&oidc.authorize).reject(Kind::Internal)?;
    let csp = format!(
        "default-src 'none'; form-action 'self' {authorize_origin}; \
         frame-ancestors 'none'; base-uri 'none'"
    );
    let signed = auth::sign(app.key.expose(), "claim-form", &form).reject(Kind::Internal)?;
    let headers = response.headers_mut();
    headers.insert(
        "content-security-policy",
        HeaderValue::from_str(&csp).reject(Kind::Internal)?,
    );
    // `no-referrer` would make the form POST send `Origin: null`. `same-origin` keeps the
    // claim URL (with its token) away from every other origin.
    headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    headers.insert(
        header::SET_COOKIE,
        claim_cookie(&app).set(&signed).reject(Kind::Internal)?,
    );
    Ok(response)
}
#[derive(Serialize, Deserialize)]
struct LoginState {
    bot: String,
    token: String,
    /// OAuth `state`, compared with the callback query.
    state: String,
    /// OIDC `nonce`, compared with the ID token. Independent of `state`.
    nonce: String,
    verifier: String,
    expires: i64,
}
async fn login(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(q): Form<LoginForm>,
) -> Web<Response> {
    // A cross-site auto-submitted form would otherwise bind the victim's IdP session to
    // the attacker's robot.
    require(same_origin(&app.public_origin, &headers), Kind::Forbidden)?;
    require(valid_claim(&q.bot, &q.token), Kind::BadRequest)?;
    let oidc = app.oidc.as_ref().reject(Kind::Unavailable)?;
    let raw = cookie(&headers, "buddy_claim").reject(Kind::Forbidden)?;
    let form: ClaimForm =
        auth::verify(app.key.expose(), "claim-form", raw).reject(Kind::Forbidden)?;
    require(
        form.expires > auth::now()
            && auth::ct_eq(form.csrf.as_bytes(), q.csrf.as_bytes())
            && form.bot == q.bot
            && auth::ct_eq(
                form.token_digest.as_bytes(),
                auth::digest(&q.token).as_bytes(),
            ),
        Kind::Forbidden,
    )?;
    let state = LoginState {
        bot: q.bot,
        token: q.token,
        state: auth::random(),
        nonce: auth::random(),
        verifier: format!("{}{}", auth::random(), auth::random()),
        expires: auth::now() + LOGIN_TTL,
    };
    let mut url = url::Url::parse(&oidc.authorize).reject(Kind::Internal)?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", oidc.client_id.as_str()),
        ("response_type", "code"),
        ("scope", "openid profile"),
        ("redirect_uri", &app.callback_url()),
        ("state", &state.state),
        ("nonce", &state.nonce),
        ("code_challenge", &auth::digest(&state.verifier)),
        ("code_challenge_method", "S256"),
    ]);
    let mut response = Redirect::to(url.as_str()).into_response();
    let signed = auth::sign(app.key.expose(), "login", &state).reject(Kind::Internal)?;
    let headers = response.headers_mut();
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.append(
        header::SET_COOKIE,
        login_cookie(&app).set(&signed).reject(Kind::Internal)?,
    );
    headers.append(
        header::SET_COOKIE,
        claim_cookie(&app).clear().reject(Kind::Internal)?,
    );
    Ok(response)
}
#[derive(Deserialize)]
struct Callback {
    code: String,
    state: String,
}
async fn callback(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Web<Response> {
    let raw = cookie(&headers, "buddy_login").reject(Kind::Unauthorized)?;
    let state: LoginState =
        auth::verify(app.key.expose(), "login", raw).reject(Kind::Unauthorized)?;
    require(state.expires > auth::now(), Kind::Unauthorized)?;
    require(
        auth::ct_eq(state.state.as_bytes(), q.state.as_bytes()),
        Kind::Forbidden,
    )?;
    let oidc = app.oidc.as_ref().reject(Kind::Unavailable)?;
    // Spent before the exchange: whatever happens next, this cookie cannot be replayed.
    match app.logins.spend(&state.state, state.expires, auth::now()) {
        oidc::Spend::Fresh => {}
        oidc::Spend::Replayed => return Err(web::ApiError(Kind::Unauthorized)),
        oidc::Spend::Full => return Err(web::ApiError(Kind::TooManyRequests)),
    }
    let redirect = app.callback_url();
    let token: Value = async {
        let form = oidc.token_form(&q.code, &state.verifier, &redirect);
        oidc::json(app.http.post(&oidc.token).form(&form).send().await?).await
    }
    .await
    .reject(Kind::BadGateway)?;
    let access = token["access_token"].as_str().reject(Kind::BadGateway)?;
    let user: Value = async {
        oidc::json(
            app.http
                .get(&oidc.userinfo)
                .bearer_auth(access)
                .send()
                .await?,
        )
        .await
    }
    .await
    .reject(Kind::BadGateway)?;
    let subject = user["sub"].as_str().reject(Kind::BadGateway)?;
    let id_token = token["id_token"].as_str().reject(Kind::BadGateway)?;
    oidc::validate(&app.http, oidc, &state, id_token, subject)
        .await
        .reject(Kind::Unauthorized)?;
    let owner = format!("{}|{subject}", oidc.issuer);
    let proposed = app
        .call(
            &state.bot,
            Command::ProposeClaim {
                token: state.token,
                owner,
            },
        )
        .await
        .reject(Kind::BadGateway)?;
    let code = proposed["code"].as_str().reject(Kind::BadGateway)?;
    let mut response = page(&format!(
        "<p>Match this code on your robot and press its confirm control:</p><h2>{}</h2>\
         <p>The robot will welcome you when registration is complete.</p>",
        escape(code)
    ));
    response.headers_mut().insert(
        header::SET_COOKIE,
        login_cookie(&app).clear().reject(Kind::Internal)?,
    );
    Ok(response)
}
/// Engine config holding the admin token and S3 credentials. It lives outside the backed-up
/// data volume and is removed when this guard drops.
struct RuntimeFile(std::path::PathBuf);
impl Drop for RuntimeFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
/// `$XDG_RUNTIME_DIR` when set, else the system temp directory; never the data volume.
fn runtime_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}
/// Writes `data` to a new `0600` file with an unguessable name (`create_new`, so an
/// existing file or symlink at that path is never followed).
fn write_runtime_config(dir: &std::path::Path, data: &[u8]) -> Result<RuntimeFile> {
    let path = dir.join(format!("robot-buddy-engine-{}.json", auth::random()));
    private_write(&path, data)?;
    Ok(RuntimeFile(path))
}
async fn spawn_engine(path: &str) -> Result<(tokio::process::Child, RuntimeFile)> {
    let data = std::env::var("BUDDY_DATA_DIR").unwrap_or_else(|_| "/data".into());
    tokio::fs::create_dir_all(format!("{data}/logs")).await?;
    tokio::fs::create_dir_all(format!("{data}/objects")).await?;
    // Earlier releases wrote the config, with secrets, into the backed-up volume.
    let _ = tokio::fs::remove_file(format!("{data}/engine.json")).await;
    let object_storage = match std::env::var("BUDDY_STORAGE_BACKEND").as_deref() {
        Ok("s3") => {
            let mut s3 = json!({"bucket":env("BUDDY_S3_BUCKET")?});
            for (field, name) in [
                ("region", "BUDDY_S3_REGION"),
                ("endpoint", "BUDDY_S3_ENDPOINT"),
                ("access_key_id", "BUDDY_S3_ACCESS_KEY_ID"),
                ("secret_access_key", "BUDDY_S3_SECRET_ACCESS_KEY"),
            ] {
                if let Ok(v) = std::env::var(name) {
                    s3[field] = json!(v);
                }
            }
            json!({"s3":s3})
        }
        Ok("file_system") | Err(_) => json!({"file_system":{"path":format!("{data}/objects")}}),
        _ => anyhow::bail!("unsupported storage backend"),
    };
    let port: u16 = std::env::var("BUDDY_ENGINE_PORT")
        .unwrap_or_else(|_| "6420".into())
        .parse()?;
    ensure!(port > 1024 && port < 65525, "invalid engine port");
    let config = json!({
        "auth": {"admin_token": env("RIVET_TOKEN")?},
        "guard": {"host": "0.0.0.0", "port": port},
        "api_peer": {"host": "127.0.0.1", "port": port + 1},
        "metrics": {"host": "127.0.0.1", "port": port + 10},
        "storage": {
            "log_directory": format!("{data}/logs"),
            "balancing": {"enabled": false},
            "auto_provision_partitions": 1,
            "log_shard": {"max_disk_bytes": 4_294_967_296u64},
            "cold_store": {"disk_cache_bytes": 0},
        },
        "object_storage": {"primary": object_storage},
        "telemetry": {"enabled": false},
    });
    let config_file = write_runtime_config(&runtime_dir(), &serde_json::to_vec_pretty(&config)?)?;
    // The engine reads the file named by `--config` at startup; it stays until shutdown.
    let mut child = tokio::process::Command::new(path)
        .arg("--config")
        .arg(&config_file.0)
        .kill_on_drop(true)
        .spawn()?;
    let http = reqwest::Client::new();
    for _ in 0..100 {
        if http
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return Ok((child, config_file));
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("engine exited during startup: {status}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("engine did not become healthy")
}
fn private_write(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}
fn initialize(args: &[String]) -> Result<()> {
    use base64::Engine;
    let dir = std::path::Path::new(args.first().context("usage: init DIRECTORY")?);
    std::fs::create_dir_all(dir)?;
    let issuer = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    private_write(
        &dir.join("issuer.key"),
        b64.encode(issuer.as_bytes()).as_bytes(),
    )?;
    let data = format!(
        "BUDDY_SIGNING_KEY={}{}\nRIVET_TOKEN={}{}\nDEVICE_ISSUER_PUBLIC_KEY={}\n",
        auth::random(),
        auth::random(),
        auth::random(),
        auth::random(),
        b64.encode(issuer.verifying_key().as_bytes())
    );
    private_write(&dir.join("secrets.env"), data.as_bytes())?;
    println!("Wrote private issuer key and server secrets. Keep issuer.key offline.");
    Ok(())
}
fn provision(args: &[String]) -> Result<()> {
    let issuer = auth::signing_key(
        std::fs::read_to_string(args.first().context("usage: provision ISSUER_KEY OUTPUT")?)?
            .trim(),
    )?;
    let identity = auth::Identity::provision(&issuer)?;
    private_write(
        std::path::Path::new(args.get(1).context("output required")?),
        &serde_json::to_vec_pretty(&identity)?,
    )?;
    println!("Provisioned unique device identity.");
    Ok(())
}
