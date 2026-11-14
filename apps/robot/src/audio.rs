//! Linux playback for the same authenticated PCM endpoint used by the browser lab.
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use robot_buddy::{audio::PcmFormat, protocol::SpeechRequest};
use std::{process::Stdio, time::Duration};
use tokio::io::AsyncWriteExt;
#[derive(Default)]
pub struct Playback {
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Playback {
    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
    pub fn start(&mut self, server: String, session: String, turn_id: String) {
        self.stop();
        if std::env::var("BUDDY_SPEECH").as_deref() != Ok("true") {
            return;
        }
        self.task = Some(tokio::spawn(async move {
            if play(&server, &session, turn_id).await.is_err() {
                tracing::warn!("speech playback failed");
            }
        }));
    }
}
impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}
async fn play(server: &str, session: &str, turn_id: String) -> Result<()> {
    play_using(server, session, turn_id, |format| {
        let mut player = tokio::process::Command::new("aplay");
        player
            .args([
                "-q",
                "-t",
                "raw",
                "-f",
                "S16_LE",
                "--buffer-time=200000",
                "-r",
                &format.sample_rate.to_string(),
                "-c",
                &format.channels.to_string(),
            ])
            .stdout(Stdio::null());
        player
    })
    .await
}
async fn play_using(
    server: &str,
    session: &str,
    turn_id: String,
    make_player: impl FnOnce(PcmFormat) -> tokio::process::Command,
) -> Result<()> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = http
        .post(format!("{}/v1/speech", server.trim_end_matches('/')))
        .bearer_auth(session)
        .json(&SpeechRequest {
            turn_id: turn_id.clone(),
            stream: true,
        })
        .send()
        .await?
        .error_for_status()?;
    ensure!(
        response
            .headers()
            .get("x-audio-turn-id")
            .and_then(|v| v.to_str().ok())
            == Some(turn_id.as_str()),
        "audio turn mismatch"
    );
    ensure!(
        response
            .headers()
            .get("x-audio-encoding")
            .and_then(|v| v.to_str().ok())
            == Some("pcm_s16le"),
        "unsupported encoding"
    );
    let format = PcmFormat::parse(
        response
            .headers()
            .get("content-type")
            .context("audio format required")?
            .to_str()?,
    )?;
    let mut player = make_player(format)
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("install ALSA aplay to enable robot speech")?;
    let mut input = player.stdin.take().context("audio player input")?;
    let mut stream = response.bytes_stream();
    let mut total = 0usize;
    while let Some(bytes) = stream.next().await {
        let bytes = bytes?;
        total += bytes.len();
        ensure!(total <= 16 * 1024 * 1024, "audio budget exceeded");
        input.write_all(&bytes).await?;
    }
    ensure!(
        total > 0 && total.is_multiple_of(usize::from(format.channels) * 2),
        "incomplete audio stream"
    );
    input.shutdown().await?;
    drop(input);
    ensure!(
        tokio::time::timeout(Duration::from_secs(10), player.wait())
            .await??
            .success(),
        "audio player failed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, body::Body, http::HeaderMap, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    #[tokio::test]
    async fn plays_incrementally_before_http_completion_and_abort_drops_stream() -> Result<()> {
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
                            match index {
                                0 => Some((Ok::<_, std::io::Error>(vec![0u8; 4096]), 1)),
                                1 => {
                                    tokio::time::sleep(Duration::from_secs(20)).await;
                                    mark.store(true, Ordering::SeqCst);
                                    None
                                }
                                _ => None,
                            }
                        }
                    });
                    (
                        [
                            ("content-type", "audio/pcm;rate=44100;channels=1"),
                            ("x-audio-encoding", "pcm_s16le"),
                            ("x-audio-turn-id", "test-turn"),
                        ],
                        Body::from_stream(stream),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let path = std::env::temp_dir().join(format!("buddy-audio-{}.pcm", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&path)?;
        let task = tokio::spawn(async move {
            play_using(
                &format!("http://{address}"),
                "test-session",
                "test-turn".into(),
                |_| {
                    let mut command = tokio::process::Command::new("cat");
                    command.stdout(Stdio::from(file));
                    command
                },
            )
            .await
        });
        for _ in 0..100 {
            if std::fs::metadata(&path)?.len() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(std::fs::metadata(&path)?.len(), 4096);
        assert!(!finished.load(Ordering::SeqCst));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        server.abort();
        std::fs::remove_file(path)?;
        Ok(())
    }
}
