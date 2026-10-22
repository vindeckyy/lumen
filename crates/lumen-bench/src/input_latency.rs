//! Input latency: from the client stamp to the host injection call.
//!
//! The client stamps every event with `CLOCK_MONOTONIC` when it reads it,
//! and the host stamps it again when the injector returns. The two clocks
//! are unrelated, so the offset between them is estimated the way NTP does it
//! and applied to the client stamp before the two are compared.
//!
//! The client's own polling interval is part of the measurement: the client
//! only learns about an event when it polls for it. The interval is an
//! explicit input and it is subtracted, so the number that comes out is the
//! cost of the transport and the injection and not the sampling period of the
//! client.
//!
//! The report is the run report of the bench crate, so it has the same shape
//! as every other bench artifact. The stage is called `host_injection`,
//! because that is the leg the numbers cover.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::record::Stats;
use crate::report::RunReport;

/// The name of the stage the latency lands in.
pub const STAGE_NAME: &str = "host_injection";

/// Collects the two stamps of every event and computes the latency of each.
///
/// The two stamps are tied together by the sequence number of the event, which
/// is the sequence number of the datagram it travelled in.
#[derive(Debug, Clone)]
pub struct InputLatencyRecorder {
    /// The client stamp of every event, by sequence number.
    client: HashMap<u64, u64>,
    /// The host stamp of every event, by sequence number.
    host: HashMap<u64, u64>,
    /// The offset to subtract from a client stamp, in microseconds.
    offset_us: i64,
    /// Whether an offset has been measured.
    has_offset: bool,
    /// The polling interval of the client, in microseconds.
    polling_interval_us: u64,
}

impl Default for InputLatencyRecorder {
    fn default() -> Self {
        Self::new(0)
    }
}

impl InputLatencyRecorder {
    /// A recorder for a client that polls every `polling_interval_us`.
    ///
    /// No offset has been measured yet, so no latency can be computed until
    /// [`Self::set_offset`] or [`Self::estimate_offset`] is called.
    #[must_use]
    pub fn new(polling_interval_us: u64) -> Self {
        Self {
            client: HashMap::new(),
            host: HashMap::new(),
            offset_us: 0,
            has_offset: false,
            polling_interval_us,
        }
    }

    /// Records the client stamp of an event.
    pub fn record_client(&mut self, sequence: u64, ts_us: u64) {
        self.client.insert(sequence, ts_us);
    }

    /// Records the host stamp of an event.
    pub fn record_host(&mut self, sequence: u64, ts_us: u64) {
        self.host.insert(sequence, ts_us);
    }

    /// Sets the offset between the client clock and the host clock.
    ///
    /// The offset is `client_clock - host_clock`, so a client stamp is mapped
    /// onto the host clock by subtracting it.
    pub fn set_offset(&mut self, offset_us: i64) {
        self.offset_us = offset_us;
        self.has_offset = true;
    }

    /// The offset between the client clock and the host clock.
    #[must_use]
    pub const fn offset_us(&self) -> i64 {
        self.offset_us
    }

    /// Whether an offset has been measured.
    #[must_use]
    pub const fn has_offset(&self) -> bool {
        self.has_offset
    }

    /// The polling interval of the client, in microseconds.
    #[must_use]
    pub const fn polling_interval_us(&self) -> u64 {
        self.polling_interval_us
    }

    /// Sets the polling interval of the client, in microseconds.
    pub fn set_polling_interval_us(&mut self, polling_interval_us: u64) {
        self.polling_interval_us = polling_interval_us;
    }

    /// Estimates the offset from a ping exchange and remembers it.
    ///
    /// The host sent a ping at `t0_host_us` on its own clock, the client
    /// answered at `t1_client_us` on its clock, and the host received the
    /// answer at `t2_host_us` on its own clock. The client's clock at the
    /// midpoint of the exchange is `(t0 + t2) / 2` on the host's clock, so the
    /// offset is `t1 - (t0 + t2) / 2`.
    pub fn estimate_offset(&mut self, t0_host_us: u64, t1_client_us: u64, t2_host_us: u64) -> i64 {
        let offset = estimate_offset(t0_host_us, t1_client_us, t2_host_us);
        self.set_offset(offset);
        offset
    }

    /// The latency of one event, in microseconds.
    ///
    /// The client stamp is mapped onto the host clock with the offset, and the
    /// polling interval of the client is taken off, so the number is the cost
    /// of the transport and the injection.
    ///
    /// It is `None` when the event has no stamp on both ends, or when the
    /// offset has not been measured, or when the arithmetic does not fit.
    #[must_use]
    pub fn latency(&self, sequence: u64) -> Option<i64> {
        if !self.has_offset {
            return None;
        }
        let client = i64::try_from(*self.client.get(&sequence)?).ok()?;
        let host = i64::try_from(*self.host.get(&sequence)?).ok()?;
        let polling = i64::try_from(self.polling_interval_us).ok()?;
        host.checked_sub(client.checked_sub(self.offset_us)?)?
            .checked_sub(polling)
    }

    /// The latency of every event that has a stamp on both ends.
    ///
    /// A negative latency means the polling interval of the client was
    /// larger than the latency itself, so the sample is dropped: the number
    /// would be meaningless.
    #[must_use]
    pub fn samples(&self) -> Vec<i64> {
        let mut out: Vec<i64> = self
            .client
            .keys()
            .filter_map(|sequence| self.latency(*sequence))
            .filter(|latency| *latency >= 0)
            .collect();
        out.sort_unstable();
        out
    }

    /// The percentiles of the latency.
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats::from_values(&self.samples())
    }

    /// The number of events that have a stamp on both ends.
    #[must_use]
    pub fn matched(&self) -> u64 {
        let count = self
            .client
            .keys()
            .filter(|sequence| self.host.contains_key(*sequence))
            .count();
        u64::try_from(count).unwrap_or(0)
    }

    /// The number of events that have a stamp on one end only.
    #[must_use]
    pub fn unmatched(&self) -> u64 {
        let only_client = self
            .client
            .keys()
            .filter(|sequence| !self.host.contains_key(*sequence))
            .count();
        let only_host = self
            .host
            .keys()
            .filter(|sequence| !self.client.contains_key(*sequence))
            .count();
        u64::try_from(only_client + only_host).unwrap_or(0)
    }
}

/// Estimates the offset between the client clock and the host clock.
///
/// The host sent a ping at `t0_host_us` on its own clock, the client answered
/// at `t1_client_us` on its clock, and the host received the answer at
/// `t2_host_us` on its own clock. The client's clock at the midpoint of the
/// exchange is `(t0 + t2) / 2` on the host's clock, so the offset is
/// `t1 - (t0 + t2) / 2`.
///
/// This is the estimator of the host service, so both ends of a session read
/// the same number. It assumes the two directions take the same time, which on
/// a LAN is accurate to well under a millisecond.
#[must_use]
pub fn estimate_offset(t0_host_us: u64, t1_client_us: u64, t2_host_us: u64) -> i64 {
    let midpoint = i64::try_from(t0_host_us)
        .unwrap_or(i64::MAX)
        .saturating_add(i64::try_from(t2_host_us).unwrap_or(i64::MAX))
        / 2;
    i64::try_from(t1_client_us)
        .unwrap_or(i64::MAX)
        .saturating_sub(midpoint)
}

/// Estimates the offset from several ping exchanges.
///
/// The exchange with the smallest round trip is the one that was least
/// delayed, so its offset is the tightest. When every exchange was delayed
/// by more than a millisecond over the smallest one, the median of all of
/// them is used.
#[must_use]
pub fn estimate_offset_from_samples(samples: &[(u64, u64, u64)]) -> Option<i64> {
    if samples.is_empty() {
        return None;
    }
    let fastest = samples
        .iter()
        .map(|(t0, _, t2)| t2.saturating_sub(*t0))
        .min()
        .unwrap_or(0);
    let mut offsets: Vec<i64> = samples
        .iter()
        .filter(|(t0, _, t2)| t2.saturating_sub(*t0) <= fastest + 1_000)
        .map(|(t0, t1, t2)| estimate_offset(*t0, *t1, *t2))
        .collect();
    if offsets.is_empty() {
        offsets = samples
            .iter()
            .map(|(t0, t1, t2)| estimate_offset(*t0, *t1, *t2))
            .collect();
    }
    offsets.sort_unstable();
    offsets.get(offsets.len() / 2).copied()
}

/// Builds the report of a run.
///
/// The stage is called `host_injection`, because that is the leg the numbers
/// cover: from the client reading the event to the host calling the injector.
#[must_use]
pub fn build_report(
    recorder: &InputLatencyRecorder,
    configuration: BTreeMap<String, String>,
) -> RunReport {
    let mut report = RunReport::new();
    report.configuration = configuration;
    report.counters.packets_received = recorder.matched();
    report.counters.packets_lost = recorder.unmatched();
    report
        .stages
        .insert(STAGE_NAME.to_owned(), recorder.stats());
    report
}

/// Writes the report of a run to a file.
///
/// # Errors
/// Returns an io error when the file cannot be written.
pub fn write_report(path: &Path, report: &RunReport) -> std::io::Result<()> {
    report.write_json(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorder with an offset of zero and no polling interval.
    fn recorder() -> InputLatencyRecorder {
        let mut r = InputLatencyRecorder::new(0);
        r.set_offset(0);
        r
    }

    #[test]
    fn a_zero_offset_gives_the_difference_of_the_two_stamps() {
        let mut r = recorder();
        r.record_client(1, 1_000_000);
        r.record_host(1, 1_000_450);
        assert_eq!(r.latency(1), Some(450));
    }

    #[test]
    fn a_known_offset_is_applied_to_the_client_stamp() {
        let mut r = InputLatencyRecorder::new(0);
        // The client clock runs 10 ms ahead of the host clock, so the client
        // stamp of the event is 10 ms above the host time of the event.
        r.set_offset(10_000);
        r.record_client(1, 1_010_000);
        r.record_host(1, 1_000_450);
        assert_eq!(r.latency(1), Some(450));
    }

    #[test]
    fn the_polling_interval_of_the_client_is_taken_off() {
        let mut r = InputLatencyRecorder::new(100);
        r.set_offset(0);
        r.record_client(1, 1_000_000);
        r.record_host(1, 1_000_450);
        assert_eq!(r.latency(1), Some(350));
    }

    #[test]
    fn a_latency_that_is_smaller_than_the_polling_interval_is_dropped() {
        let mut r = InputLatencyRecorder::new(1_000);
        r.set_offset(0);
        r.record_client(1, 1_000_000);
        r.record_host(1, 1_000_450);
        assert_eq!(r.latency(1), Some(-550));
        assert!(r.samples().is_empty(), "the sample is meaningless");
        assert_eq!(r.stats().count, 0);
    }

    #[test]
    fn the_polling_interval_can_be_changed_after_the_fact() {
        let mut r = recorder();
        r.record_client(1, 1_000_000);
        r.record_host(1, 1_000_450);
        assert_eq!(r.latency(1), Some(450));
        r.set_polling_interval_us(100);
        assert_eq!(r.latency(1), Some(350));
    }

    #[test]
    fn an_event_without_a_stamp_on_both_ends_has_no_latency() {
        let mut r = recorder();
        r.record_client(1, 1_000_000);
        r.record_host(2, 1_000_450);
        assert_eq!(r.latency(1), None);
        assert_eq!(r.latency(2), None);
        assert_eq!(r.matched(), 0);
        assert_eq!(r.unmatched(), 2);
    }

    #[test]
    fn without_an_offset_there_is_no_latency() {
        let mut r = InputLatencyRecorder::new(0);
        r.record_client(1, 1_000_000);
        r.record_host(1, 1_000_450);
        assert_eq!(r.latency(1), None);
        assert!(!r.has_offset());
        assert_eq!(r.samples(), Vec::<i64>::new());
    }

    #[test]
    fn the_percentiles_come_out_of_the_samples() {
        let mut r = recorder();
        for index in 0..100u64 {
            r.record_client(index, 1_000_000);
            r.record_host(index, 1_000_000 + index + 1);
        }
        let stats = r.stats();
        assert_eq!(stats.count, 100);
        assert_eq!(stats.min_us, 1);
        assert_eq!(stats.max_us, 100);
        assert_eq!(stats.p50_us, 50);
        assert_eq!(stats.p99_us, 99);
    }

    #[test]
    fn the_offset_of_identical_clocks_is_zero() {
        // The client answered at the midpoint of the exchange, so the two
        // clocks agree.
        assert_eq!(estimate_offset(1_000_000, 1_000_500, 1_001_000), 0);
        // The client clock runs 10 ms ahead of the host clock.
        assert_eq!(estimate_offset(1_000_000, 1_010_500, 1_001_000), 10_000);
    }

    #[test]
    fn the_recorder_remembers_the_offset_it_estimated() {
        let mut r = InputLatencyRecorder::new(0);
        let offset = r.estimate_offset(1_000_000, 1_010_500, 1_001_000);
        assert_eq!(offset, 10_000);
        assert_eq!(r.offset_us(), 10_000);
        assert!(r.has_offset());
    }

    #[test]
    fn the_fastest_exchange_gives_the_offset() {
        let samples = [
            (1_000_000, 1_010_500, 1_001_000),
            (2_000_000, 2_010_500, 2_001_000),
            (3_000_000, 3_020_500, 3_003_000),
        ];
        assert_eq!(estimate_offset_from_samples(&samples), Some(10_000));
    }

    #[test]
    fn an_offset_needs_a_sample() {
        assert_eq!(estimate_offset_from_samples(&[]), None);
    }

    #[test]
    fn the_report_carries_the_percentiles() {
        let mut r = recorder();
        for index in 0..10u64 {
            r.record_client(index, 1_000_000);
            r.record_host(index, 1_000_100 + index);
        }
        let mut configuration = BTreeMap::new();
        configuration.insert("mode".to_owned(), "netns".to_owned());
        let report = build_report(&r, configuration);
        let stats = report.stages.get(STAGE_NAME).expect("the stage");
        assert_eq!(stats.count, 10);
        assert_eq!(stats.p50_us, 104);
        assert_eq!(report.counters.packets_received, 10);
        assert_eq!(report.counters.packets_lost, 0);
        assert!(report.provisional, "the numbers are software numbers");
        assert_eq!(
            report.configuration.get("mode").map(String::as_str),
            Some("netns")
        );
    }
}
