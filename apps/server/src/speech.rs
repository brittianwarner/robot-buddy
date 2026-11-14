//! Transient media plane. Rivet owns text; audio is bounded, streamed, and never persisted.
use super::*;
use axum::{body::Body, http::header};
use futures_util::StreamExt;
use robot_buddy::{audio::PcmFormat, conversation::Conversation};
use std::{collections::HashSet, sync::Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
pub struct Speech {
    http: reqwest::Client,
    key: String,
    pub model: String,
    capacity: Arc<Semaphore>,
    active: Arc<Mutex<HashSet<String>>>,
}
struct Lease {
    bot: String,
    active: Arc<Mutex<HashSet<String>>>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.bot);
    }
}
impl Speech {
    pub fn from_env() -> Result<Option<Arc<Self>>> {
        let Some(model) = std::env::var("TTS_MODEL").ok().filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        ensure!(
            model == robot_buddy::voice::SPEECH_MODEL,
            "this PCM adapter currently supports {}",
            robot_buddy::voice::SPEECH_MODEL
        );
        let key = std::env::var("TTS_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                (std::env::var("AI_PROVIDER").unwrap_or_else(|_| "openrouter".into())
                    == "openrouter")
                    .then(|| std::env::var("AI_API_KEY").ok())
                    .flatten()
            })
            .context("TTS_API_KEY or OpenRouter AI_API_KEY required")?;
        ensure!(!key.is_empty(), "speech key missing");
        Ok(Some(Arc::new(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(90))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            key,
            model,
            capacity: Arc::new(Semaphore::new(4)),
            active: Arc::new(Mutex::new(HashSet::new())),
        })))
    }
    fn lease(&self, bot: &str) -> Result<Lease> {
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .context("speech capacity reached")?;
        ensure!(
            self.active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(bot.to_owned()),
            "speech already active for bot"
        );
        Ok(Lease {
            bot: bot.into(),
            active: self.active.clone(),
            _permit: permit,
        })
    }
}
pub async fn device(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<SpeechRequest>,
) -> Web<Response> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .context("session required")?
        .to_str()?
        .strip_prefix("Bearer ")
        .context("bearer session required")?;
    let session: auth::Session = auth::verify(&app.key, "session", bearer)?;
    session.validate(&session.bot_id, Some("device"))?;
    Ok(stream(&app, &session.bot_id, bearer, body).await?)
}
#[derive(Clone)]
struct Source {
    client: Client,
    bot: String,
    session: String,
    turn_id: String,
}
impl Source {
    async fn read(&self, index: u32) -> Result<Value> {
        let handle = self.client.get_typed::<Conversation>(
            "conversation",
            [self.bot.as_str()],
            GetOptions {
                params: Some(json!({"session":self.session})),
            },
        )?;
        tokio::time::timeout(
            Duration::from_secs(10),
            handle.send(Call {
                session: self.session.clone(),
                command: Command::SpeechChunk {
                    turn_id: self.turn_id.clone(),
                    index,
                },
            }),
        )
        .await
        .context("speech actor deadline")?
    }
    async fn wait(&self, index: u32, deadline: tokio::time::Instant) -> Result<Option<Value>> {
        loop {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "speech deadline exceeded"
            );
            let value = self.read(index).await?;
            match value["state"].as_str() {
                Some("ready") => return Ok(Some(value)),
                Some("done") => return Ok(None),
                Some("pending") => tokio::time::sleep(Duration::from_millis(100)).await,
                _ => anyhow::bail!("invalid speech state"),
            }
        }
    }
}
async fn synthesize(service: &Speech, answer: &Value) -> Result<(reqwest::Response, PcmFormat)> {
    let text = answer["text"].as_str().context("spoken text missing")?;
    let config: robot_buddy::voice::VoiceConfig =
        serde_json::from_value(answer["voice"]["config"].clone())?;
    let payload = config.speech_body(&service.model, text)?;
    let response = service
        .http
        .post("https://openrouter.ai/api/v1/audio/speech")
        .bearer_auth(&service.key)
        .json(&payload)
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(400).collect();
        anyhow::bail!("speech provider rejected request (HTTP {status}): {snippet}");
    }
    let format = PcmFormat::from_provider(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .context("audio type missing")?
            .to_str()?,
    )?;
    Ok((response, format))
}
async fn synthesize_guarded(
    service: &Speech,
    answer: &Value,
    source: &Source,
    index: u32,
) -> Result<(reqwest::Response, PcmFormat)> {
    let work = synthesize(service, answer);
    tokio::pin!(work);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {result=&mut work=>return result,_=tick.tick()=>{source.read(index).await?;}}
    }
}
type AudioStream = std::pin::Pin<
    Box<
        dyn futures_util::Stream<Item = std::result::Result<axum::body::Bytes, reqwest::Error>>
            + Send,
    >,
>;
struct Sequence {
    current: AudioStream,
    _lease: Lease,
    source: Source,
    service: Arc<Speech>,
    format: PcmFormat,
    index: u32,
    streaming: bool,
    total: usize,
    chunk_bytes: usize,
    deadline: tokio::time::Instant,
    check_at: tokio::time::Instant,
}
impl Sequence {
    async fn next(mut self) -> Result<Option<(axum::body::Bytes, Self)>> {
        loop {
            ensure!(
                tokio::time::Instant::now() < self.deadline,
                "speech deadline exceeded"
            );
            // Check revocation/turn cancellation even during a long provider stream.
            let next = if self.streaming {
                tokio::select! {
                    _=tokio::time::sleep_until(self.check_at)=>{self.source.read(self.index).await?;self.check_at=tokio::time::Instant::now()+Duration::from_millis(500);continue;},
                    next=tokio::time::timeout_at(self.deadline,self.current.next())=>next.context("speech deadline exceeded")?,
                }
            } else {
                tokio::time::timeout_at(self.deadline, self.current.next())
                    .await
                    .context("speech deadline exceeded")?
            };
            if let Some(bytes) = next {
                let bytes = bytes?;
                self.total += bytes.len();
                self.chunk_bytes += bytes.len();
                ensure!(self.total <= 16 * 1024 * 1024, "audio exceeds budget");
                return Ok(Some((bytes, self)));
            }
            ensure!(
                self.chunk_bytes > 0
                    && self
                        .chunk_bytes
                        .is_multiple_of(usize::from(self.format.channels) * 2),
                "incomplete PCM chunk"
            );
            if !self.streaming {
                return Ok(None);
            }
            self.index += 1;
            let Some(answer) = self.source.wait(self.index, self.deadline).await? else {
                return Ok(None);
            };
            // Await sequentially: no unbounded TTS fanout or reordering, and no paid retries.
            let (response, format) =
                synthesize_guarded(&self.service, &answer, &self.source, self.index).await?;
            ensure!(
                format == self.format,
                "speech format changed between chunks"
            );
            self.current = Box::pin(response.bytes_stream());
            self.chunk_bytes = 0;
        }
    }
}
pub async fn stream(app: &App, bot: &str, session: &str, body: SpeechRequest) -> Result<Response> {
    ensure!(
        !body.turn_id.is_empty() && body.turn_id.len() <= 128,
        "invalid turn id"
    );
    let service = app
        .speech
        .as_ref()
        .context("speech is not configured")?
        .clone();
    let lease = service.lease(bot)?;
    let source = Source {
        client: app.client.clone(),
        bot: bot.into(),
        session: session.into(),
        turn_id: body.turn_id.clone(),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    let answer = if body.stream {
        source
            .wait(0, deadline)
            .await?
            .context("no spoken chunks")?
    } else {
        app.client
            .get_typed::<Conversation>(
                "conversation",
                [bot],
                GetOptions {
                    params: Some(json!({"session":session})),
                },
            )?
            .send(Call {
                session: session.into(),
                command: Command::SpokenAnswer {
                    turn_id: body.turn_id.clone(),
                },
            })
            .await?
    };
    let (response, format) = if body.stream {
        synthesize_guarded(&service, &answer, &source, 0).await?
    } else {
        synthesize(&service, &answer).await?
    };
    let voice_id = answer["voice"]["voice_id"]
        .as_str()
        .context("voice missing")?;
    let revision = answer["voice"]["revision"]
        .as_i64()
        .context("voice revision missing")?;
    let sequence = Sequence {
        current: Box::pin(response.bytes_stream()),
        _lease: lease,
        source,
        service,
        format,
        index: 0,
        streaming: body.stream,
        total: 0,
        chunk_bytes: 0,
        deadline,
        check_at: tokio::time::Instant::now(),
    };
    let audio = futures_util::stream::try_unfold(sequence, |state| async move {
        state
            .next()
            .await
            .map_err(|_| std::io::Error::other("speech stream stopped"))
    });
    let mut result = Response::new(Body::from_stream(audio));
    for (name, value) in [
        ("content-type", format.content_type()),
        ("cache-control", "no-store".into()),
        ("x-audio-voice-id", voice_id.into()),
        ("x-audio-config-revision", revision.to_string()),
        ("x-audio-encoding", "pcm_s16le".into()),
        ("x-audio-turn-id", body.turn_id),
        ("x-accel-buffering", "no".into()),
    ] {
        result.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(result)
}
