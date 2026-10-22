//! Per-stage timing of the pipeline.
//!
//! The host times its own stages on its own clock. The client times its own
//! stages on its clock. The two clocks are different, so the host estimates
//! the offset between them from a ping exchange and maps the client's
//! timestamps onto its own.
//!
//! The result is one sample per frame for each stage, which the run report
//! turns into percentiles.
//!
//! The stages are the intervals between consecutive timestamps:
//!
//! | Stage | Interval |
//! |---|---|
//! | `capture` | the capture timestamp to the start of the encode |
//! | `encode` | the encoder's own time |
//! | `send` | the end of the encode to the client receiving the access unit |
//! | `recv` | the reassembly to the start of the decode |
//! | `decode` | the decoder's own time |
//! | `present` | the end of the decode to the frame being presented |
//! | `end_to_end` | the capture timestamp to the presentation |

use std::collections::HashMap;

/// The stage timestamps of one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStages {
    /// The capture timestamp of the frame, host clock. It is the moment the
    /// compositor finished copying the frame into the capture buffer.
    pub capture_ts_us: Option<u64>,
    /// When the encoder started, host clock.
    pub encode_start_ts_us: Option<u64>,
    /// When the encoder finished, host clock.
    pub encode_end_ts_us: Option<u64>,
    /// When the datagrams went out, host clock.
    pub send_ts_us: Option<u64>,
    /// When the client reassembled the access unit, client clock.
    pub recv_ts_us: Option<u64>,
    /// When the client started decoding, client clock.
    pub decode_start_ts_us: Option<u64>,
    /// When the client finished decoding, client clock.
    pub decode_ts_us: Option<u64>,
    /// When the client presented the frame, client clock.
    pub present_ts_us: Option<u64>,
}

/// The stage names the report uses, in pipeline order.
pub const STAGE_NAMES: [&str; 7] = [
    "capture",
    "encode",
    "send",
    "recv",
    "decode",
    "present",
    "end_to_end",
];

/// How many frames the recorder keeps for the percentiles.
///
/// The recorder held every frame of the session, which made the memory grow
/// for as long as the host ran and made every percentile calculation cost
/// the length of the whole session. A live window is what a dashboard wants
/// anyway, and it costs the same whatever the session has been doing.
const MAX_FRAMES: usize = 512;

/// The stage timings of a run.
#[derive(Debug, Default)]
pub struct StageRecorder {
    frames: HashMap<u32, FrameStages>,
    /// The offset to subtract from a client timestamp to get a host timestamp.
    offset_us: i64,
    /// Whether an offset has been measured.
    has_offset: bool,
    /// The number of frames whose timings were incomplete.
    incomplete_frames: u64,
}

impl StageRecorder {
    /// Create an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The capture timestamp of a frame, on the host clock.
    #[must_use]
    pub fn capture_timestamp(&self, frame_id: u32) -> Option<u64> {
        self.frames.get(&frame_id).and_then(|f| f.capture_ts_us)
    }

    /// Whether a frame may still be retransmitted.
    ///
    /// The plan's deadline is the frame's capture timestamp plus 1.5 frame
    /// intervals. Past it the frame is dropped rather than waited for.
    #[must_use]
    pub fn may_retransmit(&self, frame_id: u32, now_us: u64, frame_interval_us: u64) -> bool {
        let Some(capture) = self.capture_timestamp(frame_id) else {
            // A frame the host does not know about cannot be judged, so it is
            // allowed: the client asked for it explicitly.
            return true;
        };
        let deadline = capture
            .saturating_add(frame_interval_us)
            .saturating_add(frame_interval_us / 2);
        now_us < deadline
    }

    /// Record the host's timestamps for a frame.
    pub fn record_host(
        &mut self,
        frame_id: u32,
        capture_ts_us: u64,
        encode_start_ts_us: u64,
        encode_end_ts_us: u64,
        send_ts_us: u64,
    ) {
        let entry = self.frames.entry(frame_id).or_default();
        entry.capture_ts_us = Some(capture_ts_us);
        entry.encode_start_ts_us = Some(encode_start_ts_us);
        entry.encode_end_ts_us = Some(encode_end_ts_us);
        entry.send_ts_us = Some(send_ts_us);
    }

    /// Record the host's timestamps up to the end of the encode.
    pub fn record_host_start(
        &mut self,
        frame_id: u32,
        capture_ts_us: u64,
        encode_start_ts_us: u64,
        encode_end_ts_us: u64,
    ) {
        let entry = self.frames.entry(frame_id).or_default();
        entry.capture_ts_us = Some(capture_ts_us);
        entry.encode_start_ts_us = Some(encode_start_ts_us);
        entry.encode_end_ts_us = Some(encode_end_ts_us);
    }

    /// Record the host's send timestamp for a frame.
    pub fn record_send(&mut self, frame_id: u32, ts_us: u64) {
        let entry = self.frames.entry(frame_id).or_default();
        entry.send_ts_us = Some(ts_us);
    }

    /// Record the client's timestamps for a frame.
    pub fn record_client(
        &mut self,
        frame_id: u32,
        recv_ts_us: u64,
        decode_start_ts_us: u64,
        decode_ts_us: u64,
        present_ts_us: u64,
    ) {
        let entry = self.frames.entry(frame_id).or_default();
        entry.recv_ts_us = Some(recv_ts_us);
        entry.decode_start_ts_us = Some(decode_start_ts_us);
        entry.decode_ts_us = Some(decode_ts_us);
        entry.present_ts_us = Some(present_ts_us);
        self.prune();
    }

    /// Drop the oldest frames, so the recorder does not grow for as long as
    /// the host runs.
    fn prune(&mut self) {
        if self.frames.len() <= MAX_FRAMES {
            return;
        }
        // The frame identifiers grow, so the oldest frame has the smallest
        // one. Sorting them is cheap next to the memory it saves.
        let mut ids: Vec<u32> = self.frames.keys().copied().collect();
        ids.sort_unstable();
        let excess = self.frames.len() - MAX_FRAMES;
        for id in ids.into_iter().take(excess) {
            self.frames.remove(&id);
        }
    }

    /// Set the offset between the client clock and the host clock.
    ///
    /// The offset is `client_clock - host_clock`, so a client timestamp is
    /// mapped onto the host clock by subtracting it.
    pub fn set_offset(&mut self, offset_us: i64) {
        self.offset_us = offset_us;
        self.has_offset = true;
    }

    /// The offset between the client clock and the host clock.
    #[must_use]
    pub fn offset_us(&self) -> i64 {
        self.offset_us
    }

    /// Whether an offset has been measured.
    #[must_use]
    pub fn has_offset(&self) -> bool {
        self.has_offset
    }

    /// Map a client timestamp onto the host clock.
    fn to_host(&self, client_ts_us: u64) -> Option<i64> {
        if !self.has_offset {
            return None;
        }
        i64::try_from(client_ts_us)
            .ok()
            .map(|ts| ts - self.offset_us)
    }

    /// The per-stage duration samples, in microseconds.
    ///
    /// A frame whose timings are incomplete is skipped.
    #[must_use]
    pub fn samples(&mut self) -> StageSamples {
        let mut out = StageSamples::default();
        for stages in self.frames.values() {
            let host = |ts: Option<u64>| ts.and_then(|ts| i64::try_from(ts).ok());
            let client = |ts: Option<u64>| ts.and_then(|ts| self.to_host(ts));

            let capture = host(stages.capture_ts_us);
            let encode_start = host(stages.encode_start_ts_us);
            let encode_end = host(stages.encode_end_ts_us);
            let send = host(stages.send_ts_us);
            let recv = client(stages.recv_ts_us);
            let decode_start = client(stages.decode_start_ts_us);
            let decode = client(stages.decode_ts_us);
            let present = client(stages.present_ts_us);

            let (Some(capture), Some(encode_start), Some(encode_end), Some(send)) =
                (capture, encode_start, encode_end, send)
            else {
                self.incomplete_frames += 1;
                continue;
            };

            out.push("capture", encode_start - capture);
            out.push("encode", encode_end - encode_start);
            if let (Some(recv), Some(decode_start), Some(decode), Some(present)) =
                (recv, decode_start, decode, present)
            {
                out.push("send", recv - send);
                out.push("recv", decode_start - recv);
                out.push("decode", decode - decode_start);
                out.push("present", present - decode);
                out.push("end_to_end", present - capture);
            }
        }
        out
    }

    /// The number of frames whose timings were incomplete.
    #[must_use]
    pub fn incomplete_frames(&self) -> u64 {
        self.incomplete_frames
    }
}

/// The per-stage duration samples.
#[derive(Debug, Default, Clone)]
pub struct StageSamples {
    samples: std::collections::BTreeMap<&'static str, Vec<i64>>,
}

impl StageSamples {
    /// Add one sample to a stage.
    ///
    /// A negative interval is dropped. It can only come from a clock that is
    /// not monotonic or from an offset estimate that is out by more than the
    /// network delay, and either way the number is meaningless.
    fn push(&mut self, stage: &'static str, value: i64) {
        if value < 0 {
            return;
        }
        self.samples.entry(stage).or_default().push(value);
    }

    /// The samples of one stage.
    #[must_use]
    pub fn get(&self, stage: &str) -> Option<&[i64]> {
        self.samples.get(stage).map(Vec::as_slice)
    }

    /// The stages that hold samples.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &Vec<i64>)> {
        self.samples.iter().map(|(stage, values)| (*stage, values))
    }

    /// Whether no stage holds a sample.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.values().all(Vec::is_empty)
    }

    /// The samples as a plain map, for a report.
    #[must_use]
    pub fn to_map(&self) -> std::collections::BTreeMap<String, Vec<i64>> {
        self.samples
            .iter()
            .map(|(stage, values)| ((*stage).to_string(), values.clone()))
            .collect()
    }
}

/// Estimate the offset between the client clock and the host clock.
///
/// The host sent a ping at `t0` on its own clock, the client answered at `t1`
/// on its clock, and the host received the answer at `t2` on its clock. The
/// client's clock at the midpoint of the exchange is `(t0 + t2) / 2` on the
/// host's clock, so the offset is `t1 - (t0 + t2) / 2`.
///
/// The estimate assumes the two directions take the same time. On a LAN the
/// error is well under a millisecond.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn full_frame(recorder: &mut StageRecorder, offset: i64) {
        recorder.set_offset(offset);
        recorder.record_host(1, 1_000_000, 1_001_000, 1_005_000, 1_006_000);
        let shift = u64::try_from(offset).unwrap_or(0);
        recorder.record_client(
            1,
            1_006_000 + shift,
            1_007_000 + shift,
            1_011_000 + shift,
            1_013_000 + shift,
        );
    }

    #[test]
    fn the_offset_of_identical_clocks_is_zero() {
        // The client answered at the midpoint of the host's exchange, so the
        // two clocks agree.
        assert_eq!(estimate_offset(1_000_000, 1_000_500, 1_001_000), 0);
        // The client clock runs 10 ms ahead of the host clock.
        assert_eq!(estimate_offset(1_000_000, 1_010_500, 1_001_000), 10_000);
    }

    #[test]
    fn a_full_frame_gives_one_sample_per_stage() {
        let mut recorder = StageRecorder::new();
        full_frame(&mut recorder, 0);
        let samples = recorder.samples();
        assert_eq!(samples.get("capture"), Some([1_000].as_slice()));
        assert_eq!(samples.get("encode"), Some([4_000].as_slice()));
        assert_eq!(samples.get("send"), Some([0].as_slice()));
        assert_eq!(samples.get("recv"), Some([1_000].as_slice()));
        assert_eq!(samples.get("decode"), Some([4_000].as_slice()));
        assert_eq!(samples.get("present"), Some([2_000].as_slice()));
        assert_eq!(samples.get("end_to_end"), Some([13_000].as_slice()));
    }

    #[test]
    fn the_client_timestamps_are_mapped_onto_the_host_clock() {
        let mut recorder = StageRecorder::new();
        // The client clock runs 10 ms ahead.
        full_frame(&mut recorder, 10_000);
        let samples = recorder.samples();
        assert_eq!(samples.get("send"), Some([0].as_slice()));
        assert_eq!(samples.get("recv"), Some([1_000].as_slice()));
        assert_eq!(samples.get("end_to_end"), Some([13_000].as_slice()));
    }

    #[test]
    fn without_an_offset_the_client_stages_are_skipped() {
        let mut recorder = StageRecorder::new();
        recorder.record_host(1, 1_000_000, 1_001_000, 1_005_000, 1_006_000);
        recorder.record_client(1, 1_006_000, 1_007_000, 1_011_000, 1_013_000);
        let samples = recorder.samples();
        assert_eq!(samples.get("capture"), Some([1_000].as_slice()));
        assert!(samples.get("recv").is_none());
        assert!(samples.get("end_to_end").is_none());
    }

    #[test]
    fn a_negative_interval_is_dropped() {
        let mut recorder = StageRecorder::new();
        recorder.set_offset(0);
        // The client's clock is behind the host's, so the mapped timestamps
        // come out before the host's send.
        recorder.record_host(1, 1_000_000, 1_001_000, 1_005_000, 1_006_000);
        recorder.record_client(1, 900_000, 901_000, 902_000, 903_000);
        let samples = recorder.samples();
        assert!(samples.get("send").is_none());
        assert!(samples.get("end_to_end").is_none());
        // The host's own stages are still there.
        assert_eq!(samples.get("capture"), Some([1_000].as_slice()));
    }

    #[test]
    fn a_frame_without_a_capture_timestamp_is_skipped() {
        let mut recorder = StageRecorder::new();
        recorder.set_offset(0);
        recorder.record_client(1, 1_000_000, 1_001_000, 1_002_000, 1_003_000);
        let samples = recorder.samples();
        assert!(samples.is_empty());
        assert_eq!(recorder.incomplete_frames(), 1);
    }
}
