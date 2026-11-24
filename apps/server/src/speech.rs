//! Transient media plane. Rivet owns text; audio is bounded, streamed, and never persisted.
//!
//! `POST /v1/speech` takes a turn ID only. A spawned pipeline task reads committed
//! sentences from the Conversation actor, synthesizes them one sentence ahead and
//! forwards one ordered PCM stream through a bounded channel.
//!
//! The caller is authenticated from headers before the body is read. The body read has
//! a short deadline. Only an authenticated request that took a stream slot gets the long
//! speech head deadline.
//!
//! The caller's bearer is checked once, at admission. The stream then runs on a session
//! the server mints for it, valid for the stream's own deadline, so a bearer that expires
//! mid-answer does not cut the audio. Every actor call still re-authorizes against Bot,
//! so revoking the bot stops the stream.
mod admission;
mod error;
mod pipeline;
mod provider;
mod source;
#[cfg(test)]
mod tests;

use super::{
    App,
    limit::{Admitted, SPEECH_HEAD_DEADLINE},
    web::{self, ApiError, Kind, Reject, Web, require},
};
use admission::Admission;
use anyhow::{Context, Result, ensure};
use axum::{
    Extension,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, header},
    response::Response,
};
use error::SpeechError;
use pipeline::{Head, Job, Limits};
use provider::OpenRouter;
use robot_buddy::{auth, protocol::SpeechRequest};
use source::ActorSource;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

/// Concurrent speech streams per process.
const STREAMS: usize = 4;
/// Upstream synthesis requests per stream: the sentence playing and one prefetched.
const REQUESTS_PER_STREAM: usize = 2;
/// A speech body is a turn ID and a flag, read after authentication within
/// [`web::BODY_DEADLINE`].
const BODY_LIMIT: usize = 1024;
/// A stream's own session outlives its deadline by this much, so the last check of a
/// stream that runs to its deadline is still valid.
const STREAM_SESSION_SLACK: Duration = Duration::from_mins(1);

pub struct Speech {
    pub model: String,
    synth: Arc<OpenRouter>,
    admission: Admission,
    upstream: Arc<Semaphore>,
    limits: Limits,
}

impl Speech {
    pub fn from_env() -> Result<Option<Arc<Self>>> {
        let Some(model) = non_empty_env("TTS_MODEL") else {
            return Ok(None);
        };
        ensure!(
            model == robot_buddy::voice::SPEECH_MODEL,
            "this PCM adapter currently supports {}",
            robot_buddy::voice::SPEECH_MODEL
        );
        let base = provider::base_url(non_empty_env("TTS_BASE_URL").as_deref())?;
        let key = speech_key(base != provider::DEFAULT_BASE_URL)?;
        Ok(Some(Arc::new(Self {
            synth: Arc::new(OpenRouter::new(key, model.clone(), &base)?),
            model,
            admission: Admission::new(STREAMS),
            upstream: Arc::new(Semaphore::new(STREAMS * REQUESTS_PER_STREAM)),
            limits: Limits::default(),
        })))
    }
    /// A configured service with `streams` slots, for status tests. Its provider is a
    /// closed port.
    #[cfg(test)]
    pub fn for_tests(streams: usize) -> Arc<Self> {
        let model = robot_buddy::voice::SPEECH_MODEL.to_owned();
        let key = auth::Secret::new("test-key".to_owned());
        Arc::new(Self {
            synth: Arc::new(OpenRouter::new(key, model.clone(), "http://127.0.0.1:9").unwrap()),
            model,
            admission: Admission::new(streams),
            upstream: Arc::new(Semaphore::new(streams.max(1) * REQUESTS_PER_STREAM)),
            limits: Limits::default(),
        })
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// The `OpenRouter` inference key is reused only for the default `OpenRouter` host, so a
/// custom `TTS_BASE_URL` never receives it implicitly.
fn speech_key(custom_base: bool) -> Result<auth::Secret<String>> {
    if let Some(key) = non_empty_env("TTS_API_KEY") {
        return Ok(auth::Secret::new(key));
    }
    ensure!(!custom_base, "TTS_API_KEY required with TTS_BASE_URL");
    let provider = std::env::var("AI_PROVIDER").unwrap_or_else(|_| "openrouter".into());
    ensure!(
        provider == "openrouter",
        "TTS_API_KEY or OpenRouter AI_API_KEY required"
    );
    non_empty_env("AI_API_KEY")
        .map(auth::Secret::new)
        .context("TTS_API_KEY or OpenRouter AI_API_KEY required")
}

/// `POST /v1/speech`. A bad or missing session is 401 before any body byte is read and
/// unconfigured speech 503; then [`read`] and [`stream`] name the failure.
pub async fn device(
    State(app): State<Arc<App>>,
    admitted: Option<Extension<Admitted>>,
    headers: HeaderMap,
    body: Body,
) -> Web<Response> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .reject(Kind::Unauthorized)?;
    let session: auth::Session =
        auth::verify(app.key.expose(), "session", bearer).reject(Kind::Unauthorized)?;
    session
        .validate(&session.bot_id, Some("device"))
        .reject(Kind::Unauthorized)?;
    require(app.speech.is_some(), Kind::Unavailable)?;
    let body = read(body).await?;
    let admitted = admitted.map(|Extension(admitted)| admitted);
    stream(&app, admitted.as_ref(), &session.bot_id, body).await
}

/// Reads an already-authenticated speech body: at most 1 KiB, 408 when it does not arrive
/// within [`web::BODY_DEADLINE`], else 400 when malformed.
pub async fn read(body: Body) -> Web<SpeechRequest> {
    let bytes = web::read_body(body, BODY_LIMIT).await?;
    serde_json::from_slice(&bytes).reject(Kind::BadRequest)
}

/// When a stream's own session expires, for a stream admitted at `now`.
fn stream_session_expiry(now: i64, limits: &Limits) -> i64 {
    let lifetime = limits.deadline + STREAM_SESSION_SLACK;
    now.saturating_add(i64::try_from(lifetime.as_secs()).unwrap_or(i64::MAX))
}

/// Starts one speech stream for an authenticated caller. The newest request for a bot
/// replaces any older stream; holding a stream slot earns the long head deadline.
///
/// Statuses: 400 bad turn id, 503 unconfigured or engine unreachable, 429 stream cap,
/// then before the first audio 403 access revoked, 409 turn unknown, stopped or
/// replaced, 404 nothing to speak, 502 engine or provider failure, 504 timeout.
pub async fn stream(
    app: &App,
    admitted: Option<&Admitted>,
    bot: &str,
    body: SpeechRequest,
) -> Web<Response> {
    require(
        !body.turn_id.is_empty() && body.turn_id.len() <= 128,
        Kind::BadRequest,
    )?;
    let service = app.speech.as_ref().reject(Kind::Unavailable)?;
    let lease = service
        .admission
        .lease(bot)
        .await
        .reject(Kind::TooManyRequests)?;
    if let Some(admitted) = admitted {
        admitted.extend(SPEECH_HEAD_DEADLINE);
    }
    let expires = stream_session_expiry(auth::now(), &service.limits);
    let session = app
        .session_until(bot, "device", expires)
        .reject(Kind::Internal)?;
    let source = ActorSource::open(&app.client, bot, &session, &body.turn_id, body.stream)
        .await
        .reject(Kind::BadGateway)?;
    let (head, audio) = pipeline::spawn(Job {
        source,
        synth: service.synth.clone(),
        upstream: service.upstream.clone(),
        lease,
        limits: service.limits,
    });
    let head = match head.await {
        Ok(Ok(head)) => head,
        Ok(Err(error)) => return Err(ApiError(failure(app, bot, &session, &error).await)),
        Err(_) => return Err(ApiError(Kind::BadGateway)),
    };
    response(&head, &body.turn_id, Body::from_stream(audio.into_stream())).reject(Kind::Internal)
}

/// The status for a stream that failed before its first audio. An actor's refusal is
/// masked, so a refusal is followed by an access check: refused again means the bot's
/// access is gone (403); allowed means the turn itself cannot be spoken (409).
async fn failure(app: &App, bot: &str, session: &str, error: &anyhow::Error) -> Kind {
    match SpeechError::of(error) {
        Some(SpeechError::Refused) => {
            let check = robot_buddy::actor::check_access(app.client.clone(), bot, session);
            match tokio::time::timeout(Duration::from_secs(5), check).await {
                Ok(Ok(())) => Kind::Conflict,
                Ok(Err(error)) => match SpeechError::from_client(&error) {
                    SpeechError::Refused => Kind::Forbidden,
                    other => other.kind(),
                },
                Err(_) => Kind::GatewayTimeout,
            }
        }
        Some(kind) => kind.kind(),
        None => Kind::BadGateway,
    }
}

fn response(head: &Head, turn_id: &str, body: Body) -> Result<Response> {
    let mut result = Response::new(body);
    for (name, value) in [
        ("content-type", head.format.content_type()),
        ("cache-control", "no-store".into()),
        ("x-audio-voice-id", head.voice_id.clone()),
        ("x-audio-config-revision", head.revision.to_string()),
        ("x-audio-encoding", "pcm_s16le".into()),
        ("x-audio-turn-id", turn_id.into()),
        ("x-accel-buffering", "no".into()),
    ] {
        result.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(result)
}
