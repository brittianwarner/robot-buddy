//! Server secrets read from the environment.
use anyhow::{Context, Result, ensure};

/// The environment variable holding the HMAC key for application sessions.
pub const SIGNING_KEY_VAR: &str = "BUDDY_SIGNING_KEY";
/// The shortest accepted session signing key.
pub const MIN_SIGNING_KEY_BYTES: usize = 32;

/// Loads the session signing key. It must hold at least 32 bytes.
pub fn signing_key() -> Result<Vec<u8>> {
    parse_signing_key(std::env::var(SIGNING_KEY_VAR).ok())
}

fn parse_signing_key(value: Option<String>) -> Result<Vec<u8>> {
    let key = value
        .with_context(|| format!("{SIGNING_KEY_VAR} required"))?
        .into_bytes();
    ensure!(
        key.len() >= MIN_SIGNING_KEY_BYTES,
        "{SIGNING_KEY_VAR} must be at least {MIN_SIGNING_KEY_BYTES} bytes"
    );
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_key_requires_32_bytes() {
        assert!(parse_signing_key(None).is_err());
        assert!(parse_signing_key(Some("k".repeat(31))).is_err());
        assert_eq!(parse_signing_key(Some("k".repeat(32))).unwrap().len(), 32);
    }

    #[test]
    fn errors_never_contain_the_key() {
        let error = parse_signing_key(Some("secret-short".into())).unwrap_err();
        assert!(!format!("{error:#}").contains("secret-short"));
    }
}
