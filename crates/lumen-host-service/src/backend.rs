//! The capture backend of the host and the backend matrix of the plan.
//!
//! The plan asks for four combinations to work:
//!
//! | Capture | Present |
//! |---|---|
//! | Wayland | Wayland |
//! | Wayland | X11 |
//! | X11 | Wayland |
//! | X11 | X11 |
//!
//! The two ends pick their side independently: the host picks the display
//! server it captures from and the client picks the one it presents into.
//! Both sides read the environment when the user does not override them:
//! `WAYLAND_DISPLAY` wins over `DISPLAY`, because a Wayland session usually
//! has an XWayland server as well, and the Wayland compositor is the real
//! session.

use std::fmt;

/// The display server a Lumen endpoint talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayServer {
    /// A Wayland compositor.
    Wayland,
    /// An X11 server.
    X11,
}

impl DisplayServer {
    /// The name used on the command line and in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
        }
    }

    /// The name of the environment variable that names the display.
    #[must_use]
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Wayland => "WAYLAND_DISPLAY",
            Self::X11 => "DISPLAY",
        }
    }

    /// Whether the environment names this display server.
    #[must_use]
    pub fn available(self) -> bool {
        std::env::var_os(self.env_var()).is_some_and(|value| !value.is_empty())
    }
}

impl fmt::Display for DisplayServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What the user asked for on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendChoice {
    /// Pick from the environment.
    #[default]
    Auto,
    /// Use Wayland.
    Wayland,
    /// Use X11.
    X11,
}

impl BackendChoice {
    /// Parse the value of a `--capture` or `--present` flag.
    ///
    /// # Errors
    /// Returns a description when the name is not one of `auto`, `wayland`
    /// or `x11`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "wayland" => Ok(Self::Wayland),
            "x11" => Ok(Self::X11),
            other => Err(format!(
                "the backend must be auto, wayland or x11, got {other}"
            )),
        }
    }

    /// The name used on the command line and in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Wayland => "wayland",
            Self::X11 => "x11",
        }
    }
}

/// The display server the environment selects.
///
/// `WAYLAND_DISPLAY` wins over `DISPLAY`: a Wayland session usually runs an
/// XWayland server as well, and the Wayland compositor is the real session.
/// `None` means neither is named.
#[must_use]
pub fn detect_display_server() -> Option<DisplayServer> {
    if DisplayServer::Wayland.available() {
        return Some(DisplayServer::Wayland);
    }
    if DisplayServer::X11.available() {
        return Some(DisplayServer::X11);
    }
    None
}

/// Resolve a choice into a display server.
///
/// # Errors
/// Returns a description when the choice is `Auto` and the environment names
/// no display server, or when the chosen server is not available.
pub fn resolve(choice: BackendChoice) -> Result<DisplayServer, String> {
    match choice {
        BackendChoice::Auto => detect_display_server().ok_or_else(|| {
            "no display server is available: neither WAYLAND_DISPLAY nor DISPLAY is set".to_string()
        }),
        BackendChoice::Wayland => {
            if DisplayServer::Wayland.available() {
                Ok(DisplayServer::Wayland)
            } else {
                Err("WAYLAND_DISPLAY is not set".to_string())
            }
        }
        BackendChoice::X11 => {
            if DisplayServer::X11.available() {
                Ok(DisplayServer::X11)
            } else {
                Err("DISPLAY is not set".to_string())
            }
        }
    }
}

/// One row of the backend matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatrixEntry {
    /// The display server the host captures from.
    pub capture: DisplayServer,
    /// The display server the client presents into.
    pub present: DisplayServer,
}

/// The four combinations of the plan.
///
/// Every row is supported: the capture path and the present path are
/// independent, and neither one constrains the other.
pub const MATRIX: [MatrixEntry; 4] = [
    MatrixEntry {
        capture: DisplayServer::Wayland,
        present: DisplayServer::Wayland,
    },
    MatrixEntry {
        capture: DisplayServer::Wayland,
        present: DisplayServer::X11,
    },
    MatrixEntry {
        capture: DisplayServer::X11,
        present: DisplayServer::Wayland,
    },
    MatrixEntry {
        capture: DisplayServer::X11,
        present: DisplayServer::X11,
    },
];

/// Whether the plan supports a combination.
#[must_use]
pub fn is_supported(capture: DisplayServer, present: DisplayServer) -> bool {
    MATRIX
        .iter()
        .any(|entry| entry.capture == capture && entry.present == present)
}

/// The name of a combination, as it appears in a report.
#[must_use]
pub fn matrix_name(capture: DisplayServer, present: DisplayServer) -> String {
    format!("{capture}/{present}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_matrix_has_the_four_rows_of_the_plan() {
        assert_eq!(MATRIX.len(), 4);
        for capture in [DisplayServer::Wayland, DisplayServer::X11] {
            for present in [DisplayServer::Wayland, DisplayServer::X11] {
                assert!(
                    is_supported(capture, present),
                    "{capture}/{present} must be supported"
                );
            }
        }
        // Every row is distinct.
        let mut rows: Vec<(DisplayServer, DisplayServer)> =
            MATRIX.iter().map(|e| (e.capture, e.present)).collect();
        rows.sort_by_key(|(c, p)| (c.name(), p.name()));
        rows.dedup();
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn the_choice_names_round_trip() {
        for choice in [
            BackendChoice::Auto,
            BackendChoice::Wayland,
            BackendChoice::X11,
        ] {
            assert_eq!(BackendChoice::parse(choice.name()).unwrap(), choice);
        }
        assert!(BackendChoice::parse("wayland2").is_err());
        assert!(BackendChoice::parse("").is_err());
    }

    #[test]
    fn the_default_choice_is_auto() {
        assert_eq!(BackendChoice::default(), BackendChoice::Auto);
    }

    #[test]
    fn an_explicit_choice_needs_its_display_server() {
        // The environment of the test process names neither display, so an
        // explicit choice must fail loudly rather than pick the wrong one.
        let wayland = resolve(BackendChoice::Wayland);
        let x11 = resolve(BackendChoice::X11);
        if !DisplayServer::Wayland.available() {
            assert!(wayland.is_err(), "WAYLAND_DISPLAY is unset");
        }
        if !DisplayServer::X11.available() {
            assert!(x11.is_err(), "DISPLAY is unset");
        }
    }

    #[test]
    fn the_matrix_name_is_the_pair() {
        assert_eq!(
            matrix_name(DisplayServer::Wayland, DisplayServer::X11),
            "wayland/x11"
        );
    }
}
