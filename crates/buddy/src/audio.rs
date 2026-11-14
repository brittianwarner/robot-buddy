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
    /// Bare `audio/pcm` is OpenRouter's 16-bit little-endian mono 24 kHz default.
    /// Explicit rate or channel values still go through [`parse`](Self::parse).
    pub fn from_provider(content_type: &str) -> Result<Self> {
        let has_rate = content_type
            .split(';')
            .any(|part| part.trim().starts_with("rate="));
        let has_channels = content_type
            .split(';')
            .any(|part| part.trim().starts_with("channels="));
        if content_type.split(';').next().map(str::trim) == Some("audio/pcm")
            && !has_rate
            && !has_channels
        {
            return Ok(Self {
                sample_rate: 24_000,
                channels: 1,
            });
        }
        Self::parse(content_type)
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
        assert_eq!(
            PcmFormat::from_provider("audio/pcm").unwrap(),
            PcmFormat {
                sample_rate: 24000,
                channels: 1
            }
        );
        assert_eq!(
            PcmFormat::from_provider("audio/pcm;rate=44100;channels=1").unwrap(),
            PcmFormat {
                sample_rate: 44100,
                channels: 1
            }
        );
        assert!(PcmFormat::from_provider("audio/pcm;rate=1;channels=1").is_err());
    }
}
