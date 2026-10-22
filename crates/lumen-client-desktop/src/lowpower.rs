//! The low-power profile of the client.
//!
//! The plan's Tier B client is a Celeron N4020: two slow cores and an Intel
//! UHD 600 that decodes H.264 in hardware but has no headroom for anything
//! else. The profile puts the client in that shape:
//!
//! * one decode thread,
//! * a frame queue of depth one, so a frame that is late is dropped rather
//!   than queued,
//! * a width guard, so a client that is asked for more than the device can
//!   handle says so instead of stuttering,
//! * a soak that measures the decode time and the frame rate over time.
//!
//! The profile is data, so the decisions it drives can be tested without a
//! device.

use std::collections::VecDeque;
use std::time::Duration;

/// The profile of a low-power client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LowPowerProfile {
    /// How many threads decode.
    pub decode_threads: usize,
    /// How many frames may wait in the queue.
    pub queue_depth: usize,
    /// The largest width the client accepts.
    pub max_width: u32,
    /// The largest height the client accepts.
    pub max_height: u32,
    /// Whether the decoder may hand out DMA-BUF frames.
    pub allow_dmabuf: bool,
    /// The largest time a decode may take before the frame is late.
    pub decode_budget: Duration,
}

impl Default for LowPowerProfile {
    fn default() -> Self {
        Self {
            decode_threads: 1,
            queue_depth: 1,
            max_width: 1920,
            max_height: 1080,
            allow_dmabuf: true,
            decode_budget: Duration::from_millis(12),
        }
    }
}

impl LowPowerProfile {
    /// The profile of the Tier B device of the plan.
    ///
    /// The Celeron N4020 has two cores and an Intel UHD 600. The plan's
    /// acceptance for the two-vCPU proxy is a decode p99 of 12 ms, so that
    /// is the budget of one frame.
    #[must_use]
    pub fn tier_b() -> Self {
        Self {
            decode_threads: 1,
            queue_depth: 1,
            max_width: 1920,
            max_height: 1080,
            allow_dmabuf: true,
            decode_budget: Duration::from_millis(12),
        }
    }

    /// The profile of a machine with headroom.
    #[must_use]
    pub fn full_speed() -> Self {
        Self {
            decode_threads: 2,
            queue_depth: 2,
            max_width: 3840,
            max_height: 2160,
            allow_dmabuf: true,
            decode_budget: Duration::from_millis(20),
        }
    }

    /// Whether a frame of this size is within the width guard.
    #[must_use]
    pub fn accepts(&self, width: u32, height: u32) -> bool {
        width <= self.max_width && height <= self.max_height
    }

    /// Why a frame of this size is refused.
    #[must_use]
    pub fn refuses(&self, width: u32, height: u32) -> Option<String> {
        if self.accepts(width, height) {
            return None;
        }
        Some(format!(
            "{width}x{height} is beyond the guard of {}x{}",
            self.max_width, self.max_height
        ))
    }
}

/// The soak of a low-power client.
///
/// The soak watches the decode time of every frame and counts the frames
/// that were late. The plan's acceptance for the Tier B proxy is a decode
/// p99 of 12 ms and no drops over the whole run.
#[derive(Debug)]
pub struct DecodeSoak {
    /// The profile the soak runs under.
    profile: LowPowerProfile,
    /// The decode times of the frames, in microseconds.
    samples: Vec<u64>,
    /// How many frames were late.
    late_frames: u64,
    /// How many frames were dropped.
    dropped_frames: u64,
    /// How many frames were decoded.
    decoded_frames: u64,
    /// The window of the samples the soak keeps.
    window: usize,
    /// The samples of the current window.
    recent: VecDeque<u64>,
}

impl DecodeSoak {
    /// A soak with the given profile.
    #[must_use]
    pub fn new(profile: LowPowerProfile) -> Self {
        Self {
            profile,
            samples: Vec::new(),
            late_frames: 0,
            dropped_frames: 0,
            decoded_frames: 0,
            window: 4096,
            recent: VecDeque::new(),
        }
    }

    /// The profile.
    #[must_use]
    pub fn profile(&self) -> &LowPowerProfile {
        &self.profile
    }

    /// Record a frame that was decoded in `duration`.
    pub fn record(&mut self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        self.decoded_frames += 1;
        self.samples.push(micros);
        self.recent.push_back(micros);
        while self.recent.len() > self.window {
            self.recent.pop_front();
        }
        if duration > self.profile.decode_budget {
            self.late_frames += 1;
        }
    }

    /// Record a frame that was dropped because it was late.
    pub fn record_drop(&mut self) {
        self.dropped_frames += 1;
    }

    /// How many frames were decoded.
    #[must_use]
    pub fn decoded_frames(&self) -> u64 {
        self.decoded_frames
    }

    /// How many frames were late.
    #[must_use]
    pub fn late_frames(&self) -> u64 {
        self.late_frames
    }

    /// How many frames were dropped.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// The percentile of the decode times, in microseconds.
    ///
    /// `percentile` is between 0 and 100. The samples of the current window
    /// are used, so the number follows the machine rather than the whole run.
    #[must_use]
    pub fn percentile(&self, percentile: f64) -> u64 {
        percentile_of(&self.recent.iter().copied().collect::<Vec<_>>(), percentile)
    }

    /// The p99 of the decode times, in microseconds.
    #[must_use]
    pub fn p99_us(&self) -> u64 {
        self.percentile(99.0)
    }

    /// The p50 of the decode times, in microseconds.
    #[must_use]
    pub fn p50_us(&self) -> u64 {
        self.percentile(50.0)
    }

    /// Whether the soak met the acceptance of the plan.
    ///
    /// The p99 of the decode must be within the budget of the profile and
    /// no frame may have been dropped.
    #[must_use]
    pub fn meets_acceptance(&self) -> bool {
        self.decoded_frames > 0
            && self.p99_us()
                <= u64::try_from(self.profile.decode_budget.as_micros()).unwrap_or(u64::MAX)
            && self.dropped_frames == 0
    }
}

/// The percentile of a list of samples.
///
/// The list is sorted, so the percentile is the sample at the position the
/// percentile names, with the nearest-rank rule: the p99 of a hundred
/// samples is the ninety-ninth smallest. An empty list has no percentile.
#[must_use]
pub fn percentile_of(samples: &[u64], percentile: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = (percentile / 100.0 * sorted.len() as f64).ceil() as i64 - 1;
    let index = rank.max(0) as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tier_b_profile_is_a_single_threaded_one() {
        let profile = LowPowerProfile::tier_b();
        assert_eq!(profile.decode_threads, 1);
        assert_eq!(profile.queue_depth, 1);
        assert_eq!(profile.decode_budget, Duration::from_millis(12));
    }

    #[test]
    fn the_width_guard_refuses_a_frame_that_is_too_big() {
        let profile = LowPowerProfile::tier_b();
        assert!(profile.accepts(1920, 1080));
        assert!(!profile.accepts(2560, 1440));
        let reason = profile.refuses(2560, 1440).expect("a reason");
        assert!(reason.contains("2560x1440"), "{reason}");
        assert!(reason.contains("1920x1080"), "{reason}");
        assert!(profile.refuses(1920, 1080).is_none());
    }

    #[test]
    fn the_percentile_picks_the_right_sample() {
        let samples: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_of(&samples, 0.0), 1);
        assert_eq!(percentile_of(&samples, 50.0), 50);
        assert_eq!(percentile_of(&samples, 99.0), 99);
        assert_eq!(percentile_of(&samples, 100.0), 100);
        assert_eq!(percentile_of(&[], 99.0), 0);
    }

    #[test]
    fn the_soak_counts_the_late_and_the_dropped_frames() {
        let mut soak = DecodeSoak::new(LowPowerProfile::tier_b());
        soak.record(Duration::from_millis(4));
        soak.record(Duration::from_millis(5));
        soak.record(Duration::from_millis(20));
        soak.record_drop();
        assert_eq!(soak.decoded_frames(), 3);
        assert_eq!(soak.late_frames(), 1);
        assert_eq!(soak.dropped_frames(), 1);
        assert_eq!(soak.p50_us(), 5_000);
    }

    #[test]
    fn a_clean_soak_meets_the_acceptance() {
        let mut soak = DecodeSoak::new(LowPowerProfile::tier_b());
        for _ in 0..100 {
            soak.record(Duration::from_millis(3));
        }
        assert!(soak.meets_acceptance());
        assert_eq!(soak.p99_us(), 3_000);
    }

    #[test]
    fn a_dropped_frame_fails_the_acceptance() {
        let mut soak = DecodeSoak::new(LowPowerProfile::tier_b());
        for _ in 0..100 {
            soak.record(Duration::from_millis(3));
        }
        soak.record_drop();
        assert!(!soak.meets_acceptance());
    }

    #[test]
    fn a_slow_decode_fails_the_acceptance() {
        let mut soak = DecodeSoak::new(LowPowerProfile::tier_b());
        // Five frames in a hundred are slow, so the p99 is one of them.
        for _ in 0..95 {
            soak.record(Duration::from_millis(3));
        }
        for _ in 0..5 {
            soak.record(Duration::from_millis(30));
        }
        assert!(!soak.meets_acceptance());
        assert_eq!(soak.p99_us(), 30_000);
        assert_eq!(soak.p50_us(), 3_000);
    }

    #[test]
    fn an_empty_soak_meets_nothing() {
        let soak = DecodeSoak::new(LowPowerProfile::tier_b());
        assert!(!soak.meets_acceptance());
    }
}
