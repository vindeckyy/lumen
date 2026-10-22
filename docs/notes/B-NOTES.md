# Agent B notes — M4 audio (A/V sync, wire format, session path)

## What I changed

Files created:

- `crates/lumen-protocol/src/audio.rs` — the audio wire format: `AudioHeader`
  (40 bytes, little-endian), `AudioKind` (`Opus`, `Pcm`), `AudioError`,
  `frame_duration_us`, `is_frame_duration`, `to_bytes`/`parse`/`split`.
- `crates/lumen-audio/src/clock.rs` — `ClockOffset` and `estimate_offset`,
  the NTP-style estimator that mirrors
  `crates/lumen-host-service/src/stages.rs::estimate_offset`.
- `crates/lumen-audio/src/session.rs` — `AudioSender`, `AudioReceiver`,
  `AudioPacketSink`, `AudioSlice`, `AudioPacket`, `ReceivedAudio`,
  `AudioStats`, `SessionError`.
- `crates/lumen-audio/src/playback.rs` — `PlaybackScheduler`, `VideoClock`,
  `Playback`, `DropReason`, `PlaybackStats`.

Files changed:

- `crates/lumen-audio/src/lib.rs` — registered the three new modules and
  re-exported the new API plus the protocol audio types.
- `crates/lumen-audio/Cargo.toml` — added `lumen-protocol` and `thiserror`.

`Cargo.lock` was updated by cargo when the new dependency edge was added
(`lumen-audio` now depends on `lumen-protocol` and `thiserror`).

## Wiring needed

The lead has to apply these; I was not allowed to touch the files.

**Item 1 is a hard blocker: `lumen-audio` does not compile until the protocol
crate registers the new module.** The audio crate depends on
`lumen_protocol::audio`.

### 1. Register the module in the protocol crate

`crates/lumen-protocol/src/lib.rs`, in the module list:

```rust
pub mod audio;
```

The list is alphabetical, so it goes above `pub mod capability;`.

Optional, for callers that want the types at the crate root (the audio crate
re-exports them itself, so this is not required):

```rust
pub use audio::{AUDIO_HEADER_LEN, AUDIO_MAGIC, AUDIO_VERSION, AudioHeader, AudioKind};
```

### 2. The transport needs an audio packetise path

`crates/lumen-transport/src/media.rs::MediaSender::packetize` hardcodes
`MessageKind::VideoData` and returns fully sealed datagrams. The audio path
needs:

- a variant that seals with `MessageKind::Audio` and splits the audio packet
  into `SLICE_MAX_PAYLOAD` slices the way the video path does (the last slice
  is padded; the audio header carries the true payload length, so the
  receiver trims the padding);
- a way for the client to hand the decrypted slice payloads to
  `lumen_audio::AudioReceiver`, which takes
  `AudioSlice { index, count, payload }`. `MediaReceiver::on_datagram` today
  returns a fully reassembled `AssembledAu`, so the audio path needs the
  per-slice payloads and the slice index and count.

The transport must not depend on `lumen-audio`: the seam is the
`lumen_audio::AudioPacketSink` trait, which the transport (or the host
service) implements.

### 3. The host and the client pipelines

Host side:

- capture audio into `lumen_audio::FrameEncoder`, which yields an
  `AudioFrame` per encoded frame with its capture timestamp;
- stamp the header with the capture timestamp of the video frame the audio
  aligns to: `AudioSender::header(video_capture_ts_us)` does that, and
  `AudioSender::send` adds the frame index and the payload length;
- implement `AudioPacketSink` over the transport's sender.

Client side:

- feed the decrypted slices into `AudioReceiver::push`, which yields
  `ReceivedAudio { header, payload }`;
- decode the payload with `lumen_audio::new_decoder`;
- give the scheduler the video clock: `PlaybackScheduler::set_video_frame(host_capture_ts_us, local_present_ts_us)`
  for every presented video frame, and the clock offset
  (`ClockOffset::estimate(t0_host, t1_client, t2_host)`) from the ping
  exchange the host already runs;
- `PlaybackScheduler::schedule_received(&audio, now_us)` gives the
  client-clock time at which the frame must be played. A frame that is more
  than `DEFAULT_MAX_LATENESS_US` (20 ms, the plan's A/V skew bound) late is
  dropped, not played late.

### 4. The fuzz workspace

`fuzz/Cargo.lock` needs a refresh because `lumen-audio` gained a dependency
edge. `cargo build` inside `fuzz/` does it. The `opus_frame` target uses
`AudioConfig`, `FrameEncoder` and `new_decoder`, none of which changed.

## Notes on the design

- The audio crate does not depend on the transport and the transport does not
  depend on the audio crate. `AudioPacketSink` is the only seam.
- The audio crate does not depend on `lumen-host-service` either (that would
  be a cycle risk), so the NTP-style offset calculation is implemented twice:
  in `crates/lumen-host-service/src/stages.rs` and in
  `crates/lumen-audio/src/clock.rs`. Keep the two in step.
- The header is 40 bytes: magic `LA`, version, kind, channels, reserved,
  sample rate, frame duration, frame index, the video capture timestamp the
  audio aligns to, the payload length, reserved. The layout is pinned by the
  test `the_layout_is_pinned`.
- The header's frame duration is restricted to 2.5, 5 and 10 ms, which are
  the Opus frame sizes and the sizes `AudioConfig::frame_ms` allows.
- The receiver expects slice 0 of a packet to carry the audio header: it
  reads the frame index from it to tell a new packet from a late slice of an
  old one. With 1200-byte transport slices the header is always in slice 0.
  A packet may use at most `MAX_AUDIO_SLICES` (64) slices.

## Verification

I was not allowed to edit `crates/lumen-protocol/src/lib.rs`, so the tests
were run in a scratch copy of the workspace at
`/tmp/opencode/lumen-scratch`, with the `pub mod audio;` line applied there
by a sync script. The real tree is unchanged apart from the files listed
above.

In the scratch copy, with the workspace lint set and the `pub mod audio;`
line applied:

```
cargo clippy --workspace --all-targets -- -D warnings   # clean
cargo test -p lumen-audio -p lumen-protocol             # 45 + 64 passed
cargo test --workspace                                   # all green
```

`lumen-audio` went from 16 to 45 unit tests, `lumen-protocol` from 46 to 64.
The live PipeWire capture test passes.
