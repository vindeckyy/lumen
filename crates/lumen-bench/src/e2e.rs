//! The merge of a host run and a client run into one report.
//!
//! The host and the client each write their own counters as JSON. This module
//! reads them back and produces the run report that `artifacts/m1/e2e.json`
//! holds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::record::Stats;

/// The host half of a run.
///
/// The field names must match the ones `lumen-host-service` writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostReport {
    /// Frames the capture stage produced.
    pub frames_captured: u64,
    /// Frames converted to NV12 on the GPU. Older host reports do not carry it.
    #[serde(default)]
    pub frames_gpu_converted: u64,
    /// Frames the encoder turned into access units.
    pub frames_encoded: u64,
    /// Datagrams written to the media socket.
    pub datagrams_sent: u64,
    /// Bytes written to the media socket.
    pub bytes_sent: u64,
    /// Errors raised by the capture stage.
    pub capture_errors: u64,
    /// Errors raised by the encode stage.
    pub encode_errors: u64,
    /// Keyframes the client asked for.
    pub keyframe_requests: u64,
    /// Slices resent after a NACK.
    pub retransmits: u64,
    /// How long the host ran, in milliseconds.
    pub duration_ms: u64,
    /// The per-stage duration samples, in microseconds. The client clock is
    /// mapped onto the host clock, so every sample is one clock.
    #[serde(default)]
    pub stage_samples: BTreeMap<String, Vec<i64>>,
    /// Audio frames the host sent.
    #[serde(default)]
    pub audio_packets: u64,
    /// Clipboard transfers that completed.
    #[serde(default)]
    pub clipboard_roundtrips: u64,
    /// Resolution changes the host told the client about.
    #[serde(default)]
    pub resize_events: u64,
    /// How far the audio was from the video frame it aligns to, in
    /// microseconds. One sample per played frame.
    #[serde(default)]
    pub a_vs_skew_us: Vec<i64>,
}

/// The client half of a run.
///
/// The field names must match the ones `lumen-client-desktop` writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientReport {
    /// Datagrams read from the media socket.
    pub datagrams_received: u64,
    /// Access units reassembled from the slices.
    pub access_units: u64,
    /// Frames the decoder produced.
    pub frames_decoded: u64,
    /// Frames handed to the compositor.
    pub frames_presented: u64,
    /// Frames presented without a CPU copy. Older client reports do not carry it.
    #[serde(default)]
    pub zero_copy: u64,
    /// Frames the decoder could not decode.
    pub decode_errors: u64,
    /// NACKs sent to the host.
    pub nacks_sent: u64,
    /// On-demand keyframes requested from the host.
    pub idr_requests: u64,
    /// Access units rebuilt from FEC parity.
    pub frames_rebuilt: u64,
    /// How long the client ran, in milliseconds.
    pub duration_ms: u64,
    /// Audio frames the client played.
    #[serde(default)]
    pub audio_played: u64,
    /// Audio frames the client dropped because they were late.
    #[serde(default)]
    pub audio_dropped: u64,
    /// How far the audio was from the video frame it aligns to, in
    /// microseconds. One sample per played frame.
    #[serde(default)]
    pub a_vs_skew_us: Vec<i64>,
}

/// Read a host report from a JSON file.
///
/// # Errors
/// Returns an error when the file cannot be read or parsed.
pub fn read_host(path: &std::path::Path) -> Result<HostReport, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Read a client report from a JSON file.
///
/// # Errors
/// Returns an error when the file cannot be read or parsed.
pub fn read_client(path: &std::path::Path) -> Result<ClientReport, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Merge a host run and a client run into one report.
///
/// The report is always provisional: the numbers come from software counters,
/// not from the photodiode rig.
#[must_use]
pub fn merge(
    host: &HostReport,
    client: &ClientReport,
    configuration: &[(String, String)],
) -> crate::report::RunReport {
    let mut report = crate::report::RunReport::new();
    for (key, value) in configuration {
        report.configuration.insert(key.clone(), value.clone());
    }
    // The counters that the report schema has no field for go into the
    // configuration, so the merged document stands on its own.
    report.configuration.insert(
        "host_datagrams_sent".to_string(),
        host.datagrams_sent.to_string(),
    );
    report
        .configuration
        .insert("host_bytes_sent".to_string(), host.bytes_sent.to_string());
    report.configuration.insert(
        "host_frames_captured".to_string(),
        host.frames_captured.to_string(),
    );
    report.configuration.insert(
        "client_datagrams_received".to_string(),
        client.datagrams_received.to_string(),
    );
    report.configuration.insert(
        "client_access_units".to_string(),
        client.access_units.to_string(),
    );
    report.configuration.insert(
        "client_keyframe_requests".to_string(),
        client.idr_requests.to_string(),
    );
    report.configuration.insert(
        "client_nacks_sent".to_string(),
        client.nacks_sent.to_string(),
    );
    report.configuration.insert(
        "host_keyframe_requests".to_string(),
        host.keyframe_requests.to_string(),
    );
    report.configuration.insert(
        "duration_ms".to_string(),
        host.duration_ms.max(client.duration_ms).to_string(),
    );

    // The per-stage percentiles come from the host's raw samples.
    for (stage, values) in &host.stage_samples {
        report
            .stages
            .insert(stage.clone(), Stats::from_values(values));
    }

    report.counters.frames_sent = host.frames_encoded;
    report.counters.frames_decoded = client.frames_decoded;
    report.counters.frames_presented = client.frames_presented;
    report.counters.dropped_frames = host.frames_captured.saturating_sub(client.frames_decoded);
    report.counters.decode_errors = client.decode_errors;
    report.counters.frames_rebuilt = client.frames_rebuilt;
    report.counters.zero_copy = client.zero_copy;
    report.counters.retransmits = host.retransmits;
    report.counters.frames_gpu_converted = host.frames_gpu_converted;
    report.counters.packets_lost = host
        .datagrams_sent
        .saturating_sub(client.datagrams_received);
    report.counters.packets_received = client.datagrams_received;
    report.counters.audio_packets = host.audio_packets;
    // A frame that is dropped instead of played late is an underrun: the
    // audio the client was supposed to hear never reached the speaker.
    report.counters.audio_underruns = client.audio_dropped;
    report.counters.clipboard_roundtrips = host.clipboard_roundtrips;
    report.counters.resize_events = host.resize_events;
    // The A/V skew is a stage of its own, so its percentiles sit with the
    // others in the report.
    if !client.a_vs_skew_us.is_empty() {
        report.stages.insert(
            "a_vs_skew".to_string(),
            Stats::from_values(&client.a_vs_skew_us),
        );
    }

    let seconds = host.duration_ms.max(client.duration_ms) as f64 / 1000.0;
    if seconds > 0.0 {
        report.bitrate_kbps = (host.bytes_sent as f64 * 8.0) / seconds / 1000.0;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_counts_the_run() {
        let host = HostReport {
            frames_captured: 100,
            frames_encoded: 100,
            datagrams_sent: 300,
            bytes_sent: 300_000,
            duration_ms: 10_000,
            ..HostReport::default()
        };
        let client = ClientReport {
            datagrams_received: 297,
            access_units: 99,
            frames_decoded: 99,
            frames_presented: 99,
            duration_ms: 10_000,
            ..ClientReport::default()
        };
        let report = merge(&host, &client, &[("width".into(), "1280".into())]);
        assert_eq!(report.counters.frames_sent, 100);
        assert_eq!(report.counters.frames_decoded, 99);
        assert_eq!(report.counters.dropped_frames, 1);
        assert_eq!(report.counters.packets_lost, 3);
        assert_eq!(report.counters.packets_received, 297);
        // 300000 bytes in 10 s is 240 kbit/s.
        assert!((report.bitrate_kbps - 240.0).abs() < 0.001);
        assert!(report.provisional);
    }

    #[test]
    fn a_short_run_does_not_divide_by_zero() {
        let report = merge(&HostReport::default(), &ClientReport::default(), &[]);
        assert_eq!(report.bitrate_kbps, 0.0);
    }
}
