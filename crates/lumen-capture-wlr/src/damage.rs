//! Copy-mode selection and frame-rate gating for screencopy.

use std::time::{Duration, Instant};

/// Whether to request `copy_with_damage` instead of `copy`.
///
/// Damage-gated copy only saves work when the compositor refreshes at least
/// 1.5x as often as the stream: below that, every refresh is needed anyway.
#[must_use]
pub fn use_damage_copy(refresh_mhz: u32, stream_fps: u32, manager_version: u32) -> bool {
    manager_version >= 2
        && refresh_mhz > 0
        && u64::from(refresh_mhz) * 2 >= u64::from(stream_fps) * 3000
}

/// Admits captured frames at most `fps` times per second.
///
/// Screencopy requests stay in flight, so the compositor copies on its own
/// refresh and a frame is never held back by a request issued late. Frames
/// that arrive inside the minimum interval are dropped.
#[derive(Debug)]
pub struct Pacer {
    interval: Duration,
    slack: Duration,
    last_emit: Option<Instant>,
}

impl Pacer {
    /// Create a gate admitting `fps` frames per second (must be non-zero).
    #[must_use]
    pub fn new(fps: u32) -> Self {
        let interval = Duration::from_secs(1) / fps;
        Self {
            interval,
            // Refresh boundaries jitter by well under a millisecond; a tenth
            // of the interval absorbs that without admitting a second frame.
            slack: interval / 10,
            last_emit: None,
        }
    }

    /// Change the rate to `fps` (zero is treated as one). The last emit is kept, so the
    /// next frame is still paced from it.
    pub fn set_fps(&mut self, fps: u32) {
        let interval = Duration::from_secs(1) / fps.max(1);
        self.interval = interval;
        self.slack = interval / 10;
    }

    /// Whether a frame captured at `now` may be emitted. Records the emit.
    pub fn admit(&mut self, now: Instant) -> bool {
        let admitted = self
            .last_emit
            .is_none_or(|t| now + self.slack >= t + self.interval);
        if admitted {
            self.last_emit = Some(now);
        }
        admitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_copy_threshold_is_one_and_a_half() {
        // 60 Hz output, 40 fps stream: exactly 1.5x, so damage copy.
        assert!(use_damage_copy(60_000, 40, 2));
        // 60 Hz output, 41 fps stream: below 1.5x.
        assert!(!use_damage_copy(60_000, 41, 2));
        // Screencopy v1 has no copy_with_damage.
        assert!(!use_damage_copy(144_000, 30, 1));
        // Unknown refresh: plain copy.
        assert!(!use_damage_copy(0, 30, 3));
    }

    #[test]
    fn pacer_drops_frames_inside_the_interval() {
        let t0 = Instant::now();
        let mut p = Pacer::new(30);
        assert!(p.admit(t0));
        // Next 60 Hz refresh: 16.7 ms later is inside the 33.3 ms interval.
        assert!(!p.admit(t0 + Duration::from_micros(16_700)));
        // Two refreshes later: admitted.
        assert!(p.admit(t0 + Duration::from_micros(33_400)));
    }

    #[test]
    fn pacer_admits_every_refresh_at_stream_rate() {
        let t0 = Instant::now();
        let mut p = Pacer::new(60);
        assert!(p.admit(t0));
        // A refresh 16.7 ms later with 0.2 ms of jitter still counts.
        assert!(p.admit(t0 + Duration::from_micros(16_500)));
    }
}
