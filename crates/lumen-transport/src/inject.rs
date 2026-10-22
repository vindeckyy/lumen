//! Loss, jitter and reorder injection.
//!
//! Used by the unit tests and by the CI degradation runs. The system-level
//! runs use `tc netem` instead (see `scripts/netem-run.sh`); this module is
//! the in-process equivalent so CI without root can still exercise the loss
//! paths.

use std::time::Duration;

/// A deterministic pseudo-random generator (xorshift64*).
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Create a generator with the given seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    /// Next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Next value in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Loss, jitter and reorder configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InjectConfig {
    /// Fraction of packets dropped, `0.0..=1.0`.
    pub loss: f64,
    /// Maximum added delay.
    pub max_jitter: Duration,
    /// Probability that a packet is held back to be sent after the next one.
    pub reorder: f64,
    /// Seed for the deterministic generator.
    pub seed: u64,
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            loss: 0.0,
            max_jitter: Duration::ZERO,
            reorder: 0.0,
            seed: 0x5EED,
        }
    }
}

/// Applies loss, jitter and reorder to outgoing packets.
#[derive(Debug)]
pub struct Injector {
    config: InjectConfig,
    rng: Rng,
    /// Packets held back for reordering.
    held: Option<(std::time::Instant, Vec<u8>)>,
}

impl Injector {
    /// Create an injector with the given configuration.
    #[must_use]
    pub fn new(config: InjectConfig) -> Self {
        Self {
            rng: Rng::new(config.seed),
            config,
            held: None,
        }
    }

    /// Whether injection is active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.config.loss > 0.0 || !self.config.max_jitter.is_zero() || self.config.reorder > 0.0
    }

    /// Decide what to do with a packet.
    ///
    /// Returns the action for the caller to perform:
    /// `Drop` means the packet is lost, `Send { delay }` means send it after
    /// the delay, `Hold` means the packet is stored and released by the next
    /// call to `take_held`.
    #[must_use]
    pub fn decide(&mut self) -> Action {
        if self.rng.next_f64() < self.config.loss {
            return Action::Drop;
        }
        if self.config.reorder > 0.0 && self.rng.next_f64() < self.config.reorder {
            return Action::Hold;
        }
        let delay = if self.config.max_jitter.is_zero() {
            Duration::ZERO
        } else {
            let max = self.config.max_jitter.as_secs_f64();
            Duration::from_secs_f64(self.rng.next_f64() * max)
        };
        Action::Send { delay }
    }

    /// Store a packet for later release.
    pub fn hold(&mut self, packet: Vec<u8>) {
        self.held = Some((std::time::Instant::now(), packet));
    }

    /// Take the held packet, if any.
    pub fn take_held(&mut self) -> Option<Vec<u8>> {
        self.held.take().map(|(_, p)| p)
    }
}

/// What the injector decided to do with a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Drop the packet.
    Drop,
    /// Send the packet after the given delay.
    Send {
        /// Delay before sending.
        delay: Duration,
    },
    /// Hold the packet back so a later packet overtakes it.
    Hold,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_injection_by_default() {
        let inj = Injector::new(InjectConfig::default());
        assert!(!inj.is_active());
    }

    #[test]
    fn loss_drops_about_the_requested_fraction() {
        let mut inj = Injector::new(InjectConfig {
            loss: 0.5,
            seed: 42,
            ..Default::default()
        });
        let mut dropped = 0;
        let total = 10_000;
        for _ in 0..total {
            if inj.decide() == Action::Drop {
                dropped += 1;
            }
        }
        let fraction = f64::from(dropped) / f64::from(total);
        assert!(
            (fraction - 0.5).abs() < 0.05,
            "dropped fraction {fraction} not close to 0.5"
        );
    }

    #[test]
    fn jitter_stays_within_bound() {
        let mut inj = Injector::new(InjectConfig {
            max_jitter: Duration::from_millis(10),
            ..Default::default()
        });
        for _ in 0..1000 {
            if let Action::Send { delay } = inj.decide() {
                assert!(delay <= Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }
}
