//! Strict `OpenID` Connect ID-token validation against the configured issuer's keys.
use super::{
    Arc, Context, Deserialize, Duration, LoginState, Result, Value, auth, ensure, env, safe_url,
};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, JwkSet, KeyAlgorithm, PublicKeyUse},
};
use tokio::{sync::Mutex, time::Instant};

/// How long fetched issuer keys are trusted before a refetch.
const JWKS_TTL: Duration = Duration::from_mins(10);
/// Minimum spacing between refetches caused by an unknown `kid`, so forged tokens cannot
/// turn the server into a JWKS request amplifier.
const JWKS_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Oidc {
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    pub userinfo: String,
    pub jwks: String,
    pub client_id: String,
    /// `None` for public clients: the token request then omits `client_secret`.
    pub client_secret: Option<auth::Secret<String>>,
    pub keys: Arc<JwksCache>,
}

impl Oidc {
    /// Reads the discovery document and checks that every endpoint is HTTPS.
    pub async fn discover(http: &reqwest::Client, issuer: String) -> Result<Self> {
        safe_url(&issuer)?;
        let doc: Value = json(
            http.get(format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            ))
            .send()
            .await?,
        )
        .await?;
        ensure!(
            doc["issuer"].as_str() == Some(issuer.as_str()),
            "OIDC issuer mismatch"
        );
        let field = |name: &str| -> Result<String> {
            let s = doc[name]
                .as_str()
                .context("OIDC endpoint missing")?
                .to_owned();
            safe_url(&s)?;
            Ok(s)
        };
        Ok(Self {
            authorize: field("authorization_endpoint")?,
            token: field("token_endpoint")?,
            userinfo: field("userinfo_endpoint")?,
            jwks: field("jwks_uri")?,
            issuer,
            client_id: env("OIDC_CLIENT_ID")?,
            client_secret: std::env::var("OIDC_CLIENT_SECRET")
                .ok()
                .filter(|s| !s.is_empty())
                .map(auth::Secret::new),
            keys: Arc::new(JwksCache::default()),
        })
    }

    /// Authorization-code exchange body. The secret is included only when configured.
    pub fn token_form<'a>(
        &'a self,
        code: &'a str,
        verifier: &'a str,
        redirect_uri: &'a str,
    ) -> Vec<(&'static str, &'a str)> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("client_id", self.client_id.as_str()),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.expose().as_str()));
        }
        form
    }
}

/// Issuer keys with a bounded lifetime. One fetch runs at a time.
pub struct JwksCache {
    ttl: Duration,
    cooldown: Duration,
    state: Mutex<Option<Fetched>>,
}

struct Fetched {
    keys: JwkSet,
    at: Instant,
}

impl Default for JwksCache {
    fn default() -> Self {
        Self::new(JWKS_TTL, JWKS_REFRESH_COOLDOWN)
    }
}

impl JwksCache {
    pub fn new(ttl: Duration, cooldown: Duration) -> Self {
        Self {
            ttl,
            cooldown,
            state: Mutex::new(None),
        }
    }

    /// The key for `kid`: from cache while fresh, else refetched. An unknown `kid` forces
    /// one refetch (key rotation) unless the last fetch is within the cooldown.
    async fn key(&self, http: &reqwest::Client, url: &str, kid: &str) -> Result<Jwk> {
        let mut state = self.state.lock().await;
        if let Some(fetched) = state.as_ref() {
            let age = fetched.at.elapsed();
            if age < self.ttl {
                if let Some(key) = fetched.keys.find(kid) {
                    return Ok(key.clone());
                }
                ensure!(age >= self.cooldown, "unknown ID-token key");
            }
        }
        let keys: JwkSet = serde_json::from_value(json(http.get(url).send().await?).await?)?;
        let found = keys.find(kid).cloned();
        *state = Some(Fetched {
            keys,
            at: Instant::now(),
        });
        found.context("unknown ID-token key")
    }
}

fn key_matches(jwk: &Jwk, alg: Algorithm) -> bool {
    let usable = jwk
        .common
        .public_key_use
        .as_ref()
        .is_none_or(|u| matches!(u, PublicKeyUse::Signature));
    let compatible = jwk.common.key_algorithm.is_none_or(|a| {
        matches!(
            (a, alg),
            (KeyAlgorithm::RS256, Algorithm::RS256) | (KeyAlgorithm::ES256, Algorithm::ES256)
        )
    });
    usable && compatible
}

#[derive(Clone, Deserialize)]
struct Claims {
    sub: String,
    nonce: String,
    #[serde(default)]
    azp: Option<String>,
    aud: Value,
    iat: i64,
}

pub(super) async fn validate(
    http: &reqwest::Client,
    provider: &Oidc,
    state: &LoginState,
    token: &str,
    subject: &str,
) -> Result<()> {
    ensure!(token.len() <= 16384, "ID token too large");
    let header = decode_header(token)?;
    ensure!(
        matches!(header.alg, Algorithm::RS256 | Algorithm::ES256),
        "unsupported ID-token algorithm"
    );
    let kid = header.kid.context("ID token missing key id")?;
    let key = provider.keys.key(http, &provider.jwks, &kid).await?;
    ensure!(key_matches(&key, header.alg), "ID-token key not usable");
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[&provider.issuer]);
    validation.set_audience(&[&provider.client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
    validation.validate_nbf = true;
    validation.leeway = 30;
    let claims = decode::<Claims>(token, &DecodingKey::from_jwk(&key)?, &validation)?.claims;
    ensure!(
        claims.sub == subject
            && !claims.sub.is_empty()
            && auth::ct_eq(claims.nonce.as_bytes(), state.nonce.as_bytes()),
        "ID token subject/nonce mismatch"
    );
    ensure!(
        claims.iat <= auth::now() + 30 && claims.iat >= state.expires - 660,
        "ID token issued outside login window"
    );
    if claims.aud.as_array().is_some_and(|v| v.len() > 1) || claims.azp.is_some() {
        ensure!(
            claims.azp.as_deref() == Some(provider.client_id.as_str()),
            "ID-token authorized party mismatch"
        );
    }
    Ok(())
}

pub(super) async fn json(response: reqwest::Response) -> Result<Value> {
    use futures_util::StreamExt;
    let mut stream = response.error_for_status()?.bytes_stream();
    let mut body = Vec::new();
    while let Some(part) = stream.next().await {
        let part = part?;
        ensure!(
            body.len() + part.len() <= 1_048_576,
            "identity response too large"
        );
        body.extend_from_slice(&part);
    }
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{Json, Router, get, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FIXTURE: &str = include_str!("../tests/fixtures/jwks.json");

    /// Serves `keys` at `/keys` and counts fetches.
    async fn jwks_server(
        keys: Value,
    ) -> Result<(String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/keys",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                let keys = keys.clone();
                async move { Json(keys) }
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((format!("http://{address}/keys"), hits, server))
    }

    pub(crate) fn provider(jwks: String, keys: JwksCache) -> Oidc {
        Oidc {
            issuer: "https://issuer.example".into(),
            authorize: "https://issuer.example/authorize".into(),
            token: String::new(),
            userinfo: String::new(),
            jwks,
            client_id: "buddy".into(),
            client_secret: None,
            keys: Arc::new(keys),
        }
    }

    fn login_state() -> LoginState {
        LoginState {
            bot: String::new(),
            token: String::new(),
            state: "state".into(),
            nonce: "nonce".into(),
            verifier: String::new(),
            expires: auth::now() + 600,
        }
    }

    fn signer() -> Result<jsonwebtoken::EncodingKey> {
        use base64::Engine;
        let pem = include_str!("../tests/fixtures/oidc-test.pem");
        let encoded: String = pem
            .lines()
            .filter(|line| !line.starts_with("---"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        Ok(jsonwebtoken::EncodingKey::from_ec_der(&der))
    }

    fn claims() -> Value {
        json!({
            "iss": "https://issuer.example",
            "aud": "buddy",
            "sub": "owner",
            "nonce": "nonce",
            "iat": auth::now(),
            "exp": auth::now() + 300,
        })
    }

    fn sign(claims: &Value, kid: &str) -> Result<String> {
        let mut header = jsonwebtoken::Header::new(Algorithm::ES256);
        header.kid = Some(kid.into());
        Ok(jsonwebtoken::encode(&header, claims, &signer()?)?)
    }

    #[tokio::test]
    async fn validates_signature_and_login_binding() -> Result<()> {
        let (url, _, server) = jwks_server(serde_json::from_str(FIXTURE)?).await?;
        let provider = provider(url, JwksCache::default());
        let state = login_state();
        let http = reqwest::Client::new();
        let token = sign(&claims(), "test-only")?;
        validate(&http, &provider, &state, &token, "owner").await?;
        assert!(
            validate(&http, &provider, &state, &token, "different-owner")
                .await
                .is_err()
        );
        for (field, value) in [
            ("nonce", json!("wrong")),
            ("nonce", json!("state")),
            ("iss", json!("https://attacker.example")),
            ("aud", json!("other")),
            ("exp", json!(auth::now() - 120)),
            ("iat", json!(auth::now() + 120)),
            ("nbf", json!(auth::now() + 120)),
            ("azp", json!("other")),
        ] {
            let mut claims = claims();
            claims[field] = value;
            let token = sign(&claims, "test-only")?;
            assert!(
                validate(&http, &provider, &state, &token, "owner")
                    .await
                    .is_err(),
                "accepted invalid {field}"
            );
        }
        let mut tampered = token.into_bytes();
        let last = tampered.len() - 10;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        assert!(
            validate(
                &http,
                &provider,
                &state,
                std::str::from_utf8(&tampered)?,
                "owner"
            )
            .await
            .is_err()
        );
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn rejects_encryption_keys_and_mismatched_alg() -> Result<()> {
        let http = reqwest::Client::new();
        let token = sign(&claims(), "test-only")?;
        for (field, value) in [("use", "enc"), ("alg", "RS256")] {
            let mut keys: Value = serde_json::from_str(FIXTURE)?;
            keys["keys"][0][field] = json!(value);
            let (url, _, server) = jwks_server(keys).await?;
            let provider = provider(url, JwksCache::default());
            assert!(
                validate(&http, &provider, &login_state(), &token, "owner")
                    .await
                    .is_err(),
                "accepted JWK {field}={value}"
            );
            server.abort();
        }
        // Keys without `use`/`alg` remain acceptable.
        let mut keys: Value = serde_json::from_str(FIXTURE)?;
        let key = keys["keys"][0].as_object_mut().context("key")?;
        key.remove("use");
        key.remove("alg");
        let (url, _, server) = jwks_server(keys).await?;
        let provider = provider(url, JwksCache::default());
        validate(&http, &provider, &login_state(), &token, "owner").await?;
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn caches_keys_and_refetches_once_for_unknown_kid() -> Result<()> {
        let http = reqwest::Client::new();
        let (url, hits, server) = jwks_server(serde_json::from_str(FIXTURE)?).await?;
        let state = login_state();
        let good = sign(&claims(), "test-only")?;
        let rotated = sign(&claims(), "rotated")?;

        let cached = provider(url.clone(), JwksCache::default());
        validate(&http, &cached, &state, &good, "owner").await?;
        validate(&http, &cached, &state, &good, "owner").await?;
        assert_eq!(hits.load(Ordering::SeqCst), 1, "fresh keys come from cache");
        // Within the cooldown an unknown kid does not refetch.
        assert!(
            validate(&http, &cached, &state, &rotated, "owner")
                .await
                .is_err()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        hits.store(0, Ordering::SeqCst);
        let eager = provider(url.clone(), JwksCache::new(JWKS_TTL, Duration::ZERO));
        validate(&http, &eager, &state, &good, "owner").await?;
        assert!(
            validate(&http, &eager, &state, &rotated, "owner")
                .await
                .is_err()
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "one refetch for the unknown kid"
        );

        hits.store(0, Ordering::SeqCst);
        let expiring = provider(url, JwksCache::new(Duration::ZERO, JWKS_REFRESH_COOLDOWN));
        validate(&http, &expiring, &state, &good, "owner").await?;
        validate(&http, &expiring, &state, &good, "owner").await?;
        assert_eq!(hits.load(Ordering::SeqCst), 2, "expired keys are refetched");
        server.abort();
        Ok(())
    }

    #[test]
    fn token_form_omits_an_empty_secret() {
        let mut provider = provider(String::new(), JwksCache::default());
        let form = provider.token_form("code", "verifier", "https://b/auth/callback");
        assert!(form.iter().all(|(name, _)| *name != "client_secret"));
        provider.client_secret = Some(auth::Secret::new("s3cret".into()));
        let form = provider.token_form("code", "verifier", "https://b/auth/callback");
        assert!(form.contains(&("client_secret", "s3cret")));
    }
}
