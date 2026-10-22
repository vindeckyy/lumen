//! Driving audio over a session.
//!
//! The host frames an encoded audio frame behind the protocol header and
//! hands the packet to the transport, which seals it and sends it. The client
//! reassembles the frames that come back and hands them to the scheduler.
//!
//! The audio crate does not own the transport: [`AudioPacketSink`] is the
//! seam. The transport implements it, so the transport does not depend on the
//! audio crate, and the audio crate does not depend on the transport either.

use lumen_protocol::audio::{AUDIO_HEADER_LEN, AUDIO_VERSION, AudioHeader, AudioKind};

use crate::AudioConfig;

/// The most slices one audio packet may be split into.
///
/// A 64 KiB payload in 1200-byte slices needs 55 of them, so the cap only
/// bites a hostile peer that claims an enormous slice count.
pub const MAX_AUDIO_SLICES: u16 = 64;

/// What the audio sender hands a framed packet to.
///
/// The transport seals the packet with the session key and sends it. The
/// audio crate owns neither the socket nor the cipher.
pub trait AudioPacketSink {
    /// Seals and sends one framed audio packet.
    ///
    /// Returns the number of datagrams that went out.
    ///
    /// # Errors
    /// Returns a human-readable description of the failure.
    fn send_packet(&mut self, packet: &[u8]) -> Result<usize, String>;
}

impl<F> AudioPacketSink for F
where
    F: FnMut(&[u8]) -> Result<usize, String>,
{
    fn send_packet(&mut self, packet: &[u8]) -> Result<usize, String> {
        self(packet)
    }
}

/// Errors of the audio session path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The header does not describe the stream the sender was opened for.
    #[error("audio header does not match the stream: {0}")]
    Mismatch(&'static str),
    /// The payload is larger than the format allows.
    #[error("audio payload of {0} bytes is too large")]
    PayloadTooLarge(usize),
    /// The slice index does not fit the slice count.
    #[error("audio slice index {index} out of range for {count} slices")]
    BadSlice {
        /// The index that arrived.
        index: u16,
        /// The number of slices the packet was split into.
        count: u16,
    },
    /// The packet does not parse.
    #[error("audio packet: {0}")]
    Protocol(#[from] lumen_protocol::audio::AudioError),
    /// The transport refused the packet.
    #[error("transport refused the audio packet: {0}")]
    Transport(String),
}

/// A framed audio packet: the audio header followed by the encoded payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    /// The header that describes the payload.
    pub header: AudioHeader,
    /// The encoded payload.
    pub payload: Vec<u8>,
}

impl AudioPacket {
    /// The bytes that go on the wire: the header followed by the payload.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(AUDIO_HEADER_LEN + self.payload.len());
        bytes.extend_from_slice(&self.header.to_bytes());
        bytes.extend_from_slice(&self.payload);
        bytes
    }
}

/// Frames audio frames for the transport.
///
/// The sender owns the frame index and the payload length. The caller sets
/// the stream parameters and the timestamp of the video frame the audio
/// aligns to.
#[derive(Debug, Clone)]
pub struct AudioSender {
    /// The stream parameters every frame carries.
    template: AudioHeader,
    /// The frame index of the next frame.
    next_frame_index: u64,
}

impl AudioSender {
    /// Open a sender for the stream described by `cfg`.
    ///
    /// # Errors
    /// Returns a human-readable description when the configuration names a
    /// frame size or a layout the wire format does not carry.
    pub fn new(cfg: &AudioConfig) -> Result<Self, String> {
        let frame_duration_us = lumen_protocol::audio::frame_duration_us(cfg.frame_ms)
            .ok_or_else(|| format!("frame_ms {} is not a 2.5, 5 or 10 ms frame", cfg.frame_ms))?;
        // Rejects a zero sample rate and a zero channel count.
        cfg.frame_samples()?;
        let kind = if cfg.use_opus {
            AudioKind::Opus
        } else {
            AudioKind::Pcm
        };
        let template = AudioHeader {
            version: AUDIO_VERSION,
            kind,
            channels: cfg.channels,
            sample_rate: cfg.sample_rate,
            frame_duration_us,
            frame_index: 0,
            video_capture_ts_us: 0,
            payload_len: 0,
            seq: 0,
        };
        Ok(Self {
            template,
            next_frame_index: 0,
        })
    }

    /// The codec the sender frames.
    #[must_use]
    pub fn kind(&self) -> AudioKind {
        self.template.kind
    }

    /// The frame index the next frame will carry.
    #[must_use]
    pub fn next_frame_index(&self) -> u64 {
        self.next_frame_index
    }

    /// The header of the next frame, with the stream parameters filled in.
    ///
    /// `video_capture_ts_us` is the host capture timestamp of the video frame
    /// the audio aligns to.
    #[must_use]
    pub fn header(&self, video_capture_ts_us: u64) -> AudioHeader {
        AudioHeader {
            frame_index: self.next_frame_index,
            video_capture_ts_us,
            ..self.template
        }
    }

    /// Frames `payload` behind `header` and hands the packet to `sink`.
    ///
    /// # Errors
    /// Returns [`SessionError::Mismatch`] when the header does not describe
    /// the stream the sender was opened for, [`SessionError::PayloadTooLarge`]
    /// when the payload does not fit the format, and
    /// [`SessionError::Transport`] when the sink refuses the packet.
    pub fn send(
        &mut self,
        mut header: AudioHeader,
        payload: &[u8],
        sink: &mut dyn AudioPacketSink,
    ) -> Result<AudioPacket, SessionError> {
        self.check_header(&header)?;
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| SessionError::PayloadTooLarge(payload.len()))?;
        if payload_len > lumen_protocol::audio::MAX_AUDIO_PAYLOAD {
            return Err(SessionError::PayloadTooLarge(payload.len()));
        }
        header.frame_index = self.next_frame_index;
        header.payload_len = payload_len;
        let packet = AudioPacket {
            header,
            payload: payload.to_vec(),
        };
        sink.send_packet(&packet.to_bytes())
            .map_err(SessionError::Transport)?;
        self.next_frame_index = self.next_frame_index.wrapping_add(1);
        Ok(packet)
    }

    /// Checks that the header describes the stream of this sender.
    fn check_header(&self, header: &AudioHeader) -> Result<(), SessionError> {
        if header.kind != self.template.kind {
            return Err(SessionError::Mismatch("codec"));
        }
        if header.channels != self.template.channels {
            return Err(SessionError::Mismatch("channel count"));
        }
        if header.sample_rate != self.template.sample_rate {
            return Err(SessionError::Mismatch("sample rate"));
        }
        if header.frame_duration_us != self.template.frame_duration_us {
            return Err(SessionError::Mismatch("frame duration"));
        }
        Ok(())
    }
}

/// One slice of an audio packet, as the transport delivered it.
///
/// The transport slices a packet the way it slices a video access unit, so
/// one audio frame can arrive as several datagrams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSlice {
    /// Index of the slice inside the audio packet.
    pub index: u16,
    /// Number of slices the audio packet was split into.
    pub count: u16,
    /// The bytes of the slice.
    pub payload: Vec<u8>,
}

/// An audio frame that arrived from the wire, with its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedAudio {
    /// The header that describes the payload.
    pub header: AudioHeader,
    /// The encoded payload.
    pub payload: Vec<u8>,
}

/// Counters of the audio receiver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioStats {
    /// Frames reassembled.
    pub frames_completed: u64,
    /// Frames dropped, because a slice was malformed or the packet did not parse.
    pub frames_dropped: u64,
    /// Bytes received.
    pub bytes_received: u64,
    /// Slices that arrived after their frame was already assembled.
    pub stale_slices: u64,
}

/// Reassembles audio frames from the datagram payloads.
#[derive(Debug, Default)]
pub struct AudioReceiver {
    pending: Option<PendingFrame>,
    stats: AudioStats,
}

/// The slices of one audio packet that have arrived so far.
#[derive(Debug)]
struct PendingFrame {
    count: u16,
    slices: Vec<Option<Vec<u8>>>,
    received: u16,
    /// The frame index of the header in slice 0, once that slice arrived.
    frame_index: Option<u64>,
}

impl AudioReceiver {
    /// Create a receiver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counters since the receiver was created.
    #[must_use]
    pub fn stats(&self) -> AudioStats {
        self.stats
    }

    /// Feeds one datagram payload.
    ///
    /// Returns the frame when the last slice of it has arrived. The slices of
    /// one packet may arrive in any order.
    ///
    /// # Errors
    /// Returns [`SessionError::BadSlice`] when the slice index does not fit
    /// the slice count, and [`SessionError::Protocol`] when the assembled
    /// packet does not parse.
    pub fn push(&mut self, slice: AudioSlice) -> Result<Option<ReceivedAudio>, SessionError> {
        self.stats.bytes_received += slice.payload.len() as u64;
        if slice.count == 0 || slice.index >= slice.count {
            return Err(SessionError::BadSlice {
                index: slice.index,
                count: slice.count,
            });
        }
        if slice.count > MAX_AUDIO_SLICES {
            return Err(SessionError::BadSlice {
                index: slice.index,
                count: slice.count,
            });
        }
        // A packet whose slice count differs from the one being assembled is
        // a different packet: drop what is pending and start again.
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.count != slice.count)
        {
            self.stats.frames_dropped += 1;
            self.pending = None;
        }
        // Slice 0 carries the header, so it names the frame it belongs to. A
        // frame index that differs from the one being assembled means the
        // pending frame will never complete: it is dropped so that the new
        // frame can be assembled.
        let mut incoming_index = None;
        if slice.index == 0
            && let Ok(header) = AudioHeader::parse(&slice.payload)
        {
            incoming_index = Some(header.frame_index);
        }
        if incoming_index.is_some_and(|new| {
            self.pending
                .as_ref()
                .and_then(|p| p.frame_index)
                .is_some_and(|old| old != new)
        }) {
            self.stats.frames_dropped += 1;
            self.pending = None;
        }
        let pending = self.pending.get_or_insert_with(|| PendingFrame {
            count: slice.count,
            slices: vec![None; slice.count as usize],
            received: 0,
            frame_index: None,
        });
        if pending.slices[slice.index as usize].is_none() {
            pending.slices[slice.index as usize] = Some(slice.payload);
            pending.received += 1;
            if let Some(index) = incoming_index {
                pending.frame_index = Some(index);
            }
        } else {
            self.stats.stale_slices += 1;
        }
        if pending.received < pending.count {
            return Ok(None);
        }
        let pending = self.pending.take().ok_or(SessionError::BadSlice {
            index: slice.index,
            count: slice.count,
        })?;
        let mut bytes = Vec::new();
        for slice in pending.slices.iter().flatten() {
            bytes.extend_from_slice(slice);
        }
        let header = match AudioHeader::parse(&bytes) {
            Ok(header) => header,
            Err(e) => {
                self.stats.frames_dropped += 1;
                return Err(SessionError::Protocol(e));
            }
        };
        // The transport pads the last slice, so trim the packet to the length
        // the header declares before it is split.
        let total = AUDIO_HEADER_LEN
            .saturating_add(usize::try_from(header.payload_len).unwrap_or(usize::MAX));
        if bytes.len() < total {
            self.stats.frames_dropped += 1;
            return Err(SessionError::Protocol(
                lumen_protocol::audio::AudioError::LengthMismatch(total, bytes.len()),
            ));
        }
        match AudioHeader::split(&bytes[..total]) {
            Ok((header, payload)) => {
                self.stats.frames_completed += 1;
                Ok(Some(ReceivedAudio {
                    header,
                    payload: payload.to_vec(),
                }))
            }
            Err(e) => {
                self.stats.frames_dropped += 1;
                Err(SessionError::Protocol(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(use_opus: bool) -> AudioConfig {
        AudioConfig {
            sample_rate: 48_000,
            channels: 2,
            frame_ms: 50,
            bitrate_bps: 96_000,
            use_opus,
        }
    }

    /// A sink that keeps every packet it is handed.
    #[derive(Default)]
    struct Collector {
        packets: Vec<Vec<u8>>,
    }

    impl AudioPacketSink for Collector {
        fn send_packet(&mut self, packet: &[u8]) -> Result<usize, String> {
            self.packets.push(packet.to_vec());
            Ok(1)
        }
    }

    #[test]
    fn a_frame_survives_the_round_trip() {
        let mut sender = AudioSender::new(&cfg(true)).expect("open sender");
        let mut collector = Collector::default();
        let header = sender.header(1_234_567);
        let packet = sender
            .send(header, &[1, 2, 3, 4], &mut collector)
            .expect("send");
        assert_eq!(collector.packets.len(), 1);
        assert_eq!(packet.header.payload_len, 4);
        assert_eq!(packet.header.video_capture_ts_us, 1_234_567);
        assert_eq!(packet.header.frame_index, 0);

        let mut receiver = AudioReceiver::new();
        let bytes = collector.packets[0].clone();
        let got = receiver
            .push(AudioSlice {
                index: 0,
                count: 1,
                payload: bytes,
            })
            .expect("push")
            .expect("a frame must come out");
        assert_eq!(got.header, packet.header);
        assert_eq!(got.payload, vec![1, 2, 3, 4]);
        assert_eq!(receiver.stats().frames_completed, 1);
    }

    #[test]
    fn the_sender_stamps_the_frame_index() {
        let mut sender = AudioSender::new(&cfg(true)).expect("open sender");
        let mut collector = Collector::default();
        for i in 0..3u64 {
            let header = sender.header(i * 1000);
            let packet = sender
                .send(header, &[0u8; 2], &mut collector)
                .expect("send");
            assert_eq!(packet.header.frame_index, i);
        }
        assert_eq!(sender.next_frame_index(), 3);
    }

    #[test]
    fn the_sender_refuses_a_header_of_another_stream() {
        let mut sender = AudioSender::new(&cfg(true)).expect("open sender");
        let mut collector = Collector::default();
        let mut header = sender.header(0);
        header.sample_rate = 44_100;
        assert_eq!(
            sender.send(header, &[0], &mut collector),
            Err(SessionError::Mismatch("sample rate"))
        );
        let mut header = sender.header(0);
        header.kind = AudioKind::Pcm;
        assert_eq!(
            sender.send(header, &[0], &mut collector),
            Err(SessionError::Mismatch("codec"))
        );
        let mut header = sender.header(0);
        header.channels = 1;
        assert_eq!(
            sender.send(header, &[0], &mut collector),
            Err(SessionError::Mismatch("channel count"))
        );
        let mut header = sender.header(0);
        header.frame_duration_us = 2_500;
        assert_eq!(
            sender.send(header, &[0], &mut collector),
            Err(SessionError::Mismatch("frame duration"))
        );
        assert!(collector.packets.is_empty(), "nothing may be sent");
    }

    #[test]
    fn a_refused_packet_is_reported_by_the_transport() {
        let mut sender = AudioSender::new(&cfg(true)).expect("open sender");
        let header = sender.header(0);
        let mut failing = |_: &[u8]| Err("socket is gone".to_owned());
        assert_eq!(
            sender.send(header, &[0], &mut failing),
            Err(SessionError::Transport("socket is gone".to_owned()))
        );
    }

    #[test]
    fn a_large_frame_is_reassembled_from_its_slices() {
        let mut sender = AudioSender::new(&cfg(false)).expect("open sender");
        let mut collector = Collector::default();
        let payload: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let header = sender.header(777);
        let packet = sender.send(header, &payload, &mut collector).expect("send");
        let bytes = collector.packets[0].clone();

        // The transport splits the packet into 1200-byte slices and pads the
        // last one.
        let mut slices = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let end = (offset + 1200).min(bytes.len());
            let mut slice = bytes[offset..end].to_vec();
            slice.resize(1200, 0);
            slices.push(slice);
            offset = end;
        }
        let count = u16::try_from(slices.len()).expect("few slices");
        let mut receiver = AudioReceiver::new();
        // Deliver them back to front: the reassembly must not depend on the order.
        let mut completed = None;
        for (index, slice) in slices.iter().enumerate().rev() {
            let index = u16::try_from(index).expect("few slices");
            let got = receiver
                .push(AudioSlice {
                    index,
                    count,
                    payload: slice.clone(),
                })
                .expect("push");
            if let Some(frame) = got {
                assert!(completed.is_none(), "the frame completes exactly once");
                completed = Some(frame);
            }
        }
        let completed = completed.expect("the frame must complete");
        assert_eq!(completed.header, packet.header);
        assert_eq!(completed.payload, payload);
        assert_eq!(receiver.stats().frames_completed, 1);
        assert_eq!(receiver.stats().frames_dropped, 0);
    }

    #[test]
    fn a_slice_out_of_range_is_rejected() {
        let mut receiver = AudioReceiver::new();
        assert_eq!(
            receiver.push(AudioSlice {
                index: 3,
                count: 2,
                payload: vec![0; 10],
            }),
            Err(SessionError::BadSlice { index: 3, count: 2 })
        );
        assert_eq!(
            receiver.push(AudioSlice {
                index: 0,
                count: 0,
                payload: vec![0; 10],
            }),
            Err(SessionError::BadSlice { index: 0, count: 0 })
        );
    }

    #[test]
    fn too_many_slices_are_refused() {
        let mut receiver = AudioReceiver::new();
        assert!(matches!(
            receiver.push(AudioSlice {
                index: 0,
                count: MAX_AUDIO_SLICES + 1,
                payload: vec![0; 10],
            }),
            Err(SessionError::BadSlice { .. })
        ));
    }

    #[test]
    fn a_packet_that_does_not_parse_is_dropped() {
        let mut receiver = AudioReceiver::new();
        let mut bytes = vec![0u8; 64];
        bytes[0] = b'X';
        let err = receiver
            .push(AudioSlice {
                index: 0,
                count: 1,
                payload: bytes,
            })
            .expect_err("the magic is wrong");
        assert!(matches!(err, SessionError::Protocol(_)));
        assert_eq!(receiver.stats().frames_dropped, 1);
        assert_eq!(receiver.stats().frames_completed, 0);
    }

    #[test]
    fn a_duplicate_slice_is_counted_and_does_not_complete_the_frame() {
        let mut sender = AudioSender::new(&cfg(true)).expect("open sender");
        let mut collector = Collector::default();
        let header = sender.header(0);
        sender
            .send(header, &[1, 2, 3], &mut collector)
            .expect("send");
        let bytes = collector.packets[0].clone();
        let mut receiver = AudioReceiver::new();
        let slice = AudioSlice {
            index: 0,
            count: 2,
            payload: bytes,
        };
        assert!(receiver.push(slice.clone()).expect("push").is_none());
        assert!(receiver.push(slice).expect("push").is_none());
        assert_eq!(receiver.stats().stale_slices, 1);
        assert_eq!(receiver.stats().frames_completed, 0);
    }

    #[test]
    fn a_new_frame_replaces_one_that_never_completed() {
        let mut sender = AudioSender::new(&cfg(true)).expect("open sender");
        let mut collector = Collector::default();
        let first = sender.header(0);
        sender.send(first, &[1; 100], &mut collector).expect("send");
        let second = sender.header(5_000);
        sender
            .send(second, &[2; 100], &mut collector)
            .expect("send");

        // Both packets are split in two, so the first slice carries the header.
        let first_bytes = collector.packets[0].clone();
        let second_bytes = collector.packets[1].clone();
        let split = 70;

        let mut receiver = AudioReceiver::new();
        // The first frame is missing its second slice and never completes.
        let slice = AudioSlice {
            index: 0,
            count: 2,
            payload: first_bytes[..split].to_vec(),
        };
        assert!(receiver.push(slice).expect("push").is_none());
        assert_eq!(receiver.stats().frames_dropped, 0);

        // The next frame arrives with the same slice count, so only its frame
        // index tells the two apart.
        let slice = AudioSlice {
            index: 0,
            count: 2,
            payload: second_bytes[..split].to_vec(),
        };
        assert!(receiver.push(slice).expect("push").is_none());
        assert_eq!(receiver.stats().frames_dropped, 1);

        // The second frame still completes once its last slice arrives.
        let slice = AudioSlice {
            index: 1,
            count: 2,
            payload: second_bytes[split..].to_vec(),
        };
        let got = receiver
            .push(slice)
            .expect("push")
            .expect("the frame must complete");
        assert_eq!(got.payload, vec![2; 100]);
        assert_eq!(got.header.video_capture_ts_us, 5_000);
        assert_eq!(receiver.stats().frames_completed, 1);
    }
}
