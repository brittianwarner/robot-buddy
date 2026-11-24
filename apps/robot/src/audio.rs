//! Linux playback for the same authenticated PCM endpoint used by the browser lab.
//!
//! One pooled HTTP client serves every turn. Each turn spawns a shell-free
//! `aplay` while the speech request is in flight (so the player's start-up
//! overlaps the wait for audio), pipes PCM into it with a progress deadline
//! on every write, and reports the first PCM byte and the end of playback to
//! the session. Stop drops the task, which kills the player at once instead
//! of draining it.
use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::StreamExt;
use robot_buddy::{audio::PcmFormat, protocol::SpeechRequest};
use std::{future::Future, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStderr, ChildStdin},
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, timeout},
};

const DEFAULT_BUFFER_US: u32 = 120_000;
/// A write to the player must make progress within this long. Slow but
/// steady reading (a full pipe draining at the sample rate) never trips it.
const WRITE_DEADLINE: Duration = Duration::from_secs(2);
/// Largest single write into the player's pipe.
const WRITE_SLICE: usize = 16 * 1024;
/// Longest silence between body chunks of the speech stream.
const CHUNK_IDLE: Duration = Duration::from_secs(30);
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);
const AUDIO_BUDGET: usize = 16 * 1024 * 1024;
/// Player diagnostics kept for the journal; the rest is read and dropped.
const STDERR_CAP: usize = 1024;
/// The server's format (`OpenRouter` PCM). The player starts with it before
/// the response headers arrive and is restarted only if they differ.
const EXPECTED_FORMAT: PcmFormat = PcmFormat {
    sample_rate: 24_000,
    channels: 1,
};

/// `aplay` options for the I2S amplifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerConfig {
    /// ALSA PCM name passed as `-D` (for example `plughw:CARD=MAX98357A,DEV=0`).
    pub device: Option<String>,
    /// ALSA buffer in microseconds. Smaller stops sooner on barge-in but
    /// risks underruns when the Pi is busy.
    pub buffer_us: u32,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            device: None,
            buffer_us: DEFAULT_BUFFER_US,
        }
    }
}

impl PlayerConfig {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Self> {
        let buffer_us = match get("BUDDY_ALSA_BUFFER_US") {
            Some(v) => v.parse().context("BUDDY_ALSA_BUFFER_US")?,
            None => DEFAULT_BUFFER_US,
        };
        ensure!(
            (20_000..=1_000_000).contains(&buffer_us),
            "BUDDY_ALSA_BUFFER_US must be 20000..=1000000"
        );
        let device = get("BUDDY_ALSA_DEVICE").filter(|d| !d.is_empty());
        if let Some(device) = &device {
            let allowed = |c: char| c.is_ascii_alphanumeric() || ":,=_.-".contains(c);
            ensure!(
                device.len() <= 128 && !device.starts_with('-') && device.chars().all(allowed),
                "invalid BUDDY_ALSA_DEVICE"
            );
        }
        Ok(Self { device, buffer_us })
    }

    pub fn args(&self, format: PcmFormat) -> Vec<String> {
        let mut args = vec![
            "-q".into(),
            "-t".into(),
            "raw".into(),
            "-f".into(),
            "S16_LE".into(),
            format!("--buffer-time={}", self.buffer_us),
            "-r".into(),
            format.sample_rate.to_string(),
            "-c".into(),
            format.channels.to_string(),
        ];
        if let Some(device) = &self.device {
            args.push("-D".into());
            args.push(device.clone());
        }
        args
    }
}

pub type PlayerFactory = Arc<dyn Fn(PcmFormat) -> tokio::process::Command + Send + Sync>;

/// Shared media settings and the pooled HTTP client.
pub struct Media {
    http: reqwest::Client,
    server: String,
    enabled: bool,
    make_player: PlayerFactory,
    write_deadline: Duration,
    chunk_idle: Duration,
    /// Start the player before the response headers. Off only in the
    /// first-audio measurement.
    prespawn: bool,
}

impl Media {
    pub fn new(server: &str, enabled: bool, player: PlayerConfig) -> Result<Self> {
        let make_player: PlayerFactory = Arc::new(move |format| {
            let mut command = tokio::process::Command::new("aplay");
            command.args(player.args(format)).stdout(Stdio::null());
            command
        });
        Ok(Self {
            http: client()?,
            server: server.trim_end_matches('/').to_owned(),
            enabled,
            make_player,
            write_deadline: WRITE_DEADLINE,
            chunk_idle: CHUNK_IDLE,
            prespawn: true,
        })
    }

    #[cfg(test)]
    fn for_test(server: &str, make_player: PlayerFactory, write_deadline: Duration) -> Self {
        Self {
            http: client().unwrap(),
            server: server.to_owned(),
            enabled: true,
            make_player,
            write_deadline,
            chunk_idle: CHUNK_IDLE,
            prespawn: true,
        }
    }

    /// Open a pooled connection ahead of the first turn so the TLS handshake
    /// is not on the first-audio path.
    pub async fn prewarm(&self) {
        let request = self.http.get(format!("{}/health", self.server)).send();
        if let Ok(Ok(response)) = timeout(Duration::from_secs(5), request).await {
            let _ = response.bytes().await;
        }
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // Server media deadline is 300 s; this bounds a single speech request.
        .timeout(Duration::from_secs(320))
        .pool_idle_timeout(Duration::from_mins(5))
        .pool_max_idle_per_host(2)
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackEvent {
    /// The player's pipe took the first PCM bytes.
    Started { generation: u64 },
    /// Playback ended. `played` is false when no audio reached the player.
    Finished { generation: u64, played: bool },
}

/// Whether a stop warms a fresh speech connection for later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rewarm {
    /// The robot goes idle (wake, mute, an ended turn): warm one for the next turn.
    Yes,
    /// A new speech request follows at once (a replacing utterance), or none can
    /// (offline, shutdown): a warm-up would only race it or waste a request.
    No,
}

/// What the conversation logic needs from playback.
pub trait Speaker {
    /// Start speaking `turn_id`; returns the playback generation, or `None`
    /// when speech is disabled.
    fn start(&mut self, turn_id: &str) -> Option<u64>;
    /// Stop now. When this returns the player has been killed.
    fn stop(&mut self, rewarm: Rewarm) -> impl Future<Output = ()>;
}

/// Per-session playback. At most one turn plays at a time.
pub struct Playback {
    media: Arc<Media>,
    session: String,
    events: mpsc::Sender<PlaybackEvent>,
    current: Option<JoinHandle<()>>,
    generation: u64,
}

impl Playback {
    pub fn new(media: Arc<Media>, session: String, events: mpsc::Sender<PlaybackEvent>) -> Self {
        Self {
            media,
            session,
            events,
            current: None,
            generation: 0,
        }
    }
}

impl Speaker for Playback {
    fn start(&mut self, turn_id: &str) -> Option<u64> {
        if let Some(task) = self.current.take() {
            task.abort();
        }
        if !self.media.enabled {
            return None;
        }
        self.generation += 1;
        let generation = self.generation;
        let (media, session, events) = (
            self.media.clone(),
            self.session.clone(),
            self.events.clone(),
        );
        let turn_id = turn_id.to_owned();
        self.current = Some(tokio::spawn(async move {
            let mut played = false;
            let result = play(&media, &session, &turn_id, generation, &events, &mut played).await;
            if let Err(error) = result {
                tracing::warn!(%error, played, "speech playback failed");
            }
            // Never dropped: the session must learn that playback ended, or
            // it would show Speaking forever. Each generation sends at most
            // two events and the session drains them between RPCs.
            let _ = events
                .send(PlaybackEvent::Finished { generation, played })
                .await;
        }));
        Some(generation)
    }

    async fn stop(&mut self, rewarm: Rewarm) {
        let Some(task) = self.current.take() else {
            return;
        };
        let was_running = !task.is_finished();
        task.abort();
        // Dropping the task drops the child (`kill_on_drop`): SIGKILL, so
        // the ALSA buffer is not drained.
        let _ = task.await;
        if was_running && rewarm == Rewarm::Yes {
            // An abandoned HTTP/1.1 stream cannot be reused; warm a new one.
            let media = self.media.clone();
            tokio::spawn(async move { media.prewarm().await });
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        if let Some(task) = self.current.take() {
            task.abort();
        }
    }
}

/// A running player, its input pipe and a bounded reader of its stderr.
struct Player {
    child: Child,
    input: Option<ChildStdin>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    format: PcmFormat,
}

impl Player {
    fn spawn(media: &Media, format: PcmFormat) -> Result<Self> {
        let mut child = (media.make_player)(format)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("install ALSA aplay to enable robot speech")?;
        let input = child.stdin.take();
        let stderr = child.stderr.take().map(|pipe| tokio::spawn(capture(pipe)));
        Ok(Self {
            child,
            input,
            stderr,
            format,
        })
    }

    /// Kill and reap the player, then log what it wrote to stderr.
    async fn fail(mut self) {
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(1), self.child.wait()).await;
        let Some(reader) = self.stderr.take() else {
            return;
        };
        let Ok(Ok(bytes)) = timeout(Duration::from_millis(200), reader).await else {
            return;
        };
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim().replace(['\n', '\r'], " | ");
        if !text.is_empty() {
            tracing::warn!(stderr = %text, "audio player output");
        }
    }
}

/// Keep the first `STDERR_CAP` bytes and keep draining, so a chatty player
/// never blocks on a full stderr pipe.
async fn capture(mut pipe: ChildStderr) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 512];
    while let Ok(read @ 1..) = pipe.read(&mut buffer).await {
        let room = STDERR_CAP - kept.len();
        kept.extend_from_slice(&buffer[..read.min(room)]);
    }
    kept
}

async fn play(
    media: &Media,
    session: &str,
    turn_id: &str,
    generation: u64,
    events: &mpsc::Sender<PlaybackEvent>,
    audible: &mut bool,
) -> Result<()> {
    let mut report = Report {
        generation,
        events,
        audible,
        started: Instant::now(),
        headers_ms: None,
    };
    let mut player = media
        .prespawn
        .then(|| Player::spawn(media, EXPECTED_FORMAT))
        .transpose()?;
    let streamed = stream(media, session, turn_id, &mut report, &mut player).await;
    if streamed.is_err()
        && let Some(player) = player
    {
        player.fail().await;
    }
    streamed
}

/// Where a turn reports its first audio.
struct Report<'a> {
    generation: u64,
    events: &'a mpsc::Sender<PlaybackEvent>,
    audible: &'a mut bool,
    started: Instant,
    headers_ms: Option<u128>,
}

impl Report<'_> {
    async fn heard(&mut self) {
        *self.audible = true;
        let _ = self
            .events
            .send(PlaybackEvent::Started {
                generation: self.generation,
            })
            .await;
        tracing::debug!(
            headers_ms = self.headers_ms,
            first_pcm_ms = self.started.elapsed().as_millis(),
            "speech first audio"
        );
    }
}

/// Request the turn's audio and play it. Until the headers arrive this is one
/// pending request: dropping the playback task (a stop) cancels it there too.
async fn stream(
    media: &Media,
    session: &str,
    turn_id: &str,
    report: &mut Report<'_>,
    player: &mut Option<Player>,
) -> Result<()> {
    let response = media
        .http
        .post(format!("{}/v1/speech", media.server))
        .bearer_auth(session)
        .json(&SpeechRequest {
            turn_id: turn_id.to_owned(),
            stream: true,
        })
        .send()
        .await?
        .error_for_status()?;
    report.headers_ms = Some(report.started.elapsed().as_millis());
    let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok());
    ensure!(
        header("x-audio-turn-id") == Some(turn_id),
        "audio turn mismatch"
    );
    ensure!(
        header("x-audio-encoding") == Some("pcm_s16le"),
        "unsupported encoding"
    );
    let format = PcmFormat::parse(header("content-type").context("audio format required")?)?;
    if player.as_ref().is_none_or(|p| p.format != format) {
        // Dropping a mismatched early player kills it (`kill_on_drop`).
        *player = Some(Player::spawn(media, format)?);
    }
    let Some(player) = player.as_mut() else {
        bail!("audio player missing");
    };
    pump(media, player, response.bytes_stream(), report).await
}

/// Pipe `body` into `player`, then let it play out. `Started` goes out as soon
/// as the player's pipe takes the first bytes, not after the whole first chunk:
/// a backpressured player is already speaking while the rest waits.
async fn pump<B, E>(
    media: &Media,
    player: &mut Player,
    mut body: impl futures_util::Stream<Item = Result<B, E>> + Unpin,
    report: &mut Report<'_>,
) -> Result<()>
where
    B: AsRef<[u8]>,
    E: std::error::Error + Send + Sync + 'static,
{
    let Player {
        child,
        input,
        format,
        ..
    } = player;
    let mut input = input.take().context("audio player input")?;
    let mut total = 0usize;
    loop {
        let chunk = tokio::select! {
            biased;
            status = child.wait() => bail!("audio player exited early ({:?})", status?.code()),
            next = timeout(media.chunk_idle, body.next()) => match next {
                Err(_) => bail!("speech stream stalled for {:?}", media.chunk_idle),
                Ok(Some(chunk)) => chunk?,
                Ok(None) => break,
            },
        };
        let mut bytes = chunk.as_ref();
        total += bytes.len();
        ensure!(total <= AUDIO_BUDGET, "audio budget exceeded");
        if !*report.audible && !bytes.is_empty() {
            let written = write_some(&mut input, bytes, media.write_deadline).await?;
            bytes = &bytes[written..];
            report.heard().await;
        }
        feed(&mut input, bytes, media.write_deadline).await?;
    }
    let frame = usize::from(format.channels) * 2;
    ensure!(
        total > 0 && total.is_multiple_of(frame),
        "incomplete audio stream"
    );
    within(media.write_deadline, "audio player", input.shutdown()).await?;
    drop(input);
    let status = timeout(DRAIN_DEADLINE, child.wait())
        .await
        .map_err(|_| anyhow!("audio player did not finish"))??;
    ensure!(status.success(), "audio player failed");
    Ok(())
}

/// One write of at most [`WRITE_SLICE`] bytes; returns how many the player took.
async fn write_some(input: &mut ChildStdin, bytes: &[u8], deadline: Duration) -> Result<usize> {
    let slice = &bytes[..bytes.len().min(WRITE_SLICE)];
    let written = within(deadline, "audio player", input.write(slice)).await?;
    ensure!(written > 0, "audio player closed its input");
    Ok(written)
}

/// Write `bytes` in bounded slices. The deadline restarts whenever the player
/// takes any bytes, so only a player that stops reading is killed.
async fn feed(input: &mut ChildStdin, mut bytes: &[u8], deadline: Duration) -> Result<()> {
    while !bytes.is_empty() {
        let written = write_some(input, bytes, deadline).await?;
        bytes = &bytes[written..];
    }
    Ok(())
}

async fn within<T>(
    deadline: Duration,
    what: &str,
    work: impl Future<Output = std::io::Result<T>>,
) -> Result<T> {
    match timeout(deadline, work).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(anyhow!("{what} stopped reading for {deadline:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, body::Body, http::HeaderMap, routing::post, serve::ListenerExt};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const PCM_HEADERS: [(&str, &str); 3] = [
        ("content-type", "audio/pcm;rate=24000;channels=1"),
        ("x-audio-encoding", "pcm_s16le"),
        ("x-audio-turn-id", "test-turn"),
    ];

    /// Liveness guard for waits that must end. Every deadline under test is far
    /// shorter, and every outcome the code must avoid is far longer (30 s idle,
    /// a 30 s player, a 60 s stall), so the guard separates them without timing.
    const GUARD: Duration = Duration::from_secs(10);

    /// Like `crates/buddy/tests/support::timing_bound`: always reported, enforced
    /// only with `BUDDY_STRICT_TIMING=1` on an idle machine.
    fn timing_bound(name: &str, took: Duration, limit: Duration) {
        let ok = took < limit;
        eprintln!(
            "TIMING {name}_ms={} limit_ms={} {}",
            took.as_millis(),
            limit.as_millis(),
            if ok { "ok" } else { "OVER" }
        );
        if std::env::var("BUDDY_STRICT_TIMING").as_deref() == Ok("1") {
            assert!(ok, "{name}: {took:?} >= {limit:?}");
        }
    }

    /// Serve `app` and count accepted TCP connections.
    async fn serve(app: Router) -> (String, Arc<AtomicUsize>) {
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let listener = listener.tap_io(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{address}"), accepted)
    }

    fn player(program: &'static str, args: &'static [&'static str]) -> PlayerFactory {
        Arc::new(move |_| {
            let mut command = tokio::process::Command::new(program);
            command.args(args).stdout(Stdio::null());
            command
        })
    }

    /// What a tapped player wrote to its stdout, as it happens.
    enum Tap {
        Bytes(usize),
        Exited,
    }

    /// One `program` whose stdout is a pipe the test reads: the running byte
    /// count, then `Exited` when the pipe closes (the player died or finished).
    fn tapped(
        program: &'static str,
        args: &'static [&'static str],
    ) -> (PlayerFactory, std::sync::mpsc::Receiver<Tap>) {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let writer = Arc::new(std::sync::Mutex::new(Some(writer)));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut total = 0;
            let mut buffer = [0u8; 8192];
            while let Ok(read @ 1..) = std::io::Read::read(&mut reader, &mut buffer) {
                total += read;
                let _ = tx.send(Tap::Bytes(total));
            }
            let _ = tx.send(Tap::Exited);
        });
        let factory: PlayerFactory = Arc::new(move |_| {
            let mut command = tokio::process::Command::new(program);
            let out = writer.lock().unwrap().take().expect("one player");
            command.args(args).stdout(out);
            command
        });
        (factory, rx)
    }

    /// Wait (off the runtime) until the tap shows `bytes` or the player exited.
    async fn tap_until(rx: &mut std::sync::mpsc::Receiver<Tap>, bytes: Option<usize>) -> bool {
        let taken = std::mem::replace(rx, std::sync::mpsc::channel().1);
        let (back, found) = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + GUARD;
            let found = loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match taken.recv_timeout(left) {
                    Ok(Tap::Bytes(n)) if bytes.is_some_and(|want| n >= want) => break true,
                    Ok(Tap::Bytes(_)) => {}
                    Ok(Tap::Exited) => break bytes.is_none(),
                    Err(_) => break false,
                }
            };
            (taken, found)
        })
        .await
        .unwrap();
        *rx = back;
        found
    }

    async fn next_event(rx: &mut mpsc::Receiver<PlaybackEvent>) -> PlaybackEvent {
        timeout(GUARD, rx.recv())
            .await
            .expect("playback event")
            .expect("playback channel")
    }

    async fn finished(rx: &mut mpsc::Receiver<PlaybackEvent>) -> (u64, bool) {
        loop {
            if let PlaybackEvent::Finished { generation, played } = next_event(rx).await {
                return (generation, played);
            }
        }
    }

    #[test]
    fn player_arguments_include_buffer_and_device() {
        let get = |k: &str| match k {
            "BUDDY_ALSA_BUFFER_US" => Some("80000".to_owned()),
            "BUDDY_ALSA_DEVICE" => Some("plughw:CARD=sndrpihifiberry,DEV=0".to_owned()),
            _ => None,
        };
        let config = PlayerConfig::from_lookup(&get).unwrap();
        let args = config.args(PcmFormat {
            sample_rate: 24000,
            channels: 1,
        });
        assert!(args.contains(&"--buffer-time=80000".to_owned()));
        assert_eq!(
            args[args.len() - 2..],
            ["-D", "plughw:CARD=sndrpihifiberry,DEV=0"]
        );
        assert_eq!(
            PlayerConfig::from_lookup(&|_: &str| None).unwrap(),
            PlayerConfig::default()
        );
        let bad = |k: &str| (k == "BUDDY_ALSA_DEVICE").then(|| "--help".to_owned());
        assert!(PlayerConfig::from_lookup(&bad).is_err());
        let tiny = |k: &str| (k == "BUDDY_ALSA_BUFFER_US").then(|| "100".to_owned());
        assert!(PlayerConfig::from_lookup(&tiny).is_err());
    }

    #[tokio::test]
    async fn plays_incrementally_before_http_completion_and_stop_kills_the_player() {
        let finished = Arc::new(AtomicBool::new(false));
        let mark = finished.clone();
        let app = Router::new().route(
            "/v1/speech",
            post(move |headers: HeaderMap, Json(body): Json<SpeechRequest>| {
                let mark = mark.clone();
                async move {
                    assert!(body.stream);
                    assert_eq!(headers["authorization"], "Bearer test-session");
                    let stream = futures_util::stream::unfold(0, move |index| {
                        let mark = mark.clone();
                        async move {
                            if index == 0 {
                                return Some((Ok::<_, std::io::Error>(vec![0u8; 4096]), 1));
                            }
                            tokio::time::sleep(Duration::from_mins(1)).await;
                            mark.store(true, Ordering::SeqCst);
                            None
                        }
                    });
                    (PCM_HEADERS, Body::from_stream(stream))
                }
            }),
        );
        let (server, _) = serve(app).await;
        let (factory, mut tap) = tapped("cat", &[]);
        let media = Arc::new(Media::for_test(&server, factory, WRITE_DEADLINE));
        let (tx, mut rx) = mpsc::channel(8);
        let mut playback = Playback::new(media, "test-session".into(), tx);
        let generation = playback.start("test-turn").unwrap();
        assert_eq!(
            next_event(&mut rx).await,
            PlaybackEvent::Started { generation }
        );
        // The player has the first chunk while the response is still open.
        assert!(tap_until(&mut tap, Some(4096)).await);
        assert!(!finished.load(Ordering::SeqCst));
        let stopping = Instant::now();
        playback.stop(Rewarm::Yes).await;
        timing_bound(
            "stop_returns",
            stopping.elapsed(),
            Duration::from_millis(200),
        );
        // `cat` would hold its pipe open until the stream ends (60 s): a closed
        // pipe means the stop killed it.
        assert!(tap_until(&mut tap, None).await, "player still running");
        assert!(!finished.load(Ordering::SeqCst));
    }

    /// Regression: `Started` waited for the whole first HTTP chunk to drain into
    /// the player, so a backpressured `aplay` could be speaking while the face
    /// still showed Thinking. It now follows the first write the pipe accepts.
    #[tokio::test]
    async fn started_follows_the_first_pipe_write_not_the_whole_chunk() {
        // `sleep` never reads: only the pipe buffer takes bytes, far less than 1 MiB.
        let media = Media::for_test(
            "http://127.0.0.1:9",
            player("sleep", &["30"]),
            Duration::from_millis(300),
        );
        let mut player = Player::spawn(&media, EXPECTED_FORMAT).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let mut audible = false;
        let mut report = Report {
            generation: 7,
            events: &tx,
            audible: &mut audible,
            started: Instant::now(),
            headers_ms: None,
        };
        let body = futures_util::stream::iter([Ok::<_, std::io::Error>(vec![0u8; 1 << 20])])
            .chain(futures_util::stream::pending());
        let pumped = timeout(GUARD, pump(&media, &mut player, body, &mut report))
            .await
            .expect("the write deadline ends the pump");
        let error = pumped.unwrap_err().to_string();
        assert!(error.contains("stopped reading"), "{error}");
        assert!(audible);
        assert_eq!(
            rx.try_recv().unwrap(),
            PlaybackEvent::Started { generation: 7 }
        );
        player.fail().await;
    }

    /// The controller arms a first-audio deadline from `speech.ready` and stops a
    /// silent attempt before retrying. A stop while the request still waits for
    /// headers must drop the request (the server sees its connection close) and
    /// kill the early player, and report nothing for the abandoned generation.
    #[tokio::test]
    async fn a_stop_before_the_headers_cancels_the_request_and_the_player() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let (seen_tx, seen) = tokio::sync::oneshot::channel();
        let (closed_tx, closed) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let mut seen_tx = Some(seen_tx);
            // Never answer: read the request, then wait for the client to go.
            loop {
                match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n")
                            && let Some(tx) = seen_tx.take()
                        {
                            let _ = tx.send(());
                        }
                    }
                }
            }
            let _ = closed_tx.send(());
        });
        let (factory, mut tap) = tapped("sleep", &["30"]);
        let media = Arc::new(Media::for_test(&server, factory, WRITE_DEADLINE));
        let (tx, mut rx) = mpsc::channel(8);
        let mut playback = Playback::new(media, "s".into(), tx);
        playback.start("test-turn").unwrap();
        timeout(GUARD, seen).await.unwrap().unwrap();
        playback.stop(Rewarm::No).await;
        timeout(GUARD, closed)
            .await
            .expect("the request connection is closed")
            .unwrap();
        assert!(
            tap_until(&mut tap, None).await,
            "early player still running"
        );
        assert!(rx.try_recv().is_err(), "no event for a stopped generation");
    }

    #[tokio::test]
    async fn a_player_that_stops_reading_is_killed_at_the_deadline() {
        let app = Router::new().route(
            "/v1/speech",
            post(|| async {
                // 4 MiB: far more than a pipe buffer, so writes must block.
                let chunks = (0..64).map(|_| Ok::<_, std::io::Error>(vec![0u8; 65536]));
                (
                    PCM_HEADERS,
                    Body::from_stream(futures_util::stream::iter(chunks)),
                )
            }),
        );
        let (server, _) = serve(app).await;
        // `sleep` never reads stdin and would hold the turn for 30 s.
        let media = Media::for_test(
            &server,
            player("sleep", &["30"]),
            Duration::from_millis(300),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let mut playback = Playback::new(Arc::new(media), "s".into(), tx);
        let started = Instant::now();
        let generation = playback.start("test-turn").unwrap();
        // The pipe buffer took the first bytes, then the deadline ended the turn.
        assert_eq!(
            next_event(&mut rx).await,
            PlaybackEvent::Started { generation }
        );
        assert_eq!(finished(&mut rx).await, (generation, true));
        timing_bound(
            "stuck_player_released",
            started.elapsed(),
            Duration::from_secs(3),
        );
    }

    #[tokio::test]
    async fn early_exit_of_the_player_ends_playback() {
        let completed = Arc::new(AtomicBool::new(false));
        let mark = completed.clone();
        let app = Router::new().route(
            "/v1/speech",
            post(move || {
                let mark = mark.clone();
                async move {
                    // 30 s of audio in 50 ms chunks.
                    let stream = futures_util::stream::unfold(0, move |i| {
                        let mark = mark.clone();
                        async move {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            if i == 600 {
                                mark.store(true, Ordering::SeqCst);
                            }
                            (i < 600).then(|| (Ok::<_, std::io::Error>(vec![0u8; 256]), i + 1))
                        }
                    });
                    (PCM_HEADERS, Body::from_stream(stream))
                }
            }),
        );
        let (server, _) = serve(app).await;
        let media = Media::for_test(&server, player("true", &[]), WRITE_DEADLINE);
        let (tx, mut rx) = mpsc::channel(8);
        let mut playback = Playback::new(Arc::new(media), "s".into(), tx);
        let started = Instant::now();
        playback.start("test-turn").unwrap();
        finished(&mut rx).await;
        assert!(!completed.load(Ordering::SeqCst), "waited for the stream");
        timing_bound(
            "player_exit_ends_turn",
            started.elapsed(),
            Duration::from_secs(2),
        );
    }

    /// Latency trace: headers arrive after 300 ms, then audio streams. No audio
    /// can reach the player before the headers, and the second turn reuses the
    /// pooled connection. The upper bound is reported, enforced only when strict.
    #[tokio::test]
    async fn first_audio_tracks_server_latency_and_reuses_the_connection() {
        let app = Router::new().route(
            "/v1/speech",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let stream = futures_util::stream::unfold(0, |i| async move {
                    if i > 0 {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    (i < 5).then(|| (Ok::<_, std::io::Error>(vec![0u8; 4800]), i + 1))
                });
                (PCM_HEADERS, Body::from_stream(stream))
            }),
        );
        let (server, accepted) = serve(app).await;
        let media = Arc::new(Media::for_test(&server, player("cat", &[]), WRITE_DEADLINE));
        let (tx, mut rx) = mpsc::channel(8);
        let mut playback = Playback::new(media, "s".into(), tx);
        for turn in 1..=2 {
            let started = Instant::now();
            let generation = playback.start("test-turn").unwrap();
            assert_eq!(
                next_event(&mut rx).await,
                PlaybackEvent::Started { generation }
            );
            let first_audio = started.elapsed();
            assert!(first_audio >= Duration::from_millis(300), "{first_audio:?}");
            timing_bound(
                &format!("first_pcm_turn{turn}"),
                first_audio,
                Duration::from_millis(450),
            );
            assert_eq!(finished(&mut rx).await, (generation, true));
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "one pooled connection");
    }

    /// Shutdown path in `main`: the session future (and its `Playback`) is
    /// dropped, then the runtime shuts down. The player must be dead by the
    /// time `shutdown_timeout` returns, so no `aplay` outlives the robot.
    #[test]
    fn runtime_shutdown_kills_an_abandoned_player() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        // `sleep` stands in for aplay draining its buffer: it ignores stdin
        // closing and holds its stdout, a pipe we read, until it dies. EOF on
        // that pipe means the player has exited.
        let (mut player_out, player_in) = std::io::pipe().unwrap();
        let player_in = Arc::new(std::sync::Mutex::new(Some(player_in)));
        let take = player_in.clone();
        let factory: PlayerFactory = Arc::new(move |_| {
            let mut command = tokio::process::Command::new("sleep");
            let writer = take.lock().unwrap().take().expect("one player");
            command.arg("30").stdout(writer);
            command
        });
        runtime.block_on(async {
            let app = Router::new().route(
                "/v1/speech",
                post(|| async {
                    let stream = futures_util::stream::unfold(0, |i| async move {
                        if i > 0 {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        Some((Ok::<_, std::io::Error>(vec![0u8; 4800]), i + 1))
                    });
                    (PCM_HEADERS, Body::from_stream(stream))
                }),
            );
            let (server, _) = serve(app).await;
            let media = Arc::new(Media::for_test(&server, factory, WRITE_DEADLINE));
            let (tx, mut rx) = mpsc::channel(8);
            let mut playback = Playback::new(media, "s".into(), tx);
            let generation = playback.start("test-turn").unwrap();
            assert_eq!(
                next_event(&mut rx).await,
                PlaybackEvent::Started { generation }
            );
            drop(playback);
        });
        runtime.shutdown_timeout(Duration::from_millis(100));
        let (done, exited) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = std::io::Read::read_to_end(&mut player_out, &mut Vec::new());
            let _ = done.send(());
        });
        exited
            .recv_timeout(Duration::from_secs(2))
            .expect("player still running after runtime shutdown");
    }

    /// Regression: one 2 s deadline covered a whole chunk, so a chunk larger
    /// than 2 s of audio (96 KiB at 24 kHz) killed a player that was reading
    /// at its normal pace. The deadline now restarts on every bit of progress.
    #[tokio::test]
    async fn a_slow_steady_player_takes_a_large_chunk_past_the_deadline() {
        let media = Media::for_test(
            "http://127.0.0.1:9",
            // Reads about 4 KiB every 20 ms, then exits 0 at end of input.
            player(
                "sh",
                &[
                    "-c",
                    "while [ \"$(dd bs=4096 count=1 2>/dev/null | wc -c)\" -gt 0 ]; do sleep 0.02; done",
                ],
            ),
            Duration::from_millis(300),
        );
        let mut player = Player::spawn(&media, EXPECTED_FORMAT).unwrap();
        let mut input = player.input.take().unwrap();
        let chunk = vec![0u8; 256 * 1024];
        let started = Instant::now();
        feed(&mut input, &chunk, media.write_deadline)
            .await
            .expect("a reading player is never killed");
        let elapsed = started.elapsed();
        assert!(
            elapsed > Duration::from_millis(600),
            "not throttled: {elapsed:?}"
        );
        drop(input);
        let status = timeout(Duration::from_secs(10), player.child.wait()).await;
        assert!(status.unwrap().unwrap().success());
    }

    /// Regression: a stalled body left Pip on Speaking for the whole 320 s
    /// request timeout. With a 300 ms idle deadline the turn ends long before
    /// the 60 s stall (the event wait's guard would fail first otherwise).
    #[tokio::test]
    async fn a_stalled_speech_body_ends_playback_at_the_idle_deadline() {
        let app = Router::new().route(
            "/v1/speech",
            post(|| async {
                let stream = futures_util::stream::unfold(0, |i| async move {
                    if i > 0 {
                        tokio::time::sleep(Duration::from_mins(1)).await;
                    }
                    Some((Ok::<_, std::io::Error>(vec![0u8; 4800]), i + 1))
                });
                (PCM_HEADERS, Body::from_stream(stream))
            }),
        );
        let (server, _) = serve(app).await;
        let mut media = Media::for_test(&server, player("cat", &[]), WRITE_DEADLINE);
        media.chunk_idle = Duration::from_millis(300);
        let (tx, mut rx) = mpsc::channel(8);
        let mut playback = Playback::new(Arc::new(media), "s".into(), tx);
        let started = Instant::now();
        let generation = playback.start("test-turn").unwrap();
        assert_eq!(
            next_event(&mut rx).await,
            PlaybackEvent::Started { generation }
        );
        assert_eq!(finished(&mut rx).await, (generation, true));
        timing_bound(
            "stalled_body_released",
            started.elapsed(),
            Duration::from_secs(2),
        );
    }

    #[tokio::test]
    async fn player_stderr_is_kept_bounded_and_drained() {
        // 100 kB is more than a pipe holds: an undrained stderr would block it.
        let media = Media::for_test(
            "http://127.0.0.1:9",
            player("sh", &["-c", "yes 'audio open error' | head -c 100000 >&2"]),
            WRITE_DEADLINE,
        );
        let mut player = Player::spawn(&media, EXPECTED_FORMAT).unwrap();
        let status = timeout(Duration::from_secs(5), player.child.wait()).await;
        assert!(status.expect("player blocked on stderr").unwrap().success());
        let kept = player.stderr.take().unwrap().await.unwrap();
        assert_eq!(kept.len(), STDERR_CAP);
        assert!(kept.starts_with(b"audio open error\n"));
    }

    /// The player's own start-up (fork, exec, ALSA open) runs while the request
    /// waits for audio instead of after the headers. Proven by order, not by
    /// time: the fake server answers only once the player is running, and with
    /// prespawn off the player provably is not running when the headers go out.
    #[tokio::test]
    async fn the_player_starts_while_the_request_waits_for_audio() {
        for prespawn in [true, false] {
            let dir = std::env::temp_dir().join(format!("buddy-start-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let (marker, out) = (dir.join("running"), dir.join("out.pcm"));
            let running_at_headers = Arc::new(std::sync::Mutex::new(None));
            let (seen, probe) = (running_at_headers.clone(), marker.clone());
            let app = Router::new().route(
                "/v1/speech",
                post(move || {
                    let (seen, probe) = (seen.clone(), probe.clone());
                    async move {
                        let deadline = Instant::now() + GUARD;
                        // With prespawn, hold the headers until the player runs.
                        while prespawn && !probe.exists() && Instant::now() < deadline {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        *seen.lock().unwrap() = Some(probe.exists());
                        (PCM_HEADERS, vec![0u8; 4800])
                    }
                }),
            );
            let (server, _) = serve(app).await;
            let (touch, write) = (marker.clone(), out.clone());
            let factory: PlayerFactory = Arc::new(move |_| {
                let mut command = tokio::process::Command::new("sh");
                let script = format!(
                    "touch '{}'; exec cat > '{}'",
                    touch.display(),
                    write.display()
                );
                command.args(["-c", &script]);
                command
            });
            let mut media = Media::for_test(&server, factory, WRITE_DEADLINE);
            media.prespawn = prespawn;
            let (tx, mut rx) = mpsc::channel(8);
            let mut playback = Playback::new(Arc::new(media), "s".into(), tx);
            let generation = playback.start("test-turn").unwrap();
            assert_eq!(finished(&mut rx).await, (generation, true));
            assert_eq!(
                *running_at_headers.lock().unwrap(),
                Some(prespawn),
                "player running when the headers were sent (prespawn {prespawn})"
            );
            assert_eq!(std::fs::read(&out).unwrap().len(), 4800);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// A stop that a new speech request follows sends no warm-up beside it; a stop
    /// into idle warms one connection with `GET /health`.
    #[tokio::test]
    async fn only_an_idle_stop_warms_a_connection() {
        let health = Arc::new(AtomicUsize::new(0));
        let hits = health.clone();
        let app = Router::new()
            .route(
                "/v1/speech",
                post(|| async {
                    let stream = futures_util::stream::unfold(0, |i| async move {
                        if i > 0 {
                            tokio::time::sleep(Duration::from_mins(1)).await;
                        }
                        Some((Ok::<_, std::io::Error>(vec![0u8; 4800]), i + 1))
                    });
                    (PCM_HEADERS, Body::from_stream(stream))
                }),
            )
            .route(
                "/health",
                axum::routing::get(move || {
                    hits.fetch_add(1, Ordering::SeqCst);
                    async { "ok" }
                }),
            );
        let (server, _) = serve(app).await;
        let media = Arc::new(Media::for_test(&server, player("cat", &[]), WRITE_DEADLINE));
        let (tx, mut rx) = mpsc::channel(8);
        let mut playback = Playback::new(media, "s".into(), tx);
        for rewarm in [Rewarm::No, Rewarm::Yes] {
            let generation = playback.start("test-turn").unwrap();
            assert_eq!(
                next_event(&mut rx).await,
                PlaybackEvent::Started { generation }
            );
            playback.stop(rewarm).await;
        }
        // The idle stop's warm-up arrives; a warm-up from the first stop would
        // have been sent earlier still, and shows up as a second hit.
        let deadline = Instant::now() + GUARD;
        while health.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "no warm-up after an idle stop");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(health.load(Ordering::SeqCst), 1);
    }
}
