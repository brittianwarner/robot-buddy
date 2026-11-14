//! Strict OpenID Connect ID-token validation against the configured issuer's keys.
use super::*;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
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
    let keys: JwkSet = serde_json::from_value(json(http.get(&provider.jwks).send().await?).await?)?;
    let key = keys.find(&kid).context("unknown ID-token key")?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[&provider.issuer]);
    validation.set_audience(&[&provider.client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
    validation.leeway = 30;
    let claims = decode::<Claims>(token, &DecodingKey::from_jwk(key)?, &validation)?.claims;
    ensure!(
        claims.sub == subject && !claims.sub.is_empty() && claims.nonce == state.nonce,
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
mod tests {
    use super::*;
    #[tokio::test]
    async fn validates_signature_and_login_binding() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/keys",
                    get(|| async {
                        Json(
                            serde_json::from_str::<Value>(include_str!(
                                "../tests/fixtures/jwks.json"
                            ))
                            .unwrap(),
                        )
                    }),
                ),
            )
            .await
        });
        let provider = Oidc {
            issuer: "https://issuer.example".into(),
            authorize: String::new(),
            token: String::new(),
            userinfo: String::new(),
            jwks: format!("http://{address}/keys"),
            client_id: "buddy".into(),
            client_secret: String::new(),
        };
        let state = LoginState {
            bot: String::new(),
            token: String::new(),
            nonce: "nonce".into(),
            verifier: String::new(),
            expires: auth::now() + 600,
        };
        let valid = json!({"iss":provider.issuer,"aud":"buddy","sub":"owner","nonce":"nonce","iat":auth::now(),"exp":auth::now()+300});
        let mut header = jsonwebtoken::Header::new(Algorithm::ES256);
        header.kid = Some("test-only".into());
        use base64::Engine;
        let pem = include_str!("../tests/fixtures/oidc-test.pem");
        let encoded: String = pem
            .lines()
            .filter(|line| !line.starts_with("---"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        let key = jsonwebtoken::EncodingKey::from_ec_der(&der);
        let http = reqwest::Client::new();
        let token = jsonwebtoken::encode(&header, &valid, &key)?;
        validate(&http, &provider, &state, &token, "owner").await?;
        assert!(
            validate(&http, &provider, &state, &token, "different-owner")
                .await
                .is_err()
        );
        for (field, value) in [
            ("nonce", json!("wrong")),
            ("iss", json!("https://attacker.example")),
            ("aud", json!("other")),
            ("exp", json!(auth::now() - 120)),
            ("iat", json!(auth::now() + 120)),
            ("azp", json!("other")),
        ] {
            let mut claims = valid.clone();
            claims[field] = value;
            let token = jsonwebtoken::encode(&header, &claims, &key)?;
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
}
