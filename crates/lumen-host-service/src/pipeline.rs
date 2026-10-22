//! The host media pipeline.
//!
//! Four stages, each on its own thread, with a depth-one queue between the
//! capture and the encode stage:
//!
//! ```text
//!   capture  --frame-->  encode  --datagrams-->  send
//!      ^                     ^
//!      |                     |
//!      +---- control --------+
//! ```
//!
//! The capture stage polls the compositor and converts every frame to NV12 on
//! the CPU. The encode stage runs the VA-API H.264 encoder, slices the access
//! units and seals them into datagrams. The send stage paces the datagrams
//! onto the UDP socket. The control stage answers the client and turns
//! `RequestIdr` and `Nack` into commands for the encode stage.
//!
//! Two deviations from the plan are deliberate and recorded in
//! `docs/design.md`: the colour conversion runs on the CPU (D49), and the
//! media peer address is not negotiated but derived from the control
//! connection (D50).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::gpu_convert::GpuConvert;
use lumen_capture_wlr::{StreamConfig, WlrCapture, read_pixels};
use lumen_crypto::{PacketCipher, Role, SessionKeys};
use lumen_encode_vaapi::{EncoderConfig, H264Encoder, RateControl};
use lumen_protocol::{
    Capabilities, FrameBuffer, FrameSource, PixelFormat, VideoEncoder, VideoFrame, color,
};
use lumen_transport::estimator::Estimator;
use lumen_transport::fec::{ParityHysteresis, ParityLevel};
use lumen_transport::media::send_paced;
use lumen_transport::pacing::Pacer;
use lumen_transport::socket::{MediaSocket, SocketOptions};

use crate::backend::DisplayServer;
use crate::control::{ControlChannel, ControlListener, ControlMessage};
use crate::pairing::{PairingError, PairingStore, hex_encode as hex_key};
use crate::persist::{PersistConfig, SessionKeeper, SessionState};
use crate::resize::{CaptureRecovery, RecoveryPolicy, ResizeWatcher, Resolution};
use crate::session;
use crate::settings::HostSettings;
use crate::stages::{StageRecorder, estimate_offset};

/// Default control port, from `docs/protocol.md`.
pub const DEFAULT_CONTROL_PORT: u16 = 24800;
/// Default client video port, from `docs/protocol.md`.
pub const DEFAULT_MEDIA_PORT: u16 = 24801;
/// Default client audio port, from `docs/protocol.md`.
pub const DEFAULT_AUDIO_PORT: u16 = 24802;

/// How long the encode stage waits for a new frame before checking for
/// commands. A frame that is ready returns immediately, so this only bounds
/// the reaction to a NACK.
const COMMAND_POLL: Duration = Duration::from_millis(2);
/// How long the send stage waits for datagrams before checking for shutdown.
const SEND_POLL: Duration = Duration::from_millis(20);
/// Lowest bitrate the adaptive policy will ask for, in bytes per second.
const MIN_TARGET_BYTES_PER_SEC: f64 = 100_000.0;
/// Highest bitrate the adaptive policy will ask for, in bytes per second.
const MAX_TARGET_BYTES_PER_SEC: f64 = 50_000_000.0;
/// Length of one estimation window.
///
/// The window has to be longer than the interval at which the client reports
/// the link, or the counters are reset before the report lands and the loss
/// is never seen.
const ESTIMATOR_WINDOW: Duration = Duration::from_millis(500);
/// How often the host pings the client to estimate the clock offset.
const PING_INTERVAL: Duration = Duration::from_millis(500);

/// How often the host reads its own clipboard. The read blocks, so it must
/// not run on every round of the control loop.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);
/// How long the control stage waits for a message before looking at the clock.
const CONTROL_POLL: Duration = Duration::from_millis(20);
/// Bytes per second the media pacer allows. Well above the bitrate a lab
/// session produces; the pacer exists to spread a burst, not to shape a rate.
const PACER_BYTES_PER_SEC: f64 = 12_000_000.0;

/// Configuration of the host pipeline.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Address the control listener binds to.
    pub control_addr: SocketAddr,
    /// Address the media socket binds to.
    pub media_addr: SocketAddr,
    /// Port the client receives media on.
    pub client_media_port: u16,
    /// Which output to capture. `None` picks the output matching the size.
    pub output: Option<String>,
    /// Capture width in pixels.
    pub width: u32,
    /// Capture height in pixels.
    pub height: u32,
    /// Stream rate in frames per second.
    pub fps: u32,
    /// Capture through `wl_shm` instead of DMA-BUF.
    pub prefer_cpu: bool,
    /// Encoder rate control.
    pub rate_control: RateControl,
    /// Stop after this long. `None` runs until the client leaves.
    pub max_duration: Option<Duration>,
    /// Where the host writes the pairing code it shows. `None` means the
    /// code is only printed.
    pub pairing_code_file: Option<std::path::PathBuf>,
    /// Which display server the host captures from.
    pub capture: DisplayServer,
    /// The X display the host captures from. `None` reads `DISPLAY`.
    pub display: Option<String>,
    /// How long the host keeps the session after the client left.
    ///
    /// Zero means the session ends with the client.
    pub persist_timeout: Duration,
    /// How many sessions the host serves at the same time.
    pub max_sessions: usize,
    /// How many of them may be shadow sessions.
    pub shadow_sessions: usize,
    /// Address the audio socket binds to.
    pub audio_addr: SocketAddr,
    /// Port the client receives audio on.
    pub client_audio_port: u16,
    /// Whether the session carries audio.
    pub audio: bool,
    /// A text to put into the host's own selection once, so a run can
    /// exercise the clipboard round trip without a person copying something.
    pub clipboard_test: Option<String>,
    /// The audio stream parameters.
    pub audio_config: lumen_audio::AudioConfig,
    /// Where the control plane listens. `None` means it does not run.
    ///
    /// The control plane can change the running session, so it binds to a
    /// loopback address and every change it makes has to carry the token
    /// the host printed.
    pub web_addr: Option<SocketAddr>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            control_addr: SocketAddr::from(([0, 0, 0, 0], DEFAULT_CONTROL_PORT)),
            media_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            client_media_port: DEFAULT_MEDIA_PORT,
            output: None,
            width: 1280,
            height: 720,
            fps: 30,
            prefer_cpu: false,
            rate_control: RateControl::default(),
            max_duration: None,
            pairing_code_file: None,
            capture: DisplayServer::Wayland,
            display: None,
            persist_timeout: Duration::ZERO,
            max_sessions: 1,
            shadow_sessions: 0,
            audio_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            client_audio_port: DEFAULT_AUDIO_PORT,
            audio: false,
            clipboard_test: None,
            web_addr: None,
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

/// Errors from starting or running the pipeline.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The control listener or the media socket could not be set up.
    #[error("socket setup failed: {0}")]
    Io(#[from] std::io::Error),
    /// The session could not be established.
    #[error("session failed: {0}")]
    Session(#[from] session::SessionError),
    /// The capture source could not be opened.
    #[error("capture failed: {0}")]
    Capture(#[from] lumen_capture_wlr::Error),
    /// The encoder could not be created.
    #[error("encoder setup failed: {0}")]
    Encoder(String),
    /// The pairing store could not be read or written.
    #[error("pairing failed: {0}")]
    Pairing(#[from] PairingError),
    /// The session was refused.
    #[error("the session was refused: {0}")]
    Admission(String),
}

/// A batch of datagrams ready to send, with the frame they belong to.
#[derive(Debug, Clone)]
struct Datagrams {
    /// The host frame identifier.
    frame_id: u32,
    /// The sealed datagrams.
    datagrams: Vec<Vec<u8>>,
}

/// A command for the encode stage.
enum EncodeCommand {
    /// Resend one slice of an earlier frame, after a NACK.
    Retransmit { frame_id: u32, slice_index: u16 },
}

/// Counters the stages share.
#[derive(Debug, Default)]
pub struct Counters {
    frames_captured: AtomicU64,
    pub(crate) frames_encoded: AtomicU64,
    datagrams_sent: AtomicU64,
    bytes_sent: AtomicU64,
    capture_errors: AtomicU64,
    encode_errors: AtomicU64,
    keyframe_requests: AtomicU64,
    retransmits: AtomicU64,
    duration_ms: AtomicU64,
    estimated_bytes_per_sec: AtomicU64,
    parity_level_changes: AtomicU64,
    audio_packets: AtomicU64,
    clipboard_roundtrips: AtomicU64,
    resize_events: AtomicU64,
    frames_gpu_converted: AtomicU64,
}

/// A snapshot of the pipeline counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PipelineStats {
    /// Frames the capture stage produced.
    pub frames_captured: u64,
    /// Frames converted to NV12 on the GPU, with no CPU copy.
    pub frames_gpu_converted: u64,
    /// Frames the encoder turned into access units.
    pub frames_encoded: u64,
    /// Datagrams written to the media socket.
    pub datagrams_sent: u64,
    /// Bytes written to the media socket.
    pub bytes_sent: u64,
    /// Errors raised by the capture stage.
    pub capture_errors: u64,
    /// Errors raised by the encode stage.
    pub encode_errors: u64,
    /// Keyframes the client asked for.
    pub keyframe_requests: u64,
    /// Slices resent after a NACK.
    pub retransmits: u64,
    /// How long the pipeline ran, in milliseconds.
    pub duration_ms: u64,
    /// The bitrate the bandwidth policy estimates, in bytes per second.
    pub estimated_bytes_per_sec: u64,
    /// How many times the parity level changed.
    pub parity_level_changes: u64,
    /// Audio frames sent to the client.
    pub audio_packets: u64,
    /// Clipboard transfers that completed.
    pub clipboard_roundtrips: u64,
    /// Resolution changes the host told the client about.
    pub resize_events: u64,
    /// The per-stage duration samples, in microseconds. The client clock is
    /// mapped onto the host clock, so every sample is one clock.
    pub stage_samples: BTreeMap<String, Vec<i64>>,
}

/// The host pipeline.
#[derive(Debug)]
pub struct HostPipeline {
    config: HostConfig,
    counters: Arc<Counters>,
    pairing: Arc<Mutex<PairingStore>>,
    pairing_mode: session::PairingMode,
    stages: Arc<Mutex<StageRecorder>>,
    estimator: Arc<Mutex<Estimator>>,
    /// The settings the control plane changes.
    settings: Arc<HostSettings>,
    /// The actions the control plane asks for.
    actions: Arc<crate::web::Actions>,
}

impl HostPipeline {
    /// Create a pipeline with the given configuration.
    ///
    /// The pairing store holds the host's identity and the clients it trusts.
    ///
    /// # Errors
    /// Returns an error when the pairing store cannot be read or created.
    pub fn new(
        config: HostConfig,
        pairing: PairingStore,
        pairing_mode: session::PairingMode,
    ) -> Result<Self, PipelineError> {
        let initial_rate = match config.rate_control {
            RateControl::Cbr { bits_per_second } => Some(f64::from(bits_per_second) / 8.0),
            RateControl::Cqp { .. } => None,
        };
        // The settings start where the configuration says they should.
        let settings = HostSettings::default();
        settings.set_qp(u32::from(match config.rate_control {
            RateControl::Cqp { qp } => qp,
            RateControl::Cbr { .. } => 22,
        }));
        settings.set_bitrate_bps(match config.rate_control {
            RateControl::Cbr { bits_per_second } => bits_per_second,
            RateControl::Cqp { .. } => 0,
        });
        settings.set_fps(config.fps);
        settings.set_audio(config.audio);
        // The picture starts at the size the host captures. A bigger one
        // would be pixels the host has to invent.
        let start = crate::settings::Quality::new(config.width, config.height, config.fps);
        settings.set_quality(start);
        settings.set_quality_ceiling(start);
        Ok(Self {
            config,
            counters: Arc::new(Counters::default()),
            pairing: Arc::new(Mutex::new(pairing)),
            pairing_mode,
            stages: Arc::new(Mutex::new(StageRecorder::new())),
            settings: Arc::new(settings),
            actions: Arc::new(crate::web::Actions::default()),
            estimator: Arc::new(Mutex::new({
                let mut estimator = Estimator::new(
                    MIN_TARGET_BYTES_PER_SEC,
                    MAX_TARGET_BYTES_PER_SEC,
                    ESTIMATOR_WINDOW,
                );
                // The policy starts from the bitrate the session was
                // configured with, not from the floor.
                if let Some(rate) = initial_rate {
                    estimator.set_initial_rate(rate);
                }
                estimator
            })),
        })
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> PipelineStats {
        let samples = self
            .stages
            .lock()
            .map(|mut stages| stages.samples().to_map())
            .unwrap_or_default();
        snapshot(&self.counters, samples)
    }

    /// Serve the clients until the run ends.
    ///
    /// The host accepts a client, streams to it and keeps the session when
    /// the client goes away, so an unattended client can come back. More
    /// than one client may be served at the same time; every session has
    /// its own encoder, its own media socket and its own media port.
    ///
    /// # Errors
    /// Returns an error when a socket, the capture source, the encoder or the
    /// session cannot be set up.
    pub fn run(&mut self) -> Result<PipelineStats, PipelineError> {
        // The configuration is copied rather than borrowed: the loop borrows
        // the pipeline mutably when it carries out the control plane's
        // actions.
        let config = self.config.clone();
        let counters = Arc::clone(&self.counters);

        let started = Instant::now();
        let listener = ControlListener::bind(config.control_addr)?;
        let deadline = config.max_duration.map(|d| Instant::now() + d);
        let mut keeper = SessionKeeper::new(PersistConfig {
            disconnect_timeout: config.persist_timeout,
            keep_session: !config.persist_timeout.is_zero(),
        });
        let sessions = Arc::new(Mutex::new(crate::multi::SessionTable::new(
            crate::multi::MultiConfig {
                max_sessions: config.max_sessions.max(1),
                shadow_sessions: config.shadow_sessions,
                base_media_port: config.client_media_port,
            },
        )));
        let mut handles: Vec<SessionHandles> = Vec::new();
        // The frames of the capture go to every session.
        let broadcaster: Arc<Mutex<Vec<SyncSender<Arc<VideoFrame>>>>> =
            Arc::new(Mutex::new(Vec::new()));
        // The keyframe request is shared with the control plane, so the
        // dashboard can ask for one.
        let keyframe_requested = Arc::clone(&self.settings.keyframe_requested);
        let stages = Arc::clone(&self.stages);
        let estimator = Arc::clone(&self.estimator);
        let monitors: Arc<Mutex<Vec<crate::monitors::MonitorInfo>>> =
            Arc::new(Mutex::new(Vec::new()));
        let resolution_changes: Arc<Mutex<Vec<(u32, u32)>>> = Arc::new(Mutex::new(Vec::new()));
        let capture_running = Arc::new(AtomicBool::new(true));
        // The render node of the capture, which the encoders open. It is published once the
        // capture is open.
        let capture_node: Arc<OnceLock<Option<PathBuf>>> = Arc::new(OnceLock::new());
        // The pairing code the host is showing, so the control plane can
        // show it too.
        let pairing_code: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        // The capture timestamp of the latest frame, so the audio can be
        // aligned with the video.
        let capture_ts_us = Arc::new(AtomicU64::new(0));
        let mut capture_handle: Option<thread::JoinHandle<()>> = None;

        // The control plane runs beside the pipeline, with the same state
        // the stages read.
        if let Some(address) = config.web_addr {
            // The history of the latency, sampled while the host runs.
            let history = Arc::new(crate::web::History::default());
            let sampler = Arc::clone(&history);
            let sampler_stages = Arc::clone(&stages);
            let sampler_running = Arc::clone(&capture_running);
            std::thread::spawn(move || {
                while sampler_running.load(Ordering::SeqCst) {
                    let samples = sampler_stages
                        .lock()
                        .map(|mut stages| stages.samples().to_map())
                        .unwrap_or_default();
                    if let Some(values) = samples.get("end_to_end")
                        && !values.is_empty()
                    {
                        let mut sorted = values.clone();
                        sorted.sort_unstable();
                        let at = |q: f64| -> f64 {
                            let index = ((sorted.len() as f64 - 1.0) * q).round() as usize;
                            sorted[index.min(sorted.len() - 1)] as f64 / 1000.0
                        };
                        sampler.push(at(0.50), at(0.99));
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            });
            let view = crate::web::HostView {
                counters: Arc::clone(&counters),
                stages: Arc::clone(&stages),
                settings: Arc::clone(&self.settings),
                sessions: Arc::clone(&sessions),
                monitors: Arc::clone(&monitors),
                pairing: Arc::clone(&self.pairing),
                actions: Arc::clone(&self.actions),
                config: config.clone(),
                started,
                pairing_code: Arc::clone(&pairing_code),
                capture_running: Arc::clone(&capture_running),
                history: Arc::clone(&history),
            };
            match lumen_web::WebServer::bind(
                address,
                std::sync::Arc::new(crate::web::HostApi::new(view)),
            ) {
                Ok(server) => {
                    crate::host_log!(info "lumen-host: the control plane is on {}", server.url());
                    let _handle = server.spawn();
                }
                Err(e) => crate::host_log!(info "lumen-host: the control plane did not start: {e}"),
            }
        }

        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            // The sessions that ended are forgotten, and the session is
            // given up when the client did not come back in time.
            let mut ended = false;
            handles.retain(|handle| {
                if handle.control.is_finished() {
                    ended = true;
                    if let Ok(mut table) = sessions.lock() {
                        table.remove(handle.id);
                    }
                    false
                } else {
                    true
                }
            });
            if ended {
                keeper.client_disconnected(Instant::now());
            }
            let no_sessions = sessions.lock().map_or(true, |t| t.is_empty());
            if no_sessions && keeper.tick(Instant::now()) == SessionState::Expired {
                break;
            }

            // The actions the control plane asked for.
            for id in self.actions.take_kicks() {
                if let Some(handle) = handles.iter().find(|handle| handle.id == id) {
                    handle.kick.store(true, Ordering::SeqCst);
                }
            }
            // The picture the operator asked for.
            if let Some(quality) = self.actions.take_quality() {
                self.settings.set_quality(quality);
                self.settings.set_quality_ceiling(quality);
                if let Ok(mut changes) = resolution_changes.lock() {
                    changes.push((quality.width, quality.height));
                }
                crate::host_log!(
                    info
                    "lumen-host: the picture is now {}x{}",
                    quality.width,
                    quality.height
                );
            }
            for key in self.actions.take_revocations() {
                if let Ok(mut store) = self.pairing.lock()
                    && let Err(e) = store.untrust(&key)
                {
                    crate::host_log!(warn "lumen-host: cannot revoke a client: {e}");
                }
            }

            let accepted = listener.accept_timeout(Duration::from_millis(100))?;
            let Some((stream, peer)) = accepted else {
                continue;
            };
            match self.admit(
                stream,
                peer,
                &sessions,
                &broadcaster,
                &keyframe_requested,
                &stages,
                &estimator,
                &monitors,
                &resolution_changes,
                &capture_running,
                &mut capture_handle,
                &capture_ts_us,
                &capture_node,
                &pairing_code,
            ) {
                Ok(handle) => {
                    keeper.client_connected();
                    handles.push(handle);
                }
                Err(e) => {
                    crate::host_log!(warn "lumen-host: the session was refused: {e}");
                }
            }
        }

        capture_running.store(false, Ordering::SeqCst);
        for handle in &handles {
            handle.running.store(false, Ordering::SeqCst);
        }
        for handle in handles {
            let _ = handle.control.join();
            let _ = handle.encode.join();
            let _ = handle.send.join();
            if let Some(audio) = handle.audio {
                let _ = audio.join();
            }
            if let Some(input) = handle.input {
                let _ = input.stop();
            }
        }
        if let Some(handle) = capture_handle {
            let _ = handle.join();
        }
        counters
            .duration_ms
            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        Ok(self.stats())
    }

    /// Admit one client: the handshake, the session table and the stages.
    ///
    /// The number of arguments is what it is: the stages need the keys of
    /// the session, the shared state of the pipeline and the sinks of the
    /// other sessions.
    #[allow(clippy::too_many_arguments)]
    fn admit(
        &mut self,
        stream: std::net::TcpStream,
        peer: SocketAddr,
        sessions: &Arc<Mutex<crate::multi::SessionTable>>,
        broadcaster: &Arc<Mutex<Vec<SyncSender<Arc<VideoFrame>>>>>,
        keyframe_requested: &Arc<AtomicBool>,
        stages: &Arc<Mutex<StageRecorder>>,
        estimator: &Arc<Mutex<lumen_transport::estimator::Estimator>>,
        monitors: &Arc<Mutex<Vec<crate::monitors::MonitorInfo>>>,
        resolution_changes: &Arc<Mutex<Vec<(u32, u32)>>>,
        capture_running: &Arc<AtomicBool>,
        capture_handle: &mut Option<thread::JoinHandle<()>>,
        capture_ts_us: &Arc<AtomicU64>,
        capture_node: &Arc<OnceLock<Option<PathBuf>>>,
        pairing_code: &Arc<Mutex<Option<u32>>>,
    ) -> Result<SessionHandles, PipelineError> {
        let config = self.config.clone();
        let counters = Arc::clone(&self.counters);
        let (mut stream, peer) = (stream, peer);
        let established = session::establish(&mut stream, Role::Responder, host_capabilities())?;
        let mut channel = session::control_channel(stream, &established, Role::Responder)?;
        let policy = session::PairingPolicy {
            host_public: self.pairing.lock().map_or([0u8; 32], |p| p.public_bytes()),
            trusted: self.pairing.lock().map_or_default(|p| p.trusted_clients()),
            mode: self.pairing_mode,
            client_nonce: established.remote_static.unwrap_or([0u8; 32]),
            code_file: config.pairing_code_file.clone(),
        };
        // The code the host is about to show, so the control plane can
        // show it as well.
        let host_public = self.pairing.lock().map_or([0u8; 32], |p| p.public_bytes());
        let client_nonce = established.remote_static.unwrap_or([0u8; 32]);
        if let Ok(mut slot) = pairing_code.lock() {
            *slot = Some(lumen_crypto::PairingCode::derive(&host_public, &client_nonce).value());
        }
        let authorised = session::host_hello(
            &mut channel,
            &established.keys,
            host_capabilities(),
            &policy,
        )?;
        if authorised.newly_paired {
            crate::host_log!(info
                "lumen-host: paired the client {}",
                hex_key(&authorised.device_key)
            );
            self.pairing
                .lock()
                .map_err(|_| PipelineError::Admission("the pairing store is poisoned".to_string()))?
                .trust(authorised.device_key)?;
        }

        let kick = Arc::new(AtomicBool::new(false));
        let entry = sessions
            .lock()
            .map_err(|_| PipelineError::Admission("the session table is poisoned".to_string()))?
            .admit(peer)
            .map_err(|e| PipelineError::Admission(e.to_string()))?;
        crate::host_log!(info
            "lumen-host: the client {} is the {} session {}",
            peer,
            entry.role.name(),
            entry.id
        );
        // The media peer is the client's control peer address on the media
        // port of the session. The port is not negotiated (D50).
        let media_peer = SocketAddr::new(peer.ip(), entry.media_port);
        let socket = MediaSocket::bind(config.media_addr, SocketOptions::default())?;

        let running = Arc::new(AtomicBool::new(true));
        let (frame_tx, frame_rx) = sync_channel::<Arc<VideoFrame>>(1);
        let (command_tx, command_rx) = sync_channel::<EncodeCommand>(8);
        let (datagram_tx, datagram_rx) = sync_channel::<Datagrams>(1);
        if let Ok(mut sinks) = broadcaster.lock() {
            sinks.push(frame_tx);
        }

        // The client seals its input with the client-to-host media key. The cipher is made before
        // the keys move into the encoder.
        let input_cipher = PacketCipher::new(
            established.aead,
            established.keys.media_c2h,
            established.keys.session_id,
        );
        let encode_handle = {
            let stage = EncodeStage {
                config: config.clone(),
                keys: established.keys,
                aead: established.aead,
                frames: frame_rx,
                commands: command_rx,
                datagrams: datagram_tx,
                keyframe_requested: Arc::clone(keyframe_requested),
                running: Arc::clone(&running),
                counters: Arc::clone(&counters),
                stages: Arc::clone(stages),
                estimator: Arc::clone(estimator),
                parity_hysteresis: ParityHysteresis::default(),
                settings: Arc::clone(&self.settings),
                render_node: Arc::clone(capture_node),
            };
            thread::spawn(move || stage.run())
        };

        let audio_handle = if config.audio {
            match Self::start_audio(
                &config,
                &established,
                peer.ip(),
                config.client_audio_port,
                Arc::clone(&running),
                Arc::clone(&counters),
                Arc::clone(capture_ts_us),
            ) {
                Ok(handle) => Some(handle),
                Err(e) => {
                    crate::host_log!(warn "lumen-host: no audio: {e}");
                    None
                }
            }
        } else {
            None
        };

        // The input socket shares the media address of the host, so the client reaches it on the
        // path it already uses for media.
        let input_addr = SocketAddr::new(config.media_addr.ip(), crate::input::INPUT_PORT);
        // A host that shares an X11 display injects into it; otherwise the kernel gets the input.
        let target = match config.capture {
            DisplayServer::X11 => crate::input::InjectTarget::X11(config.display.clone()),
            DisplayServer::Wayland => crate::input::InjectTarget::Uinput,
        };
        let input_handle = crate::input::InputStage::start(
            input_addr,
            input_cipher,
            &target,
            (config.width, config.height),
            Arc::clone(&self.settings),
        );

        let send_handle = {
            let stage = SendStage {
                socket,
                peer: media_peer,
                datagrams: datagram_rx,
                running: Arc::clone(&running),
                counters: Arc::clone(&counters),
                stages: Arc::clone(stages),
                estimator: Arc::clone(estimator),
            };
            thread::spawn(move || stage.run())
        };

        let control_handle = {
            let stage = ControlStage {
                channel,
                kick: Arc::clone(&kick),
                commands: command_tx,
                keyframe_requested: Arc::clone(keyframe_requested),
                running: Arc::clone(&running),
                counters: Arc::clone(&counters),
                stages: Arc::clone(stages),
                estimator: Arc::clone(estimator),
                monitors: Arc::clone(monitors),
                resolution_changes: Arc::clone(resolution_changes),
                monitors_sent: false,
                last_feedback: None,
                settings: Arc::clone(&self.settings),
                capture_size: (config.width, config.height),
                clipboard_rx: spawn_clipboard_reader(
                    Arc::clone(&running),
                    config.clipboard_test.clone(),
                ),
                clipboard_sent: None,
            };
            thread::spawn(move || stage.run())
        };

        // The capture is shared by every session and starts with the first.
        if capture_handle.is_none() {
            let stage = CaptureStage {
                config: config.clone(),
                frames: {
                    let sinks = Arc::clone(broadcaster);
                    // The capture stage writes to every sink, so it needs a
                    // sender of its own that fans out.
                    let (own_tx, own_rx) = sync_channel::<Arc<VideoFrame>>(1);
                    std::thread::spawn(move || {
                        while let Ok(frame) = own_rx.recv() {
                            if let Ok(sinks) = sinks.lock() {
                                for sink in sinks.iter() {
                                    let _ = sink.try_send(Arc::clone(&frame));
                                }
                            }
                        }
                    });
                    own_tx
                },
                running: Arc::clone(capture_running),
                counters: Arc::clone(&counters),
                keyframe_requested: Arc::clone(keyframe_requested),
                monitors: Arc::clone(monitors),
                resolution_changes: Arc::clone(resolution_changes),
                resize: Arc::new(Mutex::new(ResizeWatcher::new())),
                reopened: Arc::new(AtomicU64::new(0)),
                capture_ts_us: Arc::clone(capture_ts_us),
                settings: Arc::clone(&self.settings),
                render_node: Arc::clone(capture_node),
            };
            *capture_handle = Some(thread::spawn(move || stage.run()));
        }

        Ok(SessionHandles {
            id: entry.id,
            running,
            control: control_handle,
            encode: encode_handle,
            send: send_handle,
            audio: audio_handle,
            input: input_handle,
            kick,
        })
    }

    /// Start the audio stage of a session.
    ///
    /// The audio goes out on a socket of its own, sealed with the audio key,
    /// so it can never collide with the video's nonces.
    fn start_audio(
        config: &HostConfig,
        established: &session::Established,
        peer_ip: std::net::IpAddr,
        client_audio_port: u16,
        running: Arc<AtomicBool>,
        counters: Arc<Counters>,
        capture_ts_us: Arc<AtomicU64>,
    ) -> Result<thread::JoinHandle<()>, String> {
        let socket = MediaSocket::bind(config.audio_addr, SocketOptions::default())
            .map_err(|e| e.to_string())?;
        let peer = SocketAddr::new(peer_ip, client_audio_port);
        let cipher = PacketCipher::new(
            established.aead,
            established.keys.audio_h2c,
            established.keys.session_id,
        );
        let mut stage = crate::audio::AudioStage::new(config.audio_config, socket, peer, cipher)?;
        Ok(thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                // The audio aligns with the video frame that is on screen.
                stage.set_video_capture_ts(capture_ts_us.load(Ordering::Relaxed));
                match stage.pump() {
                    Ok(frames) => {
                        if frames > 0 {
                            counters
                                .audio_packets
                                .fetch_add(frames as u64, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        crate::host_log!(info "lumen-host: the audio stage stopped: {e}");
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
        }))
    }
}

/// The stages of one session.
struct SessionHandles {
    /// The identifier of the session.
    #[allow(dead_code)]
    id: u32,
    /// The flag the stages watch.
    running: Arc<AtomicBool>,
    /// The control stage; it ends when the client leaves.
    control: thread::JoinHandle<()>,
    /// The encode stage.
    encode: thread::JoinHandle<()>,
    /// The send stage.
    send: thread::JoinHandle<()>,
    /// The audio stage, when the session carries audio.
    audio: Option<thread::JoinHandle<()>>,
    /// The input path, when the input socket and uinput are both available.
    input: Option<crate::input::InputStage>,
    /// Set when the control plane asks for the session to end.
    kick: Arc<AtomicBool>,
}

/// Read the host's own clipboard on a thread of its own.
///
/// The read waits for the compositor to answer, and the control channel must
/// not wait with it: a slow answer would delay the clock exchange and skew
/// the offset the whole latency measurement rests on.
fn spawn_clipboard_reader(
    running: Arc<AtomicBool>,
    test_text: Option<String>,
) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut device = match crate::clipboard::WaylandClipboard::open() {
            Ok(device) => Some(device),
            Err(reason) => {
                crate::host_log!(warn "lumen-host: no clipboard available: {reason}");
                None
            }
        };
        let test_text = test_text;
        while running.load(Ordering::SeqCst) {
            if let Some(device) = device.as_mut() {
                // The test hook owns a selection once, so a run can exercise
                // the round trip without a person copying something.
                if let Some(text) = test_text.clone()
                    && device.owned() != Some(text.as_str())
                    && let Err(reason) = device.set(&text)
                {
                    crate::host_log!(warn "lumen-host: cannot set the test clipboard: {reason}");
                }
                let text = match device.owned() {
                    Some(owned) => Some(owned.to_owned()),
                    None => device.read(Duration::from_millis(20)).ok().flatten(),
                };
                if let Some(text) = text
                    && tx.send(text).is_err()
                {
                    return;
                }
            }
            thread::sleep(CLIPBOARD_POLL);
        }
    });
    rx
}

/// The capabilities the host advertises.
///
/// `FEC_RS` is not advertised: the parity in use is GF(256), not
/// Reed-Solomon (D35).
fn host_capabilities() -> Capabilities {
    let mut capabilities = Capabilities::H264 | Capabilities::REQUEST_IDR;
    // Without AES-NI the transport cipher is the portable one.
    if !session::has_aes_ni() {
        capabilities |= Capabilities::AEAD_CHACHA20;
    }
    capabilities
}

/// The capture source of the host.
///
/// The two backends produce the same frames, so everything above the capture
/// stage is the same for both.
enum CaptureSource {
    /// A Wayland compositor, captured through wlroots screencopy.
    Wayland(Box<WlrCapture>),
    /// An X11 server, captured through DRI3, MIT-SHM or GetImage.
    X11(Box<lumen_capture_x11::X11Capture>),
}

impl CaptureSource {
    /// The next frame, when there is one.
    fn poll_frame(&mut self) -> Option<VideoFrame> {
        match self {
            Self::Wayland(capture) => capture.poll_frame(),
            Self::X11(capture) => capture.poll_frame(),
        }
    }

    /// The most recent error of the capture.
    fn take_error(&mut self) -> Option<String> {
        match self {
            Self::Wayland(capture) => capture.take_error().map(|e| e.to_string()),
            Self::X11(capture) => capture.take_error().map(|e| e.to_string()),
        }
    }

    /// Change the capture rate.
    fn set_fps(&mut self, fps: u32) {
        match self {
            Self::Wayland(capture) => capture.set_fps(fps),
            Self::X11(capture) => capture.set_fps(fps),
        }
    }

    /// The render node the DMA-BUF frames are allocated on, or `None` when there is none.
    fn render_node(&self) -> Option<PathBuf> {
        match self {
            Self::Wayland(capture) => capture.render_node().map(Path::to_path_buf),
            Self::X11(capture) => capture.render_node().map(Path::to_path_buf),
        }
    }
}

/// The capture stage.
struct CaptureStage {
    config: HostConfig,
    frames: SyncSender<Arc<VideoFrame>>,
    running: Arc<AtomicBool>,
    counters: Arc<Counters>,
    /// Set when the capture was reopened, so the encoder asks for a keyframe.
    keyframe_requested: Arc<AtomicBool>,
    /// The monitor list of the host, sent to the client.
    monitors: Arc<Mutex<Vec<crate::monitors::MonitorInfo>>>,
    /// The mode changes the host saw, sent to the client.
    resolution_changes: Arc<Mutex<Vec<(u32, u32)>>>,
    /// The size changes the capture stage saw.
    resize: Arc<Mutex<ResizeWatcher>>,
    /// How many times the capture was reopened.
    reopened: Arc<AtomicU64>,
    /// The capture timestamp of the latest frame, so the audio can be
    /// aligned with the video.
    capture_ts_us: Arc<AtomicU64>,
    /// The settings the control plane changes, which carry the picture the
    /// client asked for.
    settings: Arc<HostSettings>,
    /// The render node of the capture, published once it is open for the encoders.
    render_node: Arc<OnceLock<Option<PathBuf>>>,
}

impl CaptureStage {
    /// Open the capture source of the configured backend.
    fn open(config: &HostConfig) -> Result<CaptureSource, String> {
        match config.capture {
            DisplayServer::Wayland => {
                let stream_config = StreamConfig {
                    output: config.output.clone(),
                    width: config.width,
                    height: config.height,
                    fps: config.fps,
                    prefer_cpu: config.prefer_cpu,
                };
                WlrCapture::open(&stream_config)
                    .map(|capture| CaptureSource::Wayland(Box::new(capture)))
                    .map_err(|e| e.to_string())
            }
            DisplayServer::X11 => {
                let capture_config = lumen_capture_x11::CaptureConfig {
                    display: config.display.clone(),
                    region: None,
                    fps: config.fps,
                    backend: None,
                };
                lumen_capture_x11::X11Capture::open(&capture_config)
                    .map(|capture| CaptureSource::X11(Box::new(capture)))
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// Poll the compositor and hand NV12 frames to the encode stage.
    ///
    /// A capture that fails is reopened: a compositor that restarts leaves
    /// the capture with a dead connection, and the session has to survive it.
    /// A capture that cannot be opened at all is retried, because the
    /// compositor may not be up yet when the host starts.
    fn run(self) {
        let policy = RecoveryPolicy::default();
        let mut failures = 0u32;
        let mut capture = loop {
            match Self::open(&self.config) {
                Ok(capture) => break capture,
                Err(e) => {
                    failures = failures.saturating_add(1);
                    self.counters.capture_errors.fetch_add(1, Ordering::Relaxed);
                    crate::host_log!(warn "lumen-host: cannot open the capture source: {e}");
                    let delay = policy.delay_after(failures);
                    thread::sleep(delay);
                    if !self.running.load(Ordering::SeqCst) {
                        return;
                    }
                }
            }
        };
        let node = capture.render_node();
        match &node {
            Some(path) => {
                crate::host_log!(info "lumen-host: capture backend dmabuf on {}", path.display());
            }
            None => {
                crate::host_log!(info "lumen-host: capture backend without a render node (shm or X11)");
            }
        }
        // The encoders open the device the buffers live on. Publishing it wakes an encoder that
        // is waiting for the capture to open.
        let _ = self.render_node.set(node.clone());
        let mut gpu = node.as_deref().and_then(|path| match GpuConvert::open(path) {
            Ok(gpu) => Some(gpu),
            Err(e) => {
                crate::host_log!(warn "lumen-host: GPU conversion disabled for this session: {e}");
                None
            }
        });
        self.list_monitors();
        let mut failures = 0u32;

        while self.running.load(Ordering::SeqCst) {
            // The control plane changes the rate at run time, so the capture follows it.
            capture.set_fps(self.settings.fps());
            if let Some(frame) = capture.poll_frame() {
                failures = 0;
                let size = Resolution::new(frame.width, frame.height).ok();
                if let Some(size) = size
                    && let Ok(mut watcher) = self.resize.lock()
                    && let Some(change) = watcher.observe(size)
                {
                    crate::host_log!(info
                        "lumen-host: the output changed from {}x{} to {}x{}",
                        change.previous.width,
                        change.previous.height,
                        change.current.width,
                        change.current.height
                    );
                    if let Ok(mut changes) = self.resolution_changes.lock() {
                        changes.push((change.current.width, change.current.height));
                    }
                }
                let quality = self.settings.quality();
                let (buffer, nv12_width, nv12_height, on_gpu) =
                    match convert_frame(&frame, quality, &mut gpu) {
                        Ok(v) => v,
                        Err(e) => {
                            self.counters.capture_errors.fetch_add(1, Ordering::Relaxed);
                            crate::host_log!(warn "lumen-host: frame conversion failed: {e}");
                            continue;
                        }
                    };
                let converted = VideoFrame {
                    width: nv12_width,
                    height: nv12_height,
                    format: PixelFormat::Nv12,
                    buffer,
                    capture_ts_us: frame.capture_ts_us,
                    index: frame.index,
                };
                if on_gpu {
                    self.counters
                        .frames_gpu_converted
                        .fetch_add(1, Ordering::Relaxed);
                }
                self.counters
                    .frames_captured
                    .fetch_add(1, Ordering::Relaxed);
                // The audio aligns with the video frame that is on screen, so
                // it needs the capture timestamp of the latest frame.
                self.capture_ts_us
                    .store(frame.capture_ts_us, Ordering::Relaxed);
                if self.frames.send(Arc::new(converted)).is_err() {
                    return;
                }
            } else if let Some(e) = capture.take_error() {
                failures = failures.saturating_add(1);
                self.counters.capture_errors.fetch_add(1, Ordering::Relaxed);
                crate::host_log!(warn "lumen-host: capture error: {e}");
                match policy.action_after(failures) {
                    CaptureRecovery::Retry => {}
                    CaptureRecovery::Reopen => {
                        // The compositor may have restarted. The capture is
                        // opened again after a pause, and the encoder needs a
                        // keyframe for the new stream.
                        let delay = policy.delay_after(failures);
                        thread::sleep(delay);
                        match Self::open(&self.config) {
                            Ok(source) => {
                                capture = source;
                                self.reopened.fetch_add(1, Ordering::Relaxed);
                                self.keyframe_requested.store(true, Ordering::Relaxed);
                                crate::host_log!(info "lumen-host: the capture was reopened after {delay:?}");
                            }
                            Err(e) => {
                                crate::host_log!(warn "lumen-host: cannot reopen the capture: {e}");
                            }
                        }
                    }
                    CaptureRecovery::Stop => {
                        self.running.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Read the outputs of the host and remember them.
    ///
    /// The list is sent to the client over the control channel, so the
    /// client can show which outputs the host can capture.
    fn list_monitors(&self) {
        let source = match self.config.capture {
            DisplayServer::Wayland => crate::monitors::MonitorSource::Wayland,
            DisplayServer::X11 => crate::monitors::MonitorSource::X11,
        };
        match crate::monitors::list(source, self.config.display.as_deref()) {
            Ok(monitors) => {
                for monitor in &monitors {
                    crate::host_log!(info "lumen-host: the output {}", monitor.describe());
                }
                if let Ok(mut list) = self.monitors.lock() {
                    *list = monitors;
                }
            }
            Err(e) => {
                crate::host_log!(warn "lumen-host: cannot list the outputs: {e}");
            }
        }
    }
}

/// Convert a captured frame into tightly packed NV12.
///
/// The NV12 picture the encoder gets, with the buffer it lives in. A DMA-BUF capture is converted
/// on the GPU while the GPU works. A failure disables the GPU for the rest of the session, and the
/// frame is converted on the CPU, as are all frames of a `wl_shm` capture.
///
/// The CPU conversion runs on the CPU. The plan puts it on the GPU; the deviation is recorded as
/// D49. Returns the buffer, its width and height, and whether the GPU made it.
fn convert_frame(
    frame: &VideoFrame,
    quality: crate::settings::Quality,
    gpu: &mut Option<GpuConvert>,
) -> Result<(FrameBuffer, u32, u32, bool), String> {
    let (width, height) = (frame.width, frame.height);
    // The picture the client asked for. When it is the size of the capture,
    // the conversion is the plain one; when it is smaller, the conversion
    // scales, and a smaller picture costs less work than a big one. The
    // capture is the ceiling: a bigger picture would be invented pixels.
    let target_w = quality.width.min(width);
    let target_h = quality.height.min(height);

    if let (Some(converter), FrameBuffer::Dmabuf(image)) = (gpu.as_mut(), &frame.buffer)
        && frame.format == PixelFormat::Bgrx
    {
        match converter.convert(image, (width, height), (target_w, target_h)) {
            Ok(nv12) => return Ok((FrameBuffer::Dmabuf(nv12), target_w, target_h, true)),
            Err(e) => {
                crate::host_log!(warn "lumen-host: GPU conversion disabled for this session: {e}");
                *gpu = None;
            }
        }
    }

    let mut nv12 = vec![0u8; PixelFormat::Nv12.frame_len(target_w, target_h)];
    // The capture is already in BGRX order, so the frame is borrowed and
    // nothing is copied. Copying it would move eight megabytes a frame at
    // 1080p, and the conversion that follows reads every one of them.
    match &frame.buffer {
        FrameBuffer::Cpu { data } => {
            let bgrx = cpu_bgrx(data, frame.format)?;
            to_nv12_cpu(&bgrx, (width, height), (target_w, target_h), &mut nv12)?;
        }
        FrameBuffer::Dmabuf(image) => {
            let pixels = read_pixels(image, width, height).map_err(|e| e.to_string())?;
            let bgrx = cpu_bgrx(&pixels, frame.format)?;
            to_nv12_cpu(&bgrx, (width, height), (target_w, target_h), &mut nv12)?;
        }
    }
    Ok((FrameBuffer::Cpu { data: nv12 }, target_w, target_h, false))
}

/// Threads the unscaled CPU conversion is split over. One core takes about 7 ms for a 1080p
/// frame, which caps the capture thread near 70 fps; four bring it well under a 120 fps interval.
const CONVERT_THREADS: usize = 4;

/// Convert BGRX to NV12 on the CPU, on several threads when the picture is not scaled.
fn to_nv12_cpu(
    bgrx: &[u8],
    (width, height): (u32, u32),
    (target_w, target_h): (u32, u32),
    nv12: &mut [u8],
) -> Result<(), String> {
    if (width, height) == (target_w, target_h) {
        color::bgrx_to_nv12_threads(bgrx, width, height, nv12, CONVERT_THREADS)
    } else {
        color::bgrx_to_nv12_scaled(bgrx, width, height, target_w, target_h, nv12)
    }
}

/// Normalise a captured buffer into BGRX order.
fn cpu_bgrx(data: &[u8], format: PixelFormat) -> Result<std::borrow::Cow<'_, [u8]>, String> {
    match format {
        // The capture is already in BGRX order, so the frame is borrowed.
        // Copying it would move eight megabytes a frame at 1080p, and the
        // conversion that follows reads every one of them.
        PixelFormat::Bgrx => Ok(std::borrow::Cow::Borrowed(data)),
        PixelFormat::Rgba => {
            let mut out = Vec::with_capacity(data.len());
            for px in data.as_chunks::<4>().0 {
                out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
            Ok(std::borrow::Cow::Owned(out))
        }
        other => Err(format!("{other:?} is not a capture format")),
    }
}

/// The encode stage.
struct EncodeStage {
    config: HostConfig,
    keys: SessionKeys,
    aead: lumen_crypto::AeadKind,
    frames: Receiver<Arc<VideoFrame>>,
    commands: Receiver<EncodeCommand>,
    datagrams: SyncSender<Datagrams>,
    keyframe_requested: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    counters: Arc<Counters>,
    stages: Arc<Mutex<StageRecorder>>,
    estimator: Arc<Mutex<Estimator>>,
    /// Counts the windows the link has been lossless, which is how long the parity level waits
    /// before it falls.
    parity_hysteresis: ParityHysteresis,
    /// The settings the control plane changes.
    settings: Arc<HostSettings>,
    /// The render node the encoder opens on, published by the capture stage.
    render_node: Arc<OnceLock<Option<PathBuf>>>,
}

impl EncodeStage {
    /// The rate control the encoder runs with.
    ///
    /// The control plane chooses between a constant quantiser and a
    /// constant bitrate, and sets the value of whichever it chose.
    fn rate_control(&self) -> RateControl {
        if self.settings.constant_quantiser() {
            RateControl::Cqp {
                qp: u8::try_from(self.settings.qp()).unwrap_or(22),
            }
        } else {
            RateControl::Cbr {
                bits_per_second: self.settings.bitrate_bps().max(1),
            }
        }
    }

    /// The parity level the next frames use.
    ///
    /// In the automatic mode the level follows the measured loss. The
    /// operator can pin it instead.
    fn wanted_parity(&self, loss: f64) -> ParityLevel {
        match self.settings.parity() {
            crate::settings::ParityMode::Auto => ParityLevel::from_loss(loss),
            crate::settings::ParityMode::None => ParityLevel::None,
            crate::settings::ParityMode::Light => ParityLevel::Light,
            crate::settings::ParityMode::Heavy => ParityLevel::Heavy,
        }
    }

    /// Encode frames and slice them into sealed datagrams.
    fn run(mut self) {
        let encoder_config =
            EncoderConfig::new(self.config.width, self.config.height, self.settings.fps())
                .with_rate_control(self.rate_control());
        let mut encoder_width = self.config.width;
        let mut encoder_height = self.config.height;
        // The encoder runs on the device the capture's DMA-BUFs live on, which is known once the
        // capture is open.
        let render_node = loop {
            if let Some(node) = self.render_node.get() {
                break node.clone();
            }
            if !self.running.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mut encoder = match H264Encoder::new_on(encoder_config, render_node.as_deref()) {
            Ok(e) => e,
            Err(e) => {
                crate::host_log!(warn "lumen-host: cannot create the encoder: {e}");
                self.counters.encode_errors.fetch_add(1, Ordering::Relaxed);
                self.running.store(false, Ordering::SeqCst);
                return;
            }
        };
        let mut sender = match session::media_sender(&self.keys, self.aead) {
            Ok(s) => s,
            Err(e) => {
                crate::host_log!(warn "lumen-host: cannot create the media sender: {e}");
                self.running.store(false, Ordering::SeqCst);
                return;
            }
        };

        while self.running.load(Ordering::SeqCst) {
            while let Ok(command) = self.commands.try_recv() {
                match command {
                    EncodeCommand::Retransmit {
                        frame_id,
                        slice_index,
                    } => {
                        // The plan's deadline: the frame is not retransmitted
                        // once its capture timestamp plus 1.5 frame intervals
                        // has passed. A late frame is dropped, not waited for.
                        let interval_us = 1_000_000 / u64::from(self.settings.fps());
                        let allowed = self.stages.lock().map_or(true, |s| {
                            s.may_retransmit(frame_id, monotonic_us(), interval_us)
                        });
                        if !allowed {
                            crate::host_log!(info
                                "lumen-host: frame {frame_id} is past its deadline, not retransmitting"
                            );
                            continue;
                        }
                        match sender.retransmit(frame_id, slice_index) {
                            Ok(datagram) => {
                                self.counters.retransmits.fetch_add(1, Ordering::Relaxed);
                                // A retransmit is urgent, so it waits for room
                                // in the queue rather than being dropped.
                                for _ in 0..100 {
                                    let batch = Datagrams {
                                        frame_id,
                                        datagrams: vec![datagram.clone()],
                                    };
                                    match self.datagrams.try_send(batch) {
                                        Ok(()) => break,
                                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                            thread::sleep(Duration::from_millis(1));
                                        }
                                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                            return;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                crate::host_log!(warn "lumen-host: retransmit failed: {e}");
                            }
                        }
                    }
                }
            }
            match self.frames.recv_timeout(COMMAND_POLL) {
                Ok(frame) => {
                    if self.keyframe_requested.swap(false, Ordering::Relaxed) {
                        encoder.request_keyframe();
                    }
                    // The adaptive policies run on the estimator's window:
                    // the parity level follows the measured loss, and the
                    // bitrate follows the loss and the delay gradient.
                    // The rate is read inside the lock and applied outside
                    // it: reconfiguring the encoder takes a while, and the
                    // control stage waits for the same lock. Holding it
                    // across the reconfiguration would stall the control
                    // channel and the client would think the link is gone.
                    let new_rate = if let Ok(mut estimator) = self.estimator.lock() {
                        // The parity level is asymmetric: it rises at once
                        // when the loss asks for it, and falls only after
                        // several windows without a loss. A level that fell
                        // as soon as the loss dipped would leave the stream
                        // unprotected exactly when the link is about to get
                        // worse.
                        let wanted = self.wanted_parity(estimator.loss_rate());
                        if wanted > sender.parity() {
                            sender.set_parity(wanted);
                            self.counters
                                .parity_level_changes
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        if estimator.window_elapsed() {
                            // The loss of the window that is closing decides the fall. The
                            // parity level falls in windows, not frames, so the fall takes the
                            // same time at any frame rate.
                            let window_loss = estimator.loss_rate();
                            let wanted = self.wanted_parity(window_loss);
                            if let Some(level) = self.parity_hysteresis.on_window(
                                sender.parity(),
                                wanted,
                                window_loss,
                            ) {
                                sender.set_parity(level);
                                self.counters
                                    .parity_level_changes
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            Some(estimator.on_window_close())
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(rate) = new_rate {
                        self.counters
                            .estimated_bytes_per_sec
                            .store(rate as u64, Ordering::Relaxed);
                    }
                    // The rate reaches the encoder as soon as it differs
                    // from the one it runs with, so a change the operator
                    // makes shows up at once and not a window later. The
                    // rate only means something when the encoder runs with
                    // a constant bitrate; a constant quantiser has no rate
                    // to set.
                    if let RateControl::Cbr {
                        bits_per_second: current,
                    } = encoder.rate_control()
                    {
                        let wanted = if self.settings.adaptive_bitrate() {
                            new_rate.map(|rate| (rate * 8.0).round() as u32)
                        } else {
                            Some(self.settings.bitrate_bps())
                        };
                        if let Some(bits) = wanted
                            && bits != current
                            && let Err(e) = encoder.set_rate_control(RateControl::Cbr {
                                bits_per_second: bits,
                            })
                        {
                            crate::host_log!(warn "lumen-host: cannot set the bitrate: {e}");
                        }
                    }
                    // The encoder follows the capture. When the output
                    // changes mode the frames arrive at a new size, and the
                    // encoder has to be rebuilt for it, or every frame of
                    // the new size is refused.
                    if frame.width != encoder_width || frame.height != encoder_height {
                        let config =
                            EncoderConfig::new(frame.width, frame.height, self.settings.fps())
                                .with_rate_control(self.rate_control());
                        match H264Encoder::new_on(config, render_node.as_deref()) {
                            Ok(fresh) => {
                                crate::host_log!(info
                                    "lumen-host: the encoder is now {}x{}",
                                    frame.width, frame.height
                                );
                                encoder = fresh;
                                encoder_width = frame.width;
                                encoder_height = frame.height;
                            }
                            Err(e) => {
                                crate::host_log!(warn "lumen-host: cannot rebuild the encoder: {e}");
                                continue;
                            }
                        }
                    }
                    let encode_start = monotonic_us();
                    match encoder.encode(&frame) {
                        Ok(encoded) => {
                            self.counters.frames_encoded.fetch_add(1, Ordering::Relaxed);
                            match sender.packetize(&encoded.data, encoded.capture_ts_us) {
                                Ok(batch) => {
                                    let frame_id = sender.last_frame_id();
                                    let encode_end = monotonic_us();
                                    if let Ok(mut stages) = self.stages.lock() {
                                        stages.record_host_start(
                                            frame_id,
                                            frame.capture_ts_us,
                                            encode_start,
                                            encode_end,
                                        );
                                    }
                                    if self
                                        .datagrams
                                        .send(Datagrams {
                                            frame_id,
                                            datagrams: batch,
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Err(e) => {
                                    self.counters.encode_errors.fetch_add(1, Ordering::Relaxed);
                                    crate::host_log!(warn "lumen-host: packetize failed: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            self.counters.encode_errors.fetch_add(1, Ordering::Relaxed);
                            crate::host_log!(warn "lumen-host: encode failed: {e}");
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// The send stage.
struct SendStage {
    socket: MediaSocket,
    peer: SocketAddr,
    datagrams: Receiver<Datagrams>,
    running: Arc<AtomicBool>,
    counters: Arc<Counters>,
    stages: Arc<Mutex<StageRecorder>>,
    estimator: Arc<Mutex<Estimator>>,
}

impl SendStage {
    /// Pace the datagrams onto the media socket.
    fn run(self) {
        let mut pacer = Pacer::new(PACER_BYTES_PER_SEC, Duration::from_millis(100));
        while self.running.load(Ordering::SeqCst) {
            match self.datagrams.recv_timeout(SEND_POLL) {
                Ok(batch) => {
                    let count = batch.datagrams.len() as u64;
                    let bytes: usize = batch.datagrams.iter().map(Vec::len).sum();
                    match send_paced(&self.socket, self.peer, &batch.datagrams, &mut pacer) {
                        Ok(()) => {
                            self.counters
                                .datagrams_sent
                                .fetch_add(count, Ordering::Relaxed);
                            self.counters
                                .bytes_sent
                                .fetch_add(bytes as u64, Ordering::Relaxed);
                            let now = monotonic_us();
                            if let Ok(mut stages) = self.stages.lock() {
                                stages.record_send(batch.frame_id, now);
                            }
                            // The estimator needs to know what went out, so
                            // the loss rate is a fraction of real traffic.
                            if let Ok(mut estimator) = self.estimator.lock() {
                                estimator.on_send(bytes);
                            }
                        }
                        Err(e) => {
                            crate::host_log!(warn "lumen-host: media send failed: {e}");
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// The control stage.
struct ControlStage {
    channel: ControlChannel,
    /// Set when the control plane asks for the session to end.
    kick: Arc<AtomicBool>,
    commands: SyncSender<EncodeCommand>,
    keyframe_requested: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    counters: Arc<Counters>,
    stages: Arc<Mutex<StageRecorder>>,
    estimator: Arc<Mutex<Estimator>>,
    /// The outputs the host can capture, sent to the client.
    monitors: Arc<Mutex<Vec<crate::monitors::MonitorInfo>>>,
    /// The mode changes the capture stage saw, sent to the client.
    resolution_changes: Arc<Mutex<Vec<(u32, u32)>>>,
    /// Whether the list of outputs was already sent.
    monitors_sent: bool,
    /// The last link feedback, so the cumulative counts can be differenced.
    last_feedback: Option<(u64, u64)>,
    /// The settings the control plane changes.
    settings: Arc<HostSettings>,
    /// The size the host captures, which bounds the picture it serves.
    capture_size: (u32, u32),
    /// The text the clipboard thread found, and the text the host already
    /// sent. The clipboard is read on a thread of its own: the read waits
    /// for the compositor, and the control channel must not wait with it.
    clipboard_rx: std::sync::mpsc::Receiver<String>,
    /// The last clipboard text the host sent, so it is not sent twice.
    clipboard_sent: Option<String>,
}

impl ControlStage {
    /// Read the host's own clipboard and offer it to the client.
    ///
    /// The host is the side with the data-control manager, so it can read
    /// what the user copied. The client takes it and sends it back, which
    /// completes the round trip.
    fn send_clipboard(&mut self) {
        // The clipboard is read on a thread of its own: the read waits for
        // the compositor to answer, and the control channel must not wait
        // with it. A slow answer would delay the clock exchange and skew
        // the offset that every latency number rests on.
        let Ok(text) = self.clipboard_rx.try_recv() else {
            return;
        };
        if self.clipboard_sent.as_deref() == Some(text.as_str()) {
            return;
        }
        if self
            .channel
            .send(&ControlMessage::Clipboard { text: text.clone() })
            .is_ok()
        {
            self.clipboard_sent = Some(text);
        }
    }

    /// Answer the client and turn its requests into encoder commands.
    ///
    /// The stage also pings the client periodically, so the offset between
    /// the two clocks can be estimated and the client's stage timestamps can
    /// be mapped onto the host clock.
    fn run(mut self) {
        let mut next_ping = Instant::now();
        while self.running.load(Ordering::SeqCst) && !self.kick.load(Ordering::SeqCst) {
            if Instant::now() >= next_ping {
                next_ping = Instant::now() + PING_INTERVAL;
                let _ = self.channel.send(&ControlMessage::Ping {
                    ts_us: monotonic_us(),
                });
            }
            self.send_monitors();
            self.send_resolution_changes();
            // The clipboard is read on a thread of its own, so the control
            // loop only has to look at what it found.
            self.send_clipboard();
            match self.channel.recv_timeout(CONTROL_POLL) {
                Ok(Some(ControlMessage::RequestIdr { .. })) => {
                    self.counters
                        .keyframe_requests
                        .fetch_add(1, Ordering::Relaxed);
                    self.keyframe_requested.store(true, Ordering::Relaxed);
                }
                Ok(Some(ControlMessage::Clipboard { text })) => {
                    // The client took the clipboard and sent it back, so the
                    // round trip is complete.
                    let _ = text;
                    self.counters
                        .clipboard_roundtrips
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(Some(ControlMessage::Quality { width, height, fps })) => {
                    // The client cannot keep up at the size it has, so it
                    // asks for a smaller picture. The host scales the capture
                    // down to it, which costs less work than the big one, and
                    // the client is told so it can reallocate.
                    // A request without a frame rate keeps the one the host
                    // is running at.
                    let current = self.settings.quality();
                    if self.settings.quality_held() {
                        // The operator holds the picture. The client is told the size it
                        // gets, so its ladder starts from the held picture again.
                        if (width, height) != (current.width, current.height)
                            && let Ok(mut changes) = self.resolution_changes.lock()
                        {
                            changes.push((current.width, current.height));
                        }
                        continue;
                    }
                    let fps = if fps > 0 { fps } else { current.fps };
                    // The host never serves a picture bigger than the one it
                    // captures: the extra pixels would have to be invented,
                    // and the conversion would cost more for nothing.
                    let quality =
                        self.settings
                            .quality_within_ceiling(crate::settings::Quality::new(
                                width.min(self.capture_size.0),
                                height.min(self.capture_size.1),
                                fps,
                            ));
                    if quality != self.settings.quality() {
                        self.settings.set_quality(quality);
                        if let Ok(mut changes) = self.resolution_changes.lock() {
                            changes.push((width, height));
                        }
                        crate::host_log!(
                            info
                            "lumen-host: the client asked for {width}x{height}"
                        );
                    }
                }
                Ok(Some(ControlMessage::Nack { frame_id, slices })) => {
                    for slice_index in slices {
                        let _ = self.commands.try_send(EncodeCommand::Retransmit {
                            frame_id,
                            slice_index,
                        });
                    }
                }
                Ok(Some(ControlMessage::Ping { ts_us })) => {
                    let reply = ControlMessage::Pong {
                        ts_us,
                        reply_ts_us: monotonic_us(),
                    };
                    if let Err(e) = self.channel.send(&reply) {
                        crate::host_log!(warn "lumen-host: pong failed: {e}");
                        break;
                    }
                }
                Ok(Some(ControlMessage::LinkFeedback {
                    received,
                    lost,
                    rtt_us,
                })) => {
                    // The client tells the host what it has received so far.
                    // The counts are cumulative, so the host keeps the last
                    // ones and feeds the estimator the difference.
                    let previous = self.last_feedback.replace((received, lost));
                    let (delta_received, delta_lost) = match previous {
                        Some((prev_received, prev_lost)) => (
                            received.saturating_sub(prev_received),
                            lost.saturating_sub(prev_lost),
                        ),
                        None => (0, 0),
                    };
                    if let Ok(mut estimator) = self.estimator.lock() {
                        for _ in 0..delta_lost {
                            estimator.on_loss();
                        }
                        // The estimator counts packets, so every slice that
                        // arrived is one acknowledgement. The round trip time
                        // is the control channel's, measured by the client.
                        for _ in 0..delta_received {
                            estimator.on_ack(Duration::from_micros(rtt_us));
                        }
                    }
                }
                Ok(Some(ControlMessage::Pong { ts_us, reply_ts_us })) => {
                    // The client answered a ping: `ts_us` is the host clock
                    // when the ping went out, `reply_ts_us` is the client
                    // clock when it answered, and now is the host clock when
                    // the answer arrived.
                    let offset = estimate_offset(ts_us, reply_ts_us, monotonic_us());
                    if let Ok(mut stages) = self.stages.lock() {
                        stages.set_offset(offset);
                    }
                }
                Ok(Some(ControlMessage::StageTimes {
                    frame_id,
                    recv_ts_us,
                    decode_start_ts_us,
                    decode_ts_us,
                    present_ts_us,
                })) => {
                    if let Ok(mut stages) = self.stages.lock() {
                        stages.record_client(
                            frame_id,
                            recv_ts_us,
                            decode_start_ts_us,
                            decode_ts_us,
                            present_ts_us,
                        );
                    }
                }
                Ok(Some(ControlMessage::Bye)) => break,
                // A timeout only means that nothing arrived. The loop comes
                // back round and the ping is checked.
                Ok(Some(_) | None) => {}
                Err(e) => {
                    crate::host_log!(info "lumen-host: control channel closed: {e}");
                    break;
                }
            }
        }
        self.running.store(false, Ordering::SeqCst);
    }

    /// Send the list of outputs to the client.
    ///
    /// The list is sent once, as soon as the capture stage has read it.
    fn send_monitors(&mut self) {
        let monitors = match self.monitors.lock() {
            Ok(monitors) => monitors.clone(),
            Err(_) => return,
        };
        if monitors.is_empty() || self.monitors_sent {
            return;
        }
        for monitor in monitors {
            let message = ControlMessage::Monitor {
                index: monitor.index,
                width: monitor.width,
                height: monitor.height,
                refresh_hz: monitor.refresh_hz,
            };
            if let Err(e) = self.channel.send(&message) {
                crate::host_log!(warn "lumen-host: cannot send the monitor list: {e}");
                return;
            }
        }
        self.monitors_sent = true;
    }

    /// Send the mode changes the capture stage saw to the client.
    fn send_resolution_changes(&mut self) {
        let changes = match self.resolution_changes.lock() {
            Ok(mut changes) => std::mem::take(&mut *changes),
            Err(_) => return,
        };
        for (width, height) in changes {
            let message = ControlMessage::ResolutionChanged { width, height };
            if let Err(e) = self.channel.send(&message) {
                crate::host_log!(warn "lumen-host: cannot send the resolution change: {e}");
                return;
            }
            self.counters.resize_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Microseconds on the monotonic clock.
///
/// The same clock as the capture timestamps, so a client can compare a
/// presentation timestamp against a capture timestamp without an offset.
fn monotonic_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs * 1_000_000 + nanos / 1_000
}

/// Copy the counters into a plain snapshot.
pub(crate) fn snapshot(
    counters: &Counters,
    stage_samples: BTreeMap<String, Vec<i64>>,
) -> PipelineStats {
    PipelineStats {
        frames_captured: counters.frames_captured.load(Ordering::Relaxed),
        frames_gpu_converted: counters.frames_gpu_converted.load(Ordering::Relaxed),
        frames_encoded: counters.frames_encoded.load(Ordering::Relaxed),
        datagrams_sent: counters.datagrams_sent.load(Ordering::Relaxed),
        bytes_sent: counters.bytes_sent.load(Ordering::Relaxed),
        capture_errors: counters.capture_errors.load(Ordering::Relaxed),
        encode_errors: counters.encode_errors.load(Ordering::Relaxed),
        keyframe_requests: counters.keyframe_requests.load(Ordering::Relaxed),
        retransmits: counters.retransmits.load(Ordering::Relaxed),
        duration_ms: counters.duration_ms.load(Ordering::Relaxed),
        estimated_bytes_per_sec: counters.estimated_bytes_per_sec.load(Ordering::Relaxed),
        parity_level_changes: counters.parity_level_changes.load(Ordering::Relaxed),
        audio_packets: counters.audio_packets.load(Ordering::Relaxed),
        clipboard_roundtrips: counters.clipboard_roundtrips.load(Ordering::Relaxed),
        resize_events: counters.resize_events.load(Ordering::Relaxed),
        stage_samples,
    }
}
