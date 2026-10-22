//! Bandwidth and loss estimation.
//!
//! The estimator drives two decisions: the target bitrate and how much FEC
//! parity to send. It uses two independent signals, as required:
//!
//! - the **delay gradient**: when the delay of a packet grows relative to
//!   the minimum observed delay, the path is building a queue and the target
//!   rate must come down;
//! - the **loss rate**: persistent loss means the rate is above what the
//!   path can carry.
//!
//! The rate steps up slowly (at most 10 % per window) and down quickly
//! (multiplicative), which is the behaviour a low-latency link wants.

use std::collections::VecDeque;
use std::time::Duration;

/// A bandwidth/loss estimator.
#[derive(Debug)]
pub struct Estimator {
    /// Minimum observed round trip time.
    min_rtt: Option<Duration>,
    /// Smoothed round trip time.
    smoothed_rtt: Duration,
    /// Recent round trip samples.
    samples: VecDeque<Duration>,
    /// Number of packets sent in the current window.
    sent: u64,
    /// Number of packets acknowledged in the current window.
    acked: u64,
    /// Number of packets reported lost in the current window.
    lost: u64,
    /// Bytes sent in the current window.
    bytes: u64,
    /// Current target rate in bytes per second.
    target_rate: f64,
    /// Minimum target rate.
    min_rate: f64,
    /// Maximum target rate.
    max_rate: f64,
    /// Length of an estimation window.
    window: Duration,
    /// Time the current window started.
    window_start: std::time::Instant,
}

impl Estimator {
    /// Create an estimator with the given bounds.
    #[must_use]
    pub fn new(min_rate: f64, max_rate: f64, window: Duration) -> Self {
        Self {
            min_rtt: None,
            smoothed_rtt: Duration::ZERO,
            samples: VecDeque::new(),
            sent: 0,
            acked: 0,
            lost: 0,
            bytes: 0,
            target_rate: min_rate.max(1.0),
            min_rate,
            max_rate,
            window,
            window_start: std::time::Instant::now(),
        }
    }

    /// Set the starting target rate.
    ///
    /// The bandwidth policy starts from the bitrate the session was
    /// configured with and adapts from there, rather than from the floor.
    pub fn set_initial_rate(&mut self, rate: f64) {
        self.target_rate = rate.clamp(self.min_rate, self.max_rate);
    }

    /// Record that a packet was sent.
    pub fn on_send(&mut self, bytes: usize) {
        self.sent += 1;
        self.bytes += bytes as u64;
    }

    /// Record an acknowledgement with the round trip time of the packet.
    pub fn on_ack(&mut self, rtt: Duration) {
        self.acked += 1;
        self.samples.push_back(rtt);
        if self.samples.len() > 64 {
            self.samples.pop_front();
        }
        let min = self.min_rtt.map_or(rtt, |m| m.min(rtt));
        self.min_rtt = Some(min);
        // Exponential smoothing with a 1/8 weight.
        let alpha = 0.125;
        let rtt_s = rtt.as_secs_f64();
        let sm = self.smoothed_rtt.as_secs_f64();
        self.smoothed_rtt = Duration::from_secs_f64(sm * (1.0 - alpha) + rtt_s * alpha);
    }

    /// Record a lost packet.
    pub fn on_loss(&mut self) {
        self.lost += 1;
    }

    /// Current measured loss fraction over the current window.
    #[must_use]
    pub fn loss_rate(&self) -> f64 {
        let total = self.acked + self.lost;
        if total == 0 {
            return 0.0;
        }
        self.lost as f64 / total as f64
    }

    /// Current delay gradient: how far the smoothed RTT is above the
    /// minimum. Zero means no queueing.
    #[must_use]
    pub fn delay_gradient(&self) -> f64 {
        match self.min_rtt {
            Some(min) if !min.is_zero() => {
                let min_s = min.as_secs_f64();
                ((self.smoothed_rtt.as_secs_f64() - min_s) / min_s).max(0.0)
            }
            _ => 0.0,
        }
    }

    /// The current target rate in bytes per second.
    #[must_use]
    pub fn target_rate(&self) -> f64 {
        self.target_rate
    }

    /// The minimum observed round trip time.
    #[must_use]
    pub fn min_rtt(&self) -> Option<Duration> {
        self.min_rtt
    }

    /// The smoothed round trip time.
    #[must_use]
    pub fn smoothed_rtt(&self) -> Duration {
        self.smoothed_rtt
    }

    /// Close the current window and update the target rate.
    ///
    /// Returns the new target rate in bytes per second.
    pub fn on_window_close(&mut self) -> f64 {
        let loss = self.loss_rate();
        let gradient = self.delay_gradient();

        if loss > 0.02 {
            // Persistent loss: cut the rate hard.
            self.target_rate = (self.target_rate * 0.7).max(self.min_rate);
        } else if gradient > 0.25 {
            // Queue growth: the path is saturated, come down.
            self.target_rate = (self.target_rate * 0.85).max(self.min_rate);
        } else if loss < 0.01 && gradient < 0.1 {
            // Healthy path: probe upwards, at most 10 % per window.
            self.target_rate = (self.target_rate * 1.1).min(self.max_rate);
        }

        self.sent = 0;
        self.acked = 0;
        self.lost = 0;
        self.bytes = 0;
        self.window_start = std::time::Instant::now();
        self.target_rate
    }

    /// Whether the current window has elapsed.
    #[must_use]
    pub fn window_elapsed(&self) -> bool {
        self.window_start.elapsed() >= self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_rate_is_kept() {
        let mut e = Estimator::new(1000.0, 1e9, Duration::from_secs(1));
        e.set_initial_rate(5_000_000.0);
        assert_eq!(e.target_rate(), 5_000_000.0);
        // The rate never leaves the bounds.
        e.set_initial_rate(1.0);
        assert_eq!(e.target_rate(), 1000.0);
        e.set_initial_rate(1e12);
        assert_eq!(e.target_rate(), 1e9);
    }

    #[test]
    fn loss_rate_is_zero_without_traffic() {
        let e = Estimator::new(1000.0, 1e9, Duration::from_secs(1));
        assert_eq!(e.loss_rate(), 0.0);
    }

    #[test]
    fn loss_rate_counts_losses() {
        let mut e = Estimator::new(1000.0, 1e9, Duration::from_secs(1));
        e.on_ack(Duration::from_millis(1));
        e.on_loss();
        e.on_loss();
        assert!((e.loss_rate() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn rate_steps_up_slowly() {
        let mut e = Estimator::new(1000.0, 1e9, Duration::from_secs(1));
        let before = e.target_rate();
        e.on_ack(Duration::from_millis(1));
        let after = e.on_window_close();
        assert!(after > before);
        assert!(after <= before * 1.1001);
    }

    #[test]
    fn rate_drops_on_loss() {
        let mut e = Estimator::new(1000.0, 1e9, Duration::from_secs(1));
        let mut rate = 100_000.0;
        for _ in 0..10 {
            e.on_ack(Duration::from_millis(1));
            e.on_loss();
            e.on_loss();
            e.on_loss();
            rate = e.on_window_close();
        }
        assert!(rate < 100_000.0);
    }

    #[test]
    fn rate_drops_when_queue_grows() {
        let mut e = Estimator::new(1000.0, 1e9, Duration::from_secs(1));
        e.on_ack(Duration::from_millis(1));
        e.on_ack(Duration::from_millis(100));
        e.on_ack(Duration::from_millis(100));
        let after = e.on_window_close();
        assert!(after < 1_100.0 * 1.1001);
    }

    #[test]
    fn delay_gradient_detects_queueing() {
        let mut e = Estimator::new(1000.0, 1e9, Duration::from_secs(1));
        e.on_ack(Duration::from_millis(1));
        assert!(e.delay_gradient() < 0.01);
        for _ in 0..20 {
            e.on_ack(Duration::from_millis(50));
        }
        assert!(e.delay_gradient() > 0.1);
    }
}
