//! GPU colour conversion and scaling with the VA-API video processor (`VAEntrypointVideoProc`).
//!
//! A [`VideoProcessor`] converts one surface to another pixel format and size. Both sides use BT.709
//! limited range, the range `lumen_protocol::color` uses on the CPU, so the two paths agree.

use lumen_protocol::media::PixelFormat;

use crate::{
    config::VaConfig,
    context::VaContext,
    display::{VaDisplay, same_display},
    error::{VaError, VaResult},
    surface::OwnedVaSurface,
    sys,
};

/// ITU-T H.273 code 1: BT.709 primaries, transfer characteristics and matrix.
const BT709: u8 = 1;
/// Limited (studio) range. libva defines `VA_SOURCE_RANGE_REDUCED` as 1, checked below.
const LIMITED_RANGE: u8 = 1;
const _: () = assert!(sys::VA_SOURCE_RANGE_REDUCED == 1);

/// A video processing context on one display. Converts surfaces to another format and size.
#[derive(Debug)]
pub struct VideoProcessor {
    display: VaDisplay,
    // Kept alive for as long as the context made from it.
    _config: VaConfig,
    context: VaContext,
}

impl VideoProcessor {
    /// Creates a video processor on `display` for pictures of `width` by `height`.
    ///
    /// # Errors
    /// [`VaError::Unsupported`] if the driver has no video processing entrypoint, and the status
    /// of the failing libva call otherwise.
    pub fn new(display: &VaDisplay, width: u32, height: u32) -> VaResult<Self> {
        if !display.supports(sys::VAProfileNone, sys::VAEntrypointVideoProc) {
            return Err(VaError::Unsupported(
                "video processing (VAEntrypointVideoProc)",
            ));
        }
        let config = display.create_config(sys::VAProfileNone, sys::VAEntrypointVideoProc, &[])?;
        // No render targets: each conversion names its output in `begin_picture`.
        let context = VaContext::create(&config, width, height, &[])?;
        Ok(Self {
            display: display.clone(),
            _config: config,
            context,
        })
    }

    /// Converts `src` to `dst_format` at `dst_width` by `dst_height`, scaling the whole picture.
    ///
    /// The call waits for the GPU, so the returned surface can be read or exported at once.
    ///
    /// # Errors
    /// [`VaError::Invalid`] if `src` belongs to another display, and the status of the failing
    /// libva call otherwise.
    pub fn convert(
        &mut self,
        src: &OwnedVaSurface,
        dst_format: PixelFormat,
        dst_width: u32,
        dst_height: u32,
    ) -> VaResult<OwnedVaSurface> {
        let dst = OwnedVaSurface::create(&self.display, dst_format, dst_width, dst_height)?;
        self.convert_into(src, dst)
    }

    /// [`Self::convert`] into a linear (untiled) surface, for a consumer that imports only linear
    /// DMA-BUFs.
    ///
    /// # Errors
    /// As [`Self::convert`], and [`VaError::Status`] if the driver cannot make a linear surface.
    pub fn convert_linear(
        &mut self,
        src: &OwnedVaSurface,
        dst_format: PixelFormat,
        dst_width: u32,
        dst_height: u32,
    ) -> VaResult<OwnedVaSurface> {
        let dst = OwnedVaSurface::create_linear(&self.display, dst_format, dst_width, dst_height)?;
        self.convert_into(src, dst)
    }

    // shortcut: one output surface allocated per frame; upgrade to a pool with busy tracking if
    // vaCreateSurfaces shows up in profiles.
    fn convert_into(
        &mut self,
        src: &OwnedVaSurface,
        dst: OwnedVaSurface,
    ) -> VaResult<OwnedVaSurface> {
        same_display(&self.display.inner, &src.display)?;
        let parameters = pipeline_parameters(src);
        let pipeline = self
            .context
            .create_buffer(sys::VAProcPipelineParameterBufferType, &parameters)?;
        self.context.begin_picture(&dst)?;
        let rendered = self.context.render_picture(&[&pipeline]);
        // The picture is ended even when the buffer was refused, so the context stays usable.
        let ended = self.context.end_picture();
        rendered.and(ended)?;
        dst.sync()?;
        Ok(dst)
    }
}

/// The pipeline parameters for a whole-picture BT.709 limited-range conversion of `src`.
fn pipeline_parameters(src: &OwnedVaSurface) -> sys::VAProcPipelineParameterBuffer {
    let limited_bt709 = sys::VAProcColorProperties {
        color_range: LIMITED_RANGE,
        colour_primaries: BT709,
        transfer_characteristics: BT709,
        matrix_coefficients: BT709,
        ..sys::VAProcColorProperties::default()
    };
    sys::VAProcPipelineParameterBuffer {
        surface: src.id,
        surface_color_standard: sys::VAProcColorStandardBT709,
        output_color_standard: sys::VAProcColorStandardBT709,
        input_color_properties: limited_bt709,
        output_color_properties: limited_bt709,
        // A null region is the whole picture, and the output region defaults to the whole surface.
        ..sys::VAProcPipelineParameterBuffer::default()
    }
}
