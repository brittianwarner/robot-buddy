//! Router tests driven with `tower::ServiceExt::oneshot`. No engine, registry or network:
//! the Rivet client points at a closed port and is never reached by these routes.
use super::*;
use axum::{
    Extension,
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request},
};
use tower::ServiceExt;

const PUBLIC: &str = "https://buddy.example";
const LAB_TOKEN: &str = "lab-token-for-tests-only-0123456789abcdef";

fn app(oidc: bool, lab: bool) -> Arc<App> {
    let issuer = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    Arc::new(App {
        key: Secret::new(vec![9; 32]),
        issuer: issuer.verifying_key(),
        client: Client::new(ClientConfig::new("http://127.0.0.1:9").namespace("default")),
        http: reqwest::Client::new(),
        engine: "http://127.0.0.1:9".into(),
        admin: Secret::new("a".repeat(32)),
        public_engine: "https://rivet.buddy.example".into(),
        public_url: PUBLIC.into(),
        public_origin: PUBLIC.into(),
        namespace: "default".into(),
        oidc: oidc.then(|| oidc::tests::provider(String::new(), oidc::JwksCache::default())),
        lab_token: lab.then(|| Secret::new(LAB_TOKEN.into())),
        speech: None,
        limiter: limit::RateLimiter::new(Vec::new()),
    })
}

fn service(app: &Arc<App>, peer: [u8; 4]) -> Router {
    // What `into_make_service_with_connect_info` inserts for a real socket.
    router(app.clone()).layer(Extension(ConnectInfo(SocketAddr::from((peer, 4000)))))
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn send(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn set_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

/// `name=value` from a `Set-Cookie` header.
fn cookie_pair(headers: &HeaderMap, name: &str) -> String {
    set_cookies(headers)
        .into_iter()
        .find(|c| c.starts_with(&format!("{name}=")))
        .and_then(|c| c.split(';').next().map(str::to_owned))
        .unwrap()
}

fn hidden(body: &str, name: &str) -> String {
    let marker = format!("name={name} value=\"");
    let start = body.find(&marker).unwrap() + marker.len();
    body[start..].split('"').next().unwrap().to_owned()
}

fn login_request(form: &str, origin: Option<&str>, cookie: Option<&str>) -> Request<Body> {
    let mut request =
        Request::post("/login").header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    request.body(Body::from(form.to_owned())).unwrap()
}

async fn open_claim(router: &Router, token: &str) -> (String, String) {
    let reply = send(
        router,
        Request::get(format!("/claim?bot=bot-1&token={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["referrer-policy"], "same-origin");
    assert_eq!(reply.headers["x-frame-options"], "DENY");
    let cookie = set_cookies(&reply.headers)
        .into_iter()
        .find(|c| c.starts_with("buddy_claim="))
        .unwrap();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    assert!(cookie.contains("Path=/login") && cookie.contains("; Secure"));
    assert!(
        !cookie.contains(token),
        "cookie must not carry the claim token"
    );
    (
        cookie_pair(&reply.headers, "buddy_claim"),
        hidden(&reply.body, "csrf"),
    )
}

#[tokio::test]
async fn login_rejects_cross_site_and_unbound_posts() {
    let app = app(true, false);
    let router = service(&app, [192, 0, 2, 1]);
    let (cookie, csrf) = open_claim(&router, "claim-token").await;
    let form = format!("bot=bot-1&token=claim-token&csrf={csrf}");
    let cases = [
        ("no origin", login_request(&form, None, Some(&cookie))),
        (
            "foreign origin",
            login_request(&form, Some("https://attacker.example"), Some(&cookie)),
        ),
        (
            "null origin",
            login_request(&form, Some("null"), Some(&cookie)),
        ),
        // The attacker's own auto-submitted form: no claim cookie in the victim's browser.
        ("no cookie", login_request(&form, Some(PUBLIC), None)),
        (
            "wrong csrf",
            login_request(
                "bot=bot-1&token=claim-token&csrf=forged",
                Some(PUBLIC),
                Some(&cookie),
            ),
        ),
        (
            "other claim",
            login_request(
                &format!("bot=bot-1&token=attacker-token&csrf={csrf}"),
                Some(PUBLIC),
                Some(&cookie),
            ),
        ),
        (
            "other bot",
            login_request(
                &format!("bot=bot-2&token=claim-token&csrf={csrf}"),
                Some(PUBLIC),
                Some(&cookie),
            ),
        ),
    ];
    for (name, request) in cases {
        assert_eq!(
            send(&router, request).await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
    }
}

#[tokio::test]
async fn login_redirects_with_independent_state_and_nonce() {
    let app = app(true, false);
    let router = service(&app, [192, 0, 2, 1]);
    let (cookie, csrf) = open_claim(&router, "claim-token").await;
    let form = format!("bot=bot-1&token=claim-token&csrf={csrf}");
    let reply = send(&router, login_request(&form, Some(PUBLIC), Some(&cookie))).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.headers["referrer-policy"], "no-referrer");
    let location = url::Url::parse(reply.headers[header::LOCATION].to_str().unwrap()).unwrap();
    assert!(
        location
            .as_str()
            .starts_with("https://issuer.example/authorize")
    );
    let query: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
    assert_ne!(query["state"], query["nonce"]);
    let cookies = set_cookies(&reply.headers);
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("buddy_login=") && c.contains("; Secure"))
    );
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("buddy_claim=;") && c.contains("Max-Age=0; Secure")),
        "claim cookie is single use"
    );

    // The callback refuses a missing cookie and a mismatched state.
    let login = cookie_pair(&reply.headers, "buddy_login");
    let callback = |state: &str, cookie: Option<&str>| {
        let mut request = Request::get(format!("/auth/callback?code=c&state={state}"));
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        request.body(Body::empty()).unwrap()
    };
    let status = send(&router, callback(&query["state"], None)).await.status;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let status = send(&router, callback(&query["nonce"], Some(&login)))
        .await
        .status;
    assert_eq!(status, StatusCode::FORBIDDEN, "nonce is not the state");
    // A split Cookie header still finds the login cookie.
    let split = Request::get("/auth/callback?code=c&state=wrong")
        .header(header::COOKIE, "other=1")
        .header(header::COOKIE, login.as_str())
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&router, split).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn missing_configuration_is_503_and_disabled_lab_is_404() {
    let app = app(false, false);
    let router = service(&app, [192, 0, 2, 1]);
    let form = "bot=bot-1&token=t&csrf=x";
    let reply = send(&router, login_request(form, Some(PUBLIC), None)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!reply.body.contains("sign-in not configured"));
    let reply = send(&router, Request::get("/lab").body(Body::empty()).unwrap()).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = send(&router, lab_login(LAB_TOKEN, Some(PUBLIC))).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    // The engine is unreachable: health reports 503 without internal detail.
    let reply = send(
        &router,
        Request::get("/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!reply.body.contains("127.0.0.1"));
}

fn lab_login(token: &str, origin: Option<&str>) -> Request<Body> {
    let mut request =
        Request::post("/lab/api/login").header(header::CONTENT_TYPE, "application/json");
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    request
        .body(Body::from(json!({ "token": token }).to_string()))
        .unwrap()
}

fn lab_action(cookie: &str, csrf: Option<&str>, command: &Value) -> Request<Body> {
    let mut request = Request::post("/lab/api/action")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, PUBLIC)
        .header(header::COOKIE, cookie);
    if let Some(csrf) = csrf {
        request = request.header("x-buddy-csrf", csrf);
    }
    request
        .body(Body::from(
            json!({"target":"bot","command":command}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn lab_login_and_csrf_statuses() {
    let app = app(false, true);
    let router = service(&app, [192, 0, 2, 1]);
    let padded = format!("{LAB_TOKEN}\0");
    assert_eq!(
        send(&router, lab_login(&padded, Some(PUBLIC))).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(
            &router,
            lab_login(LAB_TOKEN, Some("https://attacker.example"))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let reply = send(&router, lab_login(LAB_TOKEN, Some(PUBLIC))).await;
    assert_eq!(reply.status, StatusCode::OK);
    let csrf = serde_json::from_str::<Value>(&reply.body).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let cookie = cookie_pair(&reply.headers, "buddy_lab");

    let state = Request::get("/lab/api/state").body(Body::empty()).unwrap();
    assert_eq!(send(&router, state).await.status, StatusCode::UNAUTHORIZED);
    let snapshot = json!({"type":"snapshot"});
    let status = send(&router, lab_action(&cookie, None, &snapshot))
        .await
        .status;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let status = send(&router, lab_action(&cookie, Some("forged"), &snapshot))
        .await
        .status;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let revoke = json!({"type":"revoke"});
    let status = send(&router, lab_action(&cookie, Some(&csrf), &revoke))
        .await
        .status;
    assert_eq!(status, StatusCode::FORBIDDEN, "allowlist");
    let status = send(&router, lab_action(&cookie, Some(&csrf), &snapshot))
        .await
        .status;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no device enrolled yet");

    let logout = Request::post("/lab/api/logout")
        .header(header::ORIGIN, PUBLIC)
        .header(header::COOKIE, cookie.as_str())
        .header("x-buddy-csrf", csrf.as_str())
        .body(Body::empty())
        .unwrap();
    let reply = send(&router, logout).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(set_cookies(&reply.headers)[0].ends_with("Max-Age=0; Secure"));
}

fn challenge_request() -> Request<Body> {
    let certificate = json!({"device_id":"d","public_key":"AAAA","issuer_signature":"AAAA"});
    Request::post("/v1/challenge")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(certificate.to_string()))
        .unwrap()
}

#[tokio::test]
async fn credential_endpoints_are_rate_limited_per_client() {
    let app = app(false, false);
    let first = service(&app, [192, 0, 2, 1]);
    for _ in 0..10 {
        let status = send(&first, challenge_request()).await.status;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "invalid certificate");
    }
    let reply = send(&first, challenge_request()).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(reply.headers.contains_key(header::RETRY_AFTER));
    let other = service(&app, [192, 0, 2, 2]);
    assert_eq!(
        send(&other, challenge_request()).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test(start_paused = true)]
async fn speech_routes_are_exempt_from_the_header_deadline() {
    async fn slow() -> StatusCode {
        tokio::time::sleep(Duration::from_mins(2)).await;
        StatusCode::OK
    }
    let router = Router::new()
        .route("/v1/speech", post(slow))
        .route("/lab/api/speech", post(slow))
        .route("/v1/enroll", post(slow))
        .layer(axum::middleware::from_fn(limit::limit_requests));
    let request = |path: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send(&router, request("/v1/enroll")).await.status,
        StatusCode::GATEWAY_TIMEOUT
    );
    for path in ["/v1/speech", "/lab/api/speech"] {
        assert_eq!(
            send(&router, request(path)).await.status,
            StatusCode::OK,
            "{path}"
        );
    }
}

#[test]
fn engine_config_is_private_ephemeral_and_removed() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir();
    let first = write_runtime_config(&dir, b"{\"auth\":{}}")?;
    let second = write_runtime_config(&dir, b"{}")?;
    assert_ne!(first.0, second.0, "unique names");
    let mode = std::fs::metadata(&first.0)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(std::fs::read(&first.0)?, b"{\"auth\":{}}");
    let path = first.0.clone();
    drop(first);
    assert!(!path.exists(), "removed on drop");
    assert!(!runtime_dir().starts_with("/data"));
    Ok(())
}

fn speech_request(bearer: Option<&str>) -> Request<Body> {
    let mut request = Request::post("/v1/speech").header(header::CONTENT_TYPE, "application/json");
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    request.body(Body::from(r#"{"turn_id":"turn-1"}"#)).unwrap()
}

#[tokio::test]
async fn speech_statuses_name_the_failure() {
    let app = app(false, false);
    let router = service(&app, [192, 0, 2, 1]);
    for bearer in [None, Some("not-a-session")] {
        let reply = send(&router, speech_request(bearer)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let admin = app.session("bot-1", "admin").unwrap();
    let reply = send(&router, speech_request(Some(&admin))).await;
    assert_eq!(
        reply.status,
        StatusCode::UNAUTHORIZED,
        "device role required"
    );
    let device = app.session("bot-1", "device").unwrap();
    let reply = send(&router, speech_request(Some(&device))).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);

    // The router holds the only other reference to the app.
    drop(router);
    let mut full = Arc::into_inner(app).unwrap();
    full.speech = Some(speech::Speech::without_capacity());
    let full = Arc::new(full);
    let router = service(&full, [192, 0, 2, 1]);
    let device = full.session("bot-1", "device").unwrap();
    let reply = send(&router, speech_request(Some(&device))).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
}
