//! The `lumen-host` daemon.
//!
//! Accepts one client, captures the screen, encodes it and streams it out.

use std::process::ExitCode;
use std::time::Duration;

use lumen_encode_vaapi::RateControl;
use lumen_host_service::host_log;
use lumen_host_service::pairing::PairingStore;
use lumen_host_service::pipeline::{HostConfig, HostPipeline, PipelineStats};
use lumen_host_service::session::PairingMode;

/// Usage text for the command line.
const USAGE: &str = "\
lumen-host - Lumen remote desktop host

Usage: lumen-host [options]

Options:
  --control <addr>        Control listener address (default 0.0.0.0:24800)
  --media <addr>          Media socket address (default 0.0.0.0:0)
  --client-media-port <n> Port the client receives media on (default 24801)
  --output <name>         Output connector to capture, e.g. HEADLESS-1
  --size <WxH>            Capture size (default 1280x720)
  --fps <n>               Stream rate (default 30)
  --qp <n>                Constant quantiser, 0..51 (default 22)
  --bitrate <bps>         Constant bitrate instead of a constant quantiser
  --web-listen <addr>     Where the control plane listens (default: none).
                           'none' turns it off. The host prints the URL
                           with the token the dashboard needs.
  --cpu                   Capture through wl_shm instead of DMA-BUF
  --hold                  Hold the --size picture: the client cannot ask for a smaller one
  --duration <seconds>    Stop after this many seconds
  --report <path>         Write the run counters as JSON to this file
  --pairing-store <path>  Host identity and trusted clients (default ~/.local/state/lumen/pairing.json)
  --pairing <mode>        prompt or refuse (default prompt)
  --pairing-code-file <path>  Write the pairing code shown to this file
  --advertise <addr>        Advertise the host over mDNS on this address, so a
                            client finds it without being told one
  --no-advertise            Do not advertise the host over mDNS
  --virtual-output        Add a headless output of --size at --fps to the running sway,
                        capture it, and remove it on exit
  --capture <name>      Capture backend: auto, wayland, x11 or portal (default auto).
                        portal asks the ScreenCast portal once and keeps the grant's
                        restore token next to the pairing store
  --encoder <name>      Hardware encoder: vaapi or nvenc (default vaapi)
  --display <name>      The X display to capture from, e.g. :77
  --persist-timeout <s> Keep the session this long after the client left
  --max-sessions <n>    How many clients may be served at the same time
  --shadow-sessions <n> How many of them may be shadow sessions
  --list-outputs          Print the available outputs and exit
  -h, --help              Print this text
";

/// Parsed command line.
struct Args {
    config: HostConfig,
    list_outputs: bool,
    report: Option<std::path::PathBuf>,
    pairing_store: std::path::PathBuf,
    pairing_mode: PairingMode,
    /// The address the host advertises over mDNS; `None` means no advertisement.
    advertise: Option<std::net::SocketAddr>,
    /// Add a headless output of this size to the running sway and capture it.
    virtual_output: bool,
}

/// Errors from the command line.
#[derive(Debug)]
enum ArgError {
    /// A flag is not known.
    Unknown(String),
    /// A flag is missing its value.
    MissingValue(String),
    /// A value is not valid.
    Invalid(String),
}

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(flag) => write!(f, "unknown option {flag}"),
            Self::MissingValue(flag) => write!(f, "option {flag} needs a value"),
            Self::Invalid(why) => write!(f, "{why}"),
        }
    }
}

/// Parse the command line.
fn parse(args: &[String]) -> Result<Args, ArgError> {
    let mut config = HostConfig {
        // `--capture` defaults to auto: an X11 session has no compositor to screencopy.
        capture: lumen_host_service::backend::detect_display_server()
            .unwrap_or(lumen_host_service::backend::DisplayServer::Wayland),
        ..HostConfig::default()
    };
    let mut list_outputs = false;
    let mut report = None;
    let mut pairing_store = default_state_dir().join("pairing.json");
    let mut portal = false;
    let mut virtual_output = false;
    let mut pairing_mode = PairingMode::Prompt;
    let mut pairing_code_file = None;
    // The address the host advertises over mDNS; none means no advertisement.
    let mut advertise = Some("0.0.0.0:24800".parse().expect("a valid default address"));
    let mut width = config.width;
    let mut height = config.height;
    let mut qp = 22u8;
    let mut bitrate = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String, ArgError> {
            it.next()
                .cloned()
                .ok_or_else(|| ArgError::MissingValue(flag.to_string()))
        };
        match arg.as_str() {
            "--control" => {
                config.control_addr = value("--control")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--control: {e}")))?;
            }
            "--media" => {
                config.media_addr = value("--media")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--media: {e}")))?;
            }
            "--with-audio" => {
                config.audio = true;
            }
            "--web-listen" => {
                let address = value("--web-listen")?;
                if address == "none" {
                    config.web_addr = None;
                } else {
                    config.web_addr = Some(
                        address
                            .parse()
                            .map_err(|e| ArgError::Invalid(format!("--web-listen: {e}")))?,
                    );
                }
            }
            "--clipboard-test" => {
                config.clipboard_test = Some(value("--clipboard-test")?);
            }
            "--audio" => {
                config.audio_addr = value("--audio")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--audio: {e}")))?;
            }
            "--client-audio-port" => {
                config.client_audio_port = value("--client-audio-port")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--client-audio-port: {e}")))?;
            }
            "--client-media-port" => {
                config.client_media_port = value("--client-media-port")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--client-media-port: {e}")))?;
            }
            "--output" => config.output = Some(value("--output")?),
            "--virtual-output" => virtual_output = true,
            "--size" => {
                let size = value("--size")?;
                let (w, h) = size
                    .split_once('x')
                    .ok_or_else(|| ArgError::Invalid("--size wants WIDTHxHEIGHT".into()))?;
                width = w
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--size width: {e}")))?;
                height = h
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--size height: {e}")))?;
            }
            "--fps" => {
                config.fps = value("--fps")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--fps: {e}")))?;
            }
            "--qp" => {
                qp = value("--qp")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--qp: {e}")))?;
                if qp > 51 {
                    return Err(ArgError::Invalid("--qp must be 0..51".into()));
                }
            }
            "--bitrate" => {
                bitrate = Some(
                    value("--bitrate")?
                        .parse()
                        .map_err(|e| ArgError::Invalid(format!("--bitrate: {e}")))?,
                );
            }
            "--cpu" => config.prefer_cpu = true,
            "--hold" => config.hold_quality = true,
            "--report" => report = Some(std::path::PathBuf::from(value("--report")?)),
            "--pairing-store" => {
                pairing_store = std::path::PathBuf::from(value("--pairing-store")?);
            }
            "--advertise" => {
                advertise = Some(
                    value("--advertise")?
                        .parse()
                        .map_err(|e| ArgError::Invalid(format!("--advertise: {e}")))?,
                );
            }
            "--no-advertise" => advertise = None,
            "--pairing-code-file" => {
                pairing_code_file = Some(std::path::PathBuf::from(value("--pairing-code-file")?));
            }
            "--pairing" => {
                pairing_mode = match value("--pairing")?.as_str() {
                    "prompt" => PairingMode::Prompt,
                    "refuse" => PairingMode::Refuse,
                    other => {
                        return Err(ArgError::Invalid(format!(
                            "--pairing must be prompt or refuse, got {other}"
                        )));
                    }
                };
            }
            "--duration" => {
                let seconds: f64 = value("--duration")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--duration: {e}")))?;
                config.max_duration = Some(Duration::from_secs_f64(seconds));
            }
            "--encoder" => {
                config.encoder =
                    lumen_host_service::pipeline::EncoderBackend::parse(&value("--encoder")?)
                        .map_err(ArgError::Invalid)?;
            }
            "--capture" => {
                let name = value("--capture")?;
                if name == "portal" {
                    // The portal is a Wayland session for injection; the token sits by the
                    // pairing store, set once the arguments are read.
                    config.capture = lumen_host_service::backend::DisplayServer::Wayland;
                    portal = true;
                    continue;
                }
                let choice = lumen_host_service::backend::BackendChoice::parse(&name)
                    .map_err(ArgError::Invalid)?;
                config.capture = match choice {
                    lumen_host_service::backend::BackendChoice::Auto => {
                        lumen_host_service::backend::detect_display_server()
                            .unwrap_or(lumen_host_service::backend::DisplayServer::Wayland)
                    }
                    lumen_host_service::backend::BackendChoice::Wayland => {
                        lumen_host_service::backend::DisplayServer::Wayland
                    }
                    lumen_host_service::backend::BackendChoice::X11 => {
                        lumen_host_service::backend::DisplayServer::X11
                    }
                };
            }
            "--display" => config.display = Some(value("--display")?),
            "--persist-timeout" => {
                let seconds: f64 = value("--persist-timeout")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--persist-timeout: {e}")))?;
                config.persist_timeout = Duration::from_secs_f64(seconds);
            }
            "--max-sessions" => {
                config.max_sessions = value("--max-sessions")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--max-sessions: {e}")))?;
                if config.max_sessions == 0 {
                    return Err(ArgError::Invalid(
                        "--max-sessions must be at least 1".into(),
                    ));
                }
            }
            "--shadow-sessions" => {
                config.shadow_sessions = value("--shadow-sessions")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--shadow-sessions: {e}")))?;
            }
            "--list-outputs" => list_outputs = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(ArgError::Unknown(other.to_string())),
        }
    }
    config.width = width;
    config.height = height;
    config.pairing_code_file = pairing_code_file;
    if portal {
        config.portal_token = Some(pairing_store.with_file_name("portal-token"));
    }
    config.rate_control = match bitrate {
        Some(bps) => RateControl::Cbr {
            bits_per_second: bps,
        },
        None => RateControl::Cqp { qp },
    };
    Ok(Args {
        config,
        list_outputs,
        report,
        pairing_store,
        pairing_mode,
        advertise,
        virtual_output,
    })
}

/// The directory the host keeps its state in.
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

/// Write the run counters as JSON.
fn write_report(path: &std::path::Path, stats: &PipelineStats) -> ExitCode {
    let json = match serde_json::to_string_pretty(stats) {
        Ok(j) => j,
        Err(e) => {
            host_log!(warn "lumen-host: cannot encode the report: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        host_log!(warn "lumen-host: cannot create {}: {e}", parent.display());
        return ExitCode::FAILURE;
    }
    match std::fs::write(path, format!("{json}\n")) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            host_log!(warn "lumen-host: cannot write {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// Run the daemon.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            host_log!(info "lumen-host: {e}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    if args.list_outputs {
        return match lumen_host_service::monitors::list(
            match args.config.capture {
                lumen_host_service::backend::DisplayServer::X11 => {
                    lumen_host_service::monitors::MonitorSource::X11
                }
                lumen_host_service::backend::DisplayServer::Wayland => {
                    lumen_host_service::monitors::MonitorSource::Wayland
                }
            },
            args.config.display.as_deref(),
        ) {
            Ok(outputs) => {
                for o in outputs {
                    // The size is the second field, so a script can read it
                    // with `awk '{print $2}'`.
                    println!("{} {}x{} @ {} Hz", o.name, o.width, o.height, o.refresh_hz);
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                host_log!(warn "lumen-host: cannot list outputs: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let mut config = args.config;
    // Held until the host exits: dropping it unplugs the output.
    let _virtual_output = if args.virtual_output {
        match lumen_host_service::virtual_display::SessionOutput::create(
            config.width,
            config.height,
            config.fps,
        ) {
            Ok(output) => {
                host_log!(info "lumen-host: added the virtual output {}", output.name());
                config.output = Some(output.name().to_owned());
                Some(output)
            }
            Err(e) => {
                host_log!(warn "lumen-host: cannot add a virtual output: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    host_log!(info
        "lumen-host: control {}, media client port {}, capture {}x{} at {} fps",
        config.control_addr, config.client_media_port, config.width, config.height, config.fps
    );

    let pairing = match PairingStore::load(&args.pairing_store) {
        Ok(p) => p,
        Err(e) => {
            host_log!(warn "lumen-host: cannot use the pairing store: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The host advertises itself, so a client finds it without an address. The advertisement
    // carries the identity fingerprint, so a client can tell two hosts apart.
    if let Some(address) = args.advertise {
        let port = address.port();
        let fingerprint = pairing.public_bytes();
        match lumen_transport::mdns::Advertiser::advertise(
            port,
            &host_name(),
            &fingerprint.iter().fold(String::new(), |mut text, byte| {
                use std::fmt::Write;
                let _ = write!(text, "{byte:02x}");
                text
            }),
            address.ip(),
        ) {
            Ok(advertiser) => {
                // The advertisement lives as long as the host does.
                let _advertiser = advertiser;
                host_log!(info "lumen-host: advertising on mDNS as {port}");
            }
            Err(e) => host_log!(warn "lumen-host: cannot advertise on mDNS: {e}"),
        }
    }
    let mut pipeline = match HostPipeline::new(config, pairing, args.pairing_mode) {
        Ok(p) => p,
        Err(e) => {
            host_log!(info "lumen-host: {e}");
            return ExitCode::FAILURE;
        }
    };
    match pipeline.run() {
        Ok(stats) => {
            host_log!(info
                "lumen-host: captured {}, encoded {}, sent {} datagrams ({} bytes), \
                 {} capture errors, {} encode errors, {} retransmits",
                stats.frames_captured,
                stats.frames_encoded,
                stats.datagrams_sent,
                stats.bytes_sent,
                stats.capture_errors,
                stats.encode_errors,
                stats.retransmits
            );
            if let Some(path) = &args.report
                && write_report(path, &stats) != ExitCode::SUCCESS
            {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            host_log!(info "lumen-host: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The name the host advertises: the machine's name, or "lumen" when it has none.
fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "lumen".to_owned())
}
