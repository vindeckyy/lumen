//! Mode changes and the encoder reconfiguration they need.
//!
//! When the output changes mode the frames change size. The encoder is
//! configured for one size, so it has to be rebuilt, and the client has to
//! rebuild its surface. The host watches the size of the output it captures
//! and reports every change; the client is told over the control channel
//! with the existing [`ControlMessage::ResolutionChanged`] message.
//!
//! The size the host watches is the size of the frames the capture produces,
//! not the size of the output: the capture is allowed to scale, and the
//! encoder has to match whatever the capture delivers.

use std::time::{Duration, Instant};

/// The size of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    /// The width in pixels.
    pub width: u32,
    /// The height in pixels.
    pub height: u32,
}

impl Resolution {
    /// The size, checked against the limits of the protocol.
    ///
    /// # Errors
    /// Returns a description when a dimension is zero.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err("a resolution must have a non-zero width and height".to_string());
        }
        Ok(Self { width, height })
    }

    /// Whether the resolution differs from `other`.
    #[must_use]
    pub fn differs_from(&self, other: &Self) -> bool {
        self.width != other.width || self.height != other.height
    }

    /// The number of pixels of a frame.
    #[must_use]
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// A mode change the host saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionChange {
    /// The size before the change.
    pub previous: Resolution,
    /// The size after the change.
    pub current: Resolution,
}

impl ResolutionChange {
    /// Whether the encoder has to be rebuilt for this change.
    ///
    /// The encoder is configured for one size, so any change of size needs a
    /// new configuration and a new session.
    #[must_use]
    pub fn needs_new_encoder(&self) -> bool {
        self.previous.differs_from(&self.current)
    }
}

/// Watches the size of the captured frames.
///
/// The first observation is the size the session starts at, not a change.
#[derive(Debug, Default)]
pub struct ResizeWatcher {
    current: Option<Resolution>,
    changes: u64,
}

impl ResizeWatcher {
    /// A watcher that has seen nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the size of a frame and report the change when it moved.
    pub fn observe(&mut self, current: Resolution) -> Option<ResolutionChange> {
        let Some(previous) = self.current else {
            self.current = Some(current);
            return None;
        };
        if !previous.differs_from(&current) {
            return None;
        }
        self.current = Some(current);
        self.changes += 1;
        Some(ResolutionChange { previous, current })
    }

    /// The size the watcher last saw.
    #[must_use]
    pub fn current(&self) -> Option<Resolution> {
        self.current
    }

    /// How many changes the watcher saw.
    #[must_use]
    pub fn changes(&self) -> u64 {
        self.changes
    }
}

/// What the capture stage has to do after it failed.
///
/// A compositor that restarts leaves the capture with a dead connection. The
/// capture has to be reopened, and the encoder needs a keyframe afterwards,
/// because the new connection starts a new stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureRecovery {
    /// Try the capture again without reopening it.
    Retry,
    /// Drop the capture and open a new one.
    Reopen,
    /// Give up.
    Stop,
}

/// The policy of the capture stage after a failure.
///
/// The delay between the attempts grows, so a compositor that is not coming
/// back does not spin the CPU.
#[derive(Debug, Clone)]
pub struct RecoveryPolicy {
    /// The delay before the first retry.
    pub initial_delay: Duration,
    /// The longest delay between two attempts.
    pub max_delay: Duration,
    /// The factor the delay grows by.
    pub factor: f64,
    /// How many attempts are made before the capture gives up. `None` means
    /// it never gives up, which is what a host wants: the compositor may be
    /// restarted at any time.
    pub max_attempts: Option<u32>,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
            factor: 2.0,
            max_attempts: None,
        }
    }
}

impl RecoveryPolicy {
    /// The delay before the attempt that follows `attempt` failed attempts.
    #[must_use]
    pub fn delay_after(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return self.initial_delay;
        }
        let growth = self.factor.powi(i32::try_from(attempt).unwrap_or(i32::MAX));
        let millis = self.initial_delay.as_millis() as f64 * growth;
        let capped = millis.min(self.max_delay.as_millis() as f64);
        Duration::from_millis(capped.max(1.0) as u64)
    }

    /// What to do after `attempt` failed attempts.
    #[must_use]
    pub fn action_after(&self, attempt: u32) -> CaptureRecovery {
        match self.max_attempts {
            Some(limit) if attempt >= limit => CaptureRecovery::Stop,
            // The first failure is usually a transient hiccup; a later one
            // means the connection is gone.
            _ if attempt == 0 => CaptureRecovery::Retry,
            _ => CaptureRecovery::Reopen,
        }
    }

    /// The moment the next attempt is due.
    #[must_use]
    pub fn next_attempt(&self, attempt: u32, now: Instant) -> Instant {
        now + self.delay_after(attempt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_observation_is_not_a_change() {
        let mut watcher = ResizeWatcher::new();
        let size = Resolution::new(1920, 1080).unwrap();
        assert!(watcher.observe(size).is_none());
        assert_eq!(watcher.changes(), 0);
        assert_eq!(watcher.current(), Some(size));
    }

    #[test]
    fn a_change_is_reported_once() {
        let mut watcher = ResizeWatcher::new();
        watcher.observe(Resolution::new(1920, 1080).unwrap());
        let change = watcher
            .observe(Resolution::new(2560, 1440).unwrap())
            .expect("the size moved");
        assert_eq!(change.previous, Resolution::new(1920, 1080).unwrap());
        assert_eq!(change.current, Resolution::new(2560, 1440).unwrap());
        assert!(change.needs_new_encoder());
        assert_eq!(watcher.changes(), 1);

        // The same size again is not a change.
        assert!(
            watcher
                .observe(Resolution::new(2560, 1440).unwrap())
                .is_none()
        );
        assert_eq!(watcher.changes(), 1);

        // And back again.
        assert!(
            watcher
                .observe(Resolution::new(1920, 1080).unwrap())
                .is_some()
        );
        assert_eq!(watcher.changes(), 2);
    }

    #[test]
    fn a_zero_resolution_is_refused() {
        assert!(Resolution::new(0, 1080).is_err());
        assert!(Resolution::new(1920, 0).is_err());
        assert!(Resolution::new(1920, 1080).is_ok());
    }

    #[test]
    fn the_recovery_delay_grows_and_is_capped() {
        let policy = RecoveryPolicy::default();
        assert_eq!(policy.delay_after(0), Duration::from_millis(100));
        assert_eq!(policy.delay_after(1), Duration::from_millis(200));
        assert_eq!(policy.delay_after(2), Duration::from_millis(400));
        assert_eq!(policy.delay_after(20), policy.max_delay);
    }

    #[test]
    fn the_first_failure_is_a_retry_and_a_later_one_reopens() {
        let policy = RecoveryPolicy::default();
        assert_eq!(policy.action_after(0), CaptureRecovery::Retry);
        assert_eq!(policy.action_after(1), CaptureRecovery::Reopen);
        assert_eq!(policy.action_after(50), CaptureRecovery::Reopen);
    }

    #[test]
    fn a_policy_with_a_limit_gives_up() {
        let policy = RecoveryPolicy {
            max_attempts: Some(3),
            ..RecoveryPolicy::default()
        };
        assert_eq!(policy.action_after(2), CaptureRecovery::Reopen);
        assert_eq!(policy.action_after(3), CaptureRecovery::Stop);
    }

    #[test]
    fn the_next_attempt_is_in_the_future() {
        let policy = RecoveryPolicy::default();
        let now = Instant::now();
        assert!(policy.next_attempt(0, now) > now);
        assert!(policy.next_attempt(1, now) > policy.next_attempt(0, now));
    }
}
