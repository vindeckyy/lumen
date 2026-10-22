//! Packet pacing.
//!
//! A burst of UDP packets hits the switch queue and creates a spike that
//! shows up as jitter on the client. The pacer spreads the packets of one
//! frame evenly over the frame interval.

use std::time::Duration;

/// Token bucket pacer.
#[derive(Debug)]
pub struct Pacer {
    /// Current token count, in bytes.
    tokens: f64,
    /// Bucket capacity in bytes.
    capacity: f64,
    /// Bytes added per second.
    rate: f64,
    /// Timestamp of the last refill.
    last: std::time::Instant,
}

impl Pacer {
    /// Create a pacer with the given rate and a one-interval burst budget.
    #[must_use]
    pub fn new(rate_bytes_per_sec: f64, burst: Duration) -> Self {
        let capacity = rate_bytes_per_sec * burst.as_secs_f64();
        Self {
            tokens: capacity,
            capacity,
            rate: rate_bytes_per_sec,
            last: std::time::Instant::now(),
        }
    }

    /// Change the target rate, keeping the burst capacity proportional.
    pub fn set_rate(&mut self, rate_bytes_per_sec: f64, burst: Duration) {
        self.capacity = rate_bytes_per_sec * burst.as_secs_f64();
        self.rate = rate_bytes_per_sec;
        self.tokens = self.tokens.min(self.capacity);
    }

    /// Current rate in bytes per second.
    #[must_use]
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// Refill the bucket and take `bytes` tokens.
    ///
    /// Returns the time the caller should wait before sending, so that the
    /// packet leaves at the paced rate. A wait of zero means "send now".
    pub fn take(&mut self, bytes: usize) -> Duration {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.tokens -= bytes as f64;
        if self.tokens >= 0.0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(-self.tokens / self.rate)
    }

    /// Tokens currently available.
    #[must_use]
    pub fn available(&self) -> f64 {
        self.tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_packet_goes_out_immediately() {
        let mut p = Pacer::new(1_000_000.0, Duration::from_millis(10));
        assert_eq!(p.take(100), Duration::ZERO);
    }

    #[test]
    fn exceeding_burst_asks_to_wait() {
        let mut p = Pacer::new(1000.0, Duration::from_millis(1));
        // Burst capacity is 1 byte, so a 100 byte packet must wait.
        let d = p.take(100);
        assert!(d > Duration::ZERO);
    }

    #[test]
    fn rate_change_keeps_pacer_usable() {
        let mut p = Pacer::new(1000.0, Duration::from_millis(10));
        p.set_rate(2000.0, Duration::from_millis(10));
        assert!((p.rate() - 2000.0).abs() < f64::EPSILON);
        assert_eq!(p.take(1), Duration::ZERO);
    }
}
