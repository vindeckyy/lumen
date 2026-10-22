//! The quality ladder the client walks.
//!
//! A client that cannot decode and present the picture it is given drops
//! frames, and a stream that drops frames is worse than a smaller stream
//! that does not: the picture is late, the reference chain breaks, and the
//! decoder asks for a keyframe it does not need.
//!
//! So the client says so. When it drops frames it asks the host for the next
//! size down, and when it has been clean for a while it asks for the next
//! size up. The host scales the capture to that size, which costs less work
//! than the big one, so the whole pipeline gets cheaper exactly when the
//! machine needs it to.

use std::time::{Duration, Instant};

/// The sizes the client asks for, from the largest to the smallest.
///
/// Every step keeps the aspect ratio of 16:9 and the height even, because
/// the NV12 planes need even sizes.
pub const LADDER: [(u32, u32); 8] = [
    (3840, 2160),
    (2560, 1440),
    (1920, 1080),
    (1600, 900),
    (1280, 720),
    (1024, 576),
    (854, 480),
    (640, 360),
];

/// How long the client watches before it decides.
const WINDOW: Duration = Duration::from_secs(1);

/// How long the client has to be clean before it asks for a bigger picture.
///
/// The wait is long on purpose: a picture that is one step too big costs
/// frames, and a picture that is one step too small costs sharpness. The
/// client stays down until it is sure.
const CLEAN_FOR: Duration = Duration::from_secs(10);

/// The share of dropped frames that makes the client ask for a smaller
/// picture.
const DROP_LIMIT: f64 = 0.02;

/// How long a picture the client failed at stays blocked.
///
/// A machine that was busy for a moment should not lower the quality for
/// good, so the client tries the bigger picture again after a while.
const BLOCK_FOR: Duration = Duration::from_secs(300);

/// Where the client is on the ladder.
#[derive(Debug)]
pub struct Ladder {
    /// The index in [`LADDER`].
    index: usize,
    /// Whether the client has been sent a picture yet.
    ///
    /// Until it has, the ladder stays where it is: it cannot know which way
    /// is up.
    known: bool,
    /// The largest picture the client has been sent.
    ///
    /// The client never asks for a picture bigger than the one it has seen:
    /// the host would scale up to reach it, and a picture bigger than the
    /// capture is a picture the host has to invent.
    ceiling: usize,
    /// The width of the biggest picture the client failed at, and when it
    /// failed.
    ///
    /// Without it the client climbs back to a picture it has already shown
    /// it cannot reach, and the quality never settles. The block expires,
    /// so a picture that failed once is tried again later.
    blocked: Option<(u32, Instant)>,
    /// The frames dropped since the last look.
    dropped: u64,
    /// The frames shown since the last look.
    shown: u64,
    /// When the client last looked.
    last_look: Instant,
    /// When the client last dropped a frame.
    last_drop: Instant,
}

impl Ladder {
    /// Start at the bottom of the ladder, on the smallest picture.
    ///
    /// The client moves up to the picture it is actually sent when the first
    /// frame arrives.
    #[must_use]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let index = LADDER.len() - 1;
        Self {
            index,
            known: false,
            ceiling: index,
            blocked: None,
            dropped: 0,
            shown: 0,
            last_look: Instant::now(),
            last_drop: Instant::now(),
        }
    }

    /// The size the client is on.
    #[must_use]
    pub fn current(&self) -> (u32, u32) {
        LADDER[self.index]
    }

    /// The index on the ladder.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// Move to the rung that matches the picture the host is sending.
    ///
    /// The largest picture the client has seen is the ceiling: it never asks
    /// for one that is bigger.
    pub fn set_size(&mut self, width: u32, height: u32) {
        if let Some(index) = LADDER.iter().position(|&(w, h)| w == width && h == height) {
            self.index = index;
            self.ceiling = self.ceiling.min(index);
            self.known = true;
        }
    }

    /// Record a frame that was shown.
    pub fn record_shown(&mut self) {
        self.shown += 1;
    }

    /// Record a frame that was dropped.
    pub fn record_dropped(&mut self) {
        self.dropped += 1;
        self.last_drop = Instant::now();
    }

    /// The size to ask the host for, when it changed.
    ///
    /// The answer is `None` when the client stays where it is, which is the
    /// common case.
    pub fn step(&mut self) -> Option<(u32, u32)> {
        if !self.known {
            return None;
        }
        let now = Instant::now();
        if now.duration_since(self.last_look) < WINDOW {
            return None;
        }
        self.last_look = now;
        let total = self.dropped + self.shown;
        let drop_rate = if total == 0 {
            0.0
        } else {
            self.dropped as f64 / total as f64
        };
        self.dropped = 0;
        self.shown = 0;

        if drop_rate > DROP_LIMIT && self.index + 1 < LADDER.len() {
            // The picture the client is on is too big for it, so it is
            // remembered as one it must not ask for again.
            let (width, _) = LADDER[self.index];
            self.blocked = Some(match self.blocked {
                Some((blocked, _)) => (blocked.min(width), Instant::now()),
                None => (width, Instant::now()),
            });
            self.index += 1;
            return Some(LADDER[self.index]);
        }
        // The client goes back up only when the ceiling allows it, which
        // means the host has sent a bigger picture at some point.
        if drop_rate == 0.0
            && now.duration_since(self.last_drop) >= CLEAN_FOR
            && self.index > 0
            && self.index > self.ceiling
        {
            let (width, _) = LADDER[self.index - 1];
            // The client does not ask for a picture it has already failed
            // at, unless the block has gone stale.
            let allowed = match self.blocked {
                Some((blocked, when)) => width < blocked || now.duration_since(when) >= BLOCK_FOR,
                None => true,
            };
            if allowed {
                self.index -= 1;
                return Some(LADDER[self.index]);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_starts_on_the_picture_the_host_sends() {
        let mut ladder = Ladder::new();
        ladder.set_size(1280, 720);
        assert_eq!(ladder.current(), (1280, 720));
    }

    #[test]
    fn the_client_does_nothing_before_it_has_seen_a_picture() {
        let mut ladder = Ladder::new();
        // The ladder is at the bottom and does not know which way is up.
        std::thread::sleep(CLEAN_FOR + WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), None);
        assert_eq!(ladder.current(), (640, 360));
    }

    #[test]
    fn the_client_never_asks_for_a_bigger_picture_than_it_has_seen() {
        let mut ladder = Ladder::new();
        ladder.set_size(1280, 720);
        // Ten clean seconds do not move it up past what the host sent.
        for _ in 0..100 {
            ladder.record_shown();
        }
        std::thread::sleep(WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), None);
        assert_eq!(ladder.current(), (1280, 720));
    }

    #[test]
    fn a_client_does_not_ask_again_for_a_picture_it_failed_at() {
        let mut ladder = Ladder::new();
        ladder.set_size(1280, 720);
        // It drops at 1280x720, so that picture is remembered as too big.
        ladder.record_dropped();
        std::thread::sleep(WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), Some((1024, 576)));
        // It is clean for a long time, but it does not go back to the
        // picture it failed at.
        for _ in 0..100 {
            ladder.record_shown();
        }
        std::thread::sleep(CLEAN_FOR + WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), None);
        assert_eq!(ladder.current(), (1024, 576));
    }

    #[test]
    fn a_client_stays_below_a_picture_it_failed_at() {
        let mut ladder = Ladder::new();
        // The host sent a big picture once, so the client may go back to it.
        ladder.set_size(1920, 1080);
        ladder.set_size(1280, 720);
        ladder.record_dropped();
        std::thread::sleep(WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), Some((1024, 576)));
        // Ten clean seconds later it would like to go back up, but 1280x720 is
        // the picture it failed at.
        for _ in 0..100 {
            ladder.record_shown();
        }
        std::thread::sleep(CLEAN_FOR + WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), None);
        assert_eq!(ladder.current(), (1024, 576));
    }

    #[test]
    fn a_blocked_picture_is_tried_again_after_a_while() {
        let mut ladder = Ladder::new();
        ladder.set_size(1280, 720);
        ladder.record_dropped();
        std::thread::sleep(WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), Some((1024, 576)));
        // The block is stale, so the client tries the bigger picture again.
        let stale = Instant::now()
            .checked_sub(BLOCK_FOR + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        ladder.blocked = Some((1280, stale));
        for _ in 0..100 {
            ladder.record_shown();
        }
        std::thread::sleep(CLEAN_FOR + WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), Some((1280, 720)));
    }

    #[test]
    fn the_ladder_follows_the_picture_the_host_sends() {
        let mut ladder = Ladder::new();
        ladder.set_size(1280, 720);
        assert_eq!(ladder.current(), (1280, 720));
        // A picture that is not on the ladder leaves it where it is.
        ladder.set_size(1000, 700);
        assert_eq!(ladder.current(), (1280, 720));
    }

    #[test]
    fn a_client_that_drops_frames_asks_for_a_smaller_picture() {
        let mut ladder = Ladder::new();
        ladder.set_size(1920, 1080);
        ladder.record_dropped();
        ladder.record_shown();
        // The window has not passed yet.
        assert_eq!(ladder.step(), None);
        // Once it has, the client asks for the next size down.
        std::thread::sleep(WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), Some((1600, 900)));
        assert_eq!(ladder.current(), (1600, 900));
    }

    #[test]
    fn a_client_that_is_clean_stays_where_it_is() {
        let mut ladder = Ladder::new();
        ladder.set_size(1920, 1080);
        for _ in 0..100 {
            ladder.record_shown();
        }
        std::thread::sleep(WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), None);
        assert_eq!(ladder.current(), (1920, 1080));
    }

    #[test]
    fn the_smallest_rung_is_the_end_of_the_road() {
        let mut ladder = Ladder::new();
        ladder.set_size(640, 360);
        ladder.record_dropped();
        std::thread::sleep(WINDOW + Duration::from_millis(50));
        assert_eq!(ladder.step(), None);
        assert_eq!(ladder.current(), (640, 360));
    }

    #[test]
    fn every_rung_has_an_even_height_and_the_same_shape() {
        for &(width, height) in &LADDER {
            assert!(
                height.is_multiple_of(2),
                "{width}x{height} has an odd height"
            );
            assert!(width.is_multiple_of(2), "{width}x{height} has an odd width");
            let ratio = f64::from(width) / f64::from(height);
            assert!(
                (ratio - 16.0 / 9.0).abs() < 0.02,
                "{width}x{height} is not 16:9"
            );
        }
    }
}
