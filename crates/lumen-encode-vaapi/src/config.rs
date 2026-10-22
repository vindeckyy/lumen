//! Encoder configuration: rate control, codec profile ladder, validation.

use lumen_ffi_va::sys;

/// Default constant-quantiser QP (plan M1.9).
pub const DEFAULT_QP: u8 = 22;

/// H.264 `level_idc` written into the sequence parameters. Level 5.2 covers
/// 4K60, so one value serves every size this encoder is asked to handle.
pub const H264_LEVEL_IDC: u8 = 52;

/// Largest frame edge accepted. Keeps macroblock arithmetic inside `u32`.
pub const MAX_EDGE: u32 = 16384;

/// Rate control mode, selected at construction time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Constant quantiser; `qp` (0..=51) applies to every macroblock.
    Cqp {
        /// H.264 quantiser parameter.
        qp: u8,
    },
    /// Constant bitrate.
    Cbr {
        /// Target bitrate in bits per second; must be non-zero.
        bits_per_second: u32,
    },
}

impl Default for RateControl {
    fn default() -> Self {
        Self::Cqp { qp: DEFAULT_QP }
    }
}

impl RateControl {
    /// `VA_RC_*` bit this mode needs in `VAConfigAttribRateControl`.
    #[must_use]
    pub(crate) const fn va_bit(self) -> u32 {
        match self {
            Self::Cqp { .. } => sys::VA_RC_CQP,
            Self::Cbr { .. } => sys::VA_RC_CBR,
        }
    }
}

/// Encoder session parameters. Width and height are fixed for the session;
/// a resolution change means a new encoder (plan M4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Frame width in pixels; must be even.
    pub width: u32,
    /// Frame height in pixels; must be even.
    pub height: u32,
    /// Frame rate used for VUI timing; must be non-zero.
    pub fps: u32,
    /// Rate control mode.
    pub rate_control: RateControl,
}

impl EncoderConfig {
    /// Configuration with CQP rate control at [`DEFAULT_QP`].
    #[must_use]
    pub const fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            rate_control: RateControl::Cqp { qp: DEFAULT_QP },
        }
    }

    /// Same configuration with a different rate control mode.
    #[must_use]
    pub const fn with_rate_control(self, rate_control: RateControl) -> Self {
        Self {
            rate_control,
            ..self
        }
    }

    /// Check the parameters that do not depend on the driver.
    ///
    /// # Errors
    /// Returns a message naming the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        let bad_size = self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || self.width > MAX_EDGE
            || self.height > MAX_EDGE;
        if bad_size {
            return Err(format!(
                "frame size {}x{} must be even, non-zero and at most {MAX_EDGE} per edge for 4:2:0 NV12",
                self.width, self.height
            ));
        }
        if self.fps == 0 {
            return Err("frame rate must be non-zero".into());
        }
        match self.rate_control {
            RateControl::Cqp { qp } if qp > 51 => {
                Err(format!("H.264 CQP qp {qp} out of range 0..=51"))
            }
            RateControl::Cbr { bits_per_second: 0 } => Err("CBR bitrate must be non-zero".into()),
            _ => Ok(()),
        }
    }
}

/// H.264 profiles, in preference order (`High` first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Profile {
    /// `VAProfileH264High`: CABAC and 8x8 transform.
    High,
    /// `VAProfileH264Main`: CABAC, no 8x8 transform.
    Main,
    /// `VAProfileH264ConstrainedBaseline`: CAVLC only.
    ConstrainedBaseline,
}

impl H264Profile {
    /// Fallback ladder: the first profile the driver offers for encode wins.
    pub const LADDER: [Self; 3] = [Self::High, Self::Main, Self::ConstrainedBaseline];

    /// Whether the profile allows CABAC entropy coding.
    #[must_use]
    pub const fn cabac(self) -> bool {
        !matches!(self, Self::ConstrainedBaseline)
    }

    /// Whether the profile allows the 8x8 transform.
    #[must_use]
    pub const fn transform_8x8(self) -> bool {
        matches!(self, Self::High)
    }

    /// The libva profile constant.
    #[must_use]
    pub(crate) const fn va(self) -> sys::VAProfile {
        match self {
            Self::High => sys::VAProfileH264High,
            Self::Main => sys::VAProfileH264Main,
            Self::ConstrainedBaseline => sys::VAProfileH264ConstrainedBaseline,
        }
    }

    /// Human-readable name for error messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::High => "H264High",
            Self::Main => "H264Main",
            Self::ConstrainedBaseline => "H264ConstrainedBaseline",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_cqp_22() {
        let c = EncoderConfig::new(1280, 720, 60);
        assert_eq!(c.rate_control, RateControl::Cqp { qp: 22 });
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(EncoderConfig::new(0, 720, 60).validate().is_err());
        assert!(EncoderConfig::new(1281, 720, 60).validate().is_err());
        assert!(
            EncoderConfig::new(MAX_EDGE + 2, 720, 60)
                .validate()
                .is_err()
        );
        assert!(EncoderConfig::new(1280, 720, 0).validate().is_err());
        let qp52 = EncoderConfig::new(1280, 720, 60).with_rate_control(RateControl::Cqp { qp: 52 });
        assert!(qp52.validate().is_err());
        let cbr0 = EncoderConfig::new(1280, 720, 60)
            .with_rate_control(RateControl::Cbr { bits_per_second: 0 });
        assert!(cbr0.validate().is_err());
    }

    #[test]
    fn profile_ladder_order_and_tools() {
        assert_eq!(H264Profile::LADDER[0], H264Profile::High);
        assert!(H264Profile::High.cabac() && H264Profile::High.transform_8x8());
        assert!(!H264Profile::ConstrainedBaseline.cabac());
        assert!(!H264Profile::Main.transform_8x8());
    }
}
