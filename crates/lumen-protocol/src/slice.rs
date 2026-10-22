//! Access-unit slicing.
//!
//! An encoded access unit is split into transport slices of a uniform length
//! so that FEC can recover a missing slice algebraically. The final slice is
//! zero padded to that length; the true access unit length travels in the
//! packet header (`au_len`), so the receiver knows where the bitstream ends.
//! Zero padding is legal in an Annex-B bitstream, where decoders stop at the
//! end of the last NAL.

use crate::error::ProtocolError;

/// Maximum payload carried by one transport slice.
pub const SLICE_MAX_PAYLOAD: usize = 1200;

/// Description of one slice inside an access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceDescriptor {
    /// Index of the slice, zero based.
    pub index: u16,
    /// Total number of slices in the access unit.
    pub count: u16,
    /// Byte offset of the slice payload inside the access unit.
    pub offset: u32,
    /// Length of the slice payload as transmitted, always
    /// [`SLICE_MAX_PAYLOAD`] except for an empty access unit.
    pub length: u16,
}

/// Split a buffer into uniform transport slices.
///
/// The number of slices is `ceil(len / SLICE_MAX_PAYLOAD)`, clamped to
/// `u16::MAX`.
///
/// # Errors
/// Returns [`ProtocolError::LengthMismatch`] when the payload cannot be
/// described by the slice count.
pub fn split(payload: &[u8]) -> Result<Vec<SliceDescriptor>, ProtocolError> {
    if payload.is_empty() {
        return Ok(Vec::new());
    }
    let count = payload.len().div_ceil(SLICE_MAX_PAYLOAD);
    if count > u16::MAX as usize {
        return Err(ProtocolError::LengthMismatch(count, u16::MAX as usize));
    }
    let count_u16 = count as u16;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(SliceDescriptor {
            index: i as u16,
            count: count_u16,
            offset: (i * SLICE_MAX_PAYLOAD) as u32,
            length: SLICE_MAX_PAYLOAD as u16,
        });
    }
    Ok(out)
}

/// The padded payload of one slice.
#[must_use]
pub fn slice_payload(payload: &[u8], desc: &SliceDescriptor) -> Vec<u8> {
    let mut out = vec![0u8; SLICE_MAX_PAYLOAD];
    let start = desc.offset as usize;
    if start < payload.len() {
        // A descriptor may claim more than one slice; never copy past the padded slice.
        let n = (payload.len() - start)
            .min(desc.length as usize)
            .min(SLICE_MAX_PAYLOAD);
        out[..n].copy_from_slice(&payload[start..start + n]);
    }
    out
}

/// Reassembles slices back into an access unit.
#[derive(Debug, Default)]
pub struct SliceReader {
    buffer: Vec<u8>,
    received: Vec<bool>,
    slices: u16,
    complete: bool,
}

impl SliceReader {
    /// Create a reader expecting `slices` slices of an access unit whose
    /// padded length is `slices * SLICE_MAX_PAYLOAD` bytes.
    #[must_use]
    pub fn new(slices: u16, total: usize) -> Self {
        Self {
            buffer: vec![0u8; total],
            received: vec![false; slices as usize],
            slices,
            complete: false,
        }
    }

    /// Number of slices still missing.
    #[must_use]
    pub fn missing(&self) -> usize {
        self.received.iter().filter(|r| !**r).count()
    }

    /// Whether every slice has arrived.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Insert one slice.
    ///
    /// # Errors
    /// Returns [`ProtocolError::BadSliceIndex`] when the index is out of range
    /// and [`ProtocolError::LengthMismatch`] when the slice does not fit.
    pub fn insert(&mut self, desc: &SliceDescriptor, payload: &[u8]) -> Result<(), ProtocolError> {
        if desc.index >= self.slices {
            return Err(ProtocolError::BadSliceIndex(desc.index, self.slices));
        }
        let start = desc.offset as usize;
        let end = start + payload.len();
        if end > self.buffer.len() {
            return Err(ProtocolError::LengthMismatch(end, self.buffer.len()));
        }
        self.buffer[start..end].copy_from_slice(payload);
        self.received[desc.index as usize] = true;
        if self.missing() == 0 {
            self.complete = true;
        }
        Ok(())
    }

    /// The reassembled access unit, if complete, trimmed to `au_len` bytes.
    #[must_use]
    pub fn finish(&self, au_len: usize) -> Option<&[u8]> {
        if !self.complete {
            return None;
        }
        Some(&self.buffer[..au_len.min(self.buffer.len())])
    }

    /// The partial buffer, for diagnostics.
    #[must_use]
    pub fn partial(&self) -> &[u8] {
        &self.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_reassemble() {
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let descs = split(&payload).unwrap();
        assert_eq!(descs.len(), 5);
        for d in &descs {
            assert_eq!(d.length as usize, SLICE_MAX_PAYLOAD);
        }
        let mut reader = SliceReader::new(descs.len() as u16, descs.len() * SLICE_MAX_PAYLOAD);
        // Deliver out of order.
        for d in descs.iter().rev() {
            reader.insert(d, &slice_payload(&payload, d)).unwrap();
        }
        assert!(reader.is_complete());
        assert_eq!(reader.finish(payload.len()).unwrap(), payload.as_slice());
    }

    #[test]
    fn missing_count_tracks_progress() {
        let payload = vec![0u8; 3000];
        let descs = split(&payload).unwrap();
        let mut reader = SliceReader::new(descs.len() as u16, descs.len() * SLICE_MAX_PAYLOAD);
        assert_eq!(reader.missing(), 3);
        let d = descs[0];
        reader.insert(&d, &slice_payload(&payload, &d)).unwrap();
        assert_eq!(reader.missing(), 2);
        assert!(reader.finish(payload.len()).is_none());
    }

    #[test]
    fn rejects_out_of_range_index() {
        let mut reader = SliceReader::new(2, 100);
        let d = SliceDescriptor {
            index: 5,
            count: 2,
            offset: 0,
            length: 10,
        };
        assert_eq!(
            reader.insert(&d, &[0; 10]),
            Err(ProtocolError::BadSliceIndex(5, 2))
        );
    }

    #[test]
    fn empty_payload_yields_no_slices() {
        assert_eq!(split(&[]).unwrap().len(), 0);
    }

    #[test]
    fn overlong_descriptor_is_clamped_to_padded_slice() {
        // Found by fuzzing protocol_slice: length 2000 used to index past the 1200-byte slice.
        let payload = vec![7u8; 3000];
        let desc = SliceDescriptor {
            index: 0,
            count: 1,
            offset: 0,
            length: 2000,
        };
        let padded = slice_payload(&payload, &desc);
        assert_eq!(padded.len(), SLICE_MAX_PAYLOAD);
        assert!(padded.iter().all(|&b| b == 7));
    }
}
