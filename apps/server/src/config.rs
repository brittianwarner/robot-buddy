//! Startup configuration read from the environment, and the URL rules it enforces.
use super::{App, Readiness, Secret, claim, lab, limit, oidc::Oidc, speech};
use anyhow::{Context, Result, ensure};
use rivetkit::client::{Client, ClientConfig};
use robot_buddy::auth;
use std::time::Duration;

pub fn env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} required"))
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// A public URL: HTTPS, or plain HTTP to localhost with `BUDDY_ALLOW_INSECURE_LOCAL`.
pub fn safe_url(raw: &str) -> Result<()> {
    let u = url::Url::parse(raw)?;
    ensure!(
        u.username().is_empty() && u.password().is_none() && u.fragment().is_none(),
        "URL contains credentials or fragment"
    );
    ensure!(
        u.scheme() == "https"
            || (std::env::var("BUDDY_ALLOW_INSECURE_LOCAL").as_deref() == Ok("true")
                && u.scheme() == "http"
                && matches!(u.host_str(), Some("localhost" | "127.0.0.1"))),
        "HTTPS required"
    );
    Ok(())
}

/// An endpoint the server sends a secret to (`RIVET_ENDPOINT` carries the engine admin
/// token, `BUDDY_S3_ENDPOINT` the storage keys), so it must be HTTPS. Plaintext is
/// allowed only to loopback, or to the one host named by `{name}_PLAINTEXT_HOST` (a
/// private Docker-network service name, for example).
pub fn secret_endpoint(name: &str, raw: &str, plaintext_host: Option<&str>) -> Result<()> {
    let u = url::Url::parse(raw).with_context(|| format!("invalid {name}"))?;
    ensure!(
        u.username().is_empty() && u.password().is_none() && u.fragment().is_none(),
        "{name} contains credentials or fragment"
    );
    let loopback = match u.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => {
            host == "localhost" || plaintext_host.is_some_and(|named| named == host)
        }
        None => false,
    };
    ensure!(
        u.scheme() == "https" || (u.scheme() == "http" && loopback),
        "{name} requires HTTPS unless it is loopback or {name}_PLAINTEXT_HOST"
    );
    Ok(())
}

/// Checks `{name}` against [`secret_endpoint`] with its `{name}_PLAINTEXT_HOST`.
pub fn secret_endpoint_from_env(name: &str, raw: &str) -> Result<()> {
    let plaintext = non_empty(&format!("{name}_PLAINTEXT_HOST"));
    secret_endpoint(name, raw, plaintext.as_deref())
}

pub fn origin_of(raw: &str) -> Result<String> {
    Ok(url::Url::parse(raw)?.origin().ascii_serialization())
}

impl App {
    pub async fn from_env() -> Result<Self> {
        let key = robot_buddy::config::signing_key()?;
        let engine = env("RIVET_ENDPOINT")?;
        secret_endpoint_from_env("RIVET_ENDPOINT", &engine)?;
        let admin = Secret::new(env("RIVET_TOKEN")?);
        ensure!(
            admin.expose().len() >= 32,
            "RIVET_TOKEN must be random and at least 32 bytes"
        );
        let namespace = std::env::var("RIVET_NAMESPACE").unwrap_or_else(|_| "default".into());
        let public_url = env("BUDDY_PUBLIC_URL")?;
        let public_engine = env("BUDDY_PUBLIC_RIVET_URL")?;
        safe_url(&public_url)?;
        safe_url(&public_engine)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let oidc = match non_empty("OIDC_ISSUER") {
            Some(issuer) => Some(Oidc::discover(&http, issuer).await?),
            None => None,
        };
        let lab_token = non_empty("BUDDY_LAB_TOKEN").map(Secret::new);
        if let Some(token) = &lab_token {
            ensure!(
                (32..=256).contains(&token.expose().len()),
                "BUDDY_LAB_TOKEN must be 32-256 bytes"
            );
        }
        Ok(Self {
            speech: speech::Speech::from_env()?,
            lab_token,
            key,
            issuer: auth::verifying_key(&env("DEVICE_ISSUER_PUBLIC_KEY")?)?,
            client: Client::new(
                ClientConfig::new(&engine)
                    .token(admin.expose())
                    .namespace(&namespace),
            ),
            http,
            engine,
            admin,
            public_engine,
            public_origin: origin_of(&public_url)?,
            public_url,
            namespace,
            oidc,
            limiter: limit::RateLimiter::new(limit::trusted_from_env()?),
            logins: claim::Logins::default(),
            lab_sessions: lab::Sessions::default(),
            readiness: Readiness::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_endpoints_need_tls_off_loopback() {
        for name in ["RIVET_ENDPOINT", "BUDDY_S3_ENDPOINT"] {
            for ok in [
                "https://rivet.example",
                "http://127.0.0.1:6420",
                "http://[::1]:6420",
                "http://localhost:6420",
            ] {
                assert!(secret_endpoint(name, ok, None).is_ok(), "{name} {ok}");
            }
            for bad in [
                "http://rivet.example",
                "http://10.0.0.5:6420",
                "http://engine:6420",
                "https://user:pass@rivet.example",
                "https://rivet.example#x",
                "ws://127.0.0.1:6420",
                "not a url",
            ] {
                let error = secret_endpoint(name, bad, None).unwrap_err().to_string();
                assert!(error.contains(name), "{name} {bad}: {error}");
            }
            // The explicit exception names exactly one host.
            assert!(secret_endpoint(name, "http://engine:6420", Some("engine")).is_ok());
            assert!(secret_endpoint(name, "http://engine.evil:6420", Some("engine")).is_err());
        }
    }
}
