//! Named character profiles pair a complete prompt with a complete speech configuration.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaDraft {
    pub name: String,
    pub expected_version: i64,
    pub prompt: String,
    pub voice: crate::voice::VoiceConfig,
    pub reason: String,
}
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name == name.trim()
            && !name.chars().any(char::is_control),
        "persona name must be 1-64 bytes without surrounding whitespace or control characters"
    );
    Ok(())
}
impl PersonaDraft {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        ensure!(
            self.expected_version >= 0
                && !self.prompt.trim().is_empty()
                && self.prompt.len() <= 8192
                && !self.reason.trim().is_empty()
                && self.reason.len() <= 500,
            "invalid persona draft"
        );
        self.voice.validate()
    }
}
