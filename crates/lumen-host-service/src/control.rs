//! The control channel.
//!
//! A TCP connection that carries structured messages: hello and capability
//! negotiation, pairing, NACKs, IDR requests, monitor and clipboard updates,
//! and keepalives. Media never travels on this channel.
//!
//! Wire format: a `u32` little-endian body length, then the body. The body is
//! the encoded message before [`ControlChannel::set_cipher`] and the AEAD
//! ciphertext after it. Each direction has its own key and its own counter,
//! so a dropped, replayed or tampered record fails authentication. After any
//! error the channel must be dropped, because the counters are no longer in
//! step.
//!
//! The channel sits on top of a Noise `XX` handshake: the handshake frames
//! travel on the same socket before any control record exists, each with a
//! 16-bit big-endian length prefix. See [`crate::session`].

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use lumen_crypto::{CryptoError, PacketCipher};
use lumen_protocol::{Capabilities, ProtocolError};

/// Largest record body accepted, to bound allocation on a hostile length.
const MAX_RECORD: usize = 1 << 20;

/// Size of one read from the socket.
const READ_CHUNK: usize = 4096;
/// Shortest read timeout that the kernel accepts.
const MIN_TIMEOUT: Duration = Duration::from_millis(1);

/// A control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    /// The client opens the session.
    Hello {
        /// Protocol version the client speaks.
        version: u8,
        /// Capabilities the client supports.
        capabilities: Capabilities,
        /// The client's Ed25519 public key.
        device_key: [u8; 32],
    },
    /// The host answers the hello.
    HelloAck {
        /// The negotiated protocol version.
        version: u8,
        /// The negotiated capabilities.
        capabilities: Capabilities,
        /// The session identifier.
        session_id: u32,
        /// Whether the client is authorised.
        paired: bool,
    },
    /// The client asks for a keyframe.
    RequestIdr {
        /// The frame the client is missing.
        frame_id: u32,
    },
    /// The client reports missing slices.
    Nack {
        /// The frame the slices belong to.
        frame_id: u32,
        /// The missing slice indices.
        slices: Vec<u16>,
    },
    /// The host reports a monitor.
    Monitor {
        /// The monitor index.
        index: u32,
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Refresh rate in Hz.
        refresh_hz: u32,
    },
    /// The host reports a resolution change.
    ResolutionChanged {
        /// The new width.
        width: u32,
        /// The new height.
        height: u32,
    },
    /// Clipboard content.
    Clipboard {
        /// The text.
        text: String,
    },
    /// A keepalive.
    Ping {
        /// The sender's timestamp in microseconds.
        ts_us: u64,
    },
    /// The pong answering a ping.
    Pong {
        /// The original timestamp.
        ts_us: u64,
        /// The responder's timestamp.
        reply_ts_us: u64,
    },
    /// The session ends.
    Bye,
    /// The host asks the client to prove it owns its identity key, and, when
    /// the client is not trusted yet, to answer a pairing code.
    PairingChallenge {
        /// The pairing code the host shows.
        code: u32,
        /// The host's Ed25519 public key, so the client can name it.
        host_key: [u8; 32],
        /// A fresh nonce. The client signs it, so the proof cannot be
        /// replayed into another session.
        nonce: [u8; 32],
    },
    /// The client answers a pairing challenge.
    PairingResponse {
        /// The code the user typed. Zero when the client is already trusted.
        code: u32,
        /// The client's signature over the proof message.
        signature: [u8; 64],
    },
    /// The client reports what it received, so the host can estimate the loss
    /// and the round trip time.
    LinkFeedback {
        /// Data slices the client received.
        received: u64,
        /// Data slices the client never received.
        lost: u64,
        /// The client's measured round trip time, microseconds.
        rtt_us: u64,
    },
    /// The client asks for a smaller picture because it cannot keep up.
    ///
    /// A weak client that is given a picture it cannot reach drops frames,
    /// and a stream that drops frames is worse than a smaller stream that
    /// does not. The host scales the capture down to this size.
    Quality {
        /// The width the client asks for.
        width: u32,
        /// The height the client asks for.
        height: u32,
        /// The frame rate the client asks for.
        fps: u32,
    },
    /// The client reports the stage timestamps of one presented frame.
    ///
    /// The host maps these onto its own clock with the offset it estimates
    /// from the ping exchange, so it can measure the whole pipeline.
    StageTimes {
        /// The host frame identifier.
        frame_id: u32,
        /// When the client reassembled the access unit, client clock.
        recv_ts_us: u64,
        /// When the client started decoding, client clock.
        decode_start_ts_us: u64,
        /// When the client finished decoding, client clock.
        decode_ts_us: u64,
        /// When the client presented it, client clock.
        present_ts_us: u64,
    },
}

/// Errors of the control channel.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// The underlying socket failed or the peer closed the connection.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The message kind byte is unknown.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// Authentication or encryption failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    /// A record or field does not fit the wire format.
    #[error("malformed control message: {0}")]
    Malformed(&'static str),
}

/// A control channel over an established TCP connection.
#[derive(Debug)]
pub struct ControlChannel {
    stream: TcpStream,
    send_cipher: Option<PacketCipher>,
    recv_cipher: Option<PacketCipher>,
    recv_counter: u64,
    /// Bytes read from the socket that do not yet form a whole record.
    recv_buf: Vec<u8>,
}

impl ControlChannel {
    /// Wrap a connected stream. Nagle is disabled so small messages leave at once.
    ///
    /// # Errors
    /// Returns an io error when the socket option cannot be set.
    pub fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            send_cipher: None,
            recv_cipher: None,
            recv_counter: 0,
            recv_buf: Vec::new(),
        })
    }

    /// Encrypt every record from now on. `send` seals with the local send
    /// cipher; `recv` opens with the peer's cipher, which uses the other key.
    pub fn set_cipher(&mut self, send: PacketCipher, recv: PacketCipher) {
        self.send_cipher = Some(send);
        self.recv_cipher = Some(recv);
        self.recv_counter = 0;
    }

    /// Send a control message.
    ///
    /// # Errors
    /// Returns [`ControlError::Malformed`] when the record exceeds the size
    /// limit, [`ControlError::Crypto`] when sealing fails, and
    /// [`ControlError::Io`] when the write fails.
    pub fn send(&mut self, message: &ControlMessage) -> Result<(), ControlError> {
        let plain = encode(message)?;
        let body = match self.send_cipher.as_mut() {
            Some(cipher) => cipher.seal(&[], &plain)?,
            None => plain,
        };
        if body.len() > MAX_RECORD {
            return Err(ControlError::Malformed("record exceeds size limit"));
        }
        let len = u32::try_from(body.len())
            .map_err(|_| ControlError::Malformed("record length overflows u32"))?;
        let mut record = Vec::with_capacity(4 + body.len());
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(&body);
        self.stream.write_all(&record)?;
        Ok(())
    }

    /// Receive a control message.
    ///
    /// # Errors
    /// Returns [`ControlError::Io`] on a read failure or peer close,
    /// [`ControlError::Malformed`] for an oversized or truncated record,
    /// [`ControlError::Crypto`] when authentication fails, and
    /// [`ControlError::Protocol`] for an unknown message kind.
    pub fn recv(&mut self) -> Result<ControlMessage, ControlError> {
        loop {
            if let Some(message) = self.take_record()? {
                return Ok(message);
            }
            self.fill(self.needed()?, None)?;
        }
    }

    /// Receive a control message, waiting at most `timeout`.
    ///
    /// Returns `Ok(None)` when the timeout passes before a whole record
    /// arrives. A partially received record is kept, so the next call
    /// continues where this one stopped.
    ///
    /// # Errors
    /// Returns [`ControlError::Io`] on a read failure or peer close,
    /// [`ControlError::Malformed`] for an oversized or truncated record,
    /// [`ControlError::Crypto`] when authentication fails, and
    /// [`ControlError::Protocol`] when there is an unknown message kind.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ControlMessage>, ControlError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(message) = self.take_record()? {
                return Ok(Some(message));
            }
            if !self.fill(self.needed()?, Some(deadline))? {
                return Ok(None);
            }
        }
    }

    /// How many bytes the next read must produce.
    ///
    /// # Errors
    /// Returns [`ControlError::Malformed`] when the length prefix exceeds the
    /// record limit.
    fn needed(&self) -> Result<usize, ControlError> {
        if self.recv_buf.len() < 4 {
            return Ok(4);
        }
        let len = u32::from_le_bytes([
            self.recv_buf[0],
            self.recv_buf[1],
            self.recv_buf[2],
            self.recv_buf[3],
        ]) as usize;
        if len > MAX_RECORD {
            return Err(ControlError::Malformed("record exceeds size limit"));
        }
        Ok(4 + len)
    }

    /// Decode the record at the head of the buffer, if a whole one is there.
    fn take_record(&mut self) -> Result<Option<ControlMessage>, ControlError> {
        if self.recv_buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([
            self.recv_buf[0],
            self.recv_buf[1],
            self.recv_buf[2],
            self.recv_buf[3],
        ]) as usize;
        if len > MAX_RECORD {
            return Err(ControlError::Malformed("record exceeds size limit"));
        }
        if self.recv_buf.len() < 4 + len {
            return Ok(None);
        }
        let body = self.recv_buf[4..4 + len].to_vec();
        self.recv_buf.drain(..4 + len);
        let plain = match self.recv_cipher.as_ref() {
            Some(cipher) => {
                let plain = cipher.open(&[], &body, self.recv_counter)?;
                self.recv_counter += 1;
                plain
            }
            None => body,
        };
        Ok(Some(decode(&plain)?))
    }

    /// Read into the buffer until it holds `want` bytes.
    ///
    /// With a deadline, a read that runs out of time returns `false` and the
    /// bytes read so far stay in the buffer. Without one, the read blocks.
    fn fill(&mut self, want: usize, deadline: Option<Instant>) -> Result<bool, ControlError> {
        let mut chunk = [0u8; READ_CHUNK];
        while self.recv_buf.len() < want {
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(false);
                    }
                    let remaining = (deadline - now).max(MIN_TIMEOUT);
                    self.stream.set_read_timeout(Some(remaining))?;
                }
                None => self.stream.set_read_timeout(None)?,
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(ControlError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "peer closed the control connection",
                    )));
                }
                Ok(n) => self.recv_buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if deadline.is_some() {
                        return Ok(false);
                    }
                    return Err(ControlError::Io(e));
                }
                Err(e) => return Err(ControlError::Io(e)),
            }
        }
        Ok(true)
    }
}

/// Encode a control message into its wire form, without the length prefix.
fn encode(message: &ControlMessage) -> Result<Vec<u8>, ControlError> {
    let mut out = Vec::new();
    match message {
        ControlMessage::Hello {
            version,
            capabilities,
            device_key,
        } => {
            out.push(1);
            out.push(*version);
            out.extend_from_slice(&capabilities.bits().to_le_bytes());
            out.extend_from_slice(device_key);
        }
        ControlMessage::HelloAck {
            version,
            capabilities,
            session_id,
            paired,
        } => {
            out.push(2);
            out.push(*version);
            out.extend_from_slice(&capabilities.bits().to_le_bytes());
            out.extend_from_slice(&session_id.to_le_bytes());
            out.push(u8::from(*paired));
        }
        ControlMessage::RequestIdr { frame_id } => {
            out.push(3);
            out.extend_from_slice(&frame_id.to_le_bytes());
        }
        ControlMessage::Nack { frame_id, slices } => {
            let count = u16::try_from(slices.len())
                .map_err(|_| ControlError::Malformed("too many NACK slices"))?;
            out.push(4);
            out.extend_from_slice(&frame_id.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            for slice in slices {
                out.extend_from_slice(&slice.to_le_bytes());
            }
        }
        ControlMessage::Monitor {
            index,
            width,
            height,
            refresh_hz,
        } => {
            out.push(5);
            for field in [index, width, height, refresh_hz] {
                out.extend_from_slice(&field.to_le_bytes());
            }
        }
        ControlMessage::ResolutionChanged { width, height } => {
            out.push(6);
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
        }
        ControlMessage::Clipboard { text } => {
            let len = u32::try_from(text.len())
                .map_err(|_| ControlError::Malformed("clipboard too large"))?;
            out.push(7);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        ControlMessage::Ping { ts_us } => {
            out.push(8);
            out.extend_from_slice(&ts_us.to_le_bytes());
        }
        ControlMessage::Pong { ts_us, reply_ts_us } => {
            out.push(9);
            out.extend_from_slice(&ts_us.to_le_bytes());
            out.extend_from_slice(&reply_ts_us.to_le_bytes());
        }
        ControlMessage::Bye => out.push(10),
        ControlMessage::PairingChallenge {
            code,
            host_key,
            nonce,
        } => {
            out.push(11);
            out.extend_from_slice(&code.to_le_bytes());
            out.extend_from_slice(host_key);
            out.extend_from_slice(nonce);
        }
        ControlMessage::PairingResponse { code, signature } => {
            out.push(12);
            out.extend_from_slice(&code.to_le_bytes());
            out.extend_from_slice(signature);
        }
        ControlMessage::LinkFeedback {
            received,
            lost,
            rtt_us,
        } => {
            out.push(14);
            out.extend_from_slice(&received.to_le_bytes());
            out.extend_from_slice(&lost.to_le_bytes());
            out.extend_from_slice(&rtt_us.to_le_bytes());
        }
        ControlMessage::Quality { width, height, fps } => {
            out.push(15);
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&fps.to_le_bytes());
        }
        ControlMessage::StageTimes {
            frame_id,
            recv_ts_us,
            decode_start_ts_us,
            decode_ts_us,
            present_ts_us,
        } => {
            out.push(13);
            out.extend_from_slice(&frame_id.to_le_bytes());
            out.extend_from_slice(&recv_ts_us.to_le_bytes());
            out.extend_from_slice(&decode_start_ts_us.to_le_bytes());
            out.extend_from_slice(&decode_ts_us.to_le_bytes());
            out.extend_from_slice(&present_ts_us.to_le_bytes());
        }
    }
    Ok(out)
}

/// A bounds-checked cursor over a received record.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], ControlError> {
        if self.buf.len() < n {
            return Err(ControlError::Malformed("truncated field"));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ControlError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, ControlError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, ControlError> {
        Ok(u16::from_le_bytes(self.array::<2>()?))
    }

    fn u32(&mut self) -> Result<u32, ControlError> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    fn u64(&mut self) -> Result<u64, ControlError> {
        Ok(u64::from_le_bytes(self.array::<8>()?))
    }
}

/// Decode a control message from its wire form.
fn decode(buf: &[u8]) -> Result<ControlMessage, ControlError> {
    let mut r = Reader { buf };
    // Struct-literal fields are evaluated in written order, so the reads below
    // consume the record front to back.
    let message = match r.u8()? {
        1 => ControlMessage::Hello {
            version: r.u8()?,
            capabilities: Capabilities::from_bits_truncate(r.u64()?),
            device_key: r.array::<32>()?,
        },
        2 => ControlMessage::HelloAck {
            version: r.u8()?,
            capabilities: Capabilities::from_bits_truncate(r.u64()?),
            session_id: r.u32()?,
            paired: r.u8()? != 0,
        },
        3 => ControlMessage::RequestIdr { frame_id: r.u32()? },
        4 => {
            let frame_id = r.u32()?;
            let count = r.u16()?;
            let mut slices = Vec::with_capacity(usize::from(count));
            for _ in 0..count {
                slices.push(r.u16()?);
            }
            ControlMessage::Nack { frame_id, slices }
        }
        5 => ControlMessage::Monitor {
            index: r.u32()?,
            width: r.u32()?,
            height: r.u32()?,
            refresh_hz: r.u32()?,
        },
        6 => ControlMessage::ResolutionChanged {
            width: r.u32()?,
            height: r.u32()?,
        },
        7 => {
            let len = r.u32()? as usize;
            let text = std::str::from_utf8(r.bytes(len)?)
                .map_err(|_| ControlError::Malformed("clipboard is not UTF-8"))?;
            ControlMessage::Clipboard {
                text: text.to_owned(),
            }
        }
        8 => ControlMessage::Ping { ts_us: r.u64()? },
        9 => ControlMessage::Pong {
            ts_us: r.u64()?,
            reply_ts_us: r.u64()?,
        },
        10 => ControlMessage::Bye,
        11 => ControlMessage::PairingChallenge {
            code: r.u32()?,
            host_key: r.array::<32>()?,
            nonce: r.array::<32>()?,
        },
        12 => ControlMessage::PairingResponse {
            code: r.u32()?,
            signature: r.array::<64>()?,
        },
        14 => ControlMessage::LinkFeedback {
            received: r.u64()?,
            lost: r.u64()?,
            rtt_us: r.u64()?,
        },
        15 => ControlMessage::Quality {
            width: r.u32()?,
            height: r.u32()?,
            fps: r.u32()?,
        },
        13 => ControlMessage::StageTimes {
            frame_id: r.u32()?,
            recv_ts_us: r.u64()?,
            decode_start_ts_us: r.u64()?,
            decode_ts_us: r.u64()?,
            present_ts_us: r.u64()?,
        },
        other => return Err(ProtocolError::BadKind(other).into()),
    };
    Ok(message)
}

/// Decode a control message from its wire form, without a channel.
///
/// Same decoder the channel uses; exposed so fuzz targets can reach it.
///
/// # Errors
/// Returns [`ControlError::Protocol`] for an unknown message kind and
/// [`ControlError::Malformed`] for a truncated or invalid field.
pub fn decode_message(buf: &[u8]) -> Result<ControlMessage, ControlError> {
    decode(buf)
}

/// A listener for incoming control connections.
#[derive(Debug)]
pub struct ControlListener {
    listener: TcpListener,
}

impl ControlListener {
    /// Bind to `addr`.
    ///
    /// # Errors
    /// Returns an io error when the bind fails.
    pub fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr)?,
        })
    }

    /// The address the listener is bound to.
    ///
    /// # Errors
    /// Returns an io error when the address cannot be read.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept the next connection.
    ///
    /// # Errors
    /// Returns an io error when the accept fails.
    pub fn accept(&self) -> std::io::Result<(TcpStream, SocketAddr)> {
        self.listener.accept()
    }

    /// Accept the next connection, waiting at most `timeout`.
    ///
    /// Returns `None` when the timeout passes before a client arrives. The
    /// host uses this to wait for a client without giving up the run.
    ///
    /// # Errors
    /// Returns an io error when the accept fails.
    pub fn accept_timeout(
        &self,
        timeout: Duration,
    ) -> std::io::Result<Option<(TcpStream, SocketAddr)>> {
        self.listener.set_nonblocking(true)?;
        let deadline = Instant::now() + timeout;
        let result = loop {
            match self.listener.accept() {
                Ok(pair) => break Ok(Some(pair)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => break Err(e),
            }
        };
        self.listener.set_nonblocking(false)?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_crypto::AeadKind;
    use lumen_protocol::VersionHandshake;

    fn every_message() -> Vec<ControlMessage> {
        vec![
            ControlMessage::Hello {
                version: 1,
                capabilities: Capabilities::H264 | Capabilities::HEVC,
                device_key: [7; 32],
            },
            ControlMessage::HelloAck {
                version: 1,
                capabilities: Capabilities::H264,
                session_id: 0xdead_beef,
                paired: true,
            },
            ControlMessage::RequestIdr { frame_id: 99 },
            ControlMessage::Nack {
                frame_id: 99,
                slices: vec![1, 2, 3],
            },
            ControlMessage::Monitor {
                index: 1,
                width: 2560,
                height: 1440,
                refresh_hz: 120,
            },
            ControlMessage::ResolutionChanged {
                width: 3840,
                height: 2160,
            },
            ControlMessage::Clipboard {
                text: "hello \u{1F600}".to_string(),
            },
            ControlMessage::Ping { ts_us: 5 },
            ControlMessage::Pong {
                ts_us: 5,
                reply_ts_us: 6,
            },
            ControlMessage::Bye,
        ]
    }

    fn pair() -> (ControlChannel, ControlChannel) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (
            ControlChannel::new(server).unwrap(),
            ControlChannel::new(client).unwrap(),
        )
    }

    #[test]
    fn every_message_roundtrips_through_the_wire_form() {
        for message in every_message() {
            let bytes = encode(&message).unwrap();
            assert_eq!(decode(&bytes).unwrap(), message);
        }
    }

    #[test]
    fn truncated_record_is_rejected() {
        let mut bytes = encode(&ControlMessage::Ping { ts_us: 1 }).unwrap();
        bytes.pop();
        assert!(matches!(decode(&bytes), Err(ControlError::Malformed(_))));
    }

    #[test]
    fn unknown_kind_is_rejected() {
        assert!(matches!(
            decode(&[200]),
            Err(ControlError::Protocol(ProtocolError::BadKind(200)))
        ));
    }

    #[test]
    fn invalid_utf8_clipboard_is_rejected() {
        let bytes = [7, 2, 0, 0, 0, 0xff, 0xfe];
        assert!(matches!(decode(&bytes), Err(ControlError::Malformed(_))));
    }

    #[test]
    fn plaintext_channel_roundtrips() {
        let (mut host, mut client) = pair();
        client.send(&ControlMessage::Ping { ts_us: 42 }).unwrap();
        assert_eq!(host.recv().unwrap(), ControlMessage::Ping { ts_us: 42 });
    }

    #[test]
    fn encrypted_channel_roundtrips_in_both_directions() {
        let (mut host, mut client) = pair();
        host.set_cipher(
            PacketCipher::new(AeadKind::Aes256Gcm, [1; 32], 7),
            PacketCipher::new(AeadKind::Aes256Gcm, [2; 32], 7),
        );
        client.set_cipher(
            PacketCipher::new(AeadKind::Aes256Gcm, [2; 32], 7),
            PacketCipher::new(AeadKind::Aes256Gcm, [1; 32], 7),
        );
        for ts in 0..3 {
            client.send(&ControlMessage::Ping { ts_us: ts }).unwrap();
            assert_eq!(host.recv().unwrap(), ControlMessage::Ping { ts_us: ts });
            host.send(&ControlMessage::Pong {
                ts_us: ts,
                reply_ts_us: ts + 1,
            })
            .unwrap();
            assert_eq!(
                client.recv().unwrap(),
                ControlMessage::Pong {
                    ts_us: ts,
                    reply_ts_us: ts + 1
                }
            );
        }
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let (mut host, mut client) = pair();
        host.set_cipher(
            PacketCipher::new(AeadKind::Aes256Gcm, [1; 32], 7),
            PacketCipher::new(AeadKind::Aes256Gcm, [2; 32], 7),
        );
        client.set_cipher(
            PacketCipher::new(AeadKind::Aes256Gcm, [2; 32], 7),
            PacketCipher::new(AeadKind::Aes256Gcm, [9; 32], 7),
        );
        host.send(&ControlMessage::Bye).unwrap();
        assert!(matches!(client.recv(), Err(ControlError::Crypto(_))));
    }

    #[test]
    fn oversized_length_is_rejected_before_allocation() {
        let (mut host, client) = pair();
        let mut raw = client.stream.try_clone().unwrap();
        raw.write_all(&u32::MAX.to_le_bytes()).unwrap();
        assert!(matches!(host.recv(), Err(ControlError::Malformed(_))));
    }

    #[test]
    fn a_timeout_returns_nothing_and_keeps_the_channel_usable() {
        let (mut host, mut client) = pair();
        let none = host.recv_timeout(Duration::from_millis(50)).unwrap();
        assert!(
            none.is_none(),
            "no message was sent, so the timeout must fire"
        );
        client.send(&ControlMessage::Ping { ts_us: 7 }).unwrap();
        assert_eq!(
            host.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some(ControlMessage::Ping { ts_us: 7 })
        );
    }

    #[test]
    fn a_partial_record_survives_a_timeout() {
        let (mut host, client) = pair();
        let mut raw = client.stream.try_clone().unwrap();
        let bytes = encode(&ControlMessage::Ping { ts_us: 7 }).unwrap();
        // The length prefix and part of the body.
        raw.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
        raw.write_all(&bytes[..3]).unwrap();
        raw.flush().unwrap();
        assert!(
            host.recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
        // The rest of the record completes it.
        raw.write_all(&bytes[3..]).unwrap();
        raw.flush().unwrap();
        let message = host
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .expect("the record completes after the timeout");
        assert_eq!(message, ControlMessage::Ping { ts_us: 7 });
    }

    #[test]
    fn negotiation_picks_common_capabilities() {
        let local = VersionHandshake {
            version: 1,
            capabilities: Capabilities::H264 | Capabilities::HEVC,
        };
        let remote = VersionHandshake {
            version: 1,
            capabilities: Capabilities::H264,
        };
        let (version, caps) =
            VersionHandshake::negotiate(&local, &remote, lumen_protocol::SUPPORTED_VERSIONS)
                .unwrap();
        assert_eq!(version, 1);
        assert_eq!(caps, Capabilities::H264);
    }
}
