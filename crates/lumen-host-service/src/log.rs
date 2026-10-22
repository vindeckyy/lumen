//! The host's log, kept so the control plane can show it.
//!
//! The host writes its progress to the standard error, and a person
//! watching a terminal is the first thing that needs it. A person watching
//! the dashboard needs the same lines, so every line goes into a small ring
//! buffer as well. The ring is bounded: a host that runs for a week must
//! not grow without limit.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

/// How many lines the ring keeps.
const CAPACITY: usize = 500;

/// How severe a line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Something ordinary happened.
    Info,
    /// Something the operator should look at.
    Warn,
    /// Something went wrong.
    Error,
}

impl Level {
    /// The name the control plane uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One line of the log.
#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    /// How severe the line is.
    pub level: String,
    /// The time the line was written, as HH:MM:SS.
    pub time: String,
    /// The message.
    pub message: String,
}

fn ring() -> &'static Mutex<VecDeque<LogLine>> {
    static RING: OnceLock<Mutex<VecDeque<LogLine>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// The time of day, as HH:MM:SS.
fn now() -> String {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Realtime);
    let secs = ts.tv_sec;
    let millis = ts.tv_nsec / 1_000_000;
    let day = secs.rem_euclid(86_400);
    let hours = day / 3600;
    let minutes = (day % 3600) / 60;
    let seconds = day % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// Write a line to the log and to the standard error.
///
/// The "lumen-host: " prefix is dropped: the panel that shows the lines
/// already says where they come from, and the terminal still gets it.
pub fn note(level: Level, message: impl AsRef<str>) {
    let message = message.as_ref();
    let stripped = message
        .strip_prefix("lumen-host: ")
        .unwrap_or(message)
        .to_string();
    match level {
        Level::Info => eprintln!("{message}"),
        Level::Warn => eprintln!("lumen-host: warning: {stripped}"),
        Level::Error => eprintln!("lumen-host: error: {stripped}"),
    }
    if let Ok(mut ring) = ring().lock() {
        if ring.len() >= CAPACITY {
            ring.pop_front();
        }
        ring.push_back(LogLine {
            level: level.name().to_string(),
            time: now(),
            message: stripped,
        });
    }
}

/// The recent lines, oldest first.
#[must_use]
pub fn recent(count: usize) -> Vec<LogLine> {
    ring()
        .lock()
        .map(|ring| ring.iter().rev().take(count).rev().cloned().collect())
        .unwrap_or_default()
}

/// Write a line to the log.
///
/// The level comes first, then the same arguments an `eprintln!` takes.
#[macro_export]
macro_rules! host_log {
    (info $($arg:tt)*) => {
        $crate::log::note($crate::log::Level::Info, format!($($arg)*))
    };
    (warn $($arg:tt)*) => {
        $crate::log::note($crate::log::Level::Warn, format!($($arg)*))
    };
    (error $($arg:tt)*) => {
        $crate::log::note($crate::log::Level::Error, format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_kept_with_its_level_and_time() {
        note(Level::Warn, "lumen-host: something to look at");
        let lines = recent(50);
        let line = lines
            .iter()
            .find(|l| l.message == "something to look at")
            .expect("the line is in the log");
        assert_eq!(line.level, "warn");
        assert!(line.time.contains(':'));
    }

    #[test]
    fn the_prefix_is_dropped() {
        note(Level::Info, "lumen-host: the prefix goes");
        let lines = recent(50);
        assert!(lines.iter().any(|l| l.message == "the prefix goes"));
    }

    #[test]
    fn the_recent_lines_come_back_oldest_first() {
        note(Level::Info, "lumen-host: first");
        note(Level::Info, "lumen-host: second");
        let lines = recent(50);
        let first = lines.iter().position(|l| l.message == "first");
        let second = lines.iter().position(|l| l.message == "second");
        assert!(first < second);
    }
}
