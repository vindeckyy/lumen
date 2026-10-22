# Lumen handoff

**Date:** 2026-10-21, 21:35 local.
**Repository:** `https://github.com/vindeckyy/lumen` (public, branch `main`, one commit so far).
**For:** the next lead agent or subagent who has to continue without this conversation.

This is the **fourth** handoff. The first described a workspace with transport, crypto and session layers but no pipeline. The second described a pipeline that measured its own latency. The third added the adaptive transport, the audio and the clipboard, and the control plane. This one adds the quality ladder, the removal of the per-frame copies, the measurement examples and the public repository.

**Read section 0 first, then section 12 (the remaining work).** Everything in between is the state you are taking over.

---

## 0. Start here

- **Where we are.** Every milestone's acceptance block has been run at least once and has an artefact. The M4 acceptance is green. The transport is adaptive, the audio and the clipboard are in real sessions, the control plane works, and the picture adapts to a client that cannot keep up.
- **The gates are green.** `cargo fmt --all --check` clean · `cargo clippy --workspace --all-targets -- -D warnings` exits 0 · `cargo test --workspace` exits 0 with **563 tests** · `cargo deny check` exits 0 · `cargo audit` exits 0 (four allowed warnings).
- **Nothing is running.** No background job, no service, no namespace. The measurement scripts clean up after themselves.
- **The working tree is clean.** The second commit holds the changes listed in section 11: the removal of the per-frame copies, the measurement examples, the CI fix and this handoff.
- **The artefacts are stale relative to the code.** The last regeneration was at 20:25, before the per-frame copies were removed. The numbers moved: the host is roughly 40 % faster at 1080p. **Regenerate every artefact** (section 12) before quoting any number.
- **The one thing that is not done is glass-to-glass.** It needs a photodiode, a camera and a second machine. None of them is here. Every latency number in this repository is a *software pipeline latency* and is labelled as one.

---

## 1. Stop state at handoff

- **Lead:** active until the end of this session.
- **Subagents:** all finished and reported. None is running.
- **Background jobs:** none.
- **Pending edits:** the working tree has seven modified files and three untracked paths (section 11). Nothing is half applied; the changes compile and the whole suite passes with them.
- **Destructive actions:** none. The repository was created and one commit pushed.
- **Disk:** the workspace `target/` is about 13 GB, a second build tree was 5 GB and was deleted. The disk filled once during the session. One build tree at a time.

---

## 2. Environment

### 2.1 Hardware and drivers (verified on this machine)

| Item | Value |
|---|---|
| OS / kernel | Ubuntu 26.10 "Stonking Stingray", Linux 7.3.0-9-generic x86_64 |
| CPU | AMD Ryzen 5 4600H with Radeon Graphics, 12 vCPU |
| iGPU | AMD Radeon (PCI 0000:05:00.0), `amdgpu`, **`/dev/dri/renderD129`** and `card2`. VA-API H.264 and HEVC encode and decode through `radeonsi_drv_video.so`, libva 1.24, Mesa 26.2.4 |
| dGPU | NVIDIA GTX 1650 (PCI 0000:01:00.0), driver 580.178.04, **`/dev/dri/renderD128`** and `card1`. `libnvidia-encode.so.1` and `libnvcuvid.so.1` present. VA-API via NVDEC only (`nvidia-vaapi-driver`). **NVENC code is not written** |
| Software Vulkan | lavapipe (`libvulkan_lvp.so`). `vulkaninfo` works |
| Display | Physical eDP-1 panel at 1920x1080. No compositor session on it. Not used by any test |
| Audio | PipeWire 1.6.8 daemon running, socket at `/run/user/1000/pipewire-0` |
| Webcam | `/dev/video0`, `/dev/video1` (USB, HD Webcam) |
| Users | `hayden` has rw ACL on `renderD128` and `renderD129`. `sudo -n` works without a password |

`renderD129` is AMD and is the default VA device. `renderD128` is NVIDIA. `lumen-ffi-va` probes `renderD129` first, then `renderD128`. `LUMEN_VA_DEVICE` overrides it.

### 2.2 Toolchain

- Stable `1.99.1` at `/home/hayden/.cargo/bin` and `/home/hayden/.rustup`. `rust-toolchain.toml` pins `channel = "1.99.1"` with `rustfmt` and `clippy`.
- Nightly `1.101.0` for fuzzing. `cargo-fuzz 0.13.2` installed. `rust-toolchain-nightly.toml` exists.
- **`cargo-deny` 0.20.2 and `cargo-audit` 0.22.2 are installed and both pass.**

Every bash call starts a fresh shell. Repeat this export in each call:

```
export CARGO_HOME=/home/hayden/.cargo RUSTUP_HOME=/home/hayden/.rustup PATH=/home/hayden/.cargo/bin:$PATH
cd /home/hayden/Desktop/projects/lumen
```

### 2.3 Network namespaces

- `scripts/netns-up.sh` creates `lumen-host` (10.77.0.1/24) and `lumen-client` (10.77.0.2/24), joined by a veth pair. `scripts/netns-down.sh` tears it down.
- Measured RTT: 0.045 to 0.055 ms plain, 2.0 to 2.7 ms with `netem delay 1ms` on both ends.
- Netem works under `sudo ip netns exec`. **Do not** write `ip -n NS tc`; that form is invalid.
- **Only one netns pair can exist at a time.** Two measurement scripts will fight over the namespace names. Never run two at once.

### 2.4 Display servers

- **Headless sway:** `WLR_BACKENDS=headless WLR_RENDERER=pixman sway -c /dev/null` with a private `XDG_RUNTIME_DIR`. The output is `HEADLESS-1` at **1280x720**, refresh reported as 0 mHz.
- **The measurement scripts start their compositors in the user's own `XDG_RUNTIME_DIR`** and tell them apart by the Wayland socket each one picks. The PipeWire socket lives there, and an audio session cannot reach it from a private one. The scripts also capture each compositor's sway IPC socket at start-up, because a mode change has to reach the right one.
- **Xvfb:** `Xvfb :90 -screen 0 1920x1080x24 -nolisten tcp`. The X11 route is how a real 1920x1080 source is made; the headless output has no 1080p mode.
- **Xwayland** is available. `LUMEN_X11_TEST_BACKEND=dri3|shm|get` forces a capture rung.
- **Xvfb has no DRI3**, so the renderer's X11 DRI3 import returns `Err`.
- Display-dependent tests skip with a printed reason when the variable is absent. Run them with `env -u WAYLAND_DISPLAY -u DISPLAY cargo test ...`.

### 2.5 Missing system packages

`xcb-xfixes`, `xcb-dri3`, `xinput` and `libvulkan` pkg-config entries are missing. The crates that need them use `x11rb` directly or were run with feature flags.

**`libxkbcommon-x11-dev` is now needed** by the `xkbcommon` crate's `x11` feature, which `lumen-input-x11` uses. The CI workflow installs it as of this session (section 11).

### 2.6 Not present

- No photodiode rig, no camera rig, no second machine, no Celeron N4020, no 4K or 144 Hz panel.
- Sunshine, Moonlight-Qt, RustDesk and GRD are **not installed**, so no baseline was measured. `scripts/baseline-check.sh` writes the honest "not available" record to `artifacts/m0/baselines.json`; the three baseline drivers say what is missing and exit non-zero.

---

## 3. Repository layout

Root: `/home/hayden/Desktop/projects/lumen`. It **is** a git repository; the public upstream is `github.com/vindeckyy/lumen`.

- `Cargo.toml` — workspace, 23 members, `exclude = ["vendor/opus", "fuzz"]`
- `Cargo.lock`, `deny.toml`, `rust-toolchain.toml`, `rust-toolchain-nightly.toml`
- `crates/` — 23 crate directories
- `docs/` — `design.md`, `protocol.md`, `benchmarks.md`, `known-limits.md`, `plan.md`, `handoff.md` (this file), `final-report.md`, `notes/` (the four subagent working notes, `A-NOTES.md` … `D-NOTES.md`)
- `docs/screenshots/` — `dashboard.png`, the control plane (this session)
- `fuzz/` — `Cargo.toml`, `Cargo.lock`, `fuzz_targets/` (12 files), `corpus/`, `artifacts/`
- `scripts/` — 20 scripts (section 4.5)
- `harness/` — `README.md` (the physical rig procedure), `photodiode/` (the wiring, `capture.sh`, `detect-trigger.py`, `selftest.sh`), `fixtures/simulated-trigger.json`
- `vendor/opus/` — a patched copy of the `opus` crate 0.4.0 (MIT OR Apache-2.0, Tad Hardesty). `src/` is byte-identical to upstream; the patch only turns off the bundled cmake build so the system libopus is linked.
- `artifacts/` — `m0` … `m8`
- `README.md`, `LICENSE-MIT`, `LICENSE-APACHE`, `.github/workflows/ci.yml`, `.github/workflows/fuzz.yml`

Stub crates (a 2-line `lib.rs` with `#![forbid(unsafe_code)]` and a doc comment, no code):
`lumen-capture-pipewire`, `lumen-decode-vulkan`, `lumen-encode-nvenc`, `lumen-encode-vulkan`, `lumen-ffi-nvenc`.

**Never add a placeholder test such as `assert!(true)`.**

---

## 4. The code, crate by crate

### 4.1 `lumen-protocol` (69 unit tests)

- `header.rs` — `MediaHeader`, `HEADER_LEN = 42`. The `flags` word carries the number of data slices per FEC block (D59). `seq` is the AEAD counter, in the clear as AAD.
- `capability.rs` — the capability bitmap. `AEAD_CHACHA20` is bit 19 (D61).
- `media.rs` — the video frame types, `FrameSource`, `FrameSink`.
- `color.rs` — **the BGRX to NV12 conversion, and the scaled variant.** The scaled variant walks the source with an accumulator instead of dividing per pixel, and the chroma averages a fixed 2x2 source block, so a smaller picture costs *less* work (D73). A block whose slices have different lengths is refused by the FEC, not rebuilt to the wrong length.
- `audio.rs` — the 48-byte `AudioHeader`, `AudioKind`, the frame duration helpers. The header carries the AEAD counter in the clear, because the receiver needs it to open the payload.
- `input.rs`, `message.rs`, `raw.rs`, `slice.rs` — as before.
- `examples/colorbench.rs` (**new**) — times the CPU conversion at each size and prints the frame rate it alone allows.

### 4.2 `lumen-crypto` (20 unit tests)

- `aead.rs` — `AeadKind`, `PacketCipher`, the 8-byte counter plus 4-byte session id nonce. **New: `seal_with`**, which takes an explicit counter, so the audio can carry its counter in the clear.
- `handshake.rs` — **the handshake carries a payload.** `write_message_with` and `read_message_with` are new, and the capability bitmap travels in the initiator's first message and the responder's second (D61). The payload is part of the transcript, so a peer that changes it fails the final hash check. `NoiseTransport` is **deleted** (O-4).
- `keys.rs` — `SessionKeys` with the four per-direction keys, **plus `audio_h2c` and `audio_c2h`** (D64).
- `pairing.rs` — `DeviceIdentity`, `PairingCode`, the signing primitives, `pairing_proof`. The code reduces modulo one million, so it is always six digits and both ends agree.

### 4.3 `lumen-transport` (65 unit tests, 9 property tests)

- `fec.rs` — Reed-Solomon over GF(2^16) from `reed-solomon-simd` (D82). **Each block recovers as many losses as it has parity slices**; `parity_for_block` gives the parity count from the block size. `ParityLevel::block_data_slices` gives eight for the light level and twelve for the heavy one, so the heavy level keeps the overhead at 25 %.
- `slice.rs` — the assembler. The parity arm creates the frame state (G2). At most 64 frames are held (G3). The block size comes from the header's `flags` word, and a missing entry is zero-filled so the rebuild is exact.
- `media.rs` — `MediaSender` with a per-frame parity level (`set_parity`). **The access unit is sliced into one padded buffer, not a vector of slices** (section 11), and the data slices are sealed straight out of it.
- `nack.rs` (**new**) — the NACK policy. The deadline is the capture timestamp plus 1.5 frame intervals, with a slack; a frame past it is dropped and a keyframe is asked for (D63).
- `retransmit.rs` — `RetransmitQueue` and `MissingSlice`. The deadline is enforced in the host's stage recorder, not here.
- `estimator.rs` — the loss and delay-gradient estimator. `set_initial_rate` starts the policy from the configured bitrate.
- `input_channel.rs` (**new**) — the input path on a socket of its own, sealed with the client-to-host media key.
- `socket.rs`, `pacing.rs`, `inject.rs` — as before.
- `tests/properties.rs` (**new**) — nine property tests for the framing and the FEC. One of them found the mixed-length block bug.

### 4.4 `lumen-host-service` (82 unit tests)

- `pipeline.rs` — the host pipeline: four stages on four threads with depth-one queues, plus the multi-session support. **New this session:** the estimator is fed by the client's link feedback, the parity level follows the measured loss with hysteresis, the bitrate follows the estimator, a retransmission is refused past the frame's deadline, the encoder is rebuilt when the output changes mode, and **the capture converts straight out of the captured frame instead of copying it first** (section 11).
- `session.rs` — the transport cipher is negotiated inside the Noise handshake (D61). `Established` carries the cipher and the peer's capabilities. `has_aes_ni` does the detection.
- `control.rs` — the control channel with a timeout-aware receive that keeps a partial record (D54). Message kinds: `PairingChallenge` (11), `PairingResponse` (12), `StageTimes` (13), **`LinkFeedback` (14)**, **`Quality` (15)**.
- `pairing.rs` — the trust store, the identity file, the pairing code, and **`untrust`**, so the control plane can revoke a client.
- `stages.rs` — the per-stage timing, the clock offset estimate, `may_retransmit` (D63), and **`session()`**, which gives the frames and the span of the session, so the dashboard can report the rate the pipeline *delivered* and not the rate that was asked for (D74).
- `audio.rs` (**new**) — the host's audio stage: a PipeWire virtual sink for the session, Opus encoding, and the header that carries the capture time of the audio.
- `web.rs` (**new**) — the host's implementation of the control plane's API over the live state.
- `log.rs` (**new**) — the host's log in a bounded ring, so the dashboard can show it (D71). The `host_log!` macro writes to both the ring and the standard error.
- `settings.rs` (**new**) — the settings the control plane changes: the quantiser, the bitrate, the frame rate, the parity mode, the audio, the picture and its ceiling.
- `clipboard.rs`, `monitors.rs`, `resize.rs`, `persist.rs`, `multi.rs`, `virtual_display.rs`, `backend.rs` — M4 to M7.
- `main.rs` — the `lumen-host` binary. **New: `--web-listen <addr>`** (and `--web-listen none` turns it off), `--with-audio`, `--audio`, `--client-audio-port`, `--clipboard-test`.

### 4.5 `lumen-client-desktop`

- `lib.rs` — the client pipeline. **New:** the link accounting that tells the host what the link did, the client pings the host so the round trip can be measured, the client asks for ChaCha20-Poly1305 when it has no AES-NI, and the client echoes the clipboard so the host can count the round trip. The control stage owns the quality ladder and sends the `Quality` request.
- `receive.rs` — the receive stage, the `LinkAccounting`, and the NACK the client sends.
- `present.rs` — the decode and present stage. It records the drops for the ladder and publishes the video clock for the audio.
- `audio.rs` (**new**) — the client's audio stage: the reassembly, the decode, the scheduling against the video clock, and the timed queue. **The stage never sleeps**: a frame that is not due yet waits in the queue, because a sleeping stage stops reading the socket and stops telling the scheduler which frame is on screen (D66).
- `ladder.rs` (**new**) — the quality ladder. It walks down when the client drops frames, remembers the picture it failed at, and never asks for a picture bigger than the one the host is serving (D75).
- `clipboard.rs`, `monitors.rs`, `reconnect.rs`, `lowpower.rs`, `backend.rs`, `events.rs` — M4 to M7.

### 4.6 `lumen-web` (**new**, 10 unit tests)

- `http.rs` — a small HTTP/1.1 server: the request line, the headers, the body, keep-alive and server-sent events. No framework, no dependency beyond `serde`.
- `api.rs` — the routes (`/`, `/api/state`, `/api/log`, `/api/stream`, `/api/command`) and the rule that a change must carry the host's token and may not come from another origin.
- `ui.rs` and `assets/index.html` — the dashboard. One file, with the styles and the script inside, so the browser needs nothing from the network but the host.

### 4.7 The rest

- `lumen-capture-wlr` — the wlroots DMA-BUF capture and the CPU readback.
- `lumen-capture-x11` — the X11 ladder, and `examples/capturecost.rs` (**new**) which times the capture on its own and reports the frame rate and the bandwidth it moves.
- `lumen-input-x11` — XI2 raw motion, key press and key release, the keymap through xkbcommon. 24 unit tests.
- `lumen-audio` — the PipeWire sink and capture, Opus, the A/V sync, the playback, and the scheduler with its history of presented frames.
- `lumen-encode-vaapi` — the H.264 encoder, and **`set_rate_control`**, so the bandwidth policy can push a new bitrate while it runs.
- `lumen-decode-vaapi` — the decoder, the probe and the backend chain.
- `lumen-render` — the Wayland and X11 present paths.
- `lumen-bench` — the measurement harness: the per-stage records, the report writer, the camera analysis, the input latency recorder.

### 4.8 The scripts

| Script | What it does |
|---|---|
| `env-setup.sh`, `probe-caps.sh` | the system packages and the capability probe |
| `netns-up.sh`, `netns-down.sh` | the namespace pair |
| `run-e2e.sh` | the end-to-end run. Flags: `--with-audio`, `--with-clipboard`, `--with-resize`, `--host-ns`, `--client-ns`, `--width`, `--height`, `--fps`, `--duration`, `--report`, `--out-dir` |
| `run-e2e-x11.sh` | the X11 variant |
| `sweep-loss.sh` | the loss sweep over the namespace pair |
| `netem-run.sh` | the system level netem driver |
| `run-input-latency.sh` | the input injection latency |
| `run-tier-b.sh` | the two-vCPU proxy |
| `run-reconnect.sh`, `run-compositor-restart.sh`, `run-wifi-flap.sh` | the M6 resilience runs |
| `soak.sh`, `chaos.sh` | the M8 runs |
| `baseline-check.sh`, `baseline-{sunshine,rustdesk,grd}.sh` | the M0 baseline probe |
| `x11-env.sh` | the X11 environment |

### 4.9 `fuzz/` and `harness/`

Twelve fuzz targets; the plan's names `protocol_parser`, `fec_reassemble` and `input_parser` exist as aliases, so the plan's acceptance loop works as written. The `harness/` directory holds the physical rig procedure, the v4l2 capture loop, the trigger finder, the self-test and the committed fixture.

---

## 5. Measured results

All of these are **provisional software measurements**. None is glass-to-glass. None comes from the target hardware.

**These numbers are from the artefacts as they stand (20:25), which is BEFORE the per-frame copies were removed. Regenerate them (section 12) before quoting anything.**

| Measurement | Result | Artefact |
|---|---|---|
| End-to-end, loopback, 30 s, 1280x720 at 30 fps | 873 sent, 872 decoded, 871 presented, 0 decode errors, 0 packets lost | `artifacts/m1/e2e.json` |
| Software pipeline latency, loopback | p50 21.37 ms, p90 23.58, p99 26.05, max 30.35 | `artifacts/m1/e2e.json` |
| End-to-end, network namespaces, 30 s | 851 sent, 851 decoded, 850 presented, 0 decode errors | `artifacts/m1/e2e-netns.json` |
| Software pipeline latency, namespaces | p50 22.23 ms, p90 39.49, p99 74.24, max 85.88 | `artifacts/m1/e2e-netns.json` |
| Loss sweep, 0.5 %, 2 %, 5 % with 10 ms jitter | 300, 298 and 287 frames presented of 308, 309 and 308 sent; 2, 4 and 4 decode errors; 28, 47 and 97 FEC recoveries | `artifacts/m2/curve.csv` |
| Input injection latency, 60 s, 1920x1080 | 60000 events, p50 46 µs, p90 85, p99 187, max 2756; 0 failures, 0 replays | `artifacts/m3/input.json` |
| M4: audio, clipboard, two resolution changes, 30 s | 852 presented, 0 decode errors, 5602 audio frames, 0 underruns, 1 clipboard round trip, 2 resolution changes, A/V skew p99 18.43 ms | `artifacts/m4/e2e.json` |
| Two-vCPU proxy, 1080p60, 300 s | 17099 decoded, 14514 presented, **2992 dropped, 240 decode errors**, decode p50 6.83 ms, p99 11.80 ms, CPU 35.5 % of two cores, **pass: false** | `artifacts/m5/tierb.json` |
| Reconnect, 20 cycles | 20/20, the slowest 5 ms | `artifacts/m6/reconnect.json` |
| Compositor restart, 5 cycles | 5/5, no file descriptor growth | `artifacts/m6/restart.json` |
| Network flap, 3 flaps | 3/3, the slowest 1057 ms | `artifacts/m6/wifi-flap.json` |
| X11 end to end, 60 s | 1710 sent, decoded and presented, 0 decode errors | `artifacts/m7/e2e-x11.json` |
| Soak, 307 s | 4691 sent, 4686 presented, 2 decode errors, 5 dropped; 10 events, 2 survived | `artifacts/m8/soak.json` |
| Chaos, 320 s | 10967 sent, 10945 presented, 6 decode errors, 22 dropped; 13 events, 2 survived | `artifacts/m8/chaos.json` |

**The costs, measured directly (this session, `crates/lumen-protocol/examples/colorbench.rs` and `crates/lumen-capture-x11/examples/capturecost.rs`):**

| What | Cost | Alone it allows |
|---|---|---|
| The CPU colour conversion at 1280x720 | 2.97 ms | 336 fps |
| The CPU colour conversion at 1600x900 | 5.06 ms | 198 fps |
| The CPU colour conversion at 1920x1080 | 6.81 ms | 147 fps |
| The X11 capture at 1920x1080 | 110 frames in 10 s, 871 MiB a second | 110 fps |

**So neither the conversion nor the capture is the limit on its own. The limit was the copies around them** (section 11). After removing them the host delivers roughly 60 to 70 fps at 1080p, and the client cannot reach 120 fps either, because its own NV12 to BGRX conversion and its `wl_shm` copy cost the same kind of memory traffic.

**The 120 fps target is not a hardware limit. It is the CPU path** (D49) and the readback (O-7), and that is what the next agent should attack.

---

## 6. Open defects

**Closed:** G2, G3, O-1, O-2, O-4, O-5, O-6, O-8, O-17, O-18, O-19, O-20, O-21, O-22, O-27, and the dependency policy.

Open, ordered by impact:

- **O-7. Decoded surfaces cannot be presented zero-copy.** The decoder's default output is CPU NV12 and the renderer has no NV12 import. This is the largest latency win left and the prerequisite for the Tier B acceptance. The CPU colour conversion is 6.81 ms at 1080p, which alone caps the host at 147 fps.
- **O-30. The two-vCPU proxy drops frames at 1080p60.** The client's colour conversion and its `wl_shm` copy take longer than the frame interval. Same fix as O-7.
- **O-3. `Handshake::finish` uses `dangerously_get_raw_split`.** Sound for this construction, but the handshake hash is not bound into the derivation. Record the decision.
- **O-9. Render-node choice in capture.** `render_node(None)` picks `renderD128`, which is NVIDIA. Verify that the compositor feedback decides.
- **O-10. `send_paced` sleeps on the calling thread.** It is on the send stage's own thread now, so it does not block the encoder.
- **O-11. Implicit sync is not handled.** The read path calls `DMA_BUF_IOCTL_SYNC`, but it has not been reproduced under load.
- **O-12. The `copy_with_damage` refresh trigger is untested.** Headless sway reports 0 mHz.
- **O-13. The input EIS path is untested live.** There is no EIS server. The X11 path is implemented and the injection path is measured.
- **O-14. The input replay window (64 entries) is an addition** beyond the spec. Reversible.
- **O-15. The audio realtime callback uses `std::sync::Mutex`.** Replace if underruns appear.
- **O-16. The vendored `opus` crate** is accepted by `cargo deny`, but the patched build links the system libopus and has not been checked on a machine without it.
- **O-23. VA decoder limits.** CAVLC P slices, several references, multi-slice pictures and POC type 1 are unexercised. The decoder returns explicit errors, so failures are loud.
- **O-24. Pool aliasing in the decoder.** A consumer holding a DMA-BUF frame must release it before the pool wraps. The client uses the CPU path.
- **O-25. The clock offset estimate assumes the two directions take the same time.** On a LAN the error is well under a millisecond, but every latency figure inherits it, and it showed: a 20 ms block in the control channel skewed the offset by 10 ms and put 10 ms into every "send" stage until the block was found and removed.
- **O-26. The `fec_recoveries` counter overstates the loss.** The receiver tries a recovery on every datagram, so a slice that is merely late is rebuilt from the parity and counted. The rebuilt bytes are exact, so this is a counter semantic. The true loss is what the client reports over the link feedback.
- **O-28. The Tier B decode p99 is 11.80 ms, inside the 12 ms budget**, but the run still fails on the drops.
- **O-29. Decode errors in the soak and in the chaos run.** Two and six. The likely cause is the compositor restart.
- **O-29a. The compositor restart survives in the M6 run and not in the soak**, and they are not measuring the same thing. The M6 script kills only the compositor, leaves the host running, and the host reopens its capture: 5 of 5 recovered. The soak's version restarts the compositor and the new one does not always take the same Wayland socket name, so the host cannot reconnect. The soak's liveness check also latched: it grepped the whole host log, so one error at any point marked every later event as a failure. It now only looks at the lines logged since the last event.
- **O-33. The adaptive parity has a cold start.** The level starts without parity, so the first losses on a link that is already bad are not recovered: the loss sweep shows 2, 4 and 4 decode errors where a fixed parity level showed none. The level rises within a feedback interval and falls only after six lossless windows.
- **O-34. The control plane cannot start a host.** It changes the running session; it cannot start one, stop one or point at a different machine.
- **O-35. The quality ladder lowers the picture and never raises it.** A client that drops frames asks for a smaller picture and stays there. It remembers the picture it failed at, so it does not climb back into it, and the block expires after five minutes. The operator can put the picture back from the dashboard.
- **O-36. The dashboard is not a low-power build.** The page redraws the log whenever it changes and the chart is redrawn on every state, and it keeps the page awake. The log is fetched on its own every two seconds and nothing is drawn while the tab is hidden, but the log is still rewritten whole.

**Two known rough edges in the dashboard, from the screenshots of this session.** The tiles show a dash when the session has no completed frames, which is what happens when the client cannot present at all: at 1920x1080 and 120 fps the client drops everything, so the host has no stage timings to show. The rate the tile shows is the rate the pipeline delivered, not the rate that was asked for; the sub-line shows the picture and the rate that was configured.

---

## 7. Documentation

| File | State |
|---|---|
| `docs/plan.md` | the approved plan, byte for byte. It has no web interface and no quality ladder in it: those are additions (section 8) |
| `docs/design.md` | the decisions, D1 to D75, with the reason and whether each is reversible. The status paragraph lists the groups |
| `docs/protocol.md` | the wire format: the 42-byte header and its `flags` word, the Noise frames, the pairing exchange, the clock negotiation, the 48-byte audio header, the control message kinds including `LinkFeedback` and `Quality` |
| `docs/benchmarks.md` | the measurements, against the artefacts. **Stale: regenerate the artefacts and update it** |
| `docs/known-limits.md` | the open defects, the closed ones and the ones that are not defects but are limits |
| `docs/final-report.md` | the M8.7 report. **Stale: it was written before the control plane and the ladder** |
| `docs/handoff.md` | this file |
| `docs/notes/` | the four subagent working notes. Every wiring item in them has been applied; they are kept for the history |
| `docs/screenshots/dashboard.png` | the control plane, 1280x720 at 30 fps, with the live numbers |
| `README.md` | the architecture, the crate table, the scripts, the measurements and the honest list of what is not done |
| `artifacts/m0` … `artifacts/m8` | the measurement record. Every number in the documentation comes from one of these |

---

## 8. The decisions

D1 to D48 are in `docs/design.md` from the earlier sessions. D33 and D47 are superseded by D52. The groups since:

- **D49 to D58** — the pipelines landed and started measuring themselves: the CPU colour conversion, the derived media peer address, the four-stage pipeline, the enforced pairing, the six-digit pairing code, the timeout-aware control receive, the two `lumen-bench` subcommands, the self-measuring pipeline, the NTP-style clock offset, the asserting report.
- **D59 to D63** — the transport became adaptive: the per-frame parity with the block size on the wire, three-loss recovery, the cipher negotiated in the handshake, the bitrate policy reaching the encoder, the NACK deadline.
- **D64 to D68** — the audio and the clipboard were wired into the sessions: the audio's own port and key pair, the alignment to the video frame of the same moment, the queue instead of a sleep, the clipboard round trip, the compositors in the user's runtime directory.
- **D69** — the parity level is asymmetric: it rises at once and falls only after six lossless windows.
- **D70 to D75** — the control plane and the ladder: the dependency-free HTTP server with the token and the origin rule, the log in a ring, the picture as a setting, the downscale inside the conversion, the bounded stage recorder, and the ladder's memory of what failed.

**The additions beyond the plan, which the final report must state:** the control plane (`lumen-web`, `--web-listen`), the quality ladder and the `Quality` message, the two measurement examples, and the bounded stage recorder.

---

## 9. The control plane

```
lumen-host --web-listen 127.0.0.1:8080
```

The host prints the URL with a token in the fragment. The dashboard shows the frame rate, the bitrate, the pipeline latency with a live chart, the breakdown per stage, the encoder settings, the outputs, the sessions, the pairing state and the host's log. It changes the rate control, the bitrate, the frame rate, the parity level, the picture, the audio, ends a session, revokes a client and asks for a keyframe.

**The security rules, and they matter:** the server binds to a loopback address, every change must carry the token the host printed, and a request that names another origin is refused. All three are verified: a command without the token gets 403, a command with a foreign `Origin` gets 403, and a command with both works.

**The cost:** the control plane adds about 1.5 % of a core to the host, and the state document is about 3 kB. The event stream leaves the log out and the dashboard fetches it on its own every two seconds, because the log changes rarely and carrying it in the stream would be most of the traffic.

---

## 10. The quality ladder

A client that cannot decode and present the picture it is given drops frames, and a stream that drops frames is worse than a smaller stream that does not. So the client says so: when it drops frames it asks the host for the next size down, and the host scales the capture to it inside the colour conversion, which costs less work and is faster.

- The ladder lives in `crates/lumen-client-desktop/src/ladder.rs`. Eight rungs from 3840x2160 down to 640x360, every one 16:9 with an even height.
- It walks down when the drop rate over a second is above 2 %, and up again after ten clean seconds, but only to a picture the client has not already failed at. That block expires after five minutes, so a picture that failed once is tried again later.
- The client never asks for a picture bigger than the one it has been sent, and the host never serves one bigger than the one it captures: the extra pixels would have to be invented.
- The operator's choice is the ceiling: the host clamps every request to it, and the dashboard has a picture selector.

**Measured:** at 1280x720 the capture stage takes 7.93 ms and the whole pipeline 23.2 ms; at 640x360 the capture takes 6.97 ms and the pipeline 16.0 ms, and the client uses 25 % less of a core. A smaller picture is cheaper and faster.

---

## 11. The second commit

These are in the repository, in the commit after the first. They compile and the whole suite passes with them.

| File | What changed |
|---|---|
| `crates/lumen-host-service/src/pipeline.rs` | the capture converts straight out of the captured frame instead of copying it into a new vector first. `cpu_bgrx` returns a `Cow`, so the BGRX case borrows. This removed eight megabytes of copying a frame |
| `crates/lumen-transport/src/media.rs` | the access unit is sliced into **one** padded buffer instead of a vector of slices, and the data slices are sealed straight out of it. A frame at 1080p was a few thousand small allocations every frame; the history keeps the one buffer |
| `crates/lumen-host-service/src/stages.rs` | the recorder keeps the first and the newest capture timestamp and the frame count, so `session()` gives the rate the pipeline delivered |
| `crates/lumen-host-service/src/web.rs` | the state carries `delivered_fps` with the other counters, and the streamed state leaves the log out |
| `crates/lumen-web/assets/index.html` | the rate tile reads the delivered rate, and the log is fetched on its own |
| `.github/workflows/ci.yml` | **the clippy, deny and audit jobs now install the system packages.** They build the workspace, and `lumen-input-x11` needs `libxkbcommon-x11-dev`, which was missing: the CI would have failed to build |
| `crates/lumen-protocol/examples/colorbench.rs` | **new.** Times the CPU conversion at each size |
| `crates/lumen-capture-x11/examples/capturecost.rs` | **new.** Times the X11 capture on its own |
| `docs/screenshots/dashboard.png` | **new.** The control plane |

**What the copies were worth:** the host delivered about 50 fps at 1080p before and about 70 fps after, and the client got the same kind of win. The measurement that started this was simple: the capture alone did 110 fps and the conversion alone allowed 147, so neither was the limit — the copies around them were.

---

## 12. The remaining work, in execution order

### P0. Regenerate

1. **Regenerate every artefact**, because the numbers moved after the copies were removed:
   ```
   MODE=loopback DURATION=30 OUT=artifacts/m1/e2e.json ./scripts/run-e2e.sh
   MODE=netns   DURATION=30 OUT=artifacts/m1/e2e-netns.json ./scripts/run-e2e.sh
   ./scripts/sweep-loss.sh --levels 0.005,0.02,0.05 --jitter 10ms --out artifacts/m2/curve.csv
   ./scripts/run-input-latency.sh --duration 60 --out artifacts/m3/input.json
   MODE=loopback DURATION=30 WITH_AUDIO=1 WITH_CLIPBOARD=1 WITH_RESIZE=1 OUT=artifacts/m4/e2e.json ./scripts/run-e2e.sh
   ./scripts/run-tier-b.sh --cpus 0,1 --duration 300 --report artifacts/m5/tierb.json
   ./scripts/run-reconnect.sh --cycles 20 --out artifacts/m6/reconnect.json
   ./scripts/run-compositor-restart.sh --cycles 5 --out artifacts/m6/restart.json
   ./scripts/run-wifi-flap.sh --out artifacts/m6/wifi-flap.json
   ./scripts/run-e2e-x11.sh --duration 60 --report artifacts/m7/e2e-x11.json
   ./scripts/soak.sh --duration 300 --out artifacts/m8/soak.json
   ./scripts/chaos.sh --duration 300 --out artifacts/m8/chaos.json
   ```
   **One netns pair at a time.** The tier B script wants the ports 24820 to 24861 and its own binaries, because another script's cleanup can end its run.
2. **Update `docs/benchmarks.md` and `docs/final-report.md`** against the new artefacts, and `docs/handoff.md` (this file) with the new numbers.

### P1. The latency that is left

4. **Zero-copy decode and present (O-7, O-30).** An EGL NV12 import with a YUV to RGB shader in `lumen-render`, and a decode DMA-BUF default. The client's present stage is 8.13 ms at 1600x900 and most of it is the conversion and the copy. This is what the Tier B acceptance needs.
5. **The GPU colour conversion on the host (D49).** The conversion is 6.81 ms at 1080p and it alone caps the host at 147 fps. A GPU path needs a GLES or Vulkan context in the capture crate; `ash` is already a dependency, `egl` and `gbm` are too, but there is no GLES binding.
6. **The quality ladder is not wired into the tier B script (O-30).** The proxy still drops 2992 frames because nothing tells the client to ask for a smaller picture in that run. This is the cheapest of the three.

### P2. The control plane, still open

7. **The control plane cannot start a host (O-34).**
8. **The dashboard rewrites the whole log when it changes (O-36).** Only the changed lines should be written.
9. **The dashboard is not in the README's screenshot set for the 1080p120 case**, and the picture selector is new: the README and the screenshots should be brought in step.

### P3. The transport, still open

10. **The FEC recovery counter (O-26).** Do not retry a recovery while a frame's slices are still arriving.
11. **The adaptive parity cold start (O-33).** Start the session at the light level and keep it there until the link has been lossless for a while.
12. **The `NoiseTransport` review (O-3).**
13. **QUIC (M2.6)** behind a feature, only if a decision needs data.

### P4. Input

14. **The input path is not wired into the host service.** `crates/lumen-transport/src/input_channel.rs` has the isolated socket, `lumen-input-wayland` and `lumen-input-inject` have the capture and the injection, but the host service has no `input.rs` that ties them together. The measured latency is the injection leg only.

### P5. The plan's remaining code

15. **HDR and 10-bit (M4.8).** P010 and the HDR metadata transport. The `dynamic_range` capability bit needs adding to `crates/lumen-protocol/src/capability.rs`.
16. **HEVC and AV1.** The plan's codec ladder. Record the D-CODEC decision.
17. **PipeWire capture (M1.7).** `lumen-capture-pipewire` is still a stub. The plan's risk table allows the wlr fallback, so this is as much a decision to record as code to write. It is the precondition for the portal restore token (M6.3).
18. **NVENC (M1.10), Vulkan encode (M1.11), Vulkan decode (M1.13).** The hardware for NVENC is present. Decide with the D-ENC-VENDOR experiment before writing them, and mark them pending with the reason if they cannot be measured here.
19. **`lumenctl`.** The plan's name for a control tool. `lumen-host` and `lumen-client` cover the same ground; either add it or record the deviation.

### P6. Baselines and the rig

20. **The baselines (M8.5).**
21. **The long soak (M8.3).** The run budget allows about two hours.
22. **The physical rig (M8.6).** The procedure is written. It needs the hardware.

---

## 13. How to verify

```
export CARGO_HOME=/home/hayden/.cargo RUSTUP_HOME=/home/hayden/.rustup PATH=/home/hayden/.cargo/bin:$PATH
cd /home/hayden/Desktop/projects/lumen

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check && cargo audit
cargo test --workspace

scripts/probe-caps.sh && scripts/netns-up.sh
test -f artifacts/m0/capabilities.json && test -f artifacts/m0/baselines.json

MODE=loopback DURATION=30 OUT=artifacts/m1/e2e.json ./scripts/run-e2e.sh
MODE=netns   DURATION=30 OUT=artifacts/m1/e2e-netns.json ./scripts/run-e2e.sh
./target/release/lumen-bench report --input artifacts/m1/e2e.json \
    --assert 'decode_errors==0' --assert 'p99_end_to_end_ms<=40'

./scripts/sweep-loss.sh --levels 0.005,0.02,0.05 --jitter 10ms --out artifacts/m2/curve.csv
./scripts/run-input-latency.sh --duration 60 --out artifacts/m3/input.json
MODE=loopback DURATION=30 WITH_AUDIO=1 WITH_CLIPBOARD=1 WITH_RESIZE=1 OUT=artifacts/m4/e2e.json ./scripts/run-e2e.sh
./scripts/run-tier-b.sh --cpus 0,1 --duration 300 --report artifacts/m5/tierb.json
./scripts/run-reconnect.sh --cycles 20 --out artifacts/m6/reconnect.json
./scripts/run-compositor-restart.sh --cycles 5 --out artifacts/m6/restart.json
./scripts/run-e2e-x11.sh --duration 60 --report artifacts/m7/e2e-x11.json

cd fuzz && for t in protocol_parser fec_reassemble transport_inject h264_au_split input_parser; do
  cargo +nightly fuzz run "$t" -- -max_total_time=60 || exit 1
done; cd ..
./target/release/lumen-bench rig-selftest --out artifacts/m1/rig-selftest.json
```

The control plane, by hand:

```
lumen-host --web-listen 127.0.0.1:8080 &
# the URL with the token is on the host's standard error
curl -s http://127.0.0.1:8080/api/state | python3 -m json.tool
curl -s -X POST http://127.0.0.1:8080/api/command \
    -H "X-Lumen-Token: $TOKEN" -H 'Content-Type: application/json' \
    -d '{"command":"set_quality","args":{"width":854,"height":480}}'
```

---

## 14. The gotchas, learned the hard way

1. **File tools resolve relative paths against `/home/hayden/Desktop/projects`, not the workspace.** Use absolute paths.
2. **Hashline tags are per-file and change on every edit.** Copy the tag from the latest read.
3. **Edit boundaries can be auto-repaired, and an edit can silently not apply.** A python script that asserts and then writes is the safest way to edit; several of the edits in this session silently did nothing and were found by grepping afterwards. **After every scripted edit, grep for what you changed.**
4. **A shared `target/` across agents breaks.** A sibling's mid-edit change shows up as an error in a crate you did not touch. Never `cargo clean` the shared target.
5. **Clippy is `-D warnings` with a pedantic set.** Expect `needless_pass_by_value`, `single_match_else`, `collapsible_if`, `manual_is_multiple_of`, `chunks_exact_to_as_chunks`, `format_collect`, `map_unwrap_or`, `needless_continue`, `unreadable_literal`, `cast_lossless`, `redundant_closure_for_method_calls`, `int_plus_one` and `if_not_else`.
6. **No placeholder tests.**
7. **Display-dependent tests must skip with a printed reason.**
8. **Use `-displayfd` for Xvfb.** Fixed display numbers collide.
9. **`ip -n NS tc` is invalid.** Use `ip netns exec NS tc`.
10. **One netns pair at a time.**
11. **Fuzz uses nightly**, from `fuzz/`, which is outside the workspace.
12. **Never `pkill -f` a pattern that appears in your own command line.** It kills the shell. Use `pkill -x`.
13. **Watch the disk.** Two build trees plus the shared `target/` filled the 234 GB disk once.
14. **A fixed sleep is not a readiness check.** The e2e script waits for the control port in the socket table, because under load the host needs more than a second to bind and a connecting probe leaves a session thread waiting for a handshake that never comes.
15. **Do not trust host load numbers.** Measure on a quiet machine and record the load.
16. **Do not put an exact frame count in prose.** The counts move with the load. Put them in the artefact.
17. **A blocking call in the control loop poisons every latency number.** The clock offset comes from the ping exchange, so a 20 ms block in the control stage put 10 ms into every "send" stage. When a stage time is implausible, look for a block in the control channel before looking at the network.
18. **A measurement that competes with the machine is not a measurement.** The X11 source painter at 200 fps burned a core and halved the numbers it was supposed to be the cause of. Run the painter at the rate the session is meant to deliver.
19. **The dashboard's rate is the delivered rate, not the requested one.** A stream configured at 120 fps that delivers 60 must say 60.

---

## 15. The risks

- **The numbers move with the load.** Every figure in section 5 is from a run on this machine with a known load, and the artefacts are the record.
- **Glass-to-glass cannot be measured here.** Every target that needs the rig stays **pending**. Do not write a number for them.
- **The pipeline latency excludes the display.** The present stage ends when the frame is handed to the compositor.
- **The two clocks are aligned by an NTP-style estimate** that assumes the two directions take the same time. The error is well under a millisecond on a LAN, but the whole cross-machine measurement rests on it.
- **The FEC recovery counter overstates the loss.** Read the true loss as what the client reports over the link feedback.
- **The adaptive parity has a cold start** and the quality ladder lowers the picture and never raises it. Both are recorded (O-33, O-35).
- **The 1080p120 case does not reach 120 fps on this machine**, and the reason is the CPU path, not the hardware. That is the next thing to fix, and the measurements to do it are in the two examples.
- **The repository has one commit.** There is no history to fall back on. Keep backups of anything you are about to rewrite.
- **The disk filled once.** Watch it.

## 16. Latency plan execution status (P0 to P7)

Measured on the zero-copy path, which needs `WLR_RENDERER=gles2` (see the paragraph after the list). Artefacts: `artifacts/latency/after-*.json`, `after-curve.csv`, `after-tierb.json`, `input.json`, `sweep-gles2/`. The pixman runs from the same session are kept as `artifacts/latency/pixman-*`; the P0 baselines are `artifacts/latency/baseline-*`.

- Loopback end-to-end p50 8.59 ms (P0 baseline 19.02 ms). Netns 8.58 ms (19.18 ms). Loopback with audio, clipboard and resize 8.31 ms. Client present p50 0.047 ms (baseline 4.37 ms). Decode errors 0 in all three.
- `zero_copy` equals `frames_presented`, and `frames_gpu_converted` equals the host frames captured, in all three runs.
- Loss sweep at 0.5, 2 and 5 %: decode errors 1, 1, 0 (P0 baseline 3, 4, 7).
- Tier-B proxy, 300 s on cpus 0,1: dropped frames 0 (P0 baseline 5), decode p50 2.28 ms (baseline 5.18 ms).
- Input latency through `run-input-latency.sh` (the example injector, XTEST): p50 46 us, p99 115 us, 60000 injected, 0 failures, 0 replays. This script does not exercise the production uinput path.
- Resize (P5 log): `/api/log?after=N` returns lines with `seq` greater than N, in increasing order; `after=<last seq>` returns `[]`.

Compositor renderer: `run-e2e.sh`, `sweep-loss.sh` and `run-tier-b.sh` start headless sway with `WLR_RENDERER`, default `pixman`. Pixman advertises no dmabuf formats, so the client plan is `Cpu` and host capture is shm. Run with `WLR_RENDERER=gles2` to exercise the zero-copy path (`DirectNv12` and `dmabuf` capture were logged).

Open items:

- `/dev/uinput` is `root:root` mode 0600 and user `hayden` cannot open it. The host runs without input injection and logs `lumen-host: input injection unavailable`. Needs a udev rule or group membership.
- `lumen-input-x11` test `env_display` fails on a dedicated Xvfb: XI2 raw motion arrives, but no raw key press within 5 s. The client keyboard path is unverified.
- The client input stage uses raw XI2 events, which are server-wide. While a session runs, every key on the client X server goes to the host, not only keys typed into the lumen window. Needs focus gating before real use.
- The VPP check lives in `lumen-decode-vaapi` (`decoded_dmabuf_converts_to_bgrx_like_the_cpu_path`), not `lumen-ffi-va --test vpp`. The 64x64 comparison passes with worst channel difference 2. The 1280x720 comparison skips because no fixture exists.
- Tier-B on the pixman CPU path dropped 33 frames (baseline 5). The cause is not confirmed. One candidate is the P2 `BLOCK_FOR` change from 300 s to 60 s. The zero-copy run drops 0.


## 17. Latency plan close-out (2026-10-22)

**State.** `LUMEN_LATENCY_PLAN.md` P0 to P6 are implemented and measured. The P7 acceptance passes on the GPU path, which needs `WLR_RENDERER=gles2`, except step 7: the input run measures the injector only (see the last gotcha). Gates: `cargo fmt --all -- --check` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo test --workspace` 575 passed, 0 failed, with `DISPLAY` and `WAYLAND_DISPLAY` unset. Numbers: `docs/benchmarks.md` (latency section). Defects: `docs/known-limits.md` O-7, O-9, O-26, O-30, O-33, O-35, O-36 updated; O-37 to O-41 new. Decisions: `docs/design.md` D76 to D81.

**Changed in this pass.** `crates/lumen-bench/src/report.rs`: `zero_copy`, `frames_gpu_converted` and `frames_rebuilt` now default when absent. The P0 baseline artefacts did not parse before, so the plan's baseline assertion could not run. The `BLOCK_FOR` experiment (300 s) was reverted to the plan's 60 s. Nothing else in code changed.

**Verified this pass.** The `after-*` reports pass the plan's acceptance (`decode_errors` 0, presented at least 97 % of sent, end-to-end p50 8.6 ms against 19.0 ms, client present p50 0.047 ms against 4.37 ms, `zero_copy` equal to presented, `frames_gpu_converted` equal to host frames captured). The two-vCPU proxy on the GPU path drops 0 frames. The host log endpoint returns increasing `seq` and `[]` after the last one. The VPP check passes at 64x64 with a worst channel difference of 2.

**Open decisions for the user.**
1. Block length after a failed picture (O-39, D78): 60 s as the plan says, 300 s, or a backoff that doubles on each repeated failure. On the CPU path, 60 s gave 29 to 33 dropped frames in 300 s against 14 with 300 s and 5 before the change.
2. Client input capture is server-wide (O-37). While a session runs, keys typed into other X applications on the client reach the host. Focus gating needs a protocol choice, since X11 cannot see the Wayland window's focus. Privacy-relevant; fix before real use.
3. `/dev/uinput` is `root:root` 0600 (O-38), so the host runs with no injection. A udev rule or group membership fixes it. That is a system change and was not made.

**Gotchas found this pass.**
- `lumen-bench report --input X` writes the merged output to `artifacts/m1/e2e.json` unless `--out` is given. It overwrote the committed file once; `git checkout` restored it. Pass `--out` to every read.
- `crates/lumen-input-x11/tests/env_display.rs` sends XTEST pointer motion (7, 3) and an `a` key press and release to the display it connects to. It was run in a loop over every X socket in `/tmp/.X11-unix` by mistake. It connected to `:2`, `:3`, `:90`, `:93` and `:95`, so those got the motion; `:2`, `:90`, `:93` and `:95` also got the `a` key. The user session on `:0` refused the connection, so it got no input. `:93` and `:95` are root-run servers from `/tmp/opencode` configs, not ours. Run this test only with `DISPLAY` pointing at a dedicated Xvfb: `:90` and `:2` pass.
- `run-e2e.sh`, `sweep-loss.sh` and `run-tier-b.sh` default to `WLR_RENDERER=pixman`. The plan's commands produce the CPU plan unless `WLR_RENDERER=gles2` is set.
- `run-input-latency.sh` measures the example injector with XTEST, not the client capture path. The plan's step 5 asked for the latter. Not done.

