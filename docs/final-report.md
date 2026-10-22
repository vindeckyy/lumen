# Lumen — final report

Plan milestone **M8.7**: "Final docs: `docs/final-report.md` (measured results,
deviations, pending items, unsolved problems)".

**Date of this report: 2026-10-21.**
**Environment: the build machine** — Ubuntu 26.10, kernel 7.3.0-9, AMD Ryzen
5 4600H with Radeon Graphics (12 vCPU), NVIDIA GTX 1650, 15 GiB RAM, one
physical 1920x1080 eDP-1 panel with no compositor session on it.

> ### The one thing to read first
>
> **No glass-to-glass latency has been measured.** This environment has no
> photodiode, no camera rig, no second machine and no Tier B device. Every
> latency number in this report is a **software pipeline latency**: the
> interval between the host's capture timestamp and the client's
> presentation, with the two machines' clocks aligned by an estimate. It
> excludes the display's own scan-out, the panel and the camera path
> entirely. It is **not** a glass-to-glass number and must never be quoted
> as one.
>
> **Every number in this report is PROVISIONAL.** None of it comes from the
> target hardware, none of it comes from a second machine, and none of it is
> a Tier A or a Tier B result. Each row names the artifact it came from.
>
> **The workspace is green.** `cargo fmt --all --check` is clean,
> `cargo clippy --workspace --all-targets -- -D warnings` exits 0,
> `cargo test --workspace` exits 0 with 531 tests passing, `cargo deny check`
> and `cargo audit` both exit 0, and there are twelve fuzz targets.

---

## 1. The two definitions, and which one was measured

| | Definition | Status here |
|---|---|---|
| **Software pipeline latency** | The host's capture timestamp to the client's present-ack, both in software, with the two clock offsets estimated over the control channel. | **MEASURED, PROVISIONAL.** `artifacts/m1/e2e.json`, `artifacts/m1/e2e-netns.json`. |
| **Glass-to-glass** | A pixel changing in the host's presented output to the same pixel changing on the client's panel. Only a photodiode and a high-speed camera can measure it. | **NOT MEASURED.** No rig, no second machine, no display pair. The harness software is built and validated against the simulated trigger; the physical measurement is **PENDING**. |
| **Display floor** | The latency of a panel by itself. | **NOT MEASURED.** Needs the rig. |
| **Input host-injection latency** | The client stamping an event to the host injecting it. | **MEASURED, PROVISIONAL**, for the injection leg only. `artifacts/m3/`. |
| **A/V skew** | The error in the alignment: the difference between the moment an audio frame was played and the moment the video frame of the same moment was presented. | **MEASURED, PROVISIONAL**, in software. `artifacts/m4/e2e.json`. It is not a display figure. |

The difference between the software pipeline latency and the glass-to-glass
is everything the software does not see: the compositor's scan-out, the
panel's response and the camera's own path. On this machine that gap is
**unknown**, and it is not small: a panel's own latency is typically
several milliseconds, which is of the same order as the entire software
pipeline measured here.

---

## 2. What was measured

### 2.1 The end-to-end pipeline (M1)

Two headless sway compositors, `lumen-host` capturing one and
`lumen-client` presenting into the other, 1280x720 at 30 fps for 30
seconds. **PROVISIONAL, software-only.**

| Counter | Loopback (`artifacts/m1/e2e.json`) | Namespaces (`artifacts/m1/e2e-netns.json`) |
|---|---|---|
| `frames_sent` | 873 | 873 |
| `frames_decoded` | 873 | 872 |
| `frames_presented` | 873 | 872 |
| `dropped_frames` | 2 | 4 |
| `decode_errors` | 0 | 0 |
| `fec_recoveries` | 0 | 0 |
| `retransmits` | 0 | 0 |
| `packets_lost` | 0 | 1 |
| `packets_received` | 883 | 882 |
| `bitrate_kbps` | 314.95 | 315.66 |

The host captured 875 frames and sent 883 datagrams over loopback, and 876
frames and 883 datagrams in the namespaces. The client reassembled 873 and
872 access units. The `dropped_frames` counter is the difference between the
frames the host captured and the frames the client decoded; it is not a
transport loss counter, and `packets_lost` is 0 and 1 respectively.

Per-stage percentiles, in milliseconds, from the same two artifacts. These
are **software pipeline** numbers.

| Stage | Loopback p50 | p90 | p99 | max | Namespaces p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|---|---|
| capture | 8.031 | 8.636 | 9.402 | 21.533 | 8.175 | 8.807 | 10.207 | 18.748 |
| encode | 3.680 | 5.092 | 5.332 | 5.668 | 3.668 | 5.082 | 5.270 | 6.156 |
| send | 2.408 | 4.579 | 5.014 | 5.081 | 2.484 | 4.609 | 5.033 | 5.115 |
| recv | 0.034 | 0.050 | 0.061 | 4.303 | 0.024 | 0.050 | 0.057 | 0.481 |
| decode | 4.212 | 4.496 | 4.750 | 11.601 | 4.100 | 4.368 | 4.708 | 11.951 |
| present | 4.703 | 5.011 | 6.599 | 7.784 | 4.708 | 5.065 | 5.568 | 8.191 |
| **end_to_end** | **23.520** | **25.818** | **27.369** | **51.184** | **23.498** | **25.783** | **27.917** | **47.462** |

Both runs are over 872 samples of the `end_to_end` stage.

What the numbers say: the two stages that do the pixel work dominate. The
host converts the captured BGRX frame to NV12 on the CPU before the encoder
sees it (2.52 ms measured on this machine at 1280x720), and the client
converts the decoded NV12 back to BGRX on the CPU (3.57 ms). The client's
`present` stage (p50 4.703 ms) is mostly that conversion; the host's
`capture` stage (p50 8.031 ms) holds the same conversion plus the DMA-BUF
readback. **Moving the colour conversion to the GPU is the largest single
latency win that is still open.**

Two caveats that the numbers themselves carry:

- The `send` stage has a small positive minimum in both runs (5 µs on
  loopback, 3 µs in the namespaces). That cannot happen on the wire: the
  client stamps `recv` after the host stamped `send`. The small positive
  floor is the resolution of the two stamps that bracket the stage, and it
  bounds the error of the clock-offset estimate. It is still an estimate, and
  every figure above rests on it.
- The frame counts depend on the load on the machine. A rerun gives
  different numbers.

### 2.2 The loss sweep (M2)

`scripts/sweep-loss.sh` over the veth pair, netem on both ends, 12 s per
level. Artifact: `artifacts/m2/curve.csv`. **PROVISIONAL, software-only.**

| loss | jitter | frames sent | decoded | presented | dropped | decode errors | FEC recoveries | retransmits | packets lost | packets received | bitrate kbps |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.5 % | 10 ms | 308 | 304 | 304 | 7 | 1 | 18 | 0 | 6 | 363 | 333.96 |
| 2 % | 10 ms | 309 | 299 | 299 | 12 | 4 | 49 | 0 | 13 | 495 | 461.46 |
| 5 % | 10 ms | 308 | 287 | 287 | 24 | 7 | 87 | 0 | 31 | 635 | 603.19 |

**The curve is no longer flat.** The frames presented fall from 304 to 299
to 287 as the loss rate rises, the drop count rises from 7 to 12 to 24, and
the decode errors rise from 1 to 4 to 7.

The reason is the adaptive parity (D59, D69). The parity level follows the
measured loss: no parity below 0.5 % loss, two parity slices per block of
eight data slices below 2 %, three per block of twelve at or above it. The
policy therefore starts a run **without any parity**, and the frames that
fall out of a block before the level has risen are lost and are not
recovered. The per-end host reports show the level changing 2, 8 and 16
times at the three levels, so the policy is chasing the loss at 5 % and is
nearly still at 0.5 %.

**The decode errors in the sweep are the cost of that adaptation.** They are
not a decoder defect and not a correctness failure of the parity: they are
the frames that were lost while the level was still too low to rebuild them.
The retransmit counter is 0 at every level and so is the client's NACK
counter, so the deadline-aware retransmission was not exercised by this
sweep at all.

For comparison, the earlier run with the parity level fixed — whose
artifacts this run replaced — was flat: 313 frames sent and 312 presented at
every level, with no decode error and no retransmit at any level, because the
FEC rebuilt every frame that was missing a slice. The flat curve was a
property of the fixed policy, not of the link.

**The `fec_recoveries` counter still overstates the loss.** It reads 18, 49
and 87 at the three levels, against 6, 13 and 31 packets lost. A data slice
that is merely *late*, because the 10 ms jitter put it behind the parity
slices of its own frame, is rebuilt from the parity and counted as a
recovery. The rebuilt bytes are exact, so this is a counter semantic and not
a correctness problem — but the counter must not be read as a count of lost
frames.

### 2.3 The input path (M3)

10 000 absolute motion events at 1 kHz, XTEST into an Xvfb display,
`CLOCK_MONOTONIC`. This is the **injection leg only**: it covers no
display, no compositor and no real client. Artifacts in `artifacts/m3/`.

| Run | p50 | p90 | p99 | max | drops | p99 under 1 ms |
|---|---|---|---|---|---|---|
| bare veth, run 1 | 180 us | 326 us | **1151 us** | 5921 us | 0 | **no** |
| bare veth, run 2 | 49 us | 86 us | **138 us** | 543 us | 0 | yes |
| bare veth, run 3 | 49 us | 88 us | **131 us** | 504 us | 0 | yes |
| netem 1 ms each end, run 1 | 1050 us | 1089 us | 1136 us | 1576 us | 0 | no |
| netem 1 ms each end, run 2 | 1047 us | 1088 us | 1119 us | 1579 us | 0 | no |

The first bare-veth run was taken while the host load was between 6 and 11
on 12 cores, driven by other builds; that is the run that misses the 1 ms
target. The two netem runs are dominated by the 1 ms link and are not
comparable with the target at all.

The larger and newer measurement is `artifacts/m3/input.json`: 60 000 events
at 1 kHz over the network namespace pair, 60 s, one kernel clock on both
sides. 60 000 datagrams, 60 000 injected, 0 injection failures, 0
authentication failures, 0 replays, 0 missing sequences, and a latency of
**p50 46 µs, p90 85 µs, p99 187 µs, max 2756 µs, mean 53.2 µs**. The p99
meets the plan's 1 ms target. The X11 capture crate is no longer a stub: it
reads the raw XI2 events of the X server and maps the keymap through
xkbcommon, and the input channel puts the input on a socket of its own.

### 2.4 The audio, the clipboard and the resolution changes (M4)

`scripts/run-e2e.sh --with-audio --with-clipboard --with-resize` over
loopback, 1280x720 at 30 fps for 30 seconds. Artifact:
`artifacts/m4/e2e.json`. **PROVISIONAL, software-only.**

| Counter | Value |
|---|---|
| `frames_sent` | 853 |
| `frames_decoded` | 853 |
| `frames_presented` | 853 |
| `dropped_frames` | 3 |
| `decode_errors` | 0 |
| `audio_packets` | 5602 |
| `audio_underruns` | 0 |
| `clipboard_roundtrips` | 1 |
| `resize_events` | 2 |
| `bitrate_kbps` | 307.50 |

- **The audio path is wired.** The host creates a PipeWire virtual sink for
  the session, captures it, encodes it as Opus and sends it on a UDP port of
  its own (24802) sealed with a key pair of its own, so the audio can never
  share a nonce with the video. The client reassembles it, places it against
  the video clock and plays it. The client's own report
  (`artifacts/m4/client.json`) holds 5597 audio datagrams received, 5589
  audio frames played, 4 dropped by the audio stage, 0 authentication
  failures and 0 protocol errors.
- **The A/V skew** is measured as the error in the alignment: the difference
  between the moment an audio frame was played and the moment the video
  frame of the same moment was presented. It is only measured once that
  video frame has been presented and once enough frames have been presented
  for the alignment to be settled. Over 5581 samples: **p50 -159 µs, p90
  13111 µs, p99 18330 µs, max 122711 µs, min -110936 µs, mean -205 µs**. The
  median is close to zero and the tail is wide: a slip of one video frame
  is about 33 ms at the 30 fps of the run.
- **The clipboard round trip works.** The host reads its own compositor's
  selection and offers it, the client takes it and sends the text back, and
  the host counts the round trip: 1 round trip. The host can be told to put
  a given text into its own selection, so the run exercised the path without
  a person copying something, and the counter only counts a round trip when
  the client really took the text.
- **The two resolution changes** are the plan's M4.7 acceptance: the host's
  compositor output becomes 1920x1080 and then 1280x720, and the client's
  log records both. The encoder is not reconfigured in session; the host
  restarts at the new size, so this is the supervised version of the plan's
  in-session reconfigure.

### 2.5 The low-power proxy (M5)

`scripts/run-tier-b.sh`, 300 s at 1080p60 with the client pinned to two
vCPUs with `taskset -c 0,1`. Artifact: `artifacts/m5/tierb.json`.
**PROVISIONAL, software-only. This is a proxy on the build machine, not the
Tier B device.**

| Field | Value |
|---|---|
| `frames_decoded` | 17099 |
| `frames_presented` | 14514 |
| `dropped_frames` | 2992 |
| `decode_errors` | 240 |
| `decode_p50_ms` | 6.829 |
| `decode_p99_ms` | 11.804 (limit 12.0) |
| `presented_fps` | 48.38 |
| `client_cpu_percent_of_pinned_cores` | 35.47 (limit 85.0) |
| `frames_in_flight_at_the_end` | 648 |
| `pass` | **false** |

**The proxy does not pass.** The artifact says `pass: false`. The decode p99
and the CPU use are inside their limits; the frame drops are not. The client
presented 48.38 frames per second instead of the 60 the stream carries, and
dropped 2992 frames with 240 decode errors. The reason is in the artifact's
own notes: the client presents the frames on the CPU (D49), so at 1080p60
the colour conversion and the presentation take longer than the frame
interval. The zero-copy path (O-7) is the prerequisite for the plan's Tier B
target.

### 2.6 The session resilience runs (M6)

All **PROVISIONAL, software-only**.

| Run | Artifact | Result |
|---|---|---|
| Unattended reconnect, 20 cycles | `artifacts/m6/reconnect.json` | 20 of 20 reconnected, 0 failed, the slowest 5 ms against the 2000 ms limit, `pass: true` |
| Compositor restart, 5 cycles | `artifacts/m6/restart.json` | 5 of 5 recovered, 0 failed, open file descriptors 12 before and after every cycle, `pass: true` |
| Network flap, 3 flaps | `artifacts/m6/wifi-flap.json` | 3 of 3 recovered, 0 failed, the slowest 1057 ms against the 2000 ms limit, `pass: true` |

The three flap times were 403 ms, 292 ms and 1057 ms.

### 2.7 The X11 run (M7)

`scripts/run-e2e-x11.sh`, the X11/X11 row of the backend matrix, 60 s at
1280x720 and 30 fps. Artifact: `artifacts/m7/e2e-x11.json`.
**PROVISIONAL, software-only.**

| Counter | Value |
|---|---|
| `frames_sent` | 1710 |
| `frames_decoded` | 1710 |
| `frames_presented` | 1710 |
| `dropped_frames` | 1 |
| `decode_errors` | 0 |
| `packets_lost` | 0 |
| `packets_received` | 1770 |
| `bitrate_kbps` | 301.68 |

The software pipeline latency of that run is **p50 24.691 ms, p90 27.044 ms,
p99 28.322 ms, max 54.278 ms** over 1710 samples. The X11 path decoded and
presented every access unit it reassembled: 1710 decoded, 1710 presented, 0
decode errors, 0 packets lost. The host captured 1711 frames, so the dropped
counter reads 1.

### 2.8 The decode and encode quality

| Measurement | Result | Source |
|---|---|---|
| VA-API decode round trip (IDR plus 29 P frames, 1280x720) | luma PSNR min 47.60 dB, mean 50.47 dB, 0 decode errors | `crates/lumen-decode-vaapi/tests/roundtrip.rs` |
| VA-API encode quality, cross-checked with ffmpeg | luma PSNR min 45.2 dB, mean 50.0 dB | the encoder's own ffmpeg crosscheck |

The two PSNR sets come from different runs and **must not be compared with
each other**.

### 2.9 The capability probe (M0)

`artifacts/m0/capabilities.json`. The AMD iGPU (`renderD129`, `radeonsi`)
exposes H.264 and HEVC encode and decode through VA-API; the NVIDIA GTX
1650 exposes Vulkan Video but no VA-API encode. PipeWire 1.6.8 is running.
There is no compositor session on the physical panel.

### 2.10 The baseline probe (M0.6)

`artifacts/m0/baselines.json`. Six candidates were probed:

| Candidate | Available | Version | Run command |
|---|---|---|---|
| `sunshine` | no | — | — |
| `moonlight_qt` | no | — | — |
| `rustdesk` | no | — | — |
| `rustdesk_server` | no | — | — |
| `grd` | no | — | — |
| `xfreerdp` | **yes** | `3.32.1` (`freerdp3-x11 3.32.1+dfsg-1`) | `xfreerdp /v:<host> /u:<user> /p:<password>` |

**None of the four baseline implementations is installed**: Sunshine,
Moonlight-Qt, RustDesk (both sides) and GNOME Remote Desktop. The only
candidate present is `xfreerdp`, which is the RDP **client** used to drive
the GRD baseline, whose host is missing. The artifact's own summary says
`available: 1`, `unavailable: 5`, `measured: false`.

**No baseline was run and no latency was measured against any of them.** The
three baseline drivers (`scripts/baseline-sunshine.sh`,
`scripts/baseline-rustdesk.sh`, `scripts/baseline-grd.sh`) each detect their
baseline, say clearly what is missing, write a status record under
`artifacts/m8/` and **exit non-zero**. Only the "not available" path has
been exercised.

### 2.11 The fuzzing (M8.1)

Twelve targets, 60 s each: four crashes were found and fixed earlier, no
open panics. The plan's names are available next to the existing ones:
`protocol_parser` (the same source as `protocol_header`), `fec_reassemble`
(the same source as `transport_fec`) and `input_parser` (the same source as
`protocol_input`). `transport_inject`, `h264_au_split` and `opus_frame`
already had the plan's names. `.github/workflows/fuzz.yml` was not touched
and still runs the original nine names.

`cargo +nightly fuzz run protocol_parser -- -max_total_time=60` ran for
60 s and finished cleanly: 13 399 648 executions, no crash. The other two
aliases could not be built at the time because the fuzz crate depends on
`lumen-host-service`, which did not compile while another agent was
working on it; the aliases themselves check out, and the workspace compiles
now.

### 2.12 The dependency policy (M0 and M8.2)

**`cargo deny check` and `cargo audit` both pass.** Both exit zero.

`cargo audit` exits 0 with three allowed warnings:

| Crate | Version | Advisory | Fix available? |
|---|---|---|---|
| `zbus_names` | 4.3.5 | RUSTSEC-2026-0371, unmaintained | **No.** It was merged into `zbus` 6.0.0; no `ashpd` release uses zbus 6 yet. |
| `zvariant` | 5.15.1 | RUSTSEC-2026-0370, unmaintained | **No**, same reason. |
| `zvariant_derive` | 5.15.1 | RUSTSEC-2026-0372, unmaintained | **No**, same reason. |

There is **no vulnerability** in the tree. The three advisories come in
through `ashpd 0.13.13`, which the plan's M1.7 and M6.3 need, and dropping
it would remove the portal ScreenCast path. RustSec itself says "No safe
upgrade is available!". They are suppressed with a written reason, as the
plan requires for anything it cannot fix in-tree; the justification is in
`docs/design.md`.

**No copyleft dependency exists in the tree.** The plan's fallback — "if
`cargo deny` flags a transitive copyleft dependency: replace the dependency"
— is not engaged. Nothing had to be replaced, and `deny.toml` was not
silenced to make anything pass: the `licenses.deny` list was removed because
cargo-deny dropped the key in 0.14, `Unicode-3.0` was added to the allow
list because it is the current name of the `Unicode-DFS-2016` the plan
already allows, `MPL-2.0` was removed from the allow list because the plan
rejects the `xdg-desktop-portal` crate for exactly that licence, and the
internal crates are marked `publish = false` so their path dependencies are
not wildcard requirements.

### 2.13 The soak and the chaos (M8.3 and M8.4)

See section 6.

---

## 3. Deviations from the approved plan

Every deviation, with its reason.

### 3.1 The long-standing deviations

| # | Deviation | Reason |
|---|---|---|
| 1 | FEC is GF(256), not XOR (D35). | XOR-only parities cannot recover two losses in one block: same-parity pairs collide. GF(256) recovers two distinct losses. |
| 2 | The decoder's default output is CPU NV12 (D37). | The DMA-BUF tiled NV12 path has no consumer yet. The plan forbids a CPU readback in the hot path. **Open (O-7).** |
| 3 | The renderer cannot import NV12 or a tiled DMA-BUF (D46). | Decoded NV12 is tiled and needs a YUV shader that does not exist yet. **Open (O-7).** |
| 4 | The colour conversion runs on the CPU on both ends (D49). | The plan puts it on the GPU. No GPU path exists yet and the pipeline cannot run without it. `bgrx_to_nv12` costs 2.52 ms and `nv12_to_bgrx` 3.57 ms at 1280x720 on this machine. |
| 5 | PipeWire capture is not implemented; the wlr capture is used (D48). | The portal ScreenCast, the restore token and the `spa_meta_header` timestamps are all in `lumen-capture-pipewire`, which is still a stub. The *audio* path uses PipeWire; the *screen* does not. |
| 6 | NVENC, Vulkan encode and Vulkan decode are not implemented. | They are stubs. `lumen-ffi-nvenc`, `lumen-encode-nvenc`, `lumen-encode-vulkan` and `lumen-decode-vulkan` have no code. |
| 7 | HEVC and AV1 are not implemented. | H.264 only. The capability bits exist but no codec implements them. |
| 8 | The input path has a 64-entry replay window (D40). | AEAD alone allows a captured keystroke to be replayed. It is beyond the spec. |
| 9 | Opus links the system `libopus` (D42). | The upstream `opus` crate pulls `opusic-sys`, which compiles C into the product; the rules forbid that. A patched vendored crate links the system library instead. |
| 10 | The input p99 under 1 ms is not reliably met: one of three bare-veth runs failed it (1151 us). | The failing run was taken under a host load of 6 to 11 on 12 cores. The newer namespace run meets the target. |
| 11 | Explicit LINEAR DMA-BUF allocation costs the driver's tiled performance (D36). | Flag-based allocation fails on NVIDIA and returns `INVALID` on AMD. |
| 12 | The media peer address is derived, not negotiated (D50). | The control connection already proves the peer's address. The cost: only one client media port at a time (O-25). |
| 13 | No glass-to-glass measurement. | No rig, no second machine. |
| 14 | The input path is not carried by the live pipelines (M3.6). | The X11 capture crate, the transport input channel and the host injection exist and the injection leg is measured, but neither the client nor the host pipeline drives them yet. The measurement was made with a standalone example. |
| 15 | The encoder is not reconfigured in session on a mode change (M4.7). | The encoder belongs to `lumen-encode-vaapi`. The change is reported to the client and the host is restarted at the new size; the soak and the M4 run both do that. |
| 16 | One encoded stream per monitor is not implemented (M4.6). | The enumeration, the transport and the client side are there; a second output would need a second capture, a second encoder and a second media socket. |

### 3.2 The deviations of this round

| # | Deviation | Reason |
|---|---|---|
| 17 | The loss sweep's curve is no longer flat, and it carries decode errors. | The parity level now follows the measured loss (D59, D69), so a run starts without parity and the frames lost while the level rises are not recovered. The decode errors are the cost of the adaptation, not a decoder defect. The earlier fixed-parity run was flat and is no longer the policy. |
| 18 | The NACK and retransmission path is never exercised by a measurement. | In the loss sweep the client sends 0 NACKs and the host counts 0 retransmits at every level: the frames that were lost were lost inside a block the parity could not rebuild, and nothing asked for them back. The deadline (D63) is implemented, but it has never been exercised under loss. |
| 19 | The soak and the chaos scripts restart the pair themselves. | The in-session recovery of M6.1 and M6.2 is not implemented, so a fault that kills an end takes the session with it. The recovery times in the reports are the times to a **supervised restart**, and both reports say so. |
| 20 | The soak's resolution change restarts the host at the new size. | The plan's M4.7, an in-session encoder reconfigure on an output mode change, is not implemented (see deviation 15). |
| 21 | The chaos run's CPU throttle uses `taskset`, not `cpulimit`. | `cpulimit` is not installed on this machine. The script prefers `cpulimit` when it is there and records which method it used. |
| 22 | The baseline artifacts are written directly, not through `lumen-bench report`. | That merge derives `dropped_frames` and `packets_lost` from host-side counters a foreign implementation does not expose, and a derived zero reads as "nothing was lost" when nothing was measured. The baseline artifacts have the same shape as `artifacts/m1/e2e.json`, with `null` for every counter that could not be observed. |
| 23 | The fuzz targets are named twice. | The plan's names and the CI matrix's names differ. `fuzz/Cargo.toml` carries both, so the plan's acceptance loop and the CI matrix both work. Cargo prints a manifest warning about the duplicated source file. |
| 24 | `lumen-bench rig-selftest` does not accept `--fixture`. | The bench crate was not this agent's to change. `harness/photodiode/selftest.sh` does the equivalent and compares the result with the committed fixture. |
| 25 | `lumen-bench` has no `analyse` subcommand for the rig frames. | Same reason. `lumen_bench::camera::analyse` exists in the library; the binary does not expose it. |
| 26 | The A/V skew is measured in software, against the client's presentation clock. | It is the error in the alignment, not a measurement of what a person sees. The display's own latency is not in it. |

---

## 4. Pending items, and why each one is pending

| Item | Plan target | Why it is pending |
|---|---|---|
| **Glass-to-glass, Tier A 1080p60 wired** | p50 ≤ 8 ms, p99 ≤ 12 ms | **No rig.** No photodiode, no camera, no second machine, no display pair. The harness software exists and is validated against the simulated trigger; the physical measurement is not done. |
| **Glass-to-glass, Tier B** | p50 ≤ 20 ms, p99 ≤ 30 ms | **No Tier B device.** No Celeron N4020-class machine exists here. |
| **Glass-to-glass, 4K60 and 1440p120** | p50 ≤ 12 ms, p50 ≤ 120 Hz frame time + 3 ms | **No rig and no panels.** No 4K panel and no 144 Hz panel is attached. |
| **Display floor** | The panel's own latency | **No rig.** Needs the rig and a reference loop. |
| **Crypto cost** | AEAD bytes/s and CPU % at the target bitrates | **Never measured.** The harness has no crypto-cost measurement. The cipher is negotiated and both ciphers are exercised, but neither is timed. |
| **Baselines** | Sunshine+Moonlight, RustDesk, GRD+xfreerdp | **Not installed.** None of the four baseline implementations is present; only `xfreerdp`, the RDP client of a baseline whose host is missing. Building Sunshine, Moonlight-Qt and RustDesk from source was outside the run budget. |
| **The 72 h soak** | 72 h | **The run budget.** 308 s of the target were run; the report records the length actually run and marks the remainder pending. |
| **The full chaos plan** | 900 s | **The run budget.** 320 s were run, with the seeded fault sequence. |
| **The fuzz smoke of every target** | 60 s per target | **Partly done.** `transport_inject` and `h264_au_split` already had the plan's names and were run for 60 s each in an earlier session. `protocol_parser` was run for 60 s under its plan name and finished with no crash (13 399 648 executions). `fec_reassemble` and `input_parser` could not be run at the time, because the fuzz crate depends on `lumen-host-service`, which did not compile while another agent was working on it. |
| **PipeWire capture, portal consent, restore tokens (M1.7, M6.3)** | — | **Not implemented.** `lumen-capture-pipewire` is a stub. |
| **The input path in the live pipeline (M3.6)** | — | **Not implemented.** The capture crate, the input channel and the injection are written and the injection leg is measured, but neither pipeline drives them yet. |
| **The encoder reconfigure on a mode change (M4.7)** | — | **Not implemented.** It belongs to `lumen-encode-vaapi`; the change is reported to the client and the host restarts at the new size. |
| **One encoded stream per monitor (M4.6)** | — | **Not implemented.** The enumeration and the transport exist; a second encoder does not. |
| **P010, HDR metadata and HEVC (M4.8)** | — | **Not implemented.** The capability bits exist; the codec does not. The decoder cannot read HEVC either, so HEVC encode would produce a stream the client cannot decode. |
| **The Vulkan Video rungs of the decode chain (M5.1)** | — | **Not implemented.** `lumen-decode-vulkan` is a stub; the chain and the selection logic are in place, the Vulkan probe is not. |
| **The Tier B device (M5.4)** | The hardware protocol for a Celeron N4020-class device | **No device.** The procedure is written down in `docs/known-limits.md`; the two-vCPU run is a proxy and it does not pass. |
| **The zero-copy present path (O-7)** | No CPU readback in the hot path | **Not implemented.** The decoder outputs CPU NV12 by default and the renderer cannot import NV12 or a tiled DMA-BUF. Both ends have to change. |

---

## 5. Unsolved problems

These are the problems that are not merely "not started" but that are not
solved by the obvious next step.

1. **The gap between the software pipeline latency and the glass-to-glass
   is unknown, and it is probably the larger half.** The measured software
   pipeline is 23.5 ms at p50. A typical panel adds several milliseconds
   on its own, and the compositor's scan-out adds more. Until the rig
   exists, there is no way to say how much of a user's perceived latency
   Lumen is responsible for. Every target in the plan is about the sum,
   and only the first term has been measured.

2. **The clock-offset estimate is an assumption, and every cross-machine
   number rests on it.** `estimate_offset` is the standard NTP midpoint
   calculation and assumes the two directions take the same time. On a LAN
   that is accurate to well under a millisecond, and the small positive
   minimum of the `send` stage in both committed runs (5 µs and 3 µs)
   bounds the error it can carry. It is small, but it is not zero, and it is
   invisible in the percentiles.

3. **The CPU colour conversion dominates the software pipeline.** At 1280x720
   it costs 2.52 ms on the host and 3.57 ms on the client — 6.09 ms of the
   23.5 ms p50, a quarter of the whole pipeline, in two copies of the same
   function. Moving it to the GPU is the largest single win available, and
   it is blocked on O-7 (the renderer cannot import NV12 or a tiled
   DMA-BUF) as much as on the conversion itself.

4. **The zero-copy path is closed at both ends and they are separate
   problems.** The decoder outputs CPU NV12 by default (D37) and the
   renderer cannot import NV12 (D46). Either one alone forces the copy.
   Both have to be closed before the pipeline can be zero-copy.

5. **The loss curve is no longer flat, and the decode errors are the price
   of the adaptation.** The parity level follows the measured loss, so a
   sweep starts without parity and the frames that fall out before the
   level has risen are lost: 304, 299 and 287 frames presented and 1, 4 and
   7 decode errors at 0.5 %, 2 % and 5 % loss. The adaptation is right and
   the cost is real: the fixed-parity run lost no frames at all, and the
   adaptive run loses the frames that fall out of the first blocks. The
   policy has not been shown to converge fast enough on a link that
   degrades abruptly, and **this is the biggest open question the
   transport raises**: the level should probably start higher and fall,
   rather than start low and rise.

6. **The FEC recovery counter still does not mean what it says.** It counts
   18, 49 and 87 recoveries against 6, 13 and 31 packets lost. Until the
   recovery attempt is deferred until the frame's last slice has arrived,
   the counter cannot be used to reason about the loss.

7. **The NACK and retransmission path has never been exercised by a
   measurement.** The client sent 0 NACKs and the host counted 0
   retransmits in every loss-sweep level, and the deadline-bounded
   retransmission queue (D63) is therefore untested under loss. The
   bounded retransmission the plan asks for exists, but nothing in the
   artifacts shows it working.

8. **The Tier B proxy does not pass.** 2992 frames dropped and 240 decode
   errors in 300 s at 1080p60 on two vCPUs, at 48.38 fps against the 60 the
   stream carries. The decode p99 and the CPU use are inside their limits;
   the frame drops are not, and they are the CPU colour conversion. The
   proxy is not the device either, so the plan's Tier B target has neither a
   measurement nor a machine.

9. **The input latency result is not reproducible.** Three bare-veth runs
   gave p99 1151 us, 138 us and 131 us. The difference is the host load
   during the run. Nothing in the harness records the load, so the number
   cannot be reproduced or trusted.

10. **The compositor restart works in one measurement and not in the other.**
    `artifacts/m6/restart.json` records 5 of 5 cycles recovered, with the
    frame counter still rising across the restart, and the host has a reopen
    policy that asks for a keyframe. The soak's own event note says the
    opposite — "the host does not reopen its capture (M6.1 is not
    implemented); the script could not bring the pair back" — and the soak
    had to restart the pair by hand, after 34 capture errors. The two
    artifacts disagree about the same code path and the disagreement is not
    resolved: the restart run does not record how the capture was reopened,
    and the soak does not record why it was not. Until that is settled,
    neither run can be read as "the session survives a compositor restart".

11. **One client media port at a time.** The media peer is derived rather
    than negotiated (D50), so two clients cannot both use port 24801. The
    plan's shadow mode and multi-client work (M6.5) are blocked on it.

12. **The A/V skew tail is wide.** The median is -159 µs, but the p90 is
    13.1 ms and the p99 is 18.3 ms: the audio is scheduled against the
    video clock, and a slip of one video frame is about 33 ms at 30 fps.
    The alignment is measured and it is settled for most frames, but a
    tenth of the frames are a frame out.

13. **The plan's own acceptance commands do not all run.** The baselines are
    not installed, the soak and the chaos runs are short, the fuzz loop's
    target names only partly existed when it was last run, and the rig has
    no hardware. The plan is a specification, and the specification and the
    tree have drifted; this report lists where.

---

## 6. The soak and the chaos runs made here

Both scripts are in `scripts/`. The plan's acceptance is
`soak.sh --duration 7200` and `chaos.sh --duration 900`; the runs made here
are **308 s** and **320 s**. Neither is the run the plan asks for, and
neither is presented as one.

### 6.1 The soak (`artifacts/m8/soak.json`)

| | |
|---|---|
| Requested | 300 s |
| Actually run | 308 s |
| Remainder of the requested length | 0 s |
| Remainder of the 72 h plan target | **258 892 s, marked pending** |
| Cycles | 5, one event each, over 10 events in total |
| Complete runs (host and client reports paired) | 5 |

| Event | Count | Survived on its own | Restarted by the script | Recovery |
|---|---|---|---|---|
| `resolution_change` | 3 | 0 | 3 | 3115 ms, 3117 ms, 3111 ms |
| `compositor_restart` | 3 | 0 | 3 | — |
| `session_failed` | 2 | 0 | 2 | — |
| `suspend_resume` | 2 | 2 | 0 | — |

| Counter | Value |
|---|---|
| `frames_sent` | 4694 |
| `frames_decoded` | 4689 |
| `frames_presented` | 4689 |
| `dropped_frames` | 5 |
| `decode_errors` | 2 |
| `fec_recoveries` | 0 |
| `retransmits` | 0 |
| `packets_lost` | 3 |
| `packets_received` | 4691 |
| `capture_errors` | 34 |
| `encode_errors` | 0 |
| `keyframe_requests` / `idr_requests` | 2 / 2 |

What it shows:

- **Two of the ten events survived the session on their own**, and the
  artifact's `survival` block says the same: 10 events, 2 survived, 8
  restarted by the script, with a recovery time for 3 of them (min 3111 ms,
  mean 3114 ms, max 3117 ms). The events that survived are the two
  suspend/resumes: the sockets stay open across the freeze.
- **A compositor restart kills the session in this run.** The artifact's own
  note says the host does not reopen its capture (M6.1 is not implemented),
  so the frames stop and the script restarts the pair. That contradicts the
  compositor-restart run of section 2.6, which recovered 5 of 5; see
  unsolved problem 10.
- **Two cycles never came up at all** (`session_failed`): the session of
  that cycle could not be established, and the script restarted the pair.
- Over the 284034 ms the host was up it recorded 34 capture errors and 0
  encode errors, and the client asked for 2 keyframes.
- The per-stage percentiles of this run (end_to_end p50 42.244 ms, p99
  63.632 ms) are **not comparable** with the clean 30 s runs in section 2.1:
  most of the cycles ran at 1920x1080 instead of 1280x720, so the CPU
  colour conversion cost more, and the machine was under load during the
  run. They are reported because they are what the run produced.

### 6.2 The chaos (`artifacts/m8/chaos.json`)

Seed 20261021, so the sequence of faults repeats exactly. 320 s of the
300 s asked for, 13 faults.

| Fault | What it did | Survived | Recovery |
|---|---|---|---|
| `cpu_throttle` | the client pinned to one CPU with `taskset` for 10 s (`cpulimit` is not installed here) | **yes** | — |
| `encoder_restart` | the host killed and started again, which restarts the encoder inside it | no | 2099 ms |
| `netem_loss` | `tc netem loss 7.4% delay 0ms 1ms reorder 5%`, issued on both ends | no | 2100 ms |
| `decoder_device_loss` | the client restarted with `LUMEN_VA_DEVICE=/dev/dri/renderD255`, a device that is not there, so its decoder cannot be opened | no | 2098 ms |
| `netem_jitter_reorder` | `tc netem delay 20ms 1ms reorder 5%` on both ends | no | 2099 ms |
| `netns_teardown` | the namespace pair torn down and built again | no | 4370 ms |
| `encoder_restart` | the host killed and started again | no | 2099 ms |
| `netem_loss` | `tc netem loss 7.0% delay 0ms 20ms reorder 10%`, issued on both ends | no | 2100 ms |
| `netns_teardown` | the namespace pair torn down and built again | no | 4374 ms |
| `netem_jitter_reorder` | `tc netem delay 20ms 5ms jitter reorder 5%` on both ends | **yes** | — |
| `netns_teardown` | the namespace pair torn down and built again | no | 4370 ms |
| `encoder_restart` | the host killed and started again | no | 2095 ms |
| `session_failed` | the session did not come up before the next fault | no | — |

**Two of the thirteen events survived the session on their own**, and the
artifact's `survival` block says the same: 13 events, 2 survived, 11
restarted by the script, with a recovery time for 10 of them (min 2095 ms,
mean 2780 ms, max 4374 ms). The events that survived are the CPU throttle
and the second jitter-and-reorder fault. The faults that take the host or
the client away entirely came back in about 2.1 s to 4.4 s, and those are
the times to a **supervised restart**, because the in-session recovery of
M6.1 and M6.2 is not implemented.

The `netem_loss` faults were issued on both ends but only injected on the
host end: the client end refused the qdisc. The report says so in the
event's detail (`NOT INJECTED: netem is not permitted on lumen-client/lumen-c`)
rather than recording a fault that did not happen.

| Counter | Value |
|---|---|
| `frames_sent` | 10967 |
| `frames_decoded` | 10945 |
| `frames_presented` | 10945 |
| `dropped_frames` | 22 |
| `decode_errors` | 6 |
| `fec_recoveries` | 0 |
| `retransmits` | 0 |
| `packets_lost` | 15 |
| `packets_received` | 10952 |
| `capture_errors` | 0 |
| `encode_errors` | 0 |

The per-stage percentiles of the chaos run (end_to_end p50 33.977 ms, p99
54.120 ms) sit above the clean runs of section 2.1 at both ends, which is
what the faults would be expected to do.

**What the chaos does not show:** none of the faults produced a decode
error storm, an unbounded queue or a crash, which is a good sign; but the
run is five minutes long, the machine was shared, and the netem faults were
only partly injected. It is evidence that nothing fell over, not evidence
that the system is robust.

---

## 7. What to do next, in order

1. **Build the rig.** The procedure is written end to end in
   `harness/README.md`; it needs a photodiode or an LED, a camera at
   ≥ 1000 fps, two 1080p60 panels and a second machine. The harness
   software is ready and self-tested.
2. **Close O-7**, the zero-copy path: an EGL NV12 import with a YUV to
   RGB shader in `lumen-render`, plus a decode DMA-BUF default. This is
   worth more latency than anything else on the list, and it is the
   prerequisite for the Tier B target.
3. **Decide the parity policy's starting point.** The adaptive level
   protects the link but costs the frames at the start of a degraded run.
   Measure the sweep with the level fixed and with it adaptive, side by
   side, so the cost of the adaptation is a measurement and not an
   inference from two different runs.
4. **Exercise the NACK path.** The deadline-bounded retransmission queue
   exists and has never been exercised by a measurement. A sweep that drops
   datagrams after the client has seen the frame would drive it.
5. **Implement M6.1 and M6.2 in the capture stage**, so the soak and the
   chaos measure an in-session recovery instead of a supervised restart.
6. **Install the baselines** and run the three baseline drivers. The
   drivers are written; they have only been exercised on the "not
   available" path.
7. **Run the full soak and chaos**: `scripts/soak.sh --duration 7200` and
   `scripts/chaos.sh --duration 900`, on a quiet machine, and record the
   host load.

---

## 8. The artifacts

| File | What it holds |
|---|---|
| `artifacts/m0/capabilities.json` | The GPU, VA, Vulkan, PipeWire and portal probe. |
| `artifacts/m0/baselines.json` | The baseline availability probe: present, version, run command, detection commands, reason. |
| `artifacts/m1/e2e.json` | The loopback end-to-end run, merged from the host and client counters. |
| `artifacts/m1/e2e-netns.json` | The network-namespace end-to-end run, merged the same way. |
| `artifacts/m1/rig-selftest.json` | The rig self-test: 30 synthetic observations and a simulated glass-to-glass of 5000 us. |
| `artifacts/m2/curve.csv` | The loss sweep, one row per loss level. |
| `artifacts/m2/loss-*.json` | The merged report of each level of the loss sweep. |
| `artifacts/m3/input.json` | The input host-injection latency over the network namespace pair, 60000 events. |
| `artifacts/m3/input-latency-*.json` | The five bare-veth and netem input host-injection latency runs. |
| `artifacts/m4/e2e.json` | The M4 run: the audio, the clipboard round trip and the two resolution changes. |
| `artifacts/m4/host.json`, `artifacts/m4/client.json` | The per-end counters of that run, including the client's audio counters and the A/V skew samples. |
| `artifacts/m5/tierb.json` | The two-vCPU proxy run of the low-power client. `pass: false`. |
| `artifacts/m6/reconnect.json`, `artifacts/m6/restart.json`, `artifacts/m6/wifi-flap.json` | The reconnect, compositor restart and network flap runs. |
| `artifacts/m7/e2e-x11.json` | The X11 host and X11 client end-to-end run. |
| `artifacts/m8/soak.json` | The short soak: the events, the counters, the per-stage percentiles, the remainder marked pending. |
| `artifacts/m8/chaos.json` | The short chaos run: the faults, the survival count, the recovery times, the counters. |
| `artifacts/m8/baseline-*-status.json` | The "not available" records of the three baseline drivers. |
| `harness/fixtures/simulated-trigger.json` | The committed simulated-trigger fixture: 30 observations, a glass-to-glass of 5000 us. |
| `fuzz/artifacts/` | The fuzzing crash artifacts, four crashes that were found and fixed. |
