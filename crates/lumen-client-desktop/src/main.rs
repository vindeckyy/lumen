//! The `lumen-client` daemon.
//!
//! Connects to a Lumen host and presents its screen.

use std::process::ExitCode;
use std::time::Duration;

use lumen_client_desktop::{ClientConfig, ClientPipeline, ClientStats};
use lumen_render::RenderBackend;

/// Usage text for the command line.
const USAGE: &str = "\
lumen-client - Lumen remote desktop client

Usage: lumen-client [options]

Options:
  --host <addr>        Address of the host's control listener (default 127.0.0.1:24800)
  --media-bind <addr>  Address the media socket binds to (default 0.0.0.0:0, any port)
  --title <text>       Window title (default Lumen)
  --wayland            Present through Wayland
  --x11                Present through X11
  --present <name>     Present backend: auto, wayland or x11
  --duration <seconds> Stop after this many seconds
  --report <path>      Write the run counters as JSON to this file
  --events <path>      Write the session events as JSON lines to this file
  --identity <path>    Where the client keeps its identity key
  --pairing-code <n>   Answer a pairing challenge with this code
  --pairing-code-file <path>  Read the pairing code from this file
  --reconnect          Come back on its own when the session ends
  --low-power          Use the low-power profile: one decode thread, a
                       depth-one queue and a 1920x1080 width guard
  -h, --help           Print this text
";

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

/// Run the daemon.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (config, report) = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lumen-client: {e}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    eprintln!(
        "lumen-client: host {}, media bind {}",
        config.host, config.media_addr
    );

    let pipeline = ClientPipeline::new(config);
    match pipeline.run() {
        Ok(stats) => {
            eprintln!(
                "lumen-client: {} datagrams, {} access units ({} recovered by FEC), \
                 {} decoded, {} presented, {} decode errors, {} keyframe requests",
                stats.datagrams_received,
                stats.access_units,
                stats.fec_recoveries,
                stats.frames_decoded,
                stats.frames_presented,
                stats.decode_errors,
                stats.idr_requests
            );
            if let Some(path) = &report
                && write_report(path, &stats) != ExitCode::SUCCESS
            {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("lumen-client: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Write the run counters as JSON.
fn write_report(path: &std::path::Path, stats: &ClientStats) -> ExitCode {
    let json = match serde_json::to_string_pretty(stats) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("lumen-client: cannot encode the report: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("lumen-client: cannot create {}: {e}", parent.display());
        return ExitCode::FAILURE;
    }
    match std::fs::write(path, format!("{json}\n")) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lumen-client: cannot write {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// Parse the command line.
fn parse(args: &[String]) -> Result<(ClientConfig, Option<std::path::PathBuf>), ArgError> {
    let mut config = ClientConfig::default();
    let mut backend = None;
    let mut report = None;
    let mut identity = None;
    let mut pairing_code = None;
    let mut pairing_code_file = None;
    let mut events = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String, ArgError> {
            it.next()
                .cloned()
                .ok_or_else(|| ArgError::MissingValue(flag.to_string()))
        };
        match arg.as_str() {
            "--host" => {
                config.host = value("--host")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--host: {e}")))?;
            }
            "--with-audio" => {
                config.audio = true;
            }
            "--audio-bind" => {
                config.audio_addr = value("--audio-bind")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--audio-bind: {e}")))?;
            }
            "--media-bind" => {
                config.media_addr = value("--media-bind")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--media-bind: {e}")))?;
            }
            "--title" => config.title = value("--title")?,
            "--wayland" => backend = Some(RenderBackend::Wayland),
            "--x11" => backend = Some(RenderBackend::X11),
            "--present" => {
                let choice =
                    lumen_host_service::backend::BackendChoice::parse(&value("--present")?)
                        .map_err(ArgError::Invalid)?;
                backend = Some(match choice {
                    lumen_host_service::backend::BackendChoice::Auto => default_backend(),
                    lumen_host_service::backend::BackendChoice::Wayland => RenderBackend::Wayland,
                    lumen_host_service::backend::BackendChoice::X11 => RenderBackend::X11,
                });
            }
            "--report" => report = Some(std::path::PathBuf::from(value("--report")?)),
            "--events" => events = Some(std::path::PathBuf::from(value("--events")?)),
            "--identity" => identity = Some(std::path::PathBuf::from(value("--identity")?)),
            "--pairing-code" => {
                pairing_code = Some(
                    value("--pairing-code")?
                        .parse()
                        .map_err(|e| ArgError::Invalid(format!("--pairing-code: {e}")))?,
                );
            }
            "--pairing-code-file" => {
                pairing_code_file = Some(std::path::PathBuf::from(value("--pairing-code-file")?));
            }
            "--reconnect" => config.reconnect = true,
            "--low-power" => config.low_power = true,
            "--duration" => {
                let seconds: f64 = value("--duration")?
                    .parse()
                    .map_err(|e| ArgError::Invalid(format!("--duration: {e}")))?;
                config.max_duration = Some(Duration::from_secs_f64(seconds));
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(ArgError::Unknown(other.to_string())),
        }
    }
    config.backend = backend;
    if let Some(path) = identity {
        config.identity = path;
    }
    config.pairing_code = pairing_code;
    config.pairing_code_file = pairing_code_file;
    config.events = events;
    Ok((config, report))
}

/// Pick the display server from the environment.
fn default_backend() -> RenderBackend {
    match lumen_host_service::backend::detect_display_server() {
        Some(lumen_host_service::backend::DisplayServer::Wayland) => RenderBackend::Wayland,
        _ => RenderBackend::X11,
    }
}
