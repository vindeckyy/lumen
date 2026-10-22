//! The host's audio stage.
//!
//! The host creates a PipeWire virtual sink for the session, captures the
//! audio that flows into it, encodes it as Opus and sends it on a socket of
//! its own. The audio never shares a socket with the video, so a busy video
//! path cannot delay it, and it never shares a key with the video either, so
//! the two streams cannot reuse a nonce.
//!
//! Every audio frame carries the capture timestamp of the video frame it
//! aligns to, so the client can play it against the video clock.

use std::net::SocketAddr;
use std::time::Duration;

use lumen_audio::{
    AUDIO_HEADER_LEN, AudioConfig, AudioHeader, AudioSender, FrameEncoder, VirtualSinkCapture,
};
use lumen_crypto::PacketCipher;
use lumen_transport::socket::{MediaSocket, SocketOptions};

/// The name of the virtual sink the session creates.
const SINK_NAME: &str = "lumen-session";

/// How long the capture waits for samples before giving up.
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(20);

/// The counters of the host's audio stage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioCounters {
    /// Frames sent to the client.
    pub frames_sent: u64,
    /// Bytes of encoded audio sent.
    pub bytes_sent: u64,
    /// Frames the capture could not encode.
    pub encode_errors: u64,
    /// Frames the socket refused.
    pub send_errors: u64,
}

/// The host's audio stage.
pub struct AudioStage {
    /// The stream parameters.
    config: AudioConfig,
    /// The capture of the session's virtual sink.
    capture: VirtualSinkCapture,
    /// The framer and encoder.
    encoder: FrameEncoder,
    /// The framer that stamps the headers.
    sender: AudioSender,
    /// The socket the audio goes out on.
    socket: MediaSocket,
    /// The peer the audio goes to.
    peer: SocketAddr,
    /// The cipher the audio is sealed with.
    cipher: PacketCipher,
    /// The capture timestamp of the video frame the audio aligns to.
    video_capture_ts_us: u64,
    /// The counters.
    stats: AudioCounters,
}

impl std::fmt::Debug for AudioStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioStage")
            .field("config", &self.config)
            .field("peer", &self.peer)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl AudioStage {
    /// Open the session's virtual sink and prepare the audio path.
    ///
    /// # Errors
    /// Returns a human-readable description when PipeWire is unreachable, the
    /// sink cannot be created or the socket cannot be bound.
    pub fn new(
        config: AudioConfig,
        socket: MediaSocket,
        peer: SocketAddr,
        cipher: PacketCipher,
    ) -> Result<Self, String> {
        let capture = VirtualSinkCapture::new(SINK_NAME, config.sample_rate, config.channels)?;
        let encoder = FrameEncoder::new(&config)?;
        let sender = AudioSender::new(&config)?;
        Ok(Self {
            config,
            capture,
            encoder,
            sender,
            socket,
            peer,
            cipher,
            video_capture_ts_us: 0,
            stats: AudioCounters::default(),
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
        self.stats
    }

    /// Set the capture timestamp of the video frame the audio aligns to.
    pub fn set_video_capture_ts(&mut self, ts_us: u64) {
        self.video_capture_ts_us = ts_us;
    }

    /// Capture, encode and send whatever audio is ready.
    ///
    /// Returns the number of frames that went out.
    ///
    /// # Errors
    /// Returns a human-readable description when the capture or the socket
    /// fails.
    pub fn pump(&mut self) -> Result<usize, String> {
        let Some(samples) = self.capture.recv_timeout(CAPTURE_TIMEOUT) else {
            return Ok(0);
        };
        if samples.is_empty() {
            return Ok(0);
        }
        // The audio carries the capture time of its own samples. That is
        // the moment it was recorded, and the client aligns it with the
        // video frame that was on screen at that moment.
        let capture_ts_us = monotonic_us();
        let frames = match self.encoder.push(&samples, capture_ts_us) {
            Ok(frames) => frames,
            Err(e) => {
                self.stats.encode_errors += 1;
                return Err(e);
            }
        };
        let mut sent = 0usize;
        for frame in frames {
            let header = self.sender.header(capture_ts_us);
            let cipher = self.cipher.clone();
            let socket = &self.socket;
            let peer = self.peer;
            // The header travels in the clear and is authenticated as
            // associated data. The sequence number is in it, so the peer can
            // open the payload.
            let mut sink = |packet: &[u8]| -> Result<usize, String> {
                if packet.len() < AUDIO_HEADER_LEN {
                    return Err("the audio packet is shorter than its header".into());
                }
                let (header, payload) = packet.split_at(AUDIO_HEADER_LEN);
                let seq = AudioHeader::parse(header).map_err(|e| e.to_string())?.seq;
                let sealed = cipher
                    .seal_with(header, payload, seq)
                    .map_err(|e| e.to_string())?;
                socket.send_to(&sealed, peer).map_err(|e| e.to_string())
            };
            match self.sender.send(header, &frame.data, &mut sink) {
                Ok(_) => {
                    sent += 1;
                    self.stats.frames_sent += 1;
                    self.stats.bytes_sent += frame.data.len() as u64;
                }
                Err(e) => {
                    self.stats.send_errors += 1;
                    return Err(e.to_string());
                }
            }
        }
        Ok(sent)
    }
}

/// Microseconds on the monotonic clock, the same clock the capture
/// timestamps use.
fn monotonic_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs * 1_000_000 + nanos / 1_000
}

/// The socket the audio goes out on, with the options the media path uses.
pub fn audio_socket(addr: SocketAddr) -> std::io::Result<MediaSocket> {
    MediaSocket::bind(addr, SocketOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sink_name_is_stable() {
        assert_eq!(SINK_NAME, "lumen-session");
    }

    #[test]
    fn the_counters_start_at_zero() {
        let counters = AudioCounters::default();
        assert_eq!(counters.frames_sent, 0);
        assert_eq!(counters.bytes_sent, 0);
    }
}
