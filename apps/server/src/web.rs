//! HTTP plumbing shared by every route: typed errors, cookies and origin checks.
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

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
    /// Rate or capacity limit.
    TooManyRequests,
    /// The engine, identity provider or another dependency failed.
    BadGateway,
    /// A feature is not configured or a dependency is down.
    Unavailable,
    Internal,
}

impl Kind {
    pub fn status(self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            Self::BadGateway => StatusCode::BAD_GATEWAY,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
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
            Self::TooManyRequests => "Too many requests. Retry shortly.",
            Self::BadGateway => "A dependent service failed. Retry shortly.",
            Self::Unavailable => "Not available. Check server configuration.",
            Self::Internal => "Internal error.",
        }
    }
}

/// Route error. The cause is dropped unlogged: it may quote tokens, URLs or prompts.
#[derive(Debug)]
pub struct ApiError(pub Kind);

/// Bare `?` keeps the historical 400 so unannotated callers do not change behavior.
/// Annotate with [`Reject::reject`] wherever another status is meant.
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        drop(error.into());
        Self(Kind::BadRequest)
    }
}

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
    fn server_errors_hide_details() {
        let response = ApiError::from(anyhow::anyhow!("secret detail")).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        for kind in [Kind::BadGateway, Kind::Unavailable, Kind::Internal] {
            assert!(!kind.message().contains("secret"));
            assert!(kind.status().is_server_error());
        }
    }
}
