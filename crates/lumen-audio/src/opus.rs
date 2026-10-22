//! Opus at 48 kHz: restricted low delay, CBR, mono or stereo.
//!
//! The `opus` crate is referenced as `::opus` because this module shares its name.

use crate::{AudioConfig, AudioDecoder, AudioEncoder};

/// Generous upper bound on one Opus packet (libopus caps it at 3 × 1275 bytes plus header).
const MAX_PACKET: usize = 4000;
/// Longest Opus frame, 120 ms, per channel at 48 kHz.
const MAX_FRAME_SAMPLES: usize = 5760;

fn channels_of(cfg: &AudioConfig) -> Result<::opus::Channels, String> {
    if cfg.sample_rate != 48_000 {
        return Err(format!(
            "Opus here runs at 48 kHz, got {} Hz",
            cfg.sample_rate
        ));
    }
    match cfg.channels {
        1 => Ok(::opus::Channels::Mono),
        2 => Ok(::opus::Channels::Stereo),
        n => Err(format!("Opus supports 1 or 2 channels, got {n}")),
    }
}

pub struct OpusEncoder {
    inner: ::opus::Encoder,
    /// Interleaved samples per frame.
    frame_len: usize,
    out: Vec<u8>,
}

impl OpusEncoder {
    pub fn new(cfg: &AudioConfig) -> Result<Self, String> {
        let channels = channels_of(cfg)?;
        let frame_len = cfg.frame_samples()? * usize::from(cfg.channels);
        let mut inner =
            ::opus::Encoder::new(cfg.sample_rate, channels, ::opus::Application::LowDelay)
                .map_err(|e| format!("opus: {e}"))?;
        inner.set_vbr(false).map_err(|e| format!("opus: {e}"))?;
        inner
            .set_bitrate(::opus::Bitrate::Bits(cfg.bitrate_bps))
            .map_err(|e| format!("opus: {e}"))?;
        Ok(Self {
            inner,
            frame_len,
            out: vec![0; MAX_PACKET],
        })
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, String> {
        if samples.len() != self.frame_len {
            return Err(format!(
                "Opus frame needs {} samples, got {}",
                self.frame_len,
                samples.len()
            ));
        }
        let n = self
            .inner
            .encode_float(samples, &mut self.out)
            .map_err(|e| format!("opus: {e}"))?;
        Ok(self.out[..n].to_vec())
    }
}

pub struct OpusDecoder {
    inner: ::opus::Decoder,
    channels: usize,
    pcm: Vec<f32>,
}

impl OpusDecoder {
    pub fn new(cfg: &AudioConfig) -> Result<Self, String> {
        let channels = channels_of(cfg)?;
        let inner =
            ::opus::Decoder::new(cfg.sample_rate, channels).map_err(|e| format!("opus: {e}"))?;
        let n = usize::from(cfg.channels);
        Ok(Self {
            inner,
            channels: n,
            pcm: vec![0.0; MAX_FRAME_SAMPLES * n],
        })
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<f32>, String> {
        if data.is_empty() {
            return Err("empty Opus packet".into());
        }
        let n = self
            .inner
            .decode_float(data, &mut self.pcm, false)
            .map_err(|e| format!("opus: {e}"))?;
        Ok(self.pcm[..n * self.channels].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn cfg(channels: u16, frame_ms: u32) -> AudioConfig {
        AudioConfig {
            sample_rate: RATE,
            channels,
            frame_ms,
            bitrate_bps: 96_000,
            use_opus: true,
        }
    }

    /// 440 Hz sine, interleaved over `channels`, starting at sample index `start`.
    fn sine(start: usize, frames: usize, channels: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|n| {
                let v = 0.5 * (std::f32::consts::TAU * 440.0 * n as f32 / RATE as f32).sin();
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    /// Best normalised correlation of `decoded[n + lag]` against `reference[n]`
    /// over the second half of the reference, for lags in `0..=max_lag`.
    fn best_lag_correlation(reference: &[f32], decoded: &[f32], max_lag: usize) -> f64 {
        let start = reference.len() / 2;
        let end = reference.len() - max_lag;
        (0..=max_lag)
            .map(|lag| {
                let (mut xy, mut xx, mut yy) = (0.0f64, 0.0f64, 0.0f64);
                for n in start..end {
                    let x = f64::from(reference[n]);
                    let y = f64::from(decoded[n + lag]);
                    xy += x * y;
                    xx += x * x;
                    yy += y * y;
                }
                xy / (xx * yy).sqrt()
            })
            .fold(f64::MIN, f64::max)
    }

    #[test]
    fn mono_round_trip_tracks_sine() {
        let mut enc = OpusEncoder::new(&cfg(1, 50)).expect("open Opus encoder");
        let mut dec = OpusDecoder::new(&cfg(1, 50)).expect("open Opus decoder");
        let frames = 50; // 250 ms, so the codec's lookahead has settled before we compare
        let input = sine(0, 240 * frames, 1);
        let mut decoded = Vec::new();
        for chunk in input.as_chunks::<240>().0 {
            let packet = enc.encode(chunk).expect("encode 240 samples");
            assert_ne!(packet.len(), 0);
            let pcm = dec.decode(&packet).expect("decode packet");
            assert_eq!(pcm.len(), 240, "one 5 ms packet decodes to 240 samples");
            decoded.extend(pcm);
        }
        let corr = best_lag_correlation(&input, &decoded, 480);
        assert!(corr > 0.9, "decoded sine correlates with input: {corr}");
    }

    #[test]
    fn every_frame_size_round_trips_stereo() {
        for (frame_ms, samples) in [(25, 120), (50, 240), (100, 480)] {
            let c = cfg(2, frame_ms);
            let mut enc = OpusEncoder::new(&c).expect("open encoder");
            let mut dec = OpusDecoder::new(&c).expect("open decoder");
            let input = sine(0, samples, 2);
            let packet = enc.encode(&input).expect("encode one frame");
            let pcm = dec.decode(&packet).expect("decode one frame");
            assert_eq!(pcm.len(), samples * 2, "frame_ms {frame_ms}");
        }
    }

    #[test]
    fn rejects_wrong_frame_length_and_empty_packet() {
        let mut enc = OpusEncoder::new(&cfg(1, 50)).expect("open encoder");
        assert!(enc.encode(&[0.0; 100]).is_err());
        let mut dec = OpusDecoder::new(&cfg(1, 50)).expect("open decoder");
        assert!(dec.decode(&[]).is_err());
    }

    #[test]
    fn rejects_unsupported_layout() {
        let mut bad_rate = cfg(1, 50);
        bad_rate.sample_rate = 44_100;
        assert!(OpusEncoder::new(&bad_rate).is_err());
        assert!(OpusEncoder::new(&cfg(6, 50)).is_err());
    }
}
