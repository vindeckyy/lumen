//! Scheduling audio playback against the video clock.
//!
//! The client presents video whenever it can and the sound card asks for
//! audio whenever it wants it. The two only stay in sync if every audio
//! frame is played at the instant the video frame it belongs to is on
//! screen, which is what this module computes.
//!
//! The client keeps one reference point for the video: the host capture
//! timestamp of the video frame it last presented and the client-clock time
//! at which it presented it. Together with the measured clock offset that
//! gives how long the video path takes. Every audio frame is then placed on
//! the same timeline: the audio that belongs to a video frame is played when
//! that video frame is on screen.
//!
//! A frame that arrives too late to be played in sync is dropped rather than
//! played late, because late audio is worse than missing audio.

use std::collections::VecDeque;

use crate::AudioFrame;
use crate::clock::ClockOffset;
use crate::session::ReceivedAudio;

/// How late an audio frame may be and still be played, in microseconds.
///
/// The plan asks for an A/V skew of at most 20 ms, so a frame that is more
/// than that late is dropped instead of played out of sync.
pub const DEFAULT_MAX_LATENESS_US: u64 = 20_000;

/// The client's video playout clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoClock {
    /// The host capture timestamp of the reference video frame.
    pub capture_ts_us: u64,
    /// The client-clock time at which the reference frame was presented.
    pub present_ts_us: u64,
}

/// Why an audio frame was not played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// No video frame has been presented yet, so there is nothing to align to.
    NoVideoClock,
    /// The offset between the host clock and the client clock is not measured.
    NoClockOffset,
    /// The frame's play time is in the past: it would land after its video.
    TooLate,
}

/// What the scheduler decided about one audio frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    /// Play the frame at this client-clock time.
    Play {
        /// The client-clock time at which the frame must be played.
        play_at_us: u64,
        /// How late the frame was when it was scheduled, zero when it was early.
        late_by_us: u64,
    },
    /// The frame must not be played.
    Drop {
        /// Why the frame was dropped.
        reason: DropReason,
    },
}

/// Counters of the scheduler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlaybackStats {
    /// Frames scheduled for playback.
    pub scheduled: u64,
    /// Frames dropped because they were too late.
    pub dropped_late: u64,
    /// Frames dropped because no video frame had been presented yet.
    pub dropped_no_video_clock: u64,
    /// Frames dropped because the clock offset had not been measured.
    pub dropped_no_clock_offset: u64,
}

/// How many presented video frames the scheduler remembers.
const MAX_VIDEO_HISTORY: usize = 32;

/// Places audio frames on the client's video clock.
#[derive(Debug, Clone)]
pub struct PlaybackScheduler {
    /// The offset between the host clock and the client clock.
    offset: ClockOffset,
    /// The video frame the audio is placed against.
    video_clock: Option<VideoClock>,
    /// How late a frame may be and still be played.
    max_lateness_us: u64,
    /// Extra delay added to every frame, for the jitter buffer.
    lead_us: u64,
    /// Counters.
    stats: PlaybackStats,
    /// The video frames the client has shown, newest last.
    ///
    /// An audio frame is aligned to the video frame that was on screen when
    /// it was captured, which is not the newest one: by the time the audio
    /// arrives the client has usually shown a frame or two more.
    history: VecDeque<VideoClock>,
}

impl PlaybackScheduler {
    /// Create a scheduler for a client whose clock offset is `offset`.
    ///
    /// The offset must be measured: without it the host's timestamps cannot
    /// be placed on the client's clock, and every frame would be dropped.
    #[must_use]
    pub fn new(offset: ClockOffset) -> Self {
        Self {
            offset,
            video_clock: None,
            max_lateness_us: DEFAULT_MAX_LATENESS_US,
            lead_us: 0,
            stats: PlaybackStats::default(),
            history: VecDeque::new(),
        }
    }

    /// Set how late a frame may be and still be played.
    #[must_use]
    pub fn with_max_lateness_us(mut self, max_lateness_us: u64) -> Self {
        self.max_lateness_us = max_lateness_us;
        self
    }

    /// Set the extra delay added to every frame.
    #[must_use]
    pub fn with_lead_us(mut self, lead_us: u64) -> Self {
        self.lead_us = lead_us;
        self
    }

    /// The offset between the host clock and the client clock.
    #[must_use]
    pub fn offset(&self) -> ClockOffset {
        self.offset
    }

    /// Replace the offset without losing the video clock.
    ///
    /// The offset is measured again and again, so it is set in place. A new
    /// scheduler would start without a video clock, and every frame would be
    /// dropped for want of one.
    pub fn set_offset(&mut self, offset: ClockOffset) {
        self.offset = offset;
    }

    /// The video frame the audio is placed against.
    #[must_use]
    pub fn video_clock(&self) -> Option<VideoClock> {
        self.video_clock
    }

    /// Record the video frame the client has just presented.
    ///
    /// `capture_ts_us` is the host capture timestamp of the frame and
    /// `present_ts_us` the client-clock time at which it was presented.
    pub fn set_video_frame(&mut self, capture_ts_us: u64, present_ts_us: u64) {
        let clock = VideoClock {
            capture_ts_us,
            present_ts_us,
        };
        self.video_clock = Some(clock);
        self.history.push_back(clock);
        while self.history.len() > MAX_VIDEO_HISTORY {
            self.history.pop_front();
        }
    }

    /// Forget the video frame, for example after the video stream stalled.
    pub fn clear_video_clock(&mut self) {
        self.video_clock = None;
        self.history.clear();
    }

    /// How many video frames the scheduler remembers.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// The presentation time of the video frame the given host timestamp is
    /// aligned to.
    #[must_use]
    pub fn aligned_present_ts(&self, align_ts_us: u64) -> Option<u64> {
        self.closest_video_frame(align_ts_us)
            .map(|frame| frame.present_ts_us)
    }

    /// The capture timestamp of the video frame the given host timestamp is
    /// aligned to.
    ///
    /// The difference between the two is the error in the A/V alignment: how
    /// far the audio is from the video frame of the same moment.
    #[must_use]
    pub fn aligned_capture_ts(&self, align_ts_us: u64) -> Option<u64> {
        self.closest_video_frame(align_ts_us)
            .map(|frame| frame.capture_ts_us)
    }

    /// The video frame the given host timestamp is closest to.
    ///
    /// The timestamps are the host's capture timestamps, so the comparison
    /// needs no offset.
    fn closest_video_frame(&self, align_ts_us: u64) -> Option<&VideoClock> {
        self.history
            .iter()
            .min_by_key(|frame| frame.capture_ts_us.abs_diff(align_ts_us))
    }

    /// Counters since the scheduler was created.
    #[must_use]
    pub fn stats(&self) -> PlaybackStats {
        self.stats
    }

    /// Schedules one audio frame against the video clock.
    ///
    /// `align_ts_us` is the host capture timestamp of the video frame the
    /// audio belongs to, and `now_us` is the client-clock time now. The
    /// frame is played when its video frame is on screen.
    pub fn schedule(&mut self, align_ts_us: u64, now_us: u64) -> Playback {
        let Some(video) = self.closest_video_frame(align_ts_us) else {
            self.stats.dropped_no_video_clock += 1;
            return Playback::Drop {
                reason: DropReason::NoVideoClock,
            };
        };
        if !self.offset.is_measured() {
            self.stats.dropped_no_clock_offset += 1;
            return Playback::Drop {
                reason: DropReason::NoClockOffset,
            };
        }
        // How far the audio's capture time is from the video frame it
        // belongs to. The audio is played when that video frame is on
        // screen, plus the difference between the two capture times.
        let capture_delta_us = align_ts_us.cast_signed() - video.capture_ts_us.cast_signed();
        let play_at_us = ((video.present_ts_us.cast_signed() + capture_delta_us).max(0) as u64)
            .saturating_add(self.lead_us);
        let late_by_us = now_us.saturating_sub(play_at_us);
        if late_by_us > self.max_lateness_us {
            self.stats.dropped_late += 1;
            return Playback::Drop {
                reason: DropReason::TooLate,
            };
        }
        self.stats.scheduled += 1;
        Playback::Play {
            play_at_us,
            late_by_us,
        }
    }

    /// Schedules one audio frame produced by the sync module.
    ///
    /// The frame is aligned to the video frame captured at the same host
    /// instant, which is what the host stamps into the header when it has no
    /// finer reference.
    pub fn schedule_frame(&mut self, frame: &AudioFrame, now_us: u64) -> Playback {
        self.schedule(frame.capture_ts_us, now_us)
    }

    /// Schedules one audio frame that arrived from the wire.
    pub fn schedule_received(&mut self, audio: &ReceivedAudio, now_us: u64) -> Playback {
        self.schedule(audio.header.video_capture_ts_us, now_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioHeader;
    use lumen_protocol::audio::{AUDIO_VERSION, AudioKind};

    /// The client clock runs 10 ms ahead of the host clock.
    const OFFSET: i64 = 10_000;

    fn scheduler() -> PlaybackScheduler {
        PlaybackScheduler::new(ClockOffset::new(OFFSET))
    }

    fn frame(capture_ts_us: u64) -> AudioFrame {
        AudioFrame {
            data: vec![0; 8],
            capture_ts_us,
            pcm: false,
        }
    }

    #[test]
    fn a_frame_is_scheduled_against_the_video_clock() {
        let mut sched = scheduler();
        // The client presented the video frame captured at host 1_000_000 at
        // its own clock time 1_030_000. The host instant is 1_010_000 on the
        // client clock, so the video path took 20 ms.
        sched.set_video_frame(1_000_000, 1_030_000);
        let playback = sched.schedule(1_000_000, 1_005_000);
        assert_eq!(
            playback,
            Playback::Play {
                play_at_us: 1_030_000,
                late_by_us: 0,
            }
        );
        assert_eq!(sched.stats().scheduled, 1);
        assert_eq!(sched.stats().dropped_late, 0);
    }

    #[test]
    fn the_schedule_follows_the_video_delay() {
        let mut sched = scheduler();
        // A video path of 30 ms: the frame captured at host 2_000_000 is
        // played at 2_010_000 + 30_000 on the client clock.
        sched.set_video_frame(1_000_000, 1_040_000);
        assert_eq!(
            sched.schedule(2_000_000, 1_000_000),
            Playback::Play {
                play_at_us: 2_040_000,
                late_by_us: 0,
            }
        );
    }

    #[test]
    fn a_late_frame_is_dropped_rather_than_played_late() {
        let mut sched = scheduler();
        sched.set_video_frame(1_000_000, 1_030_000);
        // The frame belongs to the video frame at host 1_000_000, so it is
        // due at 1_030_000. It arrives at 1_060_000, half a second of skew
        // later than the 20 ms the plan allows.
        let playback = sched.schedule(1_000_000, 1_060_000);
        assert_eq!(
            playback,
            Playback::Drop {
                reason: DropReason::TooLate
            }
        );
        assert_eq!(sched.stats().dropped_late, 1);
        assert_eq!(sched.stats().scheduled, 0);
    }

    #[test]
    fn a_frame_that_is_a_little_late_is_still_played() {
        let mut sched = scheduler();
        sched.set_video_frame(1_000_000, 1_030_000);
        // 5 ms late is inside the 20 ms the plan allows.
        assert_eq!(
            sched.schedule(1_000_000, 1_035_000),
            Playback::Play {
                play_at_us: 1_030_000,
                late_by_us: 5_000,
            }
        );
    }

    #[test]
    fn without_a_video_frame_nothing_is_played() {
        let mut sched = scheduler();
        assert_eq!(
            sched.schedule(1_000_000, 1_000_000),
            Playback::Drop {
                reason: DropReason::NoVideoClock
            }
        );
        assert_eq!(sched.stats().dropped_no_video_clock, 1);
    }

    #[test]
    fn without_a_measured_offset_nothing_is_played() {
        let mut sched = PlaybackScheduler::new(ClockOffset::unmeasured());
        sched.set_video_frame(1_000_000, 1_030_000);
        assert_eq!(
            sched.schedule(1_000_000, 1_000_000),
            Playback::Drop {
                reason: DropReason::NoClockOffset
            }
        );
        assert_eq!(sched.stats().dropped_no_clock_offset, 1);
    }

    #[test]
    fn the_sync_module_output_can_be_scheduled() {
        let mut sched = scheduler();
        sched.set_video_frame(1_000_000, 1_030_000);
        assert_eq!(
            sched.schedule_frame(&frame(1_000_000), 1_000_000),
            Playback::Play {
                play_at_us: 1_030_000,
                late_by_us: 0,
            }
        );
    }

    #[test]
    fn a_received_frame_is_scheduled_by_its_aligned_timestamp() {
        let mut sched = scheduler();
        sched.set_video_frame(1_000_000, 1_030_000);
        let audio = ReceivedAudio {
            header: AudioHeader {
                version: AUDIO_VERSION,
                kind: AudioKind::Opus,
                channels: 2,
                sample_rate: 48_000,
                frame_duration_us: 5_000,
                frame_index: 3,
                video_capture_ts_us: 1_005_000,
                payload_len: 4,
                seq: 0,
            },
            payload: vec![0; 4],
        };
        // The frame belongs to the video frame captured at host 1_005_000,
        // so it is due 5 ms after the previous one.
        assert_eq!(
            sched.schedule_received(&audio, 1_000_000),
            Playback::Play {
                play_at_us: 1_035_000,
                late_by_us: 0,
            }
        );
    }

    #[test]
    fn the_lead_delays_every_frame() {
        let mut sched = scheduler().with_lead_us(3_000);
        sched.set_video_frame(1_000_000, 1_030_000);
        assert_eq!(
            sched.schedule(1_000_000, 1_000_000),
            Playback::Play {
                play_at_us: 1_033_000,
                late_by_us: 0,
            }
        );
    }

    #[test]
    fn the_lateness_bound_can_be_widened() {
        let mut sched = scheduler().with_max_lateness_us(100_000);
        sched.set_video_frame(1_000_000, 1_030_000);
        assert!(matches!(
            sched.schedule(1_000_000, 1_100_000),
            Playback::Play { .. }
        ));
    }

    #[test]
    fn a_stalled_video_stream_stops_the_audio() {
        let mut sched = scheduler();
        sched.set_video_frame(1_000_000, 1_030_000);
        assert!(matches!(
            sched.schedule(1_000_000, 1_000_000),
            Playback::Play { .. }
        ));
        sched.clear_video_clock();
        assert_eq!(sched.video_clock(), None);
        assert_eq!(
            sched.schedule(1_000_000, 1_000_000),
            Playback::Drop {
                reason: DropReason::NoVideoClock
            }
        );
    }

    #[test]
    fn the_offset_is_visible_to_the_caller() {
        let sched = scheduler();
        assert_eq!(sched.offset().offset_us(), OFFSET);
        assert!(sched.offset().is_measured());
    }
}
