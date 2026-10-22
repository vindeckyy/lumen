//! The event log of the client.
//!
//! The client writes what it does as one JSON object per line, so a script
//! can follow the run without parsing the log of the process. The reconnect
//! script reads it to learn when a session was established and how long that
//! took.
//!
//! The file is opened for append, so a client that restarts adds to the
//! record of the run instead of replacing it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One event of the client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    /// What happened.
    pub event: String,
    /// How long the client had been running, in milliseconds.
    pub elapsed_ms: u64,
    /// How many sessions the client has established.
    pub sessions: u64,
    /// How many attempts the client has made.
    pub attempts: u32,
    /// How long the last connect took, in milliseconds.
    pub connect_ms: Option<u64>,
    /// A free-form detail.
    pub detail: Option<String>,
}

/// The writer of the event log.
#[derive(Debug, Clone)]
pub struct EventLog {
    path: Option<PathBuf>,
    started: std::time::Instant,
    sessions: Arc<Mutex<u64>>,
    attempts: Arc<Mutex<u32>>,
}

impl EventLog {
    /// A log that writes to `path`. `None` throws the events away.
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            started: std::time::Instant::now(),
            sessions: Arc::new(Mutex::new(0)),
            attempts: Arc::new(Mutex::new(0)),
        }
    }

    /// The path the log writes to.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// How long the client has been running.
    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Count a session.
    pub fn count_session(&self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            *sessions = sessions.saturating_add(1);
        }
    }

    /// Count an attempt.
    pub fn count_attempt(&self) {
        if let Ok(mut attempts) = self.attempts.lock() {
            *attempts = attempts.saturating_add(1);
        }
    }

    /// Write an event.
    pub fn write(&self, event: &str, connect_ms: Option<u64>, detail: Option<String>) {
        let record = Event {
            event: event.to_string(),
            elapsed_ms: self.elapsed_ms(),
            sessions: self
                .sessions
                .lock()
                .map(|sessions| *sessions)
                .unwrap_or_default(),
            attempts: self
                .attempts
                .lock()
                .map(|attempts| *attempts)
                .unwrap_or_default(),
            connect_ms,
            detail,
        };
        let Some(path) = &self.path else {
            return;
        };
        let Ok(json) = serde_json::to_string(&record) else {
            return;
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{json}");
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_events_round_trip_through_json() {
        let event = Event {
            event: "established".to_string(),
            elapsed_ms: 1234,
            sessions: 2,
            attempts: 3,
            connect_ms: Some(12),
            detail: Some("wayland".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn a_log_without_a_path_throws_the_events_away() {
        let log = EventLog::new(None);
        log.write("established", Some(1), None);
        assert!(log.path().is_none());
    }

    #[test]
    fn the_log_appends_to_the_file() {
        let dir = std::env::temp_dir().join(format!("lumen-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("events.jsonl");
        let log = EventLog::new(Some(path.clone()));
        log.write("connecting", None, None);
        log.count_session();
        log.write("established", Some(5), Some("primary".to_string()));
        let text = std::fs::read_to_string(&path).expect("the log file");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Event = serde_json::from_str(lines[0]).unwrap();
        let second: Event = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first.event, "connecting");
        assert_eq!(second.event, "established");
        assert_eq!(second.sessions, 1);
        assert_eq!(second.connect_ms, Some(5));
        assert_eq!(second.detail.as_deref(), Some("primary"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
