//! How long to wait before dialing a machine again.

use std::time::Duration;

/// The first wait: short enough that a machine whose core restarted, or that
/// was just updated, is back at once.
const BASE: Duration = Duration::from_millis(250);

/// The longest wait between dials; `HC_RECONNECT_MS` sets it.
fn reconnect_max() -> Duration {
    Duration::from_millis(std::env::var("HC_RECONNECT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30_000))
}

/// The wait between dials. It grows while lines keep ending soon after they
/// connect, so a path that goes quiet every silence window is not redialed
/// forever, and starts over once a line stayed up long enough to trust.
pub(super) struct Backoff {
    wait: Duration,
}

impl Backoff {
    pub(super) fn new() -> Self {
        Self { wait: BASE }
    }

    /// The wait before the next dial after one failed. It grows, and a failure
    /// never counts as a line worth trusting.
    pub(super) fn failed(&mut self) -> Duration {
        self.next()
    }

    /// The wait after a line ended; `None` dials at once. A line the machine
    /// closed says it is there (a restart, a retired line); a quiet one that
    /// died young may be flapping, so its wait grows.
    pub(super) fn ended(&mut self, quiet: bool, lived: Duration, healthy: Duration) -> Option<Duration> {
        if !quiet || lived >= healthy {
            self.wait = BASE;
            return None;
        }
        Some(self.next())
    }

    fn next(&mut self) -> Duration {
        let wait = self.wait;
        self.wait = (self.wait * 2).min(reconnect_max());
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_line_that_keeps_going_quiet_waits_longer_each_time() {
        let mut backoff = Backoff::new();
        let waits: Vec<_> = (0..4).map(|_| backoff.ended(true, secs(45), secs(90))).collect();
        let expected: Vec<_> = [250, 500, 1000, 2000].map(|ms| Some(Duration::from_millis(ms))).to_vec();
        assert_eq!(waits, expected);
    }

    #[test]
    fn a_line_that_stayed_up_or_was_closed_dials_at_once_and_starts_over() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.ended(true, secs(45), secs(90)), Some(Duration::from_millis(250)));
        assert_eq!(backoff.ended(true, secs(44), secs(90)), Some(Duration::from_millis(500)));
        // A restart there closes the line: the machine answered, so dial at once.
        assert_eq!(backoff.ended(false, secs(1), secs(90)), None);
        assert_eq!(backoff.ended(true, secs(45), secs(90)), Some(Duration::from_millis(250)));
        // A line that outlived the flapping window starts over too.
        assert_eq!(backoff.ended(true, secs(44), secs(90)), Some(Duration::from_millis(500)));
        assert_eq!(backoff.ended(true, secs(120), secs(90)), None);
        assert_eq!(backoff.ended(true, secs(45), secs(90)), Some(Duration::from_millis(250)));
    }

    #[test]
    fn a_failed_dial_keeps_growing_the_wait_until_a_line_is_trusted() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.failed(), Duration::from_millis(250));
        assert_eq!(backoff.failed(), Duration::from_millis(500));
        assert_eq!(backoff.failed(), Duration::from_millis(1000));
        assert_eq!(backoff.ended(false, Duration::ZERO, secs(90)), None);
        assert_eq!(backoff.failed(), Duration::from_millis(250));
    }
}
