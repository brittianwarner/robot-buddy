//! Short-lived application assertions and manufacturing device identity.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
pub fn random() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn digest(s: &str) -> String {
    B64.encode(Sha256::digest(s.as_bytes()))
}
pub fn sign<T: Serialize>(key: &[u8], purpose: &str, value: &T) -> Result<String> {
    ensure!(
        key.len() >= 32,
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
pub fn verify<T: DeserializeOwned>(key: &[u8], purpose: &str, token: &str) -> Result<T> {
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
    pub private_key: String,
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
            private_key: B64.encode(key.as_bytes()),
        })
    }
    pub fn proof(&self, challenge: &str) -> Result<String> {
        let key = signing_key(&self.private_key)?;
        Ok(B64.encode(key.sign(challenge.as_bytes()).to_bytes()))
    }
}
pub fn signing_key(value: &str) -> Result<SigningKey> {
    Ok(SigningKey::from_bytes(
        &B64.decode(value)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid private key length"))?,
    ))
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
        let token = sign(&[1; 32], "device", &"value")?;
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
}
