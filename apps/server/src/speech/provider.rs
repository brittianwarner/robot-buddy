//! OpenRouter speech transport (`elevenlabs/eleven-v4-turbo`, PCM).
use super::pipeline::{Spoken, Synthesis, Synthesizer};
use anyhow::{Context, Result, bail, ensure};
use axum::http::header;
use futures_util::StreamExt;
use robot_buddy::{audio::PcmFormat, voice::VoiceConfig};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Provider error bodies are read this far and discarded, never logged.
const ERROR_BODY_LIMIT: usize = 1024;

/// Validates `TTS_BASE_URL`: HTTPS, or plain HTTP to localhost for fixtures.
pub fn base_url(raw: Option<&str>) -> Result<String> {
    let raw = raw.unwrap_or(DEFAULT_BASE_URL);
    let url = url::Url::parse(raw).context("invalid TTS_BASE_URL")?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "TTS_BASE_URL must not contain credentials, query or fragment"
    );
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && local),
        "TTS_BASE_URL requires HTTPS unless the host is localhost"
    );
    Ok(raw.trim_end_matches('/').to_owned())
}

pub struct OpenRouter {
    http: reqwest::Client,
    key: String,
    model: String,
    endpoint: String,
}

impl OpenRouter {
    pub fn new(key: String, model: String, base_url: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(90))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            key,
            model,
            endpoint: format!("{base_url}/audio/speech"),
        })
    }
}

impl Synthesizer for OpenRouter {
    async fn synthesize(&self, spoken: &Spoken) -> Result<Synthesis> {
        let config: VoiceConfig = serde_json::from_value(spoken.voice["config"].clone())?;
        let payload = config.speech_body(&self.model, &spoken.text)?;
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .json(&payload)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            // The body may echo the request text; it is drained boundedly and dropped.
            discard(response).await;
            tracing::warn!(status = status.as_u16(), "speech provider rejected request");
            bail!(
                "speech provider rejected request (HTTP {})",
                status.as_u16()
            );
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .context("audio type missing")?
            .to_str()?;
        let format = PcmFormat::from_provider(content_type)?;
        let audio = response
            .bytes_stream()
            .map(|item| item.map_err(anyhow::Error::from))
            .boxed();
        Ok(Synthesis { format, audio })
    }
}

async fn discard(response: reqwest::Response) {
    let mut body = response.bytes_stream();
    let mut read = 0;
    while read < ERROR_BODY_LIMIT {
        match tokio::time::timeout(Duration::from_secs(1), body.next()).await {
            Ok(Some(Ok(bytes))) => read += bytes.len(),
            _ => break,
        }
    }
}
