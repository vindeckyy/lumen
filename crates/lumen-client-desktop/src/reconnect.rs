//! Unattended reconnect.
//!
//! A client that loses the host has to come back on its own: nobody is
//! there to press a key. The plan asks for a backoff from 200 ms to 2 s and
//! for a full reconnect within 2 s, measured from the moment the link is
//! up again.
//!
//! The backoff doubles, so a host that is down does not get hammered, and
//! the delay is capped, so a client that waits for a long time does not
//! wait for an hour.

use std::time::{Duration, Instant};

/// The smallest delay between two attempts.
pub const MIN_BACKOFF: Duration = Duration::from_millis(200);

/// The largest delay between two attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// The policy of the reconnect loop.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconnectPolicy {
    /// The delay before the first attempt.
    pub min_delay: Duration,
    /// The longest delay between two attempts.
    pub max_delay: Duration,
    /// The factor the delay grows by.
    pub factor: f64,
    /// How many attempts are made before the client gives up. `None` means
    /// it keeps trying, which is what an unattended client does.
    pub max_attempts: Option<u32>,
    /// How long the client waits for the whole reconnect, from the moment
    /// the link came up.
    pub reconnect_deadline: Option<Duration>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            min_delay: MIN_BACKOFF,
            max_delay: MAX_BACKOFF,
            factor: 2.0,
            max_attempts: None,
            reconnect_deadline: Some(Duration::from_secs(2)),
        }
    }
}

impl ReconnectPolicy {
    /// The delay before the attempt that follows `attempt` failed attempts.
    ///
    /// The first attempt waits the shortest delay, and the delay doubles
    /// until it reaches the cap. The number is the index of the retry, so
    /// zero is the first retry.
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return self.min_delay.min(self.max_delay);
        }
        let growth = self.factor.powi(i32::try_from(attempt).unwrap_or(i32::MAX));
        let millis = self.min_delay.as_millis() as f64 * growth;
        let capped = millis.min(self.max_delay.as_millis() as f64);
        Duration::from_millis(capped.max(1.0) as u64)
    }

    /// Whether another attempt is allowed.
    #[must_use]
    pub fn allows(&self, attempt: u32) -> bool {
        match self.max_attempts {
            Some(limit) => attempt < limit,
            None => true,
        }
    }

    /// The whole backoff schedule, for the plan's 200 ms to 2 s rule.
    #[must_use]
    pub fn schedule(&self, attempts: u32) -> Vec<Duration> {
        (0..attempts)
            .map(|attempt| self.delay_for(attempt))
            .collect()
    }
}

/// The counters of the reconnect loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconnectStats {
    /// How many attempts were made.
    pub attempts: u32,
    /// How many of them failed.
    pub failures: u32,
    /// How many sessions were established.
    pub established: u64,
    /// The delay before the last attempt.
    pub last_delay: Duration,
    /// The time the last successful reconnect took, from the first attempt.
    pub last_reconnect: Option<Duration>,
}

/// The reconnect loop of the client.
///
/// The loop asks the caller to try, waits the delay the policy names and
/// counts what happened. The caller does the actual connecting, so the
/// policy can be tested without a network.
#[derive(Debug)]
pub struct Reconnector {
    policy: ReconnectPolicy,
    stats: ReconnectStats,
    started: Option<Instant>,
}

impl Reconnector {
    /// A loop with the given policy.
    #[must_use]
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self {
            policy,
            stats: ReconnectStats::default(),
            started: None,
        }
    }

    /// The policy.
    #[must_use]
    pub fn policy(&self) -> &ReconnectPolicy {
        &self.policy
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> ReconnectStats {
        self.stats
    }

    /// The delay before the next attempt.
    ///
    /// The first retry waits the shortest delay, and the delay doubles until
    /// it reaches the cap.
    #[must_use]
    pub fn next_delay(&self) -> Duration {
        let retries = self.stats.failures.saturating_sub(1);
        self.policy.delay_for(retries)
    }

    /// Whether another attempt is allowed.
    #[must_use]
    pub fn should_retry(&self) -> bool {
        self.policy.allows(self.stats.attempts)
    }

    /// Record that an attempt was made and failed.
    pub fn record_failure(&mut self) -> Duration {
        if self.started.is_none() {
            self.started = Some(Instant::now());
        }
        self.stats.attempts += 1;
        self.stats.failures += 1;
        let delay = self.next_delay();
        self.stats.last_delay = delay;
        delay
    }

    /// Record that an attempt succeeded.
    pub fn record_success(&mut self) {
        self.stats.attempts += 1;
        self.stats.established += 1;
        self.stats.last_reconnect = self.started.map(|started| started.elapsed());
        self.started = None;
    }

    /// Forget the failures, so the next delay is the shortest one again.
    pub fn reset(&mut self) {
        self.stats.failures = 0;
        self.started = None;
    }
}

/// Whether a reconnect met the plan's limit.
///
/// The plan wants a full reconnect within 2 s, measured from the moment the
/// link is up.
#[must_use]
pub fn within_limit(elapsed: Duration, limit: Duration) -> bool {
    elapsed <= limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_starts_at_200_ms_and_doubles() {
        let policy = ReconnectPolicy::default();
        assert_eq!(policy.delay_for(0), Duration::from_millis(200));
        assert_eq!(policy.delay_for(1), Duration::from_millis(400));
        assert_eq!(policy.delay_for(2), Duration::from_millis(800));
        assert_eq!(policy.delay_for(3), Duration::from_millis(1600));
    }

    #[test]
    fn the_backoff_stops_at_two_seconds() {
        let policy = ReconnectPolicy::default();
        assert_eq!(policy.delay_for(4), Duration::from_secs(2));
        assert_eq!(policy.delay_for(40), Duration::from_secs(2));
        let schedule = policy.schedule(8);
        assert_eq!(schedule.len(), 8);
        assert!(schedule.iter().all(|delay| *delay <= MAX_BACKOFF));
        assert!(schedule.iter().all(|delay| *delay >= MIN_BACKOFF));
    }

    #[test]
    fn the_first_attempt_after_a_failure_waits_the_short_delay() {
        let mut reconnector = Reconnector::new(ReconnectPolicy::default());
        let delay = reconnector.record_failure();
        assert_eq!(delay, Duration::from_millis(200));
        let delay = reconnector.record_failure();
        assert_eq!(delay, Duration::from_millis(400));
        assert_eq!(reconnector.stats().attempts, 2);
        assert_eq!(reconnector.stats().failures, 2);
    }

    #[test]
    fn a_successful_attempt_ends_the_loop() {
        let mut reconnector = Reconnector::new(ReconnectPolicy::default());
        reconnector.record_failure();
        reconnector.record_success();
        let stats = reconnector.stats();
        assert_eq!(stats.established, 1);
        assert_eq!(stats.attempts, 2);
        assert!(stats.last_reconnect.is_some());
    }

    #[test]
    fn a_policy_with_a_limit_stops_the_loop() {
        let policy = ReconnectPolicy {
            max_attempts: Some(3),
            ..ReconnectPolicy::default()
        };
        let mut reconnector = Reconnector::new(policy);
        assert!(reconnector.should_retry());
        reconnector.record_failure();
        assert!(reconnector.should_retry());
        reconnector.record_failure();
        // The third attempt is still allowed.
        assert!(reconnector.should_retry());
        reconnector.record_failure();
        assert!(!reconnector.should_retry());
    }

    #[test]
    fn an_unattended_client_keeps_trying() {
        let reconnector = Reconnector::new(ReconnectPolicy::default());
        assert!(reconnector.should_retry());
        assert!(reconnector.policy().max_attempts.is_none());
    }

    #[test]
    fn the_reconnect_limit_is_two_seconds() {
        assert!(within_limit(
            Duration::from_millis(1999),
            Duration::from_secs(2)
        ));
        assert!(!within_limit(
            Duration::from_millis(2001),
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn a_reset_starts_the_backoff_over() {
        let mut reconnector = Reconnector::new(ReconnectPolicy::default());
        reconnector.record_failure();
        reconnector.record_failure();
        assert_eq!(reconnector.next_delay(), Duration::from_millis(400));
        reconnector.reset();
        assert_eq!(reconnector.next_delay(), Duration::from_millis(200));
    }
}
