//! Optional operator-only simulator. All chat calls use the real Rivet actors.
//!
//! Mounted only when `BUDDY_LAB_TOKEN` is set. Sessions live server-side: the
//! `buddy_lab` cookie carries only an opaque id, and logout deletes the session. Every
//! API route but login is checked from headers by [`guard`] (the cookie, and for a POST
//! the Origin and CSRF header) before its body is read; login checks Origin first.
use super::{App, Bot, Call, Command, enroll, limit::Admitted, speech, web};
use anyhow::{Context, Result};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, header},
    middleware::{Next, from_fn_with_state, map_response},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use rivetkit::{TypedClientExt, client::GetOptions};
use robot_buddy::{auth, protocol::EnrollmentRequest};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use web::{ApiError, CookieSpec, Kind, Reject, Web, cookie, require, same_origin};

/// Lab sessions alive at once; a new login past this replaces the oldest.
const MAX_SESSIONS: usize = 32;
const SESSION_TTL: i64 = 3600;

#[derive(Clone)]
struct LabSession {
    bot: Option<String>,
    csrf: String,
    expires: i64,
}

/// Lab sessions, keyed by the digest of their cookie id.
#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, LabSession>>);

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LabSession>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// A new session and its cookie id.
    fn create(&self, now: i64) -> (String, LabSession) {
        let mut sessions = self.lock();
        sessions.retain(|_, s| s.expires > now);
        if sessions.len() >= MAX_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest {
                sessions.remove(&key);
            }
        }
        let id = format!("{}{}", auth::random(), auth::random()).replace('-', "");
        let session = LabSession {
            bot: None,
            csrf: auth::random(),
            expires: now + SESSION_TTL,
        };
        sessions.insert(auth::digest(&id), session.clone());
        (id, session)
    }
    fn get(&self, id: &str, now: i64) -> Option<LabSession> {
        let mut sessions = self.lock();
        let key = auth::digest(id);
        match sessions.get(&key) {
            Some(s) if s.expires > now => Some(s.clone()),
            Some(_) => {
                sessions.remove(&key);
                None
            }
            None => None,
        }
    }
    fn set_bot(&self, id: &str, bot: String) {
        if let Some(session) = self.lock().get_mut(&auth::digest(id)) {
            session.bot = Some(bot);
        }
    }
    fn remove(&self, id: &str) {
        self.lock().remove(&auth::digest(id));
    }
}

/// The checked session of a lab API request, inserted by [`guard`].
#[derive(Clone)]
struct Current {
    id: String,
    session: LabSession,
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

pub fn routes(app: &Arc<App>) -> Router<Arc<App>> {
    let api = Router::new()
        .route("/lab/api/logout", post(logout))
        .route("/lab/api/state", get(state))
        .route("/lab/api/enroll", post(enroll_device))
        .route("/lab/api/action", post(action))
        .route("/lab/api/speech", post(speech))
        .route_layer(from_fn_with_state(app.clone(), guard));
    Router::new()
        .route("/lab", get(index))
        .route("/lab/app.js", get(js))
        .route("/lab/voice.js", get(voice_js))
        .route("/lab/style.css", get(css))
        .route("/lab/api/login", post(login))
        .merge(api)
        .layer(map_response(|mut response: Response| async move {
            response.headers_mut().insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(LAB_CSP),
            );
            response
        }))
}

fn origin(app: &App, headers: &HeaderMap) -> Web<()> {
    require(same_origin(&app.public_origin, headers), Kind::Forbidden)
}

/// Checks the session from headers alone; a POST also needs Origin and the CSRF header.
fn check(app: &App, mutation: bool, headers: &HeaderMap) -> Web<Current> {
    let id = cookie(headers, "buddy_lab").reject(Kind::Unauthorized)?;
    let session = app
        .lab_sessions
        .get(id, auth::now())
        .reject(Kind::Unauthorized)?;
    if mutation {
        origin(app, headers)?;
        let supplied = headers
            .get("x-buddy-csrf")
            .map_or_default(HeaderValue::as_bytes);
        require(
            auth::ct_eq(supplied, session.csrf.as_bytes()),
            Kind::Forbidden,
        )?;
    }
    Ok(Current {
        id: id.to_owned(),
        session,
    })
}

async fn guard(State(app): State<Arc<App>>, mut request: Request, next: Next) -> Response {
    let mutation = request.method() != Method::GET && request.method() != Method::HEAD;
    match check(&app, mutation, request.headers()) {
        Ok(current) => {
            request.extensions_mut().insert(current);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

fn lab_cookie(app: &App) -> CookieSpec {
    CookieSpec {
        name: "buddy_lab",
        path: "/lab",
        same_site: "Strict",
        max_age: SESSION_TTL,
        secure: app.secure_cookies(),
    }
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
    ([(header::CONTENT_TYPE, content_type)], content).into_response()
}
const LAB_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    connect-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; \
    base-uri 'none'";
async fn index() -> Response {
    asset(include_str!("lab/index.html"), "text/html; charset=utf-8")
}
async fn js() -> Response {
    asset(include_str!("lab/app.js"), "text/javascript; charset=utf-8")
}
async fn css() -> Response {
    asset(include_str!("lab/style.css"), "text/css; charset=utf-8")
}
/// Origin is checked from headers before the token body is read.
async fn login(State(app): State<Arc<App>>, headers: HeaderMap, body: Body) -> Web<Response> {
    origin(&app, &headers)?;
    let expected = app.lab_token.as_ref().reject(Kind::NotFound)?;
    let body: Login = web::json(&headers, body).await?;
    require(
        (32..=256).contains(&body.token.len())
            && lab_token_matches(app.key.expose(), expected.expose(), &body.token),
        Kind::Unauthorized,
    )?;
    let (id, session) = app.lab_sessions.create(auth::now());
    let mut response = Json(json!({"csrf":session.csrf})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        lab_cookie(&app).set(&id).reject(Kind::Internal)?,
    );
    Ok(response)
}
/// Ends the session server-side, so the cookie is dead even if a copy survives.
async fn logout(
    State(app): State<Arc<App>>,
    Extension(current): Extension<Current>,
) -> Web<Response> {
    app.lab_sessions.remove(&current.id);
    let mut r = axum::http::StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        lab_cookie(&app).clear().reject(Kind::Internal)?,
    );
    Ok(r)
}
async fn state(
    State(app): State<Arc<App>>,
    Extension(Current { session: s, .. }): Extension<Current>,
) -> Web<Json<Value>> {
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
    Extension(current): Extension<Current>,
    headers: HeaderMap,
    body: Body,
) -> Web<Json<Value>> {
    let request: EnrollmentRequest = web::json(&headers, body).await?;
    let bundle = enroll::enroll_request(&app, request).await?;
    app.lab_sessions.set_bot(&current.id, bundle.bot_id.clone());
    Ok(Json(json!({
        "bot_id": bundle.bot_id,
        "actor_id": bundle.actor_id,
        "conversation_actor_id": bundle.conversation_actor_id,
        "memory_actor_id": bundle.memory_actor_id,
    })))
}
async fn action(
    State(app): State<Arc<App>>,
    Extension(Current { session: s, .. }): Extension<Current>,
    headers: HeaderMap,
    body: Body,
) -> Web<Json<Value>> {
    let body: Action = web::json(&headers, body).await?;
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
                | Command::SetSystemPrompt { .. }
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

/// The session, Origin and CSRF were checked by [`guard`] before the body is read.
async fn speech(
    State(app): State<Arc<App>>,
    Extension(Current { session: s, .. }): Extension<Current>,
    admitted: Option<Extension<Admitted>>,
    body: Body,
) -> Web<Response> {
    let bot = s.bot.reject(Kind::BadRequest)?;
    require(app.speech.is_some(), Kind::Unavailable)?;
    let body = speech::read(body).await?;
    let admitted = admitted.map(|Extension(admitted)| admitted);
    speech::stream(&app, admitted.as_ref(), &bot, body).await
}

async fn voice_js() -> Response {
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
