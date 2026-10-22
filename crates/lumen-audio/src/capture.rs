//! Host capture: a per-session PipeWire null sink, and a capture stream on its
//! monitor. The user's real output device is never touched.

use std::fmt;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use libspa::pod::Pod;
use libspa::utils::Direction;
use pipewire::channel;
use pipewire::keys;
use pipewire::node::Node;
use pipewire::properties::properties;
use pipewire::stream::{StreamBox, StreamFlags};

use crate::{check_stream_layout, f32_format_pod, pw_connect, spawn_ready};

/// Chunks buffered between the PipeWire thread and the consumer. When full, the
/// real-time callback drops the newest chunk instead of blocking.
const CHUNK_QUEUE: usize = 64;

/// Captures what a session plays into its own null sink.
pub struct VirtualSinkCapture {
    sink_name: String,
    chunks: Receiver<Vec<f32>>,
    quit: channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl VirtualSinkCapture {
    /// Creates null sink `sink_name` and captures its monitor as interleaved f32 at `sample_rate`.
    ///
    /// `channels` must be 1 or 2. The sink belongs to this PipeWire connection,
    /// so it disappears when the capture is dropped or the process exits.
    pub fn new(sink_name: &str, sample_rate: u32, channels: u16) -> Result<Self, String> {
        check_stream_layout(sample_rate, channels)?;
        if sink_name.is_empty() || sink_name.contains('\0') {
            return Err(format!("invalid sink name {sink_name:?}"));
        }
        let (chunk_tx, chunks) = mpsc::sync_channel(CHUNK_QUEUE);
        let (quit, quit_rx) = channel::channel();
        let name = sink_name.to_owned();
        let thread = spawn_ready("lumen-audio-capture", move |ready| {
            run_capture(&name, sample_rate, channels, chunk_tx, quit_rx, &ready)
        })?;
        Ok(Self {
            sink_name: sink_name.to_owned(),
            chunks,
            quit,
            thread: Some(thread),
        })
    }

    pub fn sink_name(&self) -> &str {
        &self.sink_name
    }

    /// Waits up to `timeout` for the next chunk of interleaved f32 samples.
    /// Returns `None` on timeout or once the capture has stopped.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Vec<f32>> {
        self.chunks.recv_timeout(timeout).ok()
    }
}

impl Drop for VirtualSinkCapture {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl fmt::Debug for VirtualSinkCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualSinkCapture")
            .field("sink_name", &self.sink_name)
            .finish_non_exhaustive()
    }
}

fn run_capture(
    sink_name: &str,
    sample_rate: u32,
    channels: u16,
    chunks: SyncSender<Vec<f32>>,
    quit: channel::Receiver<()>,
    ready: &SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let (mainloop, _context, core) = pw_connect()?;

    let position = if channels == 1 {
        "[ MONO ]"
    } else {
        "[ FL FR ]"
    };
    // Held until the function returns; the sink is removed when the connection closes.
    let _sink = core
        .create_object::<Node>(
            "adapter",
            &properties! {
                *keys::FACTORY_NAME => "support.null-audio-sink",
                *keys::NODE_NAME => sink_name,
                *keys::NODE_DESCRIPTION => format!("Lumen session audio ({sink_name})"),
                *keys::MEDIA_CLASS => "Audio/Sink",
                "audio.rate" => sample_rate.to_string(),
                "audio.channels" => channels.to_string(),
                "audio.position" => position,
            },
        )
        .map_err(|e| format!("create null sink '{sink_name}': {e}"))?;

    let stream = StreamBox::new(
        &core,
        "lumen-session-capture",
        properties! {
            *keys::MEDIA_TYPE => "Audio",
            *keys::MEDIA_CATEGORY => "Capture",
            *keys::MEDIA_ROLE => "Music",
            // Capture the sink's monitor (what the session plays), not a microphone.
            *keys::STREAM_CAPTURE_SINK => "true",
            *keys::TARGET_OBJECT => sink_name,
        },
    )
    .map_err(|e| format!("capture stream: {e}"))?;

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |s, ()| {
            let Some(mut buffer) = s.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else {
                return;
            };
            let Some(bytes) = bytes.get(offset..offset + size) else {
                return;
            };
            let samples: Vec<f32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect();
            // Drop rather than block: this runs on the real-time thread.
            let _ = chunks.try_send(samples);
        })
        .register()
        .map_err(|e| format!("capture listener: {e}"))?;

    let bytes = f32_format_pod(sample_rate, channels)?;
    let mut params = [Pod::from_bytes(&bytes).ok_or("format pod is not valid")?];
    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect capture stream: {e}"))?;

    let quit_loop = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| quit_loop.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_layout_before_touching_pipewire() {
        assert!(VirtualSinkCapture::new("lumen-test-layout", 48_000, 6).is_err());
        assert!(VirtualSinkCapture::new("lumen-test-layout", 0, 2).is_err());
    }

    #[test]
    fn captures_monitor_of_session_sink() {
        let name = format!("lumen-test-capture-{}", std::process::id());
        let capture = match VirtualSinkCapture::new(&name, 48_000, 2) {
            Ok(capture) => capture,
            Err(e) => {
                eprintln!("SKIP captures_monitor_of_session_sink: PipeWire unavailable: {e}");
                return;
            }
        };
        assert_eq!(capture.sink_name(), name);
        let chunk = capture
            .recv_timeout(Duration::from_secs(5))
            .expect("no monitor audio within 5 s of a running null sink");
        assert_ne!(chunk.len(), 0);
        assert_eq!(chunk.len() % 2, 0, "whole stereo frames");
    }
}
