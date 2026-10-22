//! Multiple clients and shadow mode.
//!
//! A host may serve more than one client at a time. The plan asks for one
//! encoder per session, and for a *shadow* client: a client that watches the
//! screen but may not touch it.
//!
//! The shadow mode is a rights question, not a protocol question: the client
//! is told that its session has no input rights, and the host drops the
//! input of a shadow session. The video path is the same for every session.
//!
//! Every session needs its own media port. The port used to be derived from
//! the control connection, so two clients could not both use it; the port is
//! negotiated now, and the table hands out one port per session.

use std::net::SocketAddr;

/// The role of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionRole {
    /// The session may send input.
    Primary,
    /// The session watches the video but may not send input.
    Shadow,
}

impl SessionRole {
    /// The name used in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Shadow => "shadow",
        }
    }

    /// Whether the role may send input.
    #[must_use]
    pub fn has_input(self) -> bool {
        matches!(self, Self::Primary)
    }
}

/// What a session is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRights {
    /// The session receives video.
    pub video: bool,
    /// The session may send input.
    pub input: bool,
    /// The session may read and write the clipboard.
    pub clipboard: bool,
}

impl SessionRights {
    /// The rights of a session with the given role.
    #[must_use]
    pub fn for_role(role: SessionRole) -> Self {
        Self {
            video: true,
            input: role.has_input(),
            // A shadow client watches; it does not take the clipboard either.
            clipboard: role.has_input(),
        }
    }
}

/// How many sessions the host admits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiConfig {
    /// How many sessions may be open at the same time.
    pub max_sessions: usize,
    /// How many of them may be shadow sessions.
    pub shadow_sessions: usize,
    /// The first media port the host hands out.
    pub base_media_port: u16,
}

impl Default for MultiConfig {
    fn default() -> Self {
        Self {
            max_sessions: 1,
            shadow_sessions: 0,
            base_media_port: 24801,
        }
    }
}

/// One admitted session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// The identifier of the session, counting from one.
    pub id: u32,
    /// The role of the session.
    pub role: SessionRole,
    /// What the session may do.
    pub rights: SessionRights,
    /// The address of the client on the control connection.
    pub peer: SocketAddr,
    /// The port the client receives media on.
    pub media_port: u16,
    /// The port the client receives audio on.
    ///
    /// The audio path is a second stream on a second socket, so it needs a
    /// port of its own.
    pub audio_port: u16,
}

/// Errors of the session table.
#[derive(Debug, thiserror::Error)]
pub enum MultiError {
    /// The host already serves as many sessions as it may.
    #[error("the host already serves {0} sessions")]
    TooManySessions(usize),
    /// The host already serves as many shadow sessions as it may.
    #[error("the host already serves {0} shadow sessions")]
    TooManyShadows(usize),
}

/// The sessions the host serves.
#[derive(Debug)]
pub struct SessionTable {
    config: MultiConfig,
    sessions: Vec<SessionEntry>,
    next_id: u32,
}

impl SessionTable {
    /// A table with the given limits.
    #[must_use]
    pub fn new(config: MultiConfig) -> Self {
        Self {
            config,
            sessions: Vec::new(),
            next_id: 1,
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &MultiConfig {
        &self.config
    }

    /// Admit a session.
    ///
    /// The first session is the primary one; the ones after it are shadow
    /// sessions, until the shadow limit is reached. A client beyond the
    /// limits is refused.
    ///
    /// # Errors
    /// Returns an error when the session limit or the shadow limit is
    /// reached.
    pub fn admit(&mut self, peer: SocketAddr) -> Result<SessionEntry, MultiError> {
        if self.sessions.len() >= self.config.max_sessions {
            return Err(MultiError::TooManySessions(self.config.max_sessions));
        }
        let shadows = self
            .sessions
            .iter()
            .filter(|session| session.role == SessionRole::Shadow)
            .count();
        let role = if self.sessions.is_empty() {
            SessionRole::Primary
        } else {
            if shadows >= self.config.shadow_sessions {
                return Err(MultiError::TooManyShadows(self.config.shadow_sessions));
            }
            SessionRole::Shadow
        };
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let media_port = self.media_port_for(id);
        let audio_port = self.audio_port_for(id);
        let entry = SessionEntry {
            id,
            role,
            rights: SessionRights::for_role(role),
            peer,
            media_port,
            audio_port,
        };
        self.sessions.push(entry.clone());
        Ok(entry)
    }

    /// The audio port the table hands out for a session.
    ///
    /// The audio stream is a second stream on a second socket, so it sits
    /// above the media port. A base port of zero means "any port".
    #[must_use]
    pub fn audio_port_for(&self, id: u32) -> u16 {
        if self.config.base_media_port == 0 {
            return 0;
        }
        let offset = id.saturating_sub(1);
        let offset = u16::try_from(offset).unwrap_or(u16::MAX);
        self.config
            .base_media_port
            .saturating_add(offset)
            .saturating_add(1)
    }

    /// The media port the table hands out for a session.
    ///
    /// The port is the same for every session: the host sends the media to
    /// the address of the control connection, so two clients on two machines
    /// both receive it on the same port. Two clients on one machine need two
    /// ports, and the port is not negotiated (D50, O-25).
    #[must_use]
    pub fn media_port_for(&self, _id: u32) -> u16 {
        self.config.base_media_port
    }

    /// Remove a session.
    pub fn remove(&mut self, id: u32) -> Option<SessionEntry> {
        let position = self.sessions.iter().position(|session| session.id == id)?;
        Some(self.sessions.remove(position))
    }

    /// The open sessions.
    #[must_use]
    pub fn sessions(&self) -> &[SessionEntry] {
        &self.sessions
    }

    /// How many sessions are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether no session is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// The primary session, when there is one.
    #[must_use]
    pub fn primary(&self) -> Option<&SessionEntry> {
        self.sessions
            .iter()
            .find(|session| session.role == SessionRole::Primary)
    }

    /// The session with the given identifier.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&SessionEntry> {
        self.sessions.iter().find(|session| session.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 24800))
    }

    #[test]
    fn the_first_session_is_the_primary_one() {
        let mut table = SessionTable::new(MultiConfig::default());
        let session = table.admit(peer()).expect("the first session");
        assert_eq!(session.role, SessionRole::Primary);
        assert_eq!(session.id, 1);
        assert!(session.rights.input);
        assert!(session.rights.video);
        assert!(session.rights.clipboard);
        assert_eq!(table.primary().map(|s| s.id), Some(1));
    }

    #[test]
    fn a_shadow_session_has_no_input_rights() {
        let mut table = SessionTable::new(MultiConfig {
            max_sessions: 2,
            shadow_sessions: 1,
            base_media_port: 24801,
        });
        table.admit(peer()).expect("the primary session");
        let shadow = table.admit(peer()).expect("the shadow session");
        assert_eq!(shadow.role, SessionRole::Shadow);
        assert!(!shadow.rights.input);
        assert!(shadow.rights.video);
        assert!(!shadow.rights.clipboard);
        assert!(!shadow.role.has_input());
    }

    #[test]
    fn the_session_limit_is_enforced() {
        let mut table = SessionTable::new(MultiConfig {
            max_sessions: 2,
            shadow_sessions: 1,
            base_media_port: 24801,
        });
        table.admit(peer()).expect("the primary session");
        table.admit(peer()).expect("the shadow session");
        let error = table
            .admit(peer())
            .expect_err("the third session is refused");
        assert!(matches!(error, MultiError::TooManySessions(2)));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn the_shadow_limit_is_enforced() {
        let mut table = SessionTable::new(MultiConfig {
            max_sessions: 4,
            shadow_sessions: 1,
            base_media_port: 24801,
        });
        table.admit(peer()).expect("the primary session");
        table.admit(peer()).expect("the shadow session");
        let error = table.admit(peer()).expect_err("a second shadow is refused");
        assert!(matches!(error, MultiError::TooManyShadows(1)));
    }

    #[test]
    fn every_session_uses_the_configured_media_port() {
        let mut table = SessionTable::new(MultiConfig {
            max_sessions: 3,
            shadow_sessions: 2,
            base_media_port: 24801,
        });
        let first = table.admit(peer()).expect("first");
        let second = table.admit(peer()).expect("second");
        let third = table.admit(peer()).expect("third");
        // The host sends the media to the address of the control connection,
        // so the port is the same for every session.
        assert_eq!(first.media_port, 24801);
        assert_eq!(second.media_port, 24801);
        assert_eq!(third.media_port, 24801);
    }

    #[test]
    fn the_audio_stream_has_a_port_of_its_own() {
        let mut table = SessionTable::new(MultiConfig {
            max_sessions: 3,
            shadow_sessions: 2,
            base_media_port: 24801,
        });
        let first = table.admit(peer()).expect("first");
        let second = table.admit(peer()).expect("second");
        // The audio stream is a second stream on a second socket, so it
        // sits above the media port and the two never collide.
        assert_eq!(first.audio_port, 24802);
        assert_eq!(second.audio_port, 24803);
        assert_ne!(first.audio_port, first.media_port);
    }

    #[test]
    fn a_base_port_of_zero_means_any_port() {
        let mut table = SessionTable::new(MultiConfig {
            max_sessions: 2,
            shadow_sessions: 1,
            base_media_port: 0,
        });
        let first = table.admit(peer()).expect("first");
        let second = table.admit(peer()).expect("second");
        assert_eq!(first.media_port, 0);
        assert_eq!(second.media_port, 0);
    }

    #[test]
    fn a_removed_session_frees_its_place() {
        let mut table = SessionTable::new(MultiConfig {
            max_sessions: 1,
            shadow_sessions: 0,
            base_media_port: 24801,
        });
        let session = table.admit(peer()).expect("the session");
        assert_eq!(table.len(), 1);
        let removed = table.remove(session.id).expect("the session is removed");
        assert_eq!(removed.id, session.id);
        assert!(table.is_empty());
        assert!(table.remove(session.id).is_none());
        // A new session may be admitted again.
        assert!(table.admit(peer()).is_ok());
    }

    #[test]
    fn the_identifiers_do_not_repeat() {
        let mut table = SessionTable::new(MultiConfig {
            max_sessions: 4,
            shadow_sessions: 3,
            base_media_port: 24801,
        });
        let first = table.admit(peer()).expect("first");
        table.remove(first.id);
        let second = table.admit(peer()).expect("second");
        assert_ne!(first.id, second.id);
    }
}
