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
  --media-bind <addr>  Address the media socket binds to (default 0.0.0.0:24801, the
                       port lumen-host sends to unless its --client-media-port differs)
  --title <text>       Window title (default Lumen)
  --wayland            Present through Wayland
  --x11                Present through X11
  --present <name>     Present backend: auto, wayland or x11. Auto picks X11
                       when the Wayland compositor draws no window frames
  --duration <seconds> Stop after this many seconds
  --report <path>      Write the run counters as JSON to this file
  --events <path>      Write the session events as JSON lines to this file
  --identity <path>    Where the client keeps its identity key
  --pairing-code <n>   Answer a pairing challenge with this code
  --pairing-code-file <path>  Read the pairing code from this file
  --reconnect          Come back on its own when the session ends
  --low-power          Use the low-power profile: one decode thread, a
                       depth-one queue and a 1920x1080 width guard
  --input <mode>       Which input of the viewer window reaches the host:
                       off, pointer, keyboard or full (default full)
  --no-input           Do not send the keyboard and pointer of the viewer window
                       to the host (the same as --input off)
  --fit <mode>         How the picture fills the window: fit (whole picture
                       with bars), crop (fills the window, cuts the overflow) or
                       stretch (fills the window, each axis on its own)
  --no-hud             Do not show the heads-up display in the window
  --resolution <size>  Hold this picture size, written like 1920x1080, whatever
                       the link does. Without it the client follows the quality
                       ladder and asks for a bigger picture when it can
  --fullscreen         Open the window fullscreen
  --keep-awake         Keep the screen awake while the session runs
  --wake <mac>         Send a wake-on-lan packet to this MAC address before
                       connecting, for a host that is asleep
  --discover           List the hosts that answer on mDNS and exit
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
    let (config, report, discover) = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lumen-client: {e}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    if discover {
        // The hosts that answer on mDNS, so the user does not have to type an address.
        match lumen_transport::mdns::browse(std::time::Duration::from_secs(3)) {
            Ok(hosts) if hosts.is_empty() => {
                eprintln!("lumen-client: no host answered on mDNS");
            }
            Ok(hosts) => {
                for host in hosts {
                    println!(
                        "{} {}:{} {}",
                        host.name,
                        host.address,
                        host.port,
                        host.fingerprint.as_deref().unwrap_or("")
                    );
                }
            }
            Err(e) => {
                eprintln!("lumen-client: cannot browse on mDNS: {e}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    eprintln!(
        "lumen-client: host {}, media bind {}",
        config.host, config.media_addr
    );

    let pipeline = ClientPipeline::new(config);
    match pipeline.run() {
        Ok(stats) => {
            eprintln!(
                "lumen-client: {} datagrams, {} access units ({} rebuilt from parity), \
                 {} decoded, {} presented, {} decode errors, {} keyframe requests",
                stats.datagrams_received,
                stats.access_units,
                stats.frames_rebuilt,
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
fn parse(args: &[String]) -> Result<(ClientConfig, Option<std::path::PathBuf>, bool), ArgError> {
    let mut config = ClientConfig::default();
    let mut backend = None;
    let mut report = None;
    let mut identity = None;
    let mut pairing_code = None;
    let mut pairing_code_file = None;
    let mut events = None;
    // The hosts that answer on mDNS, for `--discover`.
    let mut discover = false;

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
                backend = match choice {
                    lumen_host_service::backend::BackendChoice::Auto => None,
                    lumen_host_service::backend::BackendChoice::Wayland => {
                        Some(RenderBackend::Wayland)
                    }
                    lumen_host_service::backend::BackendChoice::X11 => Some(RenderBackend::X11),
                };
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
            "--no-input" => config.input_mode = lumen_client_desktop::input::InputMode::Off,
            "--input" => {
                let name = value("--input")?;
                config.input_mode = lumen_client_desktop::input::InputMode::parse(&name)
                    .ok_or_else(|| {
                        ArgError::Invalid(format!(
                            "--input: {name} is not off, pointer, keyboard or full"
                        ))
                    })?;
            }
            "--fit" => {
                let name = value("--fit")?;
                config.fit = lumen_render::VideoFit::parse(&name).ok_or_else(|| {
                    ArgError::Invalid(format!("--fit: {name} is not fit, crop or stretch"))
                })?;
            }
            "--no-hud" => config.hud = false,
            "--resolution" => {
                let name = value("--resolution")?;
                config.resolution = lumen_client_desktop::ladder::ResolutionPolicy::hold(&name)
                    .ok_or_else(|| {
                        ArgError::Invalid(format!(
                            "--resolution: {name} is not a size like 1920x1080"
                        ))
                    })?;
            }
            "--fullscreen" => config.fullscreen = true,
            "--keep-awake" => config.keep_awake = true,
            "--discover" => discover = true,
            "--wake" => {
                config.wake = Some(
                    lumen_transport::MacAddr::parse(&value("--wake")?)
                        .map_err(ArgError::Invalid)?,
                );
            }
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
    Ok((config, report, discover))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_client_desktop::input::InputMode;
    use lumen_render::VideoFit;

    /// The configuration of the given arguments.
    fn config_of(args: &[&str]) -> ClientConfig {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        parse(&args).expect("the arguments parse").0
    }

    #[test]
    fn the_defaults_show_the_picture_with_bars_and_send_the_whole_input() {
        let config = config_of(&[]);
        assert_eq!(config.fit, VideoFit::Fit);
        assert!(config.hud);
        assert!(!config.fullscreen);
        assert!(!config.keep_awake);
        assert_eq!(config.input_mode, InputMode::Full);
    }

    #[test]
    fn the_fit_and_the_input_mode_are_read() {
        let config = config_of(&["--fit", "crop", "--input", "pointer"]);
        assert_eq!(config.fit, VideoFit::Crop);
        assert_eq!(config.input_mode, InputMode::Pointer);
        let config = config_of(&["--fit", "stretch"]);
        assert_eq!(config.fit, VideoFit::Stretch);
    }

    #[test]
    fn no_input_is_the_same_as_the_off_mode() {
        assert_eq!(config_of(&["--no-input"]).input_mode, InputMode::Off);
        assert_eq!(config_of(&["--input", "off"]).input_mode, InputMode::Off);
    }

    #[test]
    fn the_hud_the_fullscreen_and_the_awake_are_read() {
        let config = config_of(&["--no-hud", "--fullscreen", "--keep-awake"]);
        assert!(!config.hud);
        assert!(config.fullscreen);
        assert!(config.keep_awake);
    }

    #[test]
    fn an_unknown_fit_or_mode_is_refused() {
        let args = ["--fit", "zoom"].map(String::from);
        assert!(parse(&args).is_err());
        let args = ["--input", "mouse"].map(String::from);
        assert!(parse(&args).is_err());
        let args = ["--fit"].map(String::from);
        assert!(parse(&args).is_err());
    }
}
