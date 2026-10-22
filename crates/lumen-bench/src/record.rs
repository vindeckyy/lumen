//! Per-stage timestamp records and the percentile report.

use std::collections::BTreeMap;
use std::time::Duration;

/// The pipeline stages that are timestamped on both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// The host captured a frame.
    Capture,
    /// The host finished encoding the frame.
    Encode,
    /// The host finished sending the frame.
    Send,
    /// The client received the frame.
    Recv,
    /// The client finished decoding the frame.
    Decode,
    /// The client presented the frame.
    Present,
}

impl Stage {
    /// All stages, in pipeline order.
    pub const ALL: [Stage; 6] = [
        Stage::Capture,
        Stage::Encode,
        Stage::Send,
        Stage::Recv,
        Stage::Decode,
        Stage::Present,
    ];

    /// The stage name used in reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Encode => "encode",
            Self::Send => "send",
            Self::Recv => "recv",
            Self::Decode => "decode",
            Self::Present => "present",
        }
    }
}

/// A single timestamped event.
#[derive(Debug, Clone, Copy)]
pub struct Event {
    /// The stage the event belongs to.
    pub stage: Stage,
    /// The frame the event belongs to.
    pub frame_id: u32,
    /// The timestamp, in microseconds on the sender's clock.
    pub ts_us: u64,
}

/// A set of recorded events.
#[derive(Debug, Default, Clone)]
pub struct Recorder {
    events: Vec<Event>,
    /// The clock offset to add to remote timestamps, in microseconds.
    pub clock_offset_us: i64,
}

impl Recorder {
    /// Create an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an event.
    pub fn record(&mut self, stage: Stage, frame_id: u32, ts_us: u64) {
        self.events.push(Event {
            stage,
            frame_id,
            ts_us,
        });
    }

    /// Number of recorded events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The recorded events.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Map a remote timestamp onto the local clock.
    #[must_use]
    pub fn to_local(&self, remote_ts_us: u64) -> i64 {
        i64::try_from(remote_ts_us).unwrap_or(i64::MAX) + self.clock_offset_us
    }

    /// Compute the per-stage duration statistics.
    ///
    /// For each stage the duration is measured from the previous stage of the
    /// same frame, so the numbers are the marginal cost of each stage. The
    /// `present` stage is measured against the `capture` timestamp mapped
    /// onto the local clock, which is the end-to-end pipeline latency.
    #[must_use]
    pub fn stage_stats(&self) -> BTreeMap<&'static str, Stats> {
        let mut per_frame: BTreeMap<u32, BTreeMap<Stage, u64>> = BTreeMap::new();
        for event in &self.events {
            per_frame
                .entry(event.frame_id)
                .or_default()
                .insert(event.stage, event.ts_us);
        }

        let mut durations: BTreeMap<Stage, Vec<i64>> = BTreeMap::new();
        let mut end_to_end: Vec<i64> = Vec::new();
        for stages in per_frame.values() {
            let mut previous: Option<(Stage, u64)> = None;
            for stage in Stage::ALL {
                let Some(ts) = stages.get(&stage) else {
                    continue;
                };
                if let Some((_, prev_ts)) = previous
                    && let Some(delta) = ts.checked_sub(prev_ts)
                {
                    durations
                        .entry(stage)
                        .or_default()
                        .push(delta.cast_signed());
                }
                previous = Some((stage, *ts));
            }
            if let (Some(capture), Some(present)) =
                (stages.get(&Stage::Capture), stages.get(&Stage::Present))
            {
                let local_present = self.to_local(*present);
                if let Some(delta) =
                    local_present.checked_sub(i64::try_from(*capture).unwrap_or(i64::MAX))
                {
                    end_to_end.push(delta);
                }
            }
        }

        let mut out = BTreeMap::new();
        for (stage, values) in durations {
            out.insert(stage.name(), Stats::from_values(&values));
        }
        if !end_to_end.is_empty() {
            out.insert("end_to_end", Stats::from_values(&end_to_end));
        }
        out
    }
}

/// Percentile statistics of a set of durations in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    /// Number of samples.
    pub count: usize,
    /// 50th percentile.
    pub p50_us: i64,
    /// 90th percentile.
    pub p90_us: i64,
    /// 99th percentile.
    pub p99_us: i64,
    /// Largest value.
    pub max_us: i64,
    /// Smallest value.
    pub min_us: i64,
    /// Arithmetic mean.
    pub mean_us: i64,
}

impl Stats {
    /// Compute the statistics of a set of values.
    #[must_use]
    pub fn from_values(values: &[i64]) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let sum: i128 = sorted.iter().map(|v| i128::from(*v)).sum();
        Self {
            count: sorted.len(),
            p50_us: percentile(&sorted, 0.50),
            p90_us: percentile(&sorted, 0.90),
            p99_us: percentile(&sorted, 0.99),
            max_us: *sorted.last().unwrap_or(&0),
            min_us: *sorted.first().unwrap_or(&0),
            mean_us: (sum / i128::try_from(sorted.len()).unwrap_or(1)) as i64,
        }
    }

    /// The 50th percentile in milliseconds.
    #[must_use]
    pub fn p50_ms(&self) -> f64 {
        self.p50_us as f64 / 1000.0
    }

    /// The 99th percentile in milliseconds.
    #[must_use]
    pub fn p99_ms(&self) -> f64 {
        self.p99_us as f64 / 1000.0
    }
}

/// The `q` quantile of a sorted slice, using nearest rank.
fn percentile(sorted: &[i64], q: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

/// Estimate the clock offset between two peers.
///
/// The control channel carries timestamped ping and pong pairs. The offset
/// is the mean of `t_remote - t_local` over the round trips, which cancels
/// the asymmetric path delay only in the average case; the minimum round
/// trip time is used to pick the least-delayed sample, which gives the
/// tightest bound on the offset.
///
/// # Errors
/// Returns an error string when there are no samples.
pub fn estimate_clock_offset(samples: &[(Duration, Duration)]) -> Result<i64, String> {
    if samples.is_empty() {
        return Err("no clock samples".into());
    }
    // samples: (local send time, remote receive time) in microseconds
    let min_rtt = samples
        .iter()
        .map(|(local, remote)| {
            let rtt = remote.saturating_sub(*local);
            rtt.as_micros() as i64
        })
        .min()
        .unwrap_or(0);
    let mut offsets: Vec<i64> = samples
        .iter()
        .filter_map(|(local, remote)| {
            let rtt = remote.saturating_sub(*local);
            let rtt_us = rtt.as_micros() as i64;
            if rtt_us > min_rtt + 1000 {
                return None;
            }
            Some(remote.as_micros() as i64 - local.as_micros() as i64)
        })
        .collect();
    if offsets.is_empty() {
        offsets = samples
            .iter()
            .map(|(local, remote)| remote.as_micros() as i64 - local.as_micros() as i64)
            .collect();
    }
    offsets.sort_unstable();
    Ok(offsets[offsets.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_compute_percentiles() {
        let values: Vec<i64> = (1..=100).collect();
        let s = Stats::from_values(&values);
        assert_eq!(s.count, 100);
        assert_eq!(s.min_us, 1);
        assert_eq!(s.max_us, 100);
        assert_eq!(s.p50_us, 50);
        assert_eq!(s.p99_us, 99);
    }

    #[test]
    fn empty_stats_are_zero() {
        let s = Stats::from_values(&[]);
        assert_eq!(s.count, 0);
        assert_eq!(s.p50_us, 0);
    }

    #[test]
    fn recorder_measures_stage_deltas() {
        let mut r = Recorder::new();
        r.record(Stage::Capture, 1, 1000);
        r.record(Stage::Encode, 1, 3000);
        r.record(Stage::Send, 1, 3500);
        let stats = r.stage_stats();
        assert_eq!(stats["encode"].p50_us, 2000);
        assert_eq!(stats["send"].p50_us, 500);
    }

    #[test]
    fn end_to_end_uses_the_clock_offset() {
        let mut r = Recorder::new();
        r.clock_offset_us = 100;
        r.record(Stage::Capture, 1, 1000);
        r.record(Stage::Present, 1, 5000);
        let stats = r.stage_stats();
        assert_eq!(stats["end_to_end"].p50_us, 4100);
    }

    #[test]
    fn clock_offset_estimation_uses_the_fastest_sample() {
        let samples = vec![
            (Duration::from_micros(1000), Duration::from_micros(1500)),
            (Duration::from_micros(2000), Duration::from_micros(2500)),
        ];
        let offset = estimate_clock_offset(&samples).unwrap();
        assert_eq!(offset, 500);
    }

    #[test]
    fn clock_offset_needs_samples() {
        assert!(estimate_clock_offset(&[]).is_err());
    }
}
