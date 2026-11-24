//! A TCP listener for the robot's small local pages (the loopback panel in
//! `screen.rs` and the `SoftAP` setup portal in `wifi.rs`) that bounds what one
//! client can hold. At most [`Limits::max`] connections are open; more wait in the
//! kernel backlog. Each connection is closed after [`Limits::idle`] without a
//! byte in either direction, and after [`Limits::lifetime`] in any case, so a
//! client trickling a request header frees its slot within the lifetime.
//! `axum::serve` does not expose hyper's header-read timeout; this covers it at
//! the socket.
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
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
pub struct Limits {
    /// Open connections at once.
    pub max: usize,
    /// Longest silence in both directions.
    pub idle: Duration,
    /// Longest a connection stays open.
    pub lifetime: Duration,
}

impl Limits {
    /// The setup page and the loopback panel: one phone or kiosk, a handful of
    /// connections each, and requests that finish in milliseconds.
    pub const PAGE: Self = Self {
        max: 16,
        idle: Duration::from_secs(10),
        lifetime: Duration::from_mins(1),
    };
}

pub struct Guarded {
    listener: TcpListener,
    slots: Arc<Semaphore>,
    limits: Limits,
}

pub fn guard(listener: TcpListener, limits: Limits) -> Guarded {
    Guarded {
        listener,
        slots: Arc::new(Semaphore::new(limits.max)),
        limits,
    }
}

impl axum::serve::Listener for Guarded {
    type Io = Connection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Connection, SocketAddr) {
        // Taken before `accept`, so a full house leaves new clients in the backlog
        // instead of accepting and dropping them.
        let slot = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("the slot semaphore is never closed");
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    return (Connection::new(stream, slot, self.limits), address);
                }
                Err(error) if connection_error(&error) => {}
                // Out of descriptors and the like: back off instead of spinning.
                Err(error) => {
                    tracing::warn!(%error, "local page accept failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

fn connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// An accepted stream with its slot and deadlines. Dropping it frees the slot.
pub struct Connection {
    stream: TcpStream,
    idle_for: Duration,
    idle: Pin<Box<Sleep>>,
    end: Pin<Box<Sleep>>,
    _slot: OwnedSemaphorePermit,
}

impl Connection {
    fn new(stream: TcpStream, slot: OwnedSemaphorePermit, limits: Limits) -> Self {
        let now = Instant::now();
        Self {
            stream,
            idle_for: limits.idle,
            idle: Box::pin(tokio::time::sleep_until(now + limits.idle)),
            end: Box::pin(tokio::time::sleep_until(now + limits.lifetime)),
            _slot: slot,
        }
    }

    fn progressed(&mut self) {
        let at = Instant::now() + self.idle_for;
        self.idle.as_mut().reset(at);
    }

    fn ended(&mut self, cx: &mut Context<'_>) -> bool {
        self.end.as_mut().poll(cx).is_ready()
    }

    /// Called when the stream is pending: registers both deadlines.
    fn expired(&mut self, cx: &mut Context<'_>) -> Option<io::Error> {
        if self.ended(cx) {
            return Some(lifetime_reached());
        }
        self.idle
            .as_mut()
            .poll(cx)
            .is_ready()
            .then(|| io::Error::new(io::ErrorKind::TimedOut, "connection idle"))
    }

    fn settle<T>(
        &mut self,
        cx: &mut Context<'_>,
        polled: Poll<io::Result<T>>,
        moved: impl FnOnce(&T) -> bool,
    ) -> Poll<io::Result<T>> {
        match polled {
            Poll::Ready(Ok(value)) => {
                if moved(&value) {
                    self.progressed();
                }
                Poll::Ready(Ok(value))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => match self.expired(cx) {
                Some(error) => Poll::Ready(Err(error)),
                None => Poll::Pending,
            },
        }
    }
}

fn lifetime_reached() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connection lifetime reached")
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // A client that keeps the stream busy still meets the lifetime.
        if this.ended(cx) {
            return Poll::Ready(Err(lifetime_reached()));
        }
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.stream).poll_read(cx, buf);
        let read = buf.filled().len() - before;
        this.settle(cx, polled, |()| read > 0)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.ended(cx) {
            return Poll::Ready(Err(lifetime_reached()));
        }
        let polled = Pin::new(&mut this.stream).poll_write(cx, buf);
        this.settle(cx, polled, |written| *written > 0)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.stream).poll_flush(cx);
        this.settle(cx, polled, |()| false)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Liveness guard only: every deadline under test is far shorter.
    const GUARD: Duration = Duration::from_secs(10);

    async fn page(limits: Limits) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/", get(|| async { "ok" }));
        tokio::spawn(async move { axum::serve(guard(listener, limits), app).await });
        address
    }

    const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

    /// Read to EOF or error; true when the server answered `ok`.
    async fn answered(stream: &mut TcpStream) -> bool {
        let mut reply = Vec::new();
        let _ = tokio::time::timeout(GUARD, stream.read_to_end(&mut reply))
            .await
            .expect("the server closes the connection");
        reply.starts_with(b"HTTP/1.1 200") && reply.ends_with(b"ok")
    }

    #[tokio::test]
    async fn a_silent_or_trickling_client_is_disconnected() {
        let limits = Limits {
            max: 4,
            idle: Duration::from_millis(100),
            lifetime: Duration::from_millis(400),
        };
        let address = page(limits).await;

        // Sends nothing: closed at the idle deadline.
        let mut silent = TcpStream::connect(address).await.unwrap();
        assert!(!answered(&mut silent).await);

        // One header byte at a time, each inside the idle deadline: closed at the
        // lifetime, before the header is complete.
        let mut trickle = TcpStream::connect(address).await.unwrap();
        let mut closed = false;
        for byte in REQUEST.iter().take(REQUEST.len() - 1) {
            if trickle.write_all(&[*byte]).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert!(closed || !answered(&mut trickle).await);

        // An ordinary request is still served.
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(REQUEST).await.unwrap();
        assert!(answered(&mut client).await);
    }

    #[tokio::test]
    async fn connections_past_the_cap_wait_for_a_free_slot() {
        use axum::serve::Listener;
        let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = inner.local_addr().unwrap();
        let limits = Limits {
            max: 2,
            idle: GUARD,
            lifetime: GUARD,
        };
        let mut listener = guard(inner, limits);
        let clients: Vec<_> =
            futures_util::future::join_all((0..3).map(|_| TcpStream::connect(address))).await;
        assert!(clients.iter().all(Result::is_ok));
        let first = listener.accept().await.0;
        let _second = listener.accept().await.0;
        // The kernel completed the third handshake into the backlog; no slot is
        // free, so it is not accepted.
        let mut third = Box::pin(listener.accept());
        assert!(
            futures_util::poll!(third.as_mut()).is_pending(),
            "no slot is free"
        );
        drop(first);
        tokio::time::timeout(GUARD, third)
            .await
            .expect("a freed slot accepts the waiting client");
    }

    #[tokio::test]
    async fn a_half_sent_request_is_cut_off_and_its_slot_freed() {
        let lifetime = Duration::from_millis(200);
        let limits = Limits {
            max: 1,
            idle: lifetime,
            lifetime,
        };
        let address = page(limits).await;
        let started = std::time::Instant::now();
        let mut slow = TcpStream::connect(address).await.unwrap();
        slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        // Closed (EOF or reset) without an answer, no sooner than its deadline.
        assert!(!answered(&mut slow).await);
        assert!(started.elapsed() >= lifetime);
        // The only slot is free again: a whole request is answered.
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(REQUEST).await.unwrap();
        assert!(answered(&mut client).await);
    }
}
