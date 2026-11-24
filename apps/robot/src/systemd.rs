//! `sd_notify` for `Type=notify` with `WatchdogSec=`, without libsystemd.
//!
//! Outside systemd (`NOTIFY_SOCKET` unset) every call is a no-op.
use std::{
    io,
    os::unix::net::UnixDatagram,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// The device loop's heartbeat. The loop beats once per pass and wakes at
/// least every [`Pulse::every`], so a quiet but healthy loop still beats; the
/// watchdog is pinged only when it has.
#[derive(Clone, Debug)]
pub struct Pulse {
    beats: Arc<AtomicU64>,
    every: Duration,
}

impl Pulse {
    pub(crate) fn new(every: Duration) -> Self {
        Self {
            beats: Arc::default(),
            every,
        }
    }

    pub fn beat(&self) {
        self.beats.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn beats(&self) -> u64 {
        self.beats.load(Ordering::Relaxed)
    }

    /// The longest the loop may go without a pass.
    pub fn every(&self) -> Duration {
        self.every
    }
}

/// Tell systemd the service is up and, when the unit sets `WatchdogSec=`,
/// return the heartbeat the device loop must keep and start pinging the
/// watchdog at half the interval, each time only if the loop beat since the
/// last ping. A wedged loop (an await that never returns, a blocked runtime)
/// stops the pings, and systemd restarts the service.
pub fn ready() -> Option<Pulse> {
    let socket = std::env::var_os("NOTIFY_SOCKET")?;
    let socket = socket.to_string_lossy().into_owned();
    if let Err(error) = notify_to(&socket, "READY=1") {
        tracing::warn!(%error, "systemd readiness notification failed");
        return None;
    }
    let interval = watchdog_interval(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    )?;
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    // Two loop passes per ping, four per watchdog interval.
    let pulse = Pulse::new(interval / 4);
    runtime.spawn(watch(pulse.clone(), interval / 2, socket));
    Some(pulse)
}

/// Ping `socket` every `period` while `pulse` keeps beating.
async fn watch(pulse: Pulse, period: Duration, socket: String) {
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    let mut seen = 0;
    loop {
        ticks.tick().await;
        let beats = pulse.beats.load(Ordering::Relaxed);
        if beats == seen {
            tracing::warn!("the device loop made no progress; watchdog not pinged");
            continue;
        }
        seen = beats;
        if let Err(error) = notify_to(&socket, "WATCHDOG=1") {
            tracing::debug!(%error, "systemd watchdog ping failed");
        }
    }
}

/// The watchdog interval systemd asked for, if it is meant for this process.
fn watchdog_interval(usec: Option<&str>, pid: Option<&str>, own: u32) -> Option<Duration> {
    let usec: u64 = usec?.parse().ok().filter(|v| *v > 0)?;
    if let Some(pid) = pid
        && pid.parse::<u32>().ok() != Some(own)
    {
        return None;
    }
    Some(Duration::from_micros(usec))
}

/// Send one notification datagram. `@name` is a Linux abstract socket.
pub fn notify_to(socket: &str, message: &str) -> io::Result<()> {
    let sender = UnixDatagram::unbound()?;
    if let Some(name) = socket.strip_prefix('@') {
        return send_abstract(&sender, name, message);
    }
    sender.send_to(message.as_bytes(), socket).map(drop)
}

#[cfg(target_os = "linux")]
fn send_abstract(sender: &UnixDatagram, name: &str, message: &str) -> io::Result<()> {
    use std::os::{linux::net::SocketAddrExt, unix::net::SocketAddr};
    let address = SocketAddr::from_abstract_name(name.as_bytes())?;
    sender.send_to_addr(message.as_bytes(), &address).map(drop)
}

#[cfg(not(target_os = "linux"))]
fn send_abstract(_: &UnixDatagram, _: &str, _: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "abstract notify sockets need Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_interval_honors_pid_and_zero() {
        let own = 4242;
        assert_eq!(
            watchdog_interval(Some("60000000"), None, own),
            Some(Duration::from_mins(1))
        );
        assert_eq!(
            watchdog_interval(Some("60000000"), Some("4242"), own),
            Some(Duration::from_mins(1))
        );
        assert_eq!(watchdog_interval(Some("60000000"), Some("1"), own), None);
        assert_eq!(watchdog_interval(Some("0"), None, own), None);
        assert_eq!(watchdog_interval(Some("soon"), None, own), None);
        assert_eq!(watchdog_interval(None, None, own), None);
    }

    /// Pings follow the heartbeat: none while the loop is silent, one per
    /// period once it beats again.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_is_pinged_only_while_the_loop_beats() {
        let path = std::env::temp_dir().join(format!("buddy-watchdog-{}", uuid::Uuid::new_v4()));
        let listener = UnixDatagram::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let pings = || {
            let mut buffer = [0u8; 64];
            let mut n = 0;
            while let Ok(len) = listener.recv(&mut buffer) {
                assert_eq!(&buffer[..len], b"WATCHDOG=1");
                n += 1;
            }
            n
        };
        let period = Duration::from_secs(30);
        let pulse = Pulse::new(period / 2);
        let task = tokio::spawn(watch(
            pulse.clone(),
            period,
            path.to_str().unwrap().to_owned(),
        ));
        tokio::time::sleep(period * 3).await;
        assert_eq!(pings(), 0, "a loop that never beats is never vouched for");
        pulse.beat();
        tokio::time::sleep(period).await;
        assert_eq!(pings(), 1);
        tokio::time::sleep(period * 2).await;
        assert_eq!(pings(), 0, "wedged again");
        task.abort();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn notifications_reach_the_socket() {
        let path = std::env::temp_dir().join(format!("buddy-notify-{}", uuid::Uuid::new_v4()));
        let listener = UnixDatagram::bind(&path).unwrap();
        notify_to(path.to_str().unwrap(), "READY=1").unwrap();
        notify_to(path.to_str().unwrap(), "WATCHDOG=1").unwrap();
        let mut buffer = [0u8; 64];
        let n = listener.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"READY=1");
        let n = listener.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"WATCHDOG=1");
        std::fs::remove_file(path).unwrap();
    }
}
