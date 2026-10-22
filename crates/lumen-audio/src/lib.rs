//! Lumen audio: PipeWire session capture (host), Opus or raw PCM framing with
//! A/V timestamps (host), and ring-buffered playback (client).
//!
//! All PipeWire use is confined to this crate. pipewire-rs objects are not
//! `Send`, so each PipeWire connection lives on its own thread. Failures to
//! reach the daemon come back as `Err`, and playback falls back to a no-op
//! sink. Nothing in this crate panics because PipeWire is missing.
//!
//! `AudioConfig::frame_ms` is in tenths of a millisecond (25, 50, 100) because
//! the 2.5 ms Opus frame cannot be written as an integer number of milliseconds.
//!
//! The session path is in three pieces. [`AudioSender`] frames an encoded
//! frame behind the wire header and hands it to the transport through
//! [`AudioPacketSink`], so neither crate owns the other. [`AudioReceiver`]
//! reassembles the frames that come back. [`PlaybackScheduler`] places them
//! on the client's video clock, using the clock offset between the host and
//! the client.
#![forbid(unsafe_code)]

mod capture;
mod clock;
mod opus;
mod pcm;
mod playback;
mod session;
mod sink;
mod sync;

use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use libspa as spa;
use pipewire::context::ContextRc;
use pipewire::core::CoreRc;
use pipewire::main_loop::MainLoopRc;

pub use capture::VirtualSinkCapture;
pub use clock::{ClockOffset, estimate_offset};
pub use playback::{
    DEFAULT_MAX_LATENESS_US, DropReason, Playback, PlaybackScheduler, PlaybackStats, VideoClock,
};
pub use session::{
    AudioPacket, AudioPacketSink, AudioReceiver, AudioSender, AudioSlice, AudioStats,
    ReceivedAudio, SessionError,
};
pub use sink::{AudioSink, open_playback};
pub use sync::FrameEncoder;

// The wire format of an audio frame lives in the protocol crate, and is
// re-exported here so that a session needs this crate alone.
pub use lumen_protocol::audio::{
    AUDIO_HEADER_LEN, AUDIO_MAGIC, AUDIO_VERSION, AudioHeader, AudioKind, MAX_AUDIO_PAYLOAD,
};

/// Audio stream parameters shared by host and client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    /// Frame duration in tenths of a millisecond: 25 = 2.5 ms, 50 = 5 ms, 100 = 10 ms.
    pub frame_ms: u32,
    pub bitrate_bps: i32,
    /// Opus when true, raw little-endian f32 PCM when false.
    pub use_opus: bool,
}

impl AudioConfig {
    /// Samples per channel in one frame.
    fn frame_samples(&self) -> Result<usize, String> {
        if !matches!(self.frame_ms, 25 | 50 | 100) {
            return Err(format!(
                "frame_ms {} unsupported: use 25 (2.5 ms), 50 (5 ms) or 100 (10 ms)",
                self.frame_ms
            ));
        }
        if self.sample_rate == 0 || self.sample_rate > 384_000 || self.channels == 0 {
            return Err("sample_rate must be 1..=384000 and channels non-zero".into());
        }
        Ok((u64::from(self.sample_rate) * u64::from(self.frame_ms) / 10_000) as usize)
    }
}

/// Encodes one frame of interleaved f32 samples into one packet.
pub trait AudioEncoder: Send {
    /// Opus requires exactly one frame (`frame_samples × channels` values).
    /// PCM accepts any length.
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, String>;
}

/// Decodes one packet into interleaved f32 samples.
pub trait AudioDecoder: Send {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<f32>, String>;
}

/// One encoded audio packet, stamped with the capture time of the video frame it aligns to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub data: Vec<u8>,
    pub capture_ts_us: u64,
    /// True when `data` is raw little-endian f32 samples, false when it is Opus.
    pub pcm: bool,
}

/// Decoder for the stream described by `cfg`.
pub fn new_decoder(cfg: &AudioConfig) -> Result<Box<dyn AudioDecoder>, String> {
    if cfg.use_opus {
        Ok(Box::new(opus::OpusDecoder::new(cfg)?))
    } else {
        Ok(Box::new(pcm::PcmDecoder))
    }
}

pub(crate) fn new_encoder(cfg: &AudioConfig) -> Result<Box<dyn AudioEncoder>, String> {
    if cfg.use_opus {
        Ok(Box::new(opus::OpusEncoder::new(cfg)?))
    } else {
        Ok(Box::new(pcm::PcmEncoder))
    }
}

/// PipeWire stream layouts supported here: mono or stereo at a non-zero rate.
fn check_stream_layout(sample_rate: u32, channels: u16) -> Result<(), String> {
    if sample_rate == 0 || !(1..=2).contains(&channels) {
        return Err(format!(
            "PipeWire stream needs a non-zero rate and 1 or 2 channels, got {sample_rate} Hz x {channels}"
        ));
    }
    Ok(())
}

/// Serialised raw F32LE format pod for a mono or stereo stream at `rate`.
fn f32_format_pod(rate: u32, channels: u16) -> Result<Vec<u8>, String> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(u32::from(channels));
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    if channels == 1 {
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    } else {
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    }
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let (cursor, _) = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| format!("format pod: {e:?}"))?;
    Ok(cursor.into_inner())
}

/// Connects to the PipeWire daemon. Call on the thread that will own the objects.
fn pw_connect() -> Result<(MainLoopRc, ContextRc, CoreRc), String> {
    pipewire::init();
    let mainloop = MainLoopRc::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
    let context = ContextRc::new(&mainloop, None).map_err(|e| format!("PipeWire context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("PipeWire daemon unreachable: {e}"))?;
    Ok((mainloop, context, core))
}

/// Runs `run` on a named thread and waits for it to report ready.
///
/// `run` must send `Ok(())` on `ready` once its PipeWire objects are live. If
/// it returns `Err` first, that error is returned to the caller.
fn spawn_ready<F>(name: &str, run: F) -> Result<JoinHandle<()>, String>
where
    F: FnOnce(SyncSender<Result<(), String>>) -> Result<(), String> + Send + 'static,
{
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let err_tx = ready_tx.clone();
    let thread = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            if let Err(e) = run(ready_tx) {
                let _ = err_tx.send(Err(e));
            }
        })
        .map_err(|e| format!("spawn {name}: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(format!("{name} thread exited during startup"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(frame_ms: u32) -> AudioConfig {
        AudioConfig {
            sample_rate: 48_000,
            channels: 2,
            frame_ms,
            bitrate_bps: 128_000,
            use_opus: true,
        }
    }

    #[test]
    fn frame_sizes_match_spec() {
        assert_eq!(cfg(25).frame_samples(), Ok(120));
        assert_eq!(cfg(50).frame_samples(), Ok(240));
        assert_eq!(cfg(100).frame_samples(), Ok(480));
    }

    #[test]
    fn rejects_bad_frame_config() {
        // 5 is milliseconds, not tenths: it must not silently mean 0.5 ms.
        assert!(cfg(5).frame_samples().is_err());
        assert!(
            AudioConfig {
                sample_rate: 0,
                ..cfg(50)
            }
            .frame_samples()
            .is_err()
        );
        assert!(
            AudioConfig {
                channels: 0,
                ..cfg(50)
            }
            .frame_samples()
            .is_err()
        );
    }

    #[test]
    fn spawn_ready_reports_setup_error() {
        let err = spawn_ready("lumen-audio-test", |_ready| Err("boom".into())).unwrap_err();
        assert_eq!(err, "boom");
    }

    #[test]
    fn the_audio_header_round_trips() {
        let header = AudioHeader::new(AudioKind::Opus, 2, 48_000, 5_000, 42, 1_000_000, 96)
            .expect("valid header");
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), AUDIO_HEADER_LEN);
        assert_eq!(AudioHeader::parse(&bytes).unwrap(), header);
    }

    #[test]
    fn the_audio_header_is_the_protocol_one() {
        // The audio crate must not define a second wire format.
        let header: lumen_protocol::audio::AudioHeader =
            AudioHeader::new(AudioKind::Pcm, 1, 48_000, 2_500, 0, 0, 0).expect("valid header");
        assert_eq!(header.kind, AudioKind::Pcm);
    }
}
