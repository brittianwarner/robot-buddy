//! Device enrollment: `POST /v1/challenge` then `POST /v1/enroll`. Unauthenticated by
//! nature, so both are rate-limited per client and read their small JSON body within
//! [`web::BODY_DEADLINE`].
use super::{App, Bot, Call, Command, web};
use anyhow::{Context, Result};
use axum::{Json, body::Body, extract::State, http::HeaderMap};
use rivetkit::{TypedClientExt, client::GetOrCreateOptions};
use robot_buddy::{
    auth::{self, Certificate, Challenge},
    protocol::{Bundle, EnrollmentRequest},
};
use serde_json::{Value, json};
use std::sync::Arc;
use web::{Kind, Reject, Web, require};

pub async fn challenge(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Body,
) -> Web<Json<Value>> {
    let c: Certificate = web::json(&headers, body).await?;
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

pub async fn enroll(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Body,
) -> Web<Json<Bundle>> {
    let request = web::json(&headers, body).await?;
    enroll_request(&app, request).await.map(Json)
}

/// Enrolls a device that proved possession of its key. Shared with the lab.
pub async fn enroll_request(app: &App, r: EnrollmentRequest) -> Web<Bundle> {
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
    provision_bot(app, r.certificate, challenge)
        .await
        .reject(Kind::BadGateway)
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
