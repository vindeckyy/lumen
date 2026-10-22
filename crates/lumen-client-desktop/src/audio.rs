//! The client's audio stage.
//!
//! The client receives the audio on a socket of its own, reassembles the
//! frames, decodes them and schedules them against the video clock, so a frame
//! that arrives late is dropped rather than played late.

use std::collections::VecDeque;
use std::net::SocketAddr;

use lumen_audio::{
    AUDIO_HEADER_LEN, AudioConfig, AudioDecoder, AudioHeader, AudioReceiver, AudioSlice,
    PlaybackScheduler, VideoClock, new_decoder, open_playback,
};
use lumen_crypto::PacketCipher;
use lumen_transport::socket::MediaSocket;

/// How many video frames the scheduler must remember before the A/V skew
/// is measured. The history fills at the rate the client polls, so this is
/// a fraction of a second.
const SETTLED_HISTORY: usize = 32;

/// How long the video frame of the same moment may take to be presented.
///
/// The frame interval is not negotiated, so the value is a conservative one
/// for a stream at 30 frames per second and above.
const FRAME_INTERVAL_US: i64 = 33_000;

/// How many audio frames may wait for their play time.
const MAX_QUEUED_FRAMES: usize = 64;

/// The size of the datagram read buffer.
///
/// An Opus frame at 128 kbit/s is about 160 bytes for ten milliseconds, so
/// one frame fits in one datagram.
const DATAGRAM_BUFFER: usize = 2048;

/// The counters of the client's audio stage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioCounters {
    /// Frames played.
    pub frames_played: u64,
    /// Frames dropped because they were late.
    pub frames_dropped: u64,
    /// Frames the decoder refused.
    pub decode_errors: u64,
    /// Datagrams that did not authenticate.
    pub auth_failures: u64,
    /// Datagrams that arrived.
    pub datagrams: u64,
    /// Frames the receiver refused.
    pub protocol_errors: u64,
    /// The scheduler's own counters.
    pub playback: lumen_audio::PlaybackStats,
    /// How far each frame was from the video frame it aligns to, in
    /// microseconds. Positive means the audio was late.
    pub skew_samples: Vec<i64>,
}

/// The client's audio stage.
pub struct AudioStage {
    /// The stream parameters.
    config: AudioConfig,
    /// The socket the audio arrives on.
    socket: MediaSocket,
    /// The cipher the audio is opened with.
    cipher: PacketCipher,
    /// The reassembler.
    receiver: AudioReceiver,
    /// The decoder.
    decoder: Box<dyn AudioDecoder>,
    /// The scheduler that places the frames on the video clock.
    scheduler: PlaybackScheduler,
    /// The sink the samples go to.
    sink: Box<dyn lumen_audio::AudioSink>,
    /// The counters.
    stats: AudioCounters,
    /// The offset the scheduler was last built with.
    offset_applied: Option<lumen_audio::ClockOffset>,
    /// The audio frames that were played, waiting for the video frame they
    /// belong to.
    ///
    /// The A/V skew is only known once the video frame of the same moment
    /// has been presented, so the measurement waits for it.
    pending_skew: VecDeque<(u64, u64)>,
    /// The samples waiting for their play time, oldest first.
    ///
    /// The stage never sleeps: a frame that is not due yet waits here. A
    /// sleeping stage would stop reading the socket, and it would stop
    /// telling the scheduler which video frame is on screen.
    queued: VecDeque<(u64, Vec<f32>)>,
}

impl std::fmt::Debug for AudioStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioStage")
            .field("config", &self.config)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl AudioStage {
    /// Open the audio path.
    ///
    /// # Errors
    /// Returns a human-readable description when the socket cannot be bound
    /// or the decoder cannot be created.
    pub fn new(
        config: AudioConfig,
        socket: MediaSocket,
        cipher: PacketCipher,
    ) -> Result<Self, String> {
        let decoder = new_decoder(&config)?;
        let sink = open_playback(&config);
        Ok(Self {
            config,
            socket,
            cipher,
            receiver: AudioReceiver::new(),
            decoder,
            scheduler: PlaybackScheduler::new(lumen_audio::ClockOffset::unmeasured()),
            sink,
            stats: AudioCounters::default(),
            offset_applied: None,
            queued: VecDeque::new(),
            pending_skew: VecDeque::new(),
        })
    }

    /// The stream parameters.
    #[must_use]
    pub fn config(&self) -> AudioConfig {
        self.config
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> AudioCounters {
        let mut stats = self.stats.clone();
        stats.playback = self.scheduler.stats();
        stats
    }

    /// Measure the A/V skew of the audio frames whose video frame has now
    /// been presented.
    ///
    /// `capture_ts_us` is the capture timestamp of the video frame that was
    /// just presented.
    pub fn measure_skew(&mut self, capture_ts_us: u64) {
        if self.scheduler.history_len() < SETTLED_HISTORY {
            return;
        }
        let now = capture_ts_us.cast_signed();
        while let Some((align_ts_us, play_at_us)) = self.pending_skew.front() {
            let align = align_ts_us.cast_signed();
            // The frame must be newer than the audio, and newer by at least
            // a frame interval, so the frame of the same moment has been
            // presented.
            if now < align || now - align < FRAME_INTERVAL_US {
                break;
            }
            if let Some(present) = self.scheduler.aligned_present_ts(*align_ts_us) {
                let skew = play_at_us.cast_signed() - present.cast_signed();
                self.stats.skew_samples.push(skew);
            }
            self.pending_skew.pop_front();
        }
        while self.pending_skew.len() > MAX_QUEUED_FRAMES {
            self.pending_skew.pop_front();
        }
    }

    /// Take the A/V skew samples that have not been taken yet.
    ///
    /// The samples are taken rather than copied: the caller adds them to a
    /// running total, and copying them every time would count the same
    /// sample again and again.
    pub fn take_skew_samples(&mut self) -> Vec<i64> {
        std::mem::take(&mut self.stats.skew_samples)
    }

    /// Tell the scheduler about a video frame, so it can place the audio.
    pub fn set_video_frame(&mut self, capture_ts_us: u64, present_ts_us: u64) {
        self.scheduler.set_video_frame(capture_ts_us, present_ts_us);
    }

    /// The video clock the scheduler is using.
    #[must_use]
    pub fn video_clock(&self) -> Option<VideoClock> {
        self.scheduler.video_clock()
    }

    /// The clock offset between the host and the client.
    #[must_use]
    pub fn clock_offset(&self) -> lumen_audio::ClockOffset {
        self.scheduler.offset()
    }

    /// Set the clock offset between the host and the client.
    ///
    /// The offset only takes effect when it changes: replacing the scheduler
    /// would throw away its video clock, and the frames would all be
    /// dropped for want of one.
    pub fn set_clock_offset(&mut self, offset: lumen_audio::ClockOffset) {
        if self.offset_applied == Some(offset) {
            return;
        }
        self.offset_applied = Some(offset);
        // The offset is set in place: a new scheduler would start without a
        // video clock, and every frame would be dropped for want of one.
        self.scheduler.set_offset(offset);
    }

    /// Read, decode and play whatever audio is ready.
    ///
    /// Returns the number of frames that were played.
    ///
    /// # Errors
    /// Returns a human-readable description when the socket fails.
    pub fn pump(&mut self) -> Result<usize, String> {
        let mut buffer = vec![0u8; DATAGRAM_BUFFER];
        let mut played = 0usize;
        loop {
            let (n, _from) = match self.socket.recv_from(&mut buffer) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.to_string()),
            };
            if n < AUDIO_HEADER_LEN {
                self.stats.auth_failures += 1;
                continue;
            }
            let (header_bytes, ciphertext) = buffer[..n].split_at(AUDIO_HEADER_LEN);
            let Ok(header) = AudioHeader::parse(header_bytes) else {
                self.stats.auth_failures += 1;
                continue;
            };
            let cipher = self.cipher.clone();
            self.stats.datagrams += 1;
            let Ok(plain) = cipher.open(header_bytes, ciphertext, header.seq) else {
                self.stats.auth_failures += 1;
                continue;
            };
            // One audio frame fits in one datagram, so it is one slice. The
            // receiver expects the header at the front of the slice.
            let mut packet = header_bytes.to_vec();
            packet.extend_from_slice(&plain);
            let slice = AudioSlice {
                index: 0,
                count: 1,
                payload: packet,
            };
            let Ok(Some(audio)) = self.receiver.push(slice) else {
                self.stats.protocol_errors += 1;
                continue;
            };
            let now = monotonic_us();
            match self.scheduler.schedule_received(&audio, now) {
                lumen_audio::Playback::Play {
                    play_at_us,
                    late_by_us: _,
                } => {
                    // The skew is how far the audio is from the video frame
                    // it aligns to. It is the difference between the moment
                    // the audio must be played and the moment the video was
                    // presented.
                    // The skew is only measured once the scheduler knows
                    // which video frame the audio belongs to. Before the
                    // history has filled up, the closest frame may be the
                    // oldest one, and the measurement would be the start-up
                    // transient rather than the alignment.
                    // The A/V skew is the error in the alignment: how far
                    // the audio was played from the video frame of the same
                    // moment. It can only be measured once that video frame
                    // has been presented, so it waits.
                    self.pending_skew
                        .push_back((audio.header.video_capture_ts_us, play_at_us));
                    // The frame waits here until its play time comes. The
                    // stage never sleeps: a sleeping stage would stop
                    // reading the socket and would stop telling the
                    // scheduler which video frame is on screen.
                    match self.decoder.decode(&audio.payload) {
                        Ok(decoded) => {
                            self.queued.push_back((play_at_us, decoded));
                            while self.queued.len() > MAX_QUEUED_FRAMES {
                                self.queued.pop_front();
                                self.stats.frames_dropped += 1;
                            }
                        }
                        Err(_) => {
                            self.stats.decode_errors += 1;
                        }
                    }
                }
                lumen_audio::Playback::Drop { .. } => {
                    // The frame was too late, or there was no clock yet.
                    // The scheduler's own counters say which.
                    self.stats.frames_dropped += 1;
                }
            }
        }
        // The frames whose play time has come go to the sink.
        let now = monotonic_us();
        while let Some((play_at_us, _)) = self.queued.front() {
            if *play_at_us > now {
                break;
            }
            let samples = self.queued.pop_front().map(|(_, s)| s).unwrap_or_default();
            self.sink.push(&samples);
            played += 1;
            self.stats.frames_played += 1;
        }
        Ok(played)
    }
}

/// Microseconds on the monotonic clock.
pub fn monotonic_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs * 1_000_000 + nanos / 1_000
}

/// The address the client binds its audio socket to.
#[must_use]
pub fn client_audio_addr(addr: SocketAddr) -> SocketAddr {
    addr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counters_start_at_zero() {
        let counters = AudioCounters::default();
        assert_eq!(counters.frames_played, 0);
        assert_eq!(counters.frames_dropped, 0);
    }
}
