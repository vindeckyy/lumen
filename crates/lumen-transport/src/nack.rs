//! The NACK policy.
//!
//! A receiver that misses a slice asks for it on the control channel. The
//! host resends it only while the frame's deadline has not passed, and the
//! deadline is the frame's capture timestamp plus 1.5 frame intervals. A
//! frame that misses its deadline is dropped and a keyframe is requested
//! instead: the pipeline is never blocked waiting for a late packet.

use std::collections::HashMap;
use std::time::Duration;

/// What the receiver should do about a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameAction {
    /// The frame is still within its deadline. Keep waiting.
    Wait,
    /// Ask the host for the missing slices.
    Request {
        /// The frame the slices belong to.
        frame_id: u32,
        /// The missing slice indices.
        slices: Vec<u16>,
    },
    /// The frame is past its deadline. Drop it and ask for a keyframe.
    Drop {
        /// The frame that was dropped.
        frame_id: u32,
    },
}

/// A frame the receiver is still missing slices for.
#[derive(Debug, Clone)]
struct PendingFrame {
    /// The capture timestamp of the frame, on the sender's clock.
    capture_ts_us: u64,
    /// The slice indices that have not arrived.
    missing: Vec<u16>,
}

/// Tracks the frames the receiver is missing slices for, and decides when to
/// ask for them and when to give up.
#[derive(Debug)]
pub struct NackTracker {
    frames: HashMap<u32, PendingFrame>,
    /// The nominal interval between frames.
    frame_interval: Duration,
    /// Extra time past the deadline before a frame is given up.
    slack: Duration,
    /// The minimum time between two requests for the same frame.
    repeat_interval: Duration,
    /// When each frame was last asked for.
    last_request_us: HashMap<u32, u64>,
}

impl NackTracker {
    /// Create a tracker for a stream with the given frame interval.
    #[must_use]
    pub fn new(frame_interval: Duration) -> Self {
        Self {
            frames: HashMap::new(),
            frame_interval,
            slack: Duration::ZERO,
            repeat_interval: frame_interval / 2,
            last_request_us: HashMap::new(),
        }
    }

    /// Give up on a frame a little later than the strict deadline.
    #[must_use]
    pub fn with_slack(mut self, slack: Duration) -> Self {
        self.slack = slack;
        self
    }

    /// Record the slices that are missing for a frame.
    ///
    /// An empty list means the frame is complete, so it is forgotten.
    pub fn add(&mut self, frame_id: u32, capture_ts_us: u64, missing: Vec<u16>) {
        if missing.is_empty() {
            self.remove(frame_id);
            return;
        }
        self.frames.insert(
            frame_id,
            PendingFrame {
                capture_ts_us,
                missing,
            },
        );
    }

    /// Forget a frame that arrived complete.
    pub fn remove(&mut self, frame_id: u32) {
        self.frames.remove(&frame_id);
        self.last_request_us.remove(&frame_id);
    }

    /// The deadline of a frame, on the sender's clock.
    #[must_use]
    pub fn deadline_us(&self, frame_id: u32) -> Option<u64> {
        let frame = self.frames.get(&frame_id)?;
        let interval = self.frame_interval.as_micros() as u64;
        Some(
            frame
                .capture_ts_us
                .saturating_add(interval)
                .saturating_add(interval / 2)
                .saturating_add(self.slack.as_micros() as u64),
        )
    }

    /// What to do about a frame at `now_us`, on the sender's clock.
    ///
    /// Returns `None` when the frame is not being waited for.
    #[must_use]
    pub fn action(&self, frame_id: u32, now_us: u64) -> Option<FrameAction> {
        let frame = self.frames.get(&frame_id)?;
        let deadline = self.deadline_us(frame_id)?;
        if now_us >= deadline {
            return Some(FrameAction::Drop { frame_id });
        }
        // Do not ask twice in a row for the same frame within the repeat
        // interval: a request is a control message, not a free operation.
        if let Some(last) = self.last_request_us.get(&frame_id)
            && now_us.saturating_sub(*last) < self.repeat_interval.as_micros() as u64
        {
            return Some(FrameAction::Wait);
        }
        Some(FrameAction::Request {
            frame_id,
            slices: frame.missing.clone(),
        })
    }

    /// Note that a frame was asked for.
    pub fn on_request(&mut self, frame_id: u32, now_us: u64) {
        self.last_request_us.insert(frame_id, now_us);
    }

    /// Drop every frame that is past its deadline.
    ///
    /// Returns the frames that were dropped, so the caller can ask for a
    /// keyframe.
    pub fn expire(&mut self, now_us: u64) -> Vec<u32> {
        let expired: Vec<u32> = self
            .frames
            .keys()
            .copied()
            .filter(|frame_id| {
                self.deadline_us(*frame_id)
                    .is_some_and(|deadline| now_us >= deadline)
            })
            .collect();
        for frame_id in &expired {
            self.remove(*frame_id);
        }
        expired
    }

    /// The frames that are still being waited for.
    #[must_use]
    pub fn pending(&self) -> Vec<u32> {
        self.frames.keys().copied().collect()
    }

    /// How many frames are being waited for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether nothing is being waited for.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> NackTracker {
        NackTracker::new(Duration::from_millis(16))
    }

    #[test]
    fn a_frame_with_missing_slices_is_asked_for() {
        let mut t = tracker();
        t.add(7, 1_000_000, vec![2, 5]);
        assert_eq!(
            t.action(7, 1_001_000),
            Some(FrameAction::Request {
                frame_id: 7,
                slices: vec![2, 5]
            })
        );
    }

    #[test]
    fn the_deadline_is_capture_plus_one_and_a_half_intervals() {
        let mut t = tracker();
        t.add(7, 1_000_000, vec![2]);
        // 16 ms interval, so the deadline is 16 + 8 = 24 ms after the capture.
        assert_eq!(t.deadline_us(7), Some(1_024_000));
        assert_eq!(
            t.action(7, 1_023_999),
            Some(FrameAction::Request {
                frame_id: 7,
                slices: vec![2]
            })
        );
    }

    #[test]
    fn a_past_deadline_frame_is_dropped() {
        let mut t = tracker();
        t.add(7, 1_000_000, vec![2]);
        assert_eq!(
            t.action(7, 1_030_000),
            Some(FrameAction::Drop { frame_id: 7 })
        );
    }

    #[test]
    fn a_frame_is_not_asked_for_twice_in_a_row() {
        let mut t = tracker();
        t.add(7, 1_000_000, vec![2]);
        t.on_request(7, 1_001_000);
        // The repeat interval is half a frame interval, so 8 ms.
        assert_eq!(t.action(7, 1_002_000), Some(FrameAction::Wait));
        assert_eq!(
            t.action(7, 1_010_000),
            Some(FrameAction::Request {
                frame_id: 7,
                slices: vec![2]
            })
        );
    }

    #[test]
    fn a_complete_frame_is_forgotten() {
        let mut t = tracker();
        t.add(7, 1_000_000, vec![2]);
        t.add(7, 1_000_000, vec![]);
        assert!(t.is_empty());
        assert_eq!(t.action(7, 1_001_000), None);
    }

    #[test]
    fn expired_frames_are_dropped_and_reported() {
        let mut t = tracker();
        t.add(1, 1_000_000, vec![0]);
        t.add(2, 1_020_000, vec![0]);
        // Frame 1 is past its deadline of 1_024_000, frame 2 is not.
        let expired = t.expire(1_030_000);
        assert_eq!(expired, vec![1]);
        assert_eq!(t.pending(), vec![2]);
    }

    #[test]
    fn the_slack_defers_the_deadline() {
        let mut t = tracker().with_slack(Duration::from_millis(10));
        t.add(7, 1_000_000, vec![2]);
        assert_eq!(t.deadline_us(7), Some(1_034_000));
    }
}
