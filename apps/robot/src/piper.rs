//! Optional, device-local status speech. Only the fixed offline line crosses the
//! process boundary. Piper, its phonemiser and the voice are installed separately.
use crate::audio::PlayerConfig;
use anyhow::{Context, Result, ensure};
use robot_buddy::audio::PcmFormat;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const OFFLINE: &[u8] = b"I'm offline. I'll try again soon.\n";
const CONFIG_LIMIT: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct OfflineVoice {
    binary: PathBuf,
    model: PathBuf,
    player: PlayerConfig,
}

trait Processes {
    fn synth(&self) -> Command;
    fn player(&self, format: PcmFormat) -> Command;
}

impl Processes for OfflineVoice {
    fn synth(&self) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("--model")
            .arg(&self.model)
            .args(["--output-raw", "--length-scale", "1.18"]);
        cmd
    }

    fn player(&self, format: PcmFormat) -> Command {
        let mut cmd = Command::new("aplay");
        cmd.args(self.player.args(format));
        cmd
    }
}

impl OfflineVoice {
    pub fn from_lookup(
        get: &impl Fn(&str) -> Option<String>,
        player: PlayerConfig,
    ) -> Result<Option<Self>> {
        let Some(path) = get("BUDDY_PIPER_MODEL").filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let model = PathBuf::from(path);
        let binary =
            PathBuf::from(get("BUDDY_PIPER_BIN").unwrap_or_else(|| "/usr/local/bin/piper".into()));
        ensure!(
            model.is_absolute() && binary.is_absolute(),
            "Piper model and binary must be absolute paths"
        );
        Ok(Some(Self {
            binary,
            model,
            player,
        }))
    }

    /// Run once per offline transition. Dropping the future kills the active child.
    pub async fn announce(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let file = tokio::fs::File::open(self.model.with_added_extension("json"))
                .await
                .context("open Piper voice config")?;
            let mut json = Vec::new();
            file.take(CONFIG_LIMIT + 1).read_to_end(&mut json).await?;
            ensure!(
                json.len() as u64 <= CONFIG_LIMIT,
                "Piper voice config too large"
            );
            let config: serde_json::Value = serde_json::from_slice(&json)?;
            let rate = config
                .pointer("/audio/sample_rate")
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .context("missing Piper sample rate")?;
            ensure!(
                (8000..=48000).contains(&rate),
                "unsupported Piper sample rate"
            );
            render(self, rate).await
        })
        .await
        .context("offline voice deadline")?
    }
}

async fn render(proc: &impl Processes, rate: u32) -> Result<()> {
    let mut synth = proc
        .synth()
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start Piper")?;
    let mut input = synth.stdin.take().context("Piper stdin")?;
    input.write_all(OFFLINE).await?;
    drop(input);
    // Wait for a successful, whole utterance before opening ALSA. A bad model
    // must not emit half a warning or give an unbounded producer the speaker.
    let limit = u64::from(rate) * 2 * 8;
    let mut pcm = Vec::new();
    synth
        .stdout
        .take()
        .context("Piper stdout")?
        .take(limit + 1)
        .read_to_end(&mut pcm)
        .await?;
    ensure!(
        !pcm.is_empty() && pcm.len() as u64 <= limit && pcm.len().is_multiple_of(2),
        "invalid offline PCM"
    );
    ensure!(synth.wait().await?.success(), "Piper failed");
    colour(&mut pcm, rate);
    // Play 1.3× faster for the higher voice; the longer synthesis compensates
    // for most of the speed change. ALSA plughw handles device-rate conversion.
    let mut player = proc
        .player(PcmFormat {
            sample_rate: rate * 13 / 10,
            channels: 1,
        })
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start offline player")?;
    let mut input = player.stdin.take().context("player stdin")?;
    input.write_all(&pcm).await?;
    drop(input);
    ensure!(player.wait().await?.success(), "offline player failed");
    Ok(())
}

/// Two quiet, short reflections. Walk backwards so each tap uses the original
/// sample; scale by the sum of the weights so full-scale inputs never clip.
fn colour(pcm: &mut [u8], rate: u32) {
    let delay = (rate as usize * 45 / 10000) * 2;
    for i in (0..pcm.len()).step_by(2).rev() {
        let sample = |j| i32::from(i16::from_le_bytes([pcm[j], pcm[j + 1]]));
        let mix = 80 * sample(i)
            + 14 * i.checked_sub(delay).map_or(0, sample)
            + 7 * i.checked_sub(delay * 2).map_or(0, sample);
        let value = i16::try_from(mix / 101).unwrap_or_default();
        pcm[i..i + 2].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock(&'static str);
    impl Processes for Mock {
        fn synth(&self) -> Command {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", self.0]);
            cmd
        }
        fn player(&self, format: PcmFormat) -> Command {
            assert_eq!(
                format,
                PcmFormat {
                    sample_rate: 28665,
                    channels: 1
                }
            );
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "test \"$(wc -c | tr -d ' ')\" = 4"]);
            cmd
        }
    }

    #[tokio::test]
    async fn speaks_only_housekeeping_line_and_sends_pcm_to_player() {
        render(&Mock("IFS= read -r text && test \"$text\" = \"I'm offline. I'll try again soon.\" && printf '1234'"), 22050).await.unwrap();
    }

    #[tokio::test]
    async fn failed_partial_and_oversized_renders_never_start_playback() {
        // Any attempt to open the mock player with this rate panics.
        for script in [
            "printf 12; exit 1",
            "printf 1",
            "exit 0",
            "head -c 128002 /dev/zero",
        ] {
            assert!(render(&Mock(script), 8000).await.is_err());
        }
    }

    #[test]
    fn voice_colouring_is_feedforward_and_bounded() {
        let mut bytes = vec![0; 218];
        bytes[..2].copy_from_slice(&10100_i16.to_le_bytes());
        colour(&mut bytes, 8000);
        let impulse: Vec<_> = bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(
            [impulse[0], impulse[36], impulse[72], impulse[108]],
            [8000, 1400, 700, 0]
        );
    }
}
