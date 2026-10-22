//! The receive stage.
//!
//! Reads sealed datagrams from the media socket and hands the reassembled
//! access units to the decode stage.

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use lumen_crypto::SessionKeys;
use lumen_host_service::control::ControlMessage;
use lumen_host_service::session;
use lumen_protocol::{EncodedFrame, HEADER_LEN, MediaHeader, MessageKind};
use lumen_transport::socket::MediaSocket;

use crate::Counters;

/// How long one receive waits for a packet before the loop checks for shutdown.
const POLL: Duration = Duration::from_millis(5);
/// Size of the datagram read buffer. A media datagram is a 42-byte header
/// plus a 1200-byte slice, so this is generous.
const DATAGRAM_BUFFER: usize = 2048;

/// An access unit with the frame id it belongs to.
///
/// The frame id is needed to report a NACK or ask for a keyframe.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// The host frame identifier.
    pub frame_id: u32,
    /// The reassembled bitstream.
    pub encoded: EncodedFrame,
    /// When the client reassembled it, on the client clock.
    pub recv_ts_us: u64,
}

/// The receive stage.
#[derive(Debug)]
pub struct ReceiveStage {
    /// The media socket.
    pub(crate) socket: MediaSocket,
    /// The session keys, for the media cipher.
    pub(crate) keys: SessionKeys,
    /// The transport cipher the session agreed on.
    pub(crate) aead: lumen_crypto::AeadKind,
    /// Where the access units go.
    pub(crate) access_units: SyncSender<AccessUnit>,
    /// Where the requests for the host go.
    pub(crate) requests: SyncSender<ControlMessage>,
    /// Whether the pipeline is still running.
    pub(crate) running: Arc<AtomicBool>,
    /// Shared counters.
    pub(crate) counters: Arc<Counters>,
    /// The link accounting the host's estimator needs.
    pub(crate) link: Arc<LinkAccounting>,
}

/// What the client tells the host about the link.
///
/// The host's bandwidth and parity policies need the real loss, not the
/// number of parity rebuilds, so the client counts the slices it never saw.
#[derive(Debug, Default)]
pub struct LinkAccounting {
    /// Data slices the client received.
    pub(crate) slices_received: AtomicU64,
    /// Data slices the client never received.
    pub(crate) slices_lost: AtomicU64,
    /// The slices of the frame that is currently being assembled.
    pending: std::sync::Mutex<Option<PendingFrame>>,
}

/// The last frame that received a slice.
#[derive(Debug, Clone, Copy)]
struct PendingFrame {
    /// The frame identifier.
    frame_id: u32,
    /// How many slices the frame has.
    slice_count: u16,
    /// How many data slices arrived.
    received: u16,
    /// Whether the frame was assembled.
    ///
    /// The frame stays here after it is assembled, because a frame that
    /// never arrives leaves no trace: the loss is only visible when the next
    /// frame's slice shows up.
    completed: bool,
}

impl LinkAccounting {
    /// Record that a data slice arrived.
    pub(crate) fn on_slice(&self, frame_id: u32, slice_count: u16) {
        self.slices_received.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut pending) = self.pending.lock() {
            match *pending {
                Some(mut p) if p.frame_id == frame_id => {
                    p.received += 1;
                    *pending = Some(p);
                }
                Some(p) => {
                    if !p.completed {
                        // The previous frame never assembled, so its missing
                        // slices are lost.
                        let lost = u64::from(p.slice_count.saturating_sub(p.received));
                        self.slices_lost.fetch_add(lost, Ordering::Relaxed);
                    }
                    // The frames between the two identifiers never arrived at
                    // all, so each of them is lost whole.
                    let gap = frame_id.saturating_sub(p.frame_id).saturating_sub(1);
                    if gap > 0 {
                        let lost = u64::from(gap) * u64::from(p.slice_count);
                        self.slices_lost.fetch_add(lost, Ordering::Relaxed);
                    }
                    *pending = Some(PendingFrame {
                        frame_id,
                        slice_count,
                        received: 1,
                        completed: false,
                    });
                }
                None => {
                    *pending = Some(PendingFrame {
                        frame_id,
                        slice_count,
                        received: 1,
                        completed: false,
                    });
                }
            }
        }
    }

    /// Record that a frame arrived complete.
    pub(crate) fn on_complete(&self, frame_id: u32) {
        if let Ok(mut pending) = self.pending.lock()
            && let Some(p) = *pending
            && p.frame_id == frame_id
        {
            let mut p = p;
            p.completed = true;
            *pending = Some(p);
        }
    }

    /// The slices received and the slices lost.
    pub(crate) fn snapshot(&self) -> (u64, u64) {
        (
            self.slices_received.load(Ordering::Relaxed),
            self.slices_lost.load(Ordering::Relaxed),
        )
    }
}

impl ReceiveStage {
    /// Read datagrams until the pipeline stops.
    pub fn run(self) {
        let mut receiver = session::media_receiver(&self.keys, self.aead);
        let mut buffer = vec![0u8; DATAGRAM_BUFFER];
        // The frame whose first slice was seen last. When a new frame starts,
        // the previous one is either complete or lost.
        let mut last_frame: Option<u32> = None;

        if let Err(e) = self.socket.wait_for_packets(POLL) {
            eprintln!("lumen-client: media socket cannot wait for packets: {e}");
            return;
        }
        while self.running.load(Ordering::SeqCst) {
            match self.socket.recv_from(&mut buffer) {
                Ok((n, _from)) => {
                    self.counters
                        .datagrams_received
                        .fetch_add(1, Ordering::Relaxed);
                    if let Some(header) = parse_header(&buffer[..n])
                        && header.kind == MessageKind::VideoData
                    {
                        self.link.on_slice(header.frame_id, header.slice_count);
                    }
                    // A hole is reported before the datagram is fed in, so the
                    // receiver still holds the incomplete frame.
                    if let Some(header) = parse_header(&buffer[..n])
                        && header.kind == MessageKind::VideoData
                        && header.slice_index == 0
                        && last_frame.is_some_and(|prev| prev < header.frame_id)
                    {
                        let prev = last_frame.expect("checked above");
                        let missing = receiver.missing(prev);
                        if !missing.is_empty() {
                            let _ = self.requests.try_send(ControlMessage::Nack {
                                frame_id: prev,
                                slices: missing,
                            });
                        }
                    }
                    if let Ok(Some(au)) = receiver.on_datagram(&buffer[..n]) {
                        self.link.on_complete(au.frame_id);
                        self.counters.access_units.fetch_add(1, Ordering::Relaxed);
                        self.counters
                            .frames_rebuilt
                            .store(receiver.stats().frames_rebuilt, Ordering::Relaxed);
                        last_frame = Some(au.frame_id);
                        let is_keyframe = is_recovery_point(&au.data);
                        let frame_id = au.frame_id;
                        let access_unit = AccessUnit {
                            frame_id,
                            encoded: EncodedFrame {
                                data: au.data,
                                is_keyframe,
                                capture_ts_us: au.capture_ts_us,
                                index: u64::from(au.frame_id),
                            },
                            recv_ts_us: monotonic_us(),
                        };
                        if self.access_units.try_send(access_unit).is_err() {
                            // The decode stage is busy. The plan wants the
                            // late frames dropped and not queued: a queue
                            // that grows only adds latency. The next frame
                            // predicts from the dropped one, so it fails to
                            // decode, and the decode stage asks the host to
                            // recover from the last frame it has.
                            self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    // A late, stale or unauthenticated datagram is normal on a
                    // lossy link. The receiver stays usable.
                }
                // A timeout only means no packet came within POLL. The loop checks the running flag
                // then, and the next packet is read on arrival.
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => {
                    eprintln!("lumen-client: media receive failed: {e}");
                    return;
                }
            }
        }
    }
}

/// Microseconds on the monotonic clock.
///
/// The same clock as the stage timestamps, so the host can map them onto its
/// own clock with the offset it estimates from the ping exchange.
pub fn monotonic_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs * 1_000_000 + nanos / 1_000
}

/// Parse the header of a datagram.
fn parse_header(datagram: &[u8]) -> Option<MediaHeader> {
    if datagram.len() < HEADER_LEN {
        return None;
    }
    MediaHeader::parse(&datagram[..HEADER_LEN]).ok()
}

/// Whether an Annex-B stream is a recovery point: it holds an IDR slice, or starts with a
/// recovery point SEI, which the host puts before a picture that recovers from a loss by
/// referencing a frame the client still has.
///
/// The decoder does not need this, but the client needs to know which frame may be decoded
/// again after a loss.
#[must_use]
pub fn is_recovery_point(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < data.len() {
        let is_start = data[i] == 0
            && data[i + 1] == 0
            && (data[i + 2] == 1 || (data[i + 2] == 0 && data[i + 3] == 1));
        if is_start {
            let start = if data[i + 2] == 1 { i + 3 } else { i + 4 };
            let nal_type = data.get(start).map(|b| b & 0x1f);
            // An IDR slice, or an SEI whose first message is a recovery point (payload type 6).
            if nal_type == Some(5) || (nal_type == Some(6) && data.get(start + 1) == Some(&6)) {
                return true;
            }
            i = start;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_recovery_point;

    #[test]
    fn finds_an_idr_nal() {
        let stream = [0u8, 0, 0, 1, 0x65, 0x88, 0x84, 0x00];
        assert!(is_recovery_point(&stream));
    }

    #[test]
    fn finds_an_idr_after_a_parameter_set() {
        let mut stream = vec![0u8, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e];
        stream.extend_from_slice(&[0u8, 0, 0, 1, 0x65, 0x88]);
        assert!(is_recovery_point(&stream));
    }

    #[test]
    fn a_non_idr_stream_has_no_idr() {
        let stream = [0u8, 0, 0, 1, 0x41, 0x9a, 0x00, 0x00];
        assert!(!is_recovery_point(&stream));
    }

    #[test]
    fn a_recovery_point_sei_marks_a_p_picture() {
        let stream = [
            0u8, 0, 0, 1, 0x06, 0x06, 0x01, 0xC0, 0x80, 0, 0, 0, 1, 0x41, 0x9a,
        ];
        assert!(is_recovery_point(&stream));
        // Another SEI message (user data, type 5) does not.
        let other = [
            0u8, 0, 0, 1, 0x06, 0x05, 0x01, 0xC0, 0x80, 0, 0, 0, 1, 0x41, 0x9a,
        ];
        assert!(!is_recovery_point(&other));
    }

    #[test]
    fn an_empty_stream_has_no_idr() {
        assert!(!is_recovery_point(&[]));
    }
}
