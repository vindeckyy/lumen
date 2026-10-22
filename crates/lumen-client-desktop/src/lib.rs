//! Lumen desktop client.
//!
//! Receives sealed media datagrams, decodes them and presents the frames.
//!
//! The stages are the mirror image of the host: a receive stage, a decode
//! and present stage, and a control stage.
//!
//! ```text
//!   receive  --access unit-->  decode + present
//!      ^                            |
//!      +--------- control ---------+
//! ```

#![forbid(unsafe_code)]

pub mod audio;
pub mod backend;
pub mod clipboard;
pub mod events;
pub mod input;
pub mod ladder;
pub mod lowpower;
pub mod monitors;
pub mod present;
pub mod receive;
pub mod reconnect;

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use lumen_crypto::{DeviceIdentity, PacketCipher, PairingCode, Role};
use lumen_host_service::control::{ControlChannel, ControlMessage};
use lumen_host_service::pairing::{PairingError, load_identity};
use lumen_host_service::session::{self, PairingChallenge};
use lumen_protocol::Capabilities;
use lumen_render::{RenderBackend, Renderer};
use lumen_transport::socket::{MediaSocket, SocketOptions};

use crate::events::EventLog;
use crate::present::PresentStage;
use crate::receive::{ReceiveStage, monotonic_us};
use crate::reconnect::{ReconnectPolicy, Reconnector};

/// Default control port, from `docs/protocol.md`.
pub const DEFAULT_CONTROL_PORT: u16 = 24800;
/// Default media port, from `docs/protocol.md`.
pub const DEFAULT_MEDIA_PORT: u16 = 24801;

/// Configuration of the client pipeline.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Address of the host's control listener.
    pub host: SocketAddr,
    /// Address the media socket binds to. The host sends media to this port.
    pub media_addr: SocketAddr,
    /// Window title.
    pub title: String,
    /// Which display server to present to. `None` picks from the environment.
    pub backend: Option<RenderBackend>,
    /// Stop after this long. `None` runs until the host leaves.
    pub max_duration: Option<Duration>,
    /// Where the client keeps its Ed25519 identity key.
    pub identity: std::path::PathBuf,
    /// The pairing code to answer with, when it is already known.
    pub pairing_code: Option<u32>,
    /// A file the pairing code is read from, when the host writes one.
    pub pairing_code_file: Option<std::path::PathBuf>,
    /// Whether the client comes back on its own when the session ends.
    ///
    /// An unattended client has nobody to press a key, so it retries with
    /// the backoff of the [`reconnect`] module.
    pub reconnect: bool,
    /// Where the client writes its events, one JSON object per line.
    ///
    /// The scripts read the file to learn when a session was established.
    pub events: Option<std::path::PathBuf>,
    /// The low-power profile of the client.
    pub low_power: bool,
    /// Whether the client sends the keyboard and pointer of its window to the host.
    pub input: bool,
    /// Address the audio socket binds to. The host sends audio to this port.
    pub audio_addr: SocketAddr,
    /// Whether the session carries audio.
    pub audio: bool,
    /// The audio stream parameters.
    pub audio_config: lumen_audio::AudioConfig,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: SocketAddr::from(([127, 0, 0, 1], DEFAULT_CONTROL_PORT)),
            media_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            title: "Lumen".to_string(),
            backend: None,
            max_duration: None,
            identity: default_state_dir().join("client-identity"),
            pairing_code: None,
            pairing_code_file: None,
            reconnect: false,
            events: None,
            low_power: false,
            input: true,
            audio_addr: SocketAddr::from(([0, 0, 0, 0], 24802)),
            audio: false,
            audio_config: lumen_audio::AudioConfig {
                sample_rate: 48_000,
                channels: 2,
                frame_ms: 50,
                bitrate_bps: 128_000,
                use_opus: true,
            },
        }
    }
}

/// Errors from starting or running the client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// A socket could not be set up.
    #[error("socket setup failed: {0}")]
    Io(#[from] std::io::Error),
    /// The session could not be established.
    #[error("session failed: {0}")]
    Session(#[from] session::SessionError),
    /// The renderer could not be opened.
    #[error("renderer failed: {0}")]
    Render(String),
    /// The client identity could not be read or created.
    #[error("identity failed: {0}")]
    Identity(#[from] PairingError),
    /// The decoder could not be created.
    #[error("decoder failed: {0}")]
    Decode(String),
}

/// Counters the stages share.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    datagrams_received: AtomicU64,
    access_units: AtomicU64,
    frames_decoded: AtomicU64,
    frames_presented: AtomicU64,
    frames_zero_copy: AtomicU64,
    decode_errors: AtomicU64,
    nacks_sent: AtomicU64,
    idr_requests: AtomicU64,
    frames_rebuilt: AtomicU64,
    frames_dropped: AtomicU64,
    duration_ms: AtomicU64,
    audio_played: AtomicU64,
    audio_dropped: AtomicU64,
    clipboard_roundtrips: AtomicU64,
    a_vs_skew_us: Mutex<Vec<i64>>,
    audio_datagrams: AtomicU64,
    audio_auth_failures: AtomicU64,
    audio_protocol_errors: AtomicU64,
    audio_stage_played: AtomicU64,
    audio_stage_dropped: AtomicU64,
    audio_playback: Mutex<lumen_audio::PlaybackStats>,
}

/// How often the client reports the link to the host.
///
/// The host's parity and bitrate policies react to this report, so it has to
/// be frequent enough that a link that gets worse is noticed before the
/// frames start to go.
const LINK_FEEDBACK_INTERVAL: Duration = Duration::from_millis(100);
/// How long the client waits for a ping from the host before it calls the
/// link gone.
///
/// A network that drops the packets does not close the connection, so the
/// client has to notice the silence itself. The host pings every 500 ms, so
/// three missed pings mean the link is gone.
const PING_TIMEOUT: Duration = Duration::from_millis(1500);

/// The client's Ed25519 identity, used to prove who it is to the host.
pub type Identity = DeviceIdentity;

/// A snapshot of the client counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClientStats {
    /// Datagrams read from the media socket.
    pub datagrams_received: u64,
    /// Access units reassembled from the slices.
    pub access_units: u64,
    /// Frames the decoder produced.
    pub frames_decoded: u64,
    /// Frames handed to the compositor.
    pub frames_presented: u64,
    /// Frames presented without a CPU copy: the decoder's buffer or a GPU-made one.
    pub zero_copy: u64,
    /// Frames the decoder could not decode.
    pub decode_errors: u64,
    /// NACKs sent to the host.
    pub nacks_sent: u64,
    /// On-demand keyframes requested from the host.
    pub idr_requests: u64,
    /// Access units rebuilt from FEC parity.
    pub frames_rebuilt: u64,
    /// Audio frames played.
    pub audio_played: u64,
    /// Audio frames dropped because they were late.
    pub audio_dropped: u64,
    /// Clipboard transfers that completed.
    pub clipboard_roundtrips: u64,
    /// How far the audio was from the video frame it aligns to, in
    /// microseconds. One sample per played frame.
    pub a_vs_skew_us: Vec<i64>,
    /// Audio datagrams the client received.
    pub audio_datagrams: u64,
    /// Audio datagrams that did not authenticate.
    pub audio_auth_failures: u64,
    /// Audio frames the receiver refused.
    pub audio_protocol_errors: u64,
    /// Audio frames the audio stage played.
    pub audio_stage_played: u64,
    /// Audio frames the audio stage dropped.
    pub audio_stage_dropped: u64,
    /// The playback scheduler's counters: scheduled, dropped late, dropped
    /// without a video clock, dropped without a clock offset.
    pub audio_playback: lumen_audio::PlaybackStats,
    /// Access units dropped because the decode stage was busy.
    pub frames_dropped: u64,
    /// How long the client ran, in milliseconds.
    pub duration_ms: u64,
}

/// The client pipeline.
#[derive(Debug)]
pub struct ClientPipeline {
    config: ClientConfig,
    counters: Arc<Counters>,
}

impl ClientPipeline {
    /// Create a pipeline with the given configuration.
    #[must_use]
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            counters: Arc::new(Counters::default()),
        }
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> ClientStats {
        snapshot(&self.counters)
    }

    /// Connect to the host and present its frames until the host leaves or the
    /// configured duration elapses.
    ///
    /// With [`ClientConfig::reconnect`] the client comes back on its own
    /// when the session ends or fails, with the backoff of the
    /// [`reconnect`] module.
    ///
    /// # Errors
    /// Returns an error when a socket, the renderer, the decoder or the
    /// session cannot be set up.
    pub fn run(&self) -> Result<ClientStats, ClientError> {
        let config = &self.config;
        let counters = Arc::clone(&self.counters);
        let started = Instant::now();
        let deadline = config.max_duration.map(|d| started + d);
        let events = EventLog::new(config.events.clone());
        let mut reconnector = Reconnector::new(ReconnectPolicy::default());
        let mut failure: Option<ClientError> = None;

        loop {
            match self.run_session(started, deadline, &events) {
                Ok(()) => {
                    if !config.reconnect {
                        break;
                    }
                    // The session ended. An unattended client comes back.
                    if deadline.is_none_or(|d| Instant::now() >= d) {
                        break;
                    }
                    let delay = reconnector.record_failure();
                    events.write(
                        "session-ended",
                        None,
                        Some(format!("{} ms", delay.as_millis())),
                    );
                    thread::sleep(delay);
                }
                Err(error) => {
                    let retry = config.reconnect
                        && reconnector.should_retry()
                        && deadline.is_none_or(|d| Instant::now() < d);
                    if !retry {
                        failure = Some(error);
                        break;
                    }
                    let delay = reconnector.record_failure();
                    events.write("connect-failed", None, Some(error.to_string()));
                    thread::sleep(delay);
                }
            }
        }

        counters
            .duration_ms
            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        match failure {
            Some(error) => Err(error),
            None => Ok(self.stats()),
        }
    }

    /// Run one session, from the connect to the end of the stream.
    fn run_session(
        &self,
        started: Instant,
        deadline: Option<Instant>,
        events: &EventLog,
    ) -> Result<(), ClientError> {
        let config = &self.config;
        let counters = Arc::clone(&self.counters);

        events.count_attempt();
        events.write("connecting", None, None);
        // The media socket binds before the control connection, so the first
        // datagrams the host sends are not lost.
        let socket = MediaSocket::bind(config.media_addr, SocketOptions::default())?;

        let identity = load_identity(&config.identity)?;

        let mut stream = TcpStream::connect(config.host)?;
        let connect_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let established = session::establish(&mut stream, Role::Initiator, client_capabilities())?;
        let mut channel = session::control_channel(stream, &established, Role::Initiator)?;
        let mut answer = |challenge: PairingChallenge| -> Option<PairingCode> {
            answer_challenge(config, challenge)
        };
        let (_version, _caps, _paired) = session::client_hello(
            &mut channel,
            &identity,
            &established.keys,
            client_capabilities(),
            &mut answer,
        )?;
        events.count_session();
        events.write("established", Some(connect_ms), None);

        let backend = config.backend.unwrap_or_else(default_backend);
        let renderer = Renderer::new(backend, &config.title).map_err(ClientError::Render)?;
        // Input is read from the window the renderer presents in, so its id is taken before the
        // renderer moves into the present stage.
        let input_window = renderer.x11_window();

        let running = Arc::new(AtomicBool::new(true));
        // The link accounting the host's estimator needs.
        let link = Arc::new(crate::receive::LinkAccounting::default());
        // The video clock the audio is placed on. The present stage writes
        // it and the audio stage reads it.
        let video_clock = Arc::new(Mutex::new(None::<(u64, u64)>));
        let clock_offset = Arc::new(Mutex::new(lumen_audio::ClockOffset::unmeasured()));
        // The quality ladder: the client asks for a smaller picture when it
        // cannot keep up with the one it has.
        let ladder = Arc::new(Mutex::new(crate::ladder::Ladder::new()));
        let (au_tx, au_rx) = std::sync::mpsc::sync_channel::<receive::AccessUnit>(1);
        let (request_tx, request_rx) = std::sync::mpsc::sync_channel::<ControlMessage>(8);

        let receive_handle = {
            let stage = ReceiveStage {
                socket,
                keys: established.keys,
                aead: established.aead,
                access_units: au_tx,
                requests: request_tx.clone(),
                running: Arc::clone(&running),
                counters: Arc::clone(&counters),
                link: Arc::clone(&link),
            };
            thread::spawn(move || stage.run())
        };

        let present_handle = {
            let stage = PresentStage {
                renderer,
                access_units: au_rx,
                requests: request_tx,
                running: Arc::clone(&running),
                counters: Arc::clone(&counters),
                events: events.clone(),
                video_clock: Arc::clone(&video_clock),
                ladder: Arc::clone(&ladder),
                plan: None,
                consecutive_errors: 0,
                converter: None,
            };
            thread::spawn(move || stage.run())
        };

        let audio_handle = if config.audio {
            match Self::start_audio(
                config,
                &established,
                Arc::clone(&running),
                Arc::clone(&counters),
                Arc::clone(&video_clock),
                Arc::clone(&clock_offset),
            ) {
                Ok(handle) => Some(handle),
                Err(e) => {
                    eprintln!("lumen-client: no audio: {e}");
                    None
                }
            }
        } else {
            None
        };

        // Input leaves on its own socket, sealed with the client-to-host media key. The host
        // listens on the fixed input port of its address.
        let input_handle = if config.input {
            let local = SocketAddr::new(config.media_addr.ip(), 0);
            let peer = SocketAddr::new(config.host.ip(), lumen_host_service::input::INPUT_PORT);
            let cipher = PacketCipher::new(
                established.aead,
                established.keys.media_c2h,
                established.keys.session_id,
            );
            input::spawn(local, peer, cipher, Arc::clone(&running), input_window)
        } else {
            None
        };

        let control_handle = {
            let stage = ControlStage {
                channel,
                requests: request_rx,
                running: Arc::clone(&running),
                counters: Arc::clone(&counters),
                events: events.clone(),
                link: Arc::clone(&link),
                clock_offset: Arc::clone(&clock_offset),
                ladder: Arc::clone(&ladder),
            };
            thread::spawn(move || stage.run())
        };

        loop {
            if control_handle.is_finished() {
                break;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        running.store(false, Ordering::SeqCst);

        let _ = receive_handle.join();
        let _ = present_handle.join();
        if let Some(audio) = audio_handle {
            let _ = audio.join();
        }
        if let Some(input) = input_handle {
            let _ = input.join();
        }
        drop(control_handle);

        Ok(())
    }
}

/// Answer a pairing challenge from the host.
///
/// The code the host shows must be typed here. When the code is already
/// known it comes from the command line; otherwise it is read from the file
/// the host writes, or from the terminal.
fn answer_challenge(config: &ClientConfig, challenge: PairingChallenge) -> Option<PairingCode> {
    eprintln!(
        "lumen-client: the host {} asks to pair. Its code is {}.",
        hex_key(&challenge.host_key),
        challenge.code
    );
    if let Some(value) = config.pairing_code {
        return Some(PairingCode::new(value));
    }
    if let Some(path) = &config.pairing_code_file {
        return read_code_file(path);
    }
    eprint!("lumen-client: type the code the host shows: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => PairingCode::parse(line.trim()).ok(),
    }
}

/// Read a pairing code from a file the host writes.
///
/// The file may not be there yet when the challenge arrives, so it is
/// polled for a short while.
fn read_code_file(path: &std::path::Path) -> Option<PairingCode> {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(code) = PairingCode::parse(text.trim())
        {
            return Some(code);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "lumen-client: no pairing code appeared in {}",
        path.display()
    );
    None
}

/// Format a public key as hex for display.
fn hex_key(key: &[u8; 32]) -> String {
    lumen_host_service::pairing::hex_encode(key)
}

/// The directory the client keeps its state in.
fn default_state_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME")
        && !dir.is_empty()
    {
        return std::path::PathBuf::from(dir).join("lumen");
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => std::path::PathBuf::from(home).join(".local/state/lumen"),
        _ => std::path::PathBuf::from(".lumen"),
    }
}

impl ClientPipeline {
    /// Start the audio stage.
    ///
    /// The audio arrives on a socket of its own and is opened with the audio
    /// key, so it can never collide with the video's nonces.
    fn start_audio(
        config: &ClientConfig,
        established: &session::Established,
        running: Arc<AtomicBool>,
        counters: Arc<Counters>,
        video_clock: Arc<Mutex<Option<(u64, u64)>>>,
        clock_offset: Arc<Mutex<lumen_audio::ClockOffset>>,
    ) -> Result<thread::JoinHandle<()>, String> {
        let socket = MediaSocket::bind(config.audio_addr, SocketOptions::default())
            .map_err(|e| e.to_string())?;
        let cipher = PacketCipher::new(
            established.aead,
            established.keys.audio_h2c,
            established.keys.session_id,
        );
        let mut stage = crate::audio::AudioStage::new(config.audio_config, socket, cipher)?;
        Ok(thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                // The video clock and the offset are read first, so the audio
                // is placed against the frame that is on screen.
                // The offset comes first: setting it keeps the scheduler,
                // and the video frame goes into it afterwards.
                if let Ok(offset) = clock_offset.lock() {
                    stage.set_clock_offset(*offset);
                }
                if let Ok(clock) = video_clock.lock()
                    && let Some((capture_ts, present_ts)) = *clock
                {
                    stage.set_video_frame(capture_ts, present_ts);
                    // The A/V skew of the audio frames whose video frame has
                    // now been presented.
                    stage.measure_skew(capture_ts);
                }
                match stage.pump() {
                    Ok(frames) => {
                        if frames > 0 {
                            counters
                                .audio_played
                                .fetch_add(frames as u64, Ordering::Relaxed);
                        }
                        let stats = stage.stats();
                        counters
                            .audio_datagrams
                            .store(stats.datagrams, Ordering::Relaxed);
                        counters
                            .audio_auth_failures
                            .store(stats.auth_failures, Ordering::Relaxed);
                        counters
                            .audio_protocol_errors
                            .store(stats.protocol_errors, Ordering::Relaxed);
                        counters
                            .audio_stage_played
                            .store(stats.frames_played, Ordering::Relaxed);
                        counters
                            .audio_stage_dropped
                            .store(stats.frames_dropped, Ordering::Relaxed);
                        if let Ok(mut shared) = counters.audio_playback.lock() {
                            *shared = stats.playback;
                        }
                        let skew = stage.take_skew_samples();
                        if !skew.is_empty()
                            && let Ok(mut shared) = counters.a_vs_skew_us.lock()
                        {
                            shared.extend_from_slice(&skew);
                        }
                    }
                    Err(e) => {
                        eprintln!("lumen-client: the audio stage stopped: {e}");
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
        }))
    }
}

/// The capabilities the client advertises.
fn client_capabilities() -> Capabilities {
    let mut capabilities = Capabilities::H264 | Capabilities::REQUEST_IDR;
    // Without AES-NI the transport cipher is the portable one.
    if !lumen_host_service::session::has_aes_ni() {
        capabilities |= Capabilities::AEAD_CHACHA20;
    }
    capabilities
}

/// Pick the display server from the environment.
fn default_backend() -> RenderBackend {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        RenderBackend::Wayland
    } else {
        RenderBackend::X11
    }
}

/// The control stage.
struct ControlStage {
    channel: ControlChannel,
    requests: std::sync::mpsc::Receiver<ControlMessage>,
    running: Arc<AtomicBool>,
    counters: Arc<Counters>,
    events: EventLog,
    link: Arc<crate::receive::LinkAccounting>,
    /// The offset between the host clock and the client clock.
    clock_offset: Arc<Mutex<lumen_audio::ClockOffset>>,
    /// The quality ladder, shared with the present stage.
    ladder: Arc<Mutex<crate::ladder::Ladder>>,
}

impl ControlStage {
    /// Answer the host and forward the client's requests.
    ///
    /// The receive is bounded, so the stage can also flush the requests the
    /// decode stage made, such as an on-demand keyframe.
    ///
    /// The stage also collects what the host tells the client about the
    /// session: the list of monitors, the resolution changes and the
    /// clipboard of the host.
    fn run(mut self) {
        let mut monitors = crate::monitors::MonitorList::new();
        let mut clipboard: Option<crate::clipboard::WaylandClipboard> = None;
        // The client pings the host so it can measure the control channel's
        // round trip time, and reports the link so the host's estimator has
        // the real loss.
        let mut next_ping = Instant::now();
        let mut next_feedback = Instant::now();
        let mut last_rtt_us = 0u64;
        // When the client sent its last ping, on the client clock.
        let mut ping_sent_us = 0u64;
        // The host pings every 500 ms. When the pings stop, the link is
        // gone: a network that drops the packets does not close the
        // connection, so the client has to notice the silence itself.
        let mut last_ping = Instant::now();
        while self.running.load(Ordering::SeqCst) {
            if last_ping.elapsed() > PING_TIMEOUT {
                eprintln!(
                    "lumen-client: no ping from the host for {:?}, the link is gone",
                    last_ping.elapsed()
                );
                break;
            }
            if Instant::now() >= next_ping {
                next_ping = Instant::now() + LINK_FEEDBACK_INTERVAL;
                ping_sent_us = monotonic_us();
                let _ = self.channel.send(&ControlMessage::Ping {
                    ts_us: ping_sent_us,
                });
            }
            // The ladder decides whether the client needs a smaller
            // picture. A client that cannot keep up asks for one, and the
            // host scales the capture down to it.
            if let Some((width, height)) =
                self.ladder.lock().ok().and_then(|mut ladder| ladder.step())
            {
                let _ = self.channel.send(&ControlMessage::Quality {
                    width,
                    height,
                    fps: 0,
                });
            }
            while let Ok(request) = self.requests.try_recv() {
                match &request {
                    ControlMessage::Nack { .. } => {
                        self.counters.nacks_sent.fetch_add(1, Ordering::Relaxed);
                    }
                    ControlMessage::RequestIdr { .. } => {
                        self.counters.idr_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                if let Err(e) = self.channel.send(&request) {
                    eprintln!("lumen-client: cannot send a request: {e}");
                    break;
                }
            }
            match self.channel.recv_timeout(Duration::from_millis(20)) {
                Ok(Some(ControlMessage::Pong { ts_us, reply_ts_us })) => {
                    // The host answered our ping: the round trip is the time
                    // since we sent it, and the host's reply timestamp gives
                    // the offset between the two clocks.
                    let now = monotonic_us();
                    last_rtt_us = now.saturating_sub(ts_us);
                    if ping_sent_us > 0 {
                        // The offset is the client's clock minus the host's.
                        // The client sent the ping, so the host's reply
                        // timestamp is the host's clock at the moment the
                        // ping was answered, and the midpoint of the client's
                        // exchange is the client's clock at that moment.
                        let midpoint = ping_sent_us / 2 + now / 2;
                        let offset = midpoint.cast_signed() - reply_ts_us.cast_signed();
                        if let Ok(mut shared) = self.clock_offset.lock() {
                            *shared = lumen_audio::ClockOffset::new(offset);
                        }
                    }
                }
                Ok(Some(ControlMessage::Ping { ts_us })) => {
                    last_ping = Instant::now();
                    let reply = ControlMessage::Pong {
                        ts_us,
                        reply_ts_us: monotonic_us(),
                    };
                    if let Err(e) = self.channel.send(&reply) {
                        eprintln!("lumen-client: pong failed: {e}");
                        break;
                    }
                }
                Ok(Some(ControlMessage::Monitor {
                    index,
                    width,
                    height,
                    refresh_hz,
                })) => {
                    monitors.apply(index, width, height, refresh_hz);
                    eprintln!(
                        "lumen-client: the host offers the output {} {}",
                        index,
                        monitors
                            .monitors()
                            .last()
                            .map(monitors::Monitor::describe)
                            .unwrap_or_default()
                    );
                }
                Ok(Some(ControlMessage::ResolutionChanged { width, height })) => {
                    eprintln!("lumen-client: the host changed the resolution to {width}x{height}");
                    self.events.write(
                        "resolution-changed",
                        None,
                        Some(format!("{width}x{height}")),
                    );
                }
                Ok(Some(ControlMessage::Clipboard { text })) => {
                    // The host offers its clipboard. The client takes it, so
                    // a copy on the host can be pasted on the client.
                    if clipboard.is_none() {
                        match crate::clipboard::WaylandClipboard::open() {
                            Ok(device) => clipboard = Some(device),
                            Err(reason) => {
                                eprintln!("lumen-client: no clipboard available: {reason}");
                            }
                        }
                    }
                    // The round trip only counts when the clipboard really
                    // took the text. A compositor that does not offer the
                    // data device cannot hold a selection, and the counter
                    // must not pretend otherwise.
                    let mut taken = false;
                    if let Some(device) = clipboard.as_mut() {
                        match device.set(&text) {
                            Ok(()) => taken = true,
                            Err(reason) => {
                                eprintln!("lumen-client: cannot take the clipboard: {reason}");
                            }
                        }
                    }
                    self.events
                        .write("clipboard", None, Some(format!("{} bytes", text.len())));
                    if taken {
                        // The client sends the text back, so the host knows
                        // the clipboard made the round trip.
                        if let Err(e) = self.channel.send(&ControlMessage::Clipboard { text }) {
                            eprintln!("lumen-client: cannot confirm the clipboard: {e}");
                        }
                        self.counters
                            .clipboard_roundtrips
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(Some(ControlMessage::Bye)) => break,
                Ok(Some(_)) => {}
                Ok(None) => {
                    // Nothing arrived. Report the link so the host's
                    // estimator sees the real loss and the real round trip
                    // time. The report is rate limited: the host only needs
                    // one per estimation window.
                    if Instant::now() >= next_feedback {
                        next_feedback = Instant::now() + LINK_FEEDBACK_INTERVAL;
                        let (received, lost) = self.link.snapshot();
                        if received > 0 {
                            let feedback = ControlMessage::LinkFeedback {
                                received,
                                lost,
                                rtt_us: last_rtt_us,
                            };
                            if let Err(e) = self.channel.send(&feedback) {
                                eprintln!("lumen-client: cannot send the link feedback: {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("lumen-client: control channel closed: {e}");
                    break;
                }
            }
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

/// Copy the counters into a plain snapshot.
fn snapshot(counters: &Counters) -> ClientStats {
    ClientStats {
        datagrams_received: counters.datagrams_received.load(Ordering::Relaxed),
        access_units: counters.access_units.load(Ordering::Relaxed),
        frames_decoded: counters.frames_decoded.load(Ordering::Relaxed),
        frames_presented: counters.frames_presented.load(Ordering::Relaxed),
        zero_copy: counters.frames_zero_copy.load(Ordering::Relaxed),
        decode_errors: counters.decode_errors.load(Ordering::Relaxed),
        nacks_sent: counters.nacks_sent.load(Ordering::Relaxed),
        idr_requests: counters.idr_requests.load(Ordering::Relaxed),
        frames_rebuilt: counters.frames_rebuilt.load(Ordering::Relaxed),
        audio_played: counters.audio_played.load(Ordering::Relaxed),
        audio_dropped: counters.audio_dropped.load(Ordering::Relaxed),
        clipboard_roundtrips: counters.clipboard_roundtrips.load(Ordering::Relaxed),
        a_vs_skew_us: counters
            .a_vs_skew_us
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default(),
        audio_datagrams: counters.audio_datagrams.load(Ordering::Relaxed),
        audio_auth_failures: counters.audio_auth_failures.load(Ordering::Relaxed),
        audio_protocol_errors: counters.audio_protocol_errors.load(Ordering::Relaxed),
        audio_stage_played: counters.audio_stage_played.load(Ordering::Relaxed),
        audio_stage_dropped: counters.audio_stage_dropped.load(Ordering::Relaxed),
        audio_playback: counters
            .audio_playback
            .lock()
            .map(|s| *s)
            .unwrap_or_default(),
        frames_dropped: counters.frames_dropped.load(Ordering::Relaxed),
        duration_ms: counters.duration_ms.load(Ordering::Relaxed),
    }
}
