//! Host receiver for input datagrams.
//!
//! One thread owns the socket. Each [`InputReceiver::recv_and_inject`] call takes one datagram,
//! authenticates and decodes it, and hands the event to the injector before it returns. No queue
//! sits between the socket and the injector, so input never waits behind other input. It shares
//! nothing with the video socket either.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use lumen_crypto::PacketCipher;
use lumen_input_wayland::{InputEvent, InputEventKind, now_us};
use lumen_protocol::input::{self, InputAction, InputCodecError, ReplayWindow};

use crate::InputInjector;

/// Receive buffer size. Datagrams longer than the codec limit are refused, not truncated silently.
const RECV_BUF_LEN: usize = 2048;

/// What happened to one datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Authenticated, new, and injected.
    Injected {
        sequence: u64,
        /// Client stamp, `CLOCK_MONOTONIC` microseconds at send time.
        client_ts_us: u64,
        /// `CLOCK_MONOTONIC` microseconds taken right after the injector returned.
        injected_us: u64,
    },
    /// Authenticated and new, but the injector refused it.
    InjectFailed { sequence: u64, error: String },
    /// Dropped before injection.
    Dropped(DropReason),
}

/// Why a datagram was dropped before injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The codec refused it, including authentication failure.
    Codec(InputCodecError),
    /// It authenticated but was already seen, or is older than the replay window.
    Replay,
    /// A gamepad action: this receiver drives an [`InputInjector`], which takes keys and the
    /// pointer only. The host routes gamepads to [`crate::Gamepads`].
    Gamepad,
}

/// Running counters for one receiver.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverStats {
    /// Datagrams read from the socket.
    pub datagrams: u64,
    /// Events injected successfully.
    pub injected: u64,
    /// Authentic, new events the injector refused.
    pub inject_failures: u64,
    /// Datagrams whose tag did not verify.
    pub authentication_failures: u64,
    /// Authentic-looking datagrams refused for length, kind or field reasons.
    pub malformed: u64,
    /// Authentic datagrams refused as replays.
    pub replays: u64,
    /// Highest sequence accepted so far.
    pub highest_sequence: Option<u64>,
}

impl ReceiverStats {
    /// Distinct authenticated events: injected plus inject failures.
    #[must_use]
    pub const fn accepted(&self) -> u64 {
        self.injected + self.inject_failures
    }
}

/// Reads sealed input datagrams and injects them.
#[derive(Debug)]
pub struct InputReceiver {
    socket: UdpSocket,
    cipher: PacketCipher,
    replay: ReplayWindow,
    stats: ReceiverStats,
    buf: Vec<u8>,
}

impl InputReceiver {
    /// Binds `addr` (for example `0.0.0.0:24803`) and opens datagrams with `cipher`, the
    /// client-to-host media cipher for this session.
    ///
    /// # Errors
    /// Returns the bind error from the operating system.
    pub fn bind(addr: SocketAddr, cipher: PacketCipher) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(addr)?,
            cipher,
            replay: ReplayWindow::default(),
            stats: ReceiverStats::default(),
            buf: vec![0; RECV_BUF_LEN],
        })
    }

    /// The bound address.
    ///
    /// # Errors
    /// Returns the error from `getsockname`.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Sets the blocking timeout for [`Self::recv_and_inject`]. `None` blocks forever.
    ///
    /// # Errors
    /// Returns the error from `setsockopt`.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }

    /// A copy of the running counters.
    #[must_use]
    pub const fn stats(&self) -> ReceiverStats {
        self.stats
    }

    /// Reads one datagram, opens it, checks the replay window and injects the event.
    ///
    /// Injection happens before this returns, so the caller can timestamp `injected_us` against
    /// the client stamp.
    ///
    /// # Errors
    /// Returns the socket error. A timeout comes back as `WouldBlock` or `TimedOut`, and the
    /// caller decides whether to keep waiting. Bad datagrams are not errors; they come back as
    /// [`Outcome::Dropped`].
    pub fn recv_and_inject(&mut self, injector: &mut dyn InputInjector) -> io::Result<Outcome> {
        let (len, _from) = self.socket.recv_from(&mut self.buf)?;
        self.stats.datagrams += 1;
        let envelope = match input::open_packet(&self.cipher, &self.buf[..len]) {
            Ok(envelope) => envelope,
            Err(err) => {
                if matches!(err, InputCodecError::Authentication) {
                    self.stats.authentication_failures += 1;
                } else {
                    self.stats.malformed += 1;
                }
                return Ok(Outcome::Dropped(DropReason::Codec(err)));
            }
        };
        if !self.replay.accept(envelope.sequence) {
            self.stats.replays += 1;
            return Ok(Outcome::Dropped(DropReason::Replay));
        }
        self.stats.highest_sequence = Some(
            self.stats
                .highest_sequence
                .map_or(envelope.sequence, |h| h.max(envelope.sequence)),
        );
        let Some(event) = to_event(envelope.action, now_us()) else {
            return Ok(Outcome::Dropped(DropReason::Gamepad));
        };
        match injector.inject(&event) {
            Ok(()) => {
                self.stats.injected += 1;
                Ok(Outcome::Injected {
                    sequence: envelope.sequence,
                    client_ts_us: envelope.client_ts_us,
                    injected_us: now_us(),
                })
            }
            Err(error) => {
                self.stats.inject_failures += 1;
                Ok(Outcome::InjectFailed {
                    sequence: envelope.sequence,
                    error,
                })
            }
        }
    }
}

/// Maps a wire action back onto the local event model. Absolute motion keeps finite `x` and `y`,
/// and relative motion uses NaN there, as documented on [`InputEventKind::Motion`]. Gamepad
/// actions have no event here.
fn to_event(action: InputAction, received_us: u64) -> Option<InputEvent> {
    let kind = match action {
        InputAction::Key { keycode, pressed } => InputEventKind::Key { keycode, pressed },
        InputAction::MotionRelative { dx, dy } => InputEventKind::Motion {
            dx,
            dy,
            x: f64::NAN,
            y: f64::NAN,
        },
        InputAction::MotionAbsolute { x, y } => InputEventKind::Motion {
            dx: 0.0,
            dy: 0.0,
            x,
            y,
        },
        InputAction::Button { button, pressed } => InputEventKind::Button { button, pressed },
        InputAction::Scroll { dx, dy } => InputEventKind::Scroll { dx, dy },
        InputAction::Gamepad { .. }
        | InputAction::GamepadRemoved { .. }
        | InputAction::Touch { .. }
        | InputAction::Pen { .. } => return None,
    };
    Some(InputEvent { kind, received_us })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_crypto::AeadKind;
    use lumen_input_wayland::InputSender;
    use std::net::{Ipv4Addr, SocketAddrV4};

    /// Records injected events, and refuses everything when `fail` is set.
    #[derive(Default)]
    struct Recorder {
        events: Vec<InputEvent>,
        fail: bool,
    }

    impl InputInjector for Recorder {
        fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
            if self.fail {
                return Err("refused".to_owned());
            }
            self.events.push(*event);
            Ok(())
        }

        fn inject_text(&mut self, _text: &str) -> Result<(), String> {
            Err("not used".to_owned())
        }
    }

    const KEY: [u8; 32] = [9u8; 32];
    const SESSION: u32 = 3;

    fn cipher(key: [u8; 32]) -> PacketCipher {
        PacketCipher::new(AeadKind::Aes256Gcm, key, SESSION)
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    fn receiver(key: [u8; 32]) -> (InputReceiver, SocketAddr) {
        let rx = InputReceiver::bind(loopback(0), cipher(key)).expect("bind receiver");
        rx.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let addr = rx.local_addr().expect("addr");
        (rx, addr)
    }

    #[test]
    fn wire_actions_map_to_local_events() {
        let rel = to_event(InputAction::MotionRelative { dx: 2.0, dy: -1.0 }, 5).expect("an event");
        match rel.kind {
            InputEventKind::Motion { dx, dy, x, y } => {
                assert_eq!((dx, dy), (2.0, -1.0));
                assert!(x.is_nan() && y.is_nan());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(rel.received_us, 5);

        let abs = to_event(InputAction::MotionAbsolute { x: 7.0, y: 8.0 }, 0).expect("an event");
        assert_eq!(
            abs.kind,
            InputEventKind::Motion {
                dx: 0.0,
                dy: 0.0,
                x: 7.0,
                y: 8.0
            }
        );
        assert!(to_event(InputAction::GamepadRemoved { pad: 0 }, 0).is_none());
    }

    #[test]
    fn sender_to_receiver_injects_every_event_once() {
        let (mut rx, addr) = receiver(KEY);
        let mut tx = InputSender::bind(loopback(0), addr, cipher(KEY)).expect("bind sender");
        let mut injector = Recorder::default();

        let events = [
            InputEventKind::Key {
                keycode: 30,
                pressed: true,
            },
            InputEventKind::Motion {
                dx: 0.0,
                dy: 0.0,
                x: 100.0,
                y: 200.0,
            },
            InputEventKind::Motion {
                dx: 3.0,
                dy: -4.0,
                x: f64::NAN,
                y: f64::NAN,
            },
            InputEventKind::Button {
                button: 0x110,
                pressed: true,
            },
            InputEventKind::Scroll { dx: 0.0, dy: 15.0 },
        ];
        for kind in events {
            tx.send(&InputEvent {
                kind,
                received_us: 0,
            })
            .expect("sends");
            let outcome = rx.recv_and_inject(&mut injector).expect("receives");
            assert!(
                matches!(outcome, Outcome::Injected { .. }),
                "got {outcome:?}"
            );
        }

        assert_eq!(injector.events.len(), events.len());
        // Relative motion carries NaN coordinates, and NaN != NaN, so compare the Debug form.
        for (got, want) in injector.events.iter().zip(events) {
            assert_eq!(format!("{:?}", got.kind), format!("{want:?}"));
        }
        let stats = rx.stats();
        assert_eq!(stats.injected, events.len() as u64);
        assert_eq!(stats.highest_sequence, Some(events.len() as u64 - 1));
        assert_eq!(stats.accepted(), events.len() as u64);
    }

    #[test]
    fn replayed_datagram_is_dropped_and_counted() {
        let (mut rx, addr) = receiver(KEY);
        let sock = UdpSocket::bind(loopback(0)).expect("bind raw sender");
        let mut sealer = cipher(KEY);
        let datagram = input::seal_packet(
            &mut sealer,
            1,
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
        )
        .expect("seals");
        let mut injector = Recorder::default();

        sock.send_to(&datagram, addr).expect("send");
        assert!(matches!(
            rx.recv_and_inject(&mut injector).expect("receives"),
            Outcome::Injected { .. }
        ));
        sock.send_to(&datagram, addr).expect("send again");
        assert_eq!(
            rx.recv_and_inject(&mut injector).expect("receives"),
            Outcome::Dropped(DropReason::Replay)
        );
        assert_eq!(injector.events.len(), 1, "the replay is not injected");
        assert_eq!(rx.stats().replays, 1);
    }

    #[test]
    fn forged_datagram_is_dropped_and_counted() {
        let (mut rx, addr) = receiver(KEY);
        let sock = UdpSocket::bind(loopback(0)).expect("bind raw sender");
        // Sealed with a different key, so the tag cannot verify.
        let mut attacker = cipher([0xee; 32]);
        let forged = input::seal_packet(
            &mut attacker,
            1,
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
        )
        .expect("seals");
        let mut injector = Recorder::default();

        sock.send_to(&forged, addr).expect("send");
        assert_eq!(
            rx.recv_and_inject(&mut injector).expect("receives"),
            Outcome::Dropped(DropReason::Codec(InputCodecError::Authentication))
        );
        let stats = rx.stats();
        assert_eq!(stats.authentication_failures, 1);
        assert_eq!(stats.injected, 0);
        let none: Vec<InputEvent> = Vec::new();
        assert_eq!(injector.events, none);
    }

    #[test]
    fn refused_injection_is_reported_not_silently_lost() {
        let (mut rx, addr) = receiver(KEY);
        let mut tx = InputSender::bind(loopback(0), addr, cipher(KEY)).expect("bind sender");
        let mut injector = Recorder {
            fail: true,
            ..Recorder::default()
        };
        tx.send(&InputEvent {
            kind: InputEventKind::Key {
                keycode: 30,
                pressed: true,
            },
            received_us: 0,
        })
        .expect("sends");
        assert_eq!(
            rx.recv_and_inject(&mut injector).expect("receives"),
            Outcome::InjectFailed {
                sequence: 0,
                error: "refused".to_owned()
            }
        );
        assert_eq!(rx.stats().inject_failures, 1);
    }
}
