//! Raw PCM mode: f32 samples travel as little-endian bytes, bit-for-bit unchanged.

use crate::{AudioDecoder, AudioEncoder};

#[derive(Debug)]
pub struct PcmEncoder;

impl AudioEncoder for PcmEncoder {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(samples.len() * 4);
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        Ok(out)
    }
}

#[derive(Debug)]
pub struct PcmDecoder;

impl AudioDecoder for PcmDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<f32>, String> {
        if !data.len().is_multiple_of(4) {
            return Err(format!(
                "PCM payload of {} bytes is not whole f32 samples",
                data.len()
            ));
        }
        Ok(data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_bit_exact() {
        let input = [0.25f32, -1.5, f32::MIN_POSITIVE, -0.0, 1e-30, f32::NAN];
        let bytes = PcmEncoder.encode(&input).expect("encode");
        assert_eq!(bytes.len(), input.len() * 4);
        let output = PcmDecoder.decode(&bytes).expect("decode");
        let same = input
            .iter()
            .zip(&output)
            .all(|(a, b)| a.to_bits() == b.to_bits());
        assert!(same, "PCM must pass samples through untouched");
    }

    #[test]
    fn rejects_partial_sample() {
        assert!(PcmDecoder.decode(&[1, 2, 3]).is_err());
    }
}
