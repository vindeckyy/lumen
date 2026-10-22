//! Client sender: one input event, one sealed UDP datagram, sent at once.
//!
//! The stamp is taken immediately before sealing. Nothing waits in a queue between the event and
//! the socket, so the stamp-to-send interval is the client's whole software send path.

use std::io;
use std::net::{SocketAddr, UdpSocket};

use lumen_crypto::PacketCipher;
use lumen_protocol::input::{self, InputAction, InputCodecError};

use crate::{InputEvent, InputEventKind, now_us};

/// Why an event was not sent.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// The event has no wire form, or sealing failed.
    #[error(transparent)]
    Encode(#[from] InputCodecError),
    /// The datagram could not be handed to the socket.
    #[error("input send failed: {0}")]
    Io(#[from] io::Error),
}

/// Sends input events to the host, one sealed datagram each.
#[derive(Debug)]
pub struct InputSender {
    socket: UdpSocket,
    peer: SocketAddr,
    cipher: PacketCipher,
}

impl InputSender {
    /// Binds `local` and targets `peer`. `cipher` is the client-to-host media cipher.
    ///
    /// # Errors
    /// Returns the bind error from the operating system.
    pub fn bind(local: SocketAddr, peer: SocketAddr, cipher: PacketCipher) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(local)?,
            peer,
            cipher,
        })
    }

    /// The local address the socket is bound to.
    ///
    /// # Errors
    /// Returns the error from `getsockname`.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Stamps `event` with the current monotonic time, seals it, and sends it with one `send_to`.
    ///
    /// Sequence numbers advance when an event is sealed. If `send_to` then fails, the host sees a
    /// gap in the sequence, which the host counts as a loss.
    ///
    /// # Errors
    /// Returns [`SendError::Encode`] when the event has no wire form, and [`SendError::Io`] when
    /// the send fails.
    pub fn send(&mut self, event: &InputEvent) -> Result<(), SendError> {
        let action = action_from_kind(&event.kind)?;
        let stamp = now_us();
        let datagram = input::seal_packet(&mut self.cipher, stamp, action)?;
        self.socket.send_to(&datagram, self.peer)?;
        Ok(())
    }

    /// Events sealed so far, which is also the next sequence number.
    #[must_use]
    pub fn sealed(&self) -> u64 {
        self.cipher.counter()
    }
}

/// Maps a local event onto the wire. Absolute motion carries finite `x` and `y`. Relative motion
/// carries NaN there, as documented on [`InputEventKind::Motion`].
fn action_from_kind(kind: &InputEventKind) -> Result<InputAction, InputCodecError> {
    Ok(match *kind {
        InputEventKind::Key { keycode, pressed } => InputAction::Key { keycode, pressed },
        InputEventKind::Motion { x, y, .. } if x.is_finite() && y.is_finite() => {
            InputAction::MotionAbsolute { x, y }
        }
        InputEventKind::Motion { dx, dy, x, y } if x.is_nan() && y.is_nan() => {
            InputAction::MotionRelative { dx, dy }
        }
        InputEventKind::Motion { .. } => {
            return Err(InputCodecError::Malformed(
                "motion needs both coordinates finite, or both NaN",
            ));
        }
        InputEventKind::Button { button, pressed } => InputAction::Button { button, pressed },
        InputEventKind::Scroll { dx, dy } => InputAction::Scroll { dx, dy },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_crypto::AeadKind;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::time::Duration;

    fn cipher() -> PacketCipher {
        PacketCipher::new(AeadKind::Aes256Gcm, [9u8; 32], 3)
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    #[test]
    fn absolute_and_relative_motion_map_to_distinct_actions() {
        let abs = action_from_kind(&InputEventKind::Motion {
            dx: 0.0,
            dy: 0.0,
            x: 10.0,
            y: 20.0,
        })
        .expect("maps");
        assert_eq!(abs, InputAction::MotionAbsolute { x: 10.0, y: 20.0 });

        let rel = action_from_kind(&InputEventKind::Motion {
            dx: 1.5,
            dy: -2.0,
            x: f64::NAN,
            y: f64::NAN,
        })
        .expect("maps");
        assert_eq!(rel, InputAction::MotionRelative { dx: 1.5, dy: -2.0 });
    }

    #[test]
    fn half_nan_motion_is_refused() {
        let err = action_from_kind(&InputEventKind::Motion {
            dx: 0.0,
            dy: 0.0,
            x: 1.0,
            y: f64::NAN,
        })
        .unwrap_err();
        assert!(matches!(err, InputCodecError::Malformed(_)));
    }

    #[test]
    fn send_delivers_one_sealed_datagram_per_event() {
        let receiver = UdpSocket::bind(loopback(0)).expect("bind receiver");
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let peer = receiver.local_addr().expect("peer addr");
        let mut sender = InputSender::bind(loopback(0), peer, cipher()).expect("bind sender");

        let event = InputEvent {
            kind: InputEventKind::Key {
                keycode: 30,
                pressed: true,
            },
            received_us: 0,
        };
        sender.send(&event).expect("sends");
        assert_eq!(sender.sealed(), 1);

        let mut buf = [0u8; 256];
        let (len, _) = receiver.recv_from(&mut buf).expect("one datagram");
        let rx = cipher();
        let env = input::open_packet(&rx, &buf[..len]).expect("opens with the same key");
        assert_eq!(env.sequence, 0);
        assert!(env.client_ts_us > 0, "stamped from CLOCK_MONOTONIC");
        assert_eq!(
            env.action,
            InputAction::Key {
                keycode: 30,
                pressed: true
            }
        );

        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        assert!(
            receiver.recv_from(&mut buf).is_err(),
            "exactly one datagram per event"
        );
    }
}
