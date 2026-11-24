//! Transient media plane. Rivet owns text; audio is bounded, streamed, and never persisted.
//!
//! `POST /v1/speech` takes a turn ID only. A spawned pipeline task reads committed
//! sentences from the Conversation actor, synthesizes them one sentence ahead and
//! forwards one ordered PCM stream through a bounded channel.
mod admission;
mod pipeline;
mod provider;
mod source;
#[cfg(test)]
mod tests;

use super::{
    App,
    web::{Kind, Reject, Web, require},
};
use admission::Admission;
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, header},
    response::Response,
};
use pipeline::{Head, Job, Limits};
use provider::OpenRouter;
use robot_buddy::{auth, protocol::SpeechRequest};
use source::ActorSource;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Concurrent speech streams per process.
const STREAMS: usize = 4;
/// Upstream synthesis requests per stream: the sentence playing and one prefetched.
const REQUESTS_PER_STREAM: usize = 2;

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
    /// A configured service with no stream capacity, for status tests. Never dials out.
    #[cfg(test)]
    pub fn without_capacity() -> Arc<Self> {
        let model = robot_buddy::voice::SPEECH_MODEL.to_owned();
        let key = auth::Secret::new("test-key".to_owned());
        Arc::new(Self {
            synth: Arc::new(OpenRouter::new(key, model.clone(), "http://127.0.0.1:9").unwrap()),
            model,
            admission: Admission::new(0),
            upstream: Arc::new(Semaphore::new(1)),
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

/// `POST /v1/speech`. A bad or missing session is 401, unconfigured speech 503, a full
/// stream cap 429 and anything else 400.
pub async fn device(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<SpeechRequest>,
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
    stream(&app, &session.bot_id, bearer, body).await
}

/// Starts one speech stream. The newest request for a bot replaces any older stream.
/// Unconfigured speech is 503 and a full stream cap 429; every other failure is 400.
pub async fn stream(app: &App, bot: &str, session: &str, body: SpeechRequest) -> Web<Response> {
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
    let source = ActorSource::open(&app.client, bot, session, &body.turn_id, body.stream).await?;
    let (head, audio) = pipeline::spawn(Job {
        source,
        synth: service.synth.clone(),
        upstream: service.upstream.clone(),
        lease,
        limits: service.limits,
    });
    let head = head.await.context("speech stream stopped")??;
    Ok(response(
        &head,
        &body.turn_id,
        Body::from_stream(audio.into_stream()),
    )?)
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
