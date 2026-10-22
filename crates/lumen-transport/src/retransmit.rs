//! Deadline-aware selective retransmission.
//!
//! A receiver that misses a slice asks for it on the control channel. The
//! host retransmits only while the frame's deadline has not passed. A late
//! packet is never waited for: the frame is dropped instead, and the client
//! asks for a fresh keyframe.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A slice that is missing and may be retransmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingSlice {
    /// Frame the slice belongs to.
    pub frame_id: u32,
    /// Index of the slice inside the access unit.
    pub slice_index: u16,
}

/// Tracks outstanding slices and their deadlines.
#[derive(Debug)]
pub struct RetransmitQueue {
    /// Outstanding slices keyed by (frame_id, slice_index).
    entries: HashMap<(u32, u16), Entry>,
    /// How long past the 1.5-interval deadline a slice stays retransmittable.
    deadline_slack: Duration,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    /// Number of times the slice has been retransmitted.
    attempts: u32,
}

impl RetransmitQueue {
    /// Create a queue with the given deadline slack.
    #[must_use]
    pub fn new(deadline_slack: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            deadline_slack,
        }
    }

    /// Record a slice that has not arrived yet.
    pub fn add(&mut self, frame_id: u32, slice_index: u16) {
        self.entries
            .insert((frame_id, slice_index), Entry { attempts: 0 });
    }

    /// Remove a slice that has arrived.
    pub fn remove(&mut self, frame_id: u32, slice_index: u16) {
        self.entries.remove(&(frame_id, slice_index));
    }

    /// Number of outstanding slices.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether a slice may still be retransmitted at `now`.
    ///
    /// The deadline is `frame_start` plus 1.5 frame intervals plus the configured
    /// slack. `frame_start` is the receiver-side instant the frame started.
    #[must_use]
    pub fn may_retransmit(
        &self,
        frame_id: u32,
        slice_index: u16,
        now: Instant,
        frame_start: Instant,
        frame_interval: Duration,
    ) -> bool {
        if !self.entries.contains_key(&(frame_id, slice_index)) {
            return false;
        }
        let deadline = frame_start + frame_interval + frame_interval / 2 + self.deadline_slack;
        now < deadline
    }

    /// Mark a retransmission attempt.
    pub fn on_retransmit(&mut self, frame_id: u32, slice_index: u16) {
        if let Some(entry) = self.entries.get_mut(&(frame_id, slice_index)) {
            entry.attempts += 1;
        }
    }

    /// Number of retransmission attempts made for a slice.
    #[must_use]
    pub fn attempts(&self, frame_id: u32, slice_index: u16) -> u32 {
        self.entries
            .get(&(frame_id, slice_index))
            .map_or(0, |e| e.attempts)
    }

    /// Drop every outstanding slice of a frame, e.g. after the frame was
    /// dropped for being late.
    pub fn drop_frame(&mut self, frame_id: u32) {
        self.entries.retain(|(f, _), _| *f != frame_id);
    }

    /// Drop entries whose frame deadline has passed.
    pub fn expire(&mut self, now: Instant, frame_start: Instant, frame_interval: Duration) {
        let deadline = frame_start + frame_interval + frame_interval / 2 + self.deadline_slack;
        if now >= deadline {
            self.entries.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_outstanding_slices() {
        let mut q = RetransmitQueue::new(Duration::from_millis(2));
        q.add(1, 0);
        q.add(1, 1);
        assert_eq!(q.len(), 2);
        q.remove(1, 0);
        assert_eq!(q.len(), 1);
        assert!(!q.is_empty());
    }

    #[test]
    fn expired_slices_may_not_be_retransmitted() {
        let mut q = RetransmitQueue::new(Duration::ZERO);
        let start = Instant::now();
        q.add(1, 0);
        assert!(q.may_retransmit(1, 0, start, start, Duration::from_millis(16)));
        let later = start + Duration::from_millis(100);
        assert!(!q.may_retransmit(1, 0, later, start, Duration::from_millis(16)));
    }

    #[test]
    fn attempts_are_counted() {
        let mut q = RetransmitQueue::new(Duration::from_millis(1));
        q.add(7, 3);
        assert_eq!(q.attempts(7, 3), 0);
        q.on_retransmit(7, 3);
        assert_eq!(q.attempts(7, 3), 1);
    }

    #[test]
    fn dropping_a_frame_removes_its_slices() {
        let mut q = RetransmitQueue::new(Duration::from_millis(1));
        q.add(1, 0);
        q.add(1, 1);
        q.add(2, 0);
        q.drop_frame(1);
        assert_eq!(q.len(), 1);
    }
}
