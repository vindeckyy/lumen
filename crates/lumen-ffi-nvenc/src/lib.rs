//! A synchronous NVENC H.264 session, loaded at runtime.
//!
//! `libcuda.so.1` and `libnvidia-encode.so.1` are opened with `dlopen`, never linked, so the
//! crate builds and the host starts on a machine without an NVIDIA driver; [`Nvenc::open`] then
//! fails with [`NvencError::Unavailable`].
//!
//! The session is the low-latency shape punktfunk's `nvenc_cuda.rs` uses (MIT OR Apache-2.0,
//! Copyright (c) 2026 unom - Enrico Bühler): a CUDA device, preset P1 with the ultra-low-latency
//! tuning, no B frames, an infinite GOP with keyframes only on request, and a one-frame VBV in
//! CBR. Input is one encoder-owned buffer that the caller fills from the CPU; NVENC converts RGB
//! input to YUV itself.

use std::ffi::{CStr, c_void};
use std::ptr;

use libloading::Library;

pub mod sys;

/// Why an NVENC call failed.
#[derive(Debug, thiserror::Error)]
pub enum NvencError {
    /// The driver libraries are missing or too old.
    #[error("NVENC is not available: {0}")]
    Unavailable(String),
    /// A CUDA call failed.
    #[error("CUDA {call} failed with status {status}")]
    Cuda {
        /// The call.
        call: &'static str,
        /// The CUDA status.
        status: i32,
    },
    /// An NVENC call failed.
    #[error("{call} failed with {status:?}: {detail}")]
    Nvenc {
        /// The call.
        call: &'static str,
        /// The NVENC status.
        status: sys::NVENCSTATUS,
        /// The driver's last error string.
        detail: String,
    },
    /// The parameters cannot be encoded.
    #[error("invalid NVENC parameters: {0}")]
    Invalid(String),
}

/// The pixel layout of the input buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    /// 8-bit 4:2:0, a luma plane then interleaved chroma.
    Nv12,
    /// 32-bit, bytes B, G, R, X in memory (DRM XRGB8888, X11 ZPixmap).
    Bgrx,
    /// 32-bit, bytes R, G, B, A in memory.
    Rgba,
}

impl InputFormat {
    fn nv(self) -> sys::NV_ENC_BUFFER_FORMAT {
        match self {
            Self::Nv12 => sys::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            // NVENC names packed formats by the 32-bit word, little endian: ARGB is B,G,R,A in
            // memory and ABGR is R,G,B,A.
            Self::Bgrx => sys::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            Self::Rgba => sys::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR,
        }
    }

    /// Rows of the buffer: NV12 has half as many chroma rows below the luma.
    #[must_use]
    pub const fn rows(self, height: u32) -> usize {
        match self {
            Self::Nv12 => height as usize + (height as usize).div_ceil(2),
            Self::Bgrx | Self::Rgba => height as usize,
        }
    }
}

/// Rate control of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    /// Constant quantiser.
    Cqp {
        /// The quantiser of every frame, 0..=51.
        qp: u8,
    },
    /// Constant bitrate with a one-frame VBV.
    Cbr {
        /// Bits per second.
        bits_per_second: u32,
    },
}

/// What a session is opened with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Frames per second.
    pub fps: u32,
    /// Rate control.
    pub rate: Rate,
    /// Input layout.
    pub format: InputFormat,
}

/// One encoded access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Annex B bytes.
    pub data: Vec<u8>,
    /// Whether the picture is an IDR or I picture.
    pub keyframe: bool,
}

type CuInit = unsafe extern "C" fn(u32) -> i32;
type CuDeviceGet = unsafe extern "C" fn(*mut i32, i32) -> i32;
type CuCtxCreate = unsafe extern "C" fn(*mut *mut c_void, u32, i32) -> i32;
type CuCtxDestroy = unsafe extern "C" fn(*mut c_void) -> i32;
type GetMaxVersion = unsafe extern "C" fn(*mut u32) -> sys::NVENCSTATUS;
type CreateInstance =
    unsafe extern "C" fn(*mut sys::NV_ENCODE_API_FUNCTION_LIST) -> sys::NVENCSTATUS;

/// A CUDA context on device 0. NVENC pushes and pops it around its own calls.
struct CudaContext {
    ctx: *mut c_void,
    destroy: CuCtxDestroy,
    _lib: Library,
}

impl CudaContext {
    fn open() -> Result<Self, NvencError> {
        // SAFETY: loading the CUDA driver library runs its constructors, which is what any CUDA
        // client does; it has no other side effect.
        let lib = unsafe { Library::new("libcuda.so.1") }
            .map_err(|e| NvencError::Unavailable(format!("libcuda.so.1: {e}")))?;
        // SAFETY: the symbol types match the CUDA driver API declarations in cuda.h.
        let (init, device_get, create, destroy) = unsafe {
            (
                *lib.get::<CuInit>(b"cuInit\0")
                    .map_err(|e| NvencError::Unavailable(e.to_string()))?,
                *lib.get::<CuDeviceGet>(b"cuDeviceGet\0")
                    .map_err(|e| NvencError::Unavailable(e.to_string()))?,
                *lib.get::<CuCtxCreate>(b"cuCtxCreate_v2\0")
                    .map_err(|e| NvencError::Unavailable(e.to_string()))?,
                *lib.get::<CuCtxDestroy>(b"cuCtxDestroy_v2\0")
                    .map_err(|e| NvencError::Unavailable(e.to_string()))?,
            )
        };
        // SAFETY: the out-pointers are valid locals; flags 0 is the documented default.
        unsafe {
            cuda(init(0), "cuInit")?;
            let mut device = 0;
            cuda(device_get(&raw mut device, 0), "cuDeviceGet")?;
            let mut ctx = ptr::null_mut();
            cuda(create(&raw mut ctx, 0, device), "cuCtxCreate")?;
            Ok(Self {
                ctx,
                destroy,
                _lib: lib,
            })
        }
    }
}

impl Drop for CudaContext {
    fn drop(&mut self) {
        // SAFETY: `ctx` was created by `cuCtxCreate` and is destroyed once, after the encoder
        // that used it (the encoder is dropped first, see `Nvenc`'s field order).
        unsafe {
            (self.destroy)(self.ctx);
        }
    }
}

fn cuda(status: i32, call: &'static str) -> Result<(), NvencError> {
    if status == 0 {
        Ok(())
    } else {
        Err(NvencError::Cuda { call, status })
    }
}

/// One NVENC H.264 session with one input and one output buffer.
pub struct Nvenc {
    // Boxed: the table is over two kilobytes and the session is moved around.
    api: Box<sys::NV_ENCODE_API_FUNCTION_LIST>,
    encoder: *mut c_void,
    input: sys::NV_ENC_INPUT_PTR,
    output: sys::NV_ENC_OUTPUT_PTR,
    // Boxed: `init.encodeConfig` points at `config`, so neither may move.
    config: Box<sys::NV_ENC_CONFIG>,
    init: Box<sys::NV_ENC_INITIALIZE_PARAMS>,
    session: SessionConfig,
    frame_idx: u32,
    // Dropped after `Drop::drop` ran, in this order: the library, then the CUDA context.
    _lib: Library,
    _cuda: CudaContext,
}

impl std::fmt::Debug for Nvenc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nvenc")
            .field("session", &self.session)
            .field("frame_idx", &self.frame_idx)
            .finish_non_exhaustive()
    }
}

// SAFETY: the session is owned by one `Nvenc` and used through `&mut self` only. NVENC binds its
// CUDA context around each call itself, so the thread that calls does not matter.
unsafe impl Send for Nvenc {}

impl Nvenc {
    /// Opens a session.
    ///
    /// # Errors
    /// [`NvencError::Unavailable`] without the driver, [`NvencError::Invalid`] for an odd or empty
    /// size or a zero rate, and the failing call otherwise.
    pub fn open(session: SessionConfig) -> Result<Self, NvencError> {
        validate(&session)?;
        let cuda = CudaContext::open()?;
        // SAFETY: as for libcuda above.
        let lib = unsafe { Library::new("libnvidia-encode.so.1") }
            .map_err(|e| NvencError::Unavailable(format!("libnvidia-encode.so.1: {e}")))?;
        let mut api = sys::NV_ENCODE_API_FUNCTION_LIST {
            version: sys::NV_ENCODE_API_FUNCTION_LIST_VER,
            ..Default::default()
        };
        // SAFETY: the symbol types match nvEncodeAPI.h; the out-pointers are valid locals.
        unsafe {
            let max_version = *lib
                .get::<GetMaxVersion>(b"NvEncodeAPIGetMaxSupportedVersion\0")
                .map_err(|e| NvencError::Unavailable(e.to_string()))?;
            let create = *lib
                .get::<CreateInstance>(b"NvEncodeAPICreateInstance\0")
                .map_err(|e| NvencError::Unavailable(e.to_string()))?;
            let mut version = 0u32;
            check(
                max_version(&raw mut version),
                "NvEncodeAPIGetMaxSupportedVersion",
                None,
            )?;
            let wanted = (sys::NVENCAPI_MAJOR_VERSION << 4) | sys::NVENCAPI_MINOR_VERSION;
            if version < wanted {
                return Err(NvencError::Unavailable(format!(
                    "the driver supports NVENC API {}.{}, this build needs {}.{}",
                    version >> 4,
                    version & 0xf,
                    sys::NVENCAPI_MAJOR_VERSION,
                    sys::NVENCAPI_MINOR_VERSION
                )));
            }
            check(create(&raw mut api), "NvEncodeAPICreateInstance", None)?;
        }

        let mut params = sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: sys::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
            device: cuda.ctx,
            apiVersion: sys::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut encoder = ptr::null_mut();
        // SAFETY: `params` and `encoder` are valid for the call; the function table was filled
        // by NvEncodeAPICreateInstance.
        unsafe {
            check(
                entry(api.nvEncOpenEncodeSessionEx)?(&raw mut params, &raw mut encoder),
                "nvEncOpenEncodeSessionEx",
                None,
            )?;
        }

        // From here on `Drop` cleans up whatever was created.
        let mut nvenc = Self {
            api: Box::new(api),
            encoder,
            input: ptr::null_mut(),
            output: ptr::null_mut(),
            config: Box::default(),
            init: Box::default(),
            session,
            frame_idx: 0,
            _lib: lib,
            _cuda: cuda,
        };
        nvenc.initialize()?;
        Ok(nvenc)
    }

    fn initialize(&mut self) -> Result<(), NvencError> {
        let mut preset = sys::NV_ENC_PRESET_CONFIG {
            version: sys::NV_ENC_PRESET_CONFIG_VER,
            presetCfg: sys::NV_ENC_CONFIG {
                version: sys::NV_ENC_CONFIG_VER,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `preset` is valid for the call and `encoder` is an open session.
        unsafe {
            check(
                entry(self.api.nvEncGetEncodePresetConfigEx)?(
                    self.encoder,
                    sys::NV_ENC_CODEC_H264_GUID,
                    sys::NV_ENC_PRESET_P1_GUID,
                    sys::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &raw mut preset,
                ),
                "nvEncGetEncodePresetConfigEx",
                Some((&*self.api, self.encoder)),
            )?;
        }
        *self.config = preset.presetCfg;
        let config = &mut *self.config;
        config.version = sys::NV_ENC_CONFIG_VER;
        config.profileGUID = sys::NV_ENC_H264_PROFILE_HIGH_GUID;
        config.gopLength = sys::NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1;
        apply_rate(&mut config.rcParams, self.session.rate, self.session.fps);
        // SAFETY: the H.264 member is the active one: the session codec is H.264.
        let h264 = unsafe { &mut config.encodeCodecConfig.h264Config };
        h264.idrPeriod = sys::NVENC_INFINITE_GOPLENGTH;
        // Several references, so a loss can be answered by invalidating the lost frames instead
        // of sending an IDR.
        h264.maxNumRefFrames = ref_frames(self.session.width, self.session.height);
        // Every IDR carries SPS and PPS, so a client can join at any keyframe.
        h264.set_repeatSPSPPS(1);

        let session = self.session;
        *self.init = sys::NV_ENC_INITIALIZE_PARAMS {
            version: sys::NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: sys::NV_ENC_CODEC_H264_GUID,
            presetGUID: sys::NV_ENC_PRESET_P1_GUID,
            encodeWidth: session.width,
            encodeHeight: session.height,
            darWidth: session.width,
            darHeight: session.height,
            frameRateNum: session.fps,
            frameRateDen: 1,
            enableEncodeAsync: 0,
            enablePTD: 1,
            encodeConfig: &raw mut *self.config,
            maxEncodeWidth: session.width,
            maxEncodeHeight: session.height,
            tuningInfo: sys::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            bufferFormat: session.format.nv(),
            ..Default::default()
        };

        let mut input = sys::NV_ENC_CREATE_INPUT_BUFFER {
            version: sys::NV_ENC_CREATE_INPUT_BUFFER_VER,
            width: session.width,
            height: session.height,
            bufferFmt: session.format.nv(),
            ..Default::default()
        };
        let mut output = sys::NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: sys::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        // SAFETY: every struct is valid for its call, `init.encodeConfig` points at the boxed
        // config, which outlives the session.
        unsafe {
            check(
                entry(self.api.nvEncInitializeEncoder)?(self.encoder, &raw mut *self.init),
                "nvEncInitializeEncoder",
                Some((&*self.api, self.encoder)),
            )?;
            check(
                entry(self.api.nvEncCreateInputBuffer)?(self.encoder, &raw mut input),
                "nvEncCreateInputBuffer",
                Some((&*self.api, self.encoder)),
            )?;
            self.input = input.inputBuffer;
            check(
                entry(self.api.nvEncCreateBitstreamBuffer)?(self.encoder, &raw mut output),
                "nvEncCreateBitstreamBuffer",
                Some((&*self.api, self.encoder)),
            )?;
            self.output = output.bitstreamBuffer;
        }
        Ok(())
    }

    /// The configuration the session was opened with, with the current rate.
    #[must_use]
    pub const fn session(&self) -> SessionConfig {
        self.session
    }

    /// Encodes one picture. `fill` writes the picture into the input buffer: it gets the
    /// buffer, `pitch * rows` bytes, and the pitch in bytes.
    ///
    /// # Errors
    /// The error of `fill`, as [`NvencError::Invalid`], or the failing NVENC call.
    pub fn encode(
        &mut self,
        force_idr: bool,
        timestamp: u64,
        fill: impl FnOnce(&mut [u8], usize) -> Result<(), String>,
    ) -> Result<Packet, NvencError> {
        let mut lock = sys::NV_ENC_LOCK_INPUT_BUFFER {
            version: sys::NV_ENC_LOCK_INPUT_BUFFER_VER,
            inputBuffer: self.input,
            ..Default::default()
        };
        // SAFETY: `input` belongs to this session and is not locked.
        unsafe {
            check(
                entry(self.api.nvEncLockInputBuffer)?(self.encoder, &raw mut lock),
                "nvEncLockInputBuffer",
                Some((&*self.api, self.encoder)),
            )?;
        }
        let pitch = lock.pitch as usize;
        let len = pitch * self.session.format.rows(self.session.height);
        // SAFETY: a locked input buffer is `pitch` bytes per row for every row of the format,
        // writable by the CPU until it is unlocked below; nothing else aliases it.
        let buffer =
            unsafe { std::slice::from_raw_parts_mut(lock.bufferDataPtr.cast::<u8>(), len) };
        let filled = fill(buffer, pitch);
        // SAFETY: the buffer was locked above; `buffer` is not used after this.
        unsafe {
            check(
                entry(self.api.nvEncUnlockInputBuffer)?(self.encoder, self.input),
                "nvEncUnlockInputBuffer",
                Some((&*self.api, self.encoder)),
            )?;
        }
        filled.map_err(NvencError::Invalid)?;

        let mut flags = 0;
        if force_idr || self.frame_idx == 0 {
            flags |= sys::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                | sys::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32;
        }
        let mut pic = sys::NV_ENC_PIC_PARAMS {
            version: sys::NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.session.width,
            inputHeight: self.session.height,
            inputPitch: lock.pitch,
            encodePicFlags: flags,
            frameIdx: self.frame_idx,
            inputTimeStamp: timestamp,
            inputBuffer: self.input,
            outputBitstream: self.output,
            bufferFmt: self.session.format.nv(),
            pictureStruct: sys::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        self.frame_idx = self.frame_idx.wrapping_add(1);
        // SAFETY: `pic` names this session's buffers; the session is synchronous, so the output
        // is ready to lock when the call returns.
        unsafe {
            check(
                entry(self.api.nvEncEncodePicture)?(self.encoder, &raw mut pic),
                "nvEncEncodePicture",
                Some((&*self.api, self.encoder)),
            )?;
        }
        let mut bitstream = sys::NV_ENC_LOCK_BITSTREAM {
            version: sys::NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: self.output,
            ..Default::default()
        };
        // SAFETY: the output buffer holds the picture just encoded; the locked pointer is valid
        // for `bitstreamSizeInBytes` bytes until the unlock.
        unsafe {
            check(
                entry(self.api.nvEncLockBitstream)?(self.encoder, &raw mut bitstream),
                "nvEncLockBitstream",
                Some((&*self.api, self.encoder)),
            )?;
            let data = std::slice::from_raw_parts(
                bitstream.bitstreamBufferPtr.cast::<u8>(),
                bitstream.bitstreamSizeInBytes as usize,
            )
            .to_vec();
            check(
                entry(self.api.nvEncUnlockBitstream)?(self.encoder, self.output),
                "nvEncUnlockBitstream",
                Some((&*self.api, self.encoder)),
            )?;
            let keyframe = matches!(
                bitstream.pictureType,
                sys::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | sys::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
            );
            Ok(Packet { data, keyframe })
        }
    }

    /// Marks the frame encoded with `timestamp` as lost: no later frame predicts from it, and
    /// the encoder picks an older reference that is still valid, or an IDR when none is.
    ///
    /// # Errors
    /// The failing NVENC call.
    pub fn invalidate(&mut self, timestamp: u64) -> Result<(), NvencError> {
        // SAFETY: `encoder` is an open session; the call takes the timestamp by value.
        unsafe {
            check(
                entry(self.api.nvEncInvalidateRefFrames)?(self.encoder, timestamp),
                "nvEncInvalidateRefFrames",
                Some((&*self.api, self.encoder)),
            )
        }
    }

    /// Changes the rate control without a new keyframe.
    ///
    /// # Errors
    /// [`NvencError::Invalid`] for a zero rate, or the failing reconfigure call.
    pub fn set_rate(&mut self, rate: Rate) -> Result<(), NvencError> {
        validate(&SessionConfig {
            rate,
            ..self.session
        })?;
        apply_rate(&mut self.config.rcParams, rate, self.session.fps);
        let mut params = sys::NV_ENC_RECONFIGURE_PARAMS {
            version: sys::NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: *self.init,
            ..Default::default()
        };
        // SAFETY: `reInitEncodeParams.encodeConfig` points at the boxed config, updated above.
        unsafe {
            check(
                entry(self.api.nvEncReconfigureEncoder)?(self.encoder, &raw mut params),
                "nvEncReconfigureEncoder",
                Some((&*self.api, self.encoder)),
            )?;
        }
        self.session.rate = rate;
        Ok(())
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        // SAFETY: the handles were created by this session and are destroyed once; the status is
        // ignored because nothing can be done about a failed teardown.
        unsafe {
            if !self.input.is_null()
                && let Some(destroy) = self.api.nvEncDestroyInputBuffer
            {
                destroy(self.encoder, self.input);
            }
            if !self.output.is_null()
                && let Some(destroy) = self.api.nvEncDestroyBitstreamBuffer
            {
                destroy(self.encoder, self.output);
            }
            if let Some(destroy) = self.api.nvEncDestroyEncoder {
                destroy(self.encoder);
            }
        }
    }
}

/// `max_num_ref_frames`: up to eight, bounded by level 5.2's `MaxDpbMbs` for the frame size.
#[must_use]
pub fn ref_frames(width: u32, height: u32) -> u32 {
    const MAX_DPB_MBS: u32 = 184_320;
    let mbs = width.div_ceil(16) * height.div_ceil(16);
    (MAX_DPB_MBS / mbs.max(1)).clamp(1, 8)
}

fn validate(session: &SessionConfig) -> Result<(), NvencError> {
    if session.width == 0
        || session.height == 0
        || !session.width.is_multiple_of(2)
        || !session.height.is_multiple_of(2)
    {
        return Err(NvencError::Invalid(format!(
            "frame size {}x{} must be even and non-zero",
            session.width, session.height
        )));
    }
    if session.fps == 0 {
        return Err(NvencError::Invalid("frame rate must be non-zero".into()));
    }
    match session.rate {
        Rate::Cqp { qp } if qp > 51 => Err(NvencError::Invalid(format!("qp {qp} is above 51"))),
        Rate::Cbr { bits_per_second: 0 } => Err(NvencError::Invalid("bitrate is zero".into())),
        _ => Ok(()),
    }
}

/// Writes `rate` into the rate control parameters.
fn apply_rate(rc: &mut sys::NV_ENC_RC_PARAMS, rate: Rate, fps: u32) {
    match rate {
        Rate::Cqp { qp } => {
            rc.rateControlMode = sys::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
            let qp = u32::from(qp);
            rc.constQP = sys::NV_ENC_QP {
                qpInterP: qp,
                qpInterB: qp,
                qpIntra: qp,
            };
        }
        Rate::Cbr { bits_per_second } => {
            rc.rateControlMode = sys::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            rc.averageBitRate = bits_per_second;
            rc.maxBitRate = bits_per_second;
            // One frame of buffer: a frame never waits behind the one before it.
            let per_frame = bits_per_second / fps.max(1);
            rc.vbvBufferSize = per_frame;
            rc.vbvInitialDelay = per_frame;
        }
    }
}

fn entry<T>(f: Option<T>) -> Result<T, NvencError> {
    f.ok_or_else(|| NvencError::Unavailable("the driver left an NVENC entry point empty".into()))
}

/// Turns a status into a result, with the driver's last error string when there is a session.
fn check(
    status: sys::NVENCSTATUS,
    call: &'static str,
    session: Option<(&sys::NV_ENCODE_API_FUNCTION_LIST, *mut c_void)>,
) -> Result<(), NvencError> {
    if status == sys::NVENCSTATUS::NV_ENC_SUCCESS {
        return Ok(());
    }
    // SAFETY: callers pass only open sessions of their own table.
    let detail = session.map_or_else(String::new, |(api, encoder)| unsafe {
        last_error(api, encoder)
    });
    Err(NvencError::Nvenc {
        call,
        status,
        detail,
    })
}

/// The driver's text for the last error of a session.
///
/// # Safety
/// `encoder` must be an open session of `api`.
unsafe fn last_error(api: &sys::NV_ENCODE_API_FUNCTION_LIST, encoder: *mut c_void) -> String {
    let Some(get) = api.nvEncGetLastErrorString else {
        return String::new();
    };
    // SAFETY: the caller guarantees `encoder`; the driver returns a NUL-terminated string it owns.
    let text = unsafe { get(encoder) };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: non-null, NUL-terminated, valid until the next call on the session.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}
