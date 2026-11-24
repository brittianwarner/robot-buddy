//! Reconnect pacing and Wi-Fi recovery policy.
use rand::Rng;
use std::time::Duration;
use tokio::time::Instant;

/// A session at least this long counts as healthy and resets the backoff.
pub const HEALTHY_SESSION: Duration = Duration::from_secs(30);
/// Unhealthy attempts in a row before the Wi-Fi link is checked.
pub const LINK_CHECK_AFTER: u32 = 3;

/// Exponential backoff with jitter. Only a healthy session resets it, so a
/// server that enrolls the robot but then drops every session still backs off.
pub struct Backoff {
    failures: u32,
    base: Duration,
    max: Duration,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            failures: 0,
            base,
            max,
        }
    }

    pub fn record(&mut self, healthy: bool) {
        self.failures = if healthy {
            0
        } else {
            self.failures.saturating_add(1)
        };
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// A random delay in the upper half of the current ceiling.
    pub fn delay(&self, rng: &mut impl Rng) -> Duration {
        let exponent = self.failures.saturating_sub(1).min(16);
        let ceiling = self.base.saturating_mul(1 << exponent).min(self.max);
        let ceiling_ms = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(rng.gen_range(ceiling_ms / 2..=ceiling_ms))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LinkAction {
    Wait,
    /// No Wi-Fi for the whole grace period: reopen the setup hotspot.
    Setup,
}

/// Re-enter Wi-Fi setup only after the link has been down for `grace`, so a
/// router reboot or a brief outage never strands the robot in setup mode.
pub struct LinkWatch {
    grace: Duration,
    down_since: Option<Instant>,
}

impl LinkWatch {
    pub fn new(grace: Duration) -> Self {
        Self {
            grace,
            down_since: None,
        }
    }

    pub fn reset(&mut self) {
        self.down_since = None;
    }

    pub fn observe(&mut self, up: bool, now: Instant) -> LinkAction {
        if up {
            self.down_since = None;
            return LinkAction::Wait;
        }
        let since = *self.down_since.get_or_insert(now);
        if now.duration_since(since) >= self.grace {
            self.down_since = None;
            LinkAction::Setup
        } else {
            LinkAction::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    #[test]
    fn backoff_grows_with_jitter_and_resets_only_when_healthy() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        let mut seen = Vec::new();
        for _ in 0..8 {
            backoff.record(false);
            seen.push(backoff.delay(&mut rng));
        }
        let ceilings = [1, 2, 4, 8, 16, 30, 30, 30];
        for (delay, ceiling) in seen.iter().zip(ceilings) {
            let ceiling = Duration::from_secs(ceiling);
            assert!(*delay >= ceiling / 2 && *delay <= ceiling, "{delay:?}");
        }
        let distinct: std::collections::HashSet<_> = seen[5..].iter().collect();
        assert!(distinct.len() > 1, "jitter spreads reconnects");
        backoff.record(false);
        assert_eq!(backoff.failures(), 9);
        backoff.record(true);
        assert_eq!(backoff.failures(), 0);
        assert!(backoff.delay(&mut rng) <= Duration::from_secs(1));
    }

    #[test]
    fn setup_reopens_only_after_the_grace_period() {
        let start = Instant::now();
        let grace = Duration::from_mins(5);
        let mut link = LinkWatch::new(grace);
        assert_eq!(link.observe(false, start), LinkAction::Wait);
        let later = start + Duration::from_secs(200);
        assert_eq!(link.observe(false, later), LinkAction::Wait);
        // Link came back briefly: the clock restarts.
        assert_eq!(link.observe(true, later), LinkAction::Wait);
        assert_eq!(link.observe(false, start + grace), LinkAction::Wait);
        let gone = start + grace * 2;
        assert_eq!(link.observe(false, gone), LinkAction::Setup);
        assert_eq!(link.observe(false, gone), LinkAction::Wait);
    }
}
