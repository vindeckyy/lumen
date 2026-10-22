//! Evenly-spaced disturbance detector.
//!
//! Random loss is bursty, but a stable multi-second period is a machine: a display-topology
//! poller, a virtual-display present timer, a compositor that wakes every second. The detector
//! names the period in the log, so the cycle is identified instead of guessed at.
//!
//! The idea is punktfunk's `pf-frame/src/metronome.rs` (MIT OR Apache-2.0, Copyright (c) 2026
//! unom - Enrico Bühler).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Detects a disturbance that repeats with a stable period.
#[derive(Debug, Default)]
pub struct Metronome {
    /// The recent disturbances, oldest first.
    events: VecDeque<Instant>,
    /// When the last period was reported.
    last_warn: Option<Instant>,
}

impl Metronome {
    /// Two disturbances closer than this are one jolt: an IDR cooldown re-issue and its
    /// immediate follow-up.
    const COALESCE: Duration = Duration::from_millis(1500);
    /// Four events (three gaps) reject irregular bursts.
    const STREAK: usize = 4;
    /// Twenty percent of the mean gap: clock jitter, not another period.
    const TOLERANCE: f64 = 0.2;
    /// Thirty seconds, so a cycle that persists does not flood the log.
    const REWARN: Duration = Duration::from_secs(30);

    /// A detector with no history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a disturbance at `now` and returns the period when a cycle is recognised.
    pub fn note(&mut self, now: Instant) -> Option<Duration> {
        if self
            .events
            .back()
            .is_some_and(|last| now.duration_since(*last) < Self::COALESCE)
        {
            return None;
        }
        self.events.push_back(now);
        if self.events.len() > Self::STREAK {
            self.events.pop_front();
        }
        if self.events.len() < Self::STREAK {
            return None;
        }
        let gaps: Vec<f64> = self
            .events
            .iter()
            .zip(self.events.iter().skip(1))
            .map(|(a, b)| b.duration_since(*a).as_secs_f64())
            .collect();
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        if mean <= 0.0
            || gaps
                .iter()
                .any(|g| (g - mean).abs() > mean * Self::TOLERANCE)
        {
            return None;
        }
        if self
            .last_warn
            .is_some_and(|t| now.duration_since(t) < Self::REWARN)
        {
            return None;
        }
        self.last_warn = Some(now);
        Some(Duration::from_secs_f64(mean))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cadence_run(offsets_ms: &[u64]) -> Vec<Option<Duration>> {
        let base = Instant::now();
        let mut c = Metronome::new();
        offsets_ms
            .iter()
            .map(|ms| c.note(base + Duration::from_millis(*ms)))
            .collect()
    }

    #[test]
    fn a_cadence_is_detected() {
        let out = cadence_run(&[0, 4_000, 8_100, 11_950]);
        assert_eq!(out[..3], [None, None, None]);
        let period = out[3].expect("the cycle is detected");
        assert!(
            (period.as_secs_f64() - 3.98).abs() < 0.2,
            "period={period:?}"
        );
    }

    #[test]
    fn a_jolt_pair_is_counted_once() {
        let out = cadence_run(&[0, 700, 4_000, 4_700, 8_000, 8_650, 12_000]);
        assert!(out[..6].iter().all(Option::is_none));
        let period = out[6].expect("the cycle is detected");
        assert!(
            (period.as_secs_f64() - 4.0).abs() < 0.2,
            "period={period:?}"
        );
    }

    #[test]
    fn an_irregular_burst_is_ignored() {
        assert!(
            cadence_run(&[0, 2_000, 9_000, 12_500, 21_000])
                .iter()
                .all(Option::is_none)
        );
    }

    #[test]
    fn a_persisting_cycle_is_reported_at_most_every_thirty_seconds() {
        let offsets: Vec<u64> = (0..12).map(|i| i * 4_000).collect();
        let out = cadence_run(&offsets);
        let warned: Vec<usize> = out
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.map(|_| i))
            .collect();
        assert_eq!(warned, vec![3, 11]);
    }
}
