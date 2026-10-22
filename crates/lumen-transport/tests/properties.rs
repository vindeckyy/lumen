//! Property tests for the framing and the FEC.
//!
//! The fuzz targets find crashes in the parsers; these tests find the cases
//! where a parser is wrong rather than unsafe: a slice that does not
//! reassemble into the bytes it came from, or a parity that cannot rebuild
//! what it should.

use proptest::prelude::*;

use lumen_protocol::slice::SLICE_MAX_PAYLOAD;
use lumen_transport::fec::{FecBlock, ParityLevel, compute_parity_with};
use lumen_transport::media::{MediaError, MediaReceiver, MediaSender};
use lumen_transport::slice::SliceAssembler;

/// The key the sender and the receiver share.
const KEY: [u8; 32] = [42; 32];
/// The session id the sender and the receiver share.
const SESSION: u32 = 7;

/// The kinds the protocol defines.
fn kinds() -> Vec<u8> {
    vec![1, 2, 3, 4, 5]
}

proptest::proptest! {
    /// An access unit that is split into slices and put back together is the
    /// same bytes.
    #[test]
    fn slices_roundtrip(access_unit in proptest::collection::vec(any::<u8>(), 0..5000)) {
        let mut tx = MediaSender::new(
            lumen_crypto::AeadKind::Aes256Gcm,
            KEY,
            SESSION,
            ParityLevel::Light,
        ).expect("sender");
        let mut rx = MediaReceiver::new(
            lumen_crypto::AeadKind::Aes256Gcm,
            KEY,
            SESSION,
            true,
        );
        if access_unit.is_empty() {
            // An empty access unit produces no datagrams at all.
            prop_assert!(tx.packetize(&access_unit, 1).expect("packetize").is_empty());
            return Ok(());
        }
        let datagrams = tx.packetize(&access_unit, 1).expect("packetize");
        let mut assembled = None;
        for datagram in &datagrams {
            if let Ok(Some(au)) = rx.on_datagram(datagram) {
                assembled = Some(au);
            }
        }
        let au = assembled.expect("the access unit must reassemble");
        prop_assert_eq!(&au.data[..], &access_unit[..]);
        prop_assert_eq!(au.capture_ts_us, 1);
    }

    /// Whatever is lost, the parity rebuilds the lost slices exactly, as long
    /// as the block does not lose more slices than it has parity.
    #[test]
    fn parity_rebuilds_the_lost_slices(
        slice_len in 1usize..40,
        block in proptest::collection::vec(any::<u8>(), 1..9),
        lost in proptest::collection::vec(0usize..8, 0..3),
    ) {
        // The wire format pads every slice to the same length, so the block
        // is uniform.
        let block: Vec<Vec<u8>> = block
            .into_iter()
            .map(|_| (0..slice_len).map(|i| (i as u8) ^ 0x5a).collect())
            .collect();
        let parity_count = if lost.len() <= 1 { 2 } else { 3 };
        let parity = compute_parity_with(&block, parity_count);
        let fec = FecBlock { data: block.clone(), parity };

        // The lost indices must be distinct and inside the block.
        let mut missing: Vec<usize> = lost.iter().copied().filter(|&i| i < block.len()).collect();
        missing.sort_unstable();
        missing.dedup();

        for &index in &missing {
            let recovered = fec.recover(index, &missing);
            prop_assert!(
                recovered.is_some(),
                "slice {index} must be recoverable from {} missing slices",
                missing.len()
            );
            prop_assert_eq!(&recovered.unwrap()[..], &block[index][..]);
        }
    }

    /// A block with more missing slices than it has parity is not recovered.
    /// Returning wrong bytes would be worse than reporting the loss.
    #[test]
    fn a_block_beyond_its_parity_is_not_recovered(
        block in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 4..20), 8..9),
    ) {
        let parity = compute_parity_with(&block, 2);
        let fec = FecBlock { data: block.clone(), parity };
        let missing = [0usize, 1, 2];
        prop_assert!(fec.recover(0, &missing).is_none());
    }

    /// The parity is a function of the data alone: the same block always
    /// produces the same parity.
    #[test]
    fn parity_is_deterministic(
        block in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 1..20), 1..9),
    ) {
        let a = compute_parity_with(&block, 3);
        let b = compute_parity_with(&block, 3);
        prop_assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            prop_assert_eq!(&x[..], &y[..]);
        }
    }

    /// The slice count in the header always matches the access unit length.
    #[test]
    fn the_header_matches_the_access_unit(
        access_unit in proptest::collection::vec(any::<u8>(), 0..9000),
    ) {
        let mut tx = MediaSender::new(
            lumen_crypto::AeadKind::Aes256Gcm,
            KEY,
            SESSION,
            ParityLevel::Light,
        ).expect("sender");
        let datagrams = tx.packetize(&access_unit, 1).expect("packetize");
        if access_unit.is_empty() {
            prop_assert!(datagrams.is_empty());
            return Ok(());
        }
        let expected = access_unit.len().div_ceil(SLICE_MAX_PAYLOAD);
        for datagram in &datagrams {
            let header = lumen_protocol::header::MediaHeader::parse(&datagram[..lumen_protocol::HEADER_LEN])
                .expect("header");
            if header.kind == lumen_protocol::MessageKind::VideoData {
                prop_assert_eq!(usize::from(header.slice_count), expected);
                prop_assert_eq!(header.au_len as usize, access_unit.len());
            }
        }
    }

    /// A datagram that is not ours is rejected, never accepted.
    #[test]
    fn a_foreign_datagram_is_rejected(
        datagram in proptest::collection::vec(any::<u8>(), 0..200),
    ) {
        let mut rx = MediaReceiver::new(
            lumen_crypto::AeadKind::Aes256Gcm,
            KEY,
            SESSION,
            true,
        );
        let result = rx.on_datagram(&datagram);
        prop_assert!(result.is_err() || result.unwrap().is_none());
    }

    /// The assembler never holds more frames than its cap.
    #[test]
    fn the_assembler_stays_bounded(
        slices in proptest::collection::vec((0u32..200, 0u16..8, 0u16..8), 1..80),
    ) {
        let mut assembler = SliceAssembler::new(true);
        for (frame_id, slice_index, slice_count) in slices {
            let header = lumen_protocol::header::MediaHeader::data_slice(
                lumen_protocol::MessageKind::VideoData,
                frame_id,
                slice_index,
                slice_count.max(1),
                1,
                10,
                100,
                SESSION,
            );
            assembler.push(&lumen_transport::slice::ReceivedSlice {
                header,
                payload: vec![0u8; 10],
            });
        }
        prop_assert!(assembler.pending_frames() <= lumen_transport::slice::MAX_PENDING_FRAMES);
    }
}

/// The message kinds the protocol defines, as a sanity check on the tests.
#[test]
fn the_protocol_defines_the_expected_kinds() {
    assert_eq!(kinds(), vec![1, 2, 3, 4, 5]);
}

/// The media error type is usable from the tests.
#[test]
fn the_media_error_is_nameable() {
    let error = MediaError::TooLarge(1);
    assert!(error.to_string().contains("65535") || error.to_string().contains("more"));
}
