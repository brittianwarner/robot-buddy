//! Optional operator-only simulator. All chat calls use the real Rivet actors.
use super::{
    App, Arc, Bot, Call, Command, Context, Deserialize, Duration, EnrollmentRequest, GetOptions,
    HeaderMap, HeaderValue, IntoResponse, Json, Kind, Reject, Response, Result, Router, Serialize,
    State, StatusCode, TypedClientExt, Value, Web, auth, cookie, get, header, json, post, require,
    same_origin, web,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use web::ApiError;
use web::CookieSpec;

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
fn configured(app: &App) -> Web<&str> {
    app.lab_token
        .as_ref()
        .map(|t| t.expose().as_str())
        .reject(Kind::NotFound)
}
fn origin(app: &App, headers: &HeaderMap) -> Web<()> {
    require(same_origin(&app.public_origin, headers), Kind::Forbidden)
}
fn session(app: &App, headers: &HeaderMap, mutation: bool) -> Web<LabSession> {
    configured(app)?;
    let raw = cookie(headers, "buddy_lab").reject(Kind::Unauthorized)?;
    let session: LabSession = auth::verify(&app.key, "lab", raw).reject(Kind::Unauthorized)?;
    require(session.expires > auth::now(), Kind::Unauthorized)?;
    if mutation {
        origin(app, headers)?;
        let supplied = headers
            .get("x-buddy-csrf")
            .map(HeaderValue::as_bytes)
            .unwrap_or_default();
        require(
            auth::ct_eq(supplied, session.csrf.as_bytes()),
            Kind::Forbidden,
        )?;
    }
    Ok(session)
}
fn lab_cookie(app: &App) -> CookieSpec {
    CookieSpec {
        name: "buddy_lab",
        path: "/lab",
        same_site: "Strict",
        max_age: 3600,
        secure: app.secure_cookies(),
    }
}
fn cookie_for(app: &App, s: &LabSession) -> Web<HeaderValue> {
    let signed = auth::sign(&app.key, "lab", s).reject(Kind::Internal)?;
    lab_cookie(app).set(&signed).reject(Kind::Internal)
}
/// Both tokens are run through HMAC under the server key, with the token as the message, and the
/// tags compared in constant time. (A token in the HMAC key position is zero-padded to the
/// block size, so `token` and `token\0` would collide.)
fn lab_token_matches(key: &[u8], expected: &str, supplied: &str) -> bool {
    let tag = |token: &str| {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
        mac.update(b"lab-login.");
        mac.update(token.as_bytes());
        Some(mac)
    };
    let (Some(expected), Some(supplied)) = (tag(expected), tag(supplied)) else {
        return false;
    };
    supplied
        .verify_slice(&expected.finalize().into_bytes())
        .is_ok()
}
fn asset(content: &'static str, content_type: &'static str) -> Response {
    let mut response = ([(header::CONTENT_TYPE, content_type)], content).into_response();
    security_headers(&mut response);
    response
}
const LAB_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    connect-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; \
    base-uri 'none'";
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
    response
        .headers_mut()
        .insert("content-security-policy", HeaderValue::from_static(LAB_CSP));
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
    require(
        (32..=256).contains(&body.token.len())
            && lab_token_matches(app.key.expose(), expected, &body.token),
        Kind::Unauthorized,
    )?;
    let s = LabSession {
        bot: None,
        csrf: auth::random(),
        expires: auth::now() + 3600,
    };
    let mut response = Json(json!({"csrf":s.csrf})).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie_for(&app, &s)?);
    security_headers(&mut response);
    Ok(response)
}
async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Web<Response> {
    session(&app, &headers, true)?;
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        lab_cookie(&app).clear().reject(Kind::Internal)?,
    );
    Ok(r)
}
async fn state(State(app): State<Arc<App>>, headers: HeaderMap) -> Web<Json<Value>> {
    let s = session(&app, &headers, false)?;
    let bot = if let Some(bot) = s.bot.as_deref() {
        Some(
            device_call(&app, bot, "bot", Command::Snapshot)
                .await
                .reject(Kind::BadGateway)?,
        )
    } else {
        None
    };
    Ok(Json(json!({
        "csrf": s.csrf,
        "bot": bot,
        "oidc_configured": app.oidc.is_some(),
        "provider": std::env::var("AI_PROVIDER").unwrap_or_else(|_| "openrouter".into()),
        "model": std::env::var("AI_MODEL").unwrap_or_default(),
        "speech_model": app.speech.as_ref().map(|s| s.model.as_str()),
        "reasoning_effort": std::env::var("AI_REASONING_EFFORT").unwrap_or_default(),
        "expires": s.expires,
    })))
}
async fn enroll_device(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<EnrollmentRequest>,
) -> Web<Response> {
    let mut s = session(&app, &headers, true)?;
    let Json(bundle) = super::enroll(State(app.clone()), Json(body)).await?;
    s.bot = Some(bundle.bot_id.clone());
    let mut response = Json(json!({
        "bot_id": bundle.bot_id,
        "actor_id": bundle.actor_id,
        "conversation_actor_id": bundle.conversation_actor_id,
        "memory_actor_id": bundle.memory_actor_id,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie_for(&app, &s)?);
    security_headers(&mut response);
    Ok(response)
}
async fn action(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<Action>,
) -> Web<Json<Value>> {
    let s = session(&app, &headers, true)?;
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
        return Err(ApiError(Kind::Forbidden));
    }
    let bot = s.bot.reject(Kind::BadRequest)?;
    let claim = matches!(body.command, Command::BeginClaim);
    let mut value = device_call(&app, &bot, &body.target, body.command)
        .await
        .reject(Kind::BadGateway)?;
    if claim {
        value = claim_qr(&app, &bot, &value).reject(Kind::Internal)?;
    }
    Ok(Json(value))
}
fn claim_qr(app: &App, bot: &str, claim: &Value) -> Result<Value> {
    let mut url = url::Url::parse(&format!("{}/claim", app.public_url.trim_end_matches('/')))?;
    url.query_pairs_mut()
        .append_pair("bot", bot)
        .append_pair("token", claim["token"].as_str().context("claim token")?);
    let qr = qrcode::QrCode::new(url.as_str())?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .build();
    Ok(json!({"url":url.as_str(),"qr":qr,"expires":claim["expires"]}))
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

/// Cookie, Origin and CSRF are checked from headers before the body is read.
async fn speech(
    State(app): State<Arc<App>>,
    admitted: Option<axum::Extension<super::limit::Admitted>>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Web<Response> {
    let s = session(&app, &headers, true)?;
    let bot = s.bot.reject(Kind::BadRequest)?;
    require(app.speech.is_some(), Kind::Unavailable)?;
    let body = super::speech::read(body).await?;
    let device = app.session(&bot, "device").reject(Kind::Internal)?;
    let admitted = admitted.map(|axum::Extension(admitted)| admitted);
    // Speech maps its own errors: 429 at the stream cap, 400 for an unknown turn.
    super::speech::stream(&app, admitted.as_ref(), &bot, &device, body).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_token_rejects_nul_padding() {
        let key = [7u8; 32];
        let token = "a".repeat(40);
        assert!(lab_token_matches(&key, &token, &token));
        assert!(!lab_token_matches(&key, &token, &format!("{token}\0")));
        assert!(!lab_token_matches(&key, &token, &format!("{token}\0\0\0")));
        assert!(!lab_token_matches(&key, &token, &"a".repeat(41)));
    }
}
