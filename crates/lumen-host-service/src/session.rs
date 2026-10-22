//! Session establishment.
//!
//! A session starts with a Noise XX handshake run directly on the TCP
//! connection, before any control record exists. Both ends derive the same
//! [`SessionKeys`] from the handshake secret, install the control ciphers, and
//! exchange `Hello` and `HelloAck`. Media keys come from the same keys.
//!
//! The client is the Noise initiator and the host is the responder.

use std::io::{Read, Write};
use std::net::TcpStream;

use lumen_crypto::{
    AeadKind, ChannelBinding, CryptoError, DeviceIdentity, Handshake, PAKE_MESSAGE_LEN,
    PacketCipher, PairingCode, PairingPake, Role, SessionKeys, confirmation_matches, host_proof,
    pairing_proof, random_bytes,
};
use lumen_protocol::{Capabilities, ProtocolError, SUPPORTED_VERSIONS, VERSION, VersionHandshake};
use lumen_transport::{MediaError, MediaReceiver, MediaSender, ParityLevel};

use crate::control::{ControlChannel, ControlError, ControlMessage};

/// The AEAD used on every channel of a session when both ends have AES-NI.
///
/// Without hardware acceleration it is correct but slower, so a peer without
/// AES-NI asks for [`AeadKind::ChaCha20Poly1305`] instead (D30, D59).
pub const SESSION_AEAD: AeadKind = AeadKind::Aes256Gcm;

/// Whether the machine has AES-NI, so the transport can be accelerated.
#[must_use]
pub fn has_aes_ni() -> bool {
    std::arch::is_x86_feature_detected!("aes") && std::arch::is_x86_feature_detected!("pclmulqdq")
}

/// Errors while establishing a session or using its channels.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The TCP connection failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The Noise handshake was rejected.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    /// The control channel failed.
    #[error("control error: {0}")]
    Control(#[from] ControlError),
    /// The media path failed to set up.
    #[error("media error: {0}")]
    Media(#[from] MediaError),
    /// The versions or capabilities of the two peers do not overlap.
    #[error("negotiation error: {0}")]
    Negotiation(#[from] ProtocolError),
    /// A handshake message does not fit the 16-bit frame length.
    #[error("handshake message of {0} bytes exceeds 65535")]
    MessageTooLarge(usize),
    /// The peer sent a control message the handshake does not expect.
    #[error("unexpected control message: {0}")]
    Unexpected(&'static str),
    /// The host's session id differs from the id derived from the handshake.
    #[error("session id mismatch")]
    SessionIdMismatch,
    /// The host does not trust this client.
    #[error("the client is not paired")]
    NotPaired,
    /// The pairing was refused, by the user or by the client.
    #[error("the pairing was refused")]
    PairingRefused,
    /// The host did not prove that it owns the key it names, or that it knows the pairing code.
    #[error("the host did not prove its identity or the pairing code")]
    HostProof,
}

/// A completed handshake: the session keys and the static keys both ends used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Established {
    /// Keys and identifier of the session.
    pub keys: SessionKeys,
    /// The Noise static keys of this connection, as authenticated by the handshake.
    pub binding: ChannelBinding,
    /// The transport cipher both ends agreed on.
    pub aead: AeadKind,
    /// The peer's capability bitmap, carried in the handshake.
    pub peer_capabilities: Capabilities,
}

/// Run the Noise XX handshake on `stream` and derive the session keys.
///
/// The capability bitmap travels inside the handshake, in the first message
/// for the initiator and the second for the responder. It is part of the
/// handshake transcript, so a peer that changes it fails the final hash
/// check. Carrying it there means the transport cipher is settled before the
/// first control record exists, and the control channel stays encrypted from
/// its first byte.
///
/// # Errors
/// Returns an error when the handshake is rejected or the connection fails.
pub fn establish(
    stream: &mut TcpStream,
    role: Role,
    capabilities: Capabilities,
) -> Result<Established, SessionError> {
    let mut hs = Handshake::start(role)?;
    let (peer_capabilities, aead) = match role {
        Role::Initiator => {
            let first = hs.write_message_with(&capability_payload(capabilities))?;
            write_frame(stream, &first)?;
            // The responder's answer carries its capability bitmap, so the
            // transport cipher is settled before the first control record.
            let second = hs.read_message_with(&read_frame(stream)?)?;
            let peer = parse_capability_payload(&second);
            let third = hs.write_message_with(&[])?;
            write_frame(stream, &third)?;
            (peer, negotiate_aead(capabilities, peer))
        }
        Role::Responder => {
            let initiator = hs.read_message_with(&read_frame(stream)?)?;
            let peer = parse_capability_payload(&initiator);
            let second = hs.write_message_with(&capability_payload(capabilities))?;
            write_frame(stream, &second)?;
            hs.read_message_with(&read_frame(stream)?)?;
            (peer, negotiate_aead(capabilities, peer))
        }
    };
    let result = hs.finish()?;
    // Noise XX always sends both static keys, so a missing one is a broken handshake.
    let remote = result
        .remote_static
        .ok_or(SessionError::Unexpected("the peer sent no static key"))?;
    let binding = match role {
        Role::Initiator => ChannelBinding {
            client_static: result.local_static,
            host_static: remote,
        },
        Role::Responder => ChannelBinding {
            client_static: remote,
            host_static: result.local_static,
        },
    };
    Ok(Established {
        keys: SessionKeys::derive(&result.key_material),
        binding,
        aead,
        peer_capabilities,
    })
}

/// The capability bitmap as a handshake payload.
fn capability_payload(capabilities: Capabilities) -> Vec<u8> {
    capabilities.bits().to_le_bytes().to_vec()
}

/// Read a capability bitmap out of a handshake payload.
fn parse_capability_payload(payload: &[u8]) -> Capabilities {
    if payload.len() < 8 {
        return Capabilities::empty();
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&payload[..8]);
    Capabilities::from_bits_truncate(u64::from_le_bytes(bytes))
}

/// The transport cipher both ends use.
///
/// AES-256-GCM is the baseline because both test machines have AES-NI. A peer
/// without it asks for ChaCha20-Poly1305, which is correct but slower.
fn negotiate_aead(local: Capabilities, peer: Capabilities) -> AeadKind {
    if local.contains(Capabilities::AEAD_CHACHA20) || peer.contains(Capabilities::AEAD_CHACHA20) {
        AeadKind::ChaCha20Poly1305
    } else {
        SESSION_AEAD
    }
}

/// Write one handshake message with a 16-bit big-endian length prefix.
fn write_frame(stream: &mut TcpStream, msg: &[u8]) -> Result<(), SessionError> {
    let len = u16::try_from(msg.len()).map_err(|_| SessionError::MessageTooLarge(msg.len()))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(msg)?;
    Ok(())
}

/// Read one length-prefixed handshake message.
fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>, SessionError> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let mut msg = vec![0u8; usize::from(u16::from_be_bytes(len))];
    stream.read_exact(&mut msg)?;
    Ok(msg)
}

/// The control ciphers for one end: `(send, recv)`. Each end seals with its
/// own direction's key and opens with the peer's.
#[must_use]
pub fn control_ciphers(
    keys: &SessionKeys,
    role: Role,
    aead: AeadKind,
) -> (PacketCipher, PacketCipher) {
    let (send, recv) = match role {
        Role::Responder => (keys.control_h2c, keys.control_c2h),
        Role::Initiator => (keys.control_c2h, keys.control_h2c),
    };
    (
        PacketCipher::new(aead, send, keys.session_id),
        PacketCipher::new(aead, recv, keys.session_id),
    )
}

/// Wrap an established TCP connection in a control channel keyed for `role`.
///
/// # Errors
/// Returns an error when the socket cannot be configured.
pub fn control_channel(
    stream: TcpStream,
    established: &Established,
    role: Role,
) -> Result<ControlChannel, SessionError> {
    let mut channel = ControlChannel::new(stream)?;
    let (send, recv) = control_ciphers(&established.keys, role, established.aead);
    channel.set_cipher(send, recv);
    Ok(channel)
}

/// The host's media sender for a session. Media flows host to client.
///
/// # Errors
/// Returns an error when the FEC level is not implemented.
pub fn media_sender(keys: &SessionKeys, aead: AeadKind) -> Result<MediaSender, SessionError> {
    Ok(MediaSender::new(
        aead,
        keys.media_h2c,
        keys.session_id,
        ParityLevel::Light,
    )?)
}

/// The client's media receiver for a session.
#[must_use]
pub fn media_receiver(keys: &SessionKeys, aead: AeadKind) -> MediaReceiver {
    MediaReceiver::new(aead, keys.media_h2c, keys.session_id, true)
}

/// Client side of the hello: send `Hello`, then check the `HelloAck` session
/// id, the negotiated version and capabilities, and the host's proof.
///
/// The host always sends a pairing challenge first. When it asks for a code,
/// the `answer` callback is called; it returns the code the user typed, or
/// `None` to refuse the pairing. The code never leaves this machine: it only
/// feeds the SPAKE2 exchange, which is bound to `binding`.
///
/// # Errors
/// Returns an error when the exchange fails, the host does not authorise the
/// client, or the host does not prove its key or the pairing code.
pub fn client_hello(
    channel: &mut ControlChannel,
    identity: &DeviceIdentity,
    keys: &SessionKeys,
    binding: &ChannelBinding,
    capabilities: Capabilities,
    answer: &mut dyn FnMut(PairingChallenge) -> Option<PairingCode>,
) -> Result<ClientHello, SessionError> {
    let device_key = identity.public_bytes();
    channel.send(&ControlMessage::Hello {
        version: VERSION,
        capabilities,
        device_key,
    })?;
    // The confirmation the host must send back, when this session ran the pairing exchange.
    let mut expected_confirm = None;
    let (version, agreed, session_id, paired, host_key, confirm, signature) = loop {
        match channel.recv()? {
            ControlMessage::HelloAck {
                version,
                capabilities: agreed,
                session_id,
                paired,
                host_key,
                confirm,
                signature,
            } => {
                break (
                    version, agreed, session_id, paired, host_key, confirm, signature,
                );
            }
            ControlMessage::PairingChallenge {
                host_key,
                nonce,
                pake,
            } => {
                // The signature proves this client owns the identity key it claimed.
                let signature =
                    identity.sign_bytes(&pairing_proof(&nonce, &host_key, &device_key, binding));
                let mut response = ([0u8; PAKE_MESSAGE_LEN], [0u8; 32]);
                // An all-zero message means the host trusts this client and asks for no code.
                if pake != [0u8; PAKE_MESSAGE_LEN]
                    && let Some(code) = answer(PairingChallenge { host_key })
                {
                    let (state, message) = PairingPake::start(Role::Initiator, code, binding);
                    let confirmations = state.finish(&pake)?;
                    expected_confirm = Some(confirmations.host);
                    response = (message, confirmations.client);
                }
                channel.send(&ControlMessage::PairingResponse {
                    signature,
                    pake: response.0,
                    confirm: response.1,
                })?;
            }
            other => {
                return Err(SessionError::Unexpected(match other {
                    ControlMessage::Bye => "expected HelloAck",
                    _ => "expected HelloAck or PairingChallenge",
                }));
            }
        }
    };
    if session_id != keys.session_id {
        return Err(SessionError::SessionIdMismatch);
    }
    if !SUPPORTED_VERSIONS.contains(&version) {
        return Err(ProtocolError::Negotiation("host chose an unsupported version").into());
    }
    if !capabilities.contains_all(agreed) {
        return Err(ProtocolError::Negotiation("host chose capabilities not offered").into());
    }
    if !paired {
        return Err(SessionError::NotPaired);
    }
    // The host signs this connection with the key it names, so the key the client pins is
    // the key of the peer it talks to, not one a third party copied.
    DeviceIdentity::verify_bytes(
        &host_key,
        &host_proof(&host_key, &device_key, binding),
        &signature,
    )
    .map_err(|_| SessionError::HostProof)?;
    let pairing_confirmed = match expected_confirm {
        Some(expected) if confirmation_matches(&expected, &confirm) => true,
        Some(_) => return Err(SessionError::HostProof),
        None => false,
    };
    Ok(ClientHello {
        version,
        capabilities: agreed,
        paired,
        host_key,
        pairing_confirmed,
    })
}

/// Host side of the hello: read `Hello`, negotiate, authorise the client and
/// answer `HelloAck`.
///
/// Returns the negotiated version, capabilities, and the client's device key.
///
/// A client the host does not trust yet is authorised by the pairing code,
/// when the policy is [`PairingMode::Prompt`]: both ends run SPAKE2 over the
/// code, bound to `binding`, and the client must send the matching
/// confirmation. The code itself never crosses the wire. When the policy is
/// [`PairingMode::Refuse`] an unknown client is turned away.
///
/// # Errors
/// Returns an error when the exchange fails, the versions or capabilities do
/// not overlap, or the client is not authorised.
pub fn host_hello(
    channel: &mut ControlChannel,
    keys: &SessionKeys,
    binding: &ChannelBinding,
    capabilities: Capabilities,
    policy: &PairingPolicy,
) -> Result<Authorised, SessionError> {
    let ControlMessage::Hello {
        version,
        capabilities: offered,
        device_key,
    } = channel.recv()?
    else {
        return Err(SessionError::Unexpected("expected Hello"));
    };
    let (agreed_version, agreed) = VersionHandshake::negotiate(
        &VersionHandshake {
            version: VERSION,
            capabilities,
        },
        &VersionHandshake {
            version,
            capabilities: offered,
        },
        SUPPORTED_VERSIONS,
    )?;
    let host_key = policy.identity.public_bytes();
    // Every answer carries the host's signature over this connection, so the client can
    // check the key it pinned.
    let ack = |paired: bool, confirm: [u8; 32]| ControlMessage::HelloAck {
        version: agreed_version,
        capabilities: agreed,
        session_id: keys.session_id,
        paired,
        host_key,
        confirm,
        signature: policy
            .identity
            .sign_bytes(&host_proof(&host_key, &device_key, binding)),
    };

    let trusted = policy.is_trusted(&device_key);
    // The client proves that it owns the identity key it claimed. Without
    // this, anyone who knew the public key could claim to be that client.
    let nonce = random_bytes::<32>();
    // The exchange runs only for a client the host would pair.
    let (pake, host_message) = if !trusted && policy.mode == PairingMode::Prompt {
        crate::host_log!(info
            "lumen-host: a new client asks to pair; type the code {} on the client",
            policy.code
        );
        if let Some(path) = &policy.code_file {
            write_code_file(path, policy.code);
        }
        let (state, message) = PairingPake::start(Role::Responder, policy.code, binding);
        (Some(state), message)
    } else {
        (None, [0u8; PAKE_MESSAGE_LEN])
    };
    channel.send(&ControlMessage::PairingChallenge {
        host_key,
        nonce,
        pake: host_message,
    })?;
    let ControlMessage::PairingResponse {
        signature,
        pake: client_message,
        confirm,
    } = channel.recv()?
    else {
        return Err(SessionError::Unexpected("expected PairingResponse"));
    };
    let proof = pairing_proof(&nonce, &host_key, &device_key, binding);
    if let Err(e) = DeviceIdentity::verify_bytes(&device_key, &proof, &signature) {
        crate::host_log!(warn "lumen-host: the client did not prove its identity: {e}");
        channel.send(&ack(false, [0u8; 32]))?;
        return Err(SessionError::NotPaired);
    }

    let mut host_confirm = [0u8; 32];
    if !trusted {
        let Some(pake) = pake else {
            channel.send(&ack(false, [0u8; 32]))?;
            return Err(SessionError::NotPaired);
        };
        // A wrong code, or a third party in the middle, gives different keys, so the
        // confirmations disagree.
        let confirmed = pake
            .finish(&client_message)
            .ok()
            .filter(|c| confirmation_matches(&c.client, &confirm));
        let Some(confirmations) = confirmed else {
            channel.send(&ack(false, [0u8; 32]))?;
            return Err(SessionError::PairingRefused);
        };
        host_confirm = confirmations.host;
    }

    channel.send(&ack(true, host_confirm))?;
    Ok(Authorised {
        version: agreed_version,
        capabilities: agreed,
        device_key,
        newly_paired: !trusted,
    })
}

/// What the host's answer to the client's hello was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientHello {
    /// The negotiated protocol version.
    pub version: u8,
    /// The negotiated capabilities.
    pub capabilities: Capabilities,
    /// Whether the client is authorised.
    pub paired: bool,
    /// The host's Ed25519 public key, which the host proved it owns on this connection. The
    /// client pins it, so a different host cannot take the place of the one it paired with.
    pub host_key: [u8; 32],
    /// Whether the host proved, in this session, that it knows the pairing code the user typed.
    pub pairing_confirmed: bool,
}

/// Write the pairing code to a file, so a script can read it. The code is a
/// secret until the pairing ends, so only the owner may read the file.
fn write_code_file(path: &std::path::Path, code: PairingCode) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = crate::pairing::write_owner_only(path, format!("{code}\n").as_bytes()) {
        crate::host_log!(warn "lumen-host: cannot write the pairing code file: {e}");
    }
}

/// What the host asks the client to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingChallenge {
    /// The host's Ed25519 public key, so the client can name the host.
    pub host_key: [u8; 32],
}

/// What the host decided about a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Authorised {
    /// The negotiated protocol version.
    pub version: u8,
    /// The negotiated capabilities.
    pub capabilities: Capabilities,
    /// The client's Ed25519 public key.
    pub device_key: [u8; 32],
    /// Whether the client was paired during this session.
    pub newly_paired: bool,
}

/// What the host does with a client it does not know yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingMode {
    /// Turn the client away. The client must be trusted out of band first.
    Refuse,
    /// Show a code and pair the client that proves it knows the code.
    Prompt,
}

/// How the host authorises clients.
#[derive(Debug, Clone)]
pub struct PairingPolicy {
    /// The host's identity, which signs every answer.
    pub identity: DeviceIdentity,
    /// The clients the host already trusts.
    pub trusted: Vec<[u8; 32]>,
    /// What to do with a client that is not trusted yet.
    pub mode: PairingMode,
    /// The code the host shows for this attempt. Make a fresh one with
    /// [`PairingCode::random`] for every connection.
    pub code: PairingCode,
    /// Where the host writes the code it shows, so another process can read
    /// it. `None` means the code is only printed.
    pub code_file: Option<std::path::PathBuf>,
}

impl PairingPolicy {
    /// Whether the host already trusts this client key.
    #[must_use]
    pub fn is_trusted(&self, device_key: &[u8; 32]) -> bool {
        self.trusted.contains(device_key)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::time::Duration;

    use lumen_crypto::DeviceIdentity;
    use lumen_protocol::Capabilities;

    use super::*;

    #[test]
    fn control_directions_use_different_keys() {
        let keys = SessionKeys::derive(b"secret");
        let (mut host_send, host_recv) = control_ciphers(&keys, Role::Responder, SESSION_AEAD);
        let (mut client_send, client_recv) = control_ciphers(&keys, Role::Initiator, SESSION_AEAD);
        let sealed = host_send.seal(&[], b"from host").expect("seal");
        assert!(client_recv.open(&[], &sealed, 0).is_ok());
        assert!(host_recv.open(&[], &sealed, 0).is_err());
        let back = client_send.seal(&[], b"from client").expect("seal");
        assert!(host_recv.open(&[], &back, 0).is_ok());
    }

    #[test]
    fn session_carries_control_and_media_over_sockets() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind control");
        let control_addr = listener.local_addr().expect("control addr");
        let client_media = UdpSocket::bind("127.0.0.1:0").expect("bind media");
        client_media
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("media timeout");
        let client_media_addr = client_media.local_addr().expect("media addr");
        let au: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let host_au = au.clone();
        // The client identity is made before the host thread starts, so the
        // host can trust it.
        let device = DeviceIdentity::generate().expect("device identity");
        let trusted = vec![device.public_bytes()];

        let host = std::thread::spawn(move || -> Result<(), SessionError> {
            let (mut stream, _) = listener.accept()?;
            let est = establish(&mut stream, Role::Responder, Capabilities::H264)?;
            let mut channel = control_channel(stream, &est, Role::Responder)?;
            let policy = PairingPolicy {
                identity: DeviceIdentity::generate().expect("host identity"),
                trusted,
                mode: PairingMode::Refuse,
                code: PairingCode::random(),
                code_file: None,
            };
            let authorised = host_hello(
                &mut channel,
                &est.keys,
                &est.binding,
                Capabilities::H264,
                &policy,
            )?;
            assert!(!authorised.newly_paired);
            let ControlMessage::Ping { ts_us } = channel.recv()? else {
                return Err(SessionError::Unexpected("expected Ping"));
            };
            assert_eq!(ts_us, 5);
            let mut sender = media_sender(&est.keys, est.aead)?;
            let socket = UdpSocket::bind("127.0.0.1:0")?;
            for datagram in sender.packetize(&host_au, 1234)? {
                socket.send_to(&datagram, client_media_addr)?;
            }
            channel.send(&ControlMessage::Bye)?;
            Ok(())
        });

        let mut stream = TcpStream::connect(control_addr).expect("connect control");
        let est =
            establish(&mut stream, Role::Initiator, Capabilities::H264).expect("client handshake");
        let mut channel = control_channel(stream, &est, Role::Initiator).expect("client channel");
        let mut answer = |_challenge: PairingChallenge| None;
        let hello = client_hello(
            &mut channel,
            &device,
            &est.keys,
            &est.binding,
            Capabilities::H264,
            &mut answer,
        )
        .expect("client hello");
        assert_eq!(hello.version, VERSION);
        assert_eq!(hello.capabilities, Capabilities::H264);
        assert!(hello.paired);
        channel
            .send(&ControlMessage::Ping { ts_us: 5 })
            .expect("ping");

        let mut receiver = media_receiver(&est.keys, est.aead);
        let mut buf = [0u8; 2048];
        let got = loop {
            let (n, _) = client_media
                .recv_from(&mut buf)
                .expect("media datagram before timeout");
            if let Some(au) = receiver
                .on_datagram(&buf[..n])
                .expect("authentic media datagram")
            {
                break au;
            }
        };
        assert_eq!(got.capture_ts_us, 1234);
        assert_eq!(got.data, au);
        assert_eq!(channel.recv().expect("bye"), ControlMessage::Bye);
        host.join()
            .expect("host thread panicked")
            .expect("host session");
    }

    /// The code the host of the pairing tests shows.
    const CODE: PairingCode = PairingCode::new(123_456);

    /// The connection as both ends see it when nothing sits between them.
    const DIRECT: ChannelBinding = ChannelBinding {
        client_static: [1u8; 32],
        host_static: [2u8; 32],
    };

    /// Runs the client side of the hello, answering a code request with `code`.
    fn hello(
        channel: &mut ControlChannel,
        identity: &DeviceIdentity,
        code: Option<PairingCode>,
    ) -> Result<ClientHello, SessionError> {
        let mut answer = |_challenge: PairingChallenge| code;
        client_hello(
            channel,
            identity,
            &client_keys(),
            &DIRECT,
            Capabilities::H264,
            &mut answer,
        )
    }

    /// A host that trusts the client lets it in without asking for a code.
    #[test]
    fn a_trusted_client_is_asked_for_no_code() {
        let identity = DeviceIdentity::generate().expect("identity");
        let (mut client, host) =
            hello_pair(vec![identity.public_bytes()], PairingMode::Prompt, DIRECT);
        let mut answer = |_challenge: PairingChallenge| -> Option<PairingCode> {
            panic!("a trusted client must not be asked for a code")
        };
        let hello = client_hello(
            &mut client,
            &identity,
            &client_keys(),
            &DIRECT,
            Capabilities::H264,
            &mut answer,
        )
        .expect("client hello");
        assert!(hello.paired);
        assert!(!hello.pairing_confirmed);
        let authorised = host.join().expect("host thread").expect("host hello");
        assert_eq!(authorised.device_key, identity.public_bytes());
        assert!(!authorised.newly_paired);
    }

    /// A host that does not trust the client and does not prompt refuses it.
    #[test]
    fn an_untrusted_client_is_refused() {
        let identity = DeviceIdentity::generate().expect("identity");
        let (mut client, host) = hello_pair(Vec::new(), PairingMode::Refuse, DIRECT);
        let result = hello(&mut client, &identity, Some(CODE));
        assert!(matches!(result, Err(SessionError::NotPaired)));
        assert!(matches!(
            host.join().expect("host thread"),
            Err(SessionError::NotPaired)
        ));
    }

    /// A host that prompts pairs a client that knows the code, and proves it knows it too.
    #[test]
    fn the_right_code_pairs_the_client() {
        let identity = DeviceIdentity::generate().expect("identity");
        let (mut client, host) = hello_pair(Vec::new(), PairingMode::Prompt, DIRECT);
        let hello = hello(&mut client, &identity, Some(CODE)).expect("client hello");
        assert!(hello.paired);
        assert!(hello.pairing_confirmed);
        let authorised = host.join().expect("host thread").expect("host hello");
        assert!(authorised.newly_paired);
        assert_eq!(authorised.device_key, identity.public_bytes());
    }

    /// A host that prompts turns away a client with a wrong code, or with none.
    #[test]
    fn a_wrong_or_missing_code_is_refused() {
        for code in [Some(PairingCode::new(999_999)), None] {
            let identity = DeviceIdentity::generate().expect("identity");
            let (mut client, host) = hello_pair(Vec::new(), PairingMode::Prompt, DIRECT);
            let result = hello(&mut client, &identity, code);
            assert!(matches!(result, Err(SessionError::NotPaired)));
            assert!(matches!(
                host.join().expect("host thread"),
                Err(SessionError::PairingRefused)
            ));
        }
    }

    /// A relay between the two ends holds two connections. Even with the right code, the
    /// client's proof is bound to the connection it sees, so the host refuses it.
    #[test]
    fn a_relay_with_the_right_code_cannot_pair() {
        let identity = DeviceIdentity::generate().expect("identity");
        let host_view = ChannelBinding {
            client_static: [3u8; 32],
            ..DIRECT
        };
        let (mut client, host) = hello_pair(Vec::new(), PairingMode::Prompt, host_view);
        let result = hello(&mut client, &identity, Some(CODE));
        assert!(matches!(result, Err(SessionError::NotPaired)));
        assert!(matches!(
            host.join().expect("host thread"),
            Err(SessionError::NotPaired)
        ));
    }

    /// A client that claims a trusted key but cannot prove it is refused.
    #[test]
    fn a_client_that_cannot_prove_its_identity_is_refused() {
        let claimed = [7u8; 32];
        let liar = DeviceIdentity::generate().expect("identity");
        let (mut client, host) = hello_pair(vec![claimed], PairingMode::Prompt, DIRECT);

        client
            .send(&ControlMessage::Hello {
                version: VERSION,
                capabilities: Capabilities::H264,
                device_key: claimed,
            })
            .expect("hello");
        let ControlMessage::PairingChallenge {
            nonce, host_key, ..
        } = client.recv().expect("challenge")
        else {
            panic!("expected a pairing challenge");
        };
        // The signature is made with the wrong key.
        let signature = liar.sign_bytes(&pairing_proof(&nonce, &host_key, &claimed, &DIRECT));
        client
            .send(&ControlMessage::PairingResponse {
                signature,
                pake: [0u8; PAKE_MESSAGE_LEN],
                confirm: [0u8; 32],
            })
            .expect("response");

        let result = host.join().expect("host thread");
        assert!(matches!(result, Err(SessionError::NotPaired)));
    }

    /// A host that names a key it cannot sign with is refused, so a client never pins it.
    #[test]
    fn a_host_that_cannot_prove_its_key_is_refused() {
        let identity = DeviceIdentity::generate().expect("identity");
        let named = DeviceIdentity::generate()
            .expect("named host")
            .public_bytes();
        let liar = DeviceIdentity::generate().expect("liar");
        let mut client = fake_host(CODE, liar, named);
        let result = hello(&mut client, &identity, Some(CODE));
        assert!(matches!(result, Err(SessionError::HostProof)));
    }

    /// A host must prove it knows the code: one that does not is refused, even when it
    /// signs with its own key.
    #[test]
    fn a_host_that_does_not_know_the_code_is_refused() {
        for (host_code, accepted) in [(CODE, true), (PairingCode::new(1), false)] {
            let identity = DeviceIdentity::generate().expect("identity");
            let host = DeviceIdentity::generate().expect("host");
            let key = host.public_bytes();
            let mut client = fake_host(host_code, host, key);
            let result = hello(&mut client, &identity, Some(CODE));
            if accepted {
                assert!(result.expect("the host knows the code").pairing_confirmed);
            } else {
                assert!(matches!(result, Err(SessionError::HostProof)));
            }
        }
    }

    /// A host that runs the pairing exchange with `code`, names `host_key`, signs with
    /// `signer`, and always says the client is paired.
    fn fake_host(code: PairingCode, signer: DeviceIdentity, host_key: [u8; 32]) -> ControlChannel {
        let (client, mut host) = channel_pair();
        std::thread::spawn(move || {
            let Ok(ControlMessage::Hello { device_key, .. }) = host.recv() else {
                return;
            };
            let (pake, message) = PairingPake::start(Role::Responder, code, &DIRECT);
            let _ = host.send(&ControlMessage::PairingChallenge {
                host_key,
                nonce: [0u8; 32],
                pake: message,
            });
            let Ok(ControlMessage::PairingResponse { pake: theirs, .. }) = host.recv() else {
                return;
            };
            let confirm = pake.finish(&theirs).map_or([0u8; 32], |c| c.host);
            let _ = host.send(&ControlMessage::HelloAck {
                version: VERSION,
                capabilities: Capabilities::H264,
                session_id: client_keys().session_id,
                paired: true,
                host_key,
                confirm,
                signature: signer.sign_bytes(&host_proof(&host_key, &device_key, &DIRECT)),
            });
        });
        client
    }

    /// The keys both ends of the pairing tests use.
    fn client_keys() -> SessionKeys {
        SessionKeys::derive(b"pairing-test")
    }

    /// A connected pair of encrypted control channels over one TCP connection: client, host.
    fn channel_pair() -> (ControlChannel, ControlChannel) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind control");
        let addr = listener.local_addr().expect("control addr");
        let client_stream = TcpStream::connect(addr).expect("connect control");
        let (host_stream, _) = listener.accept().expect("accept control");

        let mut host = ControlChannel::new(host_stream).expect("host channel");
        let mut client = ControlChannel::new(client_stream).expect("client channel");
        let (send, recv) = control_ciphers(&client_keys(), Role::Initiator, SESSION_AEAD);
        client.set_cipher(send, recv);
        let (send, recv) = control_ciphers(&client_keys(), Role::Responder, SESSION_AEAD);
        host.set_cipher(send, recv);
        (client, host)
    }

    /// The client end of a channel pair whose host end already runs `host_hello` on its own
    /// thread, with `binding` as the host sees the connection.
    fn hello_pair(
        trusted: Vec<[u8; 32]>,
        mode: PairingMode,
        binding: ChannelBinding,
    ) -> (
        ControlChannel,
        std::thread::JoinHandle<Result<Authorised, SessionError>>,
    ) {
        let (client, mut host) = channel_pair();
        let policy = PairingPolicy {
            identity: DeviceIdentity::generate().expect("host identity"),
            trusted,
            mode,
            code: CODE,
            code_file: None,
        };
        let keys = client_keys();
        let handle = std::thread::spawn(move || {
            host_hello(&mut host, &keys, &binding, Capabilities::H264, &policy)
        });
        (client, handle)
    }
}

#[cfg(test)]
mod aead_tests {
    use super::*;

    #[test]
    fn a_machine_with_aes_uses_aes() {
        let caps = Capabilities::H264;
        assert_eq!(negotiate_aead(caps, caps), AeadKind::Aes256Gcm);
    }

    #[test]
    fn a_peer_without_aes_asks_for_chacha() {
        let with_aes = Capabilities::H264;
        let without_aes = Capabilities::H264 | Capabilities::AEAD_CHACHA20;
        // Either end asking is enough.
        assert_eq!(
            negotiate_aead(with_aes, without_aes),
            AeadKind::ChaCha20Poly1305
        );
        assert_eq!(
            negotiate_aead(without_aes, with_aes),
            AeadKind::ChaCha20Poly1305
        );
    }

    #[test]
    fn the_capability_bitmap_survives_the_payload() {
        let caps = Capabilities::H264 | Capabilities::AEAD_CHACHA20 | Capabilities::REQUEST_IDR;
        let payload = capability_payload(caps);
        assert_eq!(parse_capability_payload(&payload), caps);
        // A short payload is not a capability bitmap.
        assert_eq!(parse_capability_payload(&[0, 1]), Capabilities::empty());
    }
}
