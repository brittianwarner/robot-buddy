//! Router tests driven with `tower::ServiceExt::oneshot`. No engine, registry or network:
//! the Rivet client points at a closed port and is never reached by these routes.
use super::*;
use axum::{
    Extension,
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode, header},
};
use rivetkit::client::ClientConfig;
use tower::ServiceExt;

const PUBLIC: &str = "https://buddy.example";
const LAB_TOKEN: &str = "lab-token-for-tests-only-0123456789abcdef";

fn app(oidc: bool, lab: bool) -> Arc<App> {
    app_with(oidc, lab, "http://127.0.0.1:9")
}

fn app_with(oidc: bool, lab: bool, engine: &str) -> Arc<App> {
    let issuer = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    Arc::new(App {
        key: Secret::new(vec![9; 32]),
        issuer: issuer.verifying_key(),
        client: Client::new(ClientConfig::new("http://127.0.0.1:9").namespace("default")),
        http: reqwest::Client::new(),
        engine: engine.into(),
        admin: Secret::new("a".repeat(32)),
        public_engine: "https://rivet.buddy.example".into(),
        public_url: PUBLIC.into(),
        public_origin: PUBLIC.into(),
        namespace: "default".into(),
        oidc: oidc.then(|| oidc::tests::provider(String::new(), oidc::JwksCache::default())),
        lab_token: lab.then(|| Secret::new(LAB_TOKEN.into())),
        speech: None,
        limiter: limit::RateLimiter::new(Vec::new()),
        logins: claim::Logins::default(),
        lab_sessions: lab::Sessions::default(),
        readiness: Readiness::default(),
    })
}

/// `app` with speech configured: `streams` slots and a provider on a closed port.
fn with_speech(app: Arc<App>, streams: usize) -> Arc<App> {
    let mut full = Arc::into_inner(app).expect("no other reference");
    full.speech = Some(speech::Speech::for_tests(streams));
    Arc::new(full)
}

fn service(app: &Arc<App>, peer: [u8; 4]) -> Router {
    // What `into_make_service_with_connect_info` inserts for a real socket.
    let peer = serve::Peer::detached(std::net::SocketAddr::from((peer, 4000)));
    router(app.clone()).layer(Extension(ConnectInfo(peer)))
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
    // The login state lives on the server; the cookie is an opaque id that names it.
    let id = cookie_pair(&reply.headers, "buddy_login");
    let id = id.trim_start_matches("buddy_login=");
    assert!(
        id.len() >= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{id}"
    );
    for secret in [
        query["state"].as_str(),
        query["nonce"].as_str(),
        "claim-token",
    ] {
        assert!(!id.contains(secret), "the cookie carries no login secret");
    }
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
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("buddy_login=") && c.contains("Max-Age=300")),
        "a login transaction is short-lived"
    );

    // The matching state spends the transaction before the exchange. The exchange fails
    // here (no token endpoint), and the same cookie cannot try again.
    let status = send(&router, callback(&query["state"], Some(&login)))
        .await
        .status;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let status = send(&router, callback(&query["state"], Some(&login)))
        .await
        .status;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a spent login is not replayable"
    );
}

#[tokio::test]
async fn the_callback_is_rate_limited_per_client() {
    let app = app(true, false);
    let router = service(&app, [192, 0, 2, 77]);
    let callback = || {
        Request::get("/auth/callback?code=c&state=s")
            .body(Body::empty())
            .unwrap()
    };
    for _ in 0..10 {
        assert_eq!(
            send(&router, callback()).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let reply = send(&router, callback()).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(reply.headers.contains_key(header::RETRY_AFTER));
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
    for path in ["/lab/app.js", "/lab/api/state"] {
        let reply = send(&router, Request::get(path).body(Body::empty()).unwrap()).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path} is not mounted");
    }
    // The engine is unreachable: liveness still answers, readiness reports 503 without
    // internal detail.
    let reply = send(
        &router,
        Request::get("/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = send(&router, Request::get("/ready").body(Body::empty()).unwrap()).await;
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
    // Logout revoked the session: the old cookie no longer opens the lab.
    let state = Request::get("/lab/api/state")
        .header(header::COOKIE, cookie.as_str())
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&router, state).await.status, StatusCode::UNAUTHORIZED);
    let status = send(&router, lab_action(&cookie, Some(&csrf), &snapshot))
        .await
        .status;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The lab cookie names a server-side session: it carries neither the lab token nor
/// the CSRF value, and a forged id opens nothing.
#[tokio::test]
async fn the_lab_cookie_is_an_opaque_session_id() {
    let app = app(false, true);
    let router = service(&app, [192, 0, 2, 1]);
    let reply = send(&router, lab_login(LAB_TOKEN, Some(PUBLIC))).await;
    assert_eq!(reply.status, StatusCode::OK);
    let csrf = serde_json::from_str::<Value>(&reply.body).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let cookie = cookie_pair(&reply.headers, "buddy_lab");
    assert!(!cookie.contains(LAB_TOKEN) && !cookie.contains(&csrf));
    let forged = Request::get("/lab/api/state")
        .header(header::COOKIE, "buddy_lab=00000000000000000000000000000000")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&router, forged).await.status, StatusCode::UNAUTHORIZED);
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
async fn only_an_extended_request_outlives_the_header_deadline() {
    async fn slow() -> StatusCode {
        tokio::time::sleep(Duration::from_mins(2)).await;
        StatusCode::OK
    }
    async fn admitted(Extension(admitted): Extension<limit::Admitted>) -> StatusCode {
        admitted.extend(limit::SPEECH_HEAD_DEADLINE);
        slow().await
    }
    let router = Router::new()
        .route("/v1/speech", post(slow))
        .route("/admitted", post(admitted))
        .layer(from_fn_with_state(app(false, false), limit::limit_requests));
    let request = |path: &str| Request::post(path).body(Body::empty()).unwrap();
    // The path earns nothing: an unadmitted speech request gets the 30 s deadline.
    assert_eq!(
        send(&router, request("/v1/speech")).await.status,
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(
        send(&router, request("/admitted")).await.status,
        StatusCode::OK
    );
}

/// A request body that sends nothing, ever.
fn stalled() -> Body {
    Body::from_stream(futures_util::stream::pending::<
        Result<Vec<u8>, std::io::Error>,
    >())
}

fn stalled_speech(path: &str, headers: &[(header::HeaderName, String)]) -> Request<Body> {
    let mut request = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request.body(stalled()).unwrap()
}

fn enroll_body(body: Body) -> Request<Body> {
    Request::post("/v1/enroll")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}

fn speech_body(bearer: &str, body: Body) -> Request<Body> {
    Request::post("/v1/speech")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .body(body)
        .unwrap()
}

/// Unauthenticated speech bodies are refused from their headers: none is ever read.
#[tokio::test(start_paused = true)]
async fn slow_unauthenticated_speech_bodies_are_refused_from_headers() {
    let app = app(false, true);
    let router = service(&app, [192, 0, 2, 1]);
    let started = tokio::time::Instant::now();
    for path in ["/v1/speech", "/lab/api/speech"] {
        let status = send(&router, stalled_speech(path, &[])).await.status;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
    assert_eq!(started.elapsed(), Duration::ZERO, "no body was awaited");
}

/// security #1: slow unauthenticated bodies, from many clients, hold the shared pool
/// for at most the body deadline and cannot delay liveness, enrollment from another
/// client, or a speech admission.
#[tokio::test(start_paused = true)]
async fn slow_bodies_cannot_delay_health_enrollment_or_speech() {
    let app = with_speech(app(false, false), 1);
    // Ten clients, each stalling as many enrollments as its rate limit allows.
    let mut held = Vec::new();
    for client in 1..=10 {
        let router = service(&app, [198, 51, 100, client]);
        for _ in 0..10 {
            let router = router.clone();
            held.push(tokio::spawn(async move {
                let begun = tokio::time::Instant::now();
                let status = send(&router, enroll_body(stalled())).await.status;
                (status, begun.elapsed())
            }));
        }
    }
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let begun = tokio::time::Instant::now();
    let health = service(&app, [198, 51, 100, 1]);
    let reply = send(
        &health,
        Request::get("/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    // Another client's enrollment is admitted and judged on its merits (a bad body).
    let other = service(&app, [198, 51, 100, 11]);
    let reply = send(&other, enroll_body(Body::from("{}"))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    // A speech request is admitted, authenticated and read (an empty turn id is 400).
    let speaker = service(&app, [198, 51, 100, 12]);
    let device = app.session("bot-1", "device").unwrap();
    let reply = send(
        &speaker,
        speech_body(&device, Body::from(r#"{"turn_id":""}"#)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(begun.elapsed(), Duration::ZERO, "nothing above waited");
    for task in held {
        let (status, elapsed) = task.await.unwrap();
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(elapsed, web::BODY_DEADLINE);
    }
}

/// `/health` sits outside the shared pool: it answers while the pool is full.
#[tokio::test(start_paused = true)]
async fn health_answers_while_the_pool_is_full() {
    let app = with_speech(app(false, false), 1);
    let device = app.session("bot-1", "device").unwrap();
    let mut held = Vec::new();
    for client in 0..limit::IN_FLIGHT / limit::PER_CLIENT {
        let router = service(&app, [203, 0, 113, u8::try_from(client).unwrap()]);
        for _ in 0..limit::PER_CLIENT {
            let (router, device) = (router.clone(), device.clone());
            held.push(tokio::spawn(async move {
                send(&router, speech_body(&device, stalled())).await.status
            }));
        }
    }
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let late = service(&app, [203, 0, 113, 200]);
    let reply = send(&late, Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = send(
        &late,
        speech_body(&device, Body::from(r#"{"turn_id":"t"}"#)),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the pool is full"
    );
    for task in held {
        assert_eq!(task.await.unwrap(), StatusCode::REQUEST_TIMEOUT);
    }
}

/// One client cannot take more than its share of the shared pool.
#[tokio::test(start_paused = true)]
async fn one_client_holds_at_most_its_share_of_the_pool() {
    let app = with_speech(app(false, false), 1);
    let device = app.session("bot-1", "device").unwrap();
    let router = service(&app, [203, 0, 113, 9]);
    let held: Vec<_> = (0..limit::PER_CLIENT)
        .map(|_| {
            let (router, device) = (router.clone(), device.clone());
            tokio::spawn(async move { send(&router, speech_body(&device, stalled())).await.status })
        })
        .collect();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let reply = send(
        &router,
        speech_body(&device, Body::from(r#"{"turn_id":""}"#)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    let other = service(&app, [203, 0, 113, 10]);
    let reply = send(
        &other,
        speech_body(&device, Body::from(r#"{"turn_id":""}"#)),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "another client is admitted"
    );
    for task in held {
        assert_eq!(task.await.unwrap(), StatusCode::REQUEST_TIMEOUT);
    }
}

#[tokio::test(start_paused = true)]
async fn an_authenticated_speech_body_gets_a_short_read_deadline() {
    let app = with_speech(app(false, false), 1);
    let router = service(&app, [192, 0, 2, 1]);
    let device = app.session("bot-1", "device").unwrap();
    let started = tokio::time::Instant::now();
    let reply = send(&router, speech_body(&device, stalled())).await;
    assert_eq!(reply.status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(started.elapsed(), web::BODY_DEADLINE);
    // An admin session is not a speaker: rejected before the body.
    let admin = app.session("bot-1", "admin").unwrap();
    let started = tokio::time::Instant::now();
    let reply = send(&router, speech_body(&admin, stalled())).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(started.elapsed(), Duration::ZERO);
}

/// A fake engine whose `/health` counts its probes.
async fn counting_engine() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let engine = Router::new().route(
        "/health",
        get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { "ok" }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, engine).await });
    (format!("http://{address}"), hits)
}

/// security #1: `/ready` shares one engine probe among concurrent and recent callers,
/// and is rate-limited per client.
#[tokio::test]
async fn readiness_coalesces_its_engine_probe() {
    use std::sync::atomic::Ordering;
    let (engine, hits) = counting_engine().await;
    let app = app_with(false, false, &engine);
    let router = service(&app, [192, 0, 2, 50]);
    let ready = || Request::get("/ready").body(Body::empty()).unwrap();
    let replies = futures_util::future::join_all((0..8).map(|_| send(&router, ready()))).await;
    assert!(replies.iter().all(|r| r.status == StatusCode::OK));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "concurrent callers share one probe"
    );
    assert_eq!(send(&router, ready()).await.status, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "a fresh result is reused");
    // Age the cached result past its freshness.
    *app.readiness.0.lock().await = Some((Instant::now() - READY_FRESH, true));
    assert_eq!(send(&router, ready()).await.status, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let reply = send(&router, ready()).await;
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "11th from one client"
    );
    let other = service(&app, [192, 0, 2, 51]);
    assert_eq!(send(&other, ready()).await.status, StatusCode::OK);
}

/// security #9: every response carries the hardening headers once, and HSTS when the
/// public URL is HTTPS.
#[tokio::test]
async fn responses_carry_security_headers() {
    let app = app(false, true);
    let router = service(&app, [192, 0, 2, 1]);
    for path in ["/health", "/claim?bot=b&token=t", "/lab", "/nowhere"] {
        let reply = send(&router, Request::get(path).body(Body::empty()).unwrap()).await;
        let headers = &reply.headers;
        assert_eq!(
            headers.get_all("x-frame-options").iter().count(),
            1,
            "{path}"
        );
        assert_eq!(headers["x-frame-options"], "DENY", "{path}");
        assert_eq!(headers["x-content-type-options"], "nosniff", "{path}");
        assert_eq!(headers["cache-control"], "no-store", "{path}");
        assert!(
            headers.contains_key(header::CONTENT_SECURITY_POLICY),
            "{path}"
        );
        assert_eq!(
            headers["strict-transport-security"], "max-age=31536000",
            "{path}"
        );
    }
    drop(router);
    let mut plain = Arc::into_inner(app).unwrap();
    plain.public_url = "http://127.0.0.1:8080".into();
    let plain = Arc::new(plain);
    let reply = send(
        &service(&plain, [192, 0, 2, 1]),
        Request::get("/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(!reply.headers.contains_key("strict-transport-security"));
    assert_eq!(reply.headers["x-frame-options"], "DENY");
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
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "speech unconfigured"
    );
    let expired = app
        .session_until("bot-1", "device", auth::now() - 1)
        .unwrap();
    let reply = send(&router, speech_request(Some(&expired))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "expired bearer");

    // The router holds the only other reference to the app.
    drop(router);
    let full = with_speech(app, 0);
    let router = service(&full, [192, 0, 2, 1]);
    let device = full.session("bot-1", "device").unwrap();
    let reply = send(&router, speech_request(Some(&device))).await;
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "no stream slot"
    );
    let long = format!(r#"{{"turn_id":"{}"}}"#, "t".repeat(129));
    let reply = send(&router, speech_body(&device, Body::from(long))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "turn id too long");
    let reply = send(&router, speech_body(&device, Body::from("not json"))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "malformed body");
    drop(router);

    // With a slot, an unreachable engine is named, not reported as a bad request.
    let open = with_speech(full, 1);
    let router = service(&open, [192, 0, 2, 1]);
    let reply = send(&router, speech_request(Some(&device))).await;
    assert!(
        [StatusCode::SERVICE_UNAVAILABLE, StatusCode::GATEWAY_TIMEOUT].contains(&reply.status),
        "{}",
        reply.status
    );
}
