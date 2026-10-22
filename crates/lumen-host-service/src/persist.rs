//! Session persistence.
//!
//! A remote desktop has to survive the things that happen on a real desk:
//!
//! * The client goes away: the network drops, the laptop lid closes, the
//!   user walks away. The host keeps the session: the encoder is paused, the
//!   state is kept, and the client can come back and continue.
//! * The compositor restarts: the session dies with it. The host reopens
//!   the capture and asks the encoder for a keyframe, because the new
//!   capture starts a new stream.
//!
//! The host is the one that holds the state, so a client that comes back
//! finds the session where it left it. The timeout decides how long the host
//! waits before it lets the session go.

use std::time::{Duration, Instant};

/// The state of a persisted session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No client has connected yet.
    Idle,
    /// A client is connected and the frames are flowing.
    Streaming,
    /// The client is gone but the session is kept.
    Paused,
    /// The session was kept for too long and is gone.
    Expired,
}

impl SessionState {
    /// Whether the session is still usable.
    #[must_use]
    pub fn is_alive(self) -> bool {
        matches!(self, Self::Streaming | Self::Paused)
    }
}

/// How long the host keeps a session after the client left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistConfig {
    /// How long the session is kept after the client disconnected.
    pub disconnect_timeout: Duration,
    /// Whether the session is kept at all.
    pub keep_session: bool,
}

impl Default for PersistConfig {
    fn default() -> Self {
        Self {
            disconnect_timeout: Duration::from_secs(60),
            keep_session: true,
        }
    }
}

/// The counters of the persisted session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersistStats {
    /// How many clients connected.
    pub connections: u64,
    /// How many clients disconnected.
    pub disconnections: u64,
    /// How many clients came back to a kept session.
    pub resumes: u64,
    /// How many sessions were dropped because the timeout passed.
    pub expirations: u64,
    /// How many times the compositor had to be reopened.
    pub compositor_restarts: u64,
}

/// The state machine of a persisted session.
///
/// The host drives it from the control stage: the control channel tells it
/// when the client leaves, and the session tick expires a session nobody
/// came back for.
#[derive(Debug)]
pub struct SessionKeeper {
    config: PersistConfig,
    state: SessionState,
    disconnected_at: Option<Instant>,
    stats: PersistStats,
}

impl SessionKeeper {
    /// A keeper with the given configuration.
    #[must_use]
    pub fn new(config: PersistConfig) -> Self {
        Self {
            config,
            state: SessionState::Idle,
            disconnected_at: None,
            stats: PersistStats::default(),
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &PersistConfig {
        &self.config
    }

    /// The state of the session.
    #[must_use]
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> PersistStats {
        self.stats
    }

    /// A client connected.
    ///
    /// A client that comes back to a session that was kept resumes it: the
    /// counters say so, and the host asks the encoder for a keyframe, because
    /// the client lost its reference pictures.
    pub fn client_connected(&mut self) {
        self.stats.connections += 1;
        if self.state == SessionState::Paused {
            self.stats.resumes += 1;
        }
        self.state = SessionState::Streaming;
        self.disconnected_at = None;
    }

    /// The client went away.
    ///
    /// The session is kept when the configuration says so, so the client can
    /// come back. Otherwise it ends at once.
    pub fn client_disconnected(&mut self, now: Instant) -> SessionState {
        self.stats.disconnections += 1;
        if self.config.keep_session {
            self.state = SessionState::Paused;
            self.disconnected_at = Some(now);
        } else {
            self.state = SessionState::Expired;
            self.disconnected_at = None;
            self.stats.expirations += 1;
        }
        self.state
    }

    /// Advance the state machine.
    ///
    /// A paused session that nobody came back for expires once the timeout
    /// has passed.
    pub fn tick(&mut self, now: Instant) -> SessionState {
        if self.state != SessionState::Paused {
            return self.state;
        }
        let Some(since) = self.disconnected_at else {
            return self.state;
        };
        if now.duration_since(since) >= self.config.disconnect_timeout {
            self.state = SessionState::Expired;
            self.disconnected_at = None;
            self.stats.expirations += 1;
        }
        self.state
    }

    /// The compositor restarted and the capture was reopened.
    pub fn note_compositor_restart(&mut self) {
        self.stats.compositor_restarts += 1;
    }

    /// How long the session has been paused, when it is paused.
    #[must_use]
    pub fn paused_for(&self, now: Instant) -> Option<Duration> {
        self.disconnected_at.map(|since| now.duration_since(since))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keeper(timeout: Duration) -> SessionKeeper {
        SessionKeeper::new(PersistConfig {
            disconnect_timeout: timeout,
            keep_session: true,
        })
    }

    #[test]
    fn a_new_keeper_is_idle() {
        let keeper = keeper(Duration::from_secs(60));
        assert_eq!(keeper.state(), SessionState::Idle);
        assert_eq!(keeper.stats().connections, 0);
    }

    #[test]
    fn a_client_that_leaves_pauses_the_session() {
        let mut keeper = keeper(Duration::from_secs(60));
        keeper.client_connected();
        assert_eq!(keeper.state(), SessionState::Streaming);
        let now = Instant::now();
        assert_eq!(keeper.client_disconnected(now), SessionState::Paused);
        assert_eq!(keeper.state(), SessionState::Paused);
        assert_eq!(keeper.paused_for(now), Some(Duration::ZERO));
    }

    #[test]
    fn a_paused_session_expires_after_the_timeout() {
        let mut keeper = keeper(Duration::from_secs(10));
        keeper.client_connected();
        let now = Instant::now();
        keeper.client_disconnected(now);
        assert_eq!(
            keeper.tick(now + Duration::from_secs(9)),
            SessionState::Paused
        );
        assert_eq!(
            keeper.tick(now + Duration::from_secs(10)),
            SessionState::Expired
        );
        assert_eq!(keeper.stats().expirations, 1);
        // An expired session stays expired.
        assert_eq!(
            keeper.tick(now + Duration::from_secs(20)),
            SessionState::Expired
        );
        assert_eq!(keeper.stats().expirations, 1);
    }

    #[test]
    fn a_client_that_comes_back_resumes_the_session() {
        let mut keeper = keeper(Duration::from_secs(60));
        keeper.client_connected();
        keeper.client_disconnected(Instant::now());
        assert_eq!(keeper.state(), SessionState::Paused);
        keeper.client_connected();
        assert_eq!(keeper.state(), SessionState::Streaming);
        assert_eq!(keeper.stats().resumes, 1);
        assert_eq!(keeper.stats().connections, 2);
        assert_eq!(keeper.paused_for(Instant::now()), None);
    }

    #[test]
    fn a_session_that_is_not_kept_ends_at_once() {
        let mut keeper = SessionKeeper::new(PersistConfig {
            disconnect_timeout: Duration::from_secs(60),
            keep_session: false,
        });
        keeper.client_connected();
        assert_eq!(
            keeper.client_disconnected(Instant::now()),
            SessionState::Expired
        );
        assert_eq!(keeper.stats().expirations, 1);
        assert_eq!(keeper.stats().resumes, 0);
    }

    #[test]
    fn a_compositor_restart_is_counted() {
        let mut keeper = keeper(Duration::from_secs(60));
        keeper.note_compositor_restart();
        keeper.note_compositor_restart();
        assert_eq!(keeper.stats().compositor_restarts, 2);
    }

    #[test]
    fn a_streaming_session_never_expires() {
        let mut keeper = keeper(Duration::from_secs(1));
        keeper.client_connected();
        let now = Instant::now();
        assert_eq!(
            keeper.tick(now + Duration::from_secs(3600)),
            SessionState::Streaming
        );
        assert_eq!(keeper.stats().expirations, 0);
    }
}
