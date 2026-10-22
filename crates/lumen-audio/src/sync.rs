//! Framing and A/V stamping. Capture chunks arrive in arbitrary sizes; codec
//! frames must be exact. Each frame takes the capture timestamp of its first
//! sample, so the client can align playback with the video clock.

use std::collections::VecDeque;
use std::fmt;

use crate::{AudioConfig, AudioEncoder, AudioFrame};

pub struct FrameEncoder {
    enc: Box<dyn AudioEncoder>,
    pcm: bool,
    /// Interleaved samples per frame.
    frame_len: usize,
    pending: Vec<f32>,
    /// Capture timestamp and not-yet-consumed sample count of each push still in `pending`, oldest first.
    marks: VecDeque<(u64, usize)>,
}

impl FrameEncoder {
    pub fn new(cfg: &AudioConfig) -> Result<Self, String> {
        let frame_len = cfg.frame_samples()? * usize::from(cfg.channels);
        Ok(Self {
            enc: crate::new_encoder(cfg)?,
            pcm: !cfg.use_opus,
            frame_len,
            pending: Vec::new(),
            marks: VecDeque::new(),
        })
    }

    /// Queues interleaved `samples` captured at `capture_ts_us` and returns every frame now complete.
    pub fn push(&mut self, samples: &[f32], capture_ts_us: u64) -> Result<Vec<AudioFrame>, String> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }
        self.pending.extend_from_slice(samples);
        self.marks.push_back((capture_ts_us, samples.len()));
        let mut frames = Vec::new();
        while self.pending.len() >= self.frame_len {
            let data = self.enc.encode(&self.pending[..self.frame_len])?;
            let ts = self.consume(self.frame_len);
            frames.push(AudioFrame {
                data,
                capture_ts_us: ts,
                pcm: self.pcm,
            });
        }
        Ok(frames)
    }

    /// Drops `n` samples from the front and returns the capture time of the first one.
    fn consume(&mut self, mut n: usize) -> u64 {
        let ts = self.marks.front().map_or(0, |m| m.0);
        self.pending.drain(..n);
        while n > 0 {
            let Some(mark) = self.marks.front_mut() else {
                break;
            };
            let take = mark.1.min(n);
            mark.1 -= take;
            n -= take;
            if mark.1 == 0 {
                self.marks.pop_front();
            }
        }
        ts
    }
}

impl fmt::Debug for FrameEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameEncoder")
            .field("pcm", &self.pcm)
            .field("frame_len", &self.frame_len)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(use_opus: bool, frame_ms: u32) -> AudioConfig {
        AudioConfig {
            sample_rate: 48_000,
            channels: 1,
            frame_ms,
            bitrate_bps: 64_000,
            use_opus,
        }
    }

    #[test]
    fn frames_take_timestamp_of_their_first_sample() {
        // 50 = 5 ms = 240 mono samples per frame.
        let mut enc = FrameEncoder::new(&cfg(false, 50)).expect("open");
        let first = enc.push(&[0.5; 300], 1000).expect("push");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].capture_ts_us, 1000);
        assert!(first[0].pcm);
        assert_eq!(first[0].data.len(), 240 * 4);

        // 60 samples left from ts 1000, then 300 more at ts 2000: the one complete frame
        // starts with the ts 1000 samples, so it keeps ts 1000. 120 samples remain.
        let second = enc.push(&[0.5; 300], 2000).expect("push");
        let ts: Vec<u64> = second.iter().map(|f| f.capture_ts_us).collect();
        assert_eq!(ts, vec![1000]);

        // The next frame starts with the 120 leftover ts 2000 samples, so it takes ts 2000.
        let third = enc.push(&[0.5; 120], 3000).expect("push");
        let ts: Vec<u64> = third.iter().map(|f| f.capture_ts_us).collect();
        assert_eq!(ts, vec![2000]);
    }

    #[test]
    fn opus_frames_are_exact_and_stamped() {
        let mut enc = FrameEncoder::new(&cfg(true, 50)).expect("open");
        let frames = enc.push(&[0.0; 1000], 42).expect("push");
        assert_eq!(frames.len(), 4, "1000 samples hold four 240-sample frames");
        assert!(frames.iter().all(|f| f.capture_ts_us == 42 && !f.pcm));
    }

    #[test]
    fn zero_sample_rate_is_rejected_not_spun() {
        let bad = AudioConfig {
            sample_rate: 0,
            ..cfg(false, 50)
        };
        assert!(FrameEncoder::new(&bad).is_err());
    }
}
