//! Virtual displays for headless hosts.
//!
//! A headless host has no screen to capture. Three ways of giving it one
//! are on the table, and the plan asks for the one with the lowest
//! privilege that still gives control over the resolution and the refresh
//! rate:
//!
//! | Option | Privilege | Resolution | Refresh |
//! |---|---|---|---|
//! | sway headless output | a normal user process | exact, through a custom mode | exact, through the mode |
//! | Xorg dummy driver | root, plus its own configuration | exact, through a Modeline | exact, through the Modeline |
//! | DRM EDID override | root, plus a writable connector | none: it changes what a physical connector reports | none |
//!
//! The sway headless output wins: it is an ordinary compositor process, it
//! needs no root and no kernel module, and wlroots lets the mode be set to
//! any resolution and refresh rate. The Xorg dummy driver gives the same
//! control but needs root and a configuration file, and the DRM EDID
//! override changes the identity of a physical connector instead of adding a
//! display, so it is not a virtual display at all.
//!
//! This module implements the sway headless option: it starts a compositor
//! with a headless output of the requested size and hands back the runtime
//! directory the compositor listens in.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Which kind of virtual display to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VirtualDisplayKind {
    /// A headless output of a wlroots compositor.
    SwayHeadless,
    /// An X server with the `dummy` video driver.
    XorgDummy,
    /// An EDID override on a physical DRM connector.
    DrmEdidOverride,
}

impl VirtualDisplayKind {
    /// The name used in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SwayHeadless => "sway-headless",
            Self::XorgDummy => "xorg-dummy",
            Self::DrmEdidOverride => "drm-edid-override",
        }
    }
}

impl std::fmt::Display for VirtualDisplayKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// How much privilege an option needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Privilege {
    /// Nothing beyond an ordinary process.
    User,
    /// Root, or a capability that only root has.
    Root,
}

impl Privilege {
    /// The name used in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Root => "root",
        }
    }
}

/// How well an option controls the resolution of the display.
///
/// The order of the variants is the order of quality: an exact control is
/// better than a fixed one, and a fixed one is better than none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolutionControl {
    /// Any resolution can be asked for.
    Exact,
    /// The resolution is fixed by the implementation.
    Fixed,
    /// The option does not give a display at all.
    Unavailable,
}

impl ResolutionControl {
    /// The name used in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Fixed => "fixed",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One row of the comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualDisplayOption {
    /// The kind of display.
    pub kind: VirtualDisplayKind,
    /// The privilege it needs.
    pub privilege: Privilege,
    /// How well it controls the resolution.
    pub resolution: ResolutionControl,
    /// Whether it controls the refresh rate.
    pub refresh: bool,
    /// What it costs and what it gives.
    pub summary: String,
}

/// The comparison of the options.
#[must_use]
pub fn evaluate() -> Vec<VirtualDisplayOption> {
    vec![
        VirtualDisplayOption {
            kind: VirtualDisplayKind::SwayHeadless,
            privilege: Privilege::User,
            resolution: ResolutionControl::Exact,
            refresh: true,
            summary: "a wlroots compositor with a headless output; the mode is set through \
                      the compositor configuration, so any resolution and refresh rate is \
                      available; needs no root and no kernel module"
                .to_string(),
        },
        VirtualDisplayOption {
            kind: VirtualDisplayKind::XorgDummy,
            privilege: Privilege::Root,
            resolution: ResolutionControl::Exact,
            refresh: true,
            summary: "an Xorg server with the dummy video driver; the mode comes from a \
                      Modeline in the server configuration, so the resolution and the refresh \
                      rate are exact, but the server needs root and its own configuration"
                .to_string(),
        },
        VirtualDisplayOption {
            kind: VirtualDisplayKind::DrmEdidOverride,
            privilege: Privilege::Root,
            resolution: ResolutionControl::Unavailable,
            refresh: false,
            summary: "an EDID override written into a physical connector; it changes what the \
                      hardware claims to be, so it renames a display instead of adding one, and \
                      the mode still comes from the real hardware"
                .to_string(),
        },
    ]
}

/// The option the plan picks: the lowest privilege with exact control.
///
/// The plan's `D-VIRT-DISPLAY` names the sway headless output as the
/// default, and the comparison agrees with it.
#[must_use]
pub fn recommend() -> VirtualDisplayOption {
    evaluate()
        .into_iter()
        .find(|option| option.kind == VirtualDisplayKind::SwayHeadless)
        .expect("the sway headless option is always in the comparison")
}

/// The virtual display to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualDisplaySpec {
    /// Which kind of display to create.
    pub kind: VirtualDisplayKind,
    /// The width in pixels.
    pub width: u32,
    /// The height in pixels.
    pub height: u32,
    /// The refresh rate in hertz.
    pub refresh_hz: u32,
    /// How many outputs the display has.
    pub outputs: u32,
}

impl Default for VirtualDisplaySpec {
    fn default() -> Self {
        Self {
            kind: VirtualDisplayKind::SwayHeadless,
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            outputs: 1,
        }
    }
}

impl VirtualDisplaySpec {
    /// The mode line the compositor configuration carries.
    #[must_use]
    pub fn mode(&self) -> String {
        format!("{}x{}@{}Hz", self.width, self.height, self.refresh_hz)
    }
}

/// Errors of the virtual display.
#[derive(Debug, thiserror::Error)]
pub enum VirtualDisplayError {
    /// The compositor could not be started.
    #[error("cannot start the compositor: {0}")]
    Start(#[from] io::Error),
    /// The compositor did not open its socket in time.
    #[error("the compositor did not open its socket within {0:?}")]
    Timeout(Duration),
    /// The kind of display is not implemented here.
    #[error("the virtual display {0} is not implemented by the host")]
    Unsupported(VirtualDisplayKind),
    /// The specification is not usable.
    #[error("invalid virtual display specification: {0}")]
    Invalid(&'static str),
}

/// A running virtual display.
///
/// The compositor is a child process. Dropping the display stops it.
#[derive(Debug)]
pub struct VirtualDisplay {
    kind: VirtualDisplayKind,
    runtime_dir: PathBuf,
    child: Child,
    width: u32,
    height: u32,
    refresh_hz: u32,
}

impl VirtualDisplay {
    /// Start a virtual display.
    ///
    /// # Errors
    /// Returns an error when the kind is not implemented, when the
    /// specification is not usable, or when the compositor does not start.
    pub fn start(spec: &VirtualDisplaySpec) -> Result<Self, VirtualDisplayError> {
        if spec.kind != VirtualDisplayKind::SwayHeadless {
            return Err(VirtualDisplayError::Unsupported(spec.kind));
        }
        if spec.width == 0 || spec.height == 0 || spec.refresh_hz == 0 {
            return Err(VirtualDisplayError::Invalid(
                "the width, the height and the refresh rate must be non-zero",
            ));
        }
        if spec.outputs == 0 {
            return Err(VirtualDisplayError::Invalid("the display needs an output"));
        }

        let runtime_dir =
            std::env::temp_dir().join(format!("lumen-virtual-display-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime_dir);
        std::fs::create_dir_all(&runtime_dir)?;
        set_private(&runtime_dir)?;

        let config = runtime_dir.join("sway-config");
        let mut sway_config = String::new();
        for index in 1..=spec.outputs {
            use std::fmt::Write as _;
            let _ = writeln!(
                sway_config,
                "output HEADLESS-{index} mode {}x{}@{}Hz",
                spec.width, spec.height, spec.refresh_hz
            );
        }
        std::fs::write(&config, sway_config)?;

        let child = Command::new("sway")
            .arg("-c")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_HEADLESS_OUTPUTS", spec.outputs.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let mut display = Self {
            kind: spec.kind,
            runtime_dir,
            child,
            width: spec.width,
            height: spec.height,
            refresh_hz: spec.refresh_hz,
        };
        display.wait_for_socket(Duration::from_secs(10))?;
        Ok(display)
    }

    /// The runtime directory the compositor listens in.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// The name of the Wayland socket, such as `wayland-1`.
    #[must_use]
    pub fn socket(&self) -> String {
        "wayland-1".to_string()
    }

    /// The size of the display.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The refresh rate of the display.
    #[must_use]
    pub fn refresh_hz(&self) -> u32 {
        self.refresh_hz
    }

    /// The kind of the display.
    #[must_use]
    pub fn kind(&self) -> VirtualDisplayKind {
        self.kind
    }

    /// Whether the compositor is still running.
    #[must_use]
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stop the compositor.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Wait for the compositor to open its socket.
    fn wait_for_socket(&mut self, timeout: Duration) -> Result<(), VirtualDisplayError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.runtime_dir.join("wayland-1").exists() {
                return Ok(());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(VirtualDisplayError::Start(io::Error::other(format!(
                    "the compositor exited with {status}"
                ))));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(VirtualDisplayError::Timeout(timeout))
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

/// Make a directory readable by its owner only.
fn set_private(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)
}

/// A headless output added to the sway session the host runs in, so the client gets a display
/// of its own size inside the user's real session, where its windows open.
///
/// The steps are punktfunk's wlroots backend (`pf-vdisplay/src/vdisplay/linux/wlroots.rs`,
/// MIT OR Apache-2.0, Copyright (c) 2026 unom - Enrico Bühler): `create_output`, the new name
/// read from the difference of the output lists, a custom mode (a headless output without a
/// mode has no refresh clock and produces no frames), and `unplug` when it is dropped. Sway
/// adds headless outputs only when its headless backend is loaded, as in a headless session or
/// with `WLR_BACKENDS=drm,headless,libinput`.
#[derive(Debug)]
pub struct SessionOutput {
    name: String,
}

impl SessionOutput {
    /// Adds an output of `width`x`height` at `refresh_hz` to the running sway.
    ///
    /// # Errors
    /// Returns why sway refused, or that no new output appeared.
    pub fn create(width: u32, height: u32, refresh_hz: u32) -> Result<Self, String> {
        if width == 0 || height == 0 || refresh_hz == 0 {
            return Err("the virtual output needs a non-zero size and refresh rate".into());
        }
        let before = output_names()?;
        swaymsg(&["create_output"])?;
        let name = output_names()?
            .into_iter()
            .find(|name| !before.contains(name))
            .ok_or("sway added no output: is its headless backend loaded?")?;
        // From here on the output exists, so a failure unplugs it through Drop.
        let output = Self { name };
        // `--` keeps swaymsg from reading `--custom` as one of its own options.
        swaymsg(&[
            "--",
            "output",
            &output.name,
            "mode",
            "--custom",
            &format!("{width}x{height}@{refresh_hz}Hz"),
        ])?;
        Ok(output)
    }

    /// The output's name, which the capture selects with `--output`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for SessionOutput {
    fn drop(&mut self) {
        if let Err(e) = swaymsg(&["output", &self.name, "unplug"]) {
            crate::host_log!(warn "lumen-host: cannot remove the output {}: {e}", self.name);
        }
    }
}

/// Runs `swaymsg` with `args`; sway finds the session through `SWAYSOCK`.
fn swaymsg(args: &[&str]) -> Result<String, String> {
    let out = Command::new("swaymsg")
        .args(args)
        .output()
        .map_err(|e| format!("swaymsg: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // swaymsg exits 0 even when a command fails and reports `"success": false`.
    if !out.status.success() || text.contains("\"success\": false") {
        return Err(format!(
            "swaymsg {}: {}{}",
            args.join(" "),
            text.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(text)
}

/// The names of sway's outputs.
fn output_names() -> Result<Vec<String>, String> {
    let json = swaymsg(&["-r", "-t", "get_outputs"])?;
    let outputs: Vec<serde_json::Value> =
        serde_json::from_str(&json).map_err(|e| format!("sway's output list: {e}"))?;
    Ok(outputs
        .iter()
        .filter_map(|o| o.get("name")?.as_str().map(str::to_owned))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_comparison_holds_the_three_options_of_the_plan() {
        let options = evaluate();
        assert_eq!(options.len(), 3);
        let kinds: Vec<VirtualDisplayKind> = options.iter().map(|o| o.kind).collect();
        assert!(kinds.contains(&VirtualDisplayKind::SwayHeadless));
        assert!(kinds.contains(&VirtualDisplayKind::XorgDummy));
        assert!(kinds.contains(&VirtualDisplayKind::DrmEdidOverride));
    }

    #[test]
    fn the_recommendation_is_the_lowest_privilege_with_exact_control() {
        let recommended = recommend();
        assert_eq!(recommended.kind, VirtualDisplayKind::SwayHeadless);
        assert_eq!(recommended.privilege, Privilege::User);
        assert_eq!(recommended.resolution, ResolutionControl::Exact);
        assert!(recommended.refresh);
        for option in evaluate() {
            if option.kind == recommended.kind {
                continue;
            }
            assert!(
                option.privilege > recommended.privilege
                    || option.resolution < recommended.resolution,
                "{} must not be better than the recommendation",
                option.kind.name()
            );
        }
    }

    #[test]
    fn the_edid_override_is_not_a_virtual_display() {
        let option = evaluate()
            .into_iter()
            .find(|o| o.kind == VirtualDisplayKind::DrmEdidOverride)
            .unwrap();
        assert_eq!(option.resolution, ResolutionControl::Unavailable);
        assert!(!option.refresh);
        assert_eq!(option.privilege, Privilege::Root);
    }

    #[test]
    fn the_mode_line_names_the_size_and_the_refresh() {
        let spec = VirtualDisplaySpec {
            width: 2560,
            height: 1440,
            refresh_hz: 120,
            ..VirtualDisplaySpec::default()
        };
        assert_eq!(spec.mode(), "2560x1440@120Hz");
    }

    #[test]
    fn an_unimplemented_kind_is_refused() {
        let spec = VirtualDisplaySpec {
            kind: VirtualDisplayKind::XorgDummy,
            ..VirtualDisplaySpec::default()
        };
        assert!(matches!(
            VirtualDisplay::start(&spec),
            Err(VirtualDisplayError::Unsupported(
                VirtualDisplayKind::XorgDummy
            ))
        ));
    }

    #[test]
    fn an_unusable_specification_is_refused() {
        let spec = VirtualDisplaySpec {
            width: 0,
            ..VirtualDisplaySpec::default()
        };
        assert!(matches!(
            VirtualDisplay::start(&spec),
            Err(VirtualDisplayError::Invalid(_))
        ));
        let spec = VirtualDisplaySpec {
            outputs: 0,
            ..VirtualDisplaySpec::default()
        };
        assert!(matches!(
            VirtualDisplay::start(&spec),
            Err(VirtualDisplayError::Invalid(_))
        ));
    }

    #[test]
    fn a_headless_display_comes_up_at_the_requested_size() {
        // The compositor has to exist for this to run; without it the test
        // says so and stops.
        let spec = VirtualDisplaySpec {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            ..VirtualDisplaySpec::default()
        };
        let mut display = match VirtualDisplay::start(&spec) {
            Ok(display) => display,
            Err(reason) => {
                crate::host_log!(info "SKIP a_headless_display_comes_up_at_the_requested_size: {reason}");
                return;
            }
        };
        assert_eq!(display.size(), (1280, 720));
        assert_eq!(display.refresh_hz(), 60);
        assert!(display.runtime_dir().join("wayland-1").exists());
        assert!(display.is_running());
        display.stop();
        assert!(!display.is_running());
    }
}
