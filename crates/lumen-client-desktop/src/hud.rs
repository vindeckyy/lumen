//! The heads-up display of the stream window.
//!
//! The display shows what the client is doing: the picture it shows, the rate it shows it at,
//! the round trip to the host and the frames it had to drop. The present stage rebuilds the
//! lines a few times a second and hands them to the renderer, which draws them over the
//! picture.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::Counters;

/// How many frames pass between two rebuilds of the display.
const REFRESH_FRAMES: u64 = 30;

/// The heads-up display of the stream window.
#[derive(Debug, Default)]
pub struct Hud {
    /// The frames shown since the last rebuild.
    frames: u64,
    /// When the last rebuild was.
    last: Option<Instant>,
    /// The rate of the last rebuild, in frames per second.
    fps: f64,
}

impl Hud {
    /// The lines to show, or `None` when the display is not due yet.
    ///
    /// The picture is the size the client shows, the rate the frames per second it shows them
    /// at, and the round trip the time the host's answer takes.
    pub fn lines(&mut self, picture: (u32, u32), counters: &Counters) -> Option<Vec<String>> {
        self.frames += 1;
        if self.frames < REFRESH_FRAMES {
            return None;
        }
        self.frames = 0;
        let now = Instant::now();
        if let Some(last) = self.last {
            let elapsed = now.duration_since(last).as_secs_f64();
            if elapsed > 0.0 {
                self.fps = REFRESH_FRAMES as f64 / elapsed;
            }
        }
        self.last = Some(now);
        let rtt_ms = counters.rtt_us.load(Ordering::Relaxed) / 1000;
        let dropped = counters.frames_dropped.load(Ordering::Relaxed);
        let mut second = format!("rtt {rtt_ms} ms");
        if dropped > 0 {
            let _ = write!(second, "  dropped {dropped}");
        }
        Some(vec![
            format!("{}x{}  {:.0} fps", picture.0, picture.1, self.fps),
            second,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_waits_for_the_frames_it_counts() {
        let mut hud = Hud::default();
        let counters = Counters::default();
        for _ in 0..REFRESH_FRAMES - 1 {
            assert!(hud.lines((1920, 1080), &counters).is_none());
        }
        let lines = hud
            .lines((1920, 1080), &counters)
            .expect("the display is due");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("1920x1080"), "{lines:?}");
        assert!(lines[1].starts_with("rtt "), "{lines:?}");
    }

    #[test]
    fn the_display_names_the_dropped_frames() {
        let mut hud = Hud::default();
        let counters = Counters::default();
        counters.frames_dropped.store(3, Ordering::Relaxed);
        counters.rtt_us.store(12_000, Ordering::Relaxed);
        let mut lines = None;
        for _ in 0..REFRESH_FRAMES {
            lines = hud.lines((1280, 720), &counters);
        }
        let lines = lines.expect("the display is due");
        assert!(lines[0].starts_with("1280x720"), "{lines:?}");
        assert!(lines[1].contains("rtt 12 ms"), "{lines:?}");
        assert!(lines[1].contains("dropped 3"), "{lines:?}");
    }
}
