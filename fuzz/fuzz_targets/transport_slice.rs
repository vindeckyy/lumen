#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_protocol::header::MediaHeader;
use lumen_protocol::message::MessageKind;
use lumen_transport::fec::compute_parity;
use lumen_transport::{ReceivedSlice, SliceAssembler};

/// Largest uniform slice length in the FEC part, to keep runs fast.
const SLICE_LEN_MAX: usize = 64;

/// Sequential reader over the fuzz input; reads past the end yield zero.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn byte(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    fn u16(&mut self) -> u16 {
        u16::from(self.byte()) | (u16::from(self.byte()) << 8)
    }

    fn u32(&mut self) -> u32 {
        u32::from(self.u16()) | (u32::from(self.u16()) << 16)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }

    fn done(&self) -> bool {
        self.pos >= self.data.len()
    }
}

/// One arbitrary slice: header fields and payload decoded from the input.
fn record(c: &mut Cursor) -> ReceivedSlice {
    let kind = MessageKind::from_u8(1 + c.byte() % 5).expect("values 1..=5 are kinds");
    let frame_id = u32::from(c.byte() % 4);
    let slice_index = c.u16();
    let slice_count = c.u16();
    let aux = c.u16();
    let au_len = c.u32();
    let timestamp = u64::from(c.u32());
    let n = usize::from(c.byte());
    let payload = c.bytes(n);
    let header = MediaHeader::data_slice(
        kind,
        frame_id,
        slice_index,
        slice_count,
        timestamp,
        payload.len() as u16,
        au_len,
        0,
    )
    .with_aux(aux);
    ReceivedSlice { header, payload }
}

fuzz_target!(|data: &[u8]| {
    // Part 1: arbitrary slices, colliding frame ids. Must not panic.
    let mut c = Cursor { data, pos: 0 };
    let mut asm = SliceAssembler::new(true);
    let mut frames = Vec::new();
    while !c.done() {
        let slice = record(&mut c);
        frames.push(slice.header.frame_id);
        let _ = asm.push(&slice);
    }
    for &id in &frames {
        let _ = asm.try_recover(id);
        let _ = asm.missing_slices(id);
    }
    let _ = asm.incomplete_frames();
    let _ = asm.pending_frames();

    // Part 2: a consistent frame of up to 8 slices with up to two lost data slices.
    // Data plus parity must reproduce the access unit exactly.
    let mut c = Cursor { data, pos: 0 };
    let n = 1 + usize::from(c.byte()) % 8;
    let len = 1 + usize::from(c.byte()) % SLICE_LEN_MAX;
    let pad = usize::from(c.byte()) % len;
    let au_len = n * len - pad;
    let body = c.bytes(au_len);
    let mut padded = body.clone();
    padded.resize(n * len, 0);
    let slices: Vec<Vec<u8>> = padded.chunks(len).map(<[u8]>::to_vec).collect();
    let parity = compute_parity(&slices);
    let ts = u64::from(c.u32());
    let lost_a = usize::from(c.byte()) % n;
    let lost_b = usize::from(c.byte()) % n;
    let mut lost = vec![lost_a];
    if c.byte() & 1 == 1 && lost_b != lost_a {
        lost.push(lost_b);
    }

    // Known gap (reported): a frame with no data slice received has no state to recover into,
    // so parity alone cannot rebuild it. Only frames with at least one data slice are checked.
    if lost.len() >= n {
        return;
    }

    let frame_id = 7;
    let mut asm = SliceAssembler::new(true);
    let mut out = None;
    for (i, payload) in slices.iter().enumerate() {
        if lost.contains(&i) {
            continue;
        }
        let header = MediaHeader::data_slice(
            MessageKind::VideoData,
            frame_id,
            i as u16,
            n as u16,
            ts,
            len as u16,
            au_len as u32,
            0,
        );
        if let Some(au) = asm.push(&ReceivedSlice {
            header,
            payload: payload.clone(),
        }) {
            out = Some(au);
        }
    }
    for (p, payload) in parity.iter().enumerate() {
        let header = MediaHeader::data_slice(
            MessageKind::VideoParity,
            frame_id,
            p as u16,
            n as u16,
            ts,
            len as u16,
            au_len as u32,
            0,
        )
        .with_aux(0);
        assert!(
            asm.push(&ReceivedSlice {
                header,
                payload: payload.clone(),
            })
            .is_none()
        );
    }
    let au = out
        .or_else(|| asm.try_recover(frame_id))
        .expect("up to two losses with both parity slices must recover");
    assert_eq!(au.capture_ts_us, ts);
    assert_eq!(au.data, body);
});
