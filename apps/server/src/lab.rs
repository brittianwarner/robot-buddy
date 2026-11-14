//! Optional operator-only simulator. All chat calls use the real Rivet actors.
use super::*;
use axum::http::header;
use hmac::{Hmac, Mac};
use sha2::Sha256;

#[derive(Serialize, Deserialize)]
struct LabSession {
    bot: Option<String>,
    csrf: String,
    expires: i64,
}
#[derive(Deserialize)]
struct Login {
    token: String,
}
#[derive(Deserialize)]
struct Action {
    target: String,
    command: Command,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/lab", get(index))
        .route("/lab/app.js", get(js))
        .route("/lab/voice.js", get(voice_js))
        .route("/lab/style.css", get(css))
        .route("/lab/api/login", post(login))
        .route("/lab/api/logout", post(logout))
        .route("/lab/api/state", get(state))
        .route("/lab/api/enroll", post(enroll_device))
        .route("/lab/api/action", post(action))
        .route("/lab/api/speech", post(speech))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                security_headers(&mut response);
                response
            },
        ))
}
fn configured(app: &App) -> Result<&str> {
    app.lab_token.as_deref().context("lab disabled")
}
fn origin(app: &App, headers: &HeaderMap) -> Result<()> {
    let expected = url::Url::parse(&app.public_url)?
        .origin()
        .ascii_serialization();
    ensure!(
        headers.get("origin").and_then(|v| v.to_str().ok()) == Some(expected.as_str()),
        "origin rejected"
    );
    Ok(())
}
fn session(app: &App, headers: &HeaderMap, mutation: bool) -> Result<LabSession> {
    configured(app)?;
    let cookie = headers
        .get(header::COOKIE)
        .context("login required")?
        .to_str()?
        .split(';')
        .find_map(|s| s.trim().strip_prefix("buddy_lab="))
        .context("login required")?;
    let session: LabSession = auth::verify(&app.key, "lab", cookie)?;
    ensure!(session.expires > auth::now(), "lab session expired");
    if mutation {
        origin(app, headers)?;
        ensure!(
            headers.get("x-buddy-csrf").and_then(|v| v.to_str().ok())
                == Some(session.csrf.as_str()),
            "CSRF rejected"
        );
    }
    Ok(session)
}
fn cookie(app: &App, s: &LabSession) -> Result<HeaderValue> {
    let secure = if app.public_url.starts_with("https:") {
        "; Secure"
    } else {
        ""
    };
    Ok(HeaderValue::from_str(&format!(
        "buddy_lab={}; HttpOnly; SameSite=Strict; Path=/lab; Max-Age=3600{secure}",
        auth::sign(&app.key, "lab", s)?
    ))?)
}
fn asset(content: &'static str, content_type: &'static str) -> Response {
    let mut response = ([(header::CONTENT_TYPE, content_type)], content).into_response();
    security_headers(&mut response);
    response
}
pub(super) fn security_headers(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert("content-security-policy",HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"));
}
async fn index(State(app): State<Arc<App>>) -> Response {
    if app.lab_token.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset(include_str!("lab/index.html"), "text/html; charset=utf-8")
}
async fn js(State(app): State<Arc<App>>) -> Response {
    if app.lab_token.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset(include_str!("lab/app.js"), "text/javascript; charset=utf-8")
}
async fn css(State(app): State<Arc<App>>) -> Response {
    if app.lab_token.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset(include_str!("lab/style.css"), "text/css; charset=utf-8")
}
async fn login(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<Login>,
) -> Web<Response> {
    origin(&app, &headers)?;
    let expected = configured(&app)?;
    if !(32..=256).contains(&body.token.len()) {
        return Err(anyhow::anyhow!("invalid lab credential").into());
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(expected.as_bytes())?;
    mac.update(b"lab-login");
    let mut supplied = Hmac::<Sha256>::new_from_slice(body.token.as_bytes())?;
    supplied.update(b"lab-login");
    mac.verify_slice(&supplied.finalize().into_bytes())?;
    let s = LabSession {
        bot: None,
        csrf: auth::random(),
        expires: auth::now() + 3600,
    };
    let mut response = Json(json!({"csrf":s.csrf})).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie(&app, &s)?);
    security_headers(&mut response);
    Ok(response)
}
async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Web<Response> {
    session(&app, &headers, true)?;
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("buddy_lab=; HttpOnly; SameSite=Strict; Path=/lab; Max-Age=0"),
    );
    Ok(r)
}
async fn state(State(app): State<Arc<App>>, headers: HeaderMap) -> Web<Json<Value>> {
    let s = session(&app, &headers, false)?;
    let bot = if let Some(bot) = s.bot.as_deref() {
        Some(device_call(&app, bot, "bot", Command::Snapshot).await?)
    } else {
        None
    };
    Ok(Json(
        json!({"csrf":s.csrf,"bot":bot,"oidc_configured":app.oidc.is_some(),"provider":std::env::var("AI_PROVIDER").unwrap_or_else(|_|"openrouter".into()),"model":std::env::var("AI_MODEL").unwrap_or_default(),"speech_model":app.speech.as_ref().map(|s|s.model.as_str()),"reasoning_effort":std::env::var("AI_REASONING_EFFORT").unwrap_or_default(),"expires":s.expires}),
    ))
}
async fn enroll_device(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<EnrollmentRequest>,
) -> Web<Response> {
    let mut s = session(&app, &headers, true)?;
    let Json(bundle) = super::enroll(State(app.clone()), Json(body)).await?;
    s.bot = Some(bundle.bot_id.clone());
    let mut response=Json(json!({"bot_id":bundle.bot_id,"actor_id":bundle.actor_id,"conversation_actor_id":bundle.conversation_actor_id,"memory_actor_id":bundle.memory_actor_id})).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie(&app, &s)?);
    security_headers(&mut response);
    Ok(response)
}
async fn action(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<Action>,
) -> Web<Json<Value>> {
    let s = session(&app, &headers, true)?;
    let bot = s.bot.context("enroll a device first")?;
    if !matches!(
        (&*body.target, &body.command),
        (
            "bot",
            Command::Snapshot | Command::BeginClaim | Command::ConfirmClaim { .. }
        ) | (
            "conversation",
            Command::Snapshot
                | Command::History { .. }
                | Command::SearchHistory { .. }
                | Command::Submit { .. }
                | Command::Cancel { .. }
                | Command::Replay { .. }
                | Command::ListPersonas
                | Command::GetPersona { .. }
                | Command::GetVoiceConfig
                | Command::GetContextConfig
                | Command::GetSystemPrompt
                | Command::SystemPromptHistory
        ) | (
            "memory",
            Command::Snapshot | Command::SearchMemory { .. } | Command::Forget { .. }
        )
    ) {
        return Err(anyhow::anyhow!("command not allowed in lab").into());
    }
    let claim = matches!(body.command, Command::BeginClaim);
    let mut value = device_call(&app, &bot, &body.target, body.command).await?;
    if claim {
        let mut url = url::Url::parse(&format!("{}/claim", app.public_url.trim_end_matches('/')))?;
        url.query_pairs_mut()
            .append_pair("bot", &bot)
            .append_pair("token", value["token"].as_str().context("claim token")?);
        let qr = qrcode::QrCode::new(url.as_str())?
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(220, 220)
            .build();
        value = json!({"url":url.as_str(),"qr":qr,"expires":value["expires"]});
    }
    Ok(Json(value))
}
async fn device_call(app: &App, bot: &str, target: &str, command: Command) -> Result<Value> {
    let session = app.session(bot, "device")?;
    let options = GetOptions {
        params: Some(json!({"session":session})),
    };
    let call = Call { session, command };
    let work = async {
        match target {
            "bot" => {
                app.client
                    .get_typed::<Bot>("bot", [bot], options)?
                    .send(call)
                    .await
            }
            "conversation" => {
                app.client
                    .get_typed::<robot_buddy::conversation::Conversation>(
                        "conversation",
                        [bot],
                        options,
                    )?
                    .send(call)
                    .await
            }
            "memory" => {
                app.client
                    .get_typed::<robot_buddy::memory::Memory>("memory", [bot], options)?
                    .send(call)
                    .await
            }
            _ => anyhow::bail!("unknown actor"),
        }
    };
    tokio::time::timeout(Duration::from_secs(20), work)
        .await
        .context("actor request timed out")?
}

async fn speech(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<SpeechRequest>,
) -> Web<Response> {
    let s = session(&app, &headers, true)?;
    let bot = s.bot.context("enroll a device first")?;
    Ok(super::speech::stream(&app, &bot, &app.session(&bot, "device")?, body).await?)
}

async fn voice_js(State(app): State<Arc<App>>) -> Response {
    if app.lab_token.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset(
        include_str!("lab/voice.js"),
        "text/javascript; charset=utf-8",
    )
}
