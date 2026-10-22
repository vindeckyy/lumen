//! The recovery gate: what the client shows while the reference chain is broken.
//!
//! A picture whose reference is missing cannot be decoded: the driver would conceal the loss,
//! and a concealed picture is a grey smear that is worse than a frozen one. So the client stops
//! decoding, asks the host for a keyframe, and keeps the last good picture on screen until the
//! keyframe arrives.
//!
//! punktfunk solves the same problem with a bitstream parser that looks for a recovery point
//! (`punktfunk-core/src/reanchor.rs`). Lumen's decoder refuses a picture whose reference is
//! missing (`crates/lumen-decode-vaapi/src/dpb.rs`), so the gate is a flag rather than a
//! parser.

/// Whether the client may decode and present a picture.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryGate {
    /// A keyframe is needed before the next picture may be shown.
    needs_keyframe: bool,
}

impl RecoveryGate {
    /// A gate that is open: the stream is healthy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the picture may be decoded. A picture before the keyframe is refused, so the
    /// reference chain is never broken further.
    #[must_use]
    pub fn accepts(self, is_keyframe: bool) -> bool {
        !self.needs_keyframe || is_keyframe
    }

    /// Whether the gate is waiting for a keyframe.
    #[must_use]
    #[cfg_attr(not(test), expect(dead_code, reason = "used by the tests"))]
    pub fn needs_keyframe(self) -> bool {
        self.needs_keyframe
    }

    /// A picture decoded. A keyframe closes the gate.
    pub fn on_decoded(&mut self, is_keyframe: bool) {
        if is_keyframe {
            self.needs_keyframe = false;
        }
    }

    /// A picture failed to decode: the gate closes, and the caller asks the host for a
    /// keyframe. Returns whether the request should be sent, so it is sent once and not on
    /// every frame that follows.
    pub fn on_decode_error(&mut self) -> bool {
        let first = !self.needs_keyframe;
        self.needs_keyframe = true;
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_stream_passes_every_picture() {
        let mut gate = RecoveryGate::new();
        assert!(gate.accepts(false));
        gate.on_decoded(false);
        assert!(gate.accepts(false));
    }

    #[test]
    fn a_failed_picture_closes_the_gate_and_asks_once() {
        let mut gate = RecoveryGate::new();
        assert!(
            gate.on_decode_error(),
            "the first failure asks for a keyframe"
        );
        assert!(
            !gate.on_decode_error(),
            "the next failure does not ask again"
        );
        assert!(gate.needs_keyframe());
        // A picture before the keyframe is refused: the chain is broken already.
        assert!(!gate.accepts(false));
        assert!(gate.accepts(true), "the keyframe itself is accepted");
    }

    #[test]
    fn a_keyframe_opens_the_gate_again() {
        let mut gate = RecoveryGate::new();
        gate.on_decode_error();
        gate.on_decoded(true);
        assert!(!gate.needs_keyframe());
        assert!(gate.accepts(false));
    }
}
