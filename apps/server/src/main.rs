mod lab;
mod oidc;
mod speech;
use anyhow::{Context, Result, ensure};
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use rivetkit::{
    TypedClientExt,
    client::{Client, ClientConfig, GetOptions, GetOrCreateOptions},
};
use robot_buddy::{
    Bot,
    auth::{self, Certificate, Challenge, Session},
    protocol::*,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
struct App {
    key: Vec<u8>,
    issuer: ed25519_dalek::VerifyingKey,
    client: Client,
    http: reqwest::Client,
    engine: String,
    admin: String,
    public_engine: String,
    public_url: String,
    namespace: String,
    oidc: Option<Oidc>,
    lab_token: Option<String>,
    speech: Option<Arc<speech::Speech>>,
}
#[derive(Clone)]
struct Oidc {
    issuer: String,
    authorize: String,
    token: String,
    userinfo: String,
    jwks: String,
    client_id: String,
    client_secret: String,
}
struct Error(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for Error {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        drop(self.0);
        tracing::warn!("bootstrap request rejected");
        (
            StatusCode::BAD_REQUEST,
            "Request could not be completed. Retry setup or check server configuration.",
        )
            .into_response()
    }
}
fn check(condition: bool, message: &str) -> Result<()> {
    ensure!(condition, "{message}");
    Ok(())
}
type Web<T> = std::result::Result<T, Error>;
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
impl App {
    fn session(&self, bot: &str, role: &str) -> Result<String> {
        auth::sign(
            &self.key,
            "session",
            &Session {
                bot_id: bot.into(),
                role: role.into(),
                expires: auth::now() + 900,
            },
        )
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
    let key = env("BUDDY_SIGNING_KEY")?.into_bytes();
    ensure!(key.len() >= 32, "signing key too short");
    let engine = env("RIVET_ENDPOINT")?;
    let admin = env("RIVET_TOKEN")?;
    ensure!(
        admin.len() >= 32,
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
    let oidc = if let Ok(issuer) = std::env::var("OIDC_ISSUER")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or(())
    {
        safe_url(&issuer)?;
        let doc: Value = oidc::json(
            http.get(format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            ))
            .send()
            .await?,
        )
        .await?;
        ensure!(
            doc["issuer"].as_str() == Some(issuer.as_str()),
            "OIDC issuer mismatch"
        );
        let field = |name: &str| -> Result<String> {
            let s = doc[name]
                .as_str()
                .context("OIDC endpoint missing")?
                .to_owned();
            safe_url(&s)?;
            Ok(s)
        };
        Some(Oidc {
            issuer,
            authorize: field("authorization_endpoint")?,
            token: field("token_endpoint")?,
            userinfo: field("userinfo_endpoint")?,
            jwks: field("jwks_uri")?,
            client_id: env("OIDC_CLIENT_ID")?,
            client_secret: std::env::var("OIDC_CLIENT_SECRET").unwrap_or_default(),
        })
    } else {
        None
    };
    let lab_token = std::env::var("BUDDY_LAB_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(token) = &lab_token {
        ensure!(
            (32..=256).contains(&token.len()),
            "BUDDY_LAB_TOKEN must be 32-256 bytes"
        );
    }
    let app = Arc::new(App {
        speech: speech::Speech::from_env()?,
        lab_token,
        key,
        issuer: auth::verifying_key(&env("DEVICE_ISSUER_PUBLIC_KEY")?)?,
        client: Client::new(
            ClientConfig::new(&engine)
                .token(&admin)
                .namespace(&namespace),
        ),
        http,
        engine,
        admin,
        public_engine,
        public_url,
        namespace,
        oidc,
    });
    let router = Router::new()
        .merge(lab::routes())
        .route("/health", get(health))
        .route("/v1/challenge", post(challenge))
        .route("/v1/enroll", post(enroll))
        .route("/v1/speech", post(speech::device))
        .route("/claim", get(claim))
        .route("/login", post(login))
        .route("/auth/callback", get(callback))
        .layer(DefaultBodyLimit::max(16384))
        .layer(axum::middleware::from_fn(limit_requests))
        .with_state(app);
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
    let mut web = tokio::spawn(async move { axum::serve(listener, router).await });
    // The registry handles SIGTERM/SIGINT and drains actors before the engine is stopped.
    let outcome = if let Some(child) = engine_process.as_mut() {
        tokio::select! {r=&mut registry=>r.context("registry join")?,r=child.wait()=>Err(anyhow::anyhow!("engine stopped: {:?}",r)),r=&mut web=>Err(anyhow::anyhow!("HTTP server stopped: {:?}",r))}
    } else {
        tokio::select! {r=&mut registry=>r.context("registry join")?,r=&mut web=>Err(anyhow::anyhow!("HTTP server stopped: {:?}",r))}
    };
    web.abort();
    if let Some(mut child) = engine_process {
        if let Some(id) = child.id() {
            let _ = tokio::process::Command::new("kill")
                .args(["-TERM", &id.to_string()])
                .status()
                .await;
        }
        if tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .is_err()
        {
            child.kill().await?;
        }
    }
    outcome
}
async fn health(State(app): State<Arc<App>>) -> Web<Json<Value>> {
    app.http
        .get(format!("{}/health", app.engine))
        .send()
        .await?
        .error_for_status()?;
    Ok(Json(json!({"status":"ok"})))
}
async fn challenge(State(app): State<Arc<App>>, Json(c): Json<Certificate>) -> Web<Json<Value>> {
    c.validate(&app.issuer)?;
    let value = Challenge {
        device_id: c.device_id,
        public_key: c.public_key,
        nonce: auth::random(),
        expires: auth::now() + 120,
    };
    Ok(Json(
        json!({"challenge":auth::sign(&app.key,"challenge",&value)?}),
    ))
}
async fn enroll(
    State(app): State<Arc<App>>,
    Json(r): Json<EnrollmentRequest>,
) -> Web<Json<Bundle>> {
    let key = r.certificate.validate(&app.issuer)?;
    let challenge: Challenge = auth::verify(&app.key, "challenge", &r.challenge)?;
    check(
        challenge.expires > auth::now()
            && challenge.device_id == r.certificate.device_id
            && challenge.public_key == r.certificate.public_key,
        "invalid challenge",
    )?;
    auth::verify_proof(&key, &r.challenge, &r.proof)?;
    let bot = r.certificate.bot_id();
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
                device_id: r.certificate.device_id,
                public_key: r.certificate.public_key,
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

    let token:Value=app.http.post(format!("{}/auth/tokens",app.engine.trim_end_matches('/'))).bearer_auth(&app.admin).json(&json!({"namespace":app.namespace,"duration":900,"grants":[{"resource":"actor_gateway","target":{"id":actor_id},"operations":["read"]},{"resource":"actor_gateway","target":{"id":conversation_actor_id},"operations":["read"]},{"resource":"actor_gateway","target":{"id":memory_actor_id},"operations":["read"]}]})).send().await?.error_for_status()?.json().await?;
    Ok(Json(Bundle {
        bot_id: bot.clone(),
        actor_id,
        conversation_actor_id,
        memory_actor_id,
        endpoint: app.public_engine.clone(),
        namespace: app.namespace.clone(),
        gateway_token: token["token"].as_str().context("token missing")?.into(),
        session: app.session(&bot, "device")?,
        expires: auth::now() + 900,
    }))
}
#[derive(Deserialize)]
struct ClaimQuery {
    bot: String,
    token: String,
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn page(body: String) -> Response {
    let mut r=Html(format!("<!doctype html><html><head><meta name=viewport content='width=device-width,initial-scale=1'><title>Robot Buddy</title></head><body><main><h1>Robot Buddy</h1>{body}</main></body></html>")).into_response();
    r.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    r.headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    r.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    r
}
async fn claim(State(app): State<Arc<App>>, Query(q): Query<ClaimQuery>) -> Web<Response> {
    check(q.bot.len() <= 128 && q.token.len() <= 128, "invalid claim")?;
    if app.oidc.is_none() {
        return Ok(page("<p>The server administrator must configure sign-in before registration can finish. Your robot will retain its identity.</p>".into()));
    }
    let mut response = page(format!(
        "<p>Connect this robot to your account. Keep the robot nearby to confirm.</p><form method=post action=/login><input type=hidden name=bot value=\"{}\"><input type=hidden name=token value=\"{}\"><button>Sign in to register</button></form>",
        escape(&q.bot),
        escape(&q.token)
    ));
    // Browsers apply form-action to the login POST's redirect chain too.
    let authorize_origin = url::Url::parse(&app.oidc.as_ref().context("OIDC required")?.authorize)?
        .origin()
        .ascii_serialization();
    response.headers_mut().insert("content-security-policy",HeaderValue::from_str(&format!("default-src 'none'; form-action 'self' {authorize_origin}; frame-ancestors 'none'; base-uri 'none'"))?);
    Ok(response)
}
#[derive(Serialize, Deserialize)]
struct LoginState {
    bot: String,
    token: String,
    nonce: String,
    verifier: String,
    expires: i64,
}
async fn login(State(app): State<Arc<App>>, Form(q): Form<ClaimQuery>) -> Web<Response> {
    check(q.bot.len() <= 128 && q.token.len() <= 128, "invalid claim")?;
    let oidc = app.oidc.as_ref().context("sign-in not configured")?;
    let state = LoginState {
        bot: q.bot,
        token: q.token,
        nonce: auth::random(),
        verifier: format!("{}{}", auth::random(), auth::random()),
        expires: auth::now() + 600,
    };
    let mut url = url::Url::parse(&oidc.authorize)?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", oidc.client_id.as_str()),
        ("response_type", "code"),
        ("scope", "openid profile"),
        (
            "redirect_uri",
            &format!("{}/auth/callback", app.public_url.trim_end_matches('/')),
        ),
        ("state", &state.nonce),
        ("nonce", &state.nonce),
        ("code_challenge", &auth::digest(&state.verifier)),
        ("code_challenge_method", "S256"),
    ]);
    let mut response = Redirect::to(url.as_str()).into_response();
    let secure = if app.public_url.starts_with("https:") {
        "; Secure"
    } else {
        ""
    };
    response.headers_mut().insert(
        "set-cookie",
        HeaderValue::from_str(&format!(
            "buddy_login={}; HttpOnly; SameSite=Lax; Path=/auth/callback; Max-Age=600{secure}",
            auth::sign(&app.key, "login", &state)?
        ))?,
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
    let raw = headers
        .get("cookie")
        .context("login cookie missing")?
        .to_str()?
        .split(';')
        .find_map(|p| p.trim().strip_prefix("buddy_login="))
        .context("login cookie missing")?;
    let state: LoginState = auth::verify(&app.key, "login", raw)?;
    check(
        state.expires > auth::now() && state.nonce == q.state,
        "login expired",
    )?;
    let oidc = app.oidc.as_ref().context("sign-in unavailable")?;
    let token: Value = oidc::json(
        app.http
            .post(&oidc.token)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &oidc.client_id),
                ("client_secret", &oidc.client_secret),
                ("code", &q.code),
                ("code_verifier", &state.verifier),
                (
                    "redirect_uri",
                    &format!("{}/auth/callback", app.public_url.trim_end_matches('/')),
                ),
            ])
            .send()
            .await?,
    )
    .await?;
    let user: Value = oidc::json(
        app.http
            .get(&oidc.userinfo)
            .bearer_auth(
                token["access_token"]
                    .as_str()
                    .context("access token missing")?,
            )
            .send()
            .await?,
    )
    .await?;
    oidc::validate(
        &app.http,
        oidc,
        &state,
        token["id_token"].as_str().context("ID token missing")?,
        user["sub"].as_str().context("subject missing")?,
    )
    .await?;
    let owner = format!(
        "{}|{}",
        oidc.issuer,
        user["sub"].as_str().context("subject missing")?
    );
    let proposed = app
        .call(
            &state.bot,
            Command::ProposeClaim {
                token: state.token,
                owner,
            },
        )
        .await?;
    let mut response = page(format!(
        "<p>Match this code on your robot and press its confirm control:</p><h2>{}</h2><p>The robot will welcome you when registration is complete.</p>",
        escape(proposed["code"].as_str().context("code missing")?)
    ));
    response.headers_mut().insert(
        "set-cookie",
        HeaderValue::from_static(
            "buddy_login=; HttpOnly; SameSite=Lax; Path=/auth/callback; Max-Age=0",
        ),
    );
    Ok(response)
}
// Fail fast under overload; do not create an unbounded HTTP work queue.
async fn limit_requests(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    static CAPACITY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(128);
    let Ok(_permit) = CAPACITY.try_acquire() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await {
        Ok(response) => response,
        Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}
async fn spawn_engine(path: &str) -> Result<tokio::process::Child> {
    let data = std::env::var("BUDDY_DATA_DIR").unwrap_or_else(|_| "/data".into());
    tokio::fs::create_dir_all(format!("{data}/logs")).await?;
    tokio::fs::create_dir_all(format!("{data}/objects")).await?;
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
    let config = json!({"auth":{"admin_token":env("RIVET_TOKEN")?},"guard":{"host":"0.0.0.0","port":port},"api_peer":{"host":"127.0.0.1","port":port+1},"metrics":{"host":"127.0.0.1","port":port+10},"storage":{"log_directory":format!("{data}/logs"),"balancing":{"enabled":false},"auto_provision_partitions":1,"log_shard":{"max_disk_bytes":4294967296u64},"cold_store":{"disk_cache_bytes":0}},"object_storage":{"primary":object_storage},"telemetry":{"enabled":false}});
    let config_path = format!("{data}/engine.json");
    let temp = format!("{config_path}.{}", auth::random());
    private_write(
        std::path::Path::new(&temp),
        &serde_json::to_vec_pretty(&config)?,
    )?;
    std::fs::rename(temp, &config_path)?;
    let child = tokio::process::Command::new(path)
        .arg("--config")
        .arg(config_path)
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
            return Ok(child);
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
