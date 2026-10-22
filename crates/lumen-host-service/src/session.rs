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
    AeadKind, CryptoError, DeviceIdentity, Handshake, PacketCipher, PairingCode, Role, SessionKeys,
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
}

/// A completed handshake: the session keys and the peer's static public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Established {
    /// Keys and identifier of the session.
    pub keys: SessionKeys,
    /// The peer's static public key, as authenticated by the handshake.
    pub remote_static: Option<[u8; 32]>,
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
    Ok(Established {
        keys: SessionKeys::derive(&result.key_material),
        remote_static: result.remote_static,
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
/// id and the negotiated version and capabilities.
///
/// The host may answer with a pairing challenge instead of `HelloAck`. The
/// `answer` callback is then called with the code the host shows; it returns
/// the code to send back, or `None` to refuse the pairing.
///
/// # Errors
/// Returns an error when the exchange fails or the host's answer is invalid.
pub fn client_hello(
    channel: &mut ControlChannel,
    identity: &DeviceIdentity,
    keys: &SessionKeys,
    capabilities: Capabilities,
    answer: &mut dyn FnMut(PairingChallenge) -> Option<PairingCode>,
) -> Result<(u8, Capabilities, bool), SessionError> {
    let device_key = identity.public_bytes();
    channel.send(&ControlMessage::Hello {
        version: VERSION,
        capabilities,
        device_key,
    })?;
    let (version, agreed, session_id, paired) = loop {
        match channel.recv()? {
            ControlMessage::HelloAck {
                version,
                capabilities: agreed,
                session_id,
                paired,
            } => break (version, agreed, session_id, paired),
            ControlMessage::PairingChallenge {
                code,
                host_key,
                nonce,
            } => {
                // The signature proves this client owns the identity key it
                // claimed. The code is what the user reads on the host.
                let message = pairing_proof(&nonce, &host_key, &device_key);
                let signature = identity.sign_bytes(&message);
                let challenge = PairingChallenge {
                    code: PairingCode::new(code),
                    host_key,
                };
                let response = match answer(challenge) {
                    Some(code) => code.value(),
                    None => 0,
                };
                channel.send(&ControlMessage::PairingResponse {
                    code: response,
                    signature,
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
    Ok((version, agreed, paired))
}

/// Host side of the hello: read `Hello`, negotiate, authorise the client and
/// answer `HelloAck`.
///
/// Returns the negotiated version, capabilities, and the client's device key.
///
/// A client the host does not trust yet is authorised by the pairing code,
/// when the policy is [`PairingMode::Prompt`]. The code is bound to the
/// client's Noise static key, so it is valid only for this handshake. When
/// the policy is [`PairingMode::Refuse`] an unknown client is turned away.
///
/// # Errors
/// Returns an error when the exchange fails, the versions or capabilities do
/// not overlap, or the client is not authorised.
pub fn host_hello(
    channel: &mut ControlChannel,
    keys: &SessionKeys,
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

    let mut newly_paired = false;
    let trusted = policy.is_trusted(&device_key);
    // The client proves that it owns the identity key it claimed. Without
    // this, anyone who knew the public key could claim to be that client.
    let nonce = random_bytes::<32>();
    let code = policy.challenge(&device_key);
    if let Some(path) = &policy.code_file {
        let _ = write_code_file(path, code);
    }
    channel.send(&ControlMessage::PairingChallenge {
        code: code.value(),
        host_key: policy.host_public,
        nonce,
    })?;
    let ControlMessage::PairingResponse {
        code: answer,
        signature,
    } = channel.recv()?
    else {
        return Err(SessionError::Unexpected("expected PairingResponse"));
    };
    let proof = pairing_proof(&nonce, &policy.host_public, &device_key);
    if let Err(e) = DeviceIdentity::verify_bytes(&device_key, &proof, &signature) {
        crate::host_log!(warn "lumen-host: the client did not prove its identity: {e}");
        channel.send(&ControlMessage::HelloAck {
            version: agreed_version,
            capabilities: agreed,
            session_id: keys.session_id,
            paired: false,
        })?;
        return Err(SessionError::NotPaired);
    }

    if !trusted {
        match policy.mode {
            PairingMode::Refuse => {
                channel.send(&ControlMessage::HelloAck {
                    version: agreed_version,
                    capabilities: agreed,
                    session_id: keys.session_id,
                    paired: false,
                })?;
                return Err(SessionError::NotPaired);
            }
            PairingMode::Prompt => {
                if answer != code.value() {
                    channel.send(&ControlMessage::HelloAck {
                        version: agreed_version,
                        capabilities: agreed,
                        session_id: keys.session_id,
                        paired: false,
                    })?;
                    return Err(SessionError::PairingRefused);
                }
                newly_paired = true;
            }
        }
    }

    channel.send(&ControlMessage::HelloAck {
        version: agreed_version,
        capabilities: agreed,
        session_id: keys.session_id,
        paired: true,
    })?;
    Ok(Authorised {
        version: agreed_version,
        capabilities: agreed,
        device_key,
        newly_paired,
    })
}

/// Write the pairing code to a file, so a script can read it.
fn write_code_file(path: &std::path::Path, code: PairingCode) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{code}\n"))
}

/// What the host asks the client to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingChallenge {
    /// The code the host shows on its own screen.
    pub code: PairingCode,
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
    /// Show a code and wait for the client to send it back.
    Prompt,
}

/// How the host authorises clients.
#[derive(Debug, Clone)]
pub struct PairingPolicy {
    /// The host's Ed25519 public key.
    pub host_public: [u8; 32],
    /// The clients the host already trusts.
    pub trusted: Vec<[u8; 32]>,
    /// What to do with a client that is not trusted yet.
    pub mode: PairingMode,
    /// The nonce the pairing code is bound to. It is the peer's Noise static
    /// key, so it changes with every handshake.
    pub client_nonce: [u8; 32],
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
    /// The pairing code for this handshake attempt.
    #[must_use]
    pub fn challenge(&self, _device_key: &[u8; 32]) -> PairingCode {
        PairingCode::derive(&self.host_public, &self.client_nonce)
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
                host_public: [9u8; 32],
                trusted,
                mode: PairingMode::Refuse,
                client_nonce: est.remote_static.unwrap_or([0u8; 32]),
                code_file: None,
            };
            let authorised = host_hello(&mut channel, &est.keys, Capabilities::H264, &policy)?;
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
        let (version, caps, paired) = client_hello(
            &mut channel,
            &device,
            &est.keys,
            Capabilities::H264,
            &mut answer,
        )
        .expect("client hello");
        assert_eq!(version, VERSION);
        assert_eq!(caps, Capabilities::H264);
        assert!(paired);
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

    /// A host that trusts the client lets it in without a code.
    #[test]
    fn a_trusted_client_needs_no_code() {
        let identity = DeviceIdentity::generate().expect("identity");
        let (mut client, host) = hello_pair(vec![identity.public_bytes()], PairingMode::Refuse);
        let mut answer = |_challenge: PairingChallenge| None;
        let (_, _, paired) = client_hello(
            &mut client,
            &identity,
            &client_keys(),
            Capabilities::H264,
            &mut answer,
        )
        .expect("client hello");
        assert!(paired);
        let authorised = host.join().expect("host thread").expect("host hello");
        assert_eq!(authorised.device_key, identity.public_bytes());
        assert!(!authorised.newly_paired);
    }

    /// A host that does not trust the client refuses it.
    #[test]
    fn an_untrusted_client_is_refused() {
        let identity = DeviceIdentity::generate().expect("identity");
        let (mut client, host) = hello_pair(Vec::new(), PairingMode::Refuse);
        let mut answer = |_challenge: PairingChallenge| Some(PairingCode::new(1));
        let result = client_hello(
            &mut client,
            &identity,
            &client_keys(),
            Capabilities::H264,
            &mut answer,
        );
        assert!(matches!(result, Err(SessionError::NotPaired)));
        assert!(host.join().expect("host thread").is_err());
    }

    /// A host that prompts pairs a client that answers with the right code.
    #[test]
    fn the_right_code_pairs_the_client() {
        let identity = DeviceIdentity::generate().expect("identity");
        let (mut client, host) = hello_pair(Vec::new(), PairingMode::Prompt);
        let mut answer = |challenge: PairingChallenge| Some(challenge.code);
        let (_, _, paired) = client_hello(
            &mut client,
            &identity,
            &client_keys(),
            Capabilities::H264,
            &mut answer,
        )
        .expect("client hello");
        assert!(paired);
        let authorised = host.join().expect("host thread").expect("host hello");
        assert!(authorised.newly_paired);
        assert_eq!(authorised.device_key, identity.public_bytes());
    }

    /// A host that prompts turns away a client that answers with a wrong code.
    #[test]
    fn a_wrong_code_is_refused() {
        let identity = DeviceIdentity::generate().expect("identity");
        let (mut client, host) = hello_pair(Vec::new(), PairingMode::Prompt);
        let mut answer = |_challenge: PairingChallenge| Some(PairingCode::new(999_999));
        let result = client_hello(
            &mut client,
            &identity,
            &client_keys(),
            Capabilities::H264,
            &mut answer,
        );
        assert!(matches!(result, Err(SessionError::NotPaired)));
        assert!(host.join().expect("host thread").is_err());
    }

    /// A client that claims a trusted key but cannot prove it is refused.
    #[test]
    fn a_client_that_cannot_prove_its_identity_is_refused() {
        let claimed = [7u8; 32];
        let liar = DeviceIdentity::generate().expect("identity");
        let (mut client, host) = hello_pair(vec![claimed], PairingMode::Prompt);

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
        let signature = liar.sign_bytes(&pairing_proof(&nonce, &host_key, &claimed));
        client
            .send(&ControlMessage::PairingResponse { code: 0, signature })
            .expect("response");

        let result = host.join().expect("host thread");
        assert!(matches!(result, Err(SessionError::NotPaired)));
    }

    /// The keys both ends of the pairing tests use.
    fn client_keys() -> SessionKeys {
        SessionKeys::derive(b"pairing-test")
    }

    /// A connected pair of control channels over one TCP connection, with the
    /// host side already running `host_hello` on its own thread.
    fn hello_pair(
        trusted: Vec<[u8; 32]>,
        mode: PairingMode,
    ) -> (
        ControlChannel,
        std::thread::JoinHandle<Result<Authorised, SessionError>>,
    ) {
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

        let policy = PairingPolicy {
            host_public: [9u8; 32],
            trusted,
            mode,
            client_nonce: [5u8; 32],
            code_file: None,
        };
        let keys = client_keys();
        let handle =
            std::thread::spawn(move || host_hello(&mut host, &keys, Capabilities::H264, &policy));
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
