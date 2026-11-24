//! Speech pipeline evals with fake actors and a fake provider. No engine or network,
//! except a loopback HTTP fake for the provider transport tests.
use super::{
    admission::Admission,
    pipeline::{
        self, AudioBody, Chunk, ChunkSource, Head, Job, Limits, Spoken, Synthesis, Synthesizer,
    },
    provider::{self, OpenRouter},
};
use anyhow::{Result, bail};
use axum::body::Bytes;
use futures_util::{StreamExt, stream};
use robot_buddy::{audio::PcmFormat, auth::Secret};
use serde_json::json;
use std::{
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, Semaphore},
    task::JoinHandle,
    time::Instant,
};

/// 24 kHz mono s16le.
const BYTES_PER_SECOND: usize = 48_000;
const FRAME: Duration = Duration::from_millis(20);
const FRAME_BYTES: usize = BYTES_PER_SECOND / 50;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

#[derive(Clone, Default)]
struct Script {
    /// Commit time of each chunk, from the request start.
    commits: Vec<Duration>,
    /// When the turn completes; later chunks read `done`.
    done_at: Duration,
    read_latency: Duration,
    revoked_at: Option<Duration>,
    check_hangs_at: Option<Duration>,
    /// Wake the pipeline on commit, like `speech.ready`.
    events: bool,
}

struct FakeSource {
    start: Instant,
    script: Script,
    wake: Arc<Notify>,
    notifier: Option<JoinHandle<()>>,
}

impl FakeSource {
    fn new(script: Script) -> Self {
        let start = Instant::now();
        let wake = Arc::new(Notify::new());
        let notifier = script.events.then(|| {
            let (commits, wake) = (script.commits.clone(), wake.clone());
            tokio::spawn(async move {
                for at in commits {
                    tokio::time::sleep_until(start + at).await;
                    wake.notify_one();
                }
            })
        });
        Self {
            start,
            script,
            wake,
            notifier,
        }
    }

    fn revoked(&self) -> bool {
        self.script
            .revoked_at
            .is_some_and(|at| self.start.elapsed() >= at)
    }
}

impl Drop for FakeSource {
    fn drop(&mut self) {
        if let Some(task) = self.notifier.take() {
            task.abort();
        }
    }
}

fn voice() -> serde_json::Value {
    json!({
        "voice_id": "will",
        "revision": 3,
        "config": robot_buddy::voice::VoiceConfig::default(),
    })
}

impl ChunkSource for FakeSource {
    async fn chunk(&self, index: u32) -> Result<Chunk> {
        tokio::time::sleep(self.script.read_latency).await;
        if self.revoked() {
            bail!("speech turn stopped");
        }
        let elapsed = self.start.elapsed();
        Ok(match self.script.commits.get(index as usize) {
            Some(at) if elapsed >= *at => Chunk::Ready(Spoken {
                text: format!("Sentence {index}."),
                voice: voice(),
            }),
            None if elapsed >= self.script.done_at => Chunk::Done,
            _ => Chunk::Pending,
        })
    }

    async fn check(&self) -> Result<()> {
        if let Some(at) = self.script.check_hangs_at
            && self.start.elapsed() >= at
        {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(self.script.read_latency).await;
        if self.revoked() {
            bail!("session revoked");
        }
        Ok(())
    }

    async fn changed(&self) {
        self.wake.notified().await;
    }
}

/// Counts requests from start until their audio is dropped.
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct FakeSynth {
    ttfb: Duration,
    sentence: Duration,
    started: AtomicUsize,
    active: Arc<AtomicUsize>,
    max_active: AtomicUsize,
}

impl FakeSynth {
    fn new(ttfb: Duration, sentence: Duration) -> Arc<Self> {
        Arc::new(Self {
            ttfb,
            sentence,
            ..Self::default()
        })
    }
    fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

impl Synthesizer for FakeSynth {
    /// TTFB, then PCM at exactly real-time rate in 20 ms frames.
    async fn synthesize(&self, _: &Spoken) -> Result<Synthesis> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(now, Ordering::SeqCst);
        let active = Active(self.active.clone());
        tokio::time::sleep(self.ttfb).await;
        let frames = u32::try_from(self.sentence.as_millis() / FRAME.as_millis()).unwrap();
        let first = Instant::now();
        let audio = stream::unfold((0, active), move |(frame, active)| async move {
            if frame == frames {
                return None;
            }
            tokio::time::sleep_until(first + FRAME * frame).await;
            Some((Ok(Bytes::from(vec![0; FRAME_BYTES])), (frame + 1, active)))
        });
        Ok(Synthesis {
            format: PcmFormat {
                sample_rate: 24_000,
                channels: 1,
            },
            audio: audio.boxed(),
        })
    }
}

async fn start(
    admission: &Admission,
    bot: &str,
    source: FakeSource,
    synth: &Arc<FakeSynth>,
    limits: Limits,
) -> Result<(Head, AudioBody)> {
    let lease = admission.lease(bot).await?;
    let (head, body) = pipeline::spawn(Job {
        source,
        synth: synth.clone(),
        upstream: Arc::new(Semaphore::new(8)),
        lease,
        limits,
    });
    Ok((head.await??, body))
}

/// What a listener playing in real time hears.
#[derive(Debug)]
struct Heard {
    first: Option<Duration>,
    last: Duration,
    max_gap: Duration,
    bytes: usize,
    error: Option<String>,
}

/// Plays bytes as they arrive: silence is any arrival after the queued audio ran out.
async fn listen(body: AudioBody, start: Instant) -> Heard {
    let mut audio = pin!(body.into_stream());
    let mut heard = Heard {
        first: None,
        last: Duration::ZERO,
        max_gap: Duration::ZERO,
        bytes: 0,
        error: None,
    };
    let mut play_end: Option<Instant> = None;
    while let Some(item) = audio.next().await {
        let bytes = match item {
            Ok(bytes) => bytes,
            Err(error) => {
                heard.error = Some(error.to_string());
                break;
            }
        };
        let now = Instant::now();
        let length = Duration::from_secs_f64(
            f64::from(u32::try_from(bytes.len()).unwrap())
                / f64::from(u32::try_from(BYTES_PER_SECOND).unwrap()),
        );
        let from = match play_end {
            Some(end) if now > end => {
                heard.max_gap = heard.max_gap.max(now - end);
                now
            }
            Some(end) => end,
            None => {
                heard.first = Some(now - start);
                now
            }
        };
        play_end = Some(from + length);
        heard.bytes += bytes.len();
        heard.last = now - start;
    }
    heard
}

async fn until(condition: impl Fn() -> bool) {
    while !condition() {
        tokio::time::sleep(ms(1)).await;
    }
}

fn five_sentences() -> Script {
    Script {
        commits: [400, 1000, 1600, 2200, 2800].map(ms).to_vec(),
        done_at: ms(3000),
        read_latency: ms(15),
        events: true,
        ..Script::default()
    }
}

/// Eval (a): real time, 300 ms TTFB, 1 s sentences streamed at real-time rate.
#[tokio::test]
async fn eval_gap_lookahead_vs_sequential() {
    let measure = |lookahead: bool| async move {
        let admission = Admission::new(4);
        let synth = FakeSynth::new(ms(300), ms(1000));
        let limits = Limits {
            lookahead,
            ..Limits::default()
        };
        let begin = Instant::now();
        let source = FakeSource::new(five_sentences());
        let (_, body) = start(&admission, "bot", source, &synth, limits)
            .await
            .unwrap();
        let heard = listen(body, begin).await;
        (heard, synth.max_active.load(Ordering::SeqCst))
    };
    let ((ahead, ahead_requests), (sequential, sequential_requests)) =
        tokio::join!(measure(true), measure(false));
    // Sequential, analytically: each boundary waits for lookup + TTFB, less the last
    // 20 ms frame still playing.
    let analytic = (ms(15) + ms(300)).saturating_sub(FRAME);
    println!(
        "gap eval: lookahead max gap {:?}, sequential max gap {:?} (analytic ~{:?}), \
         first byte {:?} / {:?}",
        ahead.max_gap, sequential.max_gap, analytic, ahead.first, sequential.first
    );
    for heard in [&ahead, &sequential] {
        assert_eq!(heard.error, None);
        assert_eq!(heard.bytes, 5 * BYTES_PER_SECOND);
    }
    assert!(ahead.max_gap < ms(50), "lookahead gap {:?}", ahead.max_gap);
    assert!(
        sequential.max_gap > ms(200),
        "sequential {:?}",
        sequential.max_gap
    );
    assert!(ahead_requests <= 2 && sequential_requests <= 2);
}

/// Eval (b): chunk 0 commits 400 ms after the request; first byte follows the commit.
#[tokio::test]
async fn eval_first_byte_follows_commit() {
    let measure = |events: bool| async move {
        let admission = Admission::new(4);
        let synth = FakeSynth::new(ms(300), ms(200));
        let script = Script {
            commits: vec![ms(400)],
            done_at: ms(400),
            read_latency: ms(15),
            events,
            ..Script::default()
        };
        let begin = Instant::now();
        let source = FakeSource::new(script);
        let (_, body) = start(&admission, "bot", source, &synth, Limits::default())
            .await
            .unwrap();
        listen(body, begin).await.first.unwrap()
    };
    let (evented, polled) = tokio::join!(measure(true), measure(false));
    println!(
        "first byte: commit 400 ms + TTFB 300 ms -> evented {evented:?}, poll-only {polled:?}"
    );
    assert!(
        evented >= ms(700) && evented < ms(760),
        "evented {evented:?}"
    );
}

/// Eval (c): dropping the body releases the lease and cancels both syntheses.
#[tokio::test]
async fn eval_barge_in_releases_promptly() {
    let admission = Admission::new(4);
    let synth = FakeSynth::new(ms(100), ms(1000));
    let script = Script {
        commits: vec![ms(0); 3],
        read_latency: ms(5),
        events: true,
        ..Script::default()
    };
    let source = FakeSource::new(script);
    let (_, body) = start(&admission, "bot", source, &synth, Limits::default())
        .await
        .unwrap();
    let mut audio = Box::pin(body.into_stream());
    audio.next().await.unwrap().unwrap();
    until(|| synth.active() == 2).await;
    let dropped = Instant::now();
    drop(audio);
    until(|| admission.idle() && synth.active() == 0).await;
    let released = dropped.elapsed();
    println!("barge-in: lease and syntheses released {released:?} after body drop");
    assert!(released < ms(100), "released after {released:?}");
    assert_eq!(
        synth.started.load(Ordering::SeqCst),
        2,
        "one extra sentence at most"
    );
}

/// Eval (d): a second request for the same bot takes the only slot.
#[tokio::test(start_paused = true)]
async fn eval_newest_request_wins() {
    let admission = Admission::new(1);
    let synth = FakeSynth::new(ms(100), ms(1000));
    let script = Script {
        commits: vec![ms(0); 2],
        read_latency: ms(5),
        ..Script::default()
    };
    let begin = Instant::now();
    let source = FakeSource::new(script.clone());
    let (_, first) = start(&admission, "bot", source, &synth, Limits::default())
        .await
        .unwrap();
    let first = tokio::spawn(listen(first, begin));
    tokio::time::sleep(ms(500)).await;
    let replaced_at = begin.elapsed();
    let source = FakeSource::new(script);
    let (head, second) = start(&admission, "bot", source, &synth, Limits::default())
        .await
        .unwrap();
    assert_eq!(head.voice_id, "will");
    let first = first.await.unwrap();
    let second = listen(second, begin).await;
    println!(
        "latest wins: second request at {replaced_at:?}, first stream's last audio {:?}",
        first.last
    );
    assert!(first.error.unwrap().contains("replaced"));
    assert!(first.last <= replaced_at);
    assert_eq!(second.error, None);
    assert_eq!(second.bytes, 2 * BYTES_PER_SECOND);
    assert!(admission.idle());
}

#[tokio::test(start_paused = true)]
async fn capacity_stays_bounded_across_bots() {
    let admission = Admission::new(2);
    let a = admission.lease("a").await.unwrap();
    let _b = admission.lease("b").await.unwrap();
    assert!(admission.lease("c").await.is_err());
    drop(a);
    assert!(admission.lease("c").await.is_ok());
}

/// Eval (e): a failed check stops the stream within one check interval.
#[tokio::test(start_paused = true)]
async fn eval_revocation_stops_stream() {
    for (name, commits) in [("stream", vec![ms(0), ms(500)]), ("replay", vec![ms(0)])] {
        let admission = Admission::new(4);
        let synth = FakeSynth::new(ms(100), ms(3000));
        let script = Script {
            commits,
            read_latency: ms(10),
            revoked_at: Some(ms(1200)),
            ..Script::default()
        };
        let begin = Instant::now();
        let source = FakeSource::new(script);
        let (_, body) = start(&admission, "bot", source, &synth, Limits::default())
            .await
            .unwrap();
        let heard = listen(body, begin).await;
        println!(
            "revocation ({name}): revoked 1.2 s, last audio {:?}",
            heard.last
        );
        assert!(heard.error.is_some());
        assert!(
            heard.last < ms(1200) + ms(500) + ms(20),
            "{name}: {:?}",
            heard.last
        );
        until(|| admission.idle() && synth.active() == 0).await;
    }
}

/// A check that hangs pauses audio once `authorized_until` passes, then times out.
#[tokio::test(start_paused = true)]
async fn eval_slow_check_pauses_audio() {
    let admission = Admission::new(4);
    let synth = FakeSynth::new(ms(100), ms(5000));
    let script = Script {
        commits: vec![ms(0)],
        read_latency: ms(10),
        check_hangs_at: Some(ms(1000)),
        ..Script::default()
    };
    let begin = Instant::now();
    let source = FakeSource::new(script);
    let (_, body) = start(&admission, "bot", source, &synth, Limits::default())
        .await
        .unwrap();
    let heard = listen(body, begin).await;
    println!("slow check: last audio {:?}", heard.last);
    // The last good check was issued at 0.5 s and authorizes until 2.5 s.
    assert!(heard.last <= ms(2500), "{:?}", heard.last);
    assert!(heard.error.unwrap().contains("timed out"));
}

/// Eval (f): a listener that stops reading cannot hold the lease past the deadline.
#[tokio::test(start_paused = true)]
async fn eval_stalled_reader_released_at_deadline() {
    let admission = Admission::new(4);
    let synth = FakeSynth::new(ms(100), ms(30_000));
    let script = Script {
        commits: vec![ms(0)],
        read_latency: ms(10),
        ..Script::default()
    };
    let limits = Limits {
        deadline: Duration::from_secs(10),
        ..Limits::default()
    };
    let begin = Instant::now();
    let source = FakeSource::new(script);
    let (_, body) = start(&admission, "bot", source, &synth, limits)
        .await
        .unwrap();
    until(|| admission.idle()).await;
    let released = begin.elapsed();
    println!("stalled reader: lease released at {released:?} (deadline 10 s)");
    assert!(released >= Duration::from_secs(10) && released < ms(10_050));
    assert_eq!(synth.active(), 0);
    drop(body);
}

#[test]
fn base_url_policy() {
    assert_eq!(
        provider::base_url(None).unwrap(),
        provider::DEFAULT_BASE_URL
    );
    for ok in [
        "https://example.test/api/v1/",
        "http://127.0.0.1:9/v1",
        "http://localhost:9",
    ] {
        assert!(provider::base_url(Some(ok)).is_ok(), "{ok}");
    }
    for bad in [
        "http://example.test/v1",
        "ftp://localhost/v1",
        "https://user:pass@example.test/v1",
        "https://example.test/v1?key=1",
        "not a url",
    ] {
        assert!(provider::base_url(Some(bad)).is_err(), "{bad}");
    }
}

async fn fake_provider() -> String {
    use axum::{Router, body::Body, http::StatusCode, routing::post};
    let echo = || async {
        let body =
            stream::repeat_with(|| Ok::<_, std::io::Error>(Bytes::from_static(b"SECRET PROMPT ")));
        (StatusCode::BAD_REQUEST, Body::from_stream(body))
    };
    let audio = || async { ([("content-type", "audio/pcm")], vec![0u8; 4800]) };
    let router = Router::new()
        .route("/bad/audio/speech", post(echo))
        .route("/ok/audio/speech", post(audio));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

#[tokio::test]
async fn provider_error_body_is_bounded_and_not_echoed() {
    let base = fake_provider().await;
    let spoken = Spoken {
        text: "Hello.".into(),
        voice: voice(),
    };
    let model = robot_buddy::voice::SPEECH_MODEL.to_owned();
    let bad = OpenRouter::new(
        Secret::new("test-key".into()),
        model.clone(),
        &format!("{base}/bad"),
    )
    .unwrap();
    let begun = Instant::now();
    let error = match bad.synthesize(&spoken).await {
        Ok(_) => panic!("rejected request must fail"),
        Err(error) => format!("{error:#}"),
    };
    assert!(begun.elapsed() < Duration::from_secs(3));
    assert!(
        error.contains("HTTP 400") && !error.contains("SECRET"),
        "{error}"
    );

    let ok = OpenRouter::new(Secret::new("test-key".into()), model, &format!("{base}/ok")).unwrap();
    let synthesis = ok.synthesize(&spoken).await.unwrap();
    assert_eq!(synthesis.format.sample_rate, 24_000);
    let bytes: usize = synthesis
        .audio
        .map(|item| item.unwrap().len())
        .fold(0, |sum, n| async move { sum + n })
        .await;
    assert_eq!(bytes, 4800);

    // A sentence that is only model markup never reaches the provider (here it
    // would get HTTP 400) and yields no audio.
    let markup = Spoken {
        text: "[laughs]".into(),
        voice: voice(),
    };
    let silent = bad
        .synthesize(&markup)
        .await
        .expect("no request for markup");
    assert_eq!(synthesis_len(silent).await, 0);
}

async fn synthesis_len(synthesis: Synthesis) -> usize {
    synthesis
        .audio
        .map(|item| item.unwrap().len())
        .fold(0, |sum, n| async move { sum + n })
        .await
}

#[test]
fn response_keeps_audio_contract_headers() {
    let head = Head {
        format: PcmFormat {
            sample_rate: 24_000,
            channels: 1,
        },
        voice_id: "will".into(),
        revision: 3,
    };
    let response = super::response(&head, "turn-1", axum::body::Body::empty()).unwrap();
    let header = |name: &str| response.headers()[name].to_str().unwrap().to_owned();
    assert_eq!(header("content-type"), "audio/pcm;rate=24000;channels=1");
    assert_eq!(header("x-audio-encoding"), "pcm_s16le");
    assert_eq!(header("x-audio-turn-id"), "turn-1");
    assert_eq!(header("x-audio-voice-id"), "will");
    assert_eq!(header("x-audio-config-revision"), "3");
    assert_eq!(header("cache-control"), "no-store");
}
