//! `sd_notify` for `Type=notify` with `WatchdogSec=`, without libsystemd.
//!
//! Outside systemd (`NOTIFY_SOCKET` unset) every call is a no-op.
use std::{io, os::unix::net::UnixDatagram, time::Duration};

/// Tell systemd the service is up and, when the unit sets `WatchdogSec=`,
/// start pinging its watchdog from the runtime at half the interval.
///
/// The ping task proves the Tokio runtime still schedules timers and runs
/// tasks (a deadlocked or fully blocked runtime stops it, and systemd then
/// restarts the service). It does not prove the session loop is making
/// progress.
pub fn ready() {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let socket = socket.to_string_lossy().into_owned();
    if let Err(error) = notify_to(&socket, "READY=1") {
        tracing::warn!(%error, "systemd readiness notification failed");
        return;
    }
    let interval = watchdog_interval(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    );
    if let (Some(interval), Ok(runtime)) = (interval, tokio::runtime::Handle::try_current()) {
        runtime.spawn(async move {
            let mut ticks = tokio::time::interval(interval / 2);
            loop {
                ticks.tick().await;
                if let Err(error) = notify_to(&socket, "WATCHDOG=1") {
                    tracing::debug!(%error, "systemd watchdog ping failed");
                }
            }
        });
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
