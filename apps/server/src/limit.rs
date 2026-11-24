//! Request admission: a process-wide concurrency cap, a per-client share of it, head
//! deadlines and a per-client token bucket on unauthenticated endpoints.
//!
//! Every route but `/health` starts under the shared cap and the 30 s head deadline.
//! A speech handler that has authenticated the caller and taken a speech stream slot
//! calls [`Admitted::extend`]: that returns its shared permit and grants the longer
//! speech deadline, so the speech cap, not the shared pool, bounds long waits.
//!
//! A client is an IPv4 address or an IPv6 /64, the smallest block one subscriber is
//! normally given, so rotating addresses inside one allocation earns nothing.
use super::{App, serve::Peer};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Requests in flight before the server answers 429. A permit is held until the response
/// head is ready; a streaming body (speech PCM) does not hold one. An admitted speech
/// stream returns its permit early; speech streams have their own cap.
pub const IN_FLIGHT: usize = 128;
/// The share of [`IN_FLIGHT`] one client may hold, so one address cannot fill the pool.
pub const PER_CLIENT: usize = 32;
/// Deadline for a route to produce its response head, including reading its body.
pub const HEAD_DEADLINE: Duration = Duration::from_secs(30);
/// Granted only to an authenticated, admitted speech stream, which waits for the first
/// committed sentence (it can follow a long tool turn). The pipeline has its own 300 s
/// deadline; this backstop only bounds a stuck handler.
pub const SPEECH_HEAD_DEADLINE: Duration = Duration::from_secs(330);
/// Unauthenticated endpoints that verify a credential, start or finish a sign-in, or
/// reach the engine.
const LIMITED_PATHS: [&str; 7] = [
    "/v1/challenge",
    "/v1/enroll",
    "/claim",
    "/login",
    "/auth/callback",
    "/lab/api/login",
    "/ready",
];
/// Burst per client and route, then one request per `REFILL`.
const BURST: f64 = 10.0;
const REFILL: Duration = Duration::from_secs(6);
/// Tracked clients. When full, idle (refilled) buckets go first, then the oldest.
const MAX_CLIENTS: usize = 4096;

/// Who a request is from, for limits: an IPv4 address or an IPv6 /64. IPv4-mapped IPv6
/// addresses count as IPv4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Client(IpAddr);

impl Client {
    pub fn of(ip: IpAddr) -> Self {
        match canonical(ip) {
            IpAddr::V6(v6) => {
                let s = v6.segments();
                Self(IpAddr::V6(Ipv6Addr::new(
                    s[0], s[1], s[2], s[3], 0, 0, 0, 0,
                )))
            }
            v4 @ IpAddr::V4(_) => Self(v4),
        }
    }
}

/// `::ffff:a.b.c.d` is `a.b.c.d`, so dual-stack sockets match IPv4 rules.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    }
}

/// An address block: `192.0.2.0/24`, `2001:db8::/32`, or a bare address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, canonical(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(ip) & mask == u32::from(net)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(ip) & mask == u128::from(net)
            }
            _ => false,
        }
    }
}

impl FromStr for Cidr {
    type Err = anyhow::Error;
    fn from_str(raw: &str) -> anyhow::Result<Self> {
        let (address, prefix) = raw
            .split_once('/')
            .map_or((raw, None), |(a, p)| (a, Some(p)));
        let address = canonical(address.parse()?);
        let max = if address.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.map_or(Ok(max), str::parse::<u8>)?;
        anyhow::ensure!(prefix <= max, "prefix too long");
        let network = match address {
            IpAddr::V4(ip) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
                IpAddr::V4(Ipv4Addr::from(u32::from(ip) & mask))
            }
            IpAddr::V6(ip) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
                IpAddr::V6(Ipv6Addr::from(u128::from(ip) & mask))
            }
        };
        Ok(Self { network, prefix })
    }
}

/// Parses `BUDDY_TRUSTED_PROXY`: comma-separated addresses or CIDR blocks whose
/// `X-Forwarded-For` is believed. Empty means the socket peer is the client.
pub fn trusted_from_env() -> anyhow::Result<Vec<Cidr>> {
    parse_trusted(&std::env::var("BUDDY_TRUSTED_PROXY").unwrap_or_default())
}

fn parse_trusted(raw: &str) -> anyhow::Result<Vec<Cidr>> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse()
                .map_err(|_| anyhow::anyhow!("invalid BUDDY_TRUSTED_PROXY"))
        })
        .collect()
}

pub fn is_trusted(trusted: &[Cidr], ip: IpAddr) -> bool {
    trusted.iter().any(|block| block.contains(ip))
}

/// A request's place in the shared pool and its head deadline. Inserted into every
/// request by [`limit_requests`].
#[derive(Clone)]
pub struct Admitted(Arc<Admission>);

struct Admission {
    place: Mutex<Option<Place>>,
    deadline: tokio::sync::watch::Sender<tokio::time::Instant>,
}

/// One shared permit and the client's share of the pool.
struct Place {
    _permit: OwnedSemaphorePermit,
    _share: Share,
}

impl Admitted {
    /// For an authenticated request that holds its own capacity (a speech stream slot):
    /// leaves the shared pool and allows `deadline` from now to produce the head.
    pub fn extend(&self, deadline: Duration) {
        drop(
            self.0
                .place
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        self.0
            .deadline
            .send_replace(tokio::time::Instant::now() + deadline);
    }
}

/// Fail fast under overload; do not create an unbounded HTTP work queue. Inserts the
/// request's [`Client`] for later layers and handlers.
pub async fn limit_requests(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Response {
    let client = app.limiter.client(peer(&request), request.headers());
    let Some(place) = app.limiter.admit(client) else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let admitted = Admitted(Arc::new(Admission {
        place: Mutex::new(Some(place)),
        deadline: tokio::sync::watch::channel(tokio::time::Instant::now() + HEAD_DEADLINE).0,
    }));
    let mut deadline = admitted.0.deadline.subscribe();
    request.extensions_mut().insert(admitted.clone());
    request.extensions_mut().insert(client);
    let run = next.run(request);
    tokio::pin!(run);
    loop {
        let until = *deadline.borrow_and_update();
        tokio::select! {
            response = &mut run => return response,
            () = tokio::time::sleep_until(until) => {
                return StatusCode::GATEWAY_TIMEOUT.into_response();
            }
            // The sender lives in `admitted`, held here, so this never closes.
            _ = deadline.changed() => {}
        }
    }
}

fn peer(request: &Request) -> Option<SocketAddr> {
    request
        .extensions()
        .get::<ConnectInfo<Peer>>()
        .map(|c| c.0.addr)
}

/// What each client holds against one per-client cap: requests in the shared pool, or
/// open connections. A [`Share`] gives its place back on drop.
pub struct Shares {
    counts: Arc<Mutex<HashMap<Client, usize>>>,
    cap: usize,
}

impl Shares {
    pub fn new(cap: usize) -> Self {
        Self {
            counts: Arc::default(),
            cap,
        }
    }

    /// A place for `client`, or `None` when it already holds `cap`.
    pub fn take(&self, client: Client) -> Option<Share> {
        let mut counts = lock(&self.counts);
        let count = counts.entry(client).or_insert(0);
        if *count >= self.cap {
            return None;
        }
        *count += 1;
        Some(Share {
            client,
            counts: self.counts.clone(),
        })
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        lock(&self.counts).is_empty()
    }
}

/// One place among a client's [`Shares`].
pub struct Share {
    client: Client,
    counts: Arc<Mutex<HashMap<Client, usize>>>,
}

impl Drop for Share {
    fn drop(&mut self) {
        let mut counts = lock(&self.counts);
        if let Some(count) = counts.get_mut(&self.client) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.client);
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The shared pool, its per-client shares and a token bucket per (route, client). The
/// bucket map is bounded; see [`MAX_CLIENTS`].
pub struct RateLimiter {
    capacity: Arc<Semaphore>,
    in_flight: Shares,
    buckets: Mutex<HashMap<(&'static str, Client), Bucket>>,
    trusted_proxies: Vec<Cidr>,
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
    pub fn new(trusted_proxies: Vec<Cidr>) -> Self {
        Self::with_limits(trusted_proxies, BURST, REFILL, MAX_CLIENTS)
    }
    fn with_limits(
        trusted_proxies: Vec<Cidr>,
        burst: f64,
        refill: Duration,
        max_clients: usize,
    ) -> Self {
        Self {
            capacity: Arc::new(Semaphore::new(IN_FLIGHT)),
            in_flight: Shares::new(PER_CLIENT),
            buckets: Mutex::new(HashMap::new()),
            trusted_proxies,
            burst,
            refill,
            max_clients,
        }
    }

    pub fn trusted(&self) -> &[Cidr] {
        &self.trusted_proxies
    }

    /// A place in the shared pool for `client`, or `None` when the pool or the client's
    /// share of it is full.
    fn admit(&self, client: Client) -> Option<Place> {
        let permit = self.capacity.clone().try_acquire_owned().ok()?;
        Some(Place {
            _permit: permit,
            _share: self.in_flight.take(client)?,
        })
    }

    /// The client: the socket peer, unless the peer is a trusted proxy, in which case the
    /// rightmost untrusted `X-Forwarded-For` entry.
    pub fn client(&self, peer: Option<SocketAddr>, headers: &HeaderMap) -> Client {
        let peer = peer.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |p| canonical(p.ip()));
        if !is_trusted(&self.trusted_proxies, peer) {
            return Client::of(peer);
        }
        let forwarded = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .rev()
            .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
            .find(|ip| !is_trusted(&self.trusted_proxies, *ip));
        Client::of(forwarded.unwrap_or(peer))
    }

    /// Takes one token, or returns how long until one is available.
    fn take(&self, route: &'static str, client: Client, now: Instant) -> Result<(), Duration> {
        let mut buckets = lock(&self.buckets);
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

/// Runs inside [`limit_requests`], which names the client.
pub async fn rate_limit(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let Some(route) = LIMITED_PATHS
        .iter()
        .find(|p| **p == request.uri().path())
        .copied()
    else {
        return next.run(request).await;
    };
    let client = match request.extensions().get::<Client>() {
        Some(client) => *client,
        None => app.limiter.client(peer(&request), request.headers()),
    };
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
    fn client(s: &str) -> Client {
        Client::of(ip(s))
    }

    #[test]
    fn bucket_allows_a_burst_then_refills() {
        let limiter = RateLimiter::with_limits(Vec::new(), 3.0, Duration::from_secs(6), 16);
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.take("/login", client("192.0.2.1"), start).is_ok());
        }
        let wait = limiter
            .take("/login", client("192.0.2.1"), start)
            .unwrap_err();
        assert!(wait <= Duration::from_secs(6));
        assert!(
            limiter.take("/login", client("192.0.2.2"), start).is_ok(),
            "per client"
        );
        assert!(
            limiter
                .take("/v1/enroll", client("192.0.2.1"), start)
                .is_ok(),
            "per route"
        );
        let later = start + Duration::from_secs(6);
        assert!(limiter.take("/login", client("192.0.2.1"), later).is_ok());
    }

    #[test]
    fn map_stays_bounded() {
        let limiter = RateLimiter::with_limits(Vec::new(), 2.0, Duration::from_mins(1), 8);
        let now = Instant::now();
        for i in 0..100u8 {
            let _ = limiter.take(
                "/login",
                Client::of(IpAddr::V4(Ipv4Addr::new(10, 0, 0, i))),
                now,
            );
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
        assert_eq!(direct.client(peer, &headers), client("172.17.0.1"));
        let proxied = RateLimiter::new(parse_trusted("172.17.0.1").unwrap());
        // The rightmost entry was added by the trusted proxy; earlier ones are spoofable.
        assert_eq!(proxied.client(peer, &headers), client("198.51.100.7"));
        assert_eq!(
            proxied.client(peer, &HeaderMap::new()),
            client("172.17.0.1")
        );
    }

    #[test]
    fn ipv6_clients_are_keyed_by_their_64_and_mapped_ipv4_is_ipv4() {
        assert_eq!(
            client("2001:db8:1:2:aaaa::1"),
            client("2001:db8:1:2:ffff::9")
        );
        assert_ne!(client("2001:db8:1:2::1"), client("2001:db8:1:3::1"));
        assert_eq!(client("::ffff:192.0.2.7"), client("192.0.2.7"));
        assert_ne!(client("192.0.2.7"), client("192.0.2.8"), "IPv4 is a /32");
        // Rotating through one /64 shares one bucket.
        let limiter = RateLimiter::with_limits(Vec::new(), 3.0, Duration::from_secs(6), 16);
        let now = Instant::now();
        for i in 0..3u16 {
            let rotated = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, i));
            assert!(limiter.take("/login", Client::of(rotated), now).is_ok());
        }
        assert!(
            limiter
                .take("/login", client("2001:db8:1:2::ff"), now)
                .is_err()
        );
    }

    #[test]
    fn trusted_proxies_are_cidr_blocks() {
        let trusted = parse_trusted("10.0.0.0/8, 2001:db8::/32,192.0.2.5").unwrap();
        for inside in [
            "10.1.2.3",
            "::ffff:10.9.9.9",
            "2001:db8:ffff::1",
            "192.0.2.5",
        ] {
            assert!(is_trusted(&trusted, ip(inside)), "{inside}");
        }
        for outside in ["11.0.0.1", "2001:db9::1", "192.0.2.6"] {
            assert!(!is_trusted(&trusted, ip(outside)), "{outside}");
        }
        for bad in ["10.0.0.0/33", "::/129", "proxy", "10.0.0.0/x"] {
            assert!(parse_trusted(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_trusted("0.0.0.0/0").unwrap()[0].prefix, 0);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 2001:db8:5:6::1, 10.2.0.1"),
        );
        let limiter = RateLimiter::new(parse_trusted("10.0.0.0/8").unwrap());
        let peer = Some(SocketAddr::new(ip("10.0.0.2"), 4000));
        // 10.2.0.1 is another proxy in the block; the client is the next hop out.
        assert_eq!(limiter.client(peer, &headers), client("2001:db8:5:6::"));
    }

    #[test]
    fn one_client_cannot_fill_the_shared_pool() {
        let limiter = RateLimiter::new(Vec::new());
        let held: Vec<_> = (0..PER_CLIENT)
            .map(|_| limiter.admit(client("192.0.2.1")).unwrap())
            .collect();
        assert!(limiter.admit(client("192.0.2.1")).is_none());
        let other = limiter.admit(client("192.0.2.2"));
        assert!(other.is_some(), "another client still gets in");
        drop(held);
        drop(other);
        assert!(limiter.in_flight.is_empty(), "slots are released");
        assert_eq!(limiter.capacity.available_permits(), IN_FLIGHT);
    }
}
