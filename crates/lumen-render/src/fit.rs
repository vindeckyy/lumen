//! Where a picture goes in its window and which part of it shows.
//!
//! Ported from punktfunk `crates/core/punktfunk-core/src/video_fit.rs` (MIT OR Apache-2.0), with
//! the same fit rules, snap tolerance and centring. The cases in its `video-fit-vectors.json`
//! are the cross-language contract, and the tests below check the same numbers.

/// How a picture whose shape differs from the window fills it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VideoFit {
    /// The whole picture, centred, with bars.
    #[default]
    Fit,
    /// No bars; the overflow is cut off evenly on both sides.
    Crop,
    /// No bars; each axis scaled on its own.
    Stretch,
}

impl VideoFit {
    /// The fit called `name`: `fit`, `crop` or `stretch`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fit" => Some(Self::Fit),
            "crop" => Some(Self::Crop),
            "stretch" => Some(Self::Stretch),
            _ => None,
        }
    }

    /// The name [`Self::parse`] reads back.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Fit => "fit",
            Self::Crop => "crop",
            Self::Stretch => "stretch",
        }
    }
}

/// A scale within this many window pixels, along the picture's longer side, snaps to a whole number.
const SNAP_PX: f64 = 8.0;
/// Snap tolerance as a fraction of the snapped scale.
const SNAP_REL: f64 = 0.005;

/// Where a picture goes in its window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// The window rectangle the picture fills, in whole window pixels: x, y, width, height.
    pub dst: [i32; 4],
    /// The visible part of the picture, in picture pixels: x, y, width, height.
    pub src: [f64; 4],
}

/// Places a picture of `frame` pixels in a window of `view` pixels, or `None` when either has a
/// zero dimension.
#[must_use]
pub fn place(fit: VideoFit, frame: (u32, u32), view: (u32, u32)) -> Option<Placement> {
    let (fw, fh) = frame;
    let (vw, vh) = view;
    if fw == 0 || fh == 0 || vw == 0 || vh == 0 {
        return None;
    }
    let sx = f64::from(vw) / f64::from(fw);
    let sy = f64::from(vh) / f64::from(fh);
    let long = f64::from(fw.max(fh));
    let (scale_x, scale_y) = match fit {
        VideoFit::Fit => {
            let s = snap(sx.min(sy), long);
            (s, s)
        }
        VideoFit::Crop => {
            let s = snap(sx.max(sy), long);
            (s, s)
        }
        VideoFit::Stretch => (snap(sx, f64::from(fw)), snap(sy, f64::from(fh))),
    };
    let (x0, w) = axis(vw, fw, scale_x);
    let (y0, h) = axis(vh, fh, scale_y);
    // The picture is whole pixels now, so its scale is measured from those pixels.
    let scale_x = w as f64 / f64::from(fw);
    let scale_y = h as f64 / f64::from(fh);
    let (dst_x, dst_w) = clip(x0, w, vw);
    let (dst_y, dst_h) = clip(y0, h, vh);
    // Float error can push the source one ulp past the picture; the compositor refuses that.
    let src_x = ((dst_x - x0) as f64 / scale_x).min(f64::from(fw));
    let src_y = ((dst_y - y0) as f64 / scale_y).min(f64::from(fh));
    Some(Placement {
        dst: [dst_x, dst_y, dst_w, dst_h].map(to_i32),
        src: [
            src_x,
            src_y,
            (dst_w as f64 / scale_x).min(f64::from(fw) - src_x),
            (dst_h as f64 / scale_y).min(f64::from(fh) - src_y),
        ],
    })
}

/// Rounds `s` to a whole scale when it is within `SNAP_PX` window pixels along `len` picture
/// pixels, or `SNAP_REL` of the scale, whichever is larger.
fn snap(s: f64, len: f64) -> f64 {
    let k = s.round();
    if k >= 1.0 && (k - s).abs() <= (SNAP_PX / len).max(SNAP_REL * k) {
        k
    } else {
        s
    }
}

/// Unclipped origin and size of the picture along one axis, centred with the origin floored.
fn axis(view: u32, frame: u32, scale: f64) -> (i64, i64) {
    let size = ((f64::from(frame) * scale).round() as i64).max(1);
    ((i64::from(view) - size).div_euclid(2), size)
}

/// The part of `[origin, origin + size)` inside `[0, view)`, as start and length.
fn clip(origin: i64, size: i64, view: u32) -> (i64, i64) {
    let start = origin.max(0);
    let end = (origin + size).min(i64::from(view));
    (start, (end - start).max(0))
}

/// The size of a window that shows a picture of `frame` pixels, in logical units.
///
/// The picture is shown at its size on a screen with `scale` pixels per logical unit. A picture
/// that does not fit in 85 % of the screen is scaled down to fit it, keeping its aspect ratio.
/// The window is never smaller than 320 by 180.
#[must_use]
pub fn initial_size(frame: (u32, u32), display: Option<(u32, u32)>, scale: u32) -> (u32, u32) {
    let scale = scale.max(1);
    let (fw, fh) = (frame.0 / scale, frame.1 / scale);
    let (w, h) = match display {
        None => (fw, fh),
        Some((dw, dh)) => {
            let bw = dw / scale * 85 / 100;
            let bh = dh / scale * 85 / 100;
            if fw <= bw && fh <= bh {
                (fw, fh)
            } else {
                let s = (f64::from(bw) / f64::from(fw)).min(f64::from(bh) / f64::from(fh));
                (
                    (f64::from(fw) * s).round() as u32,
                    (f64::from(fh) * s).round() as u32,
                )
            }
        }
    };
    (w.max(320), h.max(180))
}

fn to_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-4;
    /// One vector: name, fit, view, frame, expected `dst` and expected `src`.
    type Case = (
        &'static str,
        VideoFit,
        (u32, u32),
        (u32, u32),
        [i32; 4],
        [f64; 4],
    );

    #[test]
    fn the_names_round_trip_and_an_unknown_name_is_refused() {
        for fit in [VideoFit::Fit, VideoFit::Crop, VideoFit::Stretch] {
            assert_eq!(VideoFit::parse(fit.name()), Some(fit));
        }
        assert_eq!(VideoFit::parse("zoom"), None);
        assert_eq!(VideoFit::default(), VideoFit::Fit);
    }

    /// The cases of `clients/shared/video-fit-vectors.json` that place a picture.
    #[test]
    fn placement_matches_the_shared_vectors() {
        let cases: [Case; 16] = [
            (
                "fit-phone-pillarbox",
                VideoFit::Fit,
                (3216, 1440),
                (1920, 1080),
                [328, 0, 2560, 1440],
                [0.0, 0.0, 1920.0, 1080.0],
            ),
            (
                "crop-phone-cuts-top-and-bottom",
                VideoFit::Crop,
                (3216, 1440),
                (1920, 1080),
                [0, 0, 3216, 1440],
                [0.0, 110.447_761, 1920.0, 859.701_493],
            ),
            (
                "stretch-phone",
                VideoFit::Stretch,
                (3216, 1440),
                (1920, 1080),
                [0, 0, 3216, 1440],
                [0.0, 0.0, 1920.0, 1080.0],
            ),
            (
                "fit-monitor-letterboxes-a-20-9-frame",
                VideoFit::Fit,
                (1920, 1080),
                (3216, 1440),
                [0, 110, 1920, 860],
                [0.0, 0.0, 3216.0, 1440.0],
            ),
            (
                "crop-monitor-cuts-the-sides",
                VideoFit::Crop,
                (1920, 1080),
                (3216, 1440),
                [0, 0, 1920, 1080],
                [328.0, 0.0, 2560.0, 1440.0],
            ),
            (
                "fit-native-is-copy",
                VideoFit::Fit,
                (1920, 1080),
                (1920, 1080),
                [0, 0, 1920, 1080],
                [0.0, 0.0, 1920.0, 1080.0],
            ),
            (
                "fit-odd-window-snaps-to-copy",
                VideoFit::Fit,
                (1921, 1081),
                (1920, 1080),
                [0, 0, 1920, 1080],
                [0.0, 0.0, 1920.0, 1080.0],
            ),
            (
                "fit-thin-bars-snap-to-copy",
                VideoFit::Fit,
                (1928, 1088),
                (1920, 1080),
                [4, 4, 1920, 1080],
                [0.0, 0.0, 1920.0, 1080.0],
            ),
            (
                "fit-4k-panel-replicates-1080p",
                VideoFit::Fit,
                (3840, 2160),
                (1920, 1080),
                [0, 0, 3840, 2160],
                [0.0, 0.0, 1920.0, 1080.0],
            ),
            (
                "fit-16-10-panel-replicates-with-bars",
                VideoFit::Fit,
                (3840, 2400),
                (1920, 1080),
                [0, 120, 3840, 2160],
                [0.0, 0.0, 1920.0, 1080.0],
            ),
            (
                "fit-near-double-crops-a-few-pixels",
                VideoFit::Fit,
                (3832, 2152),
                (1920, 1080),
                [0, 0, 3832, 2152],
                [2.0, 2.0, 1916.0, 1076.0],
            ),
            (
                "fit-just-past-the-snap-resamples",
                VideoFit::Fit,
                (3880, 2182),
                (3840, 2160),
                [0, 0, 3879, 2182],
                [0.0, 0.0, 3840.0, 2160.0],
            ),
            (
                "fit-ipad-downscale",
                VideoFit::Fit,
                (2732, 2048),
                (2880, 2160),
                [0, 0, 2731, 2048],
                [0.0, 0.0, 2880.0, 2160.0],
            ),
            (
                "fit-render-scale-2-supersample",
                VideoFit::Fit,
                (1920, 1080),
                (3840, 2160),
                [0, 0, 1920, 1080],
                [0.0, 0.0, 3840.0, 2160.0],
            ),
            (
                "fit-portrait-frame-rounds-half-away",
                VideoFit::Fit,
                (1920, 1080),
                (1080, 1920),
                [656, 0, 608, 1080],
                [0.0, 0.0, 1080.0, 1920.0],
            ),
            (
                "stretch-snaps-each-axis-alone",
                VideoFit::Stretch,
                (3840, 1081),
                (1920, 1080),
                [0, 0, 3840, 1080],
                [0.0, 0.0, 1920.0, 1080.0],
            ),
        ];
        for (name, fit, view, frame, dst, src) in cases {
            let p = place(fit, frame, view).unwrap_or_else(|| panic!("{name}: no placement"));
            assert_eq!(p.dst, dst, "{name}: dst");
            for (got, want) in p.src.iter().zip(src) {
                assert!(
                    (got - want).abs() < EPS,
                    "{name}: src {:?} != {src:?}",
                    p.src
                );
            }
        }
    }

    #[test]
    fn a_zero_dimension_places_nothing() {
        assert!(place(VideoFit::Fit, (0, 1080), (1920, 1080)).is_none());
        assert!(place(VideoFit::Crop, (1920, 1080), (0, 0)).is_none());
    }

    #[test]
    fn the_visible_source_never_leaves_the_picture() {
        for fit in [VideoFit::Fit, VideoFit::Crop, VideoFit::Stretch] {
            for view in [(1921, 1081), (3880, 2182), (1000, 7), (7, 1000)] {
                let p = place(fit, (1920, 1080), view).unwrap();
                assert!(
                    p.src[0] >= 0.0 && p.src[0] + p.src[2] <= 1920.0,
                    "{fit:?} {view:?} {:?}",
                    p.src
                );
                assert!(
                    p.src[1] >= 0.0 && p.src[1] + p.src[3] <= 1080.0,
                    "{fit:?} {view:?} {:?}",
                    p.src
                );
            }
        }
    }

    #[test]
    fn initial_size_fits_the_screen_at_85_percent() {
        assert_eq!(
            initial_size((1920, 1080), Some((1920, 1080)), 1),
            (1632, 918)
        );
        assert_eq!(
            initial_size((3840, 2160), Some((3840, 2160)), 2),
            (1632, 918)
        );
    }

    #[test]
    fn initial_size_keeps_a_picture_that_fits() {
        assert_eq!(
            initial_size((1280, 720), Some((1920, 1080)), 1),
            (1280, 720)
        );
    }

    #[test]
    fn initial_size_without_a_screen_is_the_picture_and_never_below_the_floor() {
        assert_eq!(initial_size((1280, 720), None, 1), (1280, 720));
        assert_eq!(initial_size((160, 90), None, 1), (320, 180));
    }
}
