//! Short-lived application assertions and manufacturing device identity.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

/// Minimum length of the HMAC key that signs sessions, challenges and cookies.
pub const MIN_KEY_BYTES: usize = 32;
/// Domain separation for [`ct_eq`]. Not a secret: it only fixes the HMAC used to compare.
const CT_EQ_KEY: &[u8] = b"robot-buddy/ct-eq/v1";

/// Secret material whose `Debug` output is redacted and whose bytes are wiped on drop.
///
/// There is deliberately no `Display`. Wiping is best effort: it clears the live buffer,
/// not earlier copies made by reallocation.
pub struct Secret<T: Wipe>(T);
/// Values that can overwrite their own bytes.
pub trait Wipe {
    fn wipe(&mut self);
}
impl Wipe for Vec<u8> {
    fn wipe(&mut self) {
        self.fill(0);
        std::hint::black_box(&self);
    }
}
impl Wipe for String {
    fn wipe(&mut self) {
        std::mem::take(self).into_bytes().wipe();
    }
}
impl<T: Wipe> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }
    /// Borrow the secret. Never log or format the result.
    pub fn expose(&self) -> &T {
        &self.0
    }
}
/// Lets `auth::sign(&secret, ..)` take a key without an explicit `expose()`.
impl AsRef<[u8]> for Secret<Vec<u8>> {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
impl<T: Wipe + Clone> Clone for Secret<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Wipe> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}
impl<T: Wipe> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.wipe();
    }
}
/// The identity file stores the device key as plain base64; serde is transparent.
impl Serialize for Secret<String> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}
impl<'de> Deserialize<'de> for Secret<String> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d).map(Self)
    }
}

/// Reads `BUDDY_SIGNING_KEY` and rejects keys shorter than [`MIN_KEY_BYTES`].
pub fn signing_key_from_env() -> Result<Secret<Vec<u8>>> {
    let key = Secret::new(
        std::env::var("BUDDY_SIGNING_KEY")
            .context("BUDDY_SIGNING_KEY required")?
            .into_bytes(),
    );
    ensure!(
        key.expose().len() >= MIN_KEY_BYTES,
        "BUDDY_SIGNING_KEY must be at least 32 bytes"
    );
    Ok(key)
}

/// Constant-time equality for CSRF tokens, OAuth state/nonce and similar secrets.
///
/// Both inputs are run through HMAC under a fixed key and the tags are compared with the
/// constant-time `verify_slice`, so neither content nor length leaks through timing.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let Ok(mut left) = Hmac::<Sha256>::new_from_slice(CT_EQ_KEY) else {
        return false;
    };
    let mut right = left.clone();
    left.update(a);
    right.update(b);
    right.verify_slice(&left.finalize().into_bytes()).is_ok()
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
pub fn random() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn digest(s: &str) -> String {
    B64.encode(Sha256::digest(s.as_bytes()))
}
pub fn sign<T: Serialize>(key: impl AsRef<[u8]>, purpose: &str, value: &T) -> Result<String> {
    let key = key.as_ref();
    ensure!(
        key.len() >= MIN_KEY_BYTES,
        "signing key must contain at least 32 bytes"
    );
    let body = B64.encode(serde_json::to_vec(value)?);
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(purpose.as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    Ok(format!(
        "{body}.{}",
        B64.encode(mac.finalize().into_bytes())
    ))
}
pub fn verify<T: DeserializeOwned>(key: impl AsRef<[u8]>, purpose: &str, token: &str) -> Result<T> {
    let key = key.as_ref();
    ensure!(
        key.len() >= MIN_KEY_BYTES,
        "signing key must contain at least 32 bytes"
    );
    ensure!(token.len() <= 8192, "token too large");
    let (body, signature) = token.split_once('.').context("malformed token")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(purpose.as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    mac.verify_slice(&B64.decode(signature)?)
        .context("invalid signature")?;
    Ok(serde_json::from_slice(&B64.decode(body)?)?)
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub bot_id: String,
    pub role: String,
    pub expires: i64,
}
impl Session {
    pub fn validate(&self, bot: &str, role: Option<&str>) -> Result<()> {
        ensure!(
            self.expires > now() && self.bot_id == bot,
            "session expired or wrong bot"
        );
        ensure!(role.is_none_or(|r| self.role == r), "not authorized");
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Certificate {
    pub device_id: String,
    pub public_key: String,
    pub issuer_signature: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Identity {
    pub certificate: Certificate,
    pub private_key: Secret<String>,
}
impl Certificate {
    fn message(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(&self.device_id, &self.public_key))?)
    }
    pub fn validate(&self, issuer: &VerifyingKey) -> Result<VerifyingKey> {
        ensure!(
            !self.device_id.is_empty() && self.device_id.len() <= 128,
            "invalid device id"
        );
        issuer.verify_strict(
            &self.message()?,
            &Signature::from_slice(&B64.decode(&self.issuer_signature)?)?,
        )?;
        Ok(VerifyingKey::from_bytes(
            &B64.decode(&self.public_key)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid key length"))?,
        )?)
    }
    pub fn bot_id(&self) -> String {
        digest(&format!("robot-buddy:{}", self.device_id))
    }
}
impl Identity {
    pub fn provision(issuer: &SigningKey) -> Result<Self> {
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let mut certificate = Certificate {
            device_id: random(),
            public_key: B64.encode(key.verifying_key().as_bytes()),
            issuer_signature: String::new(),
        };
        certificate.issuer_signature = B64.encode(issuer.sign(&certificate.message()?).to_bytes());
        Ok(Self {
            certificate,
            private_key: Secret::new(B64.encode(key.as_bytes())),
        })
    }
    pub fn proof(&self, challenge: &str) -> Result<String> {
        let key = signing_key(self.private_key.expose())?;
        Ok(B64.encode(key.sign(challenge.as_bytes()).to_bytes()))
    }
}
pub fn signing_key(value: &str) -> Result<SigningKey> {
    let decoded = Secret::new(B64.decode(value)?);
    let bytes: &[u8; 32] = decoded
        .expose()
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid private key length"))?;
    Ok(SigningKey::from_bytes(bytes))
}
pub fn verifying_key(value: &str) -> Result<VerifyingKey> {
    Ok(VerifyingKey::from_bytes(
        &B64.decode(value)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid public key length"))?,
    )?)
}
pub fn verify_proof(key: &VerifyingKey, challenge: &str, signature: &str) -> Result<()> {
    key.verify_strict(
        challenge.as_bytes(),
        &Signature::from_slice(&B64.decode(signature)?)?,
    )?;
    Ok(())
}
#[derive(Serialize, Deserialize)]
pub struct Challenge {
    pub device_id: String,
    pub public_key: String,
    pub nonce: String,
    pub expires: i64,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_are_bound_to_purpose_and_key() -> Result<()> {
        let token = sign([1; 32], "device", &"value")?;
        assert!(verify::<String>(&[1; 32], "claim", &token).is_err());
        assert!(verify::<String>(&[2; 32], "device", &token).is_err());
        assert_eq!(verify::<String>(&[1; 32], "device", &token)?, "value");
        Ok(())
    }
    #[test]
    fn certificate_and_proof_bind_device_key() -> Result<()> {
        let issuer = SigningKey::generate(&mut rand::rngs::OsRng);
        let identity = Identity::provision(&issuer)?;
        let key = identity.certificate.validate(&issuer.verifying_key())?;
        verify_proof(&key, "nonce", &identity.proof("nonce")?)?;
        assert!(verify_proof(&key, "other", &identity.proof("nonce")?).is_err());
        Ok(())
    }
    #[test]
    fn short_keys_are_rejected_for_sign_and_verify() -> Result<()> {
        assert!(sign([1; 31], "session", &"value").is_err());
        let token = sign([1; 32], "session", &"value")?;
        // A 31-byte prefix must not verify, even though HMAC accepts any key length.
        assert!(verify::<String>(&[1; 31], "session", &token).is_err());
        Ok(())
    }
    #[test]
    fn ct_eq_matches_equality() {
        assert!(ct_eq(b"csrf-token", b"csrf-token"));
        assert!(!ct_eq(b"csrf-token", b"csrf-tokem"));
        assert!(!ct_eq(b"csrf-token", b"csrf-token\0"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }
    #[test]
    fn secrets_are_redacted_and_round_trip_identity_files() -> Result<()> {
        let secret = Secret::new("provider-key-value".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret([redacted])");
        let issuer = SigningKey::generate(&mut rand::rngs::OsRng);
        let identity = Identity::provision(&issuer)?;
        let json = serde_json::to_string(&identity)?;
        let back: Identity = serde_json::from_str(&json)?;
        assert_eq!(back.private_key.expose(), identity.private_key.expose());
        assert!(json.contains(&format!(
            "\"private_key\":\"{}\"",
            identity.private_key.expose()
        )));
        let mut wiped = b"key".to_vec();
        wiped.wipe();
        assert_eq!(wiped, [0, 0, 0]);
        Ok(())
    }
}
