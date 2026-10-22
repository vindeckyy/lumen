# Lumen design

Status: M0 is complete. M1 through M8 are not complete. Blocker B1 (P-frame decode) and blocker B2 (DMA-BUF contract) are complete. Blocker B3 (media pipeline) is largely done: the host pipeline and the client pipeline exist and run end to end. The host captures a wlroots output, converts to NV12 on the CPU, encodes with VA-API H.264, slices and seals the access units and sends them over UDP; the client receives them, reassembles them, decodes with VA-API, converts to BGRX and presents through Wayland or X11. Pairing is enforced (D52), so the earlier statement that pairing was not enforced and that the host must not be exposed to a network no longer holds. The parity-only recovery defect (G2) is fixed. A measured end-to-end run exists: 1280x720 at 30 fps over loopback and over the network namespace pair. In the committed runs every access unit that was delivered was decoded and presented with zero decode errors, and the namespace run lost zero packets. The frame counts vary with the load on the machine, so read them from the artifacts rather than from this paragraph. These are software counters, not glass-to-glass. No glass-to-glass latency has been measured, because there is no photodiode or camera rig in this environment. Still not implemented: HEVC and AV1, NVENC, Vulkan encode and decode, PipeWire capture, audio wiring, clipboard, multi-monitor, resolution change, the adaptive FEC policy, the bandwidth adaptation, and the input path. The workspace builds and clippy passes with `-D warnings`; the last full `cargo test --workspace` predates the most recent edits, so re-run it before trusting it. See `known-limits.md` for the open defects and `benchmarks.md` for what was exercised.

Reversibility column: "yes" means the choice can be changed by editing one module or manifest. "costly" means other crates or the wire format depend on it.

## 1. Repository layout

| Crate | Role | Status |
|---|---|---|
| `lumen-protocol` | Media header, slicing, capability bitmap, `PixelFormat`, `FrameSource`/`FrameSink`/`VideoEncoder`/`VideoDecoder` traits, raw codec | Implemented, 46 tests |
| `lumen-crypto` | Noise XX handshake (snow), per-direction AEAD, key derivation, pairing code | Implemented, 20 tests |
| `lumen-transport` | UDP socket options, FEC (GF(256) block parity), pacing, retransmit queue, slice assembler, loss/jitter injector | Implemented, 48 tests. Wired to both pipelines |
| `lumen-host-service` | Control channel: framing, encryption, message codec, listener. Noise session, four per-direction session keys, control and media helpers, pairing store, four-stage media pipeline | Implemented, 22 tests. Binary `lumen-host` |
| `lumen-ffi-va` | Only VA-API FFI crate. Runtime-loaded libva, safe wrappers, DMA-BUF export/import | Implemented, tested on AMD iGPU |
| `lumen-encode-vaapi` | H.264 encoder on `VideoEncoder`. An HEVC entry point exists but HEVC is not implemented | Implemented, 27 unit + 4 hardware tests |
| `lumen-decode-vaapi` | H.264 decoder on `VideoDecoder`. I and P slices with a decoded picture buffer | Implemented, 41 unit tests and 1 roundtrip test |
| `lumen-ffi-libei` | Only crate with `unsafe` for libei. Runtime-loaded libei | Implemented, compile-checked, EIS path not exercised |
| `lumen-input-wayland` | `InputEvent` types, InputCapture/EIS source, seat fallback, keyboard keymap | Implemented, 15 tests. EIS path not exercised live |
| `lumen-input-inject` | uinput (via evdev), XTest, EI injection, text injection | Implemented, 10 tests |
| `lumen-capture-wlr` | wlroots screencopy, DMA-BUF and wl_shm, output listing | Implemented, 7 unit tests and 2 wlroots tests, verified on headless sway |
| `lumen-capture-x11` | MIT-SHM and GetImage ladder, XDamage, XFixes cursor. DRI3 opt-in | Implemented, 1 unit test plus `env_display` and `xvfb` integration tests, verified on Xvfb and Xwayland |
| `lumen-render` | Client presentation: Wayland (shm and dmabuf) and X11 (SHM, Present) | Implemented, 6 unit tests, 3 Wayland tests, 1 X11 test, verified on Xvfb and headless sway |
| `lumen-audio` | PipeWire virtual-sink capture, Opus (system libopus) and PCM encode/decode, playback sink | Implemented, 16 tests, live PipeWire capture verified |
| `lumen-bench` | Test pattern, barcode, timestamp recorder, percentile report, photodiode trigger simulation, run report merge | Implemented, 24 tests. Binary `lumen-bench` with `report` and `rig-selftest`. No camera capture loop |
| `lumen-capture-pipewire` | Portal ScreenCast capture via PipeWire DMA-BUF | Stub |
| `lumen-input-x11` | X11 client-side XInput2 capture | Stub |
| `lumen-client-desktop` | Client application: receive, decode and present pipeline, pairing answer | Implemented, 4 tests. Binary `lumen-client` |
| `lumen-encode-nvenc`, `lumen-ffi-nvenc` | NVENC encoder and its FFI | Stub |
| `lumen-encode-vulkan`, `lumen-decode-vulkan` | Vulkan Video encode and decode | Stub |
| `vendor/opus` | Vendored copy of `opus` 0.4.0 with the bundled libopus build disabled (see decision D4) | Patched manifest only |

Stub crates contain no code and no tests. They exist so the workspace shape matches the plan.

The workspace has 22 members. `README.md`, `LICENSE-MIT`, `LICENSE-APACHE` and `.github/workflows/` exist. The `harness/` directory does not exist yet. See `known-limits.md`.

## 2. Stage budgets

These are design budgets for Tier A (1080p60, wired gigabit), taken from the 8 ms p50 target. They are not measurements. `lumen-bench` has the `Stage` enum and recorder to instrument them, but the pipeline does not yet call them.

| Stage | Budget (ms, p50) | Owner crate |
|---|---|---|
| Capture (host, ready to timestamp) | 1.0 | `lumen-capture-wlr` / `lumen-capture-pipewire` |
| Colour conversion | 0.3 | `lumen-protocol/src/color.rs` (CPU, D49) |
| Encode | 2.5 | `lumen-encode-vaapi` |
| Packetize and FEC | 0.3 | `lumen-transport` |
| Network (LAN, one way) | 0.5 | socket and pacer |
| Receive and reassemble | 0.5 | `lumen-transport` |
| Decode | 1.5 | `lumen-decode-vaapi` |
| Present | 1.0 | `lumen-render` |
| Display scan-out | display-specific | measured with photodiode only |

The budgets sum to 7.6 ms before display scan-out. Nothing here has been timed.

## 3. Decisions

| ID | Decision | Data and reason | Reversible |
|---|---|---|---|
| D1 | Input capture uses the InputCapture portal, not RemoteDesktop. The spec's M3.3 wording is wrong. | RemoteDesktop `ConnectToEIS` is the sender side and carries no local input. | yes |
| D2 | uinput goes through `evdev` 0.13, not `nix`. | `nix` ioctl macros generate `unsafe fn`. Unsafe is confined to `lumen-ffi-libei`. | yes |
| D3 | `UI_SET_PHYS` is not set on uinput devices. | `evdev` 0.13.2 encodes that ioctl with size 1; the kernel returns EINVAL (confirmed with strace). The phys string is cosmetic. | yes |
| D4 | Opus is built against system libopus. The `opus` crate is vendored in `vendor/opus` with `default-features = false`. | Upstream `opus` 0.4 pulls `opusic-sys` with a bundled libopus compiled by cmake. That is C compiled into the product, which the rules forbid. `ldd` confirms the system `libopus.so.0` is linked. | costly: a vendored copy to maintain |
| D5 | `AudioConfig.frame_ms` is an integer in tenths of a millisecond (25 = 2.5 ms, 50 = 5 ms, 100 = 10 ms). | The spec's `u32` milliseconds cannot express 2.5 ms. | yes |
| D6 | wlroots capture allocates DMA-BUF buffers per frame and does not pool them. | The consumer's release point is unknown, so a pool could be reused too early. | yes; allocation cost to revisit |
| D7 | X11 capture default ladder is MIT-SHM then GetImage. DRI3 is opt-in. | DRI3 frames are unfenced. An immediate CPU read returned zeros; the same read after 500 ms was correct. The fix needs a fence fd, which needs `unsafe` outside an FFI crate. | yes: one `if` in `ladder()` |
| D8 | Presentation uses `wl_shm` and `zwp_linux_dmabuf_v1` with `create_immed`. No EGL or gbm in the presenter. | The compositor imports the dmabuf itself, so the client needs no EGL image. | yes |
| D9 | `wp_commit_timing` is not bound. Presentation timestamps come from `wp_presentation` only. | The protocol in wayland-protocols 0.32 has `set_timestamp` but no feedback event. | yes |
| D10 | The Wayland presenter commits right after attach and does not wait for a frame callback. | A frame callback adds a compositor round trip to every frame. | yes |
| D11 | The encoder device order is `LUMEN_VA_DEVICE`, then renderD129, then renderD128, then others. | On this machine renderD129 is the AMD iGPU with H.264 and HEVC encode. renderD128 is NVIDIA with no VA-API encode. | yes |
| D12 | The encoder emits IPPP with no periodic IDR. `ip_period` is 1, CQP QP is 22 by default, and IDR is on request. | Spec: infinite GOP, IDR on demand, no B-frames. | costly: the decoder must support P-frames. That dependency is now met, because P-frame decode is implemented and the decoder round trip decodes an IDR plus 29 P frames. Changing the GOP shape still changes the bitstream |
| D13 | The decoder reads back through `vaGetImage`, not `vaDeriveImage`. | `vaDeriveImage` returns raw tiled surface memory on radeonsi. | yes |
| D14 | The decoder DMA-BUF path exists and is guarded to linear NV12 in one object. It is not used on this hardware. | The AMD export carries modifier `0x0200000000000901` (tiled), so the guard rejects it. | yes |
| D15 | libva and libva-drm are loaded at runtime with `libloading`. | No link-time dependency, so the crate builds where libva is absent. `vaGetDisplayDRM` lives in `libva-drm.so.2`. | yes |
| D16 | VA calls that the spec lists but libva 2.24 lacks are not wrapped: `vaCreateConfigEntry`, `vaMapPicture`, `vaUnmapPicture`, `vaDisplayContextGetDevices`, `vaQueryContextAttributes`, `vaGetErrorLog`. `vaCreateConfig` and `vaErrorStr` are used instead. | Checked against `/usr/include/va/va.h` and the exported symbols of `libva.so.2`. | yes |
| D17 | `read_dmabuf` is not used on VA-allocated surfaces. CPU readback uses `derive_image`/`get_image`. | amdgpu returns EPERM on CPU mmap of VA-allocated DMA-BUFs. `read_dmabuf` is for CPU-mappable buffers such as PipeWire or GBM output. | yes |
| D18 | `PixelFormat::bytes_per_pixel` is replaced by `frame_len(width, height)`. | It returned 3 for NV12 and I420, which are 1.5 bytes per pixel. The old value was wrong for every 4:2:0 frame. | yes |
| D19 | Control payloads use hand-written little-endian fields, not postcard. | `protocol.md` section 7 says postcard. The explicit layout needs no dependency and is simple to fuzz. The spec was updated to match. | yes |
| D20 | The control channel uses AES-256-GCM with one key and counter per direction. `TCP_NODELAY` is set. | `protocol.md` section 6 picks AES-GCM for AES-NI hardware. Nagle would delay small messages. | yes |
| D21 | `lumen-crypto` uses snow 0.10 with feature `risky-raw-split`, then HKDF-SHA256 over the raw split keys. | snow 0.10 does not offer a stable call that exports transport key material. The feature name says "risky". The key derivation is covered by tests, but the snow API is the weak point. | costly. Replace when snow exposes the material |
| D22 | `SliceAssembler::push` and `try_recover` return `AssembledAu { capture_ts_us, data }`. | The spec requires a capture timestamp at every stage. The timestamp is already in the slice header. | yes |
| D23 | The retransmit deadline is `frame_start` plus 1.5 frame intervals plus slack, on the receiver clock. | Capture timestamps are on the host clock. Mixing the two clocks in one comparison was wrong, so the per-entry timestamp was removed. | yes |
| D24 | FEC uses GF(256) parity: parity 0 is the sum of all data slices, parity 1 is the sum of index-weighted slices. | XOR-only parity cannot recover two losses in one block because the equations collide. Test `fec_recovers_two_missing_slices` covers this. | yes |
| D25 | The audio realtime callback uses `std::sync::Mutex`. It is marked `shortcut:` in the code. | Simple first version. Not measured. | yes: an SPSC ring if underruns appear |
| D26 | Virtual sink for audio is a null-audio-sink node, owned by the PipeWire connection. | The sink disappears on drop or crash, so no stale sinks are left behind. | yes |
| D27 | The environment check found PipeWire 1.6.8 running, with a socket at `/run/user/1000/pipewire-0`. Earlier notes said no daemon ran; that was wrong. | Confirmed by `pw-cli` and the socket. Live capture test ran against the real daemon. | n/a |
| D28 | Four per-direction session keys (`control_h2c`, `control_c2h`, `media_h2c`, `media_c2h`) instead of one control key. | One shared control key forces both directions to start at counter 0 under one key, which reuses nonces. Caught by inspection before any code depended on it. | Pre-v1 only (wire change). |
| D29 | Session id derived from the handshake secret (HKDF, `lumen-session-id-v1`), not chosen by the host. `HelloAck.session_id` carries the same value and the client checks it. | The control ciphers need the id before the first record, so the id cannot come from `HelloAck`. | Pre-v1 only. |
| D30 | AEAD fixed to AES-256-GCM on every channel. No negotiation. | Both test machines have AES-NI: the Ryzen 4600H is verified here. The Celeron N4020 is expected to have it but is not verified (no device). Negotiation needs a handshake field, which is not worth it before the first milestone. | Yes (add capability and handshake field). |
| D31 | Noise XX runs on TCP with u16 big-endian length prefixes. Control records after it use u32 little-endian prefixes. | Keeps the handshake independent of the control record format. | Pre-v1 only. |
| D32 | Client is the Noise initiator; host is the responder. Host sends `h2c` keys and receives `c2h`. | Matches "client opens the control connection" in `protocol.md`. | Pre-v1 only. |
| D33 | Pairing not enforced. `HelloAck.paired` is always `false`. | Pairing UI and store are later milestones. Recorded as a security gap (O-1). | **Not reversible as a gap; must be closed.** |
| D34 | Media header is 42 bytes, carries `session_id` and a 64-bit `seq` that is also the AEAD counter, and both are authenticated as AAD. | The receiver needs `seq` before opening, so it must be in clear. | Pre-v1 only. |
| D35 | FEC is GF(256) with two parity slices per 8 data slices (`ParityLevel::Light`), not XOR. Recovers two losses per block. `Heavy` returns `UnsupportedParity`. | XOR-only parities cannot recover two losses in one block, because same-parity pairs collide. This was found while writing the two-loss recovery tests. GF(256) recovers two distinct losses. Plan M1.5 said XOR. | Yes (encoder and decoder both change). |
| D36 | Explicit LINEAR allocation for DMA-BUF capture (modifier list `[Linear]`). | Flag-based allocation fails on NVIDIA and returns `INVALID` on AMD. Costs the driver's tiled layout performance. | Yes. Revisit with a tiled path once the consumer handles implicit modifiers. |
| D37 | Decoder default output is CPU NV12 (`vaGetImage`). DMA-BUF is opt-in. | The DMA-BUF tiled NV12 path has no consumer yet. Default has a CPU copy, which violates the plan's zero-copy rule. | Yes. Deviation to close (O-7). |
| D38 | `DRM_FORMAT_MOD_INVALID` lives in `lumen-protocol`, value `0x00ff_ffff_ffff_ffff`. | Two crates defined it separately. | Yes. |
| D39 | `DmabufImage` carries per-plane offset and stride, not one pitch. | AMD tiled NV12 has UV at a different pitch and offset from Y. | Yes. |
| D40 | Input path has a 64-entry replay window, beyond the spec. | AEAD alone allows a captured keystroke to be replayed. | Yes (remove, tests isolated). |
| D41 | Input sequence equals the AEAD counter and is in clear AAD. | Same reason as D34. | Pre-v1 only. |
| D42 | Opus links the system `libopus` through pkg-config, via a patched `opus` crate in `vendor/opus`, not the cmake-bundled build. | "No C compiled into product" is kept; dynamic link to an OS library is allowed by the plan. | Yes. |
| D43 | Audio `frame_ms` in tenths of a millisecond. | Avoids float frame sizes (2.5 ms). | Yes. |
| D44 | Decoder sets `SHORT_TERM_REFERENCE` on reference pictures (ffmpeg sends 0). | Matches the spec of the reference flag. Output identical here. Not verified across drivers. | Yes. |
| D45 | Encoder runs IPPP with infinite GOP and IDR on request. No intra-refresh. | Plan asks for infinite GOP and on-demand IDR; intra-refresh deferred. | Yes. |
| D46 | Renderer uses `wl_shm` and linear XR24 DMA-BUF only. No NV12 import. | Decoded NV12 is tiled and needs a YUV shader that does not exist yet. | Yes. |
| D47 | Host reports `paired = false` (see D33). | Explicit, not silent. | Yes. |
| D48 | Capture backend order and selection not yet decided. PipeWire is stubbed. | Plan D-CAPTURE says portal+PipeWire first, wlr when portal is unavailable. Only wlr exists. Record as deviation until PipeWire lands. | Yes. |
| D49 | The colour conversion between the capture format and the encoder format runs on the CPU: BGRX to NV12 on the host, NV12 to BGRX on the client. | The plan puts colour conversion on the GPU. No GPU path exists yet, and the pipeline cannot run without it. | Yes. Both ends change. |
| D50 | The media peer address is not negotiated. The host sends media to the IP address of the control connection's peer, on a fixed client media port (24801 by default, set with `--client-media-port`). | Adding a field to the handshake would change the wire format for a case the LAN-only threat model does not need. The control connection already proves the peer's address. | Yes. A handshake field can be added later. |
| D51 | The host pipeline is four stages on four threads: capture, encode, send, control. The queue between capture and encode holds one frame. The control stage is separate so that a slow decode never delays a keyframe request. | The plan asks for one thread per stage and queues of depth 1 or 2. A depth-one queue gives backpressure without unbounded latency. | Yes. |
| D52 | Pairing is enforced. The client owns a long-term Ed25519 identity key and proves it by signing a nonce the host supplies. The host keeps the public keys of the clients it trusts in a store file. A client the host does not trust is refused unless it answers the six-digit pairing code. | The host used to accept any client that completed the Noise handshake. That is the security gap recorded as O-1. A claimed public key proves nothing, so the client must sign a fresh host nonce; the proof binds the host key, the client key and the nonce, so it cannot be replayed into another session. | No. The store format and the control message kinds are on the wire. |
| D53 | The pairing code is derived from the host's Ed25519 public key and the client's Noise static key, so it is valid for one handshake attempt only. The derivation reduces the digest modulo one million, so the code is always six digits and two peers that rebuild it independently agree. | A code that stayed valid for the life of the host's key could be replayed. The reduction was a bug: the derivation bypassed the six digit clamp, so a peer that rebuilt the code from its wire value disagreed with the host. | Yes. |
| D54 | The control channel gained a timeout-aware receive that keeps a partially received record in a buffer. | A stage that has to send while another waits cannot do so on a channel whose receive blocks forever. The old receive used `read_exact`, so a read timeout in the middle of a record would desynchronise the stream; the buffer makes a partial record safe. | Yes. |
| D55 | The `lumen-bench` binary has two subcommands: `report`, which merges a host run and a client run into one report, and `rig-selftest`, which exercises the photodiode harness arithmetic against synthetic camera frames. | The plan's M1 acceptance needs a merged run report, and the rig self-test is how the harness is verified without the rig. | Yes. |
| D56 | The pipeline measures itself. The host timestamps its capture, encode and send stages; the client timestamps its receive, decode and present stages and reports them to the host with a `StageTimes` control message. The host estimates the offset between the two `CLOCK_MONOTONIC` clocks from a ping exchange and maps the client's timestamps onto its own, so the report carries a per-stage latency for the whole pipeline and not only for the host's half. | Without this the run report holds counters only, and no pipeline latency can be read out of a run. The plan's M1 acceptance asserts on percentiles, so the percentiles have to exist. | Yes. The `StageTimes` message is on the wire. |
| D57 | The clock offset is estimated the way NTP does it: the host pings every 500 ms, the client answers with its own clock, and the offset is the client's reply timestamp minus the midpoint of the host's exchange. | The two ends have unrelated monotonic clocks. The estimate assumes the two directions take the same time, which on a LAN is accurate to well under a millisecond. A one-way assumption is enough here and needs no extra handshake. | Yes. |
| D58 | `lumen-bench report` takes `--assert <expression>`, `--input <path>` and `--provisional`, and exits non-zero when an assertion fails. An expression is a name, an operator and a number, for example `frames_decoded>1700` or `p99_present_ms<=12`. The names are the counter fields and the stage percentiles `p50_<stage>_ms`, `p90_<stage>_ms`, `p99_<stage>_ms`, `max_<stage>_ms` and `mean_<stage>_ms`. | The plan's M1 acceptance is a command with assertions in it. A harness that cannot fail is not a harness. | Yes. |
| D59 | The parity level adapts per frame, and the block size follows it: the light level uses eight data slices and two parity slices per block, the heavy level twelve and three. The block size travels in the `flags` word of every media slice, so the receiver maps a slice index to its block without being told the parity level. | The plan's parity ratio depends on the measured loss, and the block size is what makes the overhead cap reachable. The receiver has to know the block size, and it cannot be inferred from the slice count when the last block is short. | Yes. The `flags` word is on the wire. |
| D60 | Three parity slices per block recover three losses in that block. The three parity equations are a Vandermonde system in the slice indices, solved by Gaussian elimination over GF(256). | The plan wants three parity slices at two per cent or more of loss. Two parity slices cannot recover three losses, so the third has to add a real equation rather than a repeat. | Yes. |
| D61 | The transport cipher is negotiated inside the Noise handshake. The capability bitmap travels as a payload of the first handshake message from the initiator and the second from the responder, and the payload is part of the handshake transcript, so a peer that changes it fails the final hash check. | The control channel is encrypted from its first byte, so the cipher has to be settled before the first control record exists. Carrying the bitmap in the handshake keeps that property and needs no extra round trip. The plan wants ChaCha20-Poly1305 where AES-NI is missing. | Yes. The payload is on the wire. |
| D62 | The bandwidth policy drives the encoder's constant bitrate, and the encoder can change its rate control while it runs. The client reports the round trip time of the control channel and the slices it lost, and the host differences the cumulative counts. | The plan's M2.4 needs the estimated bitrate to reach the encoder. libva carries the rate in a misc-parameter buffer, so the buffer is recreated when the rate changes. The feedback has to be differenced, because the client reports cumulative counts. | Yes. |
| D63 | The NACK deadline is the frame's capture timestamp plus 1.5 frame intervals, plus a configurable slack. Past the deadline the host does not resend the slice, the client drops the frame and asks for a keyframe. | The plan's M2.2: a late frame is dropped rather than waited for. The deadline is in the sender's clock, so both ends can compute it from the capture timestamp that is already on the wire. | Yes. |
| D64 | The audio travels on a UDP port of its own (24802 by default) and is sealed with a key pair of its own, `lumen-audio-h2c-v1` and `lumen-audio-c2h-v1`. The audio header carries the AEAD counter in the clear, so the receiver can open the payload. | The plan wants the audio and the video on separate sockets. A separate key is needed as well: the video sender and the audio sender would otherwise use the same key and the same counter, which is a nonce reuse. The counter cannot be inside the ciphertext, because the receiver needs it to open the ciphertext. | Yes. The header and the ports are on the wire. |
| D65 | The client aligns an audio frame to the video frame of the same moment. It keeps the video frames it has presented and picks the one whose capture timestamp is closest to the audio frame's capture timestamp. | The audio is captured at the host now, while the video the client has is up to one pipeline latency old. Aligning to the newest frame would make the audio lead the picture by that latency. | Yes. |
| D66 | The audio stage never sleeps. A frame that is not due yet waits in a queue until its play time. | A sleeping stage stops reading the socket, and it stops telling the scheduler which video frame is on screen, so the alignment collapses. The sound card pulls the samples at its own pace, so the queue is the right place for them. | Yes. |
| D67 | The host reads the clipboard of its own compositor and offers it to the client; the client takes it and sends the text back, and that return is the round trip. The host can be told to put a given text into its own selection, so a run can exercise the path without a person copying something. | The plan counts clipboard round trips, so one side has to send and the other has to answer. A test hook makes the path measurable in an automated run. | Yes. The message is on the wire. |
| D68 | The measurement scripts start their compositors in the user's own runtime directory and tell them apart by the Wayland socket each one picks, and the readiness check reads the socket table instead of connecting to the port. | The PipeWire socket lives in the user's runtime directory, so an audio session cannot reach it from a private one. Connecting to the control port to test it leaves a session thread waiting for a handshake that never comes. | Yes. |
| D69 | The parity level is asymmetric: it rises at once when the measured loss asks for it, and falls only after six windows without a loss. The client reports the link every 100 ms and the estimator's window is 500 ms. | A level that fell as soon as the loss dipped would leave the stream unprotected exactly when the link is about to get worse. The window has to be shorter than the level's fall delay and longer than the interval at which the loss is reported, or the counters are reset before the report lands and the loss is never seen. | Yes. |
| D70 | The control plane is a dependency-free HTTP server in a new crate, `lumen-web`, embedded in the host and reached with `--web-listen`. It binds to a loopback address, every change has to carry a token the host printed, and a request that names another origin is refused. | The plan has no web interface, so this is an addition. A web framework would bring a large dependency tree into a project whose policy is permissive licences only and whose crates are small. The server answers one page, one state document, one command endpoint and one event stream, which does not need a framework. | Yes. The crate is new and nothing else depends on it. |
| D71 | The host's log is kept in a bounded ring buffer, and the dashboard asks for it on its own instead of having it in the event stream. | The log changes rarely and the event stream is sent twice a second, so carrying the log in it would be most of the traffic. | Yes. |
| D72 | The picture the host serves is a setting. A client that cannot keep up asks for a smaller one; the operator can set one from the dashboard; the host never serves a picture bigger than the one it captures. | A weak client that is given a picture it cannot reach drops frames, and a stream that drops frames is worse than a smaller stream that does not. The capture is the ceiling because the extra pixels would have to be invented. | Yes. The message is on the wire. |
| D73 | A smaller picture is produced inside the colour conversion: the conversion reads the source with an accumulator instead of a division per pixel, and the chroma averages a fixed two by two source block. | The point of a smaller picture is that it costs less. A division per pixel costs more than the whole conversion, and averaging the whole area the chroma cell covers reads every source pixel, so a downscale would cost the same as a big picture. | Yes. |
| D74 | The stage recorder keeps a bounded window of the last frames, and the percentiles are computed over that window. | The recorder held every frame of the session, so the memory grew for as long as the host ran and every percentile calculation cost the length of the whole session. A live window is what a dashboard wants, and it costs the same whatever the session has been doing. | Yes. |
| D75 | The client's quality ladder remembers the picture it failed at, and the operator's choice is the ceiling the host enforces. | Without the memory the client climbs back to a picture it has already shown it cannot reach, and the quality never settles. The block expires, so a picture that failed once is tried again later. The operator's choice is a limit the client does not cross. | Yes. |
| D76 | The host converts the captured BGRX frame to NV12 on the GPU, with the VA-API video processor (`VAEntrypointVideoProc`) on the capture's render node, and the encoder opens the same node. The CPU conversion stays as a fallback that is sticky for the session: the first GPU error turns the GPU off for that session. | The capture was copied, converted on the CPU at 6.8 ms a frame at 1080p and copied again into the encoder. The GPU path converts without a copy to host memory. A broken driver costs the session its speed, not its frames. | Yes. The CPU path is the old code, kept. |
| D77 | The client presents a decoded NV12 DMA-BUF as it is when the compositor takes NV12 with that modifier (`DirectNv12`). It converts to BGRX on the GPU when the compositor takes only BGRX (`VppBgrx`). Otherwise it presents from CPU memory (`Cpu`). Three zero-copy presents that fail in a row make the plan `Cpu` for the session. | The client's CPU conversion and `wl_shm` copy cost more than the frame interval at 1080p on the two-vCPU proxy. The plan is chosen once, from the formats the compositor advertises, so a frame does not pay for the decision. | Yes. |
| D78 | A quality request stays pending until the host sends the requested size or two seconds pass, and no new request is made meanwhile. The block after a failed picture is 60 s, down from 300 s. | Without the pending state, a frame sent before the host applied a request undoes it, and the client asks again every window. The shorter block lets a recovered client retry the bigger picture sooner. On the CPU path the 60 s block raised the drops of the two-vCPU proxy (benchmarks). The plan chose 60 s; the trade-off is open (known-limits O-39). | Yes. The constant `BLOCK_FOR` is the one place to change. |
| D79 | The parity level rises at once, as before. It falls after six lossless estimator windows, where it used to be six lossless frames. | Six frames at 30 fps is 200 ms, shorter than a window, so the fall ran faster than the estimator could see a loss. Counting windows makes the fall take the same time at every frame rate. | Yes. |
| D80 | The `fec_recoveries` counter is renamed `frames_rebuilt`, and it counts the access units the parity rebuilt. The estimator's loss is unchanged: it still comes from the link feedback. No alias is kept. | The counter was read as loss and overstates it (O-26). The name now says what it counts. The bench reader defaults the new fields, so reports written before the rename still parse. | Yes. |
| D81 | Two placements differ from the plan. The export-to-image conversion is `DmabufExport::into_image` in `lumen-ffi-va`, not `DmabufImage::from_export` in `lumen-protocol`, because the export type lives in `lumen-ffi-va` and `lumen-protocol` does not depend on it. The VPP check is `decoded_dmabuf_converts_to_bgrx_like_the_cpu_path` in `lumen-decode-vaapi`, not `crates/lumen-ffi-va/tests/vpp.rs`, because it needs a decoder. | The plan named these locations before the types were settled. The code does what the plan specifies. The plan's command `cargo test -p lumen-ffi-va --test vpp` names a test target that does not exist. | Yes. |

Decisions D28 to D48 were made in the session recorded in `docs/handoff.md`. Their reversibility wording differs from D1 to D27: "Pre-v1 only" means reversible before the wire format is frozen, "Yes" means reversible by editing one module or manifest, and "Not reversible as a gap; must be closed" marks a security gap that is recorded rather than defended.

Decisions D49 to D55 were made when the host and client pipelines landed. D33 and D47 are superseded by D52: pairing is now enforced, and `HelloAck.paired` reports the truth.

Decisions D56 to D58 were made when the pipeline started measuring itself. D56 adds a message to the wire; D57 and D58 change no wire format.

Decisions D59 to D63 were made when the transport became adaptive. D59 and D61 change the wire format; D60, D62 and D63 do not.

Decisions D64 to D68 were made when the audio and the clipboard were wired into the sessions. D64 changes the wire format; D65 to D68 do not.

Decisions D70 to D75 were made when the control plane and the quality ladder were built. D70 adds a crate, D72 adds a message to the wire; the rest change no wire format.

### Dependency policy exception: the unmaintained zvariant crates

`cargo deny check` and `cargo audit` both pass. Three advisories are
suppressed with a written reason, as the plan requires for anything it
cannot fix in-tree:

| Advisory | Crate | Why |
|---|---|---|
| RUSTSEC-2026-0370 | `zvariant` 5.15.1 | unmaintained |
| RUSTSEC-2026-0371 | `zbus_names` 4.3.5 | unmaintained |
| RUSTSEC-2026-0372 | `zvariant_derive` 5.15.1 | unmaintained |

All three come from `ashpd`, the xdg-desktop-portal client, through `zbus`
5. The `zvariant` crates are no longer maintained standalone because their
functionality moved into `zbus` 6, and no `ashpd` release uses `zbus` 6 yet,
so there is no upgrade path. The plan's rule that "no exception is granted
for GPL/AGPL code in the shipped binary" is not engaged: these advisories
say "unmaintained", not "vulnerable", and the crates are under the Apache-2.0
or MIT licences. `cargo audit` reports them as allowed warnings and exits
zero.

The rest of the policy needed no exception. The `licenses.deny` list was
removed from `deny.toml` because cargo-deny dropped the key in 0.14 and
everything outside the allow list is denied anyway. `Unicode-3.0` was added
to the allow list: it is the current name of the `Unicode-DFS-2016` the plan
already allows. `MPL-2.0` was removed from the allow list, because the plan
rejects the `xdg-desktop-portal` crate for exactly that licence. The
internal crates are marked `publish = false`, so their path dependencies are
not wildcard requirements.

## 4. M0 bootstrap

Done and verified:

- Rust 1.99.1 stable installed via rustup, pinned by `rust-toolchain.toml`.
- System packages installed, including libva, libdrm, libopus, libei, pipewire dev, xkbcommon x11 dev (installed for this milestone), Xvfb, sway, mesa VA driver.
- Host and client network namespaces (`scripts/netns-up.sh`) created and pinged each other over a veth pair. RTT was 0.045 to 0.055 ms with no netem qdisc. Teardown is clean.
- GPU probe written to `artifacts/m0/capabilities.json`: AMD Renoir (renderD129, card2) exposes H.264 and HEVC encode and decode through VA-API. NVIDIA GTX 1650 exposes Vulkan Video decode and encode, but no VA-API encode.

## 5. Spec deviations

- Control payload encoding is not postcard (D19).
- Input capture uses InputCapture, not RemoteDesktop (D1).
- Opus is built against system libopus (D4).
- DRI3 capture is opt-in (D7).
- `wp_commit_timing` is not bound (D9).
- The DMA-BUF `FrameBuffer` variant now carries the modifier, the fourcc and per-plane offset and stride (D38, D39). The contract is complete; the renderer still cannot import decoded NV12 or tiled DMA-BUF (D46, O-7). See `known-limits.md`.

## 6. Deviations from the approved plan

These are the deviations from `docs/plan.md`. They must also appear in the final report.

- FEC is GF(256), not XOR (D35).
- Decoder output is CPU by default (D37, O-7). Plan forbids CPU readback in the hot path.
- Renderer cannot import NV12 or tiled DMA-BUF (D46).
- PipeWire capture not implemented. wlr capture used (D48).
- NVENC, Vulkan encode and Vulkan decode not implemented (stubs).
- HEVC and AV1 not implemented. H.264 only.
- CPU color conversion in `lumen-protocol/src/color.rs` (plan: GPU) (D49).
- Input replay window added (D40).
- Opus dynamically links system libopus (D42).
- Input p99 under 1 ms not reliably met in one of three runs (section 5).
- Explicit LINEAR allocation costs tiled performance (D36).
- The media peer address is not negotiated; the host uses the control peer's IP on a fixed client media port (D50).
- No glass-to-glass measurement (no rig).
