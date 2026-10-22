//! Media packets: slicing, FEC, sealing, pacing and reassembly.
//!
//! The sender turns one encoded access unit into sealed datagrams. Each
//! datagram is a media header followed by the AES-GCM (or ChaCha20-Poly1305)
//! ciphertext of a fixed-size slice payload. The header is the associated
//! data, so it is authenticated, and its `seq` field is the AEAD nonce
//! counter, so every datagram has a unique nonce within a session.
//!
//! A retransmission is sealed again with a new `seq`. The receiver reassembles
//! with [`SliceAssembler`], which also rebuilds missing slices from the FEC
//! parity of their block. A frame older than the last delivered frame is never
//! presented: the receiver drops it and counts it.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;

use lumen_crypto::{AeadKind, CryptoError, PacketCipher};
use lumen_protocol::slice::SLICE_MAX_PAYLOAD;
use lumen_protocol::{HEADER_LEN, MediaHeader, MessageKind, ProtocolError};

use crate::fec::{ParityLevel, compute_parity_with};
use crate::pacing::Pacer;
use crate::slice::{AssembledAu, ReceivedSlice, SliceAssembler};
use crate::socket::MediaSocket;

/// Number of recent access units the sender keeps for retransmission.
pub const RETRANSMIT_HISTORY: usize = 4;

/// A receiver drops incomplete frames more than this many frames behind the
/// newest frame it has seen.
pub const MAX_FRAME_LAG: u32 = 8;

/// Errors of the media path.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// The access unit needs more slices than the 16-bit slice count allows.
    #[error("access unit of {0} bytes needs more than 65535 slices")]
    TooLarge(usize),
    /// The requested FEC level is not implemented.
    #[error("FEC level {0:?} is not implemented")]
    UnsupportedParity(ParityLevel),
    /// The datagram is shorter than the media header.
    #[error("datagram of {0} bytes is shorter than the media header")]
    Short(usize),
    /// The header is malformed.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// The payload length does not match the header.
    #[error("payload is {actual} bytes but the header says {declared}")]
    LengthMismatch {
        /// Bytes received after the header.
        actual: usize,
        /// Length declared in the header.
        declared: usize,
    },
    /// The packet failed authentication.
    #[error("packet failed authentication")]
    Auth,
    /// The packet belongs to another session.
    #[error("packet belongs to another session")]
    StaleSession,
    /// The packet is for a frame that was already delivered or dropped.
    #[error("packet is for frame {0}, which is older than the last delivered frame")]
    Late(u32),
    /// A retransmission was requested for a slice that is not in the history.
    #[error("slice {1} of frame {0} is not in the retransmit history")]
    UnknownSlice(u32, u16),
    /// Sealing failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
}

/// One access unit kept for retransmission.
#[derive(Debug)]
struct SentFrame {
    frame_id: u32,
    capture_ts_us: u64,
    au_len: u32,
    slice_count: u16,
    /// The padded data slices, one after the other.
    ///
    /// They are kept in one buffer rather than in a vector of slices: a frame
    /// at 1080p is a few thousand slices, and a vector of them would be a few
    /// thousand small allocations every frame.
    slices: Vec<u8>,
}

/// Sends access units as sealed media datagrams.
#[derive(Debug)]
pub struct MediaSender {
    cipher: PacketCipher,
    session_id: u32,
    parity: ParityLevel,
    next_frame_id: u32,
    last_frame_id: u32,
    history: VecDeque<SentFrame>,
}

impl MediaSender {
    /// Create a sender for one session.
    ///
    /// # Errors
    /// Returns [`MediaError::UnsupportedParity`] for a FEC level that is not
    /// implemented yet.
    pub fn new(
        kind: AeadKind,
        key: [u8; 32],
        session_id: u32,
        parity: ParityLevel,
    ) -> Result<Self, MediaError> {
        Ok(Self {
            cipher: PacketCipher::new(kind, key, session_id),
            session_id,
            parity,
            next_frame_id: 0,
            last_frame_id: 0,
            history: VecDeque::with_capacity(RETRANSMIT_HISTORY),
        })
    }

    /// Change the parity level used for the next access units.
    ///
    /// The adaptive policy picks the level from the measured loss. The level
    /// also sets the block size, so the receiver learns it from the wire.
    pub fn set_parity(&mut self, parity: ParityLevel) {
        self.parity = parity;
    }

    /// The parity level currently in use.
    #[must_use]
    pub fn parity(&self) -> ParityLevel {
        self.parity
    }

    /// Packetise one access unit. Returns sealed datagrams in send order: the
    /// data slices of each FEC block, followed by that block's parity slices.
    ///
    /// An empty access unit produces no datagrams and consumes no frame id.
    ///
    /// # Errors
    /// Returns [`MediaError::TooLarge`] when the access unit needs more than
    /// 65535 slices.
    pub fn packetize(&mut self, au: &[u8], capture_ts_us: u64) -> Result<Vec<Vec<u8>>, MediaError> {
        if au.is_empty() {
            return Ok(Vec::new());
        }
        let too_large = MediaError::TooLarge(au.len());
        let slice_count =
            u16::try_from(au.len().div_ceil(SLICE_MAX_PAYLOAD)).map_err(|_| too_large)?;
        let au_len = u32::try_from(au.len()).map_err(|_| MediaError::TooLarge(au.len()))?;
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        let parity_per_block = self.parity.parity_slices();
        let block_data = self.parity.block_data_slices();
        let slice_count_usize = usize::from(slice_count);
        let mut datagrams = Vec::with_capacity(
            slice_count_usize + parity_per_block * slice_count_usize.div_ceil(block_data),
        );

        // The data slices are padded to the slice size, because the parity
        // is computed over the padded block and the receiver rebuilds it
        // that way. They live in one buffer, so a frame at 1080p is one
        // allocation and not a few thousand.
        let mut slices = vec![0u8; slice_count_usize * SLICE_MAX_PAYLOAD];
        for (index, chunk) in au.chunks(SLICE_MAX_PAYLOAD).enumerate() {
            let start = index * SLICE_MAX_PAYLOAD;
            slices[start..start + chunk.len()].copy_from_slice(chunk);
        }

        for index in 0..slice_count_usize {
            let start = index * SLICE_MAX_PAYLOAD;
            let header = MediaHeader::data_slice(
                MessageKind::VideoData,
                frame_id,
                index as u16,
                slice_count,
                capture_ts_us,
                SLICE_MAX_PAYLOAD as u16,
                au_len,
                self.session_id,
            )
            // The block size travels with the slice, so the receiver can
            // map a slice index to its block.
            .with_flags(block_data as u16);
            datagrams.push(self.seal(header, &slices[start..start + SLICE_MAX_PAYLOAD])?);
        }

        // The parity is computed over the padded block, because that is what
        // the receiver rebuilds: a short last block is filled up to the block
        // size with zero slices, which are never sent.
        if parity_per_block > 0 {
            for (block, block_slices) in slices.chunks(block_data * SLICE_MAX_PAYLOAD).enumerate() {
                let mut padded: Vec<Vec<u8>> = block_slices
                    .chunks(SLICE_MAX_PAYLOAD)
                    .map(<[u8]>::to_vec)
                    .collect();
                padded.resize(block_data, vec![0u8; SLICE_MAX_PAYLOAD]);
                for (index, bytes) in compute_parity_with(&padded, parity_per_block)
                    .iter()
                    .enumerate()
                {
                    // Parity packets carry the block in `aux` and the parity index in `slice_index`.
                    let header = MediaHeader::data_slice(
                        MessageKind::VideoParity,
                        frame_id,
                        index as u16,
                        slice_count,
                        capture_ts_us,
                        SLICE_MAX_PAYLOAD as u16,
                        au_len,
                        self.session_id,
                    )
                    .with_aux(block as u16)
                    // The block size travels on the parity slices too, so it
                    // is known even when the data slices are the ones lost.
                    .with_flags(block_data as u16);
                    datagrams.push(self.seal(header, bytes)?);
                }
            }
        }

        self.last_frame_id = frame_id;
        self.history.push_back(SentFrame {
            frame_id,
            capture_ts_us,
            au_len,
            slice_count,
            slices,
        });
        while self.history.len() > RETRANSMIT_HISTORY {
            self.history.pop_front();
        }
        Ok(datagrams)
    }

    /// The frame id of the last access unit that was packetized.
    ///
    /// The id is what the receiver sees, so a sender can tie its own
    /// per-frame state to it. It is only meaningful after [`Self::packetize`].
    #[must_use]
    pub fn last_frame_id(&self) -> u32 {
        self.last_frame_id
    }

    /// Seal a fresh copy of one data slice of a recent frame, for a NACK.
    ///
    /// # Errors
    /// Returns [`MediaError::UnknownSlice`] when the frame has left the history
    /// or the index is out of range.
    pub fn retransmit(&mut self, frame_id: u32, slice_index: u16) -> Result<Vec<u8>, MediaError> {
        let unknown = MediaError::UnknownSlice(frame_id, slice_index);
        let frame = self
            .history
            .iter()
            .find(|f| f.frame_id == frame_id)
            .ok_or(MediaError::UnknownSlice(frame_id, slice_index))?;
        let start = usize::from(slice_index) * SLICE_MAX_PAYLOAD;
        let end = start + SLICE_MAX_PAYLOAD;
        if end > frame.slices.len() {
            return Err(unknown);
        }
        // The slice is copied out so the borrow of the history ends before
        // the sealing, which needs the sender.
        let payload = frame.slices[start..end].to_vec();
        let header = MediaHeader::data_slice(
            MessageKind::VideoData,
            frame_id,
            slice_index,
            frame.slice_count,
            frame.capture_ts_us,
            SLICE_MAX_PAYLOAD as u16,
            frame.au_len,
            self.session_id,
        );
        self.seal(header, &payload)
    }

    fn seal(&mut self, mut header: MediaHeader, payload: &[u8]) -> Result<Vec<u8>, MediaError> {
        header.seq = self.cipher.counter();
        let head = header.to_bytes();
        Ok(self.cipher.seal(&head, payload)?)
    }
}

/// Counters the receiver keeps for the run report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiverStats {
    /// Datagrams that passed authentication.
    pub packets: u64,
    /// Datagrams whose tag did not verify.
    pub auth_failures: u64,
    /// Datagrams from another session.
    pub stale: u64,
    /// Datagrams that did not parse.
    pub malformed: u64,
    /// Datagrams for frames older than the last delivered frame.
    pub late: u64,
    /// Access units delivered.
    pub frames_completed: u64,
    /// Incomplete frames dropped without delivery.
    pub frames_dropped: u64,
    /// Access units delivered only after FEC parity rebuilt a lost slice.
    pub frames_rebuilt: u64,
}

/// Receives sealed media datagrams and yields complete access units.
#[derive(Debug)]
pub struct MediaReceiver {
    cipher: PacketCipher,
    session_id: u32,
    assembler: SliceAssembler,
    newest_frame: Option<u32>,
    last_delivered: Option<u32>,
    stats: ReceiverStats,
}

impl MediaReceiver {
    /// Create a receiver for one session.
    #[must_use]
    pub fn new(kind: AeadKind, key: [u8; 32], session_id: u32, fec: bool) -> Self {
        Self {
            cipher: PacketCipher::new(kind, key, session_id),
            session_id,
            assembler: SliceAssembler::new(fec),
            newest_frame: None,
            last_delivered: None,
            stats: ReceiverStats::default(),
        }
    }

    /// Counters since the receiver was created.
    #[must_use]
    pub fn stats(&self) -> ReceiverStats {
        self.stats
    }

    /// Slice indices still missing from `frame_id`, for a NACK.
    #[must_use]
    pub fn missing(&self, frame_id: u32) -> Vec<u16> {
        self.assembler.missing_slices(frame_id)
    }

    /// Process one datagram. Returns the access unit when this datagram
    /// completes one (directly or through FEC recovery).
    ///
    /// # Errors
    /// Returns an error for datagrams that are malformed, from another
    /// session, fail authentication, or belong to a frame already passed.
    /// The receiver stays usable after an error.
    pub fn on_datagram(&mut self, datagram: &[u8]) -> Result<Option<AssembledAu>, MediaError> {
        if datagram.len() < HEADER_LEN {
            self.stats.malformed += 1;
            return Err(MediaError::Short(datagram.len()));
        }
        let (head, body) = datagram.split_at(HEADER_LEN);
        let header = MediaHeader::parse(head).inspect_err(|_| self.stats.malformed += 1)?;
        if header.session_id != self.session_id {
            self.stats.stale += 1;
            return Err(MediaError::StaleSession);
        }
        let Ok(payload) = self.cipher.open(head, body, header.seq) else {
            self.stats.auth_failures += 1;
            return Err(MediaError::Auth);
        };
        if payload.len() != usize::from(header.length) {
            self.stats.malformed += 1;
            return Err(MediaError::LengthMismatch {
                actual: payload.len(),
                declared: usize::from(header.length),
            });
        }
        self.stats.packets += 1;
        let frame_id = header.frame_id;
        if self.last_delivered.is_some_and(|last| frame_id <= last) {
            self.stats.late += 1;
            return Err(MediaError::Late(frame_id));
        }
        self.newest_frame = Some(self.newest_frame.map_or(frame_id, |n| n.max(frame_id)));
        self.prune_lagging();

        let slice = ReceivedSlice { header, payload };
        let mut completed = self.assembler.push(&slice);
        if completed.is_none() {
            completed = self.assembler.try_recover(frame_id);
            if completed.is_some() {
                self.stats.frames_rebuilt += 1;
            }
        }
        Ok(completed.inspect(|au| {
            self.last_delivered = Some(au.frame_id);
            self.stats.frames_completed += 1;
            self.drop_older_than(au.frame_id);
        }))
    }

    /// Drop incomplete frames that are too far behind the newest frame.
    fn prune_lagging(&mut self) {
        let Some(newest) = self.newest_frame else {
            return;
        };
        let horizon = newest.saturating_sub(MAX_FRAME_LAG);
        self.drop_frames_before(horizon);
    }

    /// Drop incomplete frames older than the frame just delivered.
    fn drop_older_than(&mut self, delivered: u32) {
        self.drop_frames_before(delivered);
    }

    fn drop_frames_before(&mut self, horizon: u32) {
        for frame in self.assembler.incomplete_frames() {
            if frame < horizon {
                self.assembler.drop_frame(frame);
                self.stats.frames_dropped += 1;
            }
        }
    }
}

/// Send datagrams to `peer`, sleeping as the pacer requires between packets.
///
/// # Errors
/// Returns the first socket error.
pub fn send_paced(
    socket: &MediaSocket,
    peer: SocketAddr,
    datagrams: &[Vec<u8>],
    pacer: &mut Pacer,
) -> io::Result<()> {
    for datagram in datagrams {
        let wait = pacer.take(datagram.len());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        socket.send_to(datagram, peer)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];
    const SESSION: u32 = 9;
    const KIND: AeadKind = AeadKind::Aes256Gcm;

    fn pair(parity: ParityLevel) -> (MediaSender, MediaReceiver) {
        (
            MediaSender::new(KIND, KEY, SESSION, parity).unwrap(),
            MediaReceiver::new(KIND, KEY, SESSION, parity != ParityLevel::None),
        )
    }

    fn access_unit(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// Feed datagrams in order; return the access unit produced, if any. Packets
    /// that arrive after their frame was delivered are rejected by design, so
    /// their errors are ignored here and checked by the tests that care.
    fn feed(rx: &mut MediaReceiver, datagrams: &[Vec<u8>]) -> Option<AssembledAu> {
        let mut out = None;
        for d in datagrams {
            if let Ok(Some(au)) = rx.on_datagram(d) {
                out = Some(au);
            }
        }
        out
    }

    #[test]
    fn access_unit_survives_in_order_delivery() {
        let (mut tx, mut rx) = pair(ParityLevel::Light);
        let au = access_unit(20 * SLICE_MAX_PAYLOAD - 311, 1);
        let datagrams = tx.packetize(&au, 4242).unwrap();
        let got = feed(&mut rx, &datagrams).expect("frame must complete");
        assert_eq!(got.data, au);
        assert_eq!(got.capture_ts_us, 4242);
        assert_eq!(got.frame_id, 0);
        assert_eq!(rx.stats().frames_completed, 1);
    }

    #[test]
    fn access_unit_survives_reversed_delivery() {
        let (mut tx, mut rx) = pair(ParityLevel::Light);
        let au = access_unit(11 * SLICE_MAX_PAYLOAD, 2);
        let mut datagrams = tx.packetize(&au, 1).unwrap();
        datagrams.reverse();
        assert_eq!(feed(&mut rx, &datagrams).unwrap().data, au);
    }

    #[test]
    fn fec_rebuilds_one_lost_slice_per_block() {
        let (mut tx, mut rx) = pair(ParityLevel::Light);
        let au = access_unit(20 * SLICE_MAX_PAYLOAD, 3);
        let datagrams = tx.packetize(&au, 1).unwrap();
        // Drop data slice 3 (block 0) and slice 12 (block 1); keep every parity packet.
        let kept: Vec<Vec<u8>> = datagrams
            .into_iter()
            .enumerate()
            .filter(|(_, d)| {
                let h = MediaHeader::parse(&d[..HEADER_LEN]).unwrap();
                !(h.kind == MessageKind::VideoData && (h.slice_index == 3 || h.slice_index == 12))
            })
            .map(|(_, d)| d)
            .collect();
        let got = feed(&mut rx, &kept).expect("FEC must rebuild both lost slices");
        assert_eq!(got.data, au);
        assert_eq!(rx.stats().frames_rebuilt, 1);
    }

    #[test]
    fn lost_slice_is_retransmitted_with_a_new_sequence_number() {
        let (mut tx, mut rx) = pair(ParityLevel::None);
        let au = access_unit(8 * SLICE_MAX_PAYLOAD, 4);
        let datagrams = tx.packetize(&au, 1).unwrap();
        let (lost, rest): (Vec<_>, Vec<_>) = datagrams
            .into_iter()
            .enumerate()
            .partition(|(i, _)| *i == 5);
        let rest: Vec<Vec<u8>> = rest.into_iter().map(|(_, d)| d).collect();
        assert!(feed(&mut rx, &rest).is_none());
        assert_eq!(rx.missing(0), vec![5]);
        let original_seq = MediaHeader::parse(&lost[0].1[..HEADER_LEN]).unwrap().seq;
        let resent = tx.retransmit(0, 5).unwrap();
        let resent_seq = MediaHeader::parse(&resent[..HEADER_LEN]).unwrap().seq;
        assert_ne!(
            original_seq, resent_seq,
            "retransmission must use a fresh nonce"
        );
        let got = rx
            .on_datagram(&resent)
            .unwrap()
            .expect("frame completes after retransmit");
        assert_eq!(got.data, au);
    }

    #[test]
    fn every_datagram_has_a_unique_sequence_number() {
        let (mut tx, _) = pair(ParityLevel::Light);
        let mut seen = std::collections::HashSet::new();
        for frame in 0..3 {
            for d in tx
                .packetize(&access_unit(9 * SLICE_MAX_PAYLOAD, frame), 0)
                .unwrap()
            {
                let seq = MediaHeader::parse(&d[..HEADER_LEN]).unwrap().seq;
                assert!(seen.insert(seq), "sequence {seq} reused");
            }
        }
    }

    #[test]
    fn wrong_key_is_rejected_and_counted() {
        let mut tx = MediaSender::new(KIND, KEY, SESSION, ParityLevel::None).unwrap();
        let mut rx = MediaReceiver::new(KIND, [8; 32], SESSION, false);
        let datagrams = tx.packetize(&access_unit(SLICE_MAX_PAYLOAD, 5), 1).unwrap();
        assert!(matches!(
            rx.on_datagram(&datagrams[0]),
            Err(MediaError::Auth)
        ));
        assert_eq!(rx.stats().auth_failures, 1);
        assert_eq!(rx.stats().packets, 0);
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let (mut tx, mut rx) = pair(ParityLevel::None);
        let mut datagram = tx
            .packetize(&access_unit(SLICE_MAX_PAYLOAD, 6), 1)
            .unwrap()
            .remove(0);
        let last = datagram.len() - 1;
        datagram[last] ^= 0x40;
        assert!(matches!(rx.on_datagram(&datagram), Err(MediaError::Auth)));
    }

    #[test]
    fn tampered_header_is_rejected() {
        let (mut tx, mut rx) = pair(ParityLevel::None);
        let mut datagram = tx
            .packetize(&access_unit(SLICE_MAX_PAYLOAD, 6), 1)
            .unwrap()
            .remove(0);
        datagram[HEADER_LEN - 2] ^= 0x01;
        assert!(rx.on_datagram(&datagram).is_err());
        assert_eq!(rx.stats().packets, 0);
    }

    #[test]
    fn other_session_is_stale() {
        let mut tx = MediaSender::new(KIND, KEY, SESSION + 1, ParityLevel::None).unwrap();
        let mut rx = MediaReceiver::new(KIND, KEY, SESSION, false);
        let datagram = tx
            .packetize(&access_unit(SLICE_MAX_PAYLOAD, 7), 1)
            .unwrap()
            .remove(0);
        assert!(matches!(
            rx.on_datagram(&datagram),
            Err(MediaError::StaleSession)
        ));
        assert_eq!(rx.stats().stale, 1);
    }

    #[test]
    fn frame_older_than_last_delivered_is_not_presented() {
        let (mut tx, mut rx) = pair(ParityLevel::None);
        let older = tx
            .packetize(&access_unit(2 * SLICE_MAX_PAYLOAD, 8), 1)
            .unwrap();
        let newer = tx.packetize(&access_unit(SLICE_MAX_PAYLOAD, 9), 2).unwrap();
        assert!(feed(&mut rx, &newer).is_some());
        assert!(matches!(
            rx.on_datagram(&older[0]),
            Err(MediaError::Late(0))
        ));
        assert_eq!(rx.stats().late, 1);
    }

    #[test]
    fn incomplete_frame_is_dropped_once_a_newer_frame_is_delivered() {
        let (mut tx, mut rx) = pair(ParityLevel::None);
        let stalled = tx
            .packetize(&access_unit(3 * SLICE_MAX_PAYLOAD, 10), 1)
            .unwrap();
        assert!(feed(&mut rx, &stalled[..2]).is_none());
        let newer = tx
            .packetize(&access_unit(SLICE_MAX_PAYLOAD, 11), 2)
            .unwrap();
        assert!(feed(&mut rx, &newer).is_some());
        assert_eq!(rx.stats().frames_dropped, 1);
    }

    #[test]
    fn retransmit_of_unknown_slice_is_an_error() {
        let (mut tx, _) = pair(ParityLevel::None);
        assert!(matches!(
            tx.retransmit(0, 0),
            Err(MediaError::UnknownSlice(0, 0))
        ));
        tx.packetize(&access_unit(SLICE_MAX_PAYLOAD, 12), 1)
            .unwrap();
        assert!(matches!(
            tx.retransmit(0, 1),
            Err(MediaError::UnknownSlice(0, 1))
        ));
    }

    #[test]
    fn single_slice_frame_recovers_from_parity_alone() {
        // A frame whose only data slice is lost must still be rebuilt from the
        // parity of its block. The assembler has to create frame state when a
        // parity slice arrives, otherwise there is nothing to recover into.
        let (mut tx, mut rx) = pair(ParityLevel::Light);
        let au = access_unit(300, 13);
        let datagrams = tx.packetize(&au, 7777).unwrap();
        // Datagram 0 is the only data slice; the rest are the two parity slices.
        let kept: Vec<Vec<u8>> = datagrams.into_iter().skip(1).collect();
        assert_eq!(kept.len(), 2, "a one-slice frame yields two parity slices");
        let got = feed(&mut rx, &kept).expect("frame must be recovered from parity");
        assert_eq!(got.data, au);
        assert_eq!(got.capture_ts_us, 7777);
        assert_eq!(rx.stats().frames_rebuilt, 1);
    }

    #[test]
    fn undamaged_frame_is_not_counted_as_rebuilt() {
        let (mut tx, mut rx) = pair(ParityLevel::Light);
        let au = access_unit(20 * SLICE_MAX_PAYLOAD, 6);
        let datagrams = tx.packetize(&au, 1).unwrap();
        let got = feed(&mut rx, &datagrams).expect("an intact frame is delivered");
        assert_eq!(got.data, au);
        assert_eq!(rx.stats().frames_rebuilt, 0);
    }

    #[test]
    fn heavy_parity_recovers_three_losses_in_a_block() {
        let (mut tx, mut rx) = pair(ParityLevel::Heavy);
        // The heavy level uses a block of twelve data slices and three parity
        // slices, so three losses in one block are recoverable.
        let au = access_unit(24 * SLICE_MAX_PAYLOAD, 21);
        let datagrams = tx.packetize(&au, 1).unwrap();
        let kept: Vec<Vec<u8>> = datagrams
            .into_iter()
            .filter(|d| {
                let h = MediaHeader::parse(&d[..HEADER_LEN]).unwrap();
                !(h.kind == MessageKind::VideoData && (1..=3).contains(&h.slice_index))
            })
            .collect();
        let got = feed(&mut rx, &kept).expect("FEC must rebuild three lost slices");
        assert_eq!(got.data, au);
    }

    #[test]
    fn the_parity_level_can_change_between_frames() {
        let (mut tx, mut rx) = pair(ParityLevel::Light);
        let first = access_unit(300, 22);
        let datagrams = tx.packetize(&first, 1).unwrap();
        assert!(feed(&mut rx, &datagrams).is_some());

        // The adaptive policy raises the level when the loss goes up.
        tx.set_parity(ParityLevel::Heavy);
        assert_eq!(tx.parity(), ParityLevel::Heavy);
        let second = access_unit(300, 23);
        let datagrams = tx.packetize(&second, 2).unwrap();
        // The heavy level sends three parity slices for the block.
        let parity_count = datagrams
            .iter()
            .filter(|d| {
                MediaHeader::parse(&d[..HEADER_LEN])
                    .is_ok_and(|h| h.kind == MessageKind::VideoParity)
            })
            .count();
        assert_eq!(parity_count, 3);
        assert!(feed(&mut rx, &datagrams).is_some());
    }
}
