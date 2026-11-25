//! How long to wait before relaunching a helper process (the service's input helper, the agent's
//! video process): doubling while it keeps failing quickly, back to the start once a run lasted.

use std::time::Duration;

pub struct Backoff {
    wait: Duration,
}

impl Backoff {
    const FIRST: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(60);
    /// A run at least this long was healthy, however it ended.
    pub const HEALTHY: Duration = Duration::from_secs(30);

    pub fn new() -> Backoff {
        Backoff { wait: Self::FIRST }
    }

    /// A run of `ran` ended by itself (`Duration::ZERO` for a launch that failed): how long to
    /// wait before the next.
    pub fn failed_after(&mut self, ran: Duration) -> Duration {
        self.wait = if ran >= Self::HEALTHY { Self::FIRST } else { (self.wait * 2).min(Self::MAX) };
        self.wait
    }

    /// A run of `ran` was stopped on purpose (not a failure): the next one starts at once, and
    /// a healthy run forgives earlier failures.
    #[cfg_attr(not(windows), allow(dead_code))] // only the Windows service stops its helper
    pub fn stopped_after(&mut self, ran: Duration) {
        if ran >= Self::HEALTHY {
            self.wait = Self::FIRST;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_failures_back_off_to_a_minute_and_a_healthy_run_starts_over() {
        let mut b = Backoff::new();
        let waits: Vec<u64> = (0..8).map(|_| b.failed_after(Duration::from_secs(2)).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(b.failed_after(Backoff::HEALTHY), Duration::from_secs(1));
        assert_eq!(b.failed_after(Duration::ZERO), Duration::from_secs(2), "a failed launch counts as a quick failure");
    }

    #[test]
    fn stopping_a_healthy_run_forgives_but_stopping_a_short_one_does_not() {
        let mut b = Backoff::new();
        b.failed_after(Duration::ZERO);
        b.failed_after(Duration::ZERO);
        b.stopped_after(Duration::from_secs(5));
        assert_eq!(b.failed_after(Duration::ZERO), Duration::from_secs(8));
        b.stopped_after(Duration::from_secs(40));
        assert_eq!(b.failed_after(Duration::ZERO), Duration::from_secs(2));
    }
}
