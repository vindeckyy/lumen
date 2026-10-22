#![no_main]

//! Splitting one H.264 access unit into transport slices and back.
//!
//! Proves that an access unit survives the trip through the slicing layer:
//! the protocol splits it into uniform slices and the reader reassembles it
//! byte for byte, the sealed datagrams of the media path all describe the
//! same access unit in their headers, and a lost data slice is rebuilt from
//! the parity of its block.
//!
//! Found so far: nothing. Every access unit came back unchanged and the
//! header always matched the access unit it carried.

use libfuzzer_sys::fuzz_target;
use lumen_crypto::AeadKind;
use lumen_protocol::message::MessageKind;
use lumen_protocol::slice::{SLICE_MAX_PAYLOAD, SliceReader, slice_payload, split};
use lumen_protocol::{HEADER_LEN, MediaHeader};
use lumen_transport::fec::ParityLevel;
use lumen_transport::media::{MediaReceiver, MediaSender};

const KEY: [u8; 32] = [7; 32];
const SESSION: u32 = 9;
const KIND: AeadKind = AeadKind::Aes256Gcm;
const TIMESTAMP_US: u64 = 4242;

fuzz_target!(|data: &[u8]| {
    let au = data;

    // Part 1: the protocol splits an access unit into uniform slices.
    let descs = split(au).expect("slice count fits u16 for fuzz-sized input");
    let count = descs.len();
    assert_eq!(count, au.len().div_ceil(SLICE_MAX_PAYLOAD));
    if au.is_empty() {
        // An empty access unit has no slices, so no slice is ever inserted and
        // the reader never becomes complete.
        assert!(descs.is_empty());
    } else {
        for (i, d) in descs.iter().enumerate() {
            assert_eq!(d.index, i as u16);
            assert_eq!(d.count, count as u16);
            assert_eq!(d.offset as usize, i * SLICE_MAX_PAYLOAD);
            assert_eq!(d.length as usize, SLICE_MAX_PAYLOAD);
        }
        // Reassembling out of order must reproduce the access unit exactly.
        let mut reader = SliceReader::new(count as u16, count * SLICE_MAX_PAYLOAD);
        for d in descs.iter().rev() {
            reader
                .insert(d, &slice_payload(au, d))
                .expect("split descriptors are in range");
        }
        assert!(reader.is_complete());
        assert_eq!(reader.finish(au.len()), Some(au));
    }

    // Part 2: the transport path. Every datagram header describes the same
    // access unit, and the sealed datagrams reassemble into it again.
    let mut tx = MediaSender::new(KIND, KEY, SESSION, ParityLevel::None).unwrap();
    let mut rx = MediaReceiver::new(KIND, KEY, SESSION, false);
    let datagrams = tx.packetize(au, TIMESTAMP_US).unwrap();
    if au.is_empty() {
        assert!(
            datagrams.is_empty(),
            "an empty access unit produces no datagram"
        );
        return;
    }
    let mut indices = Vec::new();
    for d in &datagrams {
        let h = MediaHeader::parse(&d[..HEADER_LEN]).expect("the sender writes a valid header");
        assert_eq!(h.slice_count as usize, count);
        assert_eq!(h.au_len as usize, au.len());
        assert_eq!(h.length as usize, SLICE_MAX_PAYLOAD);
        assert_eq!(h.timestamp_us, TIMESTAMP_US);
        assert_eq!(h.frame_id, 0);
        assert_eq!(h.session_id, SESSION);
        if h.kind == MessageKind::VideoData {
            indices.push(h.slice_index);
        }
    }
    indices.sort_unstable();
    assert_eq!(indices, (0..count as u16).collect::<Vec<u16>>());
    let mut got = None;
    for d in &datagrams {
        match rx.on_datagram(d) {
            Ok(au) => got = au,
            Err(e) => panic!("the receiver must accept every datagram of its own frame: {e}"),
        }
    }
    let got = got.expect("every slice arrives, so the frame completes");
    assert_eq!(got.data.as_slice(), au);
    assert_eq!(got.capture_ts_us, TIMESTAMP_US);
    assert_eq!(got.frame_id, 0);

    // Part 3: with parity, one lost data slice is rebuilt from the parity of
    // its block, and the access unit still comes back whole.
    let mut tx = MediaSender::new(KIND, KEY, SESSION, ParityLevel::Light).unwrap();
    let mut rx = MediaReceiver::new(KIND, KEY, SESSION, true);
    let datagrams = tx.packetize(au, TIMESTAMP_US).unwrap();
    let victim = usize::from(data.first().copied().unwrap_or(0)) % count;
    let mut got = None;
    let mut parity = 0usize;
    for d in &datagrams {
        let h = MediaHeader::parse(&d[..HEADER_LEN]).unwrap();
        if h.kind == MessageKind::VideoParity {
            parity += 1;
        } else if h.slice_index as usize == victim {
            // The data slice is lost on the way to the receiver.
            continue;
        }
        // Datagrams that arrive after their frame was delivered are refused by
        // design, so their errors are ignored here.
        if let Ok(Some(au)) = rx.on_datagram(d) {
            got = Some(au);
        }
    }
    assert!(parity > 0, "a frame with parity carries parity slices");
    let got = got.expect("the parity of the block rebuilds the lost slice");
    assert_eq!(got.data.as_slice(), au);
    assert_eq!(rx.fec_recoveries(), 1, "exactly one slice was rebuilt");
});
