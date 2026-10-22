# Lumen — low-latency LAN remote desktop for Linux (Rust)

## Context

Build **Lumen**, a Linux-only remote desktop system in Rust (host + client), LAN-only, intended to beat Moonlight+Sunshine, Vibepollo, RustDesk and GNOME Remote Desktop on latency, image quality and reliability. Both endpoints run Linux; Wayland first, X11 second. Latency targets are measured with a photodiode and a high-speed camera, never asserted.

This file is the execution spec. Every crate, file, task, acceptance command, decision experiment and fallback is fixed here, so an implementer unfamiliar with the conversation can execute it top to bottom without inventing design.

Environment reality, verified 2026-10-21 on the build machine: Ubuntu 26.10, kernel 7.3.0-9, 12 vCPU, AMD Ryzen 5 4600H with Radeon Graphics (PCI 1002:1636, `amdgpu`, `radeonsi_drv_video` present → VA-API encode/decode), NVIDIA GTX 1650 (driver 580.178.04; `/dev/nvidia0`, `libnvidia-encode.so.1`, `libnvcuvid.so.1`, `nvidia_icd.json` → NVENC/NVDEC and Vulkan Video candidate), one physical eDP-1 panel at 1920x1080 with no compositor session running, USB webcam at `/dev/video0`, PipeWire 1.6.8 daemon running, `sudo` passwordless, `ip netns` + `tc netem` usable as root, **no Rust toolchain installed**, **no photodiode rig**, **no second machine**, **no Tier B (Celeron N4020) client**.

Scope note: all first-party code is Rust. No C/C++ source is compiled into the product. The only native code linked is OS-provided system libraries reached through Rust bindings. Each native boundary lives in exactly one crate that owns the `unsafe` and exposes a safe API. No dependency that forces copyleft is permitted; `cargo deny` enforces it. The reference implementations (Sunshine, Moonlight, Apollo, Vibepollo, RustDesk, GNOME Remote Desktop) are read for design facts only — all are GPL/AGPL and none of their code is copied.

---

## Baseline eligibility (verified from upstream source, 2026-10-21)

| Candidate | Linux host | Linux client | Verdict |
|---|---|---|---|
| Sunshine (`LizardByte/Sunshine`, GPL-3.0) | yes — KMS/wlr/X11/portal capture, VAAPI/NVENC/Vulkan/software encoders | no (host only) | **Host baseline**; paired with Moonlight-Qt |
| Moonlight-Qt (`moonlight-stream/moonlight-qt`, GPL-3.0) | no (client only) | yes — Linux/macOS/Windows/Steam Link, renderers incl. Vulkan libplacebo, KMS DRM, VAAPI | **Client baseline**; `moonlight-client-qt` and `LizardByte/moonlight-embedded` do not exist (404) |
| Apollo (`ClassicOldSong/Apollo`, GPL-3.0) | source-capable but Windows-first: Windows-only binaries, no Linux CI, no PipeWire/portal capture, virtual display Windows-only | no (companion client is Android-only) | **Dropped from measured baseline** (no Linux client, no Linux binaries); design reference only |
| Vibepollo (`Nonary/Vibepollo`, GPL-3.0) | beta: native Linux service, but Arch/CachyOS-only pacman packaging, needs Plasma 6 Wayland, kernel ≥6.16 and its own DKMS `vibeshine_drm` kernel driver | no (needs `Nonary/moonlight-qt` fork) | **Dropped from measured baseline** (packaging + kernel driver not reproducible here); design reference only — it is the only intra-only codec in the set, worth studying |
| moonlight-embedded (`moonlight-stream/moonlight-embedded`, GPL-3.0) | no | ARM SBC class only (pi/imx/aml/rk/mmal/x11/sdl); zero ESP32 references | **Dropped** (not a desktop Linux endpoint) |
| RustDesk (`rustdesk/rustdesk`, AGPL-3.0) | yes | yes | **Baseline, both sides**; Linux host uses VAAPI-only hardware H.264 (`hevc_vaapi` disabled upstream), media rides reliable ordered streams (TCP / KCP-ARQ / WebRTC data channel) with no FEC → head-of-line blocking, 30 fps default, 120-frame client queue |
| GNOME Remote Desktop (`GNOME/gnome-remote-desktop`, GPL-2.0+) | yes — Mutter ScreenCast + PipeWire, VAAPI/NVENC H.264, libei input | no client of its own | **Host baseline in RDP mode**, measured against `xfreerdp` as the RDP client |

Design consequences taken directly from those sources and encoded in the tasks below: Moonlight's client keeps a 15-frame decode queue plus a 5-frame pacer budget, so Lumen keeps queues at depth 1–2; Sunshine ships 20 % Reed-Solomon parity, so Lumen ships adaptive parity instead of a fixed ratio; RustDesk's single reliable ordered stream stalls video and input together on one lost packet, so Lumen separates lossy video datagrams from a small reliable control channel; GRD disables RDP multitransport and runs everything over TCP, so Lumen never uses TCP for media.

---

## Repository layout

Root: `/home/hayden/Desktop/projects/lumen`

```
lumen/
├── Cargo.toml                 # workspace, resolver 2, [workspace.lints]
├── rust-toolchain.toml        # channel = "1.99.1", components rustfmt + clippy
├── rust-toolchain-nightly.toml# channel = "nightly" (cargo-fuzz only)
├── deny.toml                  # allow MIT/Apache-2.0/BSD/ISC/Unicode-DFS-2016 only
├── README.md
├── LICENSE-MIT  LICENSE-APACHE
├── .github/workflows/ci.yml   # fmt, clippy -D warnings, test, deny, audit, fuzz smoke
├── .github/workflows/fuzz.yml # nightly cargo-fuzz, 60 s per target
├── docs/
│   ├── design.md              # architecture, stage budgets, decisions + reversibility, dependency conflicts
│   ├── protocol.md            # wire protocol spec (written before transport code)
│   ├── benchmarks.md          # per-stage percentiles, baselines, raw data, provisional markers
│   ├── known-limits.md        # hardware-gated items and explicit non-goals
│   └── final-report.md        # measured results, deviations, pending items, unsolved problems
├── crates/                    # 23 crates, table below
├── fuzz/                      # cargo-fuzz targets (workspace member, nightly)
├── scripts/                   # env setup, netns, netem, e2e, soak, chaos, baselines
└── harness/                   # photodiode/camera rig procedure + simulated-trigger fixtures
```

### Crates

FFI rule: exactly one crate owns each native boundary; it holds every `unsafe` block, comments why it is sound, and exposes a safe API. All other crates are `#![forbid(unsafe_code)]`.

| Crate | Responsibility | Native boundary |
|---|---|---|
| `lumen-ffi-va` | libva bindings + safe wrapper: display/context creation, surface import/export by DRM PRIME, encode/decode entry points, attribute queries | libva (dlopen `libva.so.2`) — sole owner of libva `unsafe` |
| `lumen-ffi-libei` | libei bindings + safe wrapper: EIS receiver (client) and EI sender (host injection) | libei (dlopen `libei.so.1`) — sole owner of libei `unsafe` |
| `lumen-ffi-nvenc` | NVENC bindings wrapper: session config, input buffer registration (DMA-BUF via CUDA external memory), bitstream extraction | `libnvidia-encode.so.1` (dlopen) + `libcuda.so.1` via cudarc dynamic loading — sole owner of CUDA/NVENC `unsafe` |
| `lumen-protocol` | Wire format: media header, message kinds, capability bitmap, version negotiation, slice/AU descriptors, error taxonomy, codec and pixel-format enums | none (pure Rust) |
| `lumen-transport` | UDP media datagrams, pacing, FEC blocks, deadline-aware selective retransmission, RTT and clock-offset estimation, bandwidth estimate from delay gradient + loss, loss/jitter injection hooks | none |
| `lumen-crypto` | Noise XX handshake (X25519 + AES-256-GCM), HKDF key separation across control/media/input domains, Ed25519 device identity, pairing code derivation, per-direction AEAD counters | none (`snow`, `x25519-dalek`, `ed25519-dalek`, `aes-gcm`) |
| `lumen-capture-pipewire` | xdg-desktop-portal ScreenCast → PipeWire stream, DMA-BUF negotiation, `spa_meta_header` capture timestamps, restore-token reuse | none (`ashpd`, `pipewire`, `libspa`) |
| `lumen-capture-wlr` | Direct `zwlr_screencopy_manager_v1` DMA-BUF capture, `copy_with_damage` when compositor refresh ≥1.5× stream fps | none (`wayland-client`, `wayland-protocols-wlr`) |
| `lumen-capture-x11` | X11 host capture: DRI3 → XShm → XDamage ladder, cursor via XFixes | none (`x11rb`, feature `allow-unsafe-code` for DRI3 fd paths) |
| `lumen-encode-vaapi` | VA-API H.264/HEVC encode (NV12/P010), no B-frames, low-latency flag, infinite GOP, on-demand IDR, CQP/CBR rate control | via `lumen-ffi-va` |
| `lumen-encode-nvenc` | NVENC encode: ULTRA_LOW_LATENCY tuning, CBR/VBR, infinite GOP, on-demand IDR, DMA-BUF input | via `lumen-ffi-nvenc` |
| `lumen-encode-vulkan` | Vulkan Video encode candidate path (`VK_KHR_video_encode_queue`), 4:2:0 8-bit | `ash` 0.38 (pinned; Video bindings are semver-exempt) |
| `lumen-decode-vaapi` | VA-API decode of H.264/HEVC/AV1 into DMA-BUF surfaces, CPU fallback surfaces | via `lumen-ffi-va` |
| `lumen-decode-vulkan` | Vulkan Video decode, including `lavapipe` software decode (the software correctness path) | `ash` 0.38 (pinned) |
| `lumen-render` | Client present path: xdg-shell surface, `wl_shm` path, `linux-dmabuf`+EGL path, `wp_presentation`/`wp_commit_timing` feedback, `wp_tearing_control`, cursor, X11 present via x11rb | none (`wayland-client`, `gbm`, `egl`) |
| `lumen-input-wayland` | Client-side Wayland input capture: EIS device regions, xkbcommon keymap, relative pointer + pointer constraints, touch/pen/gamepad, IME via `zwp_text_input` | via `lumen-ffi-libei` |
| `lumen-input-x11` | Client-side X11 input capture: XI2 raw events (`XI_RawMotion`), keymap via xkbcommon | none (`x11rb`, feature `xinput`) |
| `lumen-input-inject` | Host-side injection: EI sender backend when the compositor supports it, uinput virtual keyboard/pointer/touch/pen, uhid gamepads, XTest for X11 hosts | uinput/uhid ioctls via `nix`/`rustix`; XTest via `x11rb` feature `xtest`; EI sender via `lumen-ffi-libei` |
| `lumen-audio` | PipeWire session-audio capture from a per-session virtual sink, Opus 2.5/5/10 ms frames at 48 kHz restricted-low-delay, PCM mode, bidirectional, timestamped A/V sync | none (`pipewire`, `libspa`, `opus`) |
| `lumen-host-service` | Host daemon: pipeline orchestration, session manager, pairing store, control plane, input injection, clipboard, monitor enumeration; bin `lumen-host` | none |
| `lumen-client-desktop` | Client app: connect, pair, receive→reassemble→decode→present, input capture, audio playback, clipboard; bins `lumen-client`, `lumenctl` | none |
| `lumen-bench` | Measurement harness: test pattern with camera-readable barcode (frame counter + host timestamp), per-stage timestamp records, clock-offset estimation, percentile reports, baseline drivers, simulated photodiode trigger; bin `lumen-bench` | none (`image`, `serde_json`) |

Rejected dependencies, with reasons recorded in `docs/design.md`: `ffmpeg-next` (links system FFmpeg; crate license WTFPL, FFmpeg LGPL/GPL), `x264`/`x265` (GPL), `openh264` (vendors Cisco C source), `ring`/`aws-lc-rs` (vendor C/asm; `snow` + RustCrypto AEAD replace them, and `ring` has no XChaCha), `xdg-desktop-portal` crate (MPL-2.0 → use `ashpd`, MIT), `gstreamer` (LGPL core), `vpx` (MPL-2.0, abandoned), RustDesk `hwcodec` (AGPL, statically compiles FFmpeg). `reed-solomon-erasure` 6.0.0 is used only with `default-features = false, features = ["std"]` so its vendored-C `simd-accel` path is never compiled.

---

## Milestones, tasks, files, acceptance

Each milestone ends with its acceptance block green. Commands run from the repo root after `scripts/env-setup.sh` has run once (M0). Benchmark artifacts land in `artifacts/<milestone>/`.

### M0 — Environment bootstrap and capability probe

| # | Task | Files |
|---|---|---|
| M0.1 | Install toolchain: rustup with stable **1.99.1**, rustfmt, clippy, nightly for fuzz; `cargo install cargo-deny cargo-audit cargo-fuzz` | `rust-toolchain.toml`, `rust-toolchain-nightly.toml`, `scripts/env-setup.sh` |
| M0.2 | Install system packages: `build-essential pkg-config clang libclang-dev libva-dev libdrm-dev libgbm-dev libegl-dev libvulkan-dev libwayland-dev libxkbcommon-dev libxcb1-dev libxcb-util-dev libpipewire-0.3-dev libopus-dev libei-dev libudev-dev libinput-dev libasound2-dev xvfb x11-xserver-utils xserver-xorg-video-dummy sway xdg-desktop-portal xdg-desktop-portal-wlr mesa-va-drivers vainfo vulkan-tools nvidia-vaapi-driver iproute2 net-tools ffmpeg v4l-utils xfreerdp` | `scripts/env-setup.sh` |
| M0.3 | Capability probe: `vainfo`, `vulkaninfo --summary`, `/dev/dri` and `/dev/nvidia*`, `nvidia-smi`, portal backends, `pkg-config` versions → `artifacts/m0/capabilities.json` | `scripts/probe-caps.sh` |
| M0.4 | Namespace harness: `lumen-host`/`lumen-client` netns pair over veth, `tc netem` attach points | `scripts/netns-up.sh`, `scripts/netns-down.sh` |
| M0.5 | Workspace skeleton: root `Cargo.toml`, `deny.toml`, CI workflows, license files, crate stubs with `#![forbid(unsafe_code)]` except the three FFI crates | `Cargo.toml`, `deny.toml`, `.github/workflows/ci.yml`, `crates/*/Cargo.toml`, `LICENSE-MIT`, `LICENSE-APACHE` |
| M0.6 | Baseline availability probe: detect runnable Sunshine, Moonlight-Qt, RustDesk (+server), GRD + `xfreerdp`; record versions and runnable commands → `artifacts/m0/baselines.json` | `scripts/baseline-check.sh` |

Acceptance (M0):
```
scripts/env-setup.sh && scripts/probe-caps.sh && scripts/netns-up.sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
test -f artifacts/m0/capabilities.json && test -f artifacts/m0/baselines.json
ip netns list | grep -q lumen-host && ip netns list | grep -q lumen-client
```

### M1 — Wayland host to Wayland client, 1080p60 on LAN

| # | Task | Files |
|---|---|---|
| M1.1 | Protocol core: 32-byte media header, message kinds, capability bitmap, version handshake structs, error enum | `crates/lumen-protocol/src/{lib.rs,header.rs,message.rs,capability.rs,error.rs}` |
| M1.2 | Protocol tests: golden vectors, round-trip, malformed rejection; fuzz target `protocol_parser` | `crates/lumen-protocol/tests/`, `fuzz/fuzz_targets/protocol_parser.rs` |
| M1.3 | Crypto: Noise XX (X25519, AES-256-GCM), HKDF separation into control/media/input keys, Ed25519 device identity, pairing code derivation, AEAD media seal with per-direction counters | `crates/lumen-crypto/src/{lib.rs,handshake.rs,keys.rs,pairing.rs,aead.rs}` |
| M1.4 | Transport core: UDP socket (GSO off, `SO_PRIORITY` 5 video / 6 audio, DSCP 46), pacing token bucket, deadline-aware retransmission queue, RTT and clock-offset estimator, bandwidth estimator (delay gradient + loss) | `crates/lumen-transport/src/{lib.rs,socket.rs,pacing.rs,retransmit.rs,estimator.rs,inject.rs}` |
| M1.5 | FEC: per-frame blocks of 8 data + 2 parity packets (XOR-all, XOR-of-halves) recovering any two losses per block; pure-Rust Reed-Solomon variant behind feature `rs-fec` | `crates/lumen-transport/src/fec.rs` |
| M1.6 | FFI crate libva: dlopen `libva.so.2`, display/context/surface wrappers, DRM PRIME import/export, attribute queries | `crates/lumen-ffi-va/src/{lib.rs,display.rs,context.rs,surface.rs,sys.rs}` |
| M1.7 | Capture (portal + PipeWire): `ashpd` ScreenCast with monitor selection and restore token, PipeWire stream with DMA-BUF negotiation, `spa_meta_header` timestamps, NV12/P010 request | `crates/lumen-capture-pipewire/src/{lib.rs,portal.rs,stream.rs,format.rs,timestamp.rs}` |
| M1.8 | Capture fallback (wlroots): `zwlr_screencopy_manager_v1` v2, `copy_with_damage` when compositor refresh ≥1.5× stream fps | `crates/lumen-capture-wlr/src/{lib.rs,screencopy.rs,damage.rs}` |
| M1.9 | Encode (VA-API): H.264 High NV12, `VAConfigAttribLowLatency`, `max_num_reorder_frames=0`, `num_ref_frames=1`, infinite GOP with on-demand IRAP, CQP 22 default, CBR mode selectable, DMA-BUF import | `crates/lumen-encode-vaapi/src/{lib.rs,session.rs,buffer.rs,config.rs}` |
| M1.10 | FFI crate NVENC + CUDA: dlopen `libnvidia-encode.so.1`, cudarc dynamic loading for external memory, `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`, `NV_ENC_PARAMS_RC_CBR`, `NV_ENC_INFINITE_GOPLENGTH`, on-demand IDR | `crates/lumen-ffi-nvenc/src/{lib.rs,sys.rs,session.rs,input.rs}`, `crates/lumen-encode-nvenc/src/{lib.rs,session.rs,config.rs}` |
| M1.11 | Encode (Vulkan Video): `VK_KHR_video_encode_queue` probe and H.264 encode, 4:2:0 8-bit | `crates/lumen-encode-vulkan/src/{lib.rs,probe.rs,encode.rs}` |
| M1.12 | Decode (VA-API): H.264/HEVC/AV1 decode into DMA-BUF surfaces, CPU fallback path | `crates/lumen-decode-vaapi/src/{lib.rs,session.rs,surface.rs}` |
| M1.13 | Decode (Vulkan Video): probe and decode, `lavapipe` software decode as the software correctness path | `crates/lumen-decode-vulkan/src/{lib.rs,probe.rs,decode.rs}` |
| M1.14 | Raw codec for correctness-only software tests: uncompressed NV12 passthrough so framing, FEC and transport are testable with no codec present | `crates/lumen-protocol/src/raw.rs` |
| M1.15 | Render: xdg-shell surface, `wl_shm` present path, `linux-dmabuf`+EGL present path, `wp_presentation` and `wp_commit_timing` feedback capture, `wp_tearing_control`, cursor rendering | `crates/lumen-render/src/{lib.rs,wayland.rs,shm.rs,dmabuf.rs,presentation.rs,cursor.rs}` |
| M1.16 | Host service: capture→encode→packetize→send pipeline, control channel (TCP, Noise-secured, length-prefixed frames), session manager; bin `lumen-host` | `crates/lumen-host-service/src/{lib.rs,main.rs,pipeline.rs,control.rs,session.rs}` |
| M1.17 | Client app: connect, pairing prompt, receive→reassemble→decode→present; bin `lumen-client` | `crates/lumen-client-desktop/src/{lib.rs,main.rs,receive.rs,present.rs}` |
| M1.18 | Bench harness: test-pattern generator (barcode frame counter + host timestamp + human-readable digits), per-stage timestamp records, clock-offset estimation over the control channel, percentile report writer, simulated-trigger fixture | `crates/lumen-bench/src/{lib.rs,pattern.rs,record.rs,report.rs,trigger.rs}` |
| M1.19 | Docs: `docs/protocol.md` (authored before transport code merges), `docs/design.md` skeleton with stage budgets | `docs/protocol.md`, `docs/design.md` |

Dependencies and parallelism: M1.1 → {M1.2, M1.3, M1.4}; M1.4 → M1.5; M1.6 → {M1.9, M1.12}; M1.7, M1.8, M1.10, M1.11, M1.14 in parallel with each other; M1.13 parallel with M1.9; M1.15 parallel with M1.12; M1.16 needs M1.1–M1.15; M1.17 needs M1.12 and M1.15; M1.18 parallel from M1.1; M1.19 must land before M1.4 merges.

Acceptance (M1):
```
cargo build --workspace --release
cargo test --workspace
scripts/run-e2e.sh --host-ns lumen-host --client-ns lumen-client \
    --width 1920 --height 1080 --fps 60 --duration 60 --report artifacts/m1/e2e.json
cargo run -p lumen-bench --bin lumen-bench -- report --input artifacts/m1/e2e.json \
    --assert frames_decoded>1700 --assert decode_errors==0 --assert dropped_frames==0 \
    --assert p50_present_ms<=8 --assert p99_present_ms<=12 --provisional
```
`e2e.json` must contain: frames_sent, frames_decoded, decode_errors, dropped_frames, fec_recoveries, retransmits, bitrate_kbps, and per-stage percentiles for capture, encode, send, recv, decode, present. The asserted percentiles are **software pipeline latency** (capture → present-ack), not glass-to-glass; the report writer stamps them `provisional: true` and `docs/benchmarks.md` states the gap to the photodiode floor is pending.

### M2 — Transport with FEC and loss simulation

| # | Task | Files |
|---|---|---|
| M2.1 | Slice layer: split each access unit into ≤1200-byte transport slices, per-slice CRC32C, decoder-start rule (start when the AU's last needed slice is present) | `crates/lumen-protocol/src/slice.rs`, `crates/lumen-transport/src/slice.rs` |
| M2.2 | Deadline-aware retransmission: NACK over the control channel, retransmit only while `now < frame_deadline` (deadline = capture_ts + 1.5× frame interval), never block the pipeline on a late packet; late frames are dropped and an IDR request is queued | `crates/lumen-transport/src/{retransmit.rs,nack.rs}` |
| M2.3 | Adaptive FEC: parity ratio from measured loss (0 % → none, <2 % → 2 per 8, ≥2 % → 3 per 8), block size from measured packet size, overhead capped at 25 % | `crates/lumen-transport/src/fec.rs` |
| M2.4 | Bandwidth adaptation: target bitrate from delay-gradient plus loss estimate; step up ≤10 % per window when loss <1 % and queue empty, multiplicative decrease when loss >2 % | `crates/lumen-transport/src/estimator.rs` |
| M2.5 | Loss/jitter/reorder injection in-process for tests, plus `tc netem` driver for system-level runs | `crates/lumen-transport/src/inject.rs`, `scripts/netem-run.sh` |
| M2.6 | QUIC candidate transport behind cargo feature `quic`: `quinn` datagrams carrying the same packet format | `crates/lumen-transport/src/quic.rs`, workspace `Cargo.toml` feature |
| M2.7 | Property tests for framing and FEC (proptest); fuzz targets `fec_reassemble` and `transport_inject` | `crates/lumen-transport/tests/`, `fuzz/fuzz_targets/{fec_reassemble.rs,transport_inject.rs}` |
| M2.8 | Publish degradation curves at 0.5 %, 2 %, 5 % loss and 10 ms jitter → CSV plus curve in `docs/benchmarks.md` | `scripts/sweep-loss.sh`, `docs/benchmarks.md` |

Acceptance (M2):
```
cargo test -p lumen-transport -p lumen-protocol
cargo fuzz run fec_reassemble -- -max_total_time=120
cargo fuzz run protocol_parser -- -max_total_time=120
scripts/sweep-loss.sh --levels 0.005,0.02,0.05 --jitter 10ms --out artifacts/m2/curve.csv
test -f artifacts/m2/curve.csv && grep -q "0.005" artifacts/m2/curve.csv
```
Recorded criterion: at 2 % loss, delivered frame rate stays ≥55 fps at 1080p60 and p99 pipeline latency ≤25 ms; the actual numbers are written into `docs/benchmarks.md`, not asserted in prose.

### M3 — Input path

| # | Task | Files |
|---|---|---|
| M3.1 | Input messages: keyboard (keycode + state), relative and absolute pointer, buttons, scroll, touch, pen, gamepad axes/buttons; compact fixed-size encoding | `crates/lumen-protocol/src/input.rs` |
| M3.2 | FFI crate libei: dlopen `libei.so.1`, safe wrappers for EIS receiver (device regions, events) and EI sender (injection) | `crates/lumen-ffi-libei/src/{lib.rs,sys.rs,receiver.rs,sender.rs}` |
| M3.3 | Client capture (Wayland): EIS over the RemoteDesktop portal, xkbcommon keymap translation, relative pointer and pointer constraints, `zwp_text_input` IME | `crates/lumen-input-wayland/src/{lib.rs,eis.rs,keyboard.rs,pointer.rs,touch.rs,ime.rs}` |
| M3.4 | Client capture (X11): XI2 raw events with `XI_RawMotion`, keymap via xkbcommon | `crates/lumen-input-x11/src/{lib.rs,raw.rs,keyboard.rs}` |
| M3.5 | Host injection: EI sender backend when the compositor supports it, else uinput virtual keyboard, relative and absolute pointer, touch and pen (absolute axis scaled to the client resolution, `UI_SET_PHYS`/`UI_SET_SWB`), uhid gamepad passthrough, XTest for X11 hosts | `crates/lumen-input-inject/src/{lib.rs,uinput.rs,uhid.rs,xtest.rs,ei_sender.rs}` |
| M3.6 | Input path isolation: dedicated thread and socket, never coalesced behind video, sent on arrival | `crates/lumen-host-service/src/input.rs`, `crates/lumen-transport/src/input_channel.rs` |
| M3.7 | Input latency measurement: timestamp at client event receipt and at host injection call, map host clock through the control channel, report p50/p99 excluding the client's own polling interval | `crates/lumen-bench/src/input_latency.rs` |

Acceptance (M3):
```
cargo test -p lumen-input-wayland -p lumen-input-x11 -p lumen-input-inject
scripts/run-input-latency.sh --duration 60 --out artifacts/m3/input.json
```
`input.json` reports p50/p99 host-injection latency measured on the build machine over the netns pair. The spec's p99 < 1 ms is a hardware-tier target; the measured value and a provisional marker go into `docs/benchmarks.md`.

### M4 — Audio, clipboard, multi-monitor, resolution changes

| # | Task | Files |
|---|---|---|
| M4.1 | Audio capture: per-session PipeWire virtual sink plus monitor capture of that sink at 48 kHz | `crates/lumen-audio/src/{lib.rs,capture.rs,sink.rs}` |
| M4.2 | Opus encode/decode at 2.5, 5 and 10 ms frames, restricted-low-delay, CBR, plus raw PCM mode; bidirectional streams on separate sockets | `crates/lumen-audio/src/{opus.rs,pcm.rs}` |
| M4.3 | A/V sync: audio packets carry the capture timestamp of the video frame they align to; the client schedules playback against the video clock | `crates/lumen-audio/src/sync.rs`, `crates/lumen-protocol/src/audio.rs` |
| M4.4 | Clipboard (Wayland): `zwlr_data_control_manager_v1` on the host, `wl_data_device` on the client, UTF-8 payloads, 1 MiB cap | `crates/lumen-host-service/src/clipboard.rs`, `crates/lumen-client-desktop/src/clipboard.rs` |
| M4.5 | Clipboard (X11): CLIPBOARD and PRIMARY selection conversion | `crates/lumen-capture-x11/src/clipboard.rs` |
| M4.6 | Multi-monitor: host enumerates outputs (wl_output / XRandR), client receives the monitor list, per-monitor scaling, one encoded stream per monitor | `crates/lumen-host-service/src/monitors.rs`, `crates/lumen-client-desktop/src/monitors.rs` |
| M4.7 | Resolution changes: host reconfigures the encoder on output mode change and notifies the client; client reallocates surfaces; 1080p→1440p→1080p exercised mid-stream | `crates/lumen-host-service/src/resize.rs`, `crates/lumen-render/src/resize.rs` |
| M4.8 | HDR and 10-bit plumbing: P010 support, `dynamic_range` capability bit, HDR metadata transport (colourimetry, max mastering display luminance) | `crates/lumen-protocol/src/capability.rs`, `crates/lumen-encode-vaapi/src/config.rs` |

Acceptance (M4):
```
cargo test -p lumen-audio
scripts/run-e2e.sh --with-audio --with-clipboard --monitors 2 --duration 60 --report artifacts/m4/e2e.json
```
`e2e.json` expectations: audio_packets > 0, audio_underruns == 0, a_vs_skew_ms p99 ≤ 20, clipboard_roundtrip_ms p99 ≤ 50, resize_events == 2, decode_errors == 0.

### M5 — Hardware decode validated on the low-power client tier

| # | Task | Files |
|---|---|---|
| M5.1 | Decode backend selection: capability probe (VA-API profiles, Vulkan Video, NVDEC) with fallback chain VA-API → Vulkan Video hardware → Vulkan Video software (lavapipe) → CPU | `crates/lumen-decode-vaapi/src/probe.rs`, `crates/lumen-decode-vulkan/src/probe.rs`, `crates/lumen-client-desktop/src/decode_select.rs` |
| M5.2 | Low-power profile: single-threaded decode, depth-1 frame queue, width guard, sustained 1080p60 soak | `crates/lumen-client-desktop/src/lowpower.rs` |
| M5.3 | Tier B proxy script: client pinned to two vCPUs with `taskset -c 0,1`, decodes 1080p60 for N minutes, reports decode percentiles and CPU | `scripts/run-tier-b.sh` |
| M5.4 | Tier B hardware protocol: exact procedure for a Celeron N4020-class device with Intel UHD 600 VA-API decode | `docs/known-limits.md`, `docs/benchmarks.md` |

Acceptance (M5):
```
cargo test -p lumen-decode-vaapi -p lumen-decode-vulkan
scripts/run-tier-b.sh --cpus 0,1 --duration 300 --report artifacts/m5/tierb.json
```
`tierb.json` expectations: decode p99 ≤ 12 ms, no frame drops for 300 s, CPU ≤ 85 % of two cores. The Tier B glass-to-glass targets (p50 ≤ 20 ms, p99 ≤ 30 ms) stay **PENDING** until a Celeron N4020-class device exists; `docs/benchmarks.md` marks them pending and reports the two-vCPU proxy separately.

### M6 — Session persistence, unattended reconnect, virtual displays for headless hosts

| # | Task | Files |
|---|---|---|
| M6.1 | Host session persistence: session survives client disconnect (encode paused, state kept, configurable timeout) and compositor restart (capture reopened, IDR requested) | `crates/lumen-host-service/src/persist.rs` |
| M6.2 | Unattended reconnect: client retries with 200 ms → 2 s backoff, full reconnect ≤2 s measured from link-up | `crates/lumen-client-desktop/src/reconnect.rs` |
| M6.3 | Restore token persistence: portal ScreenCast restore token stored per host and reused, so sessions run unattended after the first consent | `crates/lumen-capture-pipewire/src/portal.rs` |
| M6.4 | Virtual display evaluation and implementation: sway headless output, Xorg dummy driver, DRM EDID override; pick the lowest-privilege option with controllable resolution and refresh, and document the comparison | `crates/lumen-host-service/src/virtual_display.rs`, `docs/design.md` |
| M6.5 | Multiple clients and shadow mode: N concurrent sessions (one encoder each, configurable), shadow client receives video without input rights | `crates/lumen-host-service/src/multi.rs` |
| M6.6 | Network flap resilience: session survives interface down/up, client reconnects ≤2 s | `scripts/run-wifi-flap.sh` |

Acceptance (M6):
```
cargo test -p lumen-host-service
scripts/run-reconnect.sh --cycles 20 --out artifacts/m6/reconnect.json
scripts/run-compositor-restart.sh --cycles 5 --out artifacts/m6/restart.json
```
Expectations: 20/20 cycles reconnected with max reconnect ≤2000 ms; 5/5 sessions recovered with a stable open-fd count (no leaked encoder sessions).

### M7 — X11 host and client

| # | Task | Files |
|---|---|---|
| M7.1 | X11 host capture: DRI3 (pixmap from fd) → XShm → XDamage ladder, cursor via XFixes | `crates/lumen-capture-x11/src/{lib.rs,dri3.rs,shm.rs,damage.rs,cursor.rs}` |
| M7.2 | X11 client present: x11rb window with Present extension, plus MIT-SHM blit fallback, fullscreen unredirect path | `crates/lumen-render/src/x11.rs` |
| M7.3 | X11 client input and injection: XI2 raw events on the client, XTest injection on X11 hosts | `crates/lumen-input-x11/src/`, `crates/lumen-input-inject/src/xtest.rs` |
| M7.4 | Backend auto-selection matrix (Wayland/Wayland, Wayland/X11, X11/Wayland, X11/X11) with `--capture` and `--present` overrides | `crates/lumen-host-service/src/backend.rs`, `crates/lumen-client-desktop/src/backend.rs` |
| M7.5 | X11 test environment: Xvfb plus Xorg dummy driver for host and client displays | `scripts/x11-env.sh` |

Acceptance (M7):
```
cargo test -p lumen-capture-x11 -p lumen-input-x11
DISPLAY=:77 scripts/run-e2e.sh --x11 --duration 60 --report artifacts/m7/e2e-x11.json
```
Expectation: frames_decoded > 0, decode_errors == 0, per-stage percentiles present.

### M8 — Hardening, fuzzing, soak, chaos tests

| # | Task | Files |
|---|---|---|
| M8.1 | Fuzz every parser: `protocol_parser`, `fec_reassemble`, `transport_inject`, `h264_au_split`, `input_parser`, `opus_frame` | `fuzz/fuzz_targets/*.rs`, `.github/workflows/fuzz.yml` |
| M8.2 | CI gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check`, `cargo audit`, `cargo test --workspace`, fuzz smoke 60 s per target | `.github/workflows/ci.yml` |
| M8.3 | Soak: longest run the environment allows (target 72 h; the run budget allows ~2 h) with resolution changes, compositor restarts, suspend/resume; the length actually run is recorded and the remainder marked pending | `scripts/soak.sh`, `docs/benchmarks.md` |
| M8.4 | Chaos: random loss, jitter and reorder injection, encoder restart, decoder device loss, throttling simulation, netns teardown | `scripts/chaos.sh` |
| M8.5 | Baselines: Sunshine + Moonlight-Qt, RustDesk (both sides), GRD in RDP mode against `xfreerdp` — matched resolution, refresh, bitrate class and codec where comparable; every difference documented; raw data committed | `scripts/baseline-{sunshine,rustdesk,grd}.sh`, `docs/benchmarks.md` |
| M8.6 | Photodiode and camera harness: rig procedure, capture loop (v4l2-ctl to file), barcode decode, glass-to-glass computation, display-floor measurement; validated end to end with the simulated trigger | `harness/README.md`, `harness/photodiode/`, `crates/lumen-bench/src/camera.rs` |
| M8.7 | Final docs: `docs/design.md` (stage budgets, decisions with reversibility), `docs/protocol.md` final, `docs/benchmarks.md`, `docs/known-limits.md`, `docs/final-report.md` | `docs/*` |

Acceptance (M8):
```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check && cargo audit
cargo test --workspace
for t in protocol_parser fec_reassemble transport_inject h264_au_split input_parser; do
  cargo fuzz run $t -- -max_total_time=60 || exit 1
done
scripts/soak.sh --duration 7200 --out artifacts/m8/soak.json
scripts/chaos.sh --duration 900 --out artifacts/m8/chaos.json
test -f docs/design.md docs/protocol.md docs/benchmarks.md docs/known-limits.md docs/final-report.md
```

---

## Measurement plan

Definitions. **Pipeline latency** = host capture timestamp → client present-ack timestamp, both in software, with the host/client clock offset estimated over the control channel. **Glass-to-glass** = a pixel changing in the host's presented output → the corresponding pixel changing on the client panel; only a photodiode and a high-speed camera can measure it, and it needs a real second machine, a real display and a camera rig. Every number is labelled with which of the two it is.

| Target | Method | Hardware required | Status here |
|---|---|---|---|
| Tier A 1080p60 wired, glass-to-glass p50 ≤ 8 ms, p99 ≤ 12 ms | photodiode + ≥1000 fps camera; frame-counter barcode matched to the host timestamp | second machine, wired GbE, photodiode rig, 1080p60 panel | **PENDING** — harness software built and validated with the simulated trigger; entry marked pending in `docs/benchmarks.md` |
| Tier A 1080p60 wired, pipeline latency | per-stage timestamps in `lumen-bench` | none beyond this machine (netns pair) | **MEASURED PROVISIONAL** — M1/M2 artifacts |
| Tier B 1080p60, p50 ≤ 20 ms, p99 ≤ 30 ms | same rig, client on Celeron N4020 | Tier B device | **PENDING**; two-vCPU proxy recorded separately in M5 |
| 4K60 Tier A, p50 ≤ 12 ms | photodiode rig | 4K panel + second machine | **PENDING** |
| 1440p120 Tier A, p50 ≤ 8.33 ms + 3 ms | photodiode rig | 1440p144 Hz panel + second machine | **PENDING** |
| Input: client event receipt → host injection, p99 < 1 ms | client event timestamp → host injection call timestamp, host clock mapped via the control channel | none (netns pair) | **MEASURED PROVISIONAL** in M3 |
| Display's own latency (the floor) | photodiode on a known-latency panel plus a reference loop | rig + panel | **PENDING** |
| Compositor overhead versus direct scanout | present-ack versus DRM page-flip timestamp on the same frame, `wp_presentation` plus DRM timing | compositor with direct-scanout support | **MEASURED PROVISIONAL** on sway and Xvfb; fullscreen-unredirect comparison in M7 |
| Degradation curves at 0.5 / 2 / 5 % loss and 10 ms jitter | `tc netem` inside netns; delivered fps and latency percentiles | none | **MEASURED** in M2 |
| Crypto cost | AEAD bytes/s and CPU % at target bitrates | none | **MEASURED** in M1/M8 |
| Baseline comparison | same harness against Sunshine+Moonlight, RustDesk, GRD | baseline binaries runnable; glass-to-glass numbers need the rig | **MEASURED PROVISIONAL** for software-visible latency; glass-to-glass baseline **PENDING** |

Stage budget, documented in `docs/design.md` and instrumented in `lumen-bench`: capture ≤1.5 ms, GPU colour convert ≤0.3 ms, encode ≤2.0 ms, packetize+FEC+pacing ≤0.4 ms, LAN one-way ≤0.2 ms, reassemble ≤0.3 ms, decode ≤2.0 ms, present-ack ≤1.0 ms — about 7.7 ms for Tier A. These are design targets; measured values and deviations go to `docs/benchmarks.md`.

---

## Decisions to be made from data

Each decision names the experiment that decides it. The winner, the measurement that decided it, the chosen value and whether it is reversible are recorded in `docs/design.md`.

| ID | Decision | Deciding experiment | Default if the experiment cannot run |
|---|---|---|---|
| D-CAPTURE | Capture path per compositor: portal+PipeWire versus direct wlr-screencopy versus KMS/DRM versus X11 | same content, 300 frames per path, compare capture→encode-ready p50 and frame drops | portal+PipeWire (spec-mandated); wlr-screencopy when portal is unavailable |
| D-ENC-VENDOR | Encoder per vendor: VA-API versus NVENC versus Vulkan Video | encode 1080p60 of the synthetic pattern; measure encode p50/p99 and bitrate at SSIM ≥ 0.98 against the source | NVIDIA → NVENC, Intel/AMD → VA-API; Vulkan Video measured only |
| D-ENC-TUNE | Preset and rate control per vendor (CQP versus CBR, GOP policy, IDR cadence) | per-vendor grid search, quality/bitrate/latency Pareto front from bench artifacts | CQP 22, infinite GOP, on-demand IDR |
| D-TRANSPORT | QUIC datagrams via `quinn` versus custom UDP with FEC | `scripts/sweep-loss.sh` with the `quic` feature on and off at 0.5/2/5 % loss and 10 ms jitter; compare p99 delivered latency, CPU %, recovery rate | custom UDP (FEC and pacing are ours anyway; QUIC's congestion control would fight our deadline model) |
| D-PRESENT | Present policy: `wl_shm` versus `linux-dmabuf`+EGL, queue depth 1 versus 2, FIFO versus immediate/MAILBOX | present-ack latency, dropped frames and CPU per configuration over 300 frames | linux-dmabuf+EGL, depth 1, no frame throttling |
| D-JITTER | Jitter buffer depth, target 0 ms | inject 0/5/10/20 ms jitter with and without a one-frame buffer; compare p99 pipeline latency and drop rate | depth 0: decode on arrival, drop late frames |
| D-SCANOUT | Direct scanout per compositor (Wayland fullscreen, X11 unredirect) | compare present-ack against page-flip timestamps, fullscreen versus windowed, on sway and Xvfb | document availability per compositor; composited path stays the default |
| D-444 | Whether 4:4:4 is the default | text/UI crop set: SSIM and PSNR of 4:2:0 versus 4:4:4 at equal bitrate and at equal quality, plus measured chroma bleed on coloured text | 4:2:0 default with 4:4:4 as a per-session option, matching Moonlight and Sunshine behaviour |
| D-CODEC | Codec ladder H.264 versus HEVC versus AV1 | HW support matrix from `vainfo`/`vulkaninfo`, bitrate at matched SSIM, decode latency on the lowest-power path | H.264 (universal HW support), HEVC optional, AV1 decode-only |
| D-AUDIO-FRAME | Opus frame size 2.5 versus 5 ms | audio round-trip latency and CPU at each size | 5 ms |
| D-VIRT-DISPLAY | Virtual display for headless hosts: sway headless versus Xorg dummy versus DRM EDID override | compare resolution and refresh control, privilege level, capture-path availability | sway headless output (lowest privilege) |

---

## Risks and fallbacks (pre-decided, so execution never stalls)

| Risk | Pre-decided fallback |
|---|---|
| `pipewire` Rust crate 0.10.1 is unstable (known use-after-free issue #121, RT callback `Send` issues) | capture falls back to `lumen-capture-wlr` (direct wlr-screencopy DMA-BUF); the PipeWire path stays behind feature `pipewire-capture` and is revisited upstream |
| `libva` crate 0.1.5 (BSD-3, dlopen) lacks a needed entry point | generate crate-local bindgen bindings against system `libva-dev` headers inside `lumen-ffi-va`; bindgen is a build-time tool, no C enters the product |
| `nvenc` crate 0.1.0 is immature and lacks external-memory input | use NVENC host-memory input (`NV_ENC_MEM_HOST`) with a documented CPU copy on the encode input path only, or fall back to VA-API through `nvidia-vaapi-driver`; D-ENC-VENDOR then picks the winner |
| CUDA runtime unavailable for NVENC external memory | as above; cudarc dynamic loading already avoids a CUDA toolkit dependency |
| No photodiode rig, no second machine, no Tier B device | every glass-to-glass and Tier B number marked **PENDING**; pipeline-latency and degradation numbers measured here marked **PROVISIONAL**; rig software validated with the simulated trigger |
| Portal consent cannot be granted unattended | use `lumen-capture-wlr`; restore token implemented in M6 makes later runs unattended |
| `tc netem` needs root | run tests inside `sudo ip netns exec` (sudo is passwordless here); the in-process injector is the fallback for CI without root |
| 72 h soak exceeds the run budget | run 2 h, record the exact length, mark the remainder pending |
| crates.io or rustup network flakiness | `cargo vendor` into `vendor/` with a committed `Cargo.lock`; `scripts/env-setup.sh` retries with backoff |
| `cargo deny` flags a transitive copyleft dependency | replace the dependency; if impossible, pin the last permissive version and document the exception in `docs/design.md`. No exception is granted for GPL/AGPL code in the shipped binary |
| Vulkan Video unsupported on the installed drivers | the Vulkan path stays feature-gated and unused; VA-API and NVENC cover encode and decode |
| Compositor lacks direct scanout or `wl_screencopy` v2 | record that measurement as "unavailable" naming the compositor, never as a silent pass |
| Clippy, deny or audit failures on the pinned toolchain | fix in-tree; only a `cargo deny` licence advisory may be downgraded, with a written justification in `docs/design.md` |

---

## Critical files & anchors

- `crates/lumen-protocol/src/header.rs` — the 32-byte media header and capability bitmap; every other crate depends on this exact layout, and the AEAD AAD covers it.
- `crates/lumen-transport/src/estimator.rs` — delay-gradient and loss bandwidth estimate plus FEC parity selection; the single place that decides rate and redundancy.
- `crates/lumen-capture-pipewire/src/stream.rs` — PipeWire DMA-BUF negotiation and `spa_meta_header` capture timestamps; the source of the capture clock behind every latency number.
- `crates/lumen-encode-vaapi/src/session.rs` — VA-API encode session (low-latency flag, zero reorder frames, infinite GOP, on-demand IDR); the reference for applying D-ENC-TUNE on every vendor.
- `crates/lumen-render/src/presentation.rs` — present path and `wp_presentation`/`wp_commit_timing` feedback; where D-PRESENT and D-SCANOUT are measured.

---

## Verification

End-to-end proof, run from the repo root after `scripts/env-setup.sh`:

1. Build and lint gate: `cargo build --workspace --release && cargo clippy --workspace --all-targets -- -D warnings && cargo deny check && cargo audit`.
2. Unit and property tests: `cargo test --workspace` — protocol golden vectors, FEC property tests, estimator tests, capability-negotiation tests.
3. Fuzz: `cargo fuzz run <target> -- -max_total_time=60` for each of the five parsers (M8.1).
4. Live two-endpoint run: `scripts/run-e2e.sh --width 1920 --height 1080 --fps 60 --duration 60 --report artifacts/m1/e2e.json` inside the netns pair. Expected observable output: frames_decoded > 1700, decode_errors == 0, dropped_frames == 0, non-empty per-stage percentiles, bitrate_kbps within ±15 % of the configured target.
5. Loss behaviour: `scripts/sweep-loss.sh --levels 0.005,0.02,0.05 --jitter 10ms --out artifacts/m2/curve.csv`. Expected: delivered fps degrades monotonically with loss, no crash, no unbounded queue growth (`queue_depth_max <= 2`).
6. Input latency: `scripts/run-input-latency.sh --duration 60 --out artifacts/m3/input.json`. Expected: p99 host-injection latency reported with the client polling interval excluded.
7. Reconnect: `scripts/run-reconnect.sh --cycles 20`. Expected: max reconnect ≤ 2000 ms.
8. Baseline comparison: `scripts/baseline-sunshine.sh`, `scripts/baseline-rustdesk.sh`, `scripts/baseline-grd.sh`. Expected: same-harness artifacts per baseline, each difference (codec, resolution, fps, bitrate, transport, compositor) written beside the numbers.
9. Rig path in software: `cargo run -p lumen-bench --bin lumen-bench -- rig-selftest --fixture harness/fixtures/simulated-trigger.json`. Expected: the barcode decoder recovers the frame counter and host timestamp from the simulated camera frames and computes a glass-to-glass estimate equal to the injected trigger latency within 1 ms — proving the harness math. The physical measurement stays pending.

## Assumptions and contingencies

- Working name "Lumen", crate prefix `lumen-`, binaries `lumen-host`, `lumen-client`, `lumenctl`, `lumen-bench`, licence dual MIT OR Apache-2.0, repository at `/home/hayden/Desktop/projects/lumen`. Reversible: rename before any publish.
- Host and client both run Linux on the same LAN; the build machine stands in for both endpoints through `ip netns` veth pairs, so every "LAN" number here is a loopback-plus-veth number rather than copper. Marked provisional.
- Both local GPUs are exercised: NVIDIA GTX 1650 for the NVENC path, AMD Radeon for the VA-API path; the software decode path uses `lavapipe` through Vulkan Video.
- If a compositor under test lacks direct scanout or `wl_screencopy` v2, the affected measurement is recorded as "unavailable" with the compositor named, not as a failure.
- If the network is unreachable during execution, `scripts/env-setup.sh` retries and the run continues with the software and VA-API paths only; NVENC-specific tasks are then recorded as blocked-by-network rather than silently skipped.
- The owner's prompt approves the scope and structure of this plan. Every decision that an owner might want to review is recorded in `docs/design.md` with its reason and whether it is reversible, and execution proceeds without further questions.
