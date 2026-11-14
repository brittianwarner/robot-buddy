//! Shared robot/browser wire contract: signed device session + canonical turn -> PCM.
use anyhow::{Result, bail, ensure};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
}
impl PcmFormat {
    pub fn parse(content_type: &str) -> Result<Self> {
        let mut parts = content_type.split(';').map(str::trim);
        ensure!(parts.next() == Some("audio/pcm"), "expected PCM audio");
        let mut rate = None;
        let mut channels = None;
        for part in parts {
            if let Some(v) = part.strip_prefix("rate=") {
                rate = Some(v.parse()?);
            }
            if let Some(v) = part.strip_prefix("channels=") {
                channels = Some(v.parse()?);
            }
        }
        let (Some(sample_rate), Some(channels)) = (rate, channels) else {
            bail!("PCM format metadata missing")
        };
        ensure!(
            (8000..=96000).contains(&sample_rate) && (1..=2).contains(&channels),
            "unsupported PCM format"
        );
        Ok(Self {
            sample_rate,
            channels,
        })
    }
    pub fn content_type(self) -> String {
        format!(
            "audio/pcm;rate={};channels={}",
            self.sample_rate, self.channels
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_explicit_supported_audio_format() {
        assert_eq!(
            PcmFormat::parse("audio/pcm;rate=44100;channels=1").unwrap(),
            PcmFormat {
                sample_rate: 44100,
                channels: 1
            }
        );
        for invalid in [
            "application/json",
            "audio/pcm",
            "audio/pcm;rate=1;channels=1",
            "audio/pcm;rate=44100;channels=99",
        ] {
            assert!(PcmFormat::parse(invalid).is_err());
        }
    }
}
