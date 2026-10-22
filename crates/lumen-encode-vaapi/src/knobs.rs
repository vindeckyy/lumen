//! The environment knobs this crate reads.
//!
//! Every `LUMEN_*` read lives here, so the list of what can change the behaviour of the
//! encoder is in one place.

/// The encoder quality level: `LUMEN_ENC_QUALITY`, or none.
#[must_use]
pub fn encode_quality() -> Option<u32> {
    std::env::var("LUMEN_ENC_QUALITY")
        .ok()
        .and_then(|level| level.parse().ok())
}
