//! One streaming session: a `lumen-client` process the app starts and follows.
//!
//! The client runs as its own process, so a failing decoder or window cannot take the app
//! down. The app follows it through the JSON lines of its `--events` file and keeps the last
//! lines of its standard error for the details of a failure. A pairing code goes to the
//! client's standard input, which is where it reads one when no code was given on its command
//! line.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};

use crate::store::{Backend, Settings};

/// How many lines of the client's standard error are kept for the details of a failure.
const TAIL_LINES: usize = 30;

/// What the client reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The client is opening a connection.
    Connecting,
    /// The host asks to pair; the detail is the host's fingerprint (16 hex digits).
    PairingRequested(String),
    /// The session is up: the stream window opens with the first frame.
    Established,
    /// A connection failed or a session ended, and the client tries again.
    Retrying(Option<String>),
    /// The user closed the stream window.
    Closed,
    /// The client gave up; the detail is its error.
    Failed(String),
}

/// What the app needs to start a client.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The `lumen-client` program.
    pub program: PathBuf,
    /// The host's control address.
    pub host: SocketAddr,
    /// The window title.
    pub title: String,
    /// The client identity for this host.
    pub identity: PathBuf,
    /// Where the client writes its events.
    pub events: PathBuf,
    /// The MAC address to wake, if any.
    pub mac: Option<String>,
    /// The session settings.
    pub settings: Settings,
}

/// The command line of a client for `launch`.
#[must_use]
pub fn client_args(launch: &Launch) -> Vec<String> {
    let mut args = vec![
        "--host".to_owned(),
        launch.host.to_string(),
        "--identity".to_owned(),
        launch.identity.display().to_string(),
        "--events".to_owned(),
        launch.events.display().to_string(),
        "--title".to_owned(),
        launch.title.clone(),
    ];
    // An IPv6 host needs an IPv6 media socket, on the port lumen-host sends to.
    if launch.host.is_ipv6() {
        args.extend(["--media-bind".to_owned(), "[::]:24801".to_owned()]);
    }
    let settings = &launch.settings;
    match settings.backend {
        Backend::Auto => {}
        Backend::Wayland => args.extend(["--present".to_owned(), "wayland".to_owned()]),
        Backend::X11 => args.extend(["--present".to_owned(), "x11".to_owned()]),
    }
    if !settings.input {
        args.push("--no-input".to_owned());
    }
    if settings.audio {
        args.push("--with-audio".to_owned());
    }
    if settings.reconnect {
        args.push("--reconnect".to_owned());
    }
    if settings.low_power {
        args.push("--low-power".to_owned());
    }
    if let Some(mac) = &launch.mac {
        args.extend(["--wake".to_owned(), mac.clone()]);
    }
    args
}

/// Reads one line of the client's event file.
#[must_use]
pub fn parse_event(line: &str) -> Option<Update> {
    #[derive(serde::Deserialize)]
    struct Event {
        event: String,
        detail: Option<String>,
    }
    let event: Event = serde_json::from_str(line).ok()?;
    match event.event.as_str() {
        "connecting" => Some(Update::Connecting),
        "pairing-requested" => Some(Update::PairingRequested(event.detail.unwrap_or_default())),
        "established" => Some(Update::Established),
        "connect-failed" | "session-ended" => Some(Update::Retrying(event.detail)),
        "closed" => Some(Update::Closed),
        "failed" => Some(Update::Failed(event.detail.unwrap_or_default())),
        _ => None,
    }
}

/// A failure, said for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trouble {
    /// A short title.
    pub title: &'static str,
    /// What happened and what to do about it.
    pub advice: String,
}

/// Turns the client's error into words for the user. `host` is the address that was tried.
#[must_use]
pub fn explain(error: &str, host: &str) -> Trouble {
    let has = |needle: &str| error.contains(needle);
    if has("Connection refused") {
        Trouble {
            title: "Host Not Reachable",
            advice: format!(
                "Nothing answered at {host}. Make sure lumen-host is running on that computer \
                 and that the address and port are right."
            ),
        }
    } else if has("No route to host")
        || has("Network is unreachable")
        || has("timed out")
        || has("Host is unreachable")
    {
        Trouble {
            title: "Host Not Reachable",
            advice: format!(
                "The network cannot reach {host}. If the computer is asleep, add its MAC \
                 address to the host so Lumen can wake it."
            ),
        }
    } else if has("pairing was refused") || has("not paired") || has("did not prove") {
        Trouble {
            title: "Pairing Failed",
            advice: "The code did not match, or pairing was cancelled. Connect again to get a \
                     new code from the host."
                .to_owned(),
        }
    } else if has("host pin") || has("not the one this client paired with") {
        Trouble {
            title: "Host Identity Changed",
            advice: "This is not the host this computer paired with. If the host was \
                     reinstalled, forget it here and add it again; otherwise do not connect."
                .to_owned(),
        }
    } else if has("renderer failed") {
        Trouble {
            title: "Cannot Open the Stream Window",
            advice: "Lumen could not open a window on this desktop. Try another window system \
                     in Preferences."
                .to_owned(),
        }
    } else if has("decoder failed") || has("VA-API") || has("libva") {
        Trouble {
            title: "Video Decoding Unavailable",
            advice: "Lumen decodes video with VA-API. Install the VA-API driver of this \
                     computer's graphics card (mesa-va-drivers, intel-media-driver or \
                     nvidia-vaapi-driver)."
                .to_owned(),
        }
    } else if has("Connection reset") || has("Broken pipe") || has("closed") {
        Trouble {
            title: "Connection Lost",
            advice: format!("The connection to {host} dropped."),
        }
    } else {
        Trouble {
            title: "Connection Failed",
            advice: format!("Lumen could not stream from {host}."),
        }
    }
}

/// A running client.
#[derive(Debug)]
pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    events: PathBuf,
    offset: u64,
    partial: String,
    stderr: Receiver<String>,
    tail: VecDeque<String>,
    /// The last error the client reported in its events.
    failure: Option<String>,
}

impl Session {
    /// Starts the client.
    ///
    /// # Errors
    /// Returns a description when the program cannot be started.
    pub fn start(launch: &Launch) -> Result<Self, String> {
        let _ = std::fs::remove_file(&launch.events);
        let mut child = Command::new(&launch.program)
            .args(client_args(launch))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", launch.program.display()))?;
        let stdin = child.stdin.take();
        let (tx, stderr) = channel();
        if let Some(pipe) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Ok(Self {
            child,
            stdin,
            events: launch.events.clone(),
            offset: 0,
            partial: String::new(),
            stderr,
            tail: VecDeque::new(),
            failure: None,
        })
    }

    /// The updates since the last call.
    pub fn poll(&mut self) -> Vec<Update> {
        while let Ok(line) = self.stderr.try_recv() {
            if self.tail.len() == TAIL_LINES {
                self.tail.pop_front();
            }
            self.tail.push_back(line);
        }
        let mut updates = Vec::new();
        for line in self.new_event_lines() {
            if let Some(update) = parse_event(&line) {
                if let Update::Failed(detail) = &update {
                    self.failure = Some(detail.clone());
                }
                updates.push(update);
            }
        }
        updates
    }

    /// Reads the whole lines appended to the event file since the last read.
    fn new_event_lines(&mut self) -> Vec<String> {
        let Ok(mut file) = std::fs::File::open(&self.events) else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut text = String::new();
        let Ok(read) = file.read_to_string(&mut text) else {
            return Vec::new();
        };
        self.offset += read as u64;
        self.partial.push_str(&text);
        let Some(end) = self.partial.rfind('\n') else {
            return Vec::new();
        };
        let complete: String = self.partial.drain(..=end).collect();
        complete.lines().map(str::to_owned).collect()
    }

    /// Sends the pairing code the user typed.
    pub fn answer(&mut self, code: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{code}");
            let _ = stdin.flush();
        }
    }

    /// The exit status, once the client has exited.
    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// The error the client reported in its events, if it gave up.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The last line the client printed, for a client that died without reporting an error.
    #[must_use]
    pub fn last_line(&self) -> String {
        self.tail
            .iter()
            .rev()
            .find(|l| l.starts_with("lumen-client:"))
            .map(|l| l.trim_start_matches("lumen-client:").trim().to_owned())
            .unwrap_or_default()
    }

    /// The last lines of the client's standard error.
    #[must_use]
    pub fn log(&self) -> String {
        self.tail
            .iter()
            .filter(|l| !l.starts_with("libva info") && !l.starts_with("TRACE"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Ends the session. Closing the client's sockets ends it on the host at once.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.exited().is_none() {
            self.stop();
        }
        let _ = std::fs::remove_file(&self.events);
    }
}

/// The `lumen-client` program: `$LUMEN_CLIENT`, the one next to this program, or the one on
/// the `PATH`.
#[must_use]
pub fn client_program() -> PathBuf {
    if let Some(path) = std::env::var_os("LUMEN_CLIENT").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(me) = std::env::current_exe()
        && let Some(sibling) = me.parent().map(|dir| dir.join("lumen-client"))
        && sibling.is_file()
    {
        return sibling;
    }
    PathBuf::from("lumen-client")
}

/// A fresh event file in the runtime directory.
#[must_use]
pub fn events_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from);
    dir.join(format!("lumen-session-{}.jsonl", crate::store::new_id()))
}

/// Groups a hex fingerprint by four for reading aloud: `1a2b 3c4d …`.
#[must_use]
pub fn group_fingerprint(hex: &str) -> String {
    hex.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `short` (the pairing fingerprint) is the start of `full` (an mDNS fingerprint).
#[must_use]
pub fn fingerprint_matches(short: &str, full: &str) -> bool {
    !short.is_empty()
        && full
            .to_ascii_lowercase()
            .starts_with(&short.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(settings: Settings) -> Launch {
        Launch {
            program: "lumen-client".into(),
            host: "10.0.0.5:24800".parse().unwrap(),
            title: "Desk — Lumen".into(),
            identity: "/s/hosts/a/identity".into(),
            events: "/r/e.jsonl".into(),
            mac: None,
            settings,
        }
    }

    #[test]
    fn the_settings_become_client_flags() {
        let args = client_args(&launch(Settings::default()));
        assert_eq!(
            args,
            [
                "--host",
                "10.0.0.5:24800",
                "--identity",
                "/s/hosts/a/identity",
                "--events",
                "/r/e.jsonl",
                "--title",
                "Desk — Lumen",
                "--with-audio",
                "--reconnect",
            ]
        );
        let mut l = launch(Settings {
            backend: Backend::X11,
            input: false,
            audio: false,
            reconnect: false,
            low_power: true,
        });
        l.mac = Some("aa:bb:cc:dd:ee:ff".into());
        l.host = "[fe80::1]:24800".parse().unwrap();
        let args = client_args(&l);
        for flags in [
            &["--present", "x11"][..],
            &["--no-input"],
            &["--low-power"],
            &["--wake", "aa:bb:cc:dd:ee:ff"],
            &["--media-bind", "[::]:24801"],
        ] {
            assert!(
                args.windows(flags.len()).any(|w| w == flags),
                "{flags:?} in {args:?}"
            );
        }
        assert!(
            !args
                .iter()
                .any(|a| a == "--with-audio" || a == "--reconnect")
        );
    }

    #[test]
    fn client_events_become_updates() {
        let line = |event: &str, detail: Option<&str>| {
            serde_json::json!({
                "event": event, "elapsed_ms": 1, "sessions": 0, "attempts": 1,
                "connect_ms": null, "detail": detail,
            })
            .to_string()
        };
        assert_eq!(
            parse_event(&line("connecting", None)),
            Some(Update::Connecting)
        );
        assert_eq!(
            parse_event(&line("pairing-requested", Some("1a2b"))),
            Some(Update::PairingRequested("1a2b".into()))
        );
        assert_eq!(
            parse_event(&line("established", None)),
            Some(Update::Established)
        );
        assert_eq!(
            parse_event(&line("connect-failed", Some("refused"))),
            Some(Update::Retrying(Some("refused".into())))
        );
        assert_eq!(
            parse_event(&line("session-ended", None)),
            Some(Update::Retrying(None))
        );
        assert_eq!(parse_event(&line("closed", None)), Some(Update::Closed));
        assert_eq!(
            parse_event(&line("failed", Some("x"))),
            Some(Update::Failed("x".into()))
        );
        assert_eq!(parse_event(&line("progress", None)), None);
        assert_eq!(parse_event("not json"), None);
    }

    #[test]
    fn client_errors_are_explained() {
        let host = "10.0.0.5:24800";
        let title = |e: &str| explain(e, host).title;
        assert_eq!(
            title("session failed: io error: Connection refused (os error 111)"),
            "Host Not Reachable"
        );
        assert_eq!(
            title("io error: No route to host (os error 113)"),
            "Host Not Reachable"
        );
        assert_eq!(
            title("session failed: the pairing was refused"),
            "Pairing Failed"
        );
        assert_eq!(
            title("session failed: the client is not paired"),
            "Pairing Failed"
        );
        assert_eq!(
            title("host pin failed: the host is not the one this client paired with"),
            "Host Identity Changed"
        );
        assert_eq!(
            title("renderer failed: connect to X display"),
            "Cannot Open the Stream Window"
        );
        assert_eq!(
            title("decoder failed: no VA-API device"),
            "Video Decoding Unavailable"
        );
        assert_eq!(title("something else"), "Connection Failed");
        assert!(explain("Connection refused", host).advice.contains(host));
    }

    #[test]
    fn fingerprints_are_grouped_and_matched() {
        assert_eq!(group_fingerprint("1a2b3c4d5e6f7a8b"), "1a2b 3c4d 5e6f 7a8b");
        assert!(fingerprint_matches("1a2b3c4d", "1A2B3C4D99"));
        assert!(!fingerprint_matches("1a2b3c4e", "1a2b3c4d99"));
        assert!(!fingerprint_matches("", "1a2b"));
    }

    #[test]
    fn event_lines_are_read_once_and_only_when_whole() {
        let path = std::env::temp_dir().join(format!("lumen-gui-ev-{}", crate::store::new_id()));
        let mut session = Session {
            child: Command::new("true").spawn().unwrap(),
            stdin: None,
            events: path.clone(),
            offset: 0,
            partial: String::new(),
            stderr: channel().1,
            tail: VecDeque::new(),
            failure: None,
        };
        assert!(session.poll().is_empty(), "no file yet");
        std::fs::write(&path, "{\"event\":\"connecting\"}\n{\"event\":\"estab").unwrap();
        assert_eq!(session.poll(), [Update::Connecting]);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"lished\"}\n").unwrap();
        assert_eq!(session.poll(), [Update::Established]);
        assert!(session.poll().is_empty(), "nothing is read twice");
        std::fs::write(&path, "").unwrap();
        drop(session);
        assert!(!path.exists(), "the event file goes with the session");
    }
}
