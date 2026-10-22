//! The present backend of the client.
//!
//! The client presents into a Wayland compositor or an X11 server. The
//! choice is automatic by default: `WAYLAND_DISPLAY` wins over `DISPLAY`,
//! because a Wayland session usually runs an XWayland server as well and the
//! Wayland compositor is the real session. The `--present` flag overrides
//! the choice.
//!
//! The host side of the matrix lives in
//! [`lumen_host_service::backend`]; this module is the client half of it.

use lumen_host_service::backend::{
    BackendChoice, DisplayServer, MATRIX, detect_display_server, is_supported, matrix_name, resolve,
};

/// Which display server the client presents into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentBackend {
    /// Present into a Wayland compositor.
    Wayland,
    /// Present into an X11 server.
    X11,
}

impl PresentBackend {
    /// The display server this backend means.
    #[must_use]
    pub fn display_server(self) -> DisplayServer {
        match self {
            Self::Wayland => DisplayServer::Wayland,
            Self::X11 => DisplayServer::X11,
        }
    }

    /// The backend for a display server.
    #[must_use]
    pub fn from_display_server(server: DisplayServer) -> Self {
        match server {
            DisplayServer::Wayland => Self::Wayland,
            DisplayServer::X11 => Self::X11,
        }
    }
}

/// The backend the client uses.
#[derive(Debug)]
pub struct BackendSelection {
    /// The display server the client presents into.
    pub present: DisplayServer,
    /// The choice the user made.
    pub choice: BackendChoice,
    /// The display server the host captures from, when the client knows it.
    pub capture: Option<DisplayServer>,
}

impl BackendSelection {
    /// Resolve the choice into a display server.
    ///
    /// # Errors
    /// Returns a description when the chosen display server is not
    /// available.
    pub fn resolve(choice: BackendChoice) -> Result<Self, String> {
        Ok(Self {
            present: resolve(choice)?,
            choice,
            capture: None,
        })
    }

    /// The present backend.
    #[must_use]
    pub fn present_backend(&self) -> PresentBackend {
        PresentBackend::from_display_server(self.present)
    }

    /// The name of the combination, when the capture backend is known.
    #[must_use]
    pub fn matrix_name(&self) -> String {
        match self.capture {
            Some(capture) => matrix_name(capture, self.present),
            None => format!("{}/unknown", self.present),
        }
    }

    /// Whether the combination is one the plan supports.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        match self.capture {
            Some(capture) => is_supported(capture, self.present),
            // Without a capture backend the client cannot say, and the plan
            // supports every combination anyway.
            None => true,
        }
    }
}

/// The display server the environment selects, when there is one.
#[must_use]
pub fn detect() -> Option<DisplayServer> {
    detect_display_server()
}

/// The rows of the backend matrix of the plan.
#[must_use]
pub fn matrix() -> &'static [lumen_host_service::backend::MatrixEntry] {
    &MATRIX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_present_backend_maps_both_ways() {
        for server in [DisplayServer::Wayland, DisplayServer::X11] {
            let backend = PresentBackend::from_display_server(server);
            assert_eq!(backend.display_server(), server);
        }
    }

    #[test]
    fn the_matrix_has_the_four_rows_of_the_plan() {
        assert_eq!(matrix().len(), 4);
    }

    #[test]
    fn a_selection_names_its_combination() {
        let mut selection = BackendSelection {
            present: DisplayServer::Wayland,
            choice: BackendChoice::Auto,
            capture: None,
        };
        assert_eq!(selection.matrix_name(), "wayland/unknown");
        selection.capture = Some(DisplayServer::X11);
        assert_eq!(selection.matrix_name(), "x11/wayland");
        assert!(selection.is_supported());
    }

    #[test]
    fn the_present_backend_of_a_selection_follows_the_display_server() {
        let selection = BackendSelection {
            present: DisplayServer::X11,
            choice: BackendChoice::X11,
            capture: Some(DisplayServer::X11),
        };
        assert_eq!(selection.present_backend(), PresentBackend::X11);
        assert_eq!(selection.matrix_name(), "x11/x11");
    }
}
