//! Offline commands: `init` writes a new issuer key and server secrets, `provision`
//! issues one device identity. Every encoded key is held in a [`Secret`], so its bytes
//! are zeroized on drop (the `SigningKey`s zeroize themselves).
use super::engine::private_write;
use anyhow::{Context, Result};
use base64::Engine;
use robot_buddy::auth::{self, Secret};
use std::path::Path;

pub fn initialize(args: &[String]) -> Result<()> {
    let dir = Path::new(args.first().context("usage: init DIRECTORY")?);
    std::fs::create_dir_all(dir)?;
    let issuer = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let encoded = Secret::new(b64.encode(issuer.as_bytes()));
    private_write(&dir.join("issuer.key"), encoded.expose().as_bytes())?;
    let data = Secret::new(format!(
        "BUDDY_SIGNING_KEY={}{}\nRIVET_TOKEN={}{}\nDEVICE_ISSUER_PUBLIC_KEY={}\n",
        auth::random(),
        auth::random(),
        auth::random(),
        auth::random(),
        b64.encode(issuer.verifying_key().as_bytes())
    ));
    private_write(&dir.join("secrets.env"), data.expose().as_bytes())?;
    println!("Wrote private issuer key and server secrets. Keep issuer.key offline.");
    Ok(())
}

pub fn provision(args: &[String]) -> Result<()> {
    let encoded = Secret::new(std::fs::read_to_string(
        args.first().context("usage: provision ISSUER_KEY OUTPUT")?,
    )?);
    let issuer = auth::signing_key(encoded.expose().trim())?;
    let identity = auth::Identity::provision(&issuer)?;
    let json = Secret::new(serde_json::to_vec_pretty(&identity)?);
    private_write(
        Path::new(args.get(1).context("output required")?),
        json.expose(),
    )?;
    println!("Provisioned unique device identity.");
    Ok(())
}
