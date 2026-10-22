//! What the app remembers: the saved hosts and the settings.
//!
//! Both live in one JSON file in the user's configuration directory. Each host gets its own
//! client identity in the state directory, because the client pins the first host an identity
//! pairs with: one identity per host lets the app keep several hosts paired at once.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The control port `lumen-host` listens on unless it is told otherwise.
pub const DEFAULT_PORT: u16 = 24800;

/// A host the user saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// Stable id: names the host's identity directory.
    pub id: String,
    /// The name the user sees.
    pub name: String,
    /// `address` or `address:port`; a name is resolved when connecting.
    pub address: String,
    /// The MAC address that wakes the host, when the user gave one.
    #[serde(default)]
    pub mac: Option<String>,
    /// When the host was last connected to, in seconds since the epoch.
    #[serde(default)]
    pub last_used: Option<u64>,
}

/// The window system the stream window uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// The client chooses.
    #[default]
    Auto,
    /// Wayland.
    Wayland,
    /// X11.
    X11,
}

/// The settings of every session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The window system of the stream window.
    pub backend: Backend,
    /// Send the keyboard, the pointer, gamepads, touch and pens to the host.
    pub input: bool,
    /// Play the host's audio.
    pub audio: bool,
    /// Come back on its own when the connection drops.
    pub reconnect: bool,
    /// One decode thread and at most 1080p, for slower machines.
    pub low_power: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            input: true,
            audio: true,
            reconnect: true,
            low_power: false,
        }
    }
}

/// Everything the app remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    /// Saved hosts, in the order they were added.
    pub hosts: Vec<Host>,
    /// Session settings.
    pub settings: Settings,
}

impl Store {
    /// Reads the store. A missing file is an empty store; a broken one is reported, so the
    /// caller does not overwrite what the user had.
    ///
    /// # Errors
    /// Returns a description when the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("{} is not a valid Lumen file: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Writes the store: a temporary file renamed over the old one, so a crash never leaves
    /// half a file.
    ///
    /// # Errors
    /// Returns a description when the directory or the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, json).map_err(|e| format!("cannot write {}: {e}", temp.display()))?;
        std::fs::rename(&temp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// The host with `id`.
    #[must_use]
    pub fn host(&self, id: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.id == id)
    }
}

/// Splits `text` into a host name or address and a port, with [`DEFAULT_PORT`] when none is
/// given. IPv6 addresses take brackets when they carry a port: `[fe80::1]:24800`.
///
/// # Errors
/// Returns a sentence for the user when the text is empty or the port is not a number.
pub fn parse_address(text: &str) -> Result<(String, u16), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Enter the host's address".to_owned());
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok((addr.ip().to_string(), addr.port()));
    }
    if let Ok(ip) = text.parse::<std::net::IpAddr>() {
        return Ok((ip.to_string(), DEFAULT_PORT));
    }
    let (host, port) = match text.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|&p| p != 0)
                .ok_or_else(|| format!("“{port}” is not a port number"))?;
            (host, port)
        }
        None => (text, DEFAULT_PORT),
    };
    if host.is_empty() || host.contains(char::is_whitespace) || host.contains(['[', ']']) {
        return Err(format!("“{text}” is not an address"));
    }
    Ok((host.to_owned(), port))
}

/// Resolves `text` to the socket address the client connects to. Blocks on name lookup.
///
/// # Errors
/// Returns a sentence for the user when the address is invalid or the name does not resolve.
pub fn resolve(text: &str) -> Result<SocketAddr, String> {
    let (host, port) = parse_address(text)?;
    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|_| format!("The name “{host}” could not be found on the network"))?;
    // The client's media socket is IPv4 by default, so an IPv4 answer is preferred.
    let all: Vec<SocketAddr> = addrs.by_ref().collect();
    all.iter()
        .find(|a| a.is_ipv4())
        .or_else(|| all.first())
        .copied()
        .ok_or_else(|| format!("The name “{host}” has no address"))
}

/// A short "when" for the host list: today, yesterday, a weekday-free number of days.
#[must_use]
pub fn last_used_text(now: u64, then: u64) -> String {
    const DAY: u64 = 86_400;
    let days = now.saturating_sub(then) / DAY;
    match days {
        0 => "Used today".to_owned(),
        1 => "Used yesterday".to_owned(),
        2..=59 => format!("Used {days} days ago"),
        _ => format!("Used {} months ago", days / 30),
    }
}

/// Seconds since the epoch.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A new host id: the time and the process id, in hex. Unique for one user's hosts.
#[must_use]
pub fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}{:x}", std::process::id())
}

/// `$XDG_CONFIG_HOME/lumen` or `~/.config/lumen`.
#[must_use]
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_STATE_HOME/lumen` or `~/.local/state/lumen`, where the client keeps its identities.
#[must_use]
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// The store file.
#[must_use]
pub fn store_path() -> PathBuf {
    config_dir().join("client.json")
}

/// The client identity of host `id`. The pin of the host sits next to it.
#[must_use]
pub fn identity_path(id: &str) -> PathBuf {
    state_dir().join("hosts").join(id).join("identity")
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os(var).filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("lumen");
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(fallback).join("lumen")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_take_the_default_port() {
        assert_eq!(
            parse_address("192.168.1.20"),
            Ok(("192.168.1.20".into(), DEFAULT_PORT))
        );
        assert_eq!(
            parse_address(" desk.local "),
            Ok(("desk.local".into(), DEFAULT_PORT))
        );
        assert_eq!(parse_address("desk:9000"), Ok(("desk".into(), 9000)));
        assert_eq!(parse_address("10.0.0.1:1"), Ok(("10.0.0.1".into(), 1)));
        assert_eq!(
            parse_address("fe80::1"),
            Ok(("fe80::1".into(), DEFAULT_PORT))
        );
        assert_eq!(
            parse_address("[fe80::1]:9000"),
            Ok(("fe80::1".into(), 9000))
        );
    }

    #[test]
    fn bad_addresses_are_refused() {
        for bad in [
            "",
            "   ",
            "desk:",
            "desk:abc",
            "desk:0",
            "desk:70000",
            "my desk",
            ":24800",
        ] {
            assert!(parse_address(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn last_used_reads_naturally() {
        let day = 86_400;
        assert_eq!(last_used_text(10 * day, 10 * day), "Used today");
        assert_eq!(last_used_text(10 * day, 9 * day), "Used yesterday");
        assert_eq!(last_used_text(10 * day, 5 * day), "Used 5 days ago");
        assert_eq!(last_used_text(200 * day, 10 * day), "Used 6 months ago");
        assert_eq!(
            last_used_text(0, 5 * day),
            "Used today",
            "a clock that went back"
        );
    }

    #[test]
    fn the_store_survives_a_round_trip_and_old_files_get_defaults() {
        let dir = std::env::temp_dir().join(format!("lumen-gui-store-{}", new_id()));
        let path = dir.join("client.json");
        assert_eq!(
            Store::load(&path),
            Ok(Store::default()),
            "no file is an empty store"
        );
        let mut store = Store::default();
        store.hosts.push(Host {
            id: new_id(),
            name: "Desk".into(),
            address: "desk.local".into(),
            mac: Some("aa:bb:cc:dd:ee:ff".into()),
            last_used: Some(5),
        });
        store.settings.low_power = true;
        store.save(&path).unwrap();
        assert_eq!(Store::load(&path).unwrap(), store);

        std::fs::write(&path, br#"{"hosts":[{"id":"x","name":"N","address":"a"}]}"#).unwrap();
        let old = Store::load(&path).unwrap();
        assert_eq!(old.settings, Settings::default());
        assert_eq!(old.hosts[0].mac, None);

        std::fs::write(&path, b"not json").unwrap();
        assert!(
            Store::load(&path).is_err(),
            "a broken file is not silently replaced"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
