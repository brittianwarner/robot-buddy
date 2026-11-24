//! Local ALSA and whisper.cpp adapters. PCM stays in pipes; neither child inherits credentials.
use super::{
    Config,
    vad::{FRAME, RATE},
};
use anyhow::{Context, Result, ensure};
use std::{io, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdout, Command},
    time::timeout,
};

pub(super) trait Microphone {
    async fn frame(&mut self) -> Result<[i16; FRAME]>;
}

pub(super) trait Recognizer {
    async fn transcript(&self, pcm: &[i16]) -> Result<String>;
}

fn local_command(program: &std::ffi::OsStr) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear().env("LC_ALL", "C");
    for key in [
        "PATH",
        "HOME",
        "XDG_RUNTIME_DIR",
        "ALSA_CONFIG_PATH",
        "LD_LIBRARY_PATH",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.kill_on_drop(true)
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    cmd
}

pub(super) struct Alsa {
    _child: Child,
    pipe: ChildStdout,
    bytes: Vec<u8>,
    channel: usize,
    channels: usize,
}

impl Alsa {
    pub fn open(cfg: &Config) -> Result<Self> {
        let mut child = local_command("arecord".as_ref())
            .arg("-q")
            .arg("-D")
            .arg(&cfg.device)
            .args(["-t", "raw", "-f", "S16_LE", "-r", "16000", "-c"])
            .arg(cfg.channels.to_string())
            .stdin(Stdio::null())
            .spawn()
            .context("starting ALSA capture")?;
        Ok(Self {
            pipe: child.stdout.take().context("capture stdout")?,
            _child: child,
            bytes: vec![0; FRAME * 2 * cfg.channels],
            channel: cfg.channel,
            channels: cfg.channels,
        })
    }
}

impl Microphone for Alsa {
    async fn frame(&mut self) -> Result<[i16; FRAME]> {
        timeout(
            Duration::from_secs(4),
            self.pipe.read_exact(&mut self.bytes),
        )
        .await??;
        Ok(std::array::from_fn(|i| {
            let pos = 2 * (i * self.channels + self.channel);
            i16::from_le_bytes([self.bytes[pos], self.bytes[pos + 1]])
        }))
    }
}

/// RIFF with a known length: compatible with whisper-cli's nonseekable `-f -` input.
fn wav(samples: &[i16]) -> Result<Vec<u8>> {
    let bytes = u32::try_from(std::mem::size_of_val(samples))?;
    let mut out = Vec::with_capacity(bytes as usize + 44);
    out.extend(b"RIFF");
    out.extend((bytes + 36).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16_u32.to_le_bytes());
    out.extend(1_u16.to_le_bytes());
    out.extend(1_u16.to_le_bytes());
    out.extend(RATE.to_le_bytes());
    out.extend((RATE * 2).to_le_bytes());
    out.extend(2_u16.to_le_bytes());
    out.extend(16_u16.to_le_bytes());
    out.extend(b"data");
    out.extend(bytes.to_le_bytes());
    out.extend(samples.iter().flat_map(|v| v.to_le_bytes()));
    Ok(out)
}

impl Recognizer for Config {
    async fn transcript(&self, pcm: &[i16]) -> Result<String> {
        let mut child = local_command(&self.whisper)
            .arg("-m")
            .arg(&self.model)
            .args(["-l", "en", "-t", "2", "-np", "-nt", "-f", "-"])
            .stdin(Stdio::piped())
            .spawn()
            .context("starting whisper-cli")?;
        let mut input = child.stdin.take().context("recognizer stdin")?;
        let stdout = child.stdout.take().context("recognizer stdout")?;
        let bytes = wav(pcm)?;
        let ((), text, status) = timeout(Duration::from_secs(30), async {
            tokio::try_join!(
                async move { input.write_all(&bytes).await },
                async move {
                    let mut text = Vec::new();
                    stdout
                        .take(crate::input::MAX_LINE as u64 + 1)
                        .read_to_end(&mut text)
                        .await?;
                    if text.len() > crate::input::MAX_LINE {
                        return Err(io::Error::other("oversized recognizer result"));
                    }
                    Ok(text)
                },
                child.wait()
            )
        })
        .await
        .context("recognizer deadline")??;
        ensure!(status.success(), "recognizer exited unsuccessfully");
        // Joining segments only; newlines and JSON are never interpreted as control messages.
        Ok(std::str::from_utf8(&text)?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cli_drains_long_wav_stdin_and_returns_normalized_segments() {
        let config = Config {
            whisper: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/listen/tests/whisper-fixture.sh"
            )
            .into(),
            model: "test-model".into(),
            device: "default".into(),
            channel: 0,
            channels: 1,
            barge_in: true,
            wake: None,
        };
        let result = config.transcript(&vec![1200; 32_000]).await.unwrap();
        assert_eq!(result, "Hey Pip, what time is it?");
    }
    #[test]
    fn wav_has_exact_lengths_rate_and_little_endian_pcm() {
        let clip = wav(&[-1, 256, i16::MIN]).unwrap();
        assert_eq!(&clip[..12], b"RIFF\x2a\0\0\0WAVE");
        assert_eq!(&clip[24..28], &16_000_u32.to_le_bytes());
        assert_eq!(&clip[36..], b"data\x06\0\0\0\xff\xff\x00\x01\x00\x80");
    }
}
