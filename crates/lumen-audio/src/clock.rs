//! Mapping the host's capture timestamps onto the client's clock.
//!
//! The host timestamps the frames it captures on its own monotonic clock and
//! the client timestamps what it presents on its own monotonic clock. The two
//! clocks are unrelated, so before the client can place an audio frame on the
//! video clock it has to know how far the two clocks are apart.
//!
//! The offset is measured the way NTP measures it: the host sends a ping at
//! `t0`, the client answers at `t1` on its own clock, and the host receives
//! the answer at `t2`. The client's clock at the midpoint of the exchange is
//! `(t0 + t2) / 2` on the host's clock, so the offset is
//! `t1 - (t0 + t2) / 2`.
//!
//! This mirrors `estimate_offset` in `crates/lumen-host-service/src/stages.rs`.
//! The audio crate must not depend on the host service, so the calculation
//! lives here as well. Keep the two in step.

/// Estimate the offset between the host clock and the client clock.
///
/// The host sent a ping at `t0_host_us` on its own clock, the client answered
/// at `t1_client_us` on its clock, and the host received the answer at
/// `t2_host_us`. The client's clock at the midpoint of the exchange is
/// `(t0 + t2) / 2` on the host's clock, so the offset is
/// `t1 - (t0 + t2) / 2`.
///
/// The estimate assumes the two directions take the same time. On a LAN the
/// error is well under a millisecond.
#[must_use]
pub fn estimate_offset(t0_host_us: u64, t1_client_us: u64, t2_host_us: u64) -> i64 {
    let midpoint = i64::try_from(t0_host_us)
        .unwrap_or(i64::MAX)
        .saturating_add(i64::try_from(t2_host_us).unwrap_or(i64::MAX))
        / 2;
    i64::try_from(t1_client_us)
        .unwrap_or(i64::MAX)
        .saturating_sub(midpoint)
}

/// The offset between the host's capture clock and the client's clock.
///
/// The offset is added to a host timestamp to get the client timestamp: the
/// client clock reads `host clock + offset`. A positive offset means the
/// client clock runs ahead of the host clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockOffset {
    offset_us: i64,
    measured: bool,
}

impl ClockOffset {
    /// An offset that has been measured.
    #[must_use]
    pub const fn new(offset_us: i64) -> Self {
        Self {
            offset_us,
            measured: true,
        }
    }

    /// An offset that has not been measured yet.
    ///
    /// A client that has not exchanged a ping with the host cannot map the
    /// host's timestamps onto its own clock, so it must not use this.
    #[must_use]
    pub const fn unmeasured() -> Self {
        Self {
            offset_us: 0,
            measured: false,
        }
    }

    /// Estimate the offset from one ping exchange.
    ///
    /// See [`estimate_offset`].
    #[must_use]
    pub fn estimate(t0_host_us: u64, t1_client_us: u64, t2_host_us: u64) -> Self {
        Self {
            offset_us: estimate_offset(t0_host_us, t1_client_us, t2_host_us),
            measured: true,
        }
    }

    /// The offset in microseconds: client clock = host clock + offset.
    #[must_use]
    pub const fn offset_us(&self) -> i64 {
        self.offset_us
    }

    /// Whether an offset has been measured.
    #[must_use]
    pub const fn is_measured(&self) -> bool {
        self.measured
    }

    /// Map a host timestamp onto the client clock.
    #[must_use]
    pub fn to_client(&self, host_ts_us: u64) -> u64 {
        if self.offset_us >= 0 {
            host_ts_us.saturating_add(self.offset_us.unsigned_abs())
        } else {
            host_ts_us.saturating_sub(self.offset_us.unsigned_abs())
        }
    }

    /// Map a client timestamp onto the host clock.
    #[must_use]
    pub fn to_host(&self, client_ts_us: u64) -> u64 {
        if self.offset_us >= 0 {
            client_ts_us.saturating_sub(self.offset_us.unsigned_abs())
        } else {
            client_ts_us.saturating_add(self.offset_us.unsigned_abs())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offset_of_identical_clocks_is_zero() {
        // The client answered at the midpoint of the host's exchange, so the
        // two clocks agree.
        assert_eq!(estimate_offset(1_000_000, 1_000_500, 1_001_000), 0);
        // The client clock runs 10 ms ahead of the host clock.
        assert_eq!(estimate_offset(1_000_000, 1_010_500, 1_001_000), 10_000);
        // The client clock runs 10 ms behind the host clock.
        assert_eq!(estimate_offset(1_000_000, 990_500, 1_001_000), -10_000);
    }

    #[test]
    fn the_offset_maps_both_ways() {
        let offset = ClockOffset::estimate(1_000_000, 1_010_500, 1_001_000);
        assert!(offset.is_measured());
        assert_eq!(offset.offset_us(), 10_000);
        assert_eq!(offset.to_client(1_000_000), 1_010_000);
        assert_eq!(offset.to_host(1_010_000), 1_000_000);
    }

    #[test]
    fn a_negative_offset_maps_both_ways() {
        let offset = ClockOffset::new(-10_000);
        assert_eq!(offset.to_client(1_000_000), 990_000);
        assert_eq!(offset.to_host(990_000), 1_000_000);
    }

    #[test]
    fn an_unmeasured_offset_is_not_measured() {
        let offset = ClockOffset::unmeasured();
        assert!(!offset.is_measured());
        assert_eq!(offset.offset_us(), 0);
        // The default is unmeasured too, so a forgotten estimate is caught.
        assert!(!ClockOffset::default().is_measured());
    }

    #[test]
    fn the_mapping_saturates_instead_of_wrapping() {
        // The sum does not fit in a u64, so it saturates at the maximum.
        assert_eq!(ClockOffset::new(i64::MAX).to_client(u64::MAX), u64::MAX);
        // The difference would be negative, so it saturates at zero.
        assert_eq!(ClockOffset::new(i64::MIN).to_client(10), 0);
    }
}
