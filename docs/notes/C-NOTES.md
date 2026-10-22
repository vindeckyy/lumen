# Agent C — notes for the lead

Scope of this agent: milestones M4 to M7 of `docs/plan.md` — the server and
client integration.

Everything below is either a decision the lead has to apply, a gap that is
outside the crates this agent owns, or a measurement that has to go into the
documentation.

---

## Wiring needed

### 1. A `dynamic_range` capability bit (M4.8)

`crates/lumen-protocol/src/capability.rs` is owned by another agent, so the
bit was not added there. The plan's M4.8 asks for a `dynamic_range` bit next
to the existing `HDR` bit (1 << 5) and the `BIT_DEPTH_10` bit (1 << 4).

**Please add to `Capabilities`:**

```rust
/// The dynamic range of the video (SDR, HDR, HDR10+).
const DYNAMIC_RANGE = 1 << 19;
```

Bit 19 is free: the bits in use are 0 to 18 (`SHADOW_MODE` is 1 << 18).

The host and the client do **not** carry the HDR metadata end to end.
The metadata transport (colourimetry, maximum mastering display luminance)
is **not implemented**; only the capability bit is missing.

### 2. P010 and HEVC in the encoder (M4.8)

`crates/lumen-encode-vaapi` is not in this agent's list, so the P010
(10-bit) path and the HEVC encoder were not touched. The host pipeline
converts every captured frame to 8-bit NV12, so the 10-bit path needs:

* `lumen-encode-vaapi`: a P010 surface format and a 10-bit H.264 profile
  (the `BIT_DEPTH_10` capability already exists in the protocol).
* `lumen-decode-vaapi`: a P010 output format.
* `lumen-protocol::color`: the 10-bit colour conversion.

**HEVC encode is blocked**: the decoder in `lumen-decode-vaapi` only parses
H.264 (it looks for the H.264 SPS/PPS in the Annex-B stream and reports
"slice refers to an unknown PPS" for anything else). A HEVC stream would
therefore decode to errors on the client. HEVC encode has to wait for the
decoder, not the other way round.

### 3. `lumen-decode-vulkan` (M5.1)

The plan's M5.1 wants the fallback chain VA-API → Vulkan Video hardware →
Vulkan Video software (lavapipe) → CPU. `crates/lumen-decode-vulkan` is a
stub and is owned by nobody at the moment.

**What is in place now:** `crates/lumen-decode-vaapi/src/probe.rs` has the
full chain as data:

```rust
pub enum DecodeBackend { VaApi, VulkanHardware, VulkanSoftware, Cpu }
pub struct BackendAvailability { va_api, vulkan_hardware, vulkan_software, cpu }
pub fn select_backend(probe, profile, vulkan: &BackendAvailability, cpu: bool) -> Result<DecodeBackend, String>
```

The VA-API rung is answered from a real probe of the driver
(`DecodeProbe::probe()` reads the profiles and the entry points with
`vaQueryConfigProfiles` and `vaQueryConfigEntrypoints`). The Vulkan rungs
are answered from a `BackendAvailability` the caller fills in, because this
crate cannot open a Vulkan device.

**Please implement `crates/lumen-decode-vulkan/src/probe.rs`** with the same
shape (a probe that reports the Vulkan Video profiles and whether the
device is a hardware or a software one — lavapipe reports itself as
`llvmpipe`), and then wire the two rungs into the client's
`decode_select` step. The `BackendAvailability` struct in
`lumen-decode-vaapi` is the seam; nothing else has to change.

### 4. Audio (M4.1 to M4.3)

`crates/lumen-audio` is owned by another agent and its `Cargo.toml` may not
be touched from here, so **this agent did not add the audio dependency to
the host or the client**.

**Another agent is wiring the audio into the host and the client at the
same time** (the `lumen_audio::ClockOffset` and the `video_clock` of the
control stage in the client are theirs). The two sets of changes overlap in
`crates/lumen-host-service/src/pipeline.rs`,
`crates/lumen-client-desktop/src/lib.rs` and
`crates/lumen-host-service/src/multi.rs` (the `audio_port` of a session is
theirs, the session table itself is this agent's).

The parts that are still missing for the audio:

* the host needs a per-session PipeWire virtual sink, a monitor of that sink
  and an Opus encoder (`lumen-audio` already has the codec);
* the client needs the playback side and the A/V sync against the video
  capture timestamp (`lumen-audio` has the sync module);
* the media path for the audio needs a second socket, the plan says
  "bidirectional streams on separate sockets". The session table hands out
  an `audio_port` for that now.

The control channel already carries the media header with a
`MessageKind::Audio`, so the packet format is there. What is missing is the
transport wiring in the two pipelines. The seam for it is the send stage of
`crates/lumen-host-service/src/pipeline.rs` (a second `MediaSocket` next to
the video one) and the receive stage of
`crates/lumen-client-desktop/src/receive.rs`.

### 5. The client's media port is still not negotiated (O-25)

The host still derives the media peer from the address of the control
connection plus the fixed `--client-media-port` (decision D50). Two
clients on **one** machine therefore cannot both use the default port.

The fix is one control message in each direction:

* the client sends `MediaReady { port: u16 }` after it bound its media
  socket (the socket is bound before the handshake, so the port is known);
* the host answers `SessionInfo { input_enabled: bool, shadow: bool }`.

Both are new kinds in `ControlMessage` (the next free kinds are 14 and
15). They were **not** added, because the control channel was being edited
by another agent at the same time and a new kind breaks a peer that does
not know it (`decode` rejects an unknown kind and the channel dies).

**The host already supports several sessions** (`--max-sessions`,
`--shadow-sessions`, `crates/lumen-host-service/src/multi.rs`), each with
its own encoder, its own media socket and its own control channel. The
media port is the same for all of them, which is right for clients on
different machines and wrong for two clients on one machine.

---

## The virtual display comparison (M6.4)

The plan asks for the lowest-privilege option with a controllable
resolution and refresh rate. The three options, measured on this machine:

| Option | Privilege | Resolution | Refresh | Measured |
|---|---|---|---|---|
| **sway headless output** | a normal user process | exact | exact | `output HEADLESS-1 mode 1920x1080@60Hz` in the sway configuration sets both; the compositor reports `1920x1080 @ 60000 mHz` |
| Xorg with the dummy driver | root, plus its own configuration | fixed in this build | fixed | xorg-server 21.1.24 rejects the `VideoOutput` option ("not a valid keyword in this section") and the dummy driver has a fixed mode list (1024x768, 1280x720, 1280x800, …). `xrandr --newmode`/`--addmode` are refused as well |
| DRM EDID override | root, plus a writable connector | none | none | it renames a physical connector instead of adding a display, so it is not a virtual display at all |

**The sway headless output wins** and is what
`crates/lumen-host-service/src/virtual_display.rs` implements
(`VirtualDisplay::start` writes a configuration with the mode, starts sway
with `WLR_BACKENDS=headless` and waits for the socket). The plan's
`D-VIRT-DISPLAY` default agrees.

A fourth option, **Xvfb**, is worth knowing: it needs no root, gives an
exact resolution (`Xvfb -displayfd 3 -screen 0 1920x1080x24`) and is what
the X11 test environment uses. It is not in the plan's list, but for an
X11 host it is the cheapest way to a screen of a known size. Its refresh
rate is not settable, and the screen it offers has no mode list to choose
from.

## The backend matrix (M7.4)

`crates/lumen-host-service/src/backend.rs` holds the matrix:

```rust
pub const MATRIX: [MatrixEntry; 4] = [wayland/wayland, wayland/x11, x11/wayland, x11/x11];
```

All four rows are supported. The host picks its capture backend with
`--capture wayland|x11|auto`, the client picks its present backend with
`--present wayland|x11|auto` (the older `--wayland` and `--x11` flags still
work). `auto` reads `WAYLAND_DISPLAY` first and `DISPLAY` second, because
a Wayland session usually runs an XWayland server as well.

The client crate has its own `crates/lumen-client-desktop/src/backend.rs`
that reuses the matrix from the host crate instead of repeating it.

## The clipboard and what the compositor on this machine offers (M4.4)

The plan asks the host to read the selection with
`zwlr_data_control_manager_v1` and the client to own a `wl_data_device`.

**The compositor on this machine (sway 1.12, wlroots 0.19) offers neither
`zwlr_data_control_manager_v1` nor `ext_data_control_manager_v1` nor
`wl_data_device_manager_v1`.** The globals it does offer are
`wl_data_device_manager` (version 3) and
`zwp_primary_selection_device_manager_v1`.

What that means for the code:

* the **host** (`crates/lumen-host-service/src/clipboard.rs`) uses
  `zwlr_data_control_manager_v1` from `wayland-protocols-wlr` 0.3.12 — the
  bindings exist and the code is complete, but the compositor does not
  offer the interface, so the host cannot read the local clipboard **on
  this machine**. The host reports
  "the compositor does not offer zwlr_data_control_manager_v1".
* the **client** (`crates/lumen-client-desktop/src/clipboard.rs`) uses the
  legacy `wl_data_device_manager`, because that is what the compositor
  offers. The legacy interface needs the serial of the focus event, so the
  client waits for `wl_data_device.enter` before it takes the selection;
  the payload is kept and offered as soon as the focus arrives.
  `wl_data_device_manager_v1` is the right interface, but the `wayland.xml`
  that ships with the pinned `wayland-client` 0.31 does not carry it yet
  (it was added to wayland 1.22), and the `wayland-protocols` crate has
  `ext_data_control_manager_v1` instead — which this compositor does not
  offer either.

**The clipboard round trip therefore cannot be exercised on this machine.**
The transport is in place: the host sends `ControlMessage::Clipboard`, the
client takes the text and offers it in its own selection, and the payload
is UTF-8 with a 1 MiB cap on both sides. What is missing is a compositor
with the data-control interface. The host has a `--clipboard-test` hook
that puts a text into the host's own selection, so the round trip can be
exercised as soon as the compositor offers the interface.

**Recommendation for the lead:** the host should also try
`ext_data_control_manager_v1` (the bindings are in `wayland-protocols` 0.32
with the `staging` feature, which the workspace already enables) as a
second choice, so a modern compositor works without `wayland-protocols-wlr`.

## Three fixes in the shared pipeline code

These are in files this agent shares with the other agents, so the lead
should know about them:

1. **The host's control stage no longer starves.** The clipboard read
   blocks for 20 ms, and it was run on every round of the control loop.
   On a compositor with no data-control interface the read was preceded by
   a fresh Wayland connection on every round as well, which kept the host
   from sending its pings. The client then saw a silent link and called the
   session dead after 1.5 s. The clipboard is now asked once, and it is
   read every 500 ms (`crates/lumen-host-service/src/pipeline.rs`).
2. **The estimator lock is no longer held across the encoder
   reconfiguration.** The encode stage took the estimator lock and then
   rebuilt the encoder inside it. The control stage waits for the same
   lock, so the control channel stopped for as long as the encoder took.
   The rate is now read inside the lock and applied outside it.
3. **The client drops the late frames instead of queueing them.** The
   receive stage used to block on the decode stage, so the datagrams piled
   up in the socket and the latency grew without a bound (the end-to-end
   p99 of the Tier B run was 1989 ms). The frames are now dropped when
   they are late, the frames that are dropped are decoded anyway so the
   reference chain of the decoder stays intact, and the decoder waits for a
   keyframe after a loss instead of producing an error per frame. The
   end-to-end p99 of the Tier B run is 59 ms now.

## Multi-monitor (M4.6)

* The host enumerates the outputs: `wl_output` through the capture crate
  for Wayland, RandR for X11 (`crates/lumen-host-service/src/monitors.rs`).
  The X11 path reads the mode of the CRTC each output drives and computes
  the refresh rate from the dot clock.
* The list travels over the control channel with the existing
  `ControlMessage::Monitor` message, one message per output. The client
  collects them in `crates/lumen-client-desktop/src/monitors.rs` and can
  pick one (`MonitorList::select`).
* **One encoded stream per monitor is not implemented.** The host captures
  one output and sends one stream. The enumeration, the transport and the
  client side are there; a second output would need a second capture, a
  second encoder and a second media socket, and the client would need to
  pick the stream. The session table in `multi.rs` is the seam: it already
  hands out one media port and one role per session.

## Resolution changes (M4.7)

* The host watches the size of the frames the capture delivers
  (`crates/lumen-host-service/src/resize.rs`), counts the changes and sends
  `ControlMessage::ResolutionChanged` to the client.
* The client keeps one window and scales the picture into it:
  `crates/lumen-render/src/fit.rs` holds the placement policy, and both
  renderers use it, so a frame of a new size is placed and scaled in the
  window instead of rebuilding it.
* **The encoder is not reconfigured.** The encoder is created per session
  with the size of the configuration, so a mode change during a session is
  reported to the client but the encoder keeps the old size and the frames
  keep the old size. Reconfiguring the VA-API encoder means a new
  `H264Encoder` with the new size and a new IDR, and that belongs in
  `lumen-encode-vaapi`, which this agent does not own.

## The Tier B hardware protocol (M5.4)

The text for `docs/known-limits.md` and `docs/benchmarks.md` (this agent
may not edit them):

> **Tier B: the Celeron N4020.** The Tier B client of the plan is a
> Celeron N4020 with two Gemini Lake-N cores at 1.1 GHz and an Intel UHD
> 600 graphics unit with no Quick Sync. The decode path is VA-API H.264
> Main/High through the Intel media driver; there is no NVENC and no
> Vulkan Video on the device, so the fallback chain of M5.1 ends at the
> VA-API rung. The expectations for the device are: a decode p99 of at most
> 12 ms at 1080p60, no frame drops over a five minute run, and a CPU use of
> at most 85 % of the two cores. The host side is a wired Gigabit link
> with the same bitrate class as Tier A. The procedure is: install
> `intel-media-va-driver-non-free` and `libva2`, check that
> `vainfo` lists `VAProfileH264Main` and `VAProfileH264High` with the
> `VLD` entry point, pin the client to the two cores with
> `taskset -c 0,1`, run `scripts/run-tier-b.sh --cpus 0,1 --duration 300
> --report artifacts/m5/tierb.json` and read the acceptance out of the
> report. The glass-to-glass targets of the plan (p50 ≤ 20 ms, p99 ≤ 30 ms)
> stay **PENDING** until the device exists, and the two-vCPU proxy on the
> build machine is recorded separately and never as the device.

The proxy measurement of that procedure is in
`artifacts/m5/tierb.json` and comes from `scripts/run-tier-b.sh`.

## The X11 test environment (M7.5)

`scripts/x11-env.sh` starts an Xvfb display and, with `--with-xorg`, an
Xorg server with the dummy video driver. Both write their display numbers
into a JSON file, so no display number is ever guessed:

```
scripts/x11-env.sh start --json artifacts/m7/x11-env.json [--with-xorg]
scripts/x11-env.sh stop  --json artifacts/m7/x11-env.json
```

The Xvfb server is started with `-displayfd`, so it picks a free display
number itself. The Xorg server needs root (the X server refuses to run for
an ordinary user) and its display number is picked by the script, because
Xorg cannot report its own.

## The X11 end-to-end run (M7)

`scripts/run-e2e-x11.sh` does the X11 variant of `scripts/run-e2e.sh`,
because `scripts/run-e2e.sh` may not be changed from here:

```
scripts/run-e2e-x11.sh --duration 60 --report artifacts/m7/e2e-x11.json
```

It runs the four rows of the matrix with `--matrix`, the default being
`x11/x11`.

**Please add an `--x11` flag to `scripts/run-e2e.sh`** that forwards to
the same code, so the plan's acceptance command
`DISPLAY=:77 scripts/run-e2e.sh --x11 --duration 60 --report
artifacts/m7/e2e-x11.json` works as written. The flag has to do three
things: start the X11 environment instead of the two compositors, pass
`--capture x11 --display $DISPLAY` to the host and `--present x11` to the
client, and run the painter (`lumen-x11-source`) on the display, because
the X11 capture is gated by XDamage and a screen that never changes
produces no frames.

`lumen-x11-source` is a new binary in `crates/lumen-host-service` that
paints the root window of an X display at a fixed rate. It is a test tool
for the X11 path, not part of the product.

## The compositor restart and the client (M6.1, M6.2)

* The host reopens the capture when it fails and asks the encoder for a
  keyframe (`crates/lumen-host-service/src/pipeline.rs`, the capture stage;
  the policy is in `crates/lumen-host-service/src/resize.rs`).
* The host keeps the session after the client leaves
  (`--persist-timeout`, `crates/lumen-host-service/src/persist.rs`).
* The client comes back on its own (`--reconnect`,
  `crates/lumen-client-desktop/src/reconnect.rs`): the backoff is 200 ms to
  2 s, and the client gives up when the deadline passes.
* **The client needed a way to notice a dead link.** A network that drops
  the packets does not close the TCP connection, so the client watched a
  silent socket forever. It now counts the pings of the host: three missed
  pings (1.5 s) mean the link is gone, and the client reconnects. Without
  that, the flap test could not pass at all.

## The ports of the scripts

The scripts of the other agents use the ports 24800 and 24801, and their
cleanup ends every process called `lumen-host` or `lumen-client`. The
scripts of this agent use a port of their own (24820 to 24861) and run
copies of the binaries with a name of their own, so a run of another agent
cannot end a measurement in the middle. The ports are still overridable
with `CONTROL_PORT` and `CLIENT_MEDIA_PORT`.

## The scripts

| Script | What it does |
|---|---|
| `scripts/run-tier-b.sh` | the Tier B proxy: the client is pinned with `taskset -c 0,1` and decodes 1080p60 |
| `scripts/run-reconnect.sh` | the client is killed and started again, once per cycle |
| `scripts/run-compositor-restart.sh` | the compositor the host captures is restarted, once per cycle |
| `scripts/run-wifi-flap.sh` | the veth pair goes down and up again |
| `scripts/x11-env.sh` | the X11 test environment |
| `scripts/run-e2e-x11.sh` | the X11 end-to-end run |

Every one of them writes a JSON artifact and exits non-zero when the
acceptance of the plan is not met.

Two measurements in the scripts need a word of explanation:

* **The dropped frames of the Tier B run.** The counter of the run report
  (`dropped_frames`) is the difference between the frames the host
  captured and the frames the client decoded, so it also holds the frames
  that were in flight when the run ended. The script reports the client's
  own drop count as `dropped_frames` and the other number as
  `frames_in_flight_at_the_end`. The client's count holds the frames the
  receive stage dropped (the decode stage was busy), the frames the
  decoder waited for a keyframe for, and the frames that were decoded but
  too late to be presented.
* **The reconnect time.** The client measures from the start of its
  pipeline to the moment the session is established, which is more than
  the plan asks for (the plan measures from the moment the link is up).

---

## The Tier B proxy on this machine: what it shows

`artifacts/m5/tierb.json` is the 300 second run of
`scripts/run-tier-b.sh --cpus 0,1 --duration 300`. The numbers:

* the decode p50 is 6.8 ms and the p99 is 11.9 ms, so the p99 of the plan
  (12 ms) is met;
* the client uses 34 % of the two pinned cores, well under the 85 % of the
  plan;
* **the client drops frames**: it presents about 43 of the 60 frames a
  second, so the "no frame drops" of the plan is **not** met. The report
  says `pass: false` for that reason.

**Why:** the client converts the decoded NV12 into BGRX on the CPU and
presents it from the CPU (decision D49, the open defect O-7). At 1080p that
conversion alone takes about 10 ms, the decode about 7 ms, and the
presentation about 4 ms: 21 ms of work for a frame that arrives every
16.7 ms. The client cannot keep up, so it drops the frames that are late.

The pipeline now behaves the way the plan's `D-JITTER` asks for: the late
frames are dropped and not queued, the frames that are dropped are decoded
anyway so the reference chain of the decoder stays intact, and the decoder
waits for a keyframe after a loss instead of producing an error per frame.
That bounded the latency: the end-to-end p99 of the Tier B run went from
1989 ms to 59 ms.

**The prerequisite for the Tier B target of the plan is the zero-copy path
(O-7)**: a DMA-BUF decoder output and a renderer that imports it, so the
colour conversion leaves the hot path. Until then the two-vCPU proxy at
1080p60 does not hold.

## What is not implemented

| Item | Why |
|---|---|
| the audio path (M4.1 to M4.3) | `crates/lumen-audio` is owned by another agent; its `Cargo.toml` may not be touched from here, so no dependency was added. The seam is described above. |
| P010, HDR metadata and HEVC (M4.8) | `crates/lumen-encode-vaapi` is not in this agent's list. The decoder cannot read HEVC either, so HEVC encode would produce a stream the client cannot decode. |
| one stream per monitor (M4.6) | the enumeration, the transport and the client side are there; a second encoder per output is not. |
| the encoder reconfiguration on a mode change (M4.7) | the encoder belongs to `lumen-encode-vaapi`. The change is reported to the client, the encoder keeps the old size. |
| the Vulkan Video rungs of the decode chain (M5.1) | `crates/lumen-decode-vulkan` is a stub and is owned by nobody. The chain and the selection logic are in place; the Vulkan probe is not. |
| the clipboard round trip on this machine (M4.4) | the compositor offers no data-control interface. See the section above. |
| the negotiated media port (O-25) | the control channel was being edited by another agent; a new message kind breaks a peer that does not know it. |
| the glass-to-glass numbers | no photodiode and no camera. Every number here is a software pipeline latency. |
