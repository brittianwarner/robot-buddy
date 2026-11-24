//! The TCP listener: connection caps and read deadlines that `axum::serve` lacks.
//!
//! `axum::serve` gives hyper no timer, so hyper's header-read timeout never fires, and
//! it accepts without limit. [`Guarded`] adds both without a new dependency:
//!
//! - At most [`ConnLimits::total`] connections are open; at the cap, accepting waits and
//!   the kernel backlog holds the rest. One client (an IPv4 address or IPv6 /64) holds
//!   at most [`ConnLimits::per_client`]; a connection past that is closed at accept.
//!   Trusted proxies are exempt from the per-client cap.
//! - While no request is in flight, every read has a deadline: [`ConnLimits::head`] to
//!   finish a request head, counted from accept or from the first byte after an idle
//!   keep-alive, and [`ConnLimits::idle`] for a kept-alive connection to start its next
//!   request. A connection that misses one is closed. A request is in flight from when
//!   its handler starts until its response body ends ([`track`]); the route's own
//!   deadlines govern it then.
use super::limit::{Cidr, Client, Share, Shares, canonical, is_trusted};
use axum::{
    body::{Body, BodyDataStream, Bytes, HttpBody as _},
    extract::{ConnectInfo, Request, connect_info::Connected},
    middleware::Next,
    response::Response,
    serve::{IncomingStream, Listener},
};
use futures_util::{Stream, task::AtomicWaker};
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, Sleep},
};

#[derive(Clone, Copy, Debug)]
pub struct ConnLimits {
    pub total: usize,
    pub per_client: usize,
    pub head: Duration,
    pub idle: Duration,
}

impl Default for ConnLimits {
    fn default() -> Self {
        Self {
            total: 512,
            per_client: 64,
            head: Duration::from_secs(10),
            // Above the robot's 5 min connection-pool idle timeout, so the speech
            // connection it warms stays usable.
            idle: Duration::from_mins(6),
        }
    }
}

/// The accept-time checks shared by every listener: the connection pool and each
/// client's share of it.
struct Gate {
    total: Arc<Semaphore>,
    clients: Shares,
    trusted: Vec<Cidr>,
    limits: ConnLimits,
}

impl Gate {
    fn new(trusted: Vec<Cidr>, limits: ConnLimits) -> Self {
        Self {
            total: Arc::new(Semaphore::new(limits.total)),
            clients: Shares::new(limits.per_client),
            trusted,
            limits,
        }
    }

    /// Waits for room in the pool, so the kernel backlog holds what is not yet accepted.
    async fn room(&self) -> OwnedSemaphorePermit {
        self.total
            .clone()
            .acquire_owned()
            .await
            .expect("the connection semaphore is never closed")
    }

    /// The guarded connection, or `None` (and the stream closed) when its client already
    /// holds its share.
    fn admit<S>(
        &self,
        stream: S,
        addr: SocketAddr,
        permit: OwnedSemaphorePermit,
    ) -> Option<Conn<S>> {
        let ip = canonical(addr.ip());
        let share = if is_trusted(&self.trusted, ip) {
            None
        } else {
            Some(self.clients.take(Client::of(ip))?)
        };
        Some(Conn::new(stream, self.limits, permit, share))
    }
}

pub struct Guarded {
    listener: TcpListener,
    gate: Gate,
}

impl Guarded {
    pub fn new(listener: TcpListener, trusted: Vec<Cidr>, limits: ConnLimits) -> Self {
        Self {
            listener,
            gate: Gate::new(trusted, limits),
        }
    }
}

impl Listener for Guarded {
    type Io = Conn;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Conn, SocketAddr) {
        loop {
            let permit = self.gate.room().await;
            let (stream, addr) = Listener::accept(&mut self.listener).await;
            let _ = stream.set_nodelay(true);
            if let Some(conn) = self.gate.admit(stream, addr, permit) {
                return (conn, addr);
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

/// What a connection's requests tell its reader.
#[derive(Default)]
pub struct ConnState {
    /// Requests in flight.
    busy: AtomicUsize,
    /// Requests finished; a change starts a keep-alive wait.
    served: AtomicU64,
    /// The task reading the connection, woken when the last request finishes so its
    /// deadline starts even if no byte arrives.
    reader: AtomicWaker,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Head,
    Idle,
}

pub struct Conn<S = TcpStream> {
    stream: S,
    state: Arc<ConnState>,
    limits: ConnLimits,
    phase: Phase,
    seen: u64,
    deadline: Pin<Box<Sleep>>,
    _permit: OwnedSemaphorePermit,
    _share: Option<Share>,
}

impl<S> Conn<S> {
    fn new(
        stream: S,
        limits: ConnLimits,
        permit: OwnedSemaphorePermit,
        share: Option<Share>,
    ) -> Self {
        Self {
            stream,
            state: Arc::default(),
            limits,
            phase: Phase::Head,
            seen: 0,
            deadline: Box::pin(tokio::time::sleep(limits.head)),
            _permit: permit,
            _share: share,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Conn<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Registered before `busy` is read, so a request finishing in between still wakes.
        this.state.reader.register(cx.waker());
        if this.state.busy.load(Ordering::SeqCst) > 0 {
            return Pin::new(&mut this.stream).poll_read(cx, buf);
        }
        let served = this.state.served.load(Ordering::SeqCst);
        if served != this.seen {
            this.seen = served;
            this.phase = Phase::Idle;
            this.deadline
                .as_mut()
                .reset(Instant::now() + this.limits.idle);
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.stream).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if this.phase == Phase::Idle && buf.filled().len() > before {
                    // The next request has begun; its head gets the head deadline.
                    this.phase = Phase::Head;
                    this.deadline
                        .as_mut()
                        .reset(Instant::now() + this.limits.head);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => match this.deadline.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connection read deadline",
                ))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Conn<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// The socket peer and its connection, inserted into every request as `ConnectInfo`.
/// `conn` is `None` for a request that did not come through [`Guarded`].
#[derive(Clone)]
pub struct Peer {
    pub addr: SocketAddr,
    pub conn: Option<Arc<ConnState>>,
}

impl Peer {
    #[cfg(test)]
    pub fn detached(addr: SocketAddr) -> Self {
        Self { addr, conn: None }
    }
}

impl Peer {
    fn of<S>(addr: SocketAddr, conn: &Conn<S>) -> Self {
        Self {
            addr,
            conn: Some(conn.state.clone()),
        }
    }
}

impl Connected<IncomingStream<'_, Guarded>> for Peer {
    fn connect_info(stream: IncomingStream<'_, Guarded>) -> Self {
        Self::of(*stream.remote_addr(), stream.io())
    }
}

/// Marks a request in flight on its connection until its response body ends, which
/// lifts the connection's read deadline meanwhile. Outermost layer.
pub async fn track(request: Request, next: Next) -> Response {
    let Some(state) = request
        .extensions()
        .get::<ConnectInfo<Peer>>()
        .and_then(|info| info.0.conn.clone())
    else {
        return next.run(request).await;
    };
    let busy = Busy::enter(state);
    let response = next.run(request).await;
    if response.body().size_hint().exact().is_some() {
        // Fully known body: hyper only writes it now, and writes have no deadline.
        drop(busy);
        return response;
    }
    let (parts, body) = response.into_parts();
    let body = Body::from_stream(Held {
        inner: body.into_data_stream(),
        _busy: busy,
    });
    Response::from_parts(parts, body)
}

struct Busy(Arc<ConnState>);

impl Busy {
    fn enter(state: Arc<ConnState>) -> Self {
        state.busy.fetch_add(1, Ordering::SeqCst);
        Self(state)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        // `served` moves first, so a reader that sees no request in flight also sees it.
        self.0.served.fetch_add(1, Ordering::SeqCst);
        if self.0.busy.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.reader.wake();
        }
    }
}

/// A streamed response body that keeps its request in flight until it ends.
struct Held {
    inner: BodyDataStream,
    _busy: Busy,
}

impl Stream for Held {
    type Item = Result<Bytes, axum::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
        sync::mpsc,
    };

    const LIMITS: ConnLimits = ConnLimits {
        total: 8,
        per_client: 3,
        head: Duration::from_secs(1),
        idle: Duration::from_secs(3),
    };

    /// In-memory connections through the same [`Gate`] and [`Conn`] as [`Guarded`], so
    /// paused time is exact: no kernel socket can let the clock jump past real IO.
    struct Memory {
        incoming: mpsc::Receiver<(DuplexStream, SocketAddr)>,
        gate: Gate,
    }

    impl Listener for Memory {
        type Io = Conn<DuplexStream>;
        type Addr = SocketAddr;

        async fn accept(&mut self) -> (Conn<DuplexStream>, SocketAddr) {
            loop {
                let permit = self.gate.room().await;
                let Some((stream, addr)) = self.incoming.recv().await else {
                    std::future::pending::<()>().await;
                    unreachable!();
                };
                if let Some(conn) = self.gate.admit(stream, addr, permit) {
                    return (conn, addr);
                }
            }
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::from(([127, 0, 0, 1], 0)))
        }
    }

    impl Connected<IncomingStream<'_, Memory>> for Peer {
        fn connect_info(stream: IncomingStream<'_, Memory>) -> Self {
            Self::of(*stream.remote_addr(), stream.io())
        }
    }

    fn router() -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "slow"
                }),
            )
            .route(
                "/stream",
                get(|| async {
                    let frames = futures_util::stream::unfold(0, |i| async move {
                        if i == 3 {
                            return None;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        Some((Ok::<_, io::Error>(Bytes::from("frame;")), i + 1))
                    });
                    Body::from_stream(frames)
                }),
            )
            .layer(axum::middleware::from_fn(track))
    }

    /// Dials the in-memory server.
    #[derive(Clone)]
    struct Dialer(mpsc::Sender<(DuplexStream, SocketAddr)>);

    impl Dialer {
        async fn connect(&self, ip: [u8; 4]) -> DuplexStream {
            let (client, server) = tokio::io::duplex(64 * 1024);
            self.0
                .send((server, SocketAddr::from((ip, 40_000))))
                .await
                .unwrap();
            client
        }
    }

    fn start(limits: ConnLimits) -> Dialer {
        let (tx, incoming) = mpsc::channel(64);
        let listener = Memory {
            incoming,
            gate: Gate::new(Vec::new(), limits),
        };
        tokio::spawn(async move {
            axum::serve(
                listener,
                router().into_make_service_with_connect_info::<Peer>(),
            )
            .await
        });
        Dialer(tx)
    }

    const A: [u8; 4] = [192, 0, 2, 1];
    const B: [u8; 4] = [192, 0, 2, 2];
    const GET: &[u8] = b"GET / HTTP/1.1\r\nhost: t\r\n\r\n";

    /// Reads until `marker` or EOF.
    async fn read_until(stream: &mut DuplexStream, marker: &str) -> String {
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
            if String::from_utf8_lossy(&seen).contains(marker) {
                break;
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    /// Waits for the server to close: EOF or an error, nothing else read.
    async fn closed(stream: &mut DuplexStream) -> bool {
        let mut buf = [0u8; 64];
        matches!(stream.read(&mut buf).await, Ok(0) | Err(_))
    }

    async fn served(stream: &mut DuplexStream) -> bool {
        if stream.write_all(GET).await.is_err() {
            return false;
        }
        read_until(stream, "\r\n\r\nok")
            .await
            .ends_with("\r\n\r\nok")
    }

    #[tokio::test(start_paused = true)]
    async fn a_partial_head_is_dropped_at_the_head_deadline() {
        let server = start(LIMITS);
        let mut slow = server.connect(A).await;
        let begun = Instant::now();
        slow.write_all(b"GET / HTTP/1.1\r\nhost: t\r\n")
            .await
            .unwrap();
        assert!(closed(&mut slow).await);
        assert_eq!(begun.elapsed(), LIMITS.head);
    }

    /// A connection that never sends a byte is dropped at the head deadline too.
    #[tokio::test(start_paused = true)]
    async fn a_silent_connection_is_dropped_at_the_head_deadline() {
        let server = start(LIMITS);
        let mut silent = server.connect(A).await;
        let begun = Instant::now();
        assert!(closed(&mut silent).await);
        assert_eq!(begun.elapsed(), LIMITS.head);
    }

    /// Slowloris: a byte every 100 ms never extends the deadline.
    #[tokio::test(start_paused = true)]
    async fn a_trickled_head_is_dropped_at_the_head_deadline() {
        let server = start(LIMITS);
        let mut slow = server.connect(A).await;
        let begun = Instant::now();
        let head = b"GET / HTTP/1.1\r\nhost: t\r\nx-pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n";
        let (mut reader, mut writer) = tokio::io::split(&mut slow);
        let trickle = async {
            for byte in head {
                if writer.write_all(&[*byte]).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let mut buf = [0u8; 64];
        tokio::select! {
            () = trickle => panic!("the whole head was accepted"),
            read = reader.read(&mut buf) => assert!(matches!(read, Ok(0) | Err(_))),
        }
        assert_eq!(begun.elapsed(), LIMITS.head);
    }

    /// Keep-alive reuse works inside the idle window, and the connection closes at its
    /// end. A request in flight, or a body still streaming, is never cut.
    #[tokio::test(start_paused = true)]
    async fn keep_alive_lasts_the_idle_window_and_requests_are_not_cut() {
        let server = start(LIMITS);
        let mut conn = server.connect(A).await;
        assert!(served(&mut conn).await);
        tokio::time::sleep(LIMITS.idle.saturating_sub(Duration::from_millis(500))).await;
        assert!(served(&mut conn).await, "reused inside the idle window");
        let idle = Instant::now();
        assert!(closed(&mut conn).await);
        assert_eq!(idle.elapsed(), LIMITS.idle);

        let mut conn = server.connect(A).await;
        let begun = Instant::now();
        conn.write_all(b"GET /slow HTTP/1.1\r\nhost: t\r\n\r\n")
            .await
            .unwrap();
        assert!(read_until(&mut conn, "slow").await.ends_with("slow"));
        assert_eq!(
            begun.elapsed(),
            Duration::from_secs(5),
            "outlived both deadlines"
        );
        // A streamed body takes 6 s, past both deadlines, and arrives whole.
        conn.write_all(b"GET /stream HTTP/1.1\r\nhost: t\r\n\r\n")
            .await
            .unwrap();
        let body = read_until(&mut conn, "\r\n0\r\n\r\n").await;
        assert_eq!(body.matches("frame;").count(), 3, "{body}");
        assert!(body.ends_with("\r\n0\r\n\r\n"), "{body}");
    }

    /// Past its cap a client's new connection is closed unserved while its open ones
    /// and other clients are served. The slots return as connections close.
    #[tokio::test(start_paused = true)]
    async fn one_client_cannot_hold_every_connection() {
        let server = start(LIMITS);
        let mut held = Vec::new();
        for _ in 0..LIMITS.per_client {
            let mut conn = server.connect(A).await;
            assert!(served(&mut conn).await);
            held.push(conn);
        }
        let mut extra = server.connect(A).await;
        let begun = Instant::now();
        assert!(!served(&mut extra).await, "over the cap");
        assert_eq!(begun.elapsed(), Duration::ZERO, "closed at accept");
        assert!(served(&mut held[0]).await, "kept alive");
        let mut other = server.connect(B).await;
        assert!(served(&mut other).await, "another client is served");
        drop(held);
        tokio::task::yield_now().await;
        let mut again = server.connect(A).await;
        assert!(served(&mut again).await, "slots returned");
    }

    /// A client at its cap with stalled heads delays nobody else, and its stalled
    /// connections are dropped at the head deadline.
    #[tokio::test(start_paused = true)]
    async fn another_client_is_served_beside_a_slowloris() {
        let server = start(LIMITS);
        let mut stalled = Vec::new();
        for _ in 0..LIMITS.per_client {
            let mut conn = server.connect(A).await;
            conn.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
            stalled.push(conn);
        }
        let begun = Instant::now();
        let mut other = server.connect(B).await;
        assert!(served(&mut other).await);
        assert_eq!(begun.elapsed(), Duration::ZERO);
        for mut conn in stalled {
            assert!(closed(&mut conn).await, "the slowloris is dropped");
        }
        assert_eq!(begun.elapsed(), LIMITS.head);
    }

    /// At the pool cap a new connection waits for room instead of being refused.
    #[tokio::test(start_paused = true)]
    async fn the_pool_cap_queues_new_connections() {
        let limits = ConnLimits { total: 2, ..LIMITS };
        let server = start(limits);
        let mut first = server.connect(A).await;
        let mut second = server.connect(B).await;
        assert!(served(&mut first).await && served(&mut second).await);
        let mut third = server.connect([192, 0, 2, 3]).await;
        let begun = Instant::now();
        third.write_all(GET).await.unwrap();
        drop(first);
        assert!(read_until(&mut third, "\r\n\r\nok").await.ends_with("ok"));
        assert!(
            begun.elapsed() < LIMITS.head,
            "admitted once a connection closed"
        );
    }

    /// The real listener serves and reports its peer, with no timing asserted.
    #[tokio::test]
    async fn the_tcp_listener_serves() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let guarded = Guarded::new(listener, Vec::new(), ConnLimits::default());
        let addr = guarded.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/",
                get(|ConnectInfo(peer): ConnectInfo<Peer>| async move {
                    assert!(peer.conn.is_some());
                    peer.addr.ip().to_string()
                }),
            )
            .layer(axum::middleware::from_fn(track));
        tokio::spawn(async move {
            axum::serve(guarded, app.into_make_service_with_connect_info::<Peer>()).await
        });
        let mut conn = TcpStream::connect(addr).await.unwrap();
        conn.write_all(GET).await.unwrap();
        let mut reply = String::new();
        let mut buf = [0u8; 1024];
        while !reply.contains("127.0.0.1") {
            let n = conn.read(&mut buf).await.unwrap();
            assert!(n > 0, "{reply}");
            reply.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    }
}
