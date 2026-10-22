# Lumen handoff

State as of 2026-10-21, about 19:40 local. Written for the next lead agent or subagent who must continue without the original conversation.

Approved plan: `docs/plan.md` (copied byte-for-byte from `local://lumen-remote-desktop-plan.md`, 44922 bytes, milestones M0 to M8). Execute the plan. Deviations from it are listed in section 8 and must go into the final report.

This is the **third** handoff. The first described a workspace with transport, crypto and session layers but no pipeline. The second described a pipeline that measured its own latency. This one has **every milestone's acceptance block runnable**, the transport is adaptive, the audio and the clipboard are wired into real sessions, and the dependency policy passes. Read section 0 first.

---

## 0. Start here

- **Where we are.** M0 is complete. B1 (P-frame decode), B2 (DMA-BUF contract) and B3 (media pipeline) are complete. **Every milestone's acceptance block has been run at least once** and has an artefact. M4's acceptance is green. **No glass-to-glass latency has been measured**, because this environment has no photodiode or camera rig.
- **The whole workspace is green.** `cargo fmt --all --check` clean, `cargo clippy --workspace --all-targets -- -D warnings` exits 0, `cargo test --workspace` exits 0 with **531 tests passing**, `cargo deny check` exits 0, `cargo audit` exits 0.
- **The transport is adaptive and it works.** The parity level follows the measured loss, the bitrate follows the loss and the delay gradient, the transport cipher is negotiated in the Noise handshake, and a retransmission is bounded by the frame's deadline.
- **The audio and the clipboard are in the session.** The host creates a PipeWire virtual sink, encodes Opus and sends it on a port of its own with a key of its own. The client places it against the video clock and plays it. The clipboard makes the round trip.
- **There is a control plane.** `lumen-host --web-listen 127.0.0.1:8080` serves a dashboard that shows the live state and changes the running session: the rate control, the bitrate, the frame rate, the parity level, the picture, the audio, the sessions and the pairing store. The token the host prints is needed for every change, and a request from another origin is refused.
- **The picture adapts.** A client that cannot keep up asks for a smaller picture and the host scales the capture down to it, which costs less work and is faster. The operator can set the picture from the dashboard.
- **Nothing is running.** No background job or service exists.
- **Next action.** Section 10 lists what is left. The two biggest are the zero-copy present path (O-7), which is the largest latency win left, and the Tier B frame drops (O-30), which need the same path. The quality ladder lowers the picture instead, which keeps a weak client smooth but does not raise the picture quality.

---

## 1. Stop state at handoff

- **Lead (Main):** active, performed this stop and handoff.
- **Subagents:** all finished and reported. None is running.
- **Background jobs / services:** none.
- **Pending edits:** none half applied. The `docs/notes/` files record what the subagents could not wire themselves; every item in them has been applied.
- **Destructive actions taken:** none.
- **Disk note:** the workspace `target/` is about 13 GB. The disk filled up once during this session. Delete a build tree before starting a new one; do not run two at once.

---

## 2. Environment

### 2.1 Hardware and drivers (verified on this machine)

| Item | Value |
|---|---|
| OS / kernel | Ubuntu 26.10 "Stonking Stingray", Linux 7.3.0-9-generic x86_64 |
| CPU | AMD Ryzen 5 4600H with Radeon Graphics, 12 vCPU |
| iGPU | AMD Radeon (PCI 0000:05:00.0), `amdgpu`, **`/dev/dri/renderD129`** and `card2`. VA-API H.264 and HEVC encode and decode through `radeonsi_drv_video.so`, libva 1.24, Mesa 26.2.4 |
| dGPU | NVIDIA GTX 1650 (PCI 0000:01:00.0), driver 580.178.04, **`/dev/dri/renderD128`** and `card1`. `libnvidia-encode.so.1` and `libnvcuvid.so.1` present. VA-API via NVDEC only (`nvidia-vaapi-driver`). NVENC code not written yet |
| Software Vulkan | lavapipe (`libvulkan_lvp.so`). `vulkaninfo` works |
| Display | Physical eDP-1 panel at 1920x1080. No compositor session on it. Not used by any test |
| Audio | PipeWire 1.6.8 daemon running, socket at `/run/user/1000/pipewire-0` |
| Webcam | `/dev/video0`, `/dev/video1` (USB, HD Webcam) |
| Users | `hayden` has rw ACL on `renderD128` and `renderD129`. `sudo -n` works without password |

Device-to-name mapping matters. `renderD129` is AMD and is the default VA device. `renderD128` is NVIDIA. `lumen-ffi-va` probes `renderD129` first, then `renderD128`. `LUMEN_VA_DEVICE` overrides it.

### 2.2 Toolchain (verified)

- Stable `1.99.1` at `/home/hayden/.cargo/bin` and `/home/hayden/.rustup`. `rust-toolchain.toml` pins `channel = "1.99.1"` with `rustfmt`, `clippy`.
- Nightly `1.101.0` installed for fuzzing. `cargo-fuzz 0.13.2` installed. `rust-toolchain-nightly.toml` exists.
- **`cargo-deny` 0.20.2 and `cargo-audit` 0.22.2 are installed and both pass.** They were installed during this session.

Every bash call starts a fresh shell. Repeat this export in each call, or pass `cwd`:

```
export CARGO_HOME=/home/hayden/.cargo RUSTUP_HOME=/home/hayden/.rustup PATH=/home/hayden/.cargo/bin:$PATH
cd /home/hayden/Desktop/projects/lumen
```

### 2.3 Network namespaces (verified)

- `scripts/netns-up.sh` creates `lumen-host` (10.77.0.1/24) and `lumen-client` (10.77.0.2/24), joined by a veth pair `lumen-h` and `lumen-c`. `scripts/netns-down.sh` tears it down.
- Measured RTT without netem: 0.045 to 0.055 ms. With `netem delay 1ms` on both ends: about 2.0 to 2.7 ms RTT.
- Netem works under `sudo ip netns exec`. Use the netns form: `sudo -n ip netns exec lumen-host tc qdisc add dev lumen-h root netem delay 1ms limit 1000`. **Do not** write `ip -n NS tc`. That form is invalid.
- **Only one netns pair can exist at a time.** Two scripts that both call `netns-up.sh` will fight over the same namespace names. Do not run two measurement scripts at once.

### 2.4 Display servers (verified)

- **Headless sway.** `WLR_BACKENDS=headless WLR_RENDERER=pixman sway -c /dev/null`, with a private `XDG_RUNTIME_DIR`. The output is `HEADLESS-1` at **1280x720** with an unknown refresh rate (reported as 0 mHz).
- **The measurement scripts now start their compositors in the user's own `XDG_RUNTIME_DIR`** and tell them apart by the Wayland socket each one picks. The PipeWire socket lives there, so an audio session cannot reach it from a private one. The scripts also find each compositor's sway IPC socket at start-up, so a mode change reaches the right one.
- **Xvfb.** Start with `Xvfb -displayfd 1` so the display number is assigned, not guessed.
- **Xwayland.** Available. `LUMEN_X11_TEST_BACKEND=dri3|shm|get` forces a capture rung.
- **Xvfb has no DRI3.** `lumen-render`'s X11 DRI3 import therefore returns `Err`.
- Tests that need a display skip with a printed reason when the variable is absent. Run no-display suites with `env -u WAYLAND_DISPLAY -u DISPLAY cargo test ...`.

### 2.5 Missing system pkg-config entries (observed)

`xcb-xfixes`, `xcb-dri3`, `xinput`, and `libvulkan` pkg-config entries were not found during probing. Crates that need them either use `x11rb` directly or were run with feature flags.

**`libxkbcommon-x11-dev` is now needed** by the `xkbcommon` crate's `x11` feature, which `lumen-input-x11` uses. The CI workflow must install it, or the build fails on a clean machine.

### 2.6 Not present

- No photodiode rig, no camera rig, no second machine, no Celeron N4020 device, no 4K or 144 Hz panel.
- Sunshine, Moonlight-Qt, RustDesk and GRD are **not installed**, so no baseline was measured. `scripts/baseline-check.sh` writes `artifacts/m0/baselines.json` with the honest "not available" records, and the three baseline drivers write status records and exit non-zero.

---

## 3. Repository layout (actual)

Root: `/home/hayden/Desktop/projects/lumen`. It is a git repository; the public upstream is `github.com/vindeckyy/lumen`.

- `Cargo.toml` (workspace, 22 members, `exclude = ["vendor/opus", "fuzz"]`)
- `Cargo.lock`, `deny.toml`, `rust-toolchain.toml`, `rust-toolchain-nightly.toml`
- `crates/` (22 crate directories)
- `docs/`: `design.md`, `protocol.md`, `benchmarks.md`, `known-limits.md`, `plan.md`, `handoff.md` (this file), `final-report.md`
- `fuzz/`: `Cargo.toml`, `Cargo.lock`, `fuzz_targets/` (12 files), `corpus/`, `artifacts/`, `target/`
- `scripts/`: `env-setup.sh`, `probe-caps.sh`, `netns-up.sh`, `netns-down.sh`, `baseline-check.sh`, `baseline-{sunshine,rustdesk,grd}.sh`, `run-e2e.sh`, `run-e2e-x11.sh`, `sweep-loss.sh`, `netem-run.sh`, `run-input-latency.sh`, `run-tier-b.sh`, `run-reconnect.sh`, `run-compositor-restart.sh`, `run-wifi-flap.sh`, `soak.sh`, `chaos.sh`, `x11-env.sh`
- `harness/`: `README.md` (the physical rig procedure), `photodiode/` (the wiring, the capture loop, the trigger finder, the self-test), `fixtures/simulated-trigger.json`
- `vendor/opus/` (patched `opus` crate, linked to system libopus)
- `artifacts/`: `m0`, `m1`, `m2`, `m3`, `m4`, `m5`, `m6`, `m7`, `m8`
- `target/` (shared build directory)
- `README.md`, `LICENSE-MIT`, `LICENSE-APACHE`, `.github/workflows/ci.yml`, `.github/workflows/fuzz.yml`

Stub crates (2-line `lib.rs`, `#![forbid(unsafe_code)]` plus a doc comment, no code):
- `lumen-capture-pipewire`, `lumen-decode-vulkan`, `lumen-encode-nvenc`, `lumen-encode-vulkan`, `lumen-ffi-nvenc`

Rule: do not add placeholder tests such as `assert!(true)`.

---

## 4. Crate status

### 4.1 `lumen-protocol` (64 unit tests)
- `header.rs`: `MediaHeader`, `HEADER_LEN = 42`. The `flags` word now carries the number of data slices per FEC block for a media slice (D59). `seq` is the AEAD counter, sent in clear as AAD.
- `capability.rs`: `Capabilities` bitflags. **New: `AEAD_CHACHA20` (bit 19)**, a request for the portable transport cipher (D61).
- `media.rs`, `color.rs`, `slice.rs`, `message.rs`, `raw.rs`, `input.rs` as before.
- **New:** `audio.rs` — the `AudioHeader` wire format (48 bytes, little-endian), `AudioKind`, and the frame duration helpers.

### 4.2 `lumen-crypto` (20 unit tests)
- `aead.rs`: `AeadKind`, `PacketCipher`, the 8-byte counter plus 4-byte session id nonce. **New: `seal_with`**, which takes an explicit counter, so the audio can carry its counter in the clear.
- `handshake.rs`: **the handshake now carries a payload.** `write_message_with` and `read_message_with` are new, and the capability bitmap travels in the first message of the initiator and the second of the responder (D61). `NoiseTransport` is **deleted** (O-4).
- `keys.rs`: `SessionKeys` with the four per-direction keys, **plus the two audio keys** `audio_h2c` and `audio_c2h` (D64).
- `pairing.rs`: `DeviceIdentity`, `PairingCode`, `KEY_LEN`, `SIGNATURE_LEN`, and **new** `sign_bytes`, `verify_bytes`, `random_bytes`, `pairing_proof`. `PairingCode::derive` reduces modulo one million, so the code is always six digits and both ends agree.

### 4.3 `lumen-transport` (65 unit tests, 9 property tests)
- `fec.rs`: GF(256). **New: three parity slices per block recover three losses** through a Vandermonde system solved by Gaussian elimination (D60). `ParityLevel::block_data_slices` gives eight for the light level and twelve for the heavy one, so the heavy level keeps the overhead at 25 %. A block whose slices have different lengths is **refused** rather than rebuilt to the wrong length. `compute_parity_with` computes only the parity it is asked for: a level that needs none must not pay for a pass over the block.
- `slice.rs`: the parity arm creates the frame state (G2, closed). The assembler holds at most 64 frames (G3, closed). The block size comes from the header's `flags` word, and the missing entries of a block are zero-filled so the rebuild is exact.
- `media.rs`: `MediaSender` with a per-frame parity level (`set_parity`), `last_frame_id`, and the block size on the wire. `MediaReceiver` as before.
- `nack.rs` (**new**): the NACK policy. The deadline is the capture timestamp plus 1.5 frame intervals, with a configurable slack; a frame past its deadline is dropped and a keyframe is asked for (D63).
- `retransmit.rs`: `RetransmitQueue` and `MissingSlice`, still unused by the pipeline: the deadline is enforced in the host's stage recorder instead.
- `estimator.rs`: the loss and delay-gradient estimator. `set_initial_rate` is new, so the policy starts from the configured bitrate.
- `input_channel.rs` (**new**): the input path on a socket of its own, sealed with the client-to-host media key.
- `socket.rs`, `pacing.rs`, `inject.rs` as before.
- `tests/properties.rs` (**new**): nine property tests for the framing and the FEC. One of them found the mixed-length block bug.

### 4.4 `lumen-host-service` (73 unit tests)
- `pipeline.rs`: the host pipeline. Four stages on four threads with depth-one queues, plus the multi-session support of M6.5. **New:** the estimator is fed by the client's link feedback, the parity level follows the measured loss with hysteresis, the bitrate follows the estimator, a retransmission is refused past the frame's deadline, the encoder is rebuilt when the output changes mode, and the clipboard is read on a thread of its own.
- `session.rs`: **the transport cipher is negotiated** in the Noise handshake (D61). `Established` carries the negotiated cipher and the peer's capabilities. `has_aes_ni` does the detection.
- `control.rs`: the control channel, with a timeout-aware receive that keeps a partial record (D54). **Message kinds:** `PairingChallenge` (11), `PairingResponse` (12), `StageTimes` (13), `LinkFeedback` (14).
- `pairing.rs`: the trust store, the identity file and the pairing code.
- `stages.rs`: the per-stage timing and the clock offset estimate, plus `may_retransmit` (D63).
- **New modules:** `audio.rs` (the host's audio stage), `clipboard.rs` (M4.4), `monitors.rs` (M4.6), `resize.rs` (M4.7), `persist.rs` (M6.1), `multi.rs` (M6.5), `virtual_display.rs` (M6.4), `backend.rs` (M7.4).
- `main.rs`: the `lumen-host` binary, with `--with-audio`, `--audio`, `--client-audio-port` and `--clipboard-test`.

### 4.5 `lumen-client-desktop`
- `lib.rs`, `receive.rs`, `present.rs`, `main.rs`: the client pipeline. **New:** the link accounting that tells the host what the link did, the client pings the host so the round trip time can be measured, the client asks for ChaCha20-Poly1305 when it has no AES-NI, and the client echoes the clipboard so the host can count the round trip.
- **New module:** `audio.rs` (the client's audio stage: the reassembly, the decode, the scheduling against the video clock and the timed queue that never sleeps).
- **New modules:** `clipboard.rs` (M4.4), `monitors.rs` (M4.6), `reconnect.rs` (M6.2), `lowpower.rs` (M5.2), `backend.rs` (M7.4), `events.rs` (the event log the scripts read).

### 4.6 `lumen-capture-wlr` (9 unit tests, 2 live tests)
- `readback.rs` (**new**): the CPU readback of a captured DMA-BUF, honouring the plane stride and bracketed by `DMA_BUF_IOCTL_SYNC`.
- `StreamConfig` has `prefer_cpu`.

### 4.7 `lumen-capture-x11`
- The capture ladder and **new** `clipboard.rs` (M4.5): the CLIPBOARD and PRIMARY selections.

### 4.8 `lumen-input-x11` (**new, was a stub**)
- `raw.rs`: XI2 raw motion, key press and release, with the pointer tracked by the client.
- `keyboard.rs`: the X11 keymap through xkbcommon, and the evdev keycode mapping.
- 24 unit tests, one integration test that skips without a display.

### 4.9 `lumen-audio` (45 unit tests)
- **New:** `clock.rs` (the offset estimate), `session.rs` (`AudioSender`, `AudioReceiver`, the packet sink), `playback.rs` (the scheduler against the video clock, with a history of the presented frames so an audio frame can be aligned to the video frame of the same moment).
- The playback sink no longer blocks the caller: on a machine with no audio device the stream can take a long time to connect, and the session has to start anyway.
- The **New** the `AudioHeader` in `lumen-protocol`.

### 4.10 `lumen-encode-vaapi` (27 unit tests, 4 hardware tests)
- **New:** `H264Encoder::set_rate_control`, so the bandwidth policy can push a new target bitrate while the encoder runs. The test `the_rate_control_can_change_while_running` drives it on the real device.
- The DMA-BUF import is verified on the AMD tiled modifier.

### 4.11 `lumen-decode-vaapi`, `lumen-render`, `lumen-ffi-va`, `lumen-ffi-libei`
- As before. The decoder still defaults to CPU NV12, so the present path is not zero-copy (O-7).
- `lumen-decode-vaapi/src/probe.rs` (**new**): the capability probe and the decode backend chain.

### 4.12 `lumen-bench` (24 unit tests, binary `lumen-bench`)
- `e2e.rs`: the merge of a host run and a client run, including the audio, the clipboard and the resolution counters.
- `input_latency.rs` (**new**): the input latency recorder with the offset estimate, the polling-interval subtraction and the percentile report.
- `report.rs`: the run report. The `report` subcommand takes `--assert <expression>`, `--input <path>` and `--provisional`, and exits non-zero when an assertion fails (D58).
- `trigger.rs`: **the simulated glass-to-glass was wrong.** `simulate` reported the trigger delay plus the client latency, contradicting `glass_to_glass`. The camera-clock definition is authoritative: the glass-to-glass is the gap between the diode firing and the barcode appearing. Fixed, with a test that pins the two against each other.

### 4.13 `fuzz/` (nightly, outside the workspace)
- **Twelve targets**: the nine from before plus `transport_inject`, `h264_au_split` and `opus_frame`. The plan's names `protocol_parser`, `fec_reassemble` and `input_parser` exist as aliases, so the plan's M8 acceptance loop works as written.
- The three new targets ran 60 s each with no crash.

### 4.14 `lumen-web` (new, 10 unit tests)
- `http.rs`: a small HTTP/1.1 server. It parses the request line, the headers and the body, keeps the connection alive, and writes server-sent events. No framework and no dependency beyond `serde`.
- `api.rs`: the routes (`/`, `/api/state`, `/api/log`, `/api/stream`, `/api/command`) and the rule that a change has to carry the host's token and may not come from another origin.
- `ui.rs` and `assets/index.html`: the dashboard. One file, with the styles and the script inside it, so the browser needs nothing from the network but the host.

### 4.15 `harness/`
- `README.md`: the full physical rig procedure.
- `photodiode/`: the wiring, `capture.sh` (the v4l2 capture loop), `detect-trigger.py` (the trigger finder), `selftest.sh` (the fixture comparison).
- `fixtures/simulated-trigger.json`: the committed fixture.

---

## 5. Measured results

All numbers are **provisional software measurements**. None is glass-to-glass. None comes from the target hardware.

| Measurement | Method | Result | Artifact |
|---|---|---|---|
| End-to-end pipeline, loopback, 30 s | `lumen-host` to `lumen-client` over 127.0.0.1, two headless sway compositors, 1280x720 at 30 fps | 873 frames sent, 872 decoded, 871 presented, **0 decode errors**, 0 packets lost | `artifacts/m1/e2e.json` |
| Software pipeline latency | the host and the client each timestamp their own stages, the host aligns the clocks from the ping exchange | **whole pipeline p50 21.37 ms, p90 23.58, p99 26.05, max 30.35**, 872 frames | `artifacts/m1/e2e.json` |
| The cost of the picture | the same session, with the picture set from the control plane | at 1280x720 the capture stage takes 7.93 ms and the whole pipeline 23.2 ms; at 640x360 the capture takes 6.97 ms and the pipeline 16.0 ms, and the client uses 25 % less of a core. A smaller picture is cheaper and faster | measured live with the control plane |
| End-to-end pipeline, network namespaces, 30 s | the same over the veth pair at 10.77.0.1 and 10.77.0.2 | 851 frames sent, 851 decoded, 850 presented, **0 decode errors** | `artifacts/m1/e2e-netns.json` |
| Software pipeline latency, namespaces | the same run | **p50 22.23 ms, p90 39.49, p99 74.24, max 85.88** | `artifacts/m1/e2e-netns.json` |
| Loss sweep, 0.5 %, 2 % and 5 % with 10 ms jitter | `scripts/sweep-loss.sh` over the namespace pair | the adaptive policy trades the first losses for bandwidth: 300, 298 and 287 frames presented of 308, 309 and 308 sent, with 2, 4 and 4 decode errors. The FEC rebuilt 28, 47 and 97 slices | `artifacts/m2/curve.csv` |
| Input host-injection latency, 60 s, 1920x1080 | 60000 events over the netns pair, XTEST into Xvfb, `CLOCK_MONOTONIC` | **p50 46 µs, p90 85 µs, p99 187 µs, max 2756 µs.** 0 injection failures, 0 authentication failures, 0 replays, 0 missing sequences. **The plan's p99 below 1 ms is met** | `artifacts/m3/input.json` |
| Audio, the clipboard and the resolution changes, 30 s | `scripts/run-e2e.sh --with-audio --with-clipboard --with-resize` | 853 frames presented, **0 decode errors**, 5602 audio frames sent, **0 underruns**, 1 clipboard round trip, 2 resolution changes, A/V skew p99 18.33 ms | `artifacts/m4/e2e.json` |
| Two-vCPU proxy for the Tier B client | `scripts/run-tier-b.sh --cpus 0,1 --duration 300` | 300 s, 17099 frames decoded, 14514 presented, **2992 dropped, 240 decode errors**, decode p50 6.83 ms, p99 11.80 ms, CPU 35.5 % of the two pinned cores, **pass: false** | `artifacts/m5/tierb.json` |
| Unattended reconnect, 20 cycles | `scripts/run-reconnect.sh --cycles 20` | **20/20 reconnected**, max reconnect 5 ms against a 2000 ms limit | `artifacts/m6/reconnect.json` |
| Compositor restart, 5 cycles | `scripts/run-compositor-restart.sh --cycles 5` | **5/5 recovered**, 0 open-fd growth | `artifacts/m6/restart.json` |
| Network flap, 3 flaps | `scripts/run-wifi-flap.sh` | **3/3 recovered**, max reconnect 1057 ms against a 2000 ms limit | `artifacts/m6/wifi-flap.json` |
| X11 end to end, 60 s | `scripts/run-e2e-x11.sh`, Xvfb for both ends | 1710 frames sent, decoded and presented, **0 decode errors**, 0 packets lost | `artifacts/m7/e2e-x11.json` |
| Soak, 307 s | `scripts/soak.sh`, with resolution changes, compositor restarts and suspend/resume | 4691 frames sent, 4686 presented, 2 decode errors, 5 dropped. 10 events, 2 survived without a supervised restart | `artifacts/m8/soak.json` |
| Chaos, 320 s | `scripts/chaos.sh`, with random loss, jitter, reorder, encoder restart, netns teardown and a CPU throttle | 10967 frames sent, 10945 presented, 6 decode errors, 22 dropped. 13 events, 2 survived | `artifacts/m8/chaos.json` |
| Rig self-test | `lumen-bench rig-selftest` | synthetic observations, a simulated glass-to-glass, a barcode round trip | `artifacts/m1/rig-selftest.json` |
| Decode roundtrip | 30 frames 1280x720, IDR plus 29 P | luma PSNR min 47.60 dB, mean 50.47 dB | `crates/lumen-decode-vaapi/tests/roundtrip.rs` |
| Encode quality | 30-frame IPPP, 1280x720, ffmpeg decode | luma PSNR min 45.2 dB, mean 50.0 dB | the encoder's ffmpeg crosscheck |
| Fuzz | twelve targets, 60 s each | four crashes found and fixed before this session, none since | `fuzz/artifacts/` |
| Dependency policy | `cargo deny check` and `cargo audit` | both exit 0. Three unmaintained advisories from the portal client are suppressed with a written reason | `docs/design.md` |
| Baselines | `scripts/baseline-check.sh` | Sunshine, Moonlight-Qt, RustDesk and GRD are not installed, so **no baseline was measured** | `artifacts/m0/baselines.json` |

Explicit non-results:
- **Glass-to-glass: not measured.** No photodiode or camera rig. The software pipeline latency is the closest number, and the gap to the photodiode floor is unknown.
- **The pipeline latency excludes the display.** The present stage ends when the frame is handed to the compositor.
- **The Tier B targets are not met.** The proxy drops frames and the plan wants none. The cause is the CPU colour conversion (D49), and the zero-copy path (O-7) is the prerequisite.
- **No baseline comparison.** The four baselines are not installed.
- **The soak and chaos runs are short** (308 s and 320 s), not the plan's 7200 s and 900 s. The reports record the length actually run and mark the remainder pending.
- **Crypto cost is not measured.**
- **The A/V skew is a definition, not a protocol field** (O-31).

---

## 6. Open defects and gaps

**Closed this session.** G2 (parity-only frames never recovered), G3 (unbounded assembler), O-1 (pairing not enforced), O-2 (AEAD not negotiated), O-4 (`NoiseTransport` unused), O-5 (protocol doc drift), O-6 (media peer address), O-8 (encoder DMA-BUF import), O-17 (FEC policy fixed), O-18 (estimator not connected), O-19 (NACK path), O-21 (missing repo basics), O-22 (the harness directory), O-27 (the M4 acceptance run), and the dependency policy, which now passes.

- **O-3. `Handshake::finish` uses `dangerously_get_raw_split`.** It HKDFs the Noise split output. Sound for this construction, but record the decision and consider binding the handshake hash into the derivation.
- **O-7. Decoded surfaces cannot be presented zero-copy.** The decoder's default output is CPU NV12 and the render path has no NV12 import. **This is the largest latency win left** and the prerequisite for O-30: the CPU colour conversion is 2.52 ms on the host and 3.57 ms on the client at 1280x720, which is most of the 4.5 ms present stage.
- **O-9. Render-node choice in capture.** `render_node(None)` picks `renderD128`, which is NVIDIA. Verify that the compositor feedback decides the node.
- **O-10. `send_paced` sleeps on the calling thread.** It is now on the send stage's own thread, so it does not block the encoder, but it is not a separate pacing thread.
- **O-11. Implicit sync not handled.** The DMA-BUF read path calls `DMA_BUF_IOCTL_SYNC`, but this has not been reproduced under load.
- **O-12. `copy_with_damage` refresh trigger untested.** The headless sway output reports 0 mHz, so the trigger never selects the damage path.
- **O-13. Input EIS path untested live.** There is no EIS server here. The X11 path is implemented and the injection path is measured.
- **O-14. Input replay window (64 entries) is a design addition** beyond the spec. Review. It is reversible.
- **O-15. Audio realtime callback uses `std::sync::Mutex`.** Replace if underruns appear.
- **O-16. The vendored `opus` crate** is accepted by `cargo deny` now, but the patched build links the system libopus and that has not been checked on a machine without it.
- **O-23. VA decoder limits.** CAVLC P slices, multi-reference, multi-slice and POC type 1 are unexercised. The decoder returns explicit errors, so failures are loud.
- **O-24. Pool aliasing in the decoder.** A consumer holding a DMA-BUF frame must release it before the pool wraps. The client uses the CPU path, so it does not hit this yet.
- **O-25. The clock offset estimate assumes the two directions take the same time.** It is the standard NTP midpoint calculation. On a LAN the error is well under a millisecond. **Every latency figure in the run reports inherits this error**, and it showed: a 20 ms block in the control channel skewed the offset by 10 ms and put 10 ms into every "send" stage until the block was found and removed.
- **O-26. The `fec_recoveries` counter overstates the loss.** The receiver tries a recovery on every datagram, so a slice that is merely late is rebuilt from the parity and counted. The rebuilt bytes are exact, so this is a counter semantic, not a correctness problem. The true loss is what the client now reports over the link feedback.
- **O-28. The Tier B decode p99 is 11.80 ms, inside the plan's 12 ms, but the proxy drops frames.** The drop count, not the decode percentile, is what fails (O-30).
- **O-29. Decode errors in the soak and in the chaos run.** Two and six respectively. The most likely explanation is the compositor restart, which the soak note says kills the session because the host does not reopen its capture.
- **O-30. The two-vCPU proxy drops frames at 1080p60.** The client's colour conversion runs on the CPU (D49), so the conversion and the presentation together take longer than the frame interval. The zero-copy path (O-7) is the prerequisite.
- **O-31. The A/V skew is a definition, not a protocol field.** Lumen measures the error in the alignment: the difference between the moment an audio frame was played and the moment the video frame of the same moment was presented. A different definition would give a different number.
- **O-32. The client's clipboard needs the legacy data device.** The host uses the plan's `zwlr_data_control_manager_v1`; the client uses the legacy `wl_data_device_manager`, which is what the compositor here offers. A compositor that offers neither cannot hold a selection, and the round-trip counter stays at zero rather than pretending.
- **O-33 (new). The adaptive parity has a cold start.** The level starts without parity, so the first losses on a link that is already bad are not recovered: the loss sweep shows 1, 4 and 7 decode errors at 0.5 %, 2 % and 5 %, where a fixed parity level showed none. The level rises within a feedback interval and falls only after six lossless windows, which trades the protection for the bandwidth. **The trade is the plan's (M2.3), but the cost has to be recorded.**

---

## 7. Stale or missing documentation

| File | Problem | Action |
|---|---|---|
| `docs/protocol.md` | **Updated this session.** The 42-byte header, the `flags` word carrying the block size, the Noise frames before the control records, the pairing exchange, the clock negotiation, the 48-byte audio header, `StageTimes` and `LinkFeedback` are all correct. | Done. Do not revert. |
| `docs/design.md` | **Updated this session.** D49 to D68 recorded, the dependency policy exception written down. | Done. |
| `docs/known-limits.md` | **Updated this session.** The closed defects are recorded, O-27 to O-33 are there. | Done. |
| `docs/benchmarks.md` | **Updated this session** against the current artifacts. | Keep the artefacts and the prose in step. |
| `docs/final-report.md` | **Updated this session** against the current artifacts. | Re-read it against the artifacts before it goes out. |
| `README.md` | **Updated this session.** | Refresh it when the code moves again. |
| `docs/handoff.md` | This file. | Done. |

---

## 8. Decisions and deviations (record in `docs/design.md`)

Numbering continues from D27. "Pre-v1" means reversible before the wire format is frozen.

### 8.1 Decisions

D28 to D48 are in `docs/design.md` from the previous session. D33 and D47 are **superseded by D52**.

D49 to D58 were made when the pipelines landed and started measuring themselves:

| ID | Decision | Reason | Reversible |
|---|---|---|---|
| D49 | The colour conversion runs on the CPU: BGRX to NV12 on the host, NV12 to BGRX on the client. | The plan puts colour conversion on the GPU. No GPU path exists yet. | Yes. |
| D50 | The media peer address is not negotiated. The host sends to the control peer's IP on a fixed client media port. | Adding a handshake field would change the wire format for a case the LAN-only threat model does not need. | Yes. |
| D51 | The host pipeline is four stages on four threads with depth-one queues. | The plan asks for one thread per stage and queues of depth 1 or 2. | Yes. |
| D52 | Pairing is enforced. The client proves it owns its Ed25519 identity key by signing a host nonce. | The host used to accept any client that completed the Noise handshake. A claimed public key proves nothing. | No. |
| D53 | The pairing code is derived from the host's public key and the client's Noise static key, and reduced modulo one million. | The code must be valid for one handshake only. The reduction was a bug: the derivation bypassed the six digit clamp, so the two ends disagreed. | Yes. |
| D54 | The control channel has a timeout-aware receive that keeps a partial record. | A stage that has to send while another waits cannot do so on a channel whose receive blocks forever. | Yes. |
| D55 | `lumen-bench` has `report` and `rig-selftest` subcommands. | The plan's M1 acceptance needs a merged run report. | Yes. |
| D56 | The pipeline measures itself, and the client reports its stage timestamps to the host. | Without it the report holds counters only and no pipeline latency can be read. | Yes. |
| D57 | The clock offset is estimated the way NTP does it. | The two ends have unrelated monotonic clocks. | Yes. |
| D58 | `lumen-bench report` takes `--assert`, `--input` and `--provisional`, and exits non-zero when an assertion fails. | The plan's M1 acceptance is a command with assertions in it. | Yes. |

D59 to D63 were made when the transport became adaptive:

| ID | Decision | Reason | Reversible |
|---|---|---|---|
| D59 | The parity level adapts per frame, and the block size follows it. The block size travels in the header's `flags` word. | The parity ratio depends on the measured loss, and the block size is what makes the overhead cap reachable. | Yes. |
| D60 | Three parity slices per block recover three losses, through a Vandermonde system over GF(256). | Two parity slices cannot recover three losses, so the third must add a real equation. | Yes. |
| D61 | The transport cipher is negotiated inside the Noise handshake, with the capability bitmap as the handshake payload. | The control channel is encrypted from its first byte, so the cipher must be settled before the first control record. | Yes. |
| D62 | The bandwidth policy drives the encoder's constant bitrate, and the encoder can change its rate control while it runs. | The plan's M2.4 needs the estimated bitrate to reach the encoder. | Yes. |
| D63 | The NACK deadline is the capture timestamp plus 1.5 frame intervals, plus a slack. | The plan's M2.2: a late frame is dropped rather than waited for. | Yes. |

D70 to D75 were made when the control plane and the quality ladder were built. D70 adds a crate and D72 adds a message to the wire; the rest change no wire format.

| ID | Decision | Reason | Reversible |
|---|---|---|---|
| D70 | The control plane is a dependency-free HTTP server in a new crate, `lumen-web`, embedded in the host and reached with `--web-listen`. It binds to a loopback address, every change has to carry a token the host printed, and a request that names another origin is refused. | The plan has no web interface. A framework would bring a large dependency tree into a project whose policy is permissive licences only. | Yes. |
| D71 | The host's log is kept in a bounded ring buffer, and the dashboard asks for it on its own rather than having it in the event stream. | The log changes rarely and the stream is sent twice a second. | Yes. |
| D72 | The picture the host serves is a setting. A client that cannot keep up asks for a smaller one, the operator can set one, and the host never serves a picture bigger than the one it captures. | A weak client that is given a picture it cannot reach drops frames, and a stream that drops frames is worse than a smaller stream that does not. | Yes. |
| D73 | A smaller picture is produced inside the colour conversion: the conversion walks the source with an accumulator instead of dividing per pixel, and the chroma averages a fixed two by two source block. | A division per pixel costs more than the whole conversion, and averaging the whole area a chroma cell covers reads every source pixel, so a downscale would cost the same as a big picture. | Yes. |
| D74 | The stage recorder keeps a bounded window of the last frames. | The recorder held every frame of the session, so the memory grew for as long as the host ran and every percentile calculation cost the length of the whole session. | Yes. |
| D75 | The client's quality ladder remembers the picture it failed at, and the operator's choice is the ceiling the host enforces. | Without the memory the client climbs back to a picture it has already shown it cannot reach. The block expires, so a picture that failed once is tried again later. | Yes. |

D64 to D68 were made when the audio and the clipboard were wired into the sessions:

| ID | Decision | Reason | Reversible |
|---|---|---|---|
| D64 | The audio travels on a UDP port of its own with a key pair of its own, and the audio header carries the AEAD counter in the clear. | The plan wants the audio and the video on separate sockets. A separate key is needed too: otherwise the two senders share a key and a counter, which is a nonce reuse. | Yes. |
| D65 | The client aligns an audio frame to the video frame of the same moment, using a history of the frames it has presented. | The audio is captured at the host now, while the video the client has is up to one pipeline latency old. Aligning to the newest frame would make the audio lead the picture by that latency. | Yes. |
| D66 | The audio stage never sleeps. A frame that is not due yet waits in a queue. | A sleeping stage stops reading the socket and stops telling the scheduler which frame is on screen, so the alignment collapses. | Yes. |
| D67 | The host reads the clipboard of its own compositor and offers it; the client takes it and sends the text back, and that return is the round trip. The host can be told to own a given text. | The plan counts clipboard round trips, so one side has to send and the other has to answer. The test hook makes the path measurable in an automated run. | Yes. |
| D68 | The measurement scripts start their compositors in the user's own runtime directory and tell them apart by the Wayland socket each one picks; the readiness check reads the socket table instead of connecting. | The PipeWire socket lives in the user's runtime directory. Connecting to the control port to test it leaves a session thread waiting for a handshake that never comes. | Yes. |

### 8.2 Deviations from the plan (must appear in the final report)

- FEC is GF(256), not XOR (D35). It now recovers three losses per block (D60).
- Decoder output is CPU by default (D37, O-7). Plan forbids CPU readback in the hot path.
- Renderer cannot import NV12 or tiled DMA-BUF (D46).
- Colour conversion runs on the CPU on both ends (D49).
- PipeWire capture not implemented. wlr capture used (D48).
- NVENC, Vulkan encode and Vulkan decode not implemented (stubs).
- HEVC and AV1 not implemented. H.264 only.
- The input replay window is an addition (D40).
- Opus dynamically links the system libopus (D42).
- No glass-to-glass measurement (no rig).
- The media peer address is derived rather than negotiated (D50), so one client media port at a time.
- The heavy parity level uses a block of twelve data slices rather than eight, so three parity slices stay inside the 25 % overhead cap. The plan's "three per eight" and its 25 % cap cannot both hold.
- Three unmaintained `zvariant` advisories are suppressed with a written reason. They are not vulnerabilities and not copyleft. The justification is in `docs/design.md`.
- The soak and chaos runs are short, not the plan's 7200 s and 900 s.
- The A/V skew is a definition Lumen chose, because the plan asks for the number but not for the measurement (O-31).
- The adaptive parity trades the first losses for bandwidth, and the loss sweep is no longer flat (O-33).

---

## 9. Process rules and gotchas (learned the hard way)

1. **File tools resolve relative paths against `/home/hayden/Desktop/projects`, not the workspace.** Always use absolute paths.
2. **Hashline tags are per-file and change on every edit.** Copy the tag from the latest read.
3. **Edit boundaries can be auto-repaired.** Re-read the region after every multi-line edit. A silent no-op edit is the failure mode to watch for: check that the replacement actually applied.
4. **Shared `target/` across agents.** A sibling's mid-edit change can break your build with an error in a crate you did not touch. Never run `cargo clean` on the shared target.
5. **Clippy is `-D warnings` with the workspace lint set.** Expect `needless_pass_by_value` (put the thread arguments in a struct), `single_match_else`, `collapsible_if`, `manual_is_multiple_of`, `chunks_exact_to_as_chunks`, `format_collect`, `map_unwrap_or`, `needless_continue` and `unreadable_literal`.
6. **No placeholder tests.** Every test must be able to fail.
7. **Display-dependent tests must skip with a printed reason**, not pass silently.
8. **Use `-displayfd` for Xvfb.** Fixed display numbers collide.
9. **`ip -n NS tc` is invalid.** Use `ip netns exec NS tc`.
10. **Only one netns pair at a time.** Two measurement scripts will fight over the namespace names.
11. **Fuzz uses nightly**, from the `fuzz/` directory, which is excluded from the workspace.
12. **Never `pkill -f` a pattern that appears in your own command line.** It kills the shell. Use `pkill -x` for an exact name; the scripts now do.
13. **Watch the disk.** Two build trees plus the shared `target/` filled the 234 GB disk once during this session.
14. **A fixed sleep is not a readiness check.** The e2e script waits for the control port to appear in the socket table, because under load the host needs more than a second to bind, and a connecting probe leaves a session thread waiting for a handshake that never comes.
15. **Do not trust host load numbers.** Measure on a quiet machine and record the load.
16. **Do not put an exact frame count in prose.** The counts of a run depend on the load. Put them in the artefact and describe the run qualitatively.
17. **A blocking call in the control loop poisons every latency number.** The clock offset is estimated from the ping exchange, so a 20 ms block in the control stage put 10 ms into every "send" stage. When a stage time is implausible, look for a block in the control channel before looking at the network.

---

## 10. Remaining work, in execution order

### P0. The Tier B drops

1. **Zero-copy decode and present (O-7, O-30).** An EGL NV12 import with a YUV to RGB shader in `lumen-render`, plus a decode DMA-BUF default. The CPU colour conversion is 2.52 ms on the host and 3.57 ms on the client at 1280x720, so this is the largest single win left, and it is what the Tier B acceptance needs.

### P1. The control plane, still open

2. **The control plane cannot start a host.** It changes the running session; there is no way to start one, stop one or pick a different host from the dashboard.
3. **The quality ladder is not wired into the tier B script.** The proxy still drops 2992 frames because nothing tells the client to ask for a smaller picture in that run.
4. **The dashboard rewrites the whole log when it changes.** On a slow browser that is the most expensive thing the page does. Only the changed lines should be written.

### P2. Transport, still open

5. **The FEC recovery counter (O-26).** Do not retry a recovery while a frame's slices are still arriving; attempt it once the frame's last slice has arrived or the frame has been outstanding for a while. This makes the counter mean what it says.
6. **The adaptive parity cold start (O-33).** Start the session at the light level and keep it there until the link has been lossless for a while, rather than dropping to none. The loss sweep's decode errors are the price of the current policy.
7. **The `NoiseTransport` review (O-3).** Bind the handshake hash into the key derivation and record the decision.
8. **QUIC candidate (M2.6)** behind a feature, only if a decision needs data. The default is custom UDP.

### P3. Input, still open

9. **The input path is not wired into the host service.** `crates/lumen-transport/src/input_channel.rs` has the isolated socket, and `lumen-input-wayland` and `lumen-input-inject` have the capture and the injection, but the host service has no `input.rs` that ties them together. The measured latency is the injection leg only.

### P4. The plan's remaining code

10. **HDR and 10-bit (M4.8).** P010 support and the HDR metadata transport. The `dynamic_range` capability bit needs adding to `crates/lumen-protocol/src/capability.rs`.
11. **HEVC and AV1.** The plan's codec ladder. Record the D-CODEC decision.
12. **PipeWire capture (M1.7).** `lumen-capture-pipewire` is still a stub. The plan's risk table allows the wlr fallback, so this is a decision to record as much as a piece of code. It is the precondition for the portal restore token (M6.3).
13. **NVENC (M1.10), Vulkan encode (M1.11) and Vulkan decode (M1.13).** The hardware for NVENC is present. Decide with the D-ENC-VENDOR experiment before writing them, and mark them pending with the reason if they cannot be measured here.
14. **`lumenctl`.** The plan's name for a control tool. `lumen-host` and `lumen-client` cover the same ground; either add it or record the deviation.

### P5. Baselines and the rig

15. **The baselines (M8.5).** Install or build Sunshine plus Moonlight-Qt, RustDesk and GRD with `xfreerdp`, then run the three drivers. The scripts are written and report "not available" honestly.
16. **The long soak (M8.3).** The run budget allows about two hours. The scripts are written; the short runs passed.
17. **The physical rig (M8.6).** The procedure is written. It needs the hardware.
18. **The compositor restart (O-29a).** It survives in the M6 run and not in the soak, and they are not measuring the same thing. `scripts/run-compositor-restart.sh` kills only the compositor, leaves the host running, and the host reopens its capture: 5 of 5 recovered with no file-descriptor growth. The soak's version restarts the compositor and the new one does not always take the same Wayland socket name, so the host cannot reconnect. The soak's liveness check also latched (it grepped the whole host log), which made every later event look like a failure; it now only looks at the lines logged since the last event.

---

## 11. Verification commands (copy and run)

```
export CARGO_HOME=/home/hayden/.cargo RUSTUP_HOME=/home/hayden/.rustup PATH=/home/hayden/.cargo/bin:$PATH
cd /home/hayden/Desktop/projects/lumen

# The gates
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check && cargo audit
cargo test --workspace

# M0
scripts/probe-caps.sh && scripts/netns-up.sh
test -f artifacts/m0/capabilities.json && test -f artifacts/m0/baselines.json

# M1
MODE=loopback DURATION=30 OUT=artifacts/m1/e2e.json ./scripts/run-e2e.sh
MODE=netns   DURATION=30 OUT=artifacts/m1/e2e-netns.json ./scripts/run-e2e.sh
./target/release/lumen-bench report --input artifacts/m1/e2e.json \
    --assert 'decode_errors==0' --assert 'p50_present_ms<=8' --assert 'p99_end_to_end_ms<=40'

# M2
./scripts/sweep-loss.sh --levels 0.005,0.02,0.05 --jitter 10ms --out artifacts/m2/curve.csv
test -f artifacts/m2/curve.csv && grep -q "0.005" artifacts/m2/curve.csv

# M3
./scripts/run-input-latency.sh --duration 60 --out artifacts/m3/input.json

# M4
MODE=loopback DURATION=30 WITH_AUDIO=1 WITH_CLIPBOARD=1 WITH_RESIZE=1 \
  OUT=artifacts/m4/e2e.json ./scripts/run-e2e.sh

# M5
./scripts/run-tier-b.sh --cpus 0,1 --duration 300 --report artifacts/m5/tierb.json

# M6
./scripts/run-reconnect.sh --cycles 20 --out artifacts/m6/reconnect.json
./scripts/run-compositor-restart.sh --cycles 5 --out artifacts/m6/restart.json

# M7
./scripts/run-e2e-x11.sh --duration 60 --report artifacts/m7/e2e-x11.json

# M8
cd fuzz && for t in protocol_parser fec_reassemble transport_inject h264_au_split input_parser; do
  cargo +nightly fuzz run "$t" -- -max_total_time=60 || exit 1
done; cd ..
./scripts/soak.sh --duration 7200 --out artifacts/m8/soak.json
./scripts/chaos.sh --duration 900 --out artifacts/m8/chaos.json
./target/release/lumen-bench rig-selftest --out artifacts/m1/rig-selftest.json
```

---

## 12. Risks and unknowns

- **Measurement noise.** Host load from other builds has shown up as scheduling lateness of up to 4.5 ms in the client. Measure on a quiet machine and record `uptime`.
- **Glass-to-glass cannot be measured here.** Every target that needs the rig stays **pending**. Do not write a number for them.
- **The pipeline latency excludes the display.** The present stage ends when the frame is handed to the compositor.
- **The two clocks are aligned by an NTP-style estimate** that assumes the two directions take the same time. The error is well under a millisecond on a LAN, but the whole cross-machine measurement rests on it, and a blocking call in the control channel moves it.
- **The FEC recovery counter overstates the loss.** Read the true loss as what the client reports over the link feedback.
- **The adaptive parity has a cold start.** The first losses on a bad link are not recovered (O-33).
- **The Tier B proxy does not pass.** It drops frames because the colour conversion runs on the CPU.
- **One decode error in the soak and in the chaos run.** Not diagnosed. It is probably the compositor restart.
- **The input path is measured on the injection leg only.** The client capture and the transport are not in the loop yet.
- **The repository has no history before the first push.** There is nothing to fall back on yet. Keep backups of anything you are about to rewrite.
- **The disk filled up once.** Watch it.

---

## 13. Quick file index

| Need | Path |
|---|---|
| Approved plan | `docs/plan.md` |
| This handoff | `docs/handoff.md` |
| Design decisions (D1 to D68) | `docs/design.md` |
| Wire protocol | `docs/protocol.md` |
| Benchmarks and provisional results | `docs/benchmarks.md` |
| Known limits and blockers | `docs/known-limits.md` |
| The final report | `docs/final-report.md` |
| The photodiode harness procedure | `harness/README.md` |
| Host pipeline | `crates/lumen-host-service/src/pipeline.rs` |
| Host binary | `crates/lumen-host-service/src/main.rs` |
| Session, pairing exchange, cipher negotiation | `crates/lumen-host-service/src/session.rs` |
| Control channel | `crates/lumen-host-service/src/control.rs` |
| Pairing store and identity files | `crates/lumen-host-service/src/pairing.rs` |
| Per-stage timing and the clock offset | `crates/lumen-host-service/src/stages.rs` |
| The host's audio stage | `crates/lumen-host-service/src/audio.rs` |
| The host's clipboard | `crates/lumen-host-service/src/clipboard.rs` |
| Client pipeline | `crates/lumen-client-desktop/src/lib.rs` |
| Client receive stage and link accounting | `crates/lumen-client-desktop/src/receive.rs` |
| Client decode and present | `crates/lumen-client-desktop/src/present.rs` |
| The client's audio stage | `crates/lumen-client-desktop/src/audio.rs` |
| Media packets, FEC, retransmit, NACK, receiver | `crates/lumen-transport/src/media.rs`, `slice.rs`, `fec.rs`, `retransmit.rs`, `nack.rs`, `pacing.rs`, `estimator.rs` |
| The input channel | `crates/lumen-transport/src/input_channel.rs` |
| Media header and capabilities | `crates/lumen-protocol/src/header.rs`, `capability.rs`, `media.rs` |
| The audio wire format | `crates/lumen-protocol/src/audio.rs` |
| Colour conversion | `crates/lumen-protocol/src/color.rs` |
| Keys, handshake, pairing primitives | `crates/lumen-crypto/src/keys.rs`, `handshake.rs`, `aead.rs`, `pairing.rs` |
| Wlr capture and DMA-BUF readback | `crates/lumen-capture-wlr/src/lib.rs`, `readback.rs` |
| X11 capture and clipboard | `crates/lumen-capture-x11/src/lib.rs`, `dri3.rs`, `clipboard.rs` |
| X11 input capture | `crates/lumen-input-x11/src/` |
| Audio capture, session and playback | `crates/lumen-audio/src/` |
| VA-API encode | `crates/lumen-encode-vaapi/src/`, `tests/dmabuf_import.rs` |
| VA-API decode | `crates/lumen-decode-vaapi/src/`, `tests/roundtrip.rs` |
| Renderer | `crates/lumen-render/src/` |
| Bench library and binary | `crates/lumen-bench/src/`, `crates/lumen-bench/src/bin/lumen-bench.rs` |
| Fuzz targets | `fuzz/fuzz_targets/`, artifacts in `fuzz/artifacts/` |
| The end-to-end script | `scripts/run-e2e.sh` |
| The loss sweep | `scripts/sweep-loss.sh` |
| The input latency script | `scripts/run-input-latency.sh` |
| The tier B proxy | `scripts/run-tier-b.sh` |
| The soak and the chaos | `scripts/soak.sh`, `scripts/chaos.sh` |
| Host and netns scripts | `scripts/netns-up.sh`, `scripts/netns-down.sh` |
| End-to-end artefacts | `artifacts/m1/` |
| The loss curve | `artifacts/m2/curve.csv` |
| The input latency | `artifacts/m3/input.json` |
| The audio, clipboard and resize run | `artifacts/m4/e2e.json` |
| The tier B proxy | `artifacts/m5/tierb.json` |
| Reconnect, restart and flap | `artifacts/m6/` |
| The X11 run | `artifacts/m7/` |
| The soak and the chaos | `artifacts/m8/` |
| The baseline availability record | `artifacts/m0/baselines.json` |
