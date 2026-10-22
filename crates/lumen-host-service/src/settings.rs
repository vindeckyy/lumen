//! The settings the control plane can change while the host runs.
//!
//! Everything here is an atomic, because the stages read it on the hot path
//! and the control plane writes it from another thread. The values are the
//! ones an operator wants to move while a session is up: the quantiser, the
//! bitrate, the frame rate, the parity level, whether the sessions carry
//! audio.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// How much parity the host sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityMode {
    /// Follow the measured loss.
    Auto,
    /// No parity at all.
    None,
    /// Two parity slices per block of eight data slices.
    Light,
    /// Three parity slices per block of twelve data slices.
    Heavy,
}

impl ParityMode {
    /// The name the control plane uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Light => "light",
            Self::Heavy => "heavy",
        }
    }

    /// The mode a name stands for.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "none" => Some(Self::None),
            "light" => Some(Self::Light),
            "heavy" => Some(Self::Heavy),
            _ => None,
        }
    }

    /// The code the setting stores.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::None => 1,
            Self::Light => 2,
            Self::Heavy => 3,
        }
    }

    /// The mode a code stands for.
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Auto),
            1 => Some(Self::None),
            2 => Some(Self::Light),
            3 => Some(Self::Heavy),
            _ => None,
        }
    }
}

/// The picture the host serves.
///
/// A client that cannot keep up asks for a smaller one, and the host scales
/// the capture down to it. Scaling is not an extra pass: the colour
/// conversion reads the source with a stride, so a smaller picture costs
/// less work than a big one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality {
    /// The width of the picture.
    pub width: u32,
    /// The height of the picture.
    pub height: u32,
    /// The frame rate.
    pub fps: u32,
}

impl Quality {
    /// The quality the host starts with.
    #[must_use]
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self { width, height, fps }
    }
}

/// The settings of a running host.
#[derive(Debug)]
pub struct HostSettings {
    /// The quantiser, when the rate control is a constant quantiser.
    pub qp: AtomicU32,
    /// The target bitrate in bits per second, when the rate control is a
    /// constant bitrate.
    pub bitrate_bps: AtomicU32,
    /// The stream rate in frames per second.
    pub fps: AtomicU32,
    /// The parity mode.
    pub parity: AtomicU8,
    /// Whether new sessions carry audio.
    pub audio: AtomicBool,
    /// Whether the bandwidth policy may move the bitrate on its own.
    ///
    /// With it off the operator's bitrate is the bitrate, whatever the link
    /// does.
    pub adaptive_bitrate: AtomicBool,
    /// Whether a keyframe was asked for.
    ///
    /// It is shared, because the control plane asks for a keyframe from
    /// another thread and the encode stage takes the request.
    pub keyframe_requested: Arc<AtomicBool>,
    /// The rate control the encoder runs with: 0 for a constant quantiser,
    /// 1 for a constant bitrate.
    pub rate_control_mode: AtomicU8,
    /// The picture the host serves, which a client may ask to be smaller.
    ///
    /// It is behind a lock because it changes three numbers at once, and
    /// because the capture stage reads it once per frame.
    pub quality: Mutex<Quality>,
    /// The biggest picture the host serves.
    ///
    /// The operator sets it when they ask for a picture. A client that asks
    /// for a bigger one is refused: the operator's choice is the limit, and
    /// the client's own ladder is for going down, not up.
    pub quality_ceiling: Mutex<Quality>,
    /// Whether the operator holds the picture. A held picture does not follow the client's
    /// ladder: requests for another size are ignored and the client is told the held size.
    pub quality_held: AtomicBool,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            qp: AtomicU32::new(22),
            bitrate_bps: AtomicU32::new(0),
            fps: AtomicU32::new(30),
            parity: AtomicU8::new(ParityMode::Auto.code()),
            audio: AtomicBool::new(false),
            adaptive_bitrate: AtomicBool::new(true),
            keyframe_requested: Arc::new(AtomicBool::new(false)),
            rate_control_mode: AtomicU8::new(0),
            quality: Mutex::new(Quality::new(1920, 1080, 30)),
            quality_ceiling: Mutex::new(Quality::new(1920, 1080, 30)),
            quality_held: AtomicBool::new(false),
        }
    }
}

impl HostSettings {
    /// The quantiser.
    #[must_use]
    pub fn qp(&self) -> u32 {
        self.qp.load(Ordering::Relaxed)
    }

    /// Set the quantiser.
    pub fn set_qp(&self, qp: u32) {
        self.qp.store(qp.min(51), Ordering::Relaxed);
    }

    /// The target bitrate in bits per second.
    #[must_use]
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps.load(Ordering::Relaxed)
    }

    /// Set the target bitrate in bits per second.
    pub fn set_bitrate_bps(&self, bps: u32) {
        self.bitrate_bps.store(bps, Ordering::Relaxed);
    }

    /// The stream rate in frames per second.
    #[must_use]
    pub fn fps(&self) -> u32 {
        self.fps.load(Ordering::Relaxed).max(1)
    }

    /// Set the stream rate in frames per second.
    pub fn set_fps(&self, fps: u32) {
        self.fps.store(fps.clamp(1, 240), Ordering::Relaxed);
    }

    /// The parity mode.
    #[must_use]
    pub fn parity(&self) -> ParityMode {
        ParityMode::from_code(self.parity.load(Ordering::Relaxed)).unwrap_or(ParityMode::Auto)
    }

    /// Set the parity mode.
    pub fn set_parity(&self, mode: ParityMode) {
        self.parity.store(mode.code(), Ordering::Relaxed);
    }

    /// Whether new sessions carry audio.
    #[must_use]
    pub fn audio(&self) -> bool {
        self.audio.load(Ordering::Relaxed)
    }

    /// Set whether new sessions carry audio.
    pub fn set_audio(&self, audio: bool) {
        self.audio.store(audio, Ordering::Relaxed);
    }

    /// Whether the bandwidth policy may move the bitrate on its own.
    #[must_use]
    pub fn adaptive_bitrate(&self) -> bool {
        self.adaptive_bitrate.load(Ordering::Relaxed)
    }

    /// Set whether the bandwidth policy may move the bitrate on its own.
    pub fn set_adaptive_bitrate(&self, adaptive: bool) {
        self.adaptive_bitrate.store(adaptive, Ordering::Relaxed);
    }

    /// Ask for a keyframe.
    pub fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }

    /// Take the keyframe request, if there is one.
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_requested.swap(false, Ordering::Relaxed)
    }

    /// Whether the encoder runs with a constant quantiser.
    #[must_use]
    pub fn constant_quantiser(&self) -> bool {
        self.rate_control_mode.load(Ordering::Relaxed) == 0
    }

    /// Set whether the encoder runs with a constant quantiser.
    pub fn set_constant_quantiser(&self, cqp: bool) {
        self.rate_control_mode
            .store(u8::from(!cqp), Ordering::Relaxed);
    }

    /// The picture the host serves.
    #[must_use]
    pub fn quality(&self) -> Quality {
        self.quality
            .lock()
            .map_or(Quality::new(1920, 1080, 30), |q| *q)
    }

    /// Set the picture the host serves.
    pub fn set_quality(&self, quality: Quality) {
        if let Ok(mut slot) = self.quality.lock() {
            *slot = quality;
        }
    }

    /// The biggest picture the host serves.
    #[must_use]
    pub fn quality_ceiling(&self) -> Quality {
        self.quality_ceiling
            .lock()
            .map_or(Quality::new(1920, 1080, 30), |q| *q)
    }

    /// Set the biggest picture the host serves.
    pub fn set_quality_ceiling(&self, quality: Quality) {
        if let Ok(mut slot) = self.quality_ceiling.lock() {
            *slot = quality;
        }
    }

    /// The picture the host serves, never bigger than the ceiling.
    #[must_use]
    pub fn quality_within_ceiling(&self, quality: Quality) -> Quality {
        let ceiling = self.quality_ceiling();
        Quality::new(
            quality.width.min(ceiling.width),
            quality.height.min(ceiling.height),
            quality.fps,
        )
    }

    /// Whether the operator holds the picture: client requests for another size are ignored.
    #[must_use]
    pub fn quality_held(&self) -> bool {
        self.quality_held.load(Ordering::Relaxed)
    }

    /// Set whether the operator holds the picture.
    pub fn set_quality_held(&self, held: bool) {
        self.quality_held.store(held, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parity_modes_round_trip() {
        for mode in [
            ParityMode::Auto,
            ParityMode::None,
            ParityMode::Light,
            ParityMode::Heavy,
        ] {
            assert_eq!(ParityMode::parse(mode.name()), Some(mode));
            assert_eq!(ParityMode::from_code(mode.code()), Some(mode));
        }
        assert_eq!(ParityMode::parse("nope"), None);
        assert_eq!(ParityMode::from_code(9), None);
    }

    #[test]
    fn the_settings_hold_what_they_are_given() {
        let settings = HostSettings::default();
        assert_eq!(settings.qp(), 22);
        settings.set_qp(30);
        assert_eq!(settings.qp(), 30);
        // The quantiser cannot go past the codec's range.
        settings.set_qp(99);
        assert_eq!(settings.qp(), 51);
        settings.set_fps(0);
        assert_eq!(settings.fps(), 1);
        settings.set_fps(60);
        assert_eq!(settings.fps(), 60);
        settings.set_parity(ParityMode::Heavy);
        assert_eq!(settings.parity(), ParityMode::Heavy);
    }

    #[test]
    fn a_keyframe_is_asked_for_once() {
        let settings = HostSettings::default();
        assert!(!settings.take_keyframe_request());
        settings.request_keyframe();
        assert!(settings.take_keyframe_request());
        assert!(!settings.take_keyframe_request());
    }
}
