//! Server secrets read from the environment.
use crate::auth::{MIN_KEY_BYTES, Secret};
use anyhow::{Context, Result, ensure};

/// The environment variable holding the HMAC key for sessions, challenges and cookies.
pub const SIGNING_KEY_VAR: &str = "BUDDY_SIGNING_KEY";

/// Loads the signing key every actor and the server share. It must hold at least
/// [`MIN_KEY_BYTES`] bytes. The one loader, so every caller applies the same check.
pub fn signing_key() -> Result<Secret<Vec<u8>>> {
    parse_signing_key(std::env::var(SIGNING_KEY_VAR).ok())
}

fn parse_signing_key(value: Option<String>) -> Result<Secret<Vec<u8>>> {
    let key = Secret::new(
        value
            .with_context(|| format!("{SIGNING_KEY_VAR} required"))?
            .into_bytes(),
    );
    ensure!(
        key.expose().len() >= MIN_KEY_BYTES,
        "{SIGNING_KEY_VAR} must be at least {MIN_KEY_BYTES} bytes"
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
        let key = parse_signing_key(Some("k".repeat(32))).unwrap();
        assert_eq!(key.expose().len(), 32);
        assert_eq!(format!("{key:?}"), "Secret([redacted])");
    }

    #[test]
    fn errors_never_contain_the_key() {
        let error = parse_signing_key(Some("secret-short".into())).unwrap_err();
        assert!(!format!("{error:#}").contains("secret-short"));
    }
}
