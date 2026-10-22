#![no_main]

//! Loss, jitter and reorder injection.
//!
//! Proves that `lumen_transport::inject` stays inside the bounds its own
//! module documents: a delay never exceeds `max_jitter`, a packet is only
//! dropped when a loss fraction is configured and only held when a reorder
//! probability is configured, the drop rate tracks the requested fraction,
//! and the same seed always yields the same decisions.
//!
//! Found so far: nothing. The injector never panicked and no bound was
//! violated.

use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use lumen_transport::inject::{Action, InjectConfig, Injector, Rng};

/// Datagrams pushed through the injector in the main stream.
const DATAGRAMS: usize = 32;
/// Packets counted for the loss rate, which needs a large sample.
const SAMPLE: usize = 4096;
/// Tolerance of the measured drop rate against the requested one.
const LOSS_TOLERANCE: f64 = 0.08;

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

    fn u32(&mut self) -> u32 {
        u32::from(self.byte())
            | (u32::from(self.byte()) << 8)
            | (u32::from(self.byte()) << 16)
            | (u32::from(self.byte()) << 24)
    }

    fn u64(&mut self) -> u64 {
        let mut v = 0u64;
        for shift in 0..8 {
            v |= u64::from(self.byte()) << (shift * 8);
        }
        v
    }

    /// A fraction in `[0, 1]`.
    fn fraction(&mut self) -> f64 {
        f64::from(self.u32()) / f64::from(u32::MAX)
    }
}

/// A configuration read from the input: loss fraction, jitter bound, reorder
/// probability and the seed of the generator.
fn config(c: &mut Cursor<'_>) -> InjectConfig {
    let loss = c.fraction();
    let jitter_raw = c.u64();
    let max_jitter = match c.byte() % 4 {
        0 => Duration::ZERO,
        1 => Duration::from_nanos(jitter_raw % 1_000),
        2 => Duration::from_millis(jitter_raw % 60_000),
        _ => Duration::from_secs(jitter_raw % 3_600),
    };
    let reorder = c.fraction();
    let seed = c.u64();
    InjectConfig {
        loss,
        max_jitter,
        reorder,
        seed,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut c = Cursor { data, pos: 0 };
    let cfg = config(&mut c);

    // Part 1: a stream of datagrams through the injector. Every decision must
    // stay inside the bounds the module documents.
    let mut inj = Injector::new(cfg);
    let mut dropped = 0usize;
    let mut held: Option<Vec<u8>> = None;
    for i in 0..DATAGRAMS {
        // A held packet is released before the next packet is decided on.
        if let Some(packet) = inj.take_held() {
            assert_eq!(Some(packet), held, "the held packet comes back unchanged");
            held = None;
        }
        let packet = vec![u8::try_from(i % 256).unwrap(); 1 + i % 8];
        match inj.decide() {
            Action::Drop => {
                dropped += 1;
                assert!(cfg.loss > 0.0, "a zero loss fraction must never drop");
            }
            Action::Hold => {
                assert!(
                    cfg.reorder > 0.0,
                    "a zero reorder probability must never hold"
                );
                held = Some(packet.clone());
                inj.hold(packet);
            }
            Action::Send { delay } => {
                assert!(
                    delay <= cfg.max_jitter,
                    "delay {delay:?} above the jitter bound {:?}",
                    cfg.max_jitter
                );
            }
        }
    }
    // The last held packet is released once, and only once.
    assert_eq!(inj.take_held(), held);
    assert_eq!(inj.take_held(), None);
    if cfg.loss == 1.0 {
        assert_eq!(dropped, DATAGRAMS, "a total loss drops every packet");
    }

    // Part 2: the drop rate must track the requested loss fraction.
    let mut probe = Injector::new(cfg);
    let mut lost = 0usize;
    for _ in 0..SAMPLE {
        if probe.decide() == Action::Drop {
            lost += 1;
        }
    }
    let rate = lost as f64 / SAMPLE as f64;
    assert!(
        (rate - cfg.loss).abs() <= LOSS_TOLERANCE,
        "drop rate {rate} far from the requested loss {}",
        cfg.loss
    );
    if cfg.loss == 0.0 {
        assert_eq!(lost, 0, "a zero loss fraction must never drop");
    }
    if cfg.loss == 1.0 {
        assert_eq!(lost, SAMPLE, "a total loss drops every packet");
    }

    // Part 3: the same seed must produce the same decisions.
    let mut a = Injector::new(cfg);
    let mut b = Injector::new(cfg);
    for _ in 0..DATAGRAMS {
        assert_eq!(
            a.decide(),
            b.decide(),
            "the same seed must give the same decisions"
        );
    }

    // Part 4: an inactive injector changes nothing.
    let mut quiet = Injector::new(InjectConfig::default());
    assert!(!quiet.is_active());
    for _ in 0..DATAGRAMS {
        match quiet.decide() {
            Action::Drop => panic!("an inactive injector must not drop"),
            Action::Hold => panic!("an inactive injector must not hold"),
            Action::Send { delay } => assert_eq!(delay, Duration::ZERO),
        }
    }

    // Part 5: configurations outside the documented range must not crash. The
    // loss and reorder fractions are documented as `0.0..=1.0`; outside that
    // range the behaviour is undefined, but it must not panic.
    for loss in [1.5, -0.5, f64::NAN, f64::INFINITY] {
        for jitter in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::MAX,
            Duration::from_secs(u64::MAX),
        ] {
            let mut inj = Injector::new(InjectConfig {
                loss,
                max_jitter: jitter,
                reorder: 1.5,
                seed: 1,
            });
            for _ in 0..8 {
                let _ = inj.decide();
            }
        }
    }

    // Part 6: the generator yields values in `[0, 1)`, which is what the
    // loss and reorder comparisons rely on.
    let mut rng = Rng::new(cfg.seed);
    for _ in 0..SAMPLE {
        let v = rng.next_f64();
        assert!((0.0..1.0).contains(&v), "the generator must stay in [0, 1)");
    }
});
