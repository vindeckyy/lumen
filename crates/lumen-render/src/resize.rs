//! The size policy of the presentation surface.
//!
//! The renderer rebuilds its window when the frame size or the pixel format
//! changes. That decision lives here, so both backends answer it the same
//! way and the client can ask what a resolution change will cost before it
//! happens.
//!
//! A resolution change is a normal event in a remote desktop: the user drags
//! a window to another output, or the output changes mode. The host tells
//! the client, the client rebuilds its surface, and the next frame is
//! presented at the new size.

use lumen_protocol::media::PixelFormat;

/// The size of a presentation surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl SurfaceSize {
    /// The size, checked against the limits of the protocol.
    ///
    /// # Errors
    /// Returns a description when a dimension is zero or too large.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err("a surface must have a non-zero width and height".to_string());
        }
        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(format!(
                "a surface of {width}x{height} exceeds the {MAX_DIMENSION} pixel limit"
            ));
        }
        Ok(Self { width, height })
    }

    /// The number of pixels of the surface.
    #[must_use]
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Whether the size differs from `other`.
    #[must_use]
    pub fn differs_from(&self, other: &Self) -> bool {
        self.width != other.width || self.height != other.height
    }
}

/// The largest dimension a surface may have.
pub const MAX_DIMENSION: u32 = 16384;

/// What the renderer has to do when a frame arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeAction {
    /// The current surface takes the frame as it is.
    Keep,
    /// The surface has to be rebuilt at the new size and format.
    Recreate {
        /// The new width.
        width: u32,
        /// The new height.
        height: u32,
        /// The new pixel format.
        format: PixelFormat,
    },
}

/// Decide what the renderer has to do for a frame.
///
/// The current surface is `None` before the first frame. A frame of the same
/// size and format needs no work; anything else needs a new surface.
#[must_use]
pub fn decide(
    current: Option<(SurfaceSize, PixelFormat)>,
    width: u32,
    height: u32,
    format: PixelFormat,
) -> ResizeAction {
    match current {
        Some((size, current_format))
            if size.width == width && size.height == height && current_format == format =>
        {
            ResizeAction::Keep
        }
        _ => ResizeAction::Recreate {
            width,
            height,
            format,
        },
    }
}

/// The resolution changes a session saw.
///
/// The host counts them, so the run report can carry the number the plan's
/// M4 acceptance checks.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResizeHistory {
    changes: Vec<SurfaceSize>,
}

impl ResizeHistory {
    /// An empty history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the size of a frame and return the change when the size moved.
    ///
    /// The first frame is not a change: it is the size the session starts at.
    pub fn observe(&mut self, size: SurfaceSize) -> Option<SurfaceSize> {
        let previous = self.changes.last().copied();
        if previous == Some(size) {
            return None;
        }
        self.changes.push(size);
        previous
    }

    /// How many times the size changed.
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.changes.len().saturating_sub(1)
    }

    /// The sizes the session went through, oldest first.
    #[must_use]
    pub fn sizes(&self) -> &[SurfaceSize] {
        &self.changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_dimension_is_refused() {
        assert!(SurfaceSize::new(0, 720).is_err());
        assert!(SurfaceSize::new(1920, 0).is_err());
        assert!(SurfaceSize::new(1920, 1080).is_ok());
    }

    #[test]
    fn an_oversized_surface_is_refused() {
        assert!(SurfaceSize::new(MAX_DIMENSION + 1, 1080).is_err());
        assert!(SurfaceSize::new(1920, MAX_DIMENSION + 1).is_err());
    }

    #[test]
    fn the_same_size_needs_no_new_surface() {
        let size = SurfaceSize::new(1920, 1080).unwrap();
        assert_eq!(
            decide(
                Some((size, PixelFormat::Bgrx)),
                1920,
                1080,
                PixelFormat::Bgrx
            ),
            ResizeAction::Keep
        );
    }

    #[test]
    fn a_different_size_needs_a_new_surface() {
        let size = SurfaceSize::new(1920, 1080).unwrap();
        assert_eq!(
            decide(
                Some((size, PixelFormat::Bgrx)),
                2560,
                1440,
                PixelFormat::Bgrx
            ),
            ResizeAction::Recreate {
                width: 2560,
                height: 1440,
                format: PixelFormat::Bgrx,
            }
        );
    }

    #[test]
    fn a_different_format_needs_a_new_surface() {
        let size = SurfaceSize::new(1920, 1080).unwrap();
        assert_eq!(
            decide(
                Some((size, PixelFormat::Bgrx)),
                1920,
                1080,
                PixelFormat::Rgba
            ),
            ResizeAction::Recreate {
                width: 1920,
                height: 1080,
                format: PixelFormat::Rgba,
            }
        );
    }

    #[test]
    fn the_first_frame_always_builds_a_surface() {
        assert_eq!(
            decide(None, 1280, 720, PixelFormat::Bgrx),
            ResizeAction::Recreate {
                width: 1280,
                height: 720,
                format: PixelFormat::Bgrx,
            }
        );
    }

    #[test]
    fn the_history_counts_the_changes_and_not_the_first_size() {
        let mut history = ResizeHistory::new();
        assert_eq!(history.observe(SurfaceSize::new(1920, 1080).unwrap()), None);
        assert_eq!(history.change_count(), 0);
        assert_eq!(
            history.observe(SurfaceSize::new(2560, 1440).unwrap()),
            Some(SurfaceSize::new(1920, 1080).unwrap())
        );
        assert_eq!(history.change_count(), 1);
        assert_eq!(history.observe(SurfaceSize::new(2560, 1440).unwrap()), None);
        assert_eq!(history.change_count(), 1);
        assert_eq!(
            history.observe(SurfaceSize::new(1920, 1080).unwrap()),
            Some(SurfaceSize::new(2560, 1440).unwrap())
        );
        assert_eq!(history.change_count(), 2);
        assert_eq!(history.sizes().len(), 3);
    }
}
