//! Client playback: decoded samples go into a bounded queue that a PipeWire
//! playback stream drains. Without PipeWire, playback is a no-op sink.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use libspa::pod::Pod;
use libspa::utils::Direction;
use pipewire::channel;
use pipewire::keys;
use pipewire::properties::properties;
use pipewire::stream::{StreamBox, StreamFlags};

use crate::{AudioConfig, check_stream_layout, f32_format_pod, pw_connect};

/// Most audio kept queued. Older samples go first, so latency stays bounded.
// shortcut: fixed 40 ms playback cap and no adaptive jitter buffer; revisit with the D-JITTER measurements.
const MAX_QUEUED_MS: usize = 40;

/// Destination for decoded samples.
pub trait AudioSink: Send {
    /// Queues interleaved f32 samples. Never blocks.
    fn push(&mut self, samples: &[f32]);
}

/// Plays through PipeWire. Falls back to a sink that discards audio if PipeWire is unavailable.
pub fn open_playback(cfg: &AudioConfig) -> Box<dyn AudioSink> {
    match PipeWireSink::new(cfg) {
        Ok(sink) => Box::new(sink),
        Err(e) => {
            tracing::warn!("audio playback disabled, discarding audio: {e}");
            Box::new(NullSink)
        }
    }
}

#[derive(Debug)]
struct NullSink;

impl AudioSink for NullSink {
    fn push(&mut self, _samples: &[f32]) {}
}

struct PipeWireSink {
    queue: Arc<Mutex<VecDeque<f32>>>,
    capacity: usize,
    quit: channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// False once the playback thread gave up. The samples are then dropped.
    alive: Arc<AtomicBool>,
}

impl PipeWireSink {
    fn new(cfg: &AudioConfig) -> Result<Self, String> {
        check_stream_layout(cfg.sample_rate, cfg.channels)?;
        let frame_len = cfg.frame_samples()?;
        let capacity = cfg.sample_rate as usize * usize::from(cfg.channels) * MAX_QUEUED_MS / 1000;
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let (quit, quit_rx) = channel::channel();
        let (rate, channels) = (cfg.sample_rate, cfg.channels);
        let thread_queue = Arc::clone(&queue);
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = Arc::clone(&alive);

        // The playback thread must not block the caller. On a machine with no
        // audio device the stream can take a long time to connect, and the
        // session has to start anyway. The ready channel is dropped on
        // purpose: the thread reports into the void and carries on.
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        drop(ready_rx);
        let thread = thread::Builder::new()
            .name("lumen-audio-playback".to_string())
            .spawn(move || {
                if let Err(e) =
                    run_playback(rate, channels, frame_len, thread_queue, quit_rx, &ready_tx)
                {
                    tracing::warn!("audio playback stopped: {e}");
                    thread_alive.store(false, Ordering::Relaxed);
                }
            })
            .map_err(|e| format!("spawn the playback thread: {e}"))?;
        Ok(Self {
            queue,
            capacity,
            quit,
            thread: Some(thread),
            alive,
        })
    }
}

impl AudioSink for PipeWireSink {
    fn push(&mut self, samples: &[f32]) {
        if !self.alive.load(Ordering::Relaxed) {
            return;
        }
        // A poisoned lock means the playback thread panicked; audio is dropped from then on.
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        queue_capped(&mut queue, samples, self.capacity);
    }
}

impl Drop for PipeWireSink {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Appends `samples` and drops the oldest entries beyond `capacity`.
fn queue_capped(queue: &mut VecDeque<f32>, samples: &[f32], capacity: usize) {
    queue.extend(samples.iter().copied());
    let excess = queue.len().saturating_sub(capacity);
    queue.drain(..excess);
}

fn run_playback(
    rate: u32,
    channels: u16,
    frame_len: usize,
    ring: Arc<Mutex<VecDeque<f32>>>,
    quit: channel::Receiver<()>,
    ready: &SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let (mainloop, _context, core) = pw_connect()?;

    let stream = StreamBox::new(
        &core,
        "lumen-session-playback",
        properties! {
            *keys::MEDIA_TYPE => "Audio",
            *keys::MEDIA_CATEGORY => "Playback",
            *keys::MEDIA_ROLE => "Music",
            // Ask the graph for one codec frame per cycle instead of the default quantum.
            *keys::NODE_LATENCY => format!("{frame_len}/{rate}"),
        },
    )
    .map_err(|e| format!("playback stream: {e}"))?;

    // Bytes per interleaved frame, for the chunk stride the graph expects.
    let stride = i32::from(channels) * 4;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |s, ()| {
            let Some(mut buffer) = s.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let Some(out) = data.data() else {
                return;
            };
            // shortcut: std Mutex on the real-time thread; swap for a lock-free SPSC ring if underruns appear under load.
            let mut queue = ring.lock().ok();
            for dst in out.as_chunks_mut::<4>().0 {
                let sample = queue.as_mut().and_then(|q| q.pop_front()).unwrap_or(0.0);
                *dst = sample.to_le_bytes();
            }
            let size = out.len() / 4 * 4;
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride;
            *chunk.size_mut() = size as u32;
        })
        .register()
        .map_err(|e| format!("playback listener: {e}"))?;

    let bytes = f32_format_pod(rate, channels)?;
    let mut params = [Pod::from_bytes(&bytes).ok_or("format pod is not valid")?];
    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect playback stream: {e}"))?;

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
    fn queue_keeps_newest_samples_within_capacity() {
        let mut q = VecDeque::new();
        queue_capped(&mut q, &[1.0, 2.0, 3.0], 4);
        queue_capped(&mut q, &[4.0, 5.0, 6.0], 4);
        let kept: Vec<f32> = q.iter().copied().collect();
        assert_eq!(kept, vec![3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn open_playback_always_returns_a_sink() {
        let cfg = AudioConfig {
            sample_rate: 48_000,
            channels: 2,
            frame_ms: 50,
            bitrate_bps: 0,
            use_opus: false,
        };
        // Either a PipeWire stream or the no-op fallback; neither may panic.
        let mut sink = open_playback(&cfg);
        sink.push(&[0.0; 480]);
    }
}
