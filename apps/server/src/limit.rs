//! Request admission: a process-wide concurrency cap, header deadlines and a per-client
//! token bucket on the unauthenticated credential endpoints.
use super::App;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Requests in flight before the server answers 429. A permit is held until the response
/// head is ready; a streaming body (speech PCM) does not hold one. Speech streams are
/// capped separately by the speech admission.
const IN_FLIGHT: usize = 128;
/// Deadline for an ordinary route to produce its response head.
pub const HEAD_DEADLINE: Duration = Duration::from_secs(30);
/// Speech routes wait for the first committed sentence, which can follow a long tool turn.
/// The pipeline has its own 300 s deadline; this backstop only bounds a stuck handler.
pub const SPEECH_HEAD_DEADLINE: Duration = Duration::from_secs(330);
const SPEECH_PATHS: [&str; 2] = ["/v1/speech", "/lab/api/speech"];
/// Unauthenticated endpoints that verify a credential or start a sign-in.
const LIMITED_PATHS: [&str; 4] = ["/v1/challenge", "/v1/enroll", "/login", "/lab/api/login"];
/// Burst per client and route, then one request per `REFILL`.
const BURST: f64 = 10.0;
const REFILL: Duration = Duration::from_secs(6);
/// Tracked clients. When full, idle (refilled) buckets go first, then the oldest.
const MAX_CLIENTS: usize = 4096;

/// Fail fast under overload; do not create an unbounded HTTP work queue.
pub async fn limit_requests(request: Request, next: Next) -> Response {
    static CAPACITY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(IN_FLIGHT);
    let Ok(_permit) = CAPACITY.try_acquire() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let deadline = if SPEECH_PATHS.contains(&request.uri().path()) {
        SPEECH_HEAD_DEADLINE
    } else {
        HEAD_DEADLINE
    };
    match tokio::time::timeout(deadline, next.run(request)).await {
        Ok(response) => response,
        Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}

/// Token bucket per (route, client IP). The map is bounded; see [`MAX_CLIENTS`].
pub struct RateLimiter {
    buckets: Mutex<HashMap<(&'static str, IpAddr), Bucket>>,
    trusted_proxies: Vec<IpAddr>,
    burst: f64,
    refill: Duration,
    max_clients: usize,
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    fn tokens_at(self, now: Instant, burst: f64, refill: Duration) -> f64 {
        let earned =
            now.saturating_duration_since(self.updated).as_secs_f64() / refill.as_secs_f64();
        (self.tokens + earned).min(burst)
    }
}

impl RateLimiter {
    pub fn new(trusted_proxies: Vec<IpAddr>) -> Self {
        Self::with_limits(trusted_proxies, BURST, REFILL, MAX_CLIENTS)
    }
    fn with_limits(
        trusted_proxies: Vec<IpAddr>,
        burst: f64,
        refill: Duration,
        max_clients: usize,
    ) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            trusted_proxies,
            burst,
            refill,
            max_clients,
        }
    }

    /// Parses `BUDDY_TRUSTED_PROXY`: comma-separated IP addresses whose
    /// `X-Forwarded-For` is believed. Empty means the socket peer is the client.
    pub fn trusted_from_env() -> anyhow::Result<Vec<IpAddr>> {
        let raw = std::env::var("BUDDY_TRUSTED_PROXY").unwrap_or_default();
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse()
                    .map_err(|_| anyhow::anyhow!("invalid BUDDY_TRUSTED_PROXY"))
            })
            .collect()
    }

    /// The client address: the socket peer, unless the peer is a trusted proxy, in which
    /// case the rightmost untrusted `X-Forwarded-For` entry.
    pub fn client(&self, peer: Option<SocketAddr>, headers: &HeaderMap) -> IpAddr {
        let peer = peer.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |p| p.ip());
        if !self.trusted_proxies.contains(&peer) {
            return peer;
        }
        headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .rev()
            .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
            .find(|ip| !self.trusted_proxies.contains(ip))
            .unwrap_or(peer)
    }

    /// Takes one token, or returns how long until one is available.
    fn take(&self, route: &'static str, client: IpAddr, now: Instant) -> Result<(), Duration> {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (route, client);
        if !buckets.contains_key(&key) && buckets.len() >= self.max_clients {
            let (burst, refill) = (self.burst, self.refill);
            buckets.retain(|_, b| b.tokens_at(now, burst, refill) < burst);
            if buckets.len() >= self.max_clients
                && let Some(oldest) = buckets
                    .iter()
                    .min_by_key(|(_, b)| b.updated)
                    .map(|(k, _)| *k)
            {
                buckets.remove(&oldest);
            }
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let tokens = bucket.tokens_at(now, self.burst, self.refill);
        if tokens >= 1.0 {
            *bucket = Bucket {
                tokens: tokens - 1.0,
                updated: now,
            };
            Ok(())
        } else {
            Err(self.refill.mul_f64(1.0 - tokens))
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

pub async fn rate_limit(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let Some(route) = LIMITED_PATHS
        .iter()
        .find(|p| **p == request.uri().path())
        .copied()
    else {
        return next.run(request).await;
    };
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let client = app.limiter.client(peer, request.headers());
    match app.limiter.take(route, client, Instant::now()) {
        Ok(()) => next.run(request).await,
        Err(wait) => {
            let mut response =
                super::web::ApiError(super::web::Kind::TooManyRequests).into_response();
            let seconds = wait.as_secs().max(1).to_string();
            if let Ok(value) = HeaderValue::from_str(&seconds) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn bucket_allows_a_burst_then_refills() {
        let limiter = RateLimiter::with_limits(Vec::new(), 3.0, Duration::from_secs(6), 16);
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.take("/login", ip("192.0.2.1"), start).is_ok());
        }
        let wait = limiter.take("/login", ip("192.0.2.1"), start).unwrap_err();
        assert!(wait <= Duration::from_secs(6));
        assert!(
            limiter.take("/login", ip("192.0.2.2"), start).is_ok(),
            "per client"
        );
        assert!(
            limiter.take("/v1/enroll", ip("192.0.2.1"), start).is_ok(),
            "per route"
        );
        let later = start + Duration::from_secs(6);
        assert!(limiter.take("/login", ip("192.0.2.1"), later).is_ok());
    }

    #[test]
    fn map_stays_bounded() {
        let limiter = RateLimiter::with_limits(Vec::new(), 2.0, Duration::from_mins(1), 8);
        let now = Instant::now();
        for i in 0..100u8 {
            let _ = limiter.take("/login", IpAddr::V4(Ipv4Addr::new(10, 0, 0, i)), now);
            assert!(limiter.len() <= 8);
        }
    }

    #[test]
    fn forwarded_for_is_used_only_from_trusted_proxies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 198.51.100.7"),
        );
        let direct = RateLimiter::new(Vec::new());
        let peer = Some(SocketAddr::new(ip("172.17.0.1"), 4000));
        assert_eq!(direct.client(peer, &headers), ip("172.17.0.1"));
        let proxied = RateLimiter::new(vec![ip("172.17.0.1")]);
        // The rightmost entry was added by the trusted proxy; earlier ones are spoofable.
        assert_eq!(proxied.client(peer, &headers), ip("198.51.100.7"));
        assert_eq!(proxied.client(peer, &HeaderMap::new()), ip("172.17.0.1"));
    }
}
