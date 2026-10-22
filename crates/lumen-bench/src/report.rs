//! The per-run report.
//!
//! A run produces one JSON document with the counters and the per-stage
//! percentiles. Every number carries a `provisional` flag: measurements taken
//! without the photodiode rig are software pipeline latencies, not
//! glass-to-glass, and must not be presented as if they were.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use crate::record::Stats;

/// Counters of a run.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Counters {
    /// Frames the host captured.
    pub frames_sent: u64,
    /// Frames the client decoded.
    pub frames_decoded: u64,
    /// Frames the client presented.
    pub frames_presented: u64,
    /// Frames dropped because they were late.
    pub dropped_frames: u64,
    /// Decode errors.
    pub decode_errors: u64,
    /// Frames recovered through FEC.
    pub fec_recoveries: u64,
    /// Slices retransmitted.
    pub retransmits: u64,
    /// Packets lost on the wire.
    pub packets_lost: u64,
    /// Packets received.
    pub packets_received: u64,
    /// Audio packets sent.
    pub audio_packets: u64,
    /// Audio underruns on the client.
    pub audio_underruns: u64,
    /// Clipboard round trips completed.
    pub clipboard_roundtrips: u64,
    /// Resolution changes performed.
    pub resize_events: u64,
}

/// A complete run report.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RunReport {
    /// What the run was: resolution, frame rate, encoder, transport.
    pub configuration: BTreeMap<String, String>,
    /// The counters.
    pub counters: Counters,
    /// Per-stage percentiles, in microseconds.
    pub stages: BTreeMap<String, Stats>,
    /// The measured bitrate in kilobits per second.
    pub bitrate_kbps: f64,
    /// Whether these numbers are provisional.
    ///
    /// True whenever the run did not use the photodiode rig, which is the
    /// case for every run in this environment.
    pub provisional: bool,
    /// Why the numbers are provisional, when they are.
    pub provisional_reason: Option<String>,
    /// The maximum queue depth observed, which must stay at 1 or 2.
    pub queue_depth_max: usize,
}

impl RunReport {
    /// Create an empty report.
    #[must_use]
    pub fn new() -> Self {
        Self {
            provisional: true,
            provisional_reason: Some(
                "measured without the photodiode rig: these are software pipeline latencies, not \
                 glass-to-glass"
                    .to_string(),
            ),
            ..Self::default()
        }
    }

    /// Mark the report as measured with the rig.
    pub fn mark_measured(&mut self) {
        self.provisional = false;
        self.provisional_reason = None;
    }

    /// Write the report as JSON to a file.
    ///
    /// # Errors
    /// Returns an io error when the file cannot be written.
    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(path)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }

    /// Read a report from a JSON file.
    ///
    /// # Errors
    /// Returns an io error when the file cannot be read or parsed.
    pub fn read_json(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// A human readable summary.
    #[must_use]
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        out.push_str("Lumen run report\n");
        for (k, v) in &self.configuration {
            let _ = writeln!(out, "  {k}: {v}");
        }
        let c = &self.counters;
        let _ = writeln!(
            out,
            "  frames: sent {} decoded {} presented {} dropped {}",
            c.frames_sent, c.frames_decoded, c.frames_presented, c.dropped_frames
        );
        let _ = writeln!(
            out,
            "  errors: decode {} | fec recoveries {} | retransmits {} | packets lost {}",
            c.decode_errors, c.fec_recoveries, c.retransmits, c.packets_lost
        );
        let _ = writeln!(out, "  bitrate: {:.1} kbps", self.bitrate_kbps);
        out.push_str("  stage percentiles (ms):\n");
        for (stage, stats) in &self.stages {
            let _ = writeln!(
                out,
                "    {stage:<12} p50 {:.3}  p90 {:.3}  p99 {:.3}  max {:.3}  n {}",
                stats.p50_ms(),
                stats.p90_us as f64 / 1000.0,
                stats.p99_ms(),
                stats.max_us as f64 / 1000.0,
                stats.count
            );
        }
        if self.provisional {
            let _ = writeln!(
                out,
                "  PROVISIONAL: {}",
                self.provisional_reason.as_deref().unwrap_or("unspecified")
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_roundtrips_through_json() {
        let mut r = RunReport::new();
        r.counters.frames_sent = 10;
        r.configuration.insert("width".into(), "1920".into());
        let dir = std::env::temp_dir().join("lumen-report-test");
        let path = dir.join("report.json");
        r.write_json(&path).unwrap();
        let back = RunReport::read_json(&path).unwrap();
        assert_eq!(back.counters.frames_sent, 10);
        assert!(back.provisional);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn measured_reports_are_not_provisional() {
        let mut r = RunReport::new();
        r.mark_measured();
        assert!(!r.provisional);
        assert!(r.provisional_reason.is_none());
    }

    #[test]
    fn summary_mentions_provisional() {
        let r = RunReport::new();
        assert!(r.summary().contains("PROVISIONAL"));
    }
}
