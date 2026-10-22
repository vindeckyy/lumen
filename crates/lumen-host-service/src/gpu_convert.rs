//! GPU conversion of captured frames for the host's encode stage.
//!
//! The capture hands over a BGRX DMA-BUF allocated on the compositor's render node. This module
//! imports it as a VA surface, scales and converts it to NV12 on the GPU, and exports the NV12
//! buffer for the encoder. The encoder opens the same node, so no picture is copied to host memory.

use std::os::fd::AsFd;
use std::path::Path;

use lumen_ffi_va::{OwnedVaSurface, VaDisplay, VideoProcessor, sync_dmabuf};
use lumen_protocol::media::{DmabufImage, PixelFormat};

/// The sizes one processor was made for: the capture size, then the picture the client asked for.
type Shape = (u32, u32, u32, u32);

/// A GPU converter on the capture's render node.
#[derive(Debug)]
pub struct GpuConvert {
    display: VaDisplay,
    processor: Option<(Shape, VideoProcessor)>,
}

impl GpuConvert {
    /// Opens the render node the capture allocates its buffers on.
    ///
    /// # Errors
    /// Returns the libva error when the node cannot be opened as a VA display.
    pub fn open(node: &Path) -> Result<Self, String> {
        let display = VaDisplay::open(node).map_err(|e| e.to_string())?;
        Ok(Self {
            display,
            processor: None,
        })
    }

    /// Converts the BGRX capture in `image`, which is `in_w` by `in_h`, to NV12 of `out_w` by
    /// `out_h`.
    ///
    /// The compositor writes the capture asynchronously. The buffer is synced before the GPU reads
    /// it, and again after, so the GPU's writes to the NV12 buffer are visible to the encoder.
    ///
    /// # Errors
    /// Returns the first sync, import, conversion or export failure. The caller then stops using
    /// the GPU for the session.
    pub fn convert(
        &mut self,
        image: &DmabufImage,
        (in_w, in_h): (u32, u32),
        (out_w, out_h): (u32, u32),
    ) -> Result<DmabufImage, String> {
        let t0 = std::time::Instant::now();
        let shape = (in_w, in_h, out_w, out_h);
        sync_dmabuf(image.fd.as_fd(), true, true).map_err(|e| e.to_string())?;
        if self
            .processor
            .as_ref()
            .is_none_or(|(current, _)| *current != shape)
        {
            let processor =
                VideoProcessor::new(&self.display, in_w, in_h).map_err(|e| e.to_string())?;
            self.processor = Some((shape, processor));
        }
        let (_, processor) = self
            .processor
            .as_mut()
            .ok_or("the video processor is missing")?;
        let source =
            OwnedVaSurface::import_frame(&self.display, image, PixelFormat::Bgrx, in_w, in_h)
                .map_err(|e| e.to_string())?;
        let t_import = t0.elapsed().as_micros();
        let nv12 = processor
            .convert(&source, PixelFormat::Nv12, out_w, out_h)
            .map_err(|e| e.to_string())?;
        let t_convert = t0.elapsed().as_micros();
        sync_dmabuf(image.fd.as_fd(), true, false).map_err(|e| e.to_string())?;
        eprintln!(
            "TRACE conv import_done_us={t_import} convert_done_us={t_convert} end_us={}",
            t0.elapsed().as_micros()
        );
        nv12.export_dmabuf()
            .map_err(|e| e.to_string())?
            .into_image()
    }
}
