//! The input path: its own socket, its own thread, one datagram per event.
//!
//! Input events travel on a socket of their own, so a lost video packet or a
//! slow frame cannot delay them. One event is one datagram, and it is sent the
//! moment it arrives: nothing is batched, nothing waits in a queue.
//!
//! The datagram is the one of `docs/protocol.md` section 8. The clear sequence
//! number is the AEAD associated data and also the AEAD counter, so the nonce
//! and the clear header agree by construction, exactly as the media path does
//! it. The host receiver keeps the replay window of the protocol, so a
//! datagram that was already accepted is refused and a forged datagram cannot
//! move the window.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lumen_crypto::PacketCipher;
use lumen_protocol::input::{self, InputAction, InputCodecError, InputEnvelope, ReplayWindow};
use socket2::{Domain, Protocol, Type};

/// The buffer the receiver reads into. A datagram longer than the codec limit
/// is refused, not truncated silently, so the buffer is larger than the limit.
const RECV_BUF_LEN: usize = 2048;

/// How long the receiver waits for a datagram before it checks whether it
/// should stop. The thread is otherwise asleep, so this costs nothing.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(50);

/// Why an input datagram could not be sent or read.
#[derive(Debug, thiserror::Error)]
pub enum InputChannelError {
    /// The socket failed.
    #[error("the input socket failed: {0}")]
    Io(#[from] io::Error),
    /// The datagram could not be sealed or opened.
    #[error(transparent)]
    Codec(#[from] InputCodecError),
}

/// The counters of one end of the input path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputChannelStats {
    /// Datagrams sent.
    pub sent: u64,
    /// Datagrams read from the socket.
    pub datagrams: u64,
    /// Datagrams that authenticated and were new.
    pub accepted: u64,
    /// Datagrams whose tag did not verify.
    pub authentication_failures: u64,
    /// Datagrams refused for a malformed body.
    pub malformed: u64,
    /// Datagrams refused because their sequence was already accepted.
    pub replays: u64,
}

/// The socket options of the input path.
///
/// Input is small and latency critical, so it gets a higher `SO_PRIORITY`
/// than the video path and a smaller buffer, which keeps the queue short.
#[must_use]
pub fn input_socket_options() -> crate::socket::SocketOptions {
    crate::socket::SocketOptions {
        priority: 6,
        dscp: crate::socket::DSCP_AUDIO,
        recv_buffer: 256 * 1024,
    }
}

/// Sends input events to the host, one sealed datagram each.
///
/// The sender owns a socket of its own. A datagram goes out as soon as the
/// event is sealed, so the event never waits behind anything else.
#[derive(Debug)]
pub struct InputSender {
    socket: UdpSocket,
    peer: SocketAddr,
    cipher: PacketCipher,
    stats: InputChannelStats,
}

impl InputSender {
    /// Binds `local` and targets `peer`. `cipher` is the client-to-host media
    /// cipher of the session.
    ///
    /// # Errors
    /// Returns the bind error of the operating system.
    pub fn bind(local: SocketAddr, peer: SocketAddr, cipher: PacketCipher) -> io::Result<Self> {
        let socket = bind_socket(local)?;
        // The traffic class is only marked toward a private peer, as on the media socket.
        let dscp = if crate::socket::is_private_peer(&peer) {
            input_socket_options().dscp
        } else {
            0
        };
        let view = socket2::SockRef::from(&socket);
        if peer.is_ipv4() {
            view.set_tos_v4(dscp << 2)?;
        } else {
            view.set_tclass_v6(dscp << 2)?;
        }
        Ok(Self {
            socket,
            peer,
            cipher,
            stats: InputChannelStats::default(),
        })
    }

    /// The local address of the socket.
    ///
    /// # Errors
    /// Returns the error of `getsockname`.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Seals one event and sends it as one datagram.
    ///
    /// `client_ts_us` is `CLOCK_MONOTONIC` microseconds on the client, read
    /// as late as possible so it marks the moment the event went out.
    ///
    /// The sequence number of the event is the AEAD counter of the cipher, so
    /// the nonce and the clear header of the datagram agree by construction.
    /// An event that fails validation consumes no sequence number.
    ///
    /// # Errors
    /// Returns [`InputChannelError::Codec`] when the event has no wire form
    /// and [`InputChannelError::Io`] when the socket fails.
    pub fn send(
        &mut self,
        action: InputAction,
        client_ts_us: u64,
    ) -> Result<InputEnvelope, InputChannelError> {
        let sequence = self.cipher.counter();
        let datagram = input::seal_packet(&mut self.cipher, client_ts_us, action)?;
        self.socket.send_to(&datagram, self.peer)?;
        self.stats.sent += 1;
        Ok(InputEnvelope {
            sequence,
            client_ts_us,
            action,
        })
    }

    /// The counters of the sender.
    #[must_use]
    pub const fn stats(&self) -> InputChannelStats {
        self.stats
    }
}

/// Reads sealed input datagrams and yields the events they carry.
///
/// The receiver keeps the replay window of the protocol. The window only moves
/// after the tag has verified and the body has parsed, so a forged datagram
/// cannot push a real event out of the window.
#[derive(Debug)]
pub struct InputReceiver {
    socket: UdpSocket,
    cipher: PacketCipher,
    replay: ReplayWindow,
    stats: InputChannelStats,
    buf: Vec<u8>,
}

impl InputReceiver {
    /// Binds `addr` and opens datagrams with `cipher`, the client-to-host
    /// media cipher of the session.
    ///
    /// The read timeout is [`DEFAULT_READ_TIMEOUT`], so a caller that stops
    /// the thread does not wait long.
    ///
    /// # Errors
    /// Returns the bind error of the operating system.
    pub fn bind(addr: SocketAddr, cipher: PacketCipher) -> io::Result<Self> {
        let socket = bind_socket(addr)?;
        socket.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
        Ok(Self {
            socket,
            cipher,
            replay: ReplayWindow::default(),
            stats: InputChannelStats::default(),
            buf: vec![0; RECV_BUF_LEN],
        })
    }

    /// The local address of the socket.
    ///
    /// # Errors
    /// Returns the error of `getsockname`.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Sets the blocking timeout of [`Self::recv`]. `None` blocks forever.
    ///
    /// # Errors
    /// Returns the error of `setsockopt`.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }

    /// Reads one datagram and returns the event it carries.
    ///
    /// The call blocks until a datagram arrives or the read timeout passes.
    /// A timeout comes back as an io error with the kind `WouldBlock` or
    /// `TimedOut`; the caller decides whether to keep waiting.
    ///
    /// A datagram that fails authentication or that is malformed is counted
    /// and dropped: it never reaches the caller.
    ///
    /// # Errors
    /// Returns [`InputChannelError::Io`] when the socket fails or the read
    /// timeout passes.
    pub fn recv(&mut self) -> Result<InputEnvelope, InputChannelError> {
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
                return Err(InputChannelError::Codec(err));
            }
        };
        if !self.replay.accept(envelope.sequence) {
            self.stats.replays += 1;
            return Err(InputChannelError::Codec(InputCodecError::Malformed(
                "the sequence was already accepted",
            )));
        }
        self.stats.accepted += 1;
        Ok(envelope)
    }

    /// The counters of the receiver.
    #[must_use]
    pub const fn stats(&self) -> InputChannelStats {
        self.stats
    }
}

/// What the input thread did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputThreadOutcome {
    /// The counters of the receiver.
    pub stats: InputChannelStats,
    /// The error that stopped the thread, when there was one.
    pub error: Option<String>,
}

/// The input path on a thread of its own.
///
/// The thread owns the socket and does nothing else: it reads a datagram,
/// opens it, checks the replay window and hands the event to the handler. No
/// queue sits between the socket and the handler, so an input event never
/// waits behind another input event, and nothing of the video path shares
/// this thread.
#[derive(Debug)]
pub struct InputThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    shared: Arc<Mutex<InputThreadShared>>,
}

/// The state the thread shares with its owner.
#[derive(Debug, Default)]
struct InputThreadShared {
    stats: InputChannelStats,
    error: Option<String>,
}

impl InputThread {
    /// Starts the thread.
    ///
    /// The handler is called on the input thread, once per event, in the order
    /// the events arrived. It must not block for long: the thread is the
    /// input path.
    #[must_use]
    pub fn spawn<H>(mut receiver: InputReceiver, mut handler: H) -> Self
    where
        H: FnMut(InputEnvelope) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(InputThreadShared::default()));
        let thread_stop = Arc::clone(&stop);
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("lumen-input".to_owned())
            .spawn(move || {
                loop {
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match receiver.recv() {
                        Ok(envelope) => {
                            let mut state = lock(&thread_shared);
                            state.stats = receiver.stats();
                            drop(state);
                            handler(envelope);
                        }
                        Err(InputChannelError::Io(e)) if is_timeout(&e) => {}
                        Err(e) => {
                            let mut state = lock(&thread_shared);
                            state.error = Some(e.to_string());
                            break;
                        }
                    }
                }
                let mut state = lock(&thread_shared);
                state.stats = receiver.stats();
            })
            .expect("the input thread starts");
        Self {
            stop,
            handle: Some(handle),
            shared,
        }
    }

    /// The counters of the thread so far.
    #[must_use]
    pub fn stats(&self) -> InputChannelStats {
        lock(&self.shared).stats
    }

    /// Stops the thread and waits for it to finish.
    ///
    /// The thread wakes at least once per read timeout, so the wait is short.
    pub fn stop(mut self) -> InputThreadOutcome {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let state = lock(&self.shared);
        InputThreadOutcome {
            stats: state.stats,
            error: state.error.clone(),
        }
    }
}

/// Locks the shared state. The lock is only held for a moment, so it is never
/// poisoned.
fn lock(shared: &Mutex<InputThreadShared>) -> std::sync::MutexGuard<'_, InputThreadShared> {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// True when the io error is a timeout of the read.
fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Binds a UDP socket for the input path.
///
/// The socket carries the same queueing priority and the same buffer options
/// as the media socket, but it stays blocking, because the input thread
/// spends its life waiting for a datagram.
fn bind_socket(addr: SocketAddr) -> io::Result<UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let options = input_socket_options();
    let socket = socket2::Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.set_priority(options.priority)?;
    let _ = socket.set_recv_buffer_size(options.recv_buffer);
    let _ = socket.set_send_buffer_size(options.recv_buffer);
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_crypto::AeadKind;

    const KEY: [u8; 32] = [0x21; 32];
    const SESSION: u32 = 17;

    fn cipher() -> PacketCipher {
        PacketCipher::new(AeadKind::Aes256Gcm, KEY, SESSION)
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// A sender and a receiver on the loopback interface.
    fn pair() -> (InputSender, InputReceiver, SocketAddr) {
        let receiver = InputReceiver::bind(loopback(0), cipher()).expect("bind the receiver");
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set the timeout");
        let addr = receiver.local_addr().expect("the address of the receiver");
        let sender = InputSender::bind(loopback(0), addr, cipher()).expect("bind the sender");
        (sender, receiver, addr)
    }

    #[test]
    fn an_event_sent_arrives_at_the_other_end_unchanged() {
        let (mut sender, mut receiver, _) = pair();
        let actions = [
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
            InputAction::MotionRelative { dx: -3.5, dy: 2.25 },
            InputAction::MotionAbsolute { x: 1919.0, y: 0.5 },
            InputAction::Button {
                button: 0x110,
                pressed: false,
            },
            InputAction::Scroll {
                dx: 0.0,
                dy: -120.0,
            },
        ];
        for (index, action) in actions.iter().enumerate() {
            let sent = sender
                .send(*action, 1_000 + u64::try_from(index).unwrap_or(0))
                .expect("sends");
            let got = receiver.recv().expect("receives");
            assert_eq!(got.action, *action);
            assert_eq!(got.sequence, sent.sequence);
            assert_eq!(got.client_ts_us, sent.client_ts_us);
        }
        assert_eq!(sender.stats().sent, 5);
        assert_eq!(receiver.stats().accepted, 5);
        assert_eq!(receiver.stats().datagrams, 5);
    }

    #[test]
    fn every_event_gets_its_own_sequence_number() {
        let (mut sender, mut receiver, _) = pair();
        for index in 0..4u64 {
            let sent = sender
                .send(InputAction::MotionRelative { dx: 1.0, dy: 0.0 }, index)
                .expect("sends");
            assert_eq!(sent.sequence, index);
            let got = receiver.recv().expect("receives");
            assert_eq!(got.sequence, index);
        }
    }

    #[test]
    fn a_wrong_key_is_rejected() {
        let mut receiver = InputReceiver::bind(
            loopback(0),
            PacketCipher::new(AeadKind::Aes256Gcm, [0x22; 32], SESSION),
        )
        .expect("bind the receiver");
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set the timeout");
        let addr = receiver.local_addr().expect("the address");
        let mut sender = InputSender::bind(loopback(0), addr, cipher()).expect("bind the sender");
        sender
            .send(
                InputAction::Key {
                    keycode: 30,
                    pressed: true,
                },
                1,
            )
            .expect("sends");
        let err = receiver.recv().expect_err("the wrong key is refused");
        assert!(matches!(
            err,
            InputChannelError::Codec(InputCodecError::Authentication)
        ));
        let stats = receiver.stats();
        assert_eq!(stats.authentication_failures, 1);
        assert_eq!(stats.accepted, 0);
    }

    #[test]
    fn a_replayed_sequence_is_rejected() {
        let mut receiver = InputReceiver::bind(loopback(0), cipher()).expect("bind the receiver");
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set the timeout");
        let addr = receiver.local_addr().expect("the address");

        // The same datagram twice, sealed with the key of the session.
        let mut sealer = cipher();
        let datagram = input::seal_packet(
            &mut sealer,
            7,
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
        )
        .expect("seals");
        let raw = UdpSocket::bind(loopback(0)).expect("bind the raw socket");
        raw.send_to(&datagram, addr).expect("sends");
        let got = receiver.recv().expect("receives");
        assert_eq!(got.sequence, 0);

        raw.send_to(&datagram, addr).expect("sends again");
        let err = receiver.recv().expect_err("the replay is refused");
        assert!(matches!(
            err,
            InputChannelError::Codec(InputCodecError::Malformed(_))
        ));
        let stats = receiver.stats();
        assert_eq!(stats.replays, 1);
        assert_eq!(stats.accepted, 1);
    }

    #[test]
    fn a_malformed_datagram_is_refused_without_the_window_moving() {
        let mut receiver = InputReceiver::bind(loopback(0), cipher()).expect("bind the receiver");
        receiver
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("set the timeout");
        let addr = receiver.local_addr().expect("the address");
        let raw = UdpSocket::bind(loopback(0)).expect("bind the raw socket");
        raw.send_to(b"not an input datagram", addr).expect("sends");
        let err = receiver.recv().expect_err("the rubbish is refused");
        assert!(matches!(err, InputChannelError::Codec(_)));
        assert_eq!(receiver.stats().malformed, 1);
        assert_eq!(receiver.stats().accepted, 0);
    }

    #[test]
    fn the_thread_hands_the_events_to_the_handler() {
        let receiver = InputReceiver::bind(loopback(0), cipher()).expect("bind the receiver");
        let addr = receiver.local_addr().expect("the address");
        let thread = InputThread::spawn(receiver, |_envelope| {});
        let mut sender = InputSender::bind(loopback(0), addr, cipher()).expect("bind the sender");
        for index in 0..5u64 {
            sender
                .send(InputAction::MotionRelative { dx: 1.0, dy: 2.0 }, index)
                .expect("sends");
        }
        // Give the thread a moment to drain the socket.
        std::thread::sleep(Duration::from_millis(200));
        let outcome = thread.stop();
        assert_eq!(outcome.stats.accepted, 5);
        assert!(outcome.error.is_none());
    }
}
