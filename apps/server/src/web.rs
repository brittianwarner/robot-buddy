//! HTTP plumbing shared by every route: typed errors, bounded body reads, cookies,
//! origin checks, HTML escaping and the response security headers.
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde::de::DeserializeOwned;
use std::time::Duration;

/// Every request body is read within this deadline, after the route's header checks.
pub const BODY_DEADLINE: Duration = Duration::from_secs(5);
/// Largest body any route reads, unless it passes a smaller limit.
pub const BODY_LIMIT: usize = 16 * 1024;

/// What went wrong, as the client may learn it. Internal error text never reaches a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Malformed or invalid input.
    BadRequest,
    /// Missing, expired or invalid credentials or session.
    Unauthorized,
    /// Authenticated or not, this request is refused (origin, CSRF, allowlist).
    Forbidden,
    NotFound,
    /// The body did not arrive within [`BODY_DEADLINE`].
    RequestTimeout,
    /// The request conflicts with the target's state (a stopped or replaced turn).
    Conflict,
    /// Rate or capacity limit.
    TooManyRequests,
    /// The engine, identity provider or another dependency failed.
    BadGateway,
    /// A feature is not configured or a dependency is down.
    Unavailable,
    /// A dependency did not answer in time.
    GatewayTimeout,
    Internal,
}

impl Kind {
    pub fn status(self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::RequestTimeout => StatusCode::REQUEST_TIMEOUT,
            Self::Conflict => StatusCode::CONFLICT,
            Self::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            Self::BadGateway => StatusCode::BAD_GATEWAY,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::GatewayTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::BadRequest => {
                "Request could not be completed. Retry setup or check server configuration."
            }
            Self::Unauthorized => "Sign-in required or expired.",
            Self::Forbidden => "Request not allowed.",
            Self::NotFound => "Not found.",
            Self::RequestTimeout => "Request body took too long.",
            Self::Conflict => "Request no longer applies.",
            Self::TooManyRequests => "Too many requests. Retry shortly.",
            Self::BadGateway => "A dependent service failed. Retry shortly.",
            Self::Unavailable => "Not available. Check server configuration.",
            Self::GatewayTimeout => "A dependent service did not answer in time.",
            Self::Internal => "Internal error.",
        }
    }
}

/// Route error. The cause is dropped unlogged: it may quote tokens, URLs or prompts.
#[derive(Debug)]
pub struct ApiError(pub Kind);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.0.status();
        tracing::warn!(status = status.as_u16(), "request rejected");
        let mut response = (status, self.0.message()).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

pub type Web<T> = Result<T, ApiError>;

/// Attach a status to a fallible value.
pub trait Reject<T> {
    fn reject(self, kind: Kind) -> Web<T>;
}
impl<T, E: Into<anyhow::Error>> Reject<T> for Result<T, E> {
    fn reject(self, kind: Kind) -> Web<T> {
        self.map_err(|error| {
            drop(error.into());
            ApiError(kind)
        })
    }
}
impl<T> Reject<T> for Option<T> {
    fn reject(self, kind: Kind) -> Web<T> {
        self.ok_or(ApiError(kind))
    }
}

/// Fails with `kind` unless `condition` holds.
pub fn require(condition: bool, kind: Kind) -> Web<()> {
    if condition {
        Ok(())
    } else {
        Err(ApiError(kind))
    }
}

/// Reads a body of at most `limit` bytes within [`BODY_DEADLINE`]. Routes call this only
/// after their header checks, so an unauthenticated caller never holds a body open.
pub async fn read_body(body: Body, limit: usize) -> Web<Bytes> {
    tokio::time::timeout(BODY_DEADLINE, axum::body::to_bytes(body, limit))
        .await
        .reject(Kind::RequestTimeout)?
        .reject(Kind::BadRequest)
}

/// A JSON body (`application/json`), read as [`read_body`].
pub async fn json<T: DeserializeOwned>(headers: &HeaderMap, body: Body) -> Web<T> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    require(json, Kind::BadRequest)?;
    serde_json::from_slice(&read_body(body, BODY_LIMIT).await?).reject(Kind::BadRequest)
}

/// A URL-encoded form body of string fields, read as [`read_body`]. A repeated field
/// is refused rather than resolved.
pub async fn form<T: DeserializeOwned>(body: Body) -> Web<T> {
    let bytes = read_body(body, BODY_LIMIT).await?;
    let mut fields = serde_json::Map::new();
    for (name, value) in url::form_urlencoded::parse(&bytes) {
        let previous = fields.insert(name.into_owned(), value.into_owned().into());
        require(previous.is_none(), Kind::BadRequest)?;
    }
    serde_json::from_value(fields.into()).reject(Kind::BadRequest)
}

/// Escapes text for an HTML element or a quoted attribute.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A small server-rendered page. `body` must already be escaped.
pub fn page(body: &str) -> Response {
    Html(format!(
        "<!doctype html><html><head>\
         <meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>Robot Buddy</title></head>\
         <body><main><h1>Robot Buddy</h1>{body}</main></body></html>"
    ))
    .into_response()
}

/// Baseline for every response, route values win: no caching, no sniffing, no
/// framing, no referrer and nothing loadable unless a route sets its own policy.
/// `Strict-Transport-Security` once the public URL is HTTPS.
pub fn security_headers(response: &mut Response, https: bool) {
    let headers = response.headers_mut();
    let defaults = [
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ];
    for (name, value) in defaults {
        insert_default(headers, name, value);
    }
    if https {
        insert_default(
            headers,
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=31536000",
        );
    }
}

fn insert_default(headers: &mut HeaderMap, name: HeaderName, value: &'static str) {
    headers
        .entry(name)
        .or_insert_with(|| HeaderValue::from_static(value));
}

/// Reads one cookie across every `Cookie` header (HTTP/2 may split them).
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(name)?.strip_prefix('='))
}

/// Cookie attributes. `secure` is set when the public URL is HTTPS.
pub struct CookieSpec {
    pub name: &'static str,
    pub path: &'static str,
    pub same_site: &'static str,
    pub max_age: i64,
    pub secure: bool,
}

impl CookieSpec {
    pub fn set(&self, value: &str) -> anyhow::Result<HeaderValue> {
        self.header(value, self.max_age)
    }
    pub fn clear(&self) -> anyhow::Result<HeaderValue> {
        self.header("", 0)
    }
    fn header(&self, value: &str, max_age: i64) -> anyhow::Result<HeaderValue> {
        let secure = if self.secure { "; Secure" } else { "" };
        Ok(HeaderValue::from_str(&format!(
            "{}={value}; HttpOnly; SameSite={}; Path={}; Max-Age={max_age}{secure}",
            self.name, self.same_site, self.path
        ))?)
    }
}

/// `Origin` must be present and equal the public origin. Browsers send it on every
/// form or fetch POST unless the page's referrer policy is `no-referrer`.
pub fn same_origin(expected: &str, headers: &HeaderMap) -> bool {
    headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|origin| robot_buddy::auth::ct_eq(origin.as_bytes(), expected.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cookies_from_every_header() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("a=1; buddy_login2=x"),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("buddy_login=v; b=2"),
        );
        assert_eq!(cookie(&headers, "buddy_login"), Some("v"));
        assert_eq!(cookie(&headers, "b"), Some("2"));
        assert_eq!(cookie(&headers, "missing"), None);
    }

    #[test]
    fn clearing_cookies_keeps_secure() -> anyhow::Result<()> {
        let spec = CookieSpec {
            name: "n",
            path: "/p",
            same_site: "Lax",
            max_age: 60,
            secure: true,
        };
        assert_eq!(
            spec.clear()?,
            "n=; HttpOnly; SameSite=Lax; Path=/p; Max-Age=0; Secure"
        );
        Ok(())
    }

    #[test]
    fn escape_covers_html_and_attribute_metacharacters() {
        assert_eq!(
            escape(r#"<a href="x" title='y'>&</a>"#),
            "&lt;a href=&quot;x&quot; title=&#39;y&#39;&gt;&amp;&lt;/a&gt;"
        );
        assert_eq!(escape("plain é"), "plain é");
    }

    #[test]
    fn security_headers_fill_defaults_and_keep_route_values() {
        let mut response = page("<p>x</p>");
        response.headers_mut().insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        );
        security_headers(&mut response, true);
        let headers = response.headers();
        assert_eq!(headers[header::REFERRER_POLICY], "same-origin");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            headers[header::STRICT_TRANSPORT_SECURITY],
            "max-age=31536000"
        );
        let mut plain = page("");
        security_headers(&mut plain, false);
        assert!(
            !plain
                .headers()
                .contains_key(header::STRICT_TRANSPORT_SECURITY)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bodies_are_read_within_the_deadline() {
        let stalled =
            Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
        let started = tokio::time::Instant::now();
        let error = read_body(stalled, 16).await.unwrap_err();
        assert_eq!(error.0, Kind::RequestTimeout);
        assert_eq!(started.elapsed(), BODY_DEADLINE);
        let large = read_body(Body::from(vec![0u8; 17]), 16).await.unwrap_err();
        assert_eq!(large.0, Kind::BadRequest);
        let mut headers = HeaderMap::new();
        let plain = json::<serde_json::Value>(&headers, Body::from("{}")).await;
        assert_eq!(plain.unwrap_err().0, Kind::BadRequest, "content type");
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let value: serde_json::Value = json(&headers, Body::from("{}")).await.unwrap();
        assert!(value.is_object());
    }

    #[tokio::test]
    async fn forms_refuse_repeated_fields() {
        #[derive(serde::Deserialize)]
        struct F {
            a: String,
        }
        let f: F = form(Body::from("a=x%20y&b=2")).await.unwrap();
        assert_eq!(f.a, "x y");
        assert!(form::<F>(Body::from("a=1&a=2")).await.is_err());
        assert!(form::<F>(Body::from("b=1")).await.is_err());
    }

    #[test]
    fn server_errors_hide_details() {
        let response = Err::<(), _>(anyhow::anyhow!("secret detail"))
            .reject(Kind::BadRequest)
            .unwrap_err()
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        for kind in [
            Kind::BadGateway,
            Kind::Unavailable,
            Kind::GatewayTimeout,
            Kind::Internal,
        ] {
            assert!(!kind.message().contains("secret"));
            assert!(kind.status().is_server_error());
        }
    }
}
