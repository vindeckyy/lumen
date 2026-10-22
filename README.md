# Lumen

**Lumen** is a LAN-only remote desktop system for Linux, written in Rust. A host
captures the screen, encodes it with hardware H.264, and streams it over the
local network to a client that decodes and presents it. Input events travel
back over a separate channel so a lost video packet never stalls the keyboard.

Everything is first-party Rust. No C or C++ source is compiled into the
product; the only native code linked is OS-provided system libraries reached
through Rust bindings, and each native boundary is owned by exactly one crate.

**Status: in development.** M0 (environment bootstrap) is complete, and M1 to
M7 each have a measured run: the pipeline, the loss sweep, the input
measurement, the audio and clipboard run, the two-vCPU proxy, the session
resilience runs and the X11 row. The transport is adaptive, the cipher is
negotiated and the audio path is wired. M8 is partly done: the soak and the
chaos runs are short and the remainder is pending. Several crates are still
stubs, and no glass-to-glass latency has been measured. See
[Current status](#current-status),
[Not implemented yet](#not-implemented-yet) and
[Status of measurements](#status-of-measurements) before relying on anything
here.

---

## What it is

- Linux only, Wayland first and X11 second, on both ends of the connection.
- LAN only. There is no relay, no cloud component and no internet path.
- The shape is a host daemon (`lumen-host`, in `crates/lumen-host-service`) and
  a client application (`lumen-client`, in `crates/lumen-client-desktop`). Both
  binaries exist and run the media pipeline end to end. The control utility
  (`lumenctl`) is not written yet.
- Hardware video encode and decode through VA-API, with a fallback path for
  software correctness testing.
- An adaptive transport: the FEC parity level follows the measured loss, the
  encoder's bitrate follows the measured loss and delay, and the transport
  cipher is negotiated inside the handshake.
- Audio travels with the video, on a socket of its own and under a key of its
  own, and the client plays it aligned against the video clock.
- No copyleft dependencies. `cargo deny` enforces a permissive licence allow
  list and rejects GPL, AGPL, LGPL, CDDL, EPL, SSPL and WTFPL.

## Architecture

```
  HOST                                                    CLIENT
 ┌───────────────────────────────────┐        ┌──────────────────────────┐
 │ screen capture                     │        │                          │
 │  wlroots screencopy (DMA-BUF)     │        │  VA-API H.264 decode     │
 │  or X11 DRI3 → XShm → XDamage      │        │        │                 │
 │            │                       │        │        ▼                 │
 │            ▼                       │        │  present                 │
 │  VA-API H.264 encode               │        │   Wayland xdg-shell      │
 │  NV12, low-latency, no B-frames,   │        │   (wl_shm / linux-dmabuf │
 │  infinite GOP, on-demand IDR       │        │    + EGL)                │
 │            │                       │        │   or X11 present         │
 │            ▼                       │        │        ▲                 │
 │  packetize → slice → adaptive FEC  │        │        │                 │
 │            │                       │        │  reassemble → decode     │
 └────────────┼───────────────────────┘        └─────────┼─────────────────┘
              │                                         │
              │  media: UDP datagrams, adaptive RS FEC   │
              │  parity, token-bucket pacing,           │
              │  deadline-aware selective retransmission│
              │                                         │
              └────────► ──────────  LAN  ───────────────┘

  input:  client capture (X11 XI2) → UDP datagram on a socket of its own
          (per-direction AEAD counter) → host injection
          (uinput / XTest / EI sender)

  audio:  host PipeWire virtual sink → capture → Opus → UDP datagrams on a
          port of its own (24802) under a key of its own → client playback,
          aligned against the video clock
```

- **Capture (host).** The screen is grabbed as DMA-BUF planes. The wlroots
  `zwlr_screencopy_manager_v1` path is implemented and verified live under
  headless sway; the X11 path walks a DRI3 → XShm → XDamage ladder with the
  cursor tracked via XFixes. The xdg-desktop-portal/PipeWire path is a stub.
- **Encode (host).** VA-API H.264 High profile in NV12, with
  `VAConfigAttribLowLatency`, `max_num_reorder_frames = 0`,
  `num_ref_frames = 1`, an infinite GOP and IDR on request. CQP is the default
  rate control; CBR is selectable. Only H.264 is implemented.
- **Media transport.** Video is split into ≤1200-byte slices and carried over
  UDP. The FEC level is **adaptive**: the parity level follows the measured
  loss, with no parity below 0.5 % loss, 2 parity slices per block of 8 data
  slices below 2 %, and 3 parity slices per block of 12 data slices at or above
  it. The heavy level uses twelve data slices so that three parity slices stay
  inside the 25 % overhead cap. The block size travels in the media header's
  `flags` word, so the receiver maps a slice to its block without being told
  the level. The parity is Reed-Solomon over GF(2^16)
  (Leopard-RS, from `reed-solomon-simd`, as in punktfunk), so **as many losses
  inside one block as it has parity slices are recoverable**. A token-bucket pacer paces
  datagrams, and a deadline-aware retransmission queue does selective
  recovery. A retransmission is bounded by the frame's deadline: the capture
  timestamp plus 1.5 frame intervals. The client generates the NACKs that drive
  the queue. The parity level is **asymmetric**: it rises at once when the
  measured loss asks for it and falls only after six windows without a loss,
  so the stream does not lose its protection exactly when the link is about to
  get worse. The client reports the slices it received and the slices it lost
  every 100 ms over the control channel, the host differences those cumulative
  counts, and the estimator's window is 500 ms.
- **Decode and present (client).** VA-API H.264 decode into a surface pool,
  with CPU NV12 as the default output and DMA-BUF output available on request.
  Presentation is through a Wayland xdg-shell surface using `wl_shm` or
  `linux-dmabuf` with EGL, with `wp_presentation`, `wp_commit_timing` and
  `wp_tearing_control` bound; an X11 present path exists. Decoded NV12 cannot
  be imported by the renderer yet, so the present path is currently
  decode → CPU convert → `wl_shm`.
- **Control channel.** A TCP connection carries a Noise XX handshake
  (`Noise_XX_25519_ChaChaPoly_SHA256`) followed by length-prefixed, encrypted
  control records. The handshake secret is expanded with HKDF-SHA256 into the
  per-direction session keys — `control_h2c`, `control_c2h`, `media_h2c`,
  `media_c2h`, and the audio pair `audio_h2c` and `audio_c2h` — so no key is
  ever used in both directions with the same counter, and the audio can never
  share a nonce with the video. The transport cipher is **negotiated inside
  the handshake**: the capability bitmap travels as a handshake payload, so a
  peer without AES-NI gets ChaCha20-Poly1305 instead of AES-256-GCM, and the
  control channel stays encrypted from its first byte. Every record is sealed
  with the negotiated AEAD using a per-direction counter, and the media header
  carries its sequence number as authenticated associated data.
- **Input channel.** Client-side input capture is sealed with AEAD under a
  session key and sent as its own UDP datagram, on a socket of its own, so
  input is never coalesced behind the video and never head-of-line blocked by
  a lost video slice. The input wire format carries a monotonic sequence
  number and a 64-entry replay window, because AEAD alone would still let a
  captured keystroke be replayed. On the host the events are injected through
  XTEST, the wlroots virtual input protocols, uinput or an EI sender. The X11
  capture crate reads the raw XI2 events of the X server and maps the keymap
  through xkbcommon. Gamepads ride the same socket: the client reads every
  evdev gamepad of its machine and sends its whole state on each change and
  every 100 ms, and the host gives each one a virtual DualSense through
  `/dev/uhid`. What the game asks of those pads' motors comes back over the
  control channel, and the client plays it on the pad of its own machine.
- **Audio.** The host creates a PipeWire virtual sink for the session,
  captures it, encodes it as Opus and sends it on a UDP port of its own
  (24802 by default), sealed with a key pair of its own, so the audio can
  never share a nonce with the video. The client reassembles the frames,
  places them against the video clock and plays them. The A/V skew is
  measured as the error in the alignment: the difference between the moment
  an audio frame was played and the moment the video frame of the same moment
  was presented. It is only measured once that video frame has been presented
  and once enough frames have been presented for the alignment to be settled.
- **Clipboard.** The host reads the selection of its own compositor and
  offers it to the client; the client takes it and sends the text back, and
  that return is the round trip the host counts. The host can be told to put
  a given text into its own selection (`--clipboard-test`), so a run can
  exercise the path without a person copying something. The counter only
  counts a round trip when the client really took the text.
- **Bandwidth policy.** A bandwidth and loss estimator runs on the host. It
  uses two signals, the delay gradient and the loss rate: persistent loss
  cuts the target rate hard, a growing queue brings it down multiplicatively,
  and a healthy path probes upwards by at most 10 % per window. The policy
  drives the encoder's constant bitrate, and the encoder can change its rate
  control while it runs.
- **Pairing.** The host owns a long-term Ed25519 identity and keeps a store of
  the client keys it trusts. A client proves it owns its identity key by
  signing a nonce the host supplies during the handshake. A client the host
  does not trust yet is refused unless it proves, through SPAKE2, that it
  knows the six-digit code the host shows for that attempt; the code never
  crosses the wire, and the exchange is bound to the connection's Noise keys,
  so a relay in the middle cannot pair. The host signs every session and the
  client pins the host's key. With `--pairing refuse` the host turns every
  unknown client away. The host takes `--pairing-store <path>` and
  `--pairing prompt|refuse`; the client takes `--identity <path>`,
  `--pairing-code <code>` and `--pairing-code-file <path>`. The pairing store
  is written with owner-only permissions.
- **Pipelines.** The host runs four stages, each on its own thread: capture
  polls the compositor and converts every frame to NV12 on the CPU, encode
  runs the VA-API encoder and slices the access units into sealed datagrams,
  send paces the datagrams onto the media socket, and control answers the
  client and turns `RequestIdr` and `Nack` into commands for the encode
  stage. The client mirrors it with three threads: receive reads and
  reassembles the datagrams, decode and present decodes each access unit and
  hands the frame to the compositor, and control answers the host and carries
  the client's requests. The host also holds the session: it keeps the
  session alive when the client goes away, reopens the capture when the
  compositor restarts, enumerates the monitors, follows the resolution
  changes, carries the clipboard, serves more than one client at a time and
  runs the session's audio stage on its own socket.
- **Running the pipeline.** `scripts/run-e2e.sh` starts two headless sway
  compositors, runs `lumen-host` against one and `lumen-client` against the
  other, and merges the two counter reports into `artifacts/m1/e2e.json`.
  `MODE=loopback` puts both ends on 127.0.0.1; `MODE=netns` puts the host in
  the `lumen-host` namespace and the client in `lumen-client`, over the veth
  pair from `scripts/netns-up.sh`.

### Finding the host

The host advertises itself over mDNS, so a client finds it without being told
an address:

```
lumen-host --advertise 192.168.0.72:24800
lumen-client --discover
```

The advertisement carries the host's identity fingerprint, so two hosts on the
same network are told apart before a client pairs with either. A client that
knows the host can also wake it: `--wake <mac>` sends a wake-on-lan packet
before it connects.

### The desktop app

`lumen` is the client for people rather than scripts: a GTK 4 and libadwaita
window that lists the saved hosts and the ones answering on mDNS, adds and
edits hosts (with a MAC address for wake-on-lan), pairs through a dialog that
shows the host's fingerprint and asks for its six-digit code, and starts a
`lumen-client` for the session. It follows the desktop's light or dark style.
Failures are explained in plain words, with the client's own output under
Details. Closing the stream window ends the session; Ctrl+Shift+F toggles fullscreen.

```
cargo build --release -p lumen-client-gui -p lumen-client-desktop
target/release/lumen
```

The app looks for `lumen-client` next to itself, then on `PATH`;
`LUMEN_CLIENT` overrides both. Hosts and settings live in
`$XDG_CONFIG_HOME/lumen/client.json`, and each host's client identity and pin
in `$XDG_STATE_HOME/lumen/hosts/<id>/`. The desktop entry and icon are in
`crates/lumen-client-gui/data/`; copy them under `~/.local/share/` to put the
app in the menu.

### The control plane

The host serves a dashboard of its own live session and changes it while it
runs:

```
lumen-host --web-listen 127.0.0.1:8080
```

The host prints the URL, with a token in the fragment. The dashboard shows
the frame rate, the bitrate, the pipeline latency with a live chart, the
breakdown per stage, the encoder settings, the outputs, the sessions, the
pairing state and the host's log. It changes the rate control, the bitrate,
the frame rate, the parity level, the picture, the audio, ends a session,
revokes a client and asks for a keyframe.

The server is a small HTTP/1.1 implementation with no framework behind it
(`crates/lumen-web`), and the page is one file with its styles and its
script inside it, so the browser needs nothing from the network but the
host. It binds to a loopback address, every change has to carry the token
the host printed, and a request that names another origin is refused.

### The picture adapts

A client that cannot decode and present the picture it is given drops
frames, and a stream that drops frames is worse than a smaller stream that
does not. So the client says so: when it drops frames it asks the host for
the next size down, and the host scales the capture to it inside the colour
conversion, which costs less work rather than more. The operator can set
the picture from the dashboard, and that is the ceiling the client does not
cross.

Measured on the build machine at 1280x720: the capture stage takes 7.93 ms
and the whole pipeline 23.2 ms. At 640x360 the capture takes 6.97 ms and
the pipeline 16.0 ms, and the client uses 25 % less of a core.

### The window stays, the picture is scaled

The stream window is opened once and stays for the whole session, also
across a reconnect: a picture of a new size is scaled and placed inside the
window instead of a new window taking its place. The placement follows
punktfunk's rules — the whole picture with bars, filled and cropped, or
filled and stretched — and the client can hold one size whatever the link
does (`--resolution 1920x1080`).

The window carries a heads-up display with the picture size, the rate and
the round trip to the host. The client answers a few key combinations
itself instead of sending them on: Ctrl+Alt+Shift with Q holds the pointer
in the window, with D leaves the session, with H shows or hides the
display, and F11 or Alt+Enter (and Ctrl+Shift+F) switch fullscreen. On a
gamepad the Guide and the A are the same chord, and their buttons stay off
the wire.

A machine without a VA-API driver still shows the stream: the client falls
back to the OpenH264 software decoder, and the picture arrives as CPU
memory.

### The scripts

Every measurement has its own script, and every script writes its own
artifact. The committed artifacts are the record; the scripts are how to
repeat them.

| Script | What it measures | Artifact |
|---|---|---|
| `scripts/run-e2e.sh` | The end-to-end run, `MODE=loopback` or `MODE=netns` | `artifacts/m1/e2e.json`, `artifacts/m1/e2e-netns.json` |
| `scripts/run-e2e.sh --with-audio --with-clipboard --with-resize` | The M4 run: audio on the wire, a clipboard round trip and two resolution changes | `artifacts/m4/e2e.json` |
| `scripts/sweep-loss.sh` | The loss sweep at 0.5 %, 2 % and 5 % with 10 ms jitter | `artifacts/m2/curve.csv` |
| `scripts/run-input-latency.sh` | The input host-injection latency over the namespace pair | `artifacts/m3/input.json` |
| `scripts/run-tier-b.sh` | The low-power client profile on two pinned vCPUs, a proxy for the Tier B device | `artifacts/m5/tierb.json` |
| `scripts/run-reconnect.sh` | The unattended reconnect, 20 cycles | `artifacts/m6/reconnect.json` |
| `scripts/run-compositor-restart.sh` | The session across a compositor restart, 5 cycles | `artifacts/m6/restart.json` |
| `scripts/run-wifi-flap.sh` | The session across a network flap, 3 flaps | `artifacts/m6/wifi-flap.json` |
| `scripts/run-e2e-x11.sh` | The X11 host and X11 client row of the matrix | `artifacts/m7/e2e-x11.json` |
| `scripts/soak.sh` | The soak, with the disturbances of the plan | `artifacts/m8/soak.json` |
| `scripts/chaos.sh` | The chaos run, one fault per cycle | `artifacts/m8/chaos.json` |
| `scripts/netem-run.sh` | The `tc netem` driver for the namespace pair | — |
| `scripts/baseline-check.sh` | The baseline availability probe | `artifacts/m0/baselines.json` |

`scripts/x11-env.sh` brings the X server up and down for the X11 run,
`scripts/netns-up.sh` and `scripts/netns-down.sh` build the namespace pair,
`scripts/probe-caps.sh` writes the capability probe and `scripts/env-setup.sh`
installs the toolchain. The three `scripts/baseline-*.sh` runners drive the
baseline implementations; each of them found its binary missing, so no
baseline was run and no number was measured.

## Crate layout

The workspace has 23 member crates, all under `crates/`. Status is the honest
state of each crate at the time of writing.

| Crate | Responsibility | Status |
|---|---|---|
| `lumen-ffi-va` | libva bindings and safe wrapper: display/context/surface, DMA-BUF import and export | implemented |
| `lumen-ffi-nvenc` | NVENC API 12.1 bindings, loaded at runtime (libcuda.so.1, libnvidia-encode.so.1) | working |
| `lumen-protocol` | Wire format: 42-byte media header, message kinds, capability bitmap, slice descriptors, input codec, the 48-byte audio header, the scaled colour conversion | implemented |
| `lumen-transport` | UDP datagrams, token-bucket pacing, Reed-Solomon FEC with adaptive parity, deadline-aware retransmission, RTT/loss estimation, the link feedback, fault injection, the input channel | implemented |
| `lumen-crypto` | Noise XX handshake, HKDF key separation, Ed25519 device identity, SPAKE2 pairing bound to the connection, AEAD | implemented |
| `lumen-capture-pipewire` | xdg-desktop-portal ScreenCast → PipeWire capture, host flag `--capture portal` | working (shared memory) |
| `lumen-capture-wlr` | wlroots `zwlr_screencopy_manager_v1` DMA-BUF capture | implemented |
| `lumen-capture-x11` | X11 host capture: DRI3 → XShm → XDamage, XFixes cursor | implemented |
| `lumen-encode-vaapi` | VA-API H.264 encode (NV12, low-latency, infinite GOP, on-demand IDR, CQP/CBR) | implemented (H.264 only) |
| `lumen-encode-nvenc` | NVENC H.264 encode, host flag `--encoder nvenc` | working |
| `lumen-encode-vulkan` | Vulkan Video encode | stub |
| `lumen-decode-vaapi` | VA-API H.264 decode, DPB handling, CPU NV12 output with opt-in DMA-BUF | implemented (H.264 only) |
| `lumen-decode-vulkan` | Vulkan Video decode | stub |
| `lumen-render` | Client present path: Wayland xdg-shell, `wl_shm`, `linux-dmabuf` + EGL, presentation feedback, X11 present | partial |
| `lumen-input-wayland` | Client-side Wayland input capture, EIS source, sealed UDP sender | partial |
| `lumen-input-x11` | Client-side X11 input capture: XI2 raw motion, key press and key release, keymap through xkbcommon | implemented |
| `lumen-input-inject` | Host-side injection: uinput, wlroots virtual input, XTest, EI sender, replay window | partial |
| `lumen-audio` | PipeWire virtual sink and capture, Opus encode/decode, A/V sync, playback | implemented (wired into the host and the client sessions) |
| `lumen-host-service` | Host daemon: Noise session establishment, pairing, control channel, media sender and receiver, media pipeline, session persistence, multiple clients, clipboard, monitors, resolution change, backend selection, the control plane | implemented (pipeline, session and adaptive transport) |
| `lumen-client-desktop` | Client application: connect, pair, receive, decode, present, reconnect with backoff, low-power profile, event log, clipboard, monitors, the quality ladder | implemented (pipeline, session handling) |
| `lumen-client-gui` | The `lumen` desktop app (GTK 4, libadwaita): saved and discovered hosts, add and edit, pairing dialog, wake-on-lan, preferences, starts `lumen-client` per session | implemented |
| `lumen-bench` | Measurement harness: test pattern, per-stage records, report writer, camera analysis | partial, `lumen-bench` binary |
| `lumen-web` | The control plane: a dependency-free HTTP/1.1 server, the JSON API and the dashboard it serves | implemented |

`fuzz/` holds twelve `cargo-fuzz` targets. It is **excluded** from the workspace
(`exclude = ["vendor/opus", "fuzz"]`) and carries its own `Cargo.lock`. Three of
them carry the names the plan uses, as aliases of the same source:
`protocol_parser` is `protocol_header`, `fec_reassemble` is `transport_fec` and
`input_parser` is `protocol_input`, so the plan's M8 acceptance loop works as
written.

## Build and test

The toolchain is pinned by `rust-toolchain.toml` to stable **1.99.1** with
`rustfmt` and `clippy`. Nightly is only needed for the fuzz targets.
`scripts/env-setup.sh` installs the toolchain, the system packages and the
cargo tooling (`cargo-deny`, `cargo-audit`, `cargo-fuzz`); it is idempotent.

Every shell needs the toolchain on `PATH`:

```
export CARGO_HOME=/home/hayden/.cargo RUSTUP_HOME=/home/hayden/.rustup PATH=/home/hayden/.cargo/bin:$PATH
cd /home/hayden/Desktop/projects/lumen
```

The verification commands, as recorded in `docs/handoff.md` section 11:

```
# Whole-workspace lint gate
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# The whole workspace's tests
cargo test --workspace

# Protocol, transport, crypto, host service
cargo test -p lumen-protocol -p lumen-transport -p lumen-crypto -p lumen-host-service

# VA-API codecs on renderD129 (hardware)
cargo test -p lumen-ffi-va -p lumen-encode-vaapi -p lumen-decode-vaapi

# Capture, no display variables set
env -u WAYLAND_DISPLAY -u DISPLAY cargo test -p lumen-capture-wlr -p lumen-capture-x11

# Live capture under headless sway (start sway first with a private XDG_RUNTIME_DIR)
#   WLR_BACKENDS=headless WLR_RENDERER=pixman sway -c /dev/null
cargo test -p lumen-capture-wlr --test wlroots -- --nocapture

# Input latency over the namespace pair (writes artifacts/m3/input.json)
scripts/run-input-latency.sh --duration 60 --out artifacts/m3/input.json

# Loss sweep, 0.5 %, 2 % and 5 % with 10 ms jitter (writes artifacts/m2)
scripts/sweep-loss.sh

# Low-power client profile on two pinned vCPUs (writes artifacts/m5/tierb.json)
scripts/run-tier-b.sh

# End-to-end pipeline (writes artifacts/m1/e2e.json)
MODE=loopback scripts/run-e2e.sh
# or, over the network namespaces from scripts/netns-up.sh
MODE=netns scripts/run-e2e.sh

# The M4 run: audio, a clipboard round trip and two resolution changes
# (writes artifacts/m4/e2e.json)
scripts/run-e2e.sh --with-audio --with-clipboard --with-resize --report artifacts/m4/e2e.json

# Fuzz (nightly), the plan's M8 acceptance loop, by the plan's target names
cd fuzz && for t in protocol_parser fec_reassemble transport_inject h264_au_split input_parser; do
  cargo +nightly fuzz run "$t" -- -max_total_time=60
done; cd ..

# Dependency policy
cargo deny check
cargo audit
```

Notes:

- The workspace is green: `cargo fmt --all --check` is clean,
  `cargo clippy --workspace --all-targets -- -D warnings` exits 0 and
  `cargo test --workspace` exits 0 with 531 tests passing.

- Tests that need a display skip themselves with a printed reason when
  `WAYLAND_DISPLAY` or `DISPLAY` is unset, so `cargo test --workspace` is
  expected to pass on a headless machine.
- The VA-API and capture tests need a DRM render node and a compositor or X
  server. Use `LUMEN_VA_DEVICE` to override the device; the probe looks at
  `renderD129` first and then `renderD128`.

## Hardware requirements

- **VA-API H.264 encode and decode.** The host needs a hardware H.264 encoder
  (Intel iGPU, AMD `amdgpu`, or NVIDIA via NVENC with `--encoder nvenc`)
  and the client needs an H.264 decoder. On NVIDIA, VA-API is decode-only
  (`nvidia-vaapi-driver`), so encoding there requires NVENC.
- **A DRM render node.** `/dev/dri/renderD*` must exist and be readable and
  writable by the user running Lumen. `lumen-ffi-va` dlopens `libva.so.2` and
  probes the render nodes; `LUMEN_VA_DEVICE` overrides the choice.
- **A Wayland or X11 session** on both ends. Wayland is the primary target;
  X11 is supported for capture and presentation.
- **Linux.** The capture, render, input and DRM paths are Linux-specific.
- **Input injection on the host** needs write access to `/dev/uinput` (and
  `/dev/uhid` for gamepads). Many distributions grant the active seat user an
  ACL on `/dev/uinput` but leave `/dev/uhid` to root; a headless or service
  host, or one with gamepads, needs
  [`packaging/udev/60-lumen-input.rules`](packaging/udev/60-lumen-input.rules)
  and membership of the `input` group. Without access the host logs
  `lumen-host: input injection unavailable` (or `gamepad input failed`) and
  streams without that input.
- No photodiode or camera hardware is required to build or test, but it is
  required to measure glass-to-glass latency. The procedure for that
  measurement is written down in [`harness/README.md`](harness/README.md); the
  hardware it needs does not exist here.

## Current status

- **M0 is complete**: toolchain, system packages, capability probe, network
  namespace harness, workspace skeleton and baseline probe are in place.
- **The M1 pipeline exists and has a measured run.** The host pipeline
  (capture → encode → send) and the client pipeline (receive → decode →
  present) are implemented, in `lumen-host` and `lumen-client`, and they run
  end to end over loopback and over the network namespaces. The merged report
  carries the counters and the per-stage percentiles. The run is
  **PROVISIONAL, software-only**: it is not a glass-to-glass latency.
- **Measured end-to-end run, loopback, 30 s at 1280x720 and 30 fps.**
  **PROVISIONAL, software-only.** Host: 875 frames captured, 873 frames sent,
  883 datagrams sent. Client: 883 datagrams received, 873 access units
  reassembled, 0 recovered by FEC, 873 decoded, 873 presented, 2 dropped,
  0 decode errors, 0 retransmits, 0 packets lost. Measured bitrate
  314.9 kbps.
- **Measured end-to-end run, network namespaces, 30 s at 1280x720 and 30
  fps.** **PROVISIONAL, software-only.** Host: 876 frames captured, 873 frames
  sent, 883 datagrams sent. Client: 882 datagrams received, 872 access units,
  872 decoded, 872 presented, 4 dropped, 0 decode errors, 0 retransmits,
  1 packet lost. Measured bitrate 315.7 kbps.
- **Measured software pipeline latency, capture to present.** The host and
  the client each timestamp their own stages; the host aligns the two clocks
  from a ping exchange and reports a per-stage latency. The loopback run:
  capture p50 8.031 ms, encode p50 3.680 ms,
  send p50 2.408 ms, recv p50 0.034 ms,
  decode p50 4.212 ms, present p50 4.703 ms, and the whole
  pipeline **p50 23.520 ms, p99 27.369 ms,
  max 51.184 ms** over 872 frames. The namespace run:
  **p50 23.498 ms, p99 27.917 ms, max 47.462 ms** over 872 frames.
- **Where the time goes.** The CPU colour conversion is the largest single
  item: `bgrx_to_nv12` takes 2.52 ms and `nv12_to_bgrx` takes 3.57 ms at
  1280x720 on this machine. The client's present stage is mostly the
  conversion; the host's capture stage holds it plus the DMA-BUF readback.
  Moving that work to the GPU is the largest latency win left.
- Those figures are software pipeline latencies from the build machine, not
  glass-to-glass. They exclude the display's own scan-out and the camera
  path. The frame counts depend on the load on the machine, so a rerun gives
  different numbers; the merged report holds them.
- **The transport is adaptive, and it works.** The parity level follows the
  measured loss: no parity below 0.5 %, two parity slices per block of eight
  data slices below 2 %, three per block of twelve at or above it. The block
  size travels in the media header's `flags` word, and three losses inside
  one block are recoverable. The client reports the slices it received and
  lost every 100 ms over the control channel; the host differences the
  cumulative counts, raises the parity level at once when the loss asks for
  it and lowers it only after several lossless windows. The bitrate follows
  the same estimator and reaches the encoder's constant bitrate.
- **The cipher is negotiated** inside the Noise handshake: the capability
  bitmap travels as a handshake payload, so a peer without AES-NI gets
  ChaCha20-Poly1305 and the control channel stays encrypted from its first
  byte.
- **M2, the loss sweep, is measured, and the curve is no longer flat.**
  **PROVISIONAL, software-only.** At 0.5 %, 2 % and 5 % loss with 10 ms
  jitter: 304, 299 and 287 frames presented, 7, 12 and 24 dropped, 1, 4 and
  7 decode errors, 18, 49 and 87 FEC recoveries, 0 retransmits at every
  level. The adaptive policy starts without parity and the frames lost while
  the level rises are not recovered, so the decode errors are the cost of the
  adaptation. The earlier fixed-parity run, whose artifacts this run
  replaced, presented 312 frames at every level with no decode error.
- **M3, the input path, is measured.** **PROVISIONAL, software-only.** Over
  the network namespace pair: 60000 events, 60000 datagrams, 60000 injected,
  0 injection failures, 0 authentication failures, 0 replays, 0 missing
  sequences. The host-injection latency is **p50 46 µs, p99 187 µs, max
  2756 µs**, so the p99 meets the plan's 1 ms target. The measurement is the
  leg between the client stamping an event and the host injecting it; there is
  no display, no compositor and no real client in it.
- **M4, the run with the audio, the clipboard and the resolution changes, is
  measured.** **PROVISIONAL, software-only.** 30 s over loopback at 1280x720
  and 30 fps: 853 frames sent, 853 decoded, 853 presented, 3 dropped, 0
  decode errors, 5602 audio packets, 0 audio underruns, 1 clipboard round
  trip and 2 resolution changes. The A/V skew, measured as the error in the
  alignment over 5581 samples: **p50 -159 µs, p90 13.111 ms, p99 18.330 ms,
  max 122.711 ms**.
- **M5, the two-vCPU proxy, did not pass.** **PROVISIONAL, software-only.**
  300 s at 1080p60 with the client pinned to two vCPUs: 17099 frames decoded
  and 14514 presented, 2992 dropped, 240 decode errors, decode p50 6.829 ms
  and p99 11.804 ms against the 12 ms limit, and the client used 35.47 % of
  the two cores and presented 48.38 fps. The artifact says `pass: false` and
  gives the reason: the client presents on the CPU, so at 1080p60 the
  conversion and the presentation take longer than the frame interval. This
  is a proxy on the build machine, not the Tier B device.
- **M6, the session survives the disturbances.** **PROVISIONAL,
  software-only.** 20 of 20 reconnect cycles reconnected, the slowest in
  5 ms against a 2000 ms limit. 5 of 5 compositor restarts recovered, with no
  file descriptor growth. 3 of 3 network flaps recovered, the slowest in
  1057 ms.
- **M7, the X11 run, is measured.** **PROVISIONAL, software-only.** 60 s at
  1280x720 and 30 fps: 1711 frames captured, 1710 frames sent, 1770 datagrams
  received, 1710 access units, 1710 decoded, 1710 presented, 1 dropped, 0
  decode errors, 0 packets lost, 301.7 kbps; the software pipeline latency
  is p50 24.691 ms, p99 28.322 ms, max 54.278 ms.
- **M8 is partly done.** **PROVISIONAL, software-only.** The soak ran 308 s
  of the 72 h target and presented 4689 frames, with 5 dropped and 2 decode
  errors; 2 of its 10 events survived the session on their own. The chaos
  run ran 320 s and presented 10945 frames, with 22 dropped and 6 decode
  errors; 2 of its 13 events survived the session on their own. The rest of
  both runs is pending.
- **The rig harness exists as software.** `harness/README.md` is the
  physical procedure, `harness/photodiode/` holds the wiring, the v4l2
  capture loop, the trigger finder and the self-test, and
  `harness/fixtures/simulated-trigger.json` is the committed fixture. The
  harness is validated against the simulated trigger; the physical
  measurement stays pending.
- **M1 to M8 are not complete.** The protocol, crypto, transport, session,
  capture, codec, render and input crates have real implementations, the
  audio path is in a measured run, and the input path is measured — but the
  input path is not yet driven by the live host and client pipelines.
- **The workspace is green.** `cargo fmt --all --check` is clean,
  `cargo clippy --workspace --all-targets -- -D warnings` exits 0 and
  `cargo test --workspace` exits 0 with 531 tests passing.
- The transport, crypto and session layers are done; a real TCP plus UDP
  loopback with a Noise handshake passes its session test.
- **There are twelve fuzz targets**, not nine. Three of them carry the
  names the plan uses, as aliases of the same source, so the plan's M8
  acceptance loop works as written.
- **The dependency policy passes.** `cargo deny check` and `cargo audit` both
  exit zero. Three `zvariant` advisories from the xdg-desktop-portal client
  are suppressed with a written reason; the justification is in
  [`docs/design.md`](docs/design.md).

## Not implemented yet

These are real gaps, not future work that is merely unstarted:

- **HEVC and AV1.** The capability bitmap carries `HEVC` and `AV1` bits, but no
  codec implements them. Only H.264 is encoded and decoded.
- **NVENC input is a CPU copy.** The NVENC encoder copies each frame into its
  input buffer from the CPU; a tiled DMA-BUF is refused. CUDA import of the
  capture's DMA-BUF is not implemented.
- **Vulkan Video encode and decode.** `lumen-encode-vulkan` and
  `lumen-decode-vulkan` are stubs. lavapipe is available for a future software
  correctness path.
- **PipeWire capture is shared memory only.** `--capture portal` asks the
  ScreenCast portal once, keeps the restore token next to the pairing store,
  and streams BGRx/RGBA shared-memory frames, which the host converts to NV12.
  No DMA-BUF is negotiated, so there is no explicit sync, and frames are
  stamped on arrival rather than from `spa_meta_header`. A damage-driven
  producer such as xdg-desktop-portal-wlr sends no frames while the screen
  does not change.
- **The zero-copy present path.** The decoder's default output is CPU NV12
  and the renderer cannot import NV12 or a tiled DMA-BUF, so the colour
  conversion runs on the CPU at both ends. This is the largest latency win
  left, and it is also what stops the two-vCPU proxy from holding 1080p60.
- **The input path in the live pipeline.** The X11 capture crate is written
  and is no longer a stub, the input channel puts the input on a socket of
  its own, and the host-injection leg is measured, but the live host and
  client pipelines do not carry the input yet.
- **The glass-to-glass measurement.** The rig harness is written, but no
  photodiode and no camera exist here, so the number has never been
  captured.
- **The Tier B device.** The plan's Celeron N4020 does not exist in this
  environment. The two-vCPU run is a proxy, not the device.
- **The long soak and the full chaos plan.** The short runs are done; the
  72 h target and the rest of the chaos plan are pending.

## Status of measurements

**No glass-to-glass latency has been measured.** There is no photodiode and no
high-speed camera rig available, so the number that actually matters — light
leaving the host panel and reaching the eye — has never been captured. The
plan is explicit that this must be measured with a photodiode and a camera and
never asserted, so no such figure is stated here or anywhere else in this
repository.

What exists today are provisional **software** measurements, none of them
glass-to-glass and none of them taken on the target hardware:

| Measurement | Result | Artifact |
|---|---|---|
| Input host-injection latency, bare veth, three runs | p50 180 µs, p99 1151 µs, max 5921 µs, 0 drops (run 1); p50 49 µs, p99 138 µs (run 2); p50 49 µs, p99 131 µs (run 3) | `artifacts/m3/input-latency-bare-veth.json` |
| Same, netem 1 ms each end | p99 1136 µs and 1119 µs — the emulated link dominates, so this is not comparable to the target | `artifacts/m3/` |
| Input host-injection latency, network namespace pair, 60000 events | p50 46 µs, p90 85 µs, p99 187 µs, max 2756 µs; 60000 datagrams, 60000 injected, 0 injection failures, 0 replays, 0 missing sequences. The p99 meets the plan's 1 ms target | `artifacts/m3/input.json` |
| Decode roundtrip, 30 frames 1280x720, IDR + 29 P | luma PSNR min 47.60 dB, mean 50.47 dB, 0 decode errors | `crates/lumen-decode-vaapi/tests/roundtrip.rs` |
| Encode quality, 30-frame IPPP, 1280x720 | luma PSNR min 45.2 dB, mean 50.0 dB | VaEncoder2 report |
| Transport and session loopback | functional pass on real TCP and UDP with a Noise handshake; no latency measured | `lumen-host-service` test |
| Fuzz | twelve targets, 60 s each; four crashes found and fixed, no open panics | `fuzz/artifacts/` |
| Capture | headless sway, 1280x720, DMA-BUF XR24 linear; every pixel read back correct using the reported stride | `wlroots` test output |
| End-to-end pipeline, loopback, 30 s | host 875 frames captured, 873 frames sent, 883 datagrams; client 883 datagrams received, 873 access units, 873 decoded, 873 presented, 2 dropped, 0 decode errors, 0 packets lost; 314.9 kbps; pipeline latency p50 23.520 ms, p99 27.369 ms | `artifacts/m1/e2e.json` |
| End-to-end pipeline, network namespaces, 30 s | host 876 frames captured, 873 frames sent, 883 datagrams; client 882 datagrams received, 872 access units, 872 decoded, 872 presented, 4 dropped, 0 decode errors, 1 packet lost; 315.7 kbps; pipeline latency p50 23.498 ms, p99 27.917 ms | `artifacts/m1/e2e-netns.json` |
| Loss sweep, 0.5 %, 2 % and 5 % with 10 ms jitter | `scripts/sweep-loss.sh` over the namespace pair; the curve is no longer flat: 304, 299 and 287 frames presented, 1, 4 and 7 decode errors, 18, 49 and 87 FEC recoveries, 0 retransmits. The adaptive policy starts without parity and the frames lost while the level rises are not recovered | `artifacts/m2/curve.csv` |
| M4 run: audio, clipboard and two resolution changes, 30 s | 853 frames sent, 853 decoded, 853 presented, 3 dropped, 0 decode errors; 5602 audio packets, 0 audio underruns, 1 clipboard round trip, 2 resolution changes; A/V skew p50 -159 µs, p90 13.111 ms, p99 18.330 ms over 5581 samples; 307.5 kbps | `artifacts/m4/e2e.json` |
| Low-power client, two-vCPU proxy, 1080p60, 300 s | 17099 frames decoded and 14514 presented, 2992 dropped, 240 decode errors; decode p50 6.829 ms, p99 11.804 ms against the 12 ms limit, so the run does not pass; the client used 35.47 % of the two pinned cores and presented 48.38 fps. A proxy, not the Tier B device | `artifacts/m5/tierb.json` |
| Unattended reconnect, 20 cycles | 20 reconnected, 0 failed, the slowest 5 ms against the 2000 ms limit | `artifacts/m6/reconnect.json` |
| Compositor restart, 5 cycles | 5 recovered, 0 failed, open file descriptors 12 before and after every cycle | `artifacts/m6/restart.json` |
| Network flap, 3 flaps | 3 recovered, 0 failed, the slowest 1057 ms against the 2000 ms limit | `artifacts/m6/wifi-flap.json` |
| End-to-end pipeline, X11 host and X11 client, 60 s | host 1711 frames captured, 1710 frames sent, 1770 datagrams; client 1710 access units, 1710 decoded, 1710 presented, 1 dropped, 0 decode errors, 0 packets lost; 301.7 kbps; pipeline latency p50 24.691 ms, p99 28.322 ms | `artifacts/m7/e2e-x11.json` |
| Soak, short run, 308 s of the 72 h target | 4689 frames decoded and presented, 5 dropped, 2 decode errors; 2 of the 10 events survived the session on their own; the remainder is pending | `artifacts/m8/soak.json` |
| Chaos, short run, 320 s | 10945 frames decoded and presented, 22 dropped, 6 decode errors; 2 of the 13 events survived the session on their own | `artifacts/m8/chaos.json` |
| Rig self-test | 30 synthetic observations and a simulated glass-to-glass of 5000 µs | `artifacts/m1/rig-selftest.json` |
| Photodiode harness, software only | the wiring, the v4l2 capture loop, the trigger finder and the self-test, validated against the committed simulated-trigger fixture. The physical measurement is pending | `harness/README.md`, `harness/fixtures/simulated-trigger.json` |

Explicit non-results:

- Glass-to-glass: **not measured** (no photodiode or camera rig).
- Glass-to-glass latency from capture to present: **not measured**. The
  software pipeline latency is measured (p50 23.520 ms, p99 27.369 ms over
  loopback), but the gap between it and the photodiode floor is pending the
  rig.
- Crypto cost: **not measured**.
- Baselines (Sunshine, Moonlight, RustDesk, GNOME Remote Desktop): **not run**.
  None of the four is installed; the probe found only `xfreerdp`, which is the
  RDP client of the GRD baseline, whose host is missing.
- Tier B low-power client (Celeron N4020): **no device**. The two-vCPU run is
  a proxy.
- The long soak: **not run to length**. 308 s of the 72 h target; the
  remainder is pending.

The input injection p99 above the 1 ms target in the first bare-veth run, and
the fact that it varies by more than an order of magnitude between otherwise
identical runs, are the reason every figure here is marked provisional. The
newer namespace run meets the 1 ms p99 target, but measurement noise from
concurrent load on the build machine has shown up as scheduling lateness of up
to 4.5 ms, so the counters move with the load on the machine and the artifact
is the record.

## Documentation

- `docs/plan.md` — the approved execution spec: crates, tasks, acceptance
  commands, milestones M0 to M8.
- `docs/handoff.md` — the current state of the repository, open defects and
  gaps, measured results and the verification commands.
- `docs/design.md` — architecture, stage budgets and numbered decisions.
- `docs/protocol.md` — the wire protocol.
- `docs/benchmarks.md` — per-stage percentiles and raw artifacts.
- `docs/settings.md` — what may become a setting, and what may not.
- `docs/known-limits.md` — what is not finished, with the next action for each item.
- `docs/known-limits.md` — hardware-gated items and explicit non-goals.
- `harness/README.md` — the photodiode and camera rig procedure, the wiring
  and the self-test.

## Licence

Dual licensed under [MIT](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE),
at your option. All dependency licences are permissive; `cargo deny` enforces
the allow list.
