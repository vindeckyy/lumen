# Benchmarks

**No glass-to-glass latency has been measured.** The photodiode and camera rig does not exist in this environment, and no second machine is available. Every latency target in the spec is unmeasured. The numbers below are functional checks, tooling results and software pipeline latencies. None of them is a latency result for the targets, and none should be quoted as one.

Every result in this file is **PROVISIONAL**. None of it is glass-to-glass. None of it comes from the target hardware. Each entry names the raw artifact it came from.

The counters move with the load on the machine. A rerun of the same script gives different frame counts, different percentiles and sometimes a different bitrate. The **artifacts are the record**; the prose in this file describes the run that produced them, and every number below is quoted from the named artifact rather than recomputed.

## Status against the latency targets

| Target | Status |
|---|---|
| Tier A 1080p60 wired, p50 at most 8 ms, p99 at most 12 ms | Not measured. Pending rig and client hardware |
| Tier B low-power 1080p60 wired, p50 at most 20 ms, p99 at most 30 ms | Not measured. No low-power client. A two-vCPU proxy run exists and is reported below; it is not the Tier B device, and it does not pass |
| 4K60 Tier A, p50 at most 12 ms | Not measured. Pending rig |
| 1440p120 Tier A, p50 at most 120 Hz frame time plus 3 ms | Not measured. Pending rig |
| Input path, p99 under 1 ms on LAN | **Partly measured.** The host-injection leg over the network namespace pair has a p99 of 187 µs over 60000 events, which meets the 1 ms target. The leg is software-only: no display, no compositor and no real client are in the measurement, and the live pipeline does not carry the input yet. See the input injection section below |
| Display floor (the panel's own latency) | Not measured. Pending rig |
| Baseline comparison: Moonlight with Sunshine, RustDesk, GNOME Remote Desktop, Apollo or Vibepollo | Not run. None of the four is installed; the probe found only `xfreerdp`, which is the RDP client of the GRD baseline, whose host is missing |
| Software pipeline latency, capture to present, 1280x720 at 30 fps | **PROVISIONAL, software-only.** p50 23.520 ms, p99 27.369 ms over loopback and p50 23.498 ms, p99 27.917 ms over the network namespaces, from the 30 s runs below. Not a target result: it excludes the display's own scan-out and the camera path, and the two clocks are aligned by an estimate |
| A/V sync (M4.3) | **PROVISIONAL, software-only.** The A/V skew of the M4 run is p50 -159 µs, p90 13.111 ms, p99 18.330 ms over 5581 samples. It is the error in the alignment, not a glass-to-glass figure |

## Software-measured results (provisional)

All of these are **PROVISIONAL / software-only**. They were produced on the build machine over the network namespaces, on the loopback interface, or against the local AMD iGPU. None is a glass-to-glass result and none is a Tier A or Tier B result.

| Measurement | Method | Result | Artifact |
|---|---|---|---|
| Input host-injection latency, bare veth, run 1 | 10000 absolute motion events at 1 kHz, XTEST into Xvfb `:93`, `CLOCK_MONOTONIC`, host load 6 to 11 on 12 cores | **PROVISIONAL, software-only.** p50 180 µs, p99 1151 µs (fails under 1 ms), max 5921 µs, 0 drops | `artifacts/m3/input-latency-bare-veth.json` |
| Input host-injection latency, bare veth, run 2 | same | **PROVISIONAL, software-only.** p50 49 µs, p99 138 µs, max 543 µs, 0 drops | `artifacts/m3/input-latency-bare-veth-run2.json` |
| Input host-injection latency, bare veth, run 3 | same | **PROVISIONAL, software-only.** p50 49 µs, p99 131 µs, max 504 µs, 0 drops | `artifacts/m3/input-latency-bare-veth-run3.json` |
| Input host-injection latency, netem 1 ms each end, run 1 | same, with `netem delay 1ms` on both ends of the veth pair | **PROVISIONAL, software-only.** p50 1050 µs, p99 1136 µs, max 1576 µs, 0 drops. The 1 ms link dominates, so this is not comparable to the target | `artifacts/m3/input-latency-netem-1ms-run1.json` |
| Input host-injection latency, netem 1 ms each end, run 2 | same | **PROVISIONAL, software-only.** p50 1047 µs, p99 1119 µs, max 1579 µs, 0 drops. The 1 ms link dominates, so this is not comparable to the target | `artifacts/m3/input-latency-netem-1ms-run2.json` |
| Input host-injection latency, network namespace pair, 60000 events | 1000 events per second for 60 s over the veth pair of `scripts/netns-up.sh`, one kernel clock on both sides | **PROVISIONAL, software-only.** p50 46 µs, p90 85 µs, p99 187 µs, max 2756 µs, 60000 datagrams, 60000 injected, 0 injection failures, 0 replays. The p99 meets the plan's 1 ms target | `artifacts/m3/input.json` |
| Decode roundtrip (VaDecoder2's encoder output, `renderD129`) | 30 frames 1280x720, IDR plus 29 P | **PROVISIONAL, software-only.** Luma PSNR min 47.60 dB, mean 50.47 dB, 0 decode errors | `tests/roundtrip.rs` output (in `crates/lumen-decode-vaapi`) |
| Encode quality (VaEncoder2, ffmpeg decode) | 30-frame IPPP, 1280x720 | **PROVISIONAL, software-only.** Luma PSNR min 45.2 dB, mean 50.0 dB | VaEncoder2 report |
| Transport and session loopback | real TCP and UDP on 127.0.0.1, 5000-byte AU, Noise handshake | **PROVISIONAL, software-only.** Functional pass. No latency measured | `lumen-host-service` test (in `crates/lumen-host-service/src/session.rs`) |
| Fuzz | twelve targets, 60 s each | **PROVISIONAL, software-only.** Four crashes found and fixed, no open panics | `fuzz/artifacts/` |
| Capture verification | headless sway, 800x600, DMA-BUF XR24 linear | **PROVISIONAL, software-only.** Every pixel read back correct using the reported stride | `wlroots` test output (in `crates/lumen-capture-wlr`) |
| End-to-end pipeline, loopback, 30 s | `lumen-host` to `lumen-client` over 127.0.0.1, two headless sway compositors, 1280x720 at 30 fps | **PROVISIONAL, software-only.** 873 frames sent, 873 decoded, 873 presented, 2 dropped, 0 decode errors, 0 FEC recoveries, 0 retransmits, 0 packets lost, 883 packets received, measured bitrate 314.9 kbps. Software pipeline latency p50 23.520 ms, p99 27.369 ms, max 51.184 ms. See the end-to-end section below | `artifacts/m1/e2e.json` |
| End-to-end pipeline, network namespaces, 30 s | `lumen-host` in the `lumen-host` namespace at 10.77.0.1 and `lumen-client` in `lumen-client` at 10.77.0.2, over a veth pair, 1280x720 at 30 fps | **PROVISIONAL, software-only.** 873 frames sent, 872 decoded, 872 presented, 4 dropped, 0 decode errors, 0 FEC recoveries, 0 retransmits, 1 packet lost, 882 packets received, measured bitrate 315.7 kbps. Software pipeline latency p50 23.498 ms, p99 27.917 ms, max 47.462 ms. See the end-to-end section below | `artifacts/m1/e2e-netns.json` |
| Loss sweep, 0.5 %, 2 % and 5 % loss with 10 ms jitter | netem on both ends of the veth pair, 12 s per level, 1280x720 at 30 fps | **PROVISIONAL, software-only.** The curve is **no longer flat**: 304, 299 and 287 frames presented at the three levels, with 1, 4 and 7 decode errors and 18, 49 and 87 FEC recoveries, 0 retransmits. The adaptive policy starts without parity and the frames lost while the level rises are not recovered | `artifacts/m2/curve.csv` |
| M4 run: audio, clipboard and two resolution changes, 30 s | `lumen-host` to `lumen-client` over 127.0.0.1 with `--with-audio --with-clipboard --with-resize`, 1280x720 at 30 fps | **PROVISIONAL, software-only.** 853 frames sent, 853 decoded, 853 presented, 3 dropped, 0 decode errors, 5602 audio packets, 0 audio underruns, 1 clipboard round trip, 2 resolution changes, measured bitrate 307.5 kbps. A/V skew p50 -159 µs, p90 13.111 ms, p99 18.330 ms over 5581 samples. See the M4 section below | `artifacts/m4/e2e.json` |
| Low-power client, two-vCPU proxy, 1080p60, 300 s | the client pinned to two vCPUs with `taskset -c 0,1`, decoding 1080p60 | **PROVISIONAL, software-only.** 17099 frames decoded and 14514 presented, 2992 dropped, 240 decode errors, decode p50 6.829 ms and decode p99 11.804 ms against the 12 ms limit, so the run does not pass. The client used 35.47 % of the two pinned cores and presented 48.38 fps. A proxy on the build machine, not the Tier B device | `artifacts/m5/tierb.json` |
| Unattended reconnect, 20 cycles | the client is killed and started again, once per cycle; the host keeps the session | **PROVISIONAL, software-only.** 20 of 20 cycles reconnected, 0 failed, the slowest reconnect 5 ms against the 2000 ms limit | `artifacts/m6/reconnect.json` |
| Compositor restart, 5 cycles | the host compositor is killed and started again, once per cycle | **PROVISIONAL, software-only.** 5 of 5 sessions recovered, 0 failed, open file descriptors 12 before and after every cycle, so no descriptor leak | `artifacts/m6/restart.json` |
| Network flap, 3 flaps | the host end of the veth pair goes down for 2 s and comes back up, three times | **PROVISIONAL, software-only.** 3 of 3 flaps recovered, 0 failed, the slowest reconnect 1057 ms against the 2000 ms limit | `artifacts/m6/wifi-flap.json` |
| End-to-end pipeline, X11 host and X11 client, 60 s | the X11 row of the matrix: an X server is captured and the client presents into an X server | **PROVISIONAL, software-only.** 1711 frames captured, 1710 frames sent, 1770 datagrams received, 1710 access units, 1710 decoded, 1710 presented, 1 dropped, 0 decode errors, measured bitrate 301.7 kbps. Software pipeline latency p50 24.691 ms, p99 28.322 ms | `artifacts/m7/e2e-x11.json` |
| Soak, short run | 308 s over the namespace pair, with resolution changes, compositor restarts, interface flaps and suspend/resumes | **PROVISIONAL, software-only.** 4689 frames decoded and presented, 5 dropped, 2 decode errors. Two of the ten events survived the session on their own; the remainder of the 72 h target is pending | `artifacts/m8/soak.json` |
| Chaos, short run | 320 s over the namespace pair, one fault per cycle, seed 20261021 | **PROVISIONAL, software-only.** 10945 frames decoded and presented, 22 dropped, 6 decode errors. Two of the thirteen events survived the session on their own | `artifacts/m8/chaos.json` |
| Rig self-test | `lumen-bench rig-selftest`, synthetic camera observations | **PROVISIONAL, software-only.** 30 synthetic observations, a simulated glass-to-glass of 5000 µs, and a barcode round trip that reads back the frame index and the host timestamp. Harness arithmetic only, no rig | `artifacts/m1/rig-selftest.json` |
| Photodiode harness, software only | the wiring, the v4l2 capture loop, the trigger finder and the self-test of `harness/` | **PROVISIONAL, software-only.** The harness is written and validated against the committed simulated-trigger fixture. The physical measurement is pending | `harness/fixtures/simulated-trigger.json` |

Notes on the input latency rows. The handoff quotes p50 and p99 for the three bare-veth runs and p99 only for the two netem runs. Every remaining figure comes from the named artifact files, which record 10000 datagrams, 10000 injected, and zero inject failures, authentication failures, malformed packets, replays and missing sequences in every run. The measured span is the injection leg only, between the client stamping the event and the host injecting it. It covers no display, no compositor and no real client.

The `artifacts/m3/input.json` run is a different measurement: 60000 events at 1 kHz over the namespace pair, with the same span (client stamp to host injection call) and the same single kernel clock on both sides. It has its own section below.

The decoder and encoder rows do not share a source. The encoder figures come from an ffmpeg decode of the encoder's own output; the decoder figures come from a separate round trip. The two PSNR sets must not be compared with each other.

The first bare-veth run was taken while host load was between 6 and 11 on 12 cores, driven by other builds. Re-measure on a quiet machine and record the load before trusting any of these numbers.

One row in that table changed shape, and it is worth saying why: the loss sweep used to be flat, because the parity level was fixed and the FEC rebuilt every frame that was missing a slice. The parity level now follows the measured loss, so the run that starts without parity loses the frames that fall out before the level has risen. That is why the loss sweep row above is no longer flat, and why it now carries decode errors.

## End-to-end pipeline latency (provisional)

The M1 host and client pipelines run end to end, and the pipeline writes
a timestamp at every stage boundary, so the merged report carries per-stage
percentiles.
`scripts/run-e2e.sh` starts two headless sway compositors, runs `lumen-host`
against one and `lumen-client` against the other, and merges the two counter
reports into `artifacts/m1/e2e.json` with `lumen-bench report`. The mode is
chosen with `MODE=loopback` or `MODE=netns`. Both runs below are 1280x720 at
30 fps for 30 seconds.

The report stores the stage percentiles in microseconds. The tables below give
them in milliseconds, which is the unit `lumen-bench` prints and the unit its
assertions use.

**Run A — loopback.** Both ends on 127.0.0.1. Artifact:
`artifacts/m1/e2e.json`.

| Counter | Value |
|---|---|
| `frames_sent` | 873 |
| `frames_decoded` | 873 |
| `frames_presented` | 873 |
| `dropped_frames` | 2 |
| `decode_errors` | 0 |
| `fec_recoveries` | 0 |
| `retransmits` | 0 |
| `packets_lost` | 0 |
| `packets_received` | 883 |
| `audio_packets` | 0 |
| `audio_underruns` | 0 |
| `clipboard_roundtrips` | 0 |
| `resize_events` | 0 |
| `bitrate_kbps` | 314.94584632832436 |
| `queue_depth_max` | 0 |

| Stage | Samples | p50 (ms) | p90 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|---|
| capture | 873 | 8.031 | 8.636 | 9.402 | 21.533 |
| encode | 873 | 3.680 | 5.092 | 5.332 | 5.668 |
| send | 867 | 2.408 | 4.579 | 5.014 | 5.081 |
| recv | 872 | 0.034 | 0.050 | 0.061 | 4.303 |
| decode | 872 | 4.212 | 4.496 | 4.750 | 11.601 |
| present | 872 | 4.703 | 5.011 | 6.599 | 7.784 |
| end_to_end | 872 | 23.520 | 25.818 | 27.369 | 51.184 |

The `dropped_frames` counter is the difference between the frames the host
captured and the frames the client decoded. It is not a transport loss
counter, and the two counters that are (`packets_lost` and `decode_errors`)
are both 0 in this run. The frame counts depend on the load on the machine,
so a rerun gives different numbers. The merged report holds every counter,
including the ones the table has no column for.

**Run B — network namespaces.** The host in the `lumen-host` namespace at
10.77.0.1, the client in `lumen-client` at 10.77.0.2, over the veth pair
from `scripts/netns-up.sh`. Artifact: `artifacts/m1/e2e-netns.json`.

| Counter | Value |
|---|---|
| `frames_sent` | 873 |
| `frames_decoded` | 872 |
| `frames_presented` | 872 |
| `dropped_frames` | 4 |
| `decode_errors` | 0 |
| `fec_recoveries` | 0 |
| `retransmits` | 0 |
| `packets_lost` | 1 |
| `packets_received` | 882 |
| `audio_packets` | 0 |
| `audio_underruns` | 0 |
| `clipboard_roundtrips` | 0 |
| `resize_events` | 0 |
| `bitrate_kbps` | 315.66183574879227 |
| `queue_depth_max` | 0 |

| Stage | Samples | p50 (ms) | p90 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|---|
| capture | 873 | 8.175 | 8.807 | 10.207 | 18.748 |
| encode | 873 | 3.668 | 5.082 | 5.270 | 6.156 |
| send | 872 | 2.484 | 4.609 | 5.033 | 5.115 |
| recv | 872 | 0.024 | 0.050 | 0.057 | 0.481 |
| decode | 872 | 4.100 | 4.368 | 4.708 | 11.951 |
| present | 872 | 4.708 | 5.065 | 5.568 | 8.191 |
| end_to_end | 872 | 23.498 | 25.783 | 27.917 | 47.462 |

### What each stage measures
The two stages that do the pixel work are the two that dominate. The host
converts the captured BGRX frame to NV12 on the CPU before the encoder sees
it; the client converts the decoded NV12 back to BGRX before the compositor
sees it. Measured on this machine at 1280x720, `bgrx_to_nv12` takes 2.52 ms
and `nv12_to_bgrx` takes 3.57 ms. The client's present stage, whose p50 is
4.703 ms, is therefore mostly the colour conversion. The host's capture
stage, whose p50 is 8.031 ms, contains that same 2.52 ms conversion plus the
DMA-BUF readback it feeds. This is the cost the plan wanted on the GPU
(D36, D49); moving it there is the largest single latency win left.


The pipeline stamps a time at every boundary, and each stage is the interval
between two of those stamps:

| Stage | Interval it measures |
|---|---|
| capture | the capture timestamp to the start of the encode |
| encode | the encoder's own time |
| send | the end of the encode to the client receiving the access unit |
| recv | the reassembly to the start of the decode |
| decode | the decoder's own time |
| present | the end of the decode to the frame being presented |
| end_to_end | the capture timestamp to the presentation, the whole pipeline |

`end_to_end` is the **software pipeline latency**: capture to present on the
client's clock, with the two clocks aligned by the offset the host estimates
from the ping exchange. It is **not** glass-to-glass. It excludes the
display's own scan-out, and it excludes the camera path entirely.

For every frame the six stage deltas add up to the `end_to_end` value of that
frame, so the table's p50 column does not add up to the p50 of the total: a
percentile of a sum is not the sum of the percentiles. The `capture` stage is
dominated by waiting for the next frame at the stream rate rather than by the
cost of the copy itself.

The `send` stage has a small positive minimum in both runs: 5 µs on loopback
and 3 µs in the namespaces. That is the floor of the two stamps that bracket
the stage, and it is also the size of the error of the clock offset estimate:
the client stamps `recv` when the datagram arrives, which is after the host
stamped `send`, so the offset cannot be wrong by more than a few
microseconds here. It is still an estimate, and it is carried by every figure
above.

Both runs are **PROVISIONAL, software-only**. They are numbers from a
software pipeline on the build machine, not glass-to-glass latency, and they
are not a Tier A or Tier B result.

## Loss sweep (provisional)

Plan milestone M2.8 asks for a degradation curve at 0.5 %, 2 % and 5 % loss
with 10 ms jitter. `scripts/sweep-loss.sh` produced it. Every level runs the
host and the client over the veth pair of `scripts/netns-up.sh` with a netem
qdisc on both ends, 1280x720 at 30 fps for 12 seconds. The table is
`artifacts/m2/curve.csv`, one row per level.

| loss_rate | jitter_ms | frames_sent | frames_decoded | frames_presented | dropped_frames | decode_errors | fec_recoveries | retransmits | packets_lost | packets_received | bitrate_kbps |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.005 | 10 | 308 | 304 | 304 | 7 | 1 | 18 | 0 | 6 | 363 | 333.958 |
| 0.02 | 10 | 309 | 299 | 299 | 12 | 4 | 49 | 0 | 13 | 495 | 461.460 |
| 0.05 | 10 | 308 | 287 | 287 | 24 | 7 | 87 | 0 | 31 | 635 | 603.188 |

The raw artifacts of every level are `artifacts/m2/loss-0.005.json`,
`artifacts/m2/loss-0.02.json` and `artifacts/m2/loss-0.05.json`, the merged
reports, and the per-end counters `artifacts/m2/loss-0.005-host.json` and
`artifacts/m2/loss-0.005-client.json`, and the same pair for the other two
levels. Every level captured 311 frames and ran for 11.1 s; the host sent
369, 508 and 666 datagrams of 464202, 639064 and 837828 bytes, and the
client received 363, 495 and 635 datagrams and reassembled 306, 304 and
298 access units.

**The curve is no longer flat, and the reason is the adaptive parity.** The
policy starts with no parity at all and raises the level only when the
measured loss asks for it, so the frames that fall out of a block before the
level has risen are lost and are not recovered. The frames presented fall
from 304 to 299 to 287 as the loss rate rises, and the decode errors rise
from 1 to 4 to 7. The retransmit counter is 0 at every level, and so is the
client's NACK counter, so the deadline-aware retransmission was not exercised
by this sweep at all: what was lost was lost inside a block the parity could
not rebuild.

What the fixed-parity run showed, for comparison: with the parity level
fixed, the earlier sweep was flat — 313 frames sent and 312 presented at
every level, with no decode error and no retransmit at any level, because the
FEC rebuilt every frame that was missing a slice. That run's artifacts were
replaced by the adaptive run above. The flat curve was not a property of the
link; it was a property of the fixed policy.

The merged reports of the three levels also carry the per-stage percentiles.
The `send` stage is the one the link touches, and it is the one that moves
with the loss rate: its p50 is 11.020 ms, 14.845 ms and 7.464 ms at the three
levels, against 2.408 ms over loopback and 2.484 ms in the namespaces in the
two lossless runs above. The end-to-end figures follow, p50 31.939 ms,
36.115 ms and 28.057 ms and p99 44.255 ms, 49.018 ms and 43.613 ms.

| loss_rate | send p50 (ms) | send p99 (ms) | recv p50 (ms) | end_to_end p50 (ms) | end_to_end p99 (ms) | parity level changes |
|---|---|---|---|---|---|---|
| 0.005 | 11.020 | 22.946 | 0.029 | 31.939 | 44.255 | 2 |
| 0.02 | 14.845 | 27.305 | 0.027 | 36.115 | 49.018 | 8 |
| 0.05 | 7.464 | 22.156 | 0.029 | 28.057 | 43.613 | 16 |

The three levels are separate 12 second runs, so the link timing does not
order with the loss rate: the 5 % level has a lower `send` p50 and a lower
end-to-end p50 than the 2 % level, and it presented fewer frames. What the
counter of parity level changes does order with the loss rate: 2, 8 and 16
changes at the three levels, read from the per-end host reports. The policy
is chasing the loss at 5 % and is nearly still at 0.5 %.

### Caveat: the FEC recovery counter still overstates the loss

`fec_recoveries` reads 18, 49 and 87 at the three levels, against 6, 13 and
31 packets lost. The counter is above the number of lost packets, so a slice
that is merely **late**, because the 10 ms jitter put it behind the parity
slices of its own frame, is rebuilt from the parity and counted as a
recovery. The rebuilt bytes are exact, so this is a counter semantic and not
a correctness problem, but the counter must not be read as a count of lost
frames.

## The M4 run: the audio, the clipboard and the resolution changes (provisional)

Plan milestone M4 asks for the audio path, the clipboard round trip and the
resolution changes. `scripts/run-e2e.sh --with-audio --with-clipboard
--with-resize` produced one run that exercises all three, over loopback at
1280x720 and 30 fps for 30 seconds. Artifact: `artifacts/m4/e2e.json`.

| Counter | Value |
|---|---|
| `frames_sent` | 853 |
| `frames_decoded` | 853 |
| `frames_presented` | 853 |
| `dropped_frames` | 3 |
| `decode_errors` | 0 |
| `fec_recoveries` | 0 |
| `retransmits` | 0 |
| `packets_lost` | 0 |
| `packets_received` | 863 |
| `audio_packets` | 5602 |
| `audio_underruns` | 0 |
| `clipboard_roundtrips` | 1 |
| `resize_events` | 2 |
| `bitrate_kbps` | 307.49626482563286 |
| `queue_depth_max` | 0 |

The per-end report `artifacts/m4/client.json` holds the client's own view of
the audio: 5597 audio datagrams received, 5589 audio frames played and 4
dropped by the audio stage, 0 authentication failures and 0 protocol errors.
The playback scheduler reports 5593 frames scheduled, 0 dropped as late and 4
dropped because no video clock was available yet.

| Stage | Samples | p50 (ms) | p90 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|---|
| capture | 853 | 4.144 | 7.871 | 18.868 | 24.146 |
| encode | 853 | 3.558 | 5.022 | 6.434 | 6.780 |
| send | 847 | 2.680 | 4.617 | 5.006 | 5.047 |
| recv | 852 | 0.022 | 0.049 | 0.054 | 0.087 |
| decode | 852 | 5.306 | 6.067 | 7.076 | 10.827 |
| present | 852 | 4.798 | 5.169 | 11.194 | 19.451 |
| end_to_end | 852 | 21.018 | 24.647 | 47.028 | 59.277 |

The A/V skew is measured as the error in the alignment: the difference
between the moment an audio frame was played and the moment the video frame
of the same moment was presented. It is only measured once that video frame
has been presented and once enough frames have been presented for the
alignment to be settled, which is why the sample count (5581) is larger than
the number of video frames in the run.

| Skew | Samples | p50 (µs) | p90 (µs) | p99 (µs) | max (µs) | min (µs) | mean (µs) |
|---|---|---|---|---|---|---|---|
| `a_vs_skew` | 5581 | -159 | 13111 | 18330 | 122711 | -110936 | -205 |

A negative skew means the audio was played before the video frame of the
same moment reached the screen. The median is close to zero and the tail is
wide: the audio is scheduled against the video clock, and a slip of one
video frame is about 33 ms at the 30 fps of this run, which is the order of
the p90 and the p99 here. The skew is an alignment error measured in
software, and it says nothing about the display.

The two resolution changes are the plan's M4.7 acceptance: the host's
compositor output becomes 1920x1080 and then 1280x720, and the client's log
records both changes. The encoder is not reconfigured in session; the host
restarts at the new size, so this is the supervised version of the plan's
in-session reconfigure.

## Input injection latency (provisional)

Plan milestone M3.7 asks for the timestamp at the client event receipt and at
the host injection call, with the host clock mapped through the control
channel, reported as p50 and p99 excluding the client's own polling
interval. `scripts/run-input-latency.sh` produced it over the veth pair of
`scripts/netns-up.sh`. Artifact: `artifacts/m3/input.json`.

The run is 60 s at 1000 events per second, 1920x1080, with the client
polling the X server every 1000 µs. Both namespaces share one kernel clock,
so the measured offset is 0 µs and no clock mapping was needed. The CPUs were
not pinned; the host load was 3.21 / 1.91 / 1.84 before the run and
1.90 / 1.76 / 1.79 after it.

| Field | Value |
|---|---|
| `expected` | 60000 |
| `datagrams` | 60000 |
| `injected` | 60000 |
| `inject_failures` | 0 |
| `authentication_failures` | 0 |
| `malformed` | 0 |
| `replays` | 0 |
| `missing` | 0 |
| `negative_samples` | 0 |
| `p99_below_1ms` | true |
| `aead` | `Aes256Gcm` |
| pointer position | expected 443, 109; observed 443, 109; match true |

| Latency | Value |
|---|---|
| `count` | 60000 |
| `min` | 12 µs |
| `p50` | 46 µs |
| `p90` | 85 µs |
| `p99` | 187 µs |
| `max` | 2756 µs |
| `mean` | 53.2 µs |

The p99 of 187 µs meets the plan's 1 ms target, and nothing was dropped,
replayed or refused on the way. The maximum of 2756 µs is the tail of the
scheduler on a machine that is not idle; the median is 46 µs, more than an
order of magnitude below the target.

What this measures is the **injection leg only**: the client stamps the
event, the event travels one datagram over the veth pair, and the host calls
the injection function. It covers no display, no compositor and no real
client, and the live host and client pipelines do not carry the input yet.

## Low-power client, two-vCPU proxy (provisional)

Plan milestone M5.3 asks for a Tier B proxy: the client pinned to two vCPUs
with `taskset -c 0,1`, decoding 1080p60 for N minutes, reporting the decode
percentiles and the CPU use. `scripts/run-tier-b.sh` produced it. Artifact:
`artifacts/m5/tierb.json`.

**This is a proxy, not the Tier B device.** The plan's Tier B client is a
Celeron N4020, which does not exist in this environment. The run below is
the build machine with the client restricted to two of its cores, so it says
something about the shape of the workload and nothing about the hardware.

| Field | Value |
|---|---|
| `tier` | `b-proxy` |
| `duration_s` | 300 |
| `frames_decoded` | 17099 |
| `frames_presented` | 14514 |
| `dropped_frames` | 2992 |
| `decode_errors` | 240 |
| `decode_p50_ms` | 6.829 |
| `decode_p99_ms` | 11.804 |
| `decode_p99_limit_ms` | 12.0 |
| `presented_fps` | 48.38 |
| `client_cpu_percent_of_pinned_cores` | 35.47 |
| `client_cpu_limit_percent` | 85.0 |
| `client_cpu_seconds` | 212.38 |
| `cpu_count` | 2 |
| `cpus` | `0,1` |
| `frames_in_flight_at_the_end` | 648 |
| `pass` | false |

The run **does not pass**, and the artifact says so: `pass: false`. Two of
the three limits hold — the decode p99 of 11.804 ms is inside the 12 ms limit
and the client used 35.47 % of the two pinned cores, well under the 85 %
limit — but 2992 frames were dropped, 240 decode errors were counted, and
the client presented 48.38 frames per second instead of the 60 the stream
carries. The plan's "no frame drops" is what fails.

The artifact gives the reason in its own `notes` field: the client presents
the frames on the CPU (decision D49), so at 1080p60 the colour conversion and
the presentation take longer than the frame interval and the client drops the
frames that are late. The zero-copy path (O-7) is the prerequisite for the
Tier B target of the plan. The 648 frames in flight at the end of the run are
the frames the low-power profile was still holding when the run stopped.

## Session resilience: reconnect, compositor restart, network flap (provisional)

Plan milestones M6.1, M6.2 and M6.6. Three scripts, three artifacts, all
**PROVISIONAL, software-only**.

**Unattended reconnect** (`scripts/run-reconnect.sh`, artifact
`artifacts/m6/reconnect.json`). The client is killed and started again, once
per cycle, and the host keeps the session between the cycles. The measured
span is the whole reconnect, from the start of the client process, which is
more than the plan asks for: the plan measures from the moment the link is
up, and the process start is earlier.

| Field | Value |
|---|---|
| `cycles` | 20 |
| `reconnected` | 20 |
| `failed` | 0 |
| `max_reconnect_ms` | 5 |
| `limit_ms` | 2000 |
| `pass` | true |

Every cycle established its session, and the slowest of the 20 took 5 ms
against the 2000 ms limit. The 20 times run from 1 ms to 5 ms.

**Compositor restart** (`scripts/run-compositor-restart.sh`, artifact
`artifacts/m6/restart.json`). The host compositor is killed and started
again, once per cycle. The host has to reopen the capture and carry on, and
it must not leak a file descriptor per cycle.

| Field | Value |
|---|---|
| `cycles` | 5 |
| `recovered` | 5 |
| `failed` | 0 |
| `open_fd_growth` | 0 |
| `pass` | true |

| Cycle | Frames before | Frames after | Open fds before | Open fds after | Recovered |
|---|---|---|---|---|---|
| 1 | 90 | 120 | 12 | 12 | yes |
| 2 | 120 | 150 | 12 | 12 | yes |
| 3 | 150 | 180 | 12 | 12 | yes |
| 4 | 180 | 210 | 12 | 12 | yes |
| 5 | 210 | 240 | 12 | 12 | yes |

Every cycle recovered and the client kept counting frames across the restart,
so the session carried on. The open file descriptor count was 12 before and
after every cycle, so nothing leaked. The artifact records no recovery time
and does not say how the capture was reopened: the soak run below records
that the in-session recovery of the capture is not implemented, so this
result must not be read as a reopened capture.

**Network flap** (`scripts/run-wifi-flap.sh`, artifact
`artifacts/m6/wifi-flap.json`). The host end of the veth pair goes down for
2 s and comes back up, three times. The client runs with `--reconnect`, so it
comes back on its own with the 200 ms to 2 s backoff of the plan.

| Field | Value |
|---|---|
| `flaps` | 3 |
| `recovered` | 3 |
| `failed` | 0 |
| `max_reconnect_ms` | 1057 |
| `limit_ms` | 2000 |
| `pass` | true |

| Flap | Reconnect | Sessions before | Sessions after | Recovered |
|---|---|---|---|---|
| 1 | 403 ms | 0 | 2 | yes |
| 2 | 292 ms | 3 | 4 | yes |
| 3 | 1057 ms | 4 | 5 | yes |

The slowest of the three took 1057 ms, inside the 2000 ms limit. The three
times are 403 ms, 292 ms and 1057 ms, so the time does not grow with the
number of flaps: the third one is the slowest and the second one is the
fastest.

## The X11 end-to-end run (provisional)

Plan milestone M7 asks for the backend matrix: the four rows of capture and
presentation over Wayland and X11. `scripts/run-e2e-x11.sh` runs the X11/X11
row: an X server is captured and the client presents into an X server.
Artifact: `artifacts/m7/e2e-x11.json`. The run is 60 s at 1280x720 and
30 fps.

| Counter | Value |
|---|---|
| `frames_sent` | 1710 |
| `frames_decoded` | 1710 |
| `frames_presented` | 1710 |
| `dropped_frames` | 1 |
| `decode_errors` | 0 |
| `fec_recoveries` | 0 |
| `retransmits` | 0 |
| `packets_lost` | 0 |
| `packets_received` | 1770 |
| `bitrate_kbps` | 301.6796788998594 |
| `queue_depth_max` | 0 |

| Stage | Samples | p50 (ms) | p90 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|---|
| capture | 1710 | 8.824 | 9.409 | 10.884 | 25.402 |
| encode | 1710 | 4.852 | 5.261 | 5.490 | 6.239 |
| send | 1708 | 2.566 | 4.587 | 5.038 | 5.088 |
| recv | 1710 | 0.024 | 0.050 | 0.059 | 4.826 |
| decode | 1710 | 4.169 | 4.676 | 5.022 | 11.276 |
| present | 1710 | 4.420 | 4.766 | 5.598 | 8.191 |
| end_to_end | 1710 | 24.691 | 27.044 | 28.322 | 54.278 |

The X11 path decoded and presented every access unit it reassembled: 1710
access units, 1710 decoded, 1710 presented, 0 decode errors. The host
captured 1711 frames and sent 1710, so the dropped counter reads 1: it is the
difference between what the host captured and what the client decoded, not a
transport loss, and `packets_lost` is 0.

The bitrate of 301.7 kbps is much lower than the 314.9 kbps of the Wayland
run. The two runs show different content, and a bitrate is a property of the
content as much as of the encoder, so the two numbers must not be compared
as if they measured the same thing.

## The soak and the chaos run, short (provisional)

Plan milestones M8.3 and M8.4 ask for a long soak and a chaos run. The run
budget of this environment does not allow the full length, so both runs are
short: the length actually run is recorded, and the soak marks the remainder
of the target as pending in the artifact itself.

**The soak** (`scripts/soak.sh`, artifact `artifacts/m8/soak.json`).
Requested duration 300 s, actual 308 s, over the namespace pair at 1280x720
and 30 fps, with one event per cycle. The plan targets 72 h, and the
artifact records 258892 s of the target as pending.

| Field | Value |
|---|---|
| `milestone` | M8.3 |
| `cycles` | 5 |
| `complete_runs` | 5 |
| `duration_actual_s` | 308 |
| `requested_duration_s` | 300 |
| `plan_target_duration_s` | 259200 |
| `plan_remainder_s` | 258892 |
| `pending` | true |
| `frames_sent` | 4694 |
| `frames_decoded` | 4689 |
| `frames_presented` | 4689 |
| `dropped_frames` | 5 |
| `decode_errors` | 2 |
| `packets_lost` | 3 |
| `packets_received` | 4691 |
| `retransmits` | 0 |
| `fec_recoveries` | 0 |
| `bitrate_kbps` | 166.31958145855776 |

| Stage | Samples | p50 (ms) | p90 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|---|
| capture | 4694 | 11.537 | 18.683 | 20.153 | 24.500 |
| encode | 4694 | 5.929 | 9.202 | 9.848 | 10.696 |
| send | 4687 | 11.509 | 14.375 | 15.007 | 15.114 |
| recv | 4687 | 0.024 | 0.050 | 0.059 | 8.586 |
| decode | 4687 | 5.216 | 6.884 | 7.636 | 13.279 |
| present | 4687 | 6.661 | 10.959 | 15.280 | 19.019 |
| end_to_end | 4687 | 42.244 | 58.321 | 63.632 | 75.498 |

| Event | Detail | Session survived |
|---|---|---|
| resolution_change | the host output becomes 1920x1080 and the host restarts at that size | no, the script restarted the pair after 3115 ms |
| compositor_restart | the host compositor is killed and started again after 3 s | no, the script could not bring the pair back |
| session_failed | the session of that cycle did not come up | no, the script restarted the pair |
| suspend_resume | the host and the client are stopped with SIGSTOP for 3 s and continued | yes |
| resolution_change | the host output becomes 1440x900 and the host restarts at that size | no, the script restarted the pair after 3117 ms |
| compositor_restart | the host compositor is killed and started again after 3 s | no, the script could not bring the pair back |
| session_failed | the session of that cycle did not come up | no, the script restarted the pair |
| suspend_resume | the host and the client are stopped with SIGSTOP for 3 s and continued | yes |
| resolution_change | the host output becomes 1280x720 and the host restarts at that size | no, the script restarted the pair after 3111 ms |
| compositor_restart | the host compositor is killed and started again after 3 s | no, the script could not bring the pair back |

**Two of the ten events survived the session on their own**, and the
artifact's `survival` block says the same: 10 events, 2 survived, 8 restarted
by the script, and a recovery time for 3 of them (min 3111 ms, mean 3114 ms,
max 3117 ms). The events that survived are the two suspend/resumes: the
sockets stay open across the freeze. The compositor restart kills the
session, because the host does not reopen its capture — the in-session
recovery of M6.1 is not implemented — and the two `session_failed` events
are cycles whose session never came up. The three recovery times are the
times to a supervised restart of the pair, not to an in-session recovery.
Over the 284034 ms the host was up it recorded 34 capture errors and 0
encode errors, and the client asked for 2 keyframes.

**The chaos run** (`scripts/chaos.sh`, artifact `artifacts/m8/chaos.json`).
Requested duration 300 s, actual 320 s, with one fault per cycle, picked from
a seeded sequence so the run can be repeated exactly (seed 20261021). The
requested length was reached, so the artifact does not mark this run pending,
but it records that the rest of the chaos plan was not run.

| Field | Value |
|---|---|
| `milestone` | M8.4 |
| `cycles` | 13 |
| `complete_runs` | 9 |
| `duration_actual_s` | 320 |
| `requested_duration_s` | 300 |
| `seed` | 20261021 |
| `frames_sent` | 10967 |
| `frames_decoded` | 10945 |
| `frames_presented` | 10945 |
| `dropped_frames` | 22 |
| `decode_errors` | 6 |
| `packets_lost` | 15 |
| `packets_received` | 10952 |
| `retransmits` | 0 |
| `fec_recoveries` | 0 |
| `bitrate_kbps` | 305.27166971370724 |

| Stage | Samples | p50 (ms) | p90 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|---|
| capture | 10967 | 8.161 | 8.799 | 9.884 | 21.224 |
| encode | 10967 | 4.566 | 5.191 | 5.427 | 6.982 |
| send | 10937 | 12.581 | 14.652 | 32.479 | 39.622 |
| recv | 10937 | 0.025 | 0.051 | 0.063 | 0.986 |
| decode | 10937 | 4.120 | 4.441 | 4.810 | 12.984 |
| present | 10937 | 4.591 | 4.956 | 7.318 | 9.577 |
| end_to_end | 10937 | 33.977 | 36.411 | 54.120 | 62.025 |

| Fault | Detail | Session survived |
|---|---|---|
| cpu_throttle | the client is pinned to CPU 6 with `taskset` for 10 s, because `cpulimit` is not installed | yes |
| encoder_restart | the host process is killed and started again, which restarts the encoder | no, the script restarted the pair after 2099 ms |
| netem_loss | `tc netem` with 7.4 % loss, 1 ms delay and 5 % reorder. **Not injected**: netem is not permitted on the `lumen-client` namespace | no, the script restarted the pair after 2100 ms |
| decoder_device_loss | the client is restarted with `LUMEN_VA_DEVICE=/dev/dri/renderD255`, so its decoder cannot be opened | no, the script restarted the pair after 2098 ms |
| netem_jitter_reorder | `tc netem` with 20 ms delay, 1 ms jitter and 5 % reorder | no, the script restarted the pair after 2099 ms |
| netns_teardown | the network namespace pair is torn down and built again | no, the script restarted the pair after 4370 ms |
| encoder_restart | the host process is killed and started again | no, the script restarted the pair after 2099 ms |
| netem_loss | `tc netem` with 7.0 % loss, 20 ms delay and 10 % reorder. **Not injected**: netem is not permitted on the `lumen-client` namespace | no, the script restarted the pair after 2100 ms |
| netns_teardown | the network namespace pair is torn down and built again | no, the script restarted the pair after 4374 ms |
| netem_jitter_reorder | `tc netem` with 20 ms delay, 5 ms jitter and 5 % reorder | yes |
| netns_teardown | the network namespace pair is torn down and built again | no, the script restarted the pair after 4370 ms |
| encoder_restart | the host process is killed and started again | no, the script restarted the pair after 2095 ms |
| session_failed | the session did not come up before the next fault | no, the script restarted the pair |

**Two of the thirteen events survived the session on their own**, and the
artifact's `survival` block says the same: 13 events, 2 survived, 11 restarted
by the script, with a recovery time for 10 of them (min 2095 ms, mean
2780 ms, max 4374 ms). The events that survived are the CPU throttle and the
second jitter-and-reorder fault. The three that took the host or the client
away entirely — the encoder restart, the decoder device loss and the
namespace teardown — came back in about 2.1 s, 2.1 s and 4.4 s, and those
are the times to a supervised restart of the pair, because the in-session
recovery of M6.1 and M6.2 is not implemented. Both `netem_loss` faults were
not injected: the netem qdisc is not permitted in the `lumen-client`
namespace, and the artifact says so in the event itself.

The chaos run recorded 6 decode errors and 22 dropped frames, and the host
recorded 0 capture errors and 0 encode errors over the 361553 ms of the run.

## Asserting on a run report

`lumen-bench report` takes `--assert <expression>`, `--input <path>` and
`--provisional`, so a run can fail its own gate instead of only being read by
a human.

An expression is a name, an operator and a number, for example
`frames_decoded>1700` or `p99_present_ms<=12`. The operators are `>`, `>=`,
`<`, `<=`, `==` and `!=`. The names are the counter fields (`frames_sent`,
`frames_decoded`, `frames_presented`, `dropped_frames`, `decode_errors`,
`fec_recoveries`, `retransmits`, `packets_lost`, `packets_received` and
`bitrate_kbps`) and the stage percentiles `p50_<stage>_ms`,
`p90_<stage>_ms`, `p99_<stage>_ms`, `max_<stage>_ms` and `mean_<stage>_ms`,
where `<stage>` is `capture`, `encode`, `send`, `recv`, `decode`, `present` or
`end_to_end`. The percentile names are in milliseconds, even though the
report stores the stages in microseconds. The command exits non-zero when
an assertion fails.

`--input <path>` asserts against a report that is already merged, so the host
and the client files are not needed again. `--provisional` stamps the output
report provisional, whatever the input says. The output path defaults to
`artifacts/m1/e2e.json`, so pass `--out` as well when you assert against a
committed artifact and do not want it overwritten.

Worked example, against the committed loopback artifact:

```console
$ lumen-bench report --input artifacts/m1/e2e.json --out /tmp/assert.json \
    --assert 'frames_decoded>800' --assert 'p99_end_to_end_ms<=40' --assert 'decode_errors==0'
  ok   frames_decoded>800
  ok   p99_end_to_end_ms<=40
  ok   decode_errors==0
Lumen run report
  capture: wlroots-screencopy
  client_access_units: 873
  client_datagrams_received: 883
  client_keyframe_requests: 0
  client_nacks_sent: 0
  codec: h264-vaapi
  duration_ms: 28216
  duration_s: 30
  fps: 30
  host_bytes_sent: 1110814
  host_datagrams_sent: 883
  host_frames_captured: 875
  host_keyframe_requests: 0
  mode: loopback
  size: 1280x720
  transport: custom-udp+fec-gf256
  frames: sent 873 decoded 873 presented 873 dropped 2
  errors: decode 0 | fec recoveries 0 | retransmits 0 | packets lost 0
  bitrate: 314.9 kbps
  stage percentiles (ms):
    capture      p50 8.031  p90 8.636  p99 9.402  max 21.533  n 873
    decode       p50 4.212  p90 4.496  p99 4.750  max 11.601  n 872
    encode       p50 3.680  p90 5.092  p99 5.332  max 5.668  n 873
    end_to_end   p50 23.520  p90 25.818  p99 27.369  max 51.184  n 872
    present      p50 4.703  p90 5.011  p99 6.599  max 7.784  n 872
    recv         p50 0.034  p90 0.050  p99 0.061  max 4.303  n 872
    send         p50 2.408  p90 4.579  p99 5.014  max 5.081  n 867
  PROVISIONAL: measured without the photodiode rig: these are software pipeline latencies, not glass-to-glass
$ echo $?
0
```

The exit status is 0 because all three assertions held. A failing assertion
prints `FAIL` with the actual value and the command exits non-zero:

```console
$ lumen-bench report --input artifacts/m1/e2e.json --out /tmp/assert.json \
    --assert 'p99_end_to_end_ms<=12'
  FAIL p99_end_to_end_ms<=12: 27.369 is not <= 12
```

### Rig self-test

`lumen-bench rig-selftest` passes. It builds 30 synthetic camera
observations, computes a simulated glass-to-glass of 5000 µs from them, and
reads a barcode back out of a synthetic camera image: the round trip returns
the frame index and the host timestamp that were encoded into it. That
exercises the harness arithmetic and the barcode reader without a
photodiode or a camera. The 5000 µs is an input to the simulation, not a
measurement of anything.

## Functional checks run (not latency)

All numbers are from the build machine (Ubuntu 26.10, kernel 7.3.0-9, AMD Ryzen 5 4600H, 15 GiB RAM). Times are wall clock for the command.

| Check | Environment | Result |
|---|---|---|
| Network namespace round trip, host to client over veth, 3 pings | Two namespaces, no netem | RTT 0.045 / 0.049 / 0.055 ms (min / avg / max). Software loopback only, not a target result |
| VA-API H.264 I-slice decode, 64x64 fixture | AMD renderD129 (radeonsi, Mesa 26.2.4) | Output matches ffmpeg reference decode (Y 126, UV 128 everywhere) |
| VA-API NV12 encode, 1280x720 synthetic | AMD renderD129 | Output starts with Annex-B start code and contains an IDR NAL. Encode time not recorded |
| VA-API `derive_image` and `map`, 256x128 NV12 | AMD renderD129 | Pitches [256, 256, 0], data size 49152 |
| DMA-BUF export from VA surface | AMD renderD129 | NV12, modifier 0x0200000000000901, size 131072 |
| wlroots DMA-BUF capture | Headless sway, gles2 renderer | Output HEADLESS-1 at 800x600, DMA-BUF used. Pixel (0,0) is red |
| wlroots wl_shm capture | Headless sway, pixman renderer | DMA-BUF not used. wl_shm path passes |
| X11 MIT-SHM capture, 10 consecutive runs | Xvfb | 10 of 10 pass |
| X11 capture through Xwayland | Xwayland :50 | SHM backend, pixel check passes |
| Wayland present (shm and dmabuf commit) | Headless sway | Test passes. `present_count` increments. Present timestamp from `wp_presentation` |
| X11 present on Xvfb, 1920x1080 and 640x360 BGRX | Xvfb :98 | `present_count` and Present timestamp checked |
| PipeWire virtual-sink capture, one chunk | Live PipeWire 1.6.8 daemon | Monitor audio received within 5 s |
| Opus round trip, mono and stereo, 2.5 / 5 / 10 ms frames | System libopus 1.6.1 | Correlation above 0.9 on 50 frames of a 440 Hz sine, lag-searched. All frame sizes produce the right length |
| Control channel, encrypted round trip and tamper rejection | Loopback TCP | Passes. Wrong key is rejected; oversized length is rejected before allocation. The crate has 11 unit tests, including the Noise session round trip over real TCP and UDP |
| uinput device creation | sudo, /dev/uinput | 5 virtual devices created. Event delivery not checked |
| XTEST absolute motion | Xvfb :91 | Pointer read back at (400, 300) |
| Photodiode trigger simulation | Software only | Glass-to-glass arithmetic and barcode round trip pass. Not a hardware result |

## Not measured

These are explicit non-results. Do not fill any of them in without a new measurement and a new artifact.

- **Glass-to-glass: not measured.** No photodiode or camera rig exists. The
  harness software is written and validated against the simulated trigger,
  but the physical measurement is pending.
- **The display floor: not measured.** It needs the same rig.
- **Crypto cost: not measured.**
- **Baselines (Sunshine, Moonlight, RustDesk, GRD): not run.** The probe in
  `artifacts/m0/baselines.json` found only `xfreerdp`, which is the RDP client
  of the GRD baseline, whose host is missing; the other five candidates are
  not installed and no latency was measured against any of them.
- **Tier B (Celeron N4020): no device.** The two-vCPU run is a proxy, and the
  proxy does not pass.
- **The long soak: not run to length.** 308 s of the 72 h target; the
  remainder is pending in the artifact.

## Raw data

- `artifacts/m0/capabilities.json`: GPU, VA, Vulkan, PipeWire, and portal probe output.
- `artifacts/m0/baselines.json`: the baseline availability probe. One candidate available, five not, nothing measured.
- `artifacts/m1/e2e.json`: the loopback end-to-end run, merged from the host and client counters.
- `artifacts/m1/e2e-netns.json`: the network-namespace end-to-end run, merged the same way.
- `artifacts/m1/host.json`, `artifacts/m1/client.json`: the per-end counters of one run. `scripts/run-e2e.sh` rewrites both files on every run, so they hold the most recent run only.
- `artifacts/m1/rig-selftest.json`: the rig self-test fixture, 30 synthetic observations and a simulated glass-to-glass of 5000 µs.
- `artifacts/m2/curve.csv`: the loss sweep, one row per loss level.
- `artifacts/m2/loss-0.005.json`, `artifacts/m2/loss-0.02.json`, `artifacts/m2/loss-0.05.json`: the merged report of each level of the loss sweep.
- `artifacts/m2/loss-<rate>-host.json`, `artifacts/m2/loss-<rate>-client.json`: the per-end counters of each level of the loss sweep.
- `artifacts/m3/input.json`: the input host-injection latency over the network namespace pair, 60000 events.
- `artifacts/m3/input-latency-bare-veth.json`, `artifacts/m3/input-latency-bare-veth-run2.json`, `artifacts/m3/input-latency-bare-veth-run3.json`: bare-veth input host-injection latency runs.
- `artifacts/m3/input-latency-netem-1ms-run1.json`, `artifacts/m3/input-latency-netem-1ms-run2.json`: netem 1 ms per end input host-injection latency runs.
- `artifacts/m4/e2e.json`: the M4 run, with the audio, the clipboard round trip and the two resolution changes.
- `artifacts/m4/host.json`, `artifacts/m4/client.json`: the per-end counters of that run, including the client's own audio counters.
- `artifacts/m5/tierb.json`: the two-vCPU proxy run of the low-power client.
- `artifacts/m6/reconnect.json`, `artifacts/m6/restart.json`, `artifacts/m6/wifi-flap.json`: the reconnect, compositor restart and network flap runs.
- `artifacts/m7/e2e-x11.json`: the X11 host and X11 client end-to-end run.
- `artifacts/m8/soak.json`, `artifacts/m8/chaos.json`: the short soak, with the remainder of the 72 h target marked pending, and the short chaos run.
- `harness/fixtures/simulated-trigger.json`: the committed fixture of the photodiode harness self-test.
- `fuzz/artifacts/`: fuzzing crash artifacts.
- `/tmp` outputs from the agent runs are not kept in the repository. The commands are listed above so they can be reproduced.

## What to measure next

1. Run the procedure of `harness/README.md` on a machine that has the rig, and record the display's own latency, so every target can be reported relative to the floor.
2. Re-measure the pipeline latency on a quiet machine, with the host load recorded, and with the clock offset checked in both directions.
3. Measure the baselines with the same harness: Moonlight with Sunshine, RustDesk, and GNOME Remote Desktop in RDP mode.
4. Run the soak for its full length, and the rest of the chaos plan.
5. Re-run the loss sweep with the parity level fixed and with it adaptive, side by side, so the cost of the adaptation is a measurement rather than an inference from two different runs.

## The latency plan: before and after (provisional)

Software pipeline latency on this machine, not glass-to-glass. Before: `artifacts/latency/baseline-*` (P0, the code before the latency change). After: `artifacts/latency/after-*` (the GPU path, `WLR_RENDERER=gles2`). The reports were read with `lumen-bench report --input <file> --out <tmp>`.

| Run | Before | After |
|---|---|---|
| Loopback 30 s, presented / sent | 872 / 872 | 870 / 870 |
| Loopback, zero-copy presents | 0 | 870 |
| Loopback, frames converted on the GPU / host frames captured | 0 | 871 / 871 |
| Loopback end-to-end p50 / p99 (ms) | 19.02 / 22.82 | 8.59 / 11.95 |
| Loopback client present p50 (ms) | 4.37 | 0.047 |
| Netns end-to-end p50 / p99 (ms) | 19.18 / 23.24 | 8.58 / 11.99 |
| Netns client present p50 (ms) | 4.46 | 0.047 |
| Loss sweep 0.5 / 2 / 5 %, decode errors | 3 / 4 / 7 | 1 / 1 / 0 |
| Loss sweep 0.5 / 2 / 5 %, frames presented of sent | 299/307, 296/308, 278/301 | 302/305, 303/308, 307/309 |
| Two-vCPU proxy 300 s, dropped frames | 5 | 0 |
| Two-vCPU proxy 300 s, decode p50 / p99 (ms) | 5.18 / 6.32 | 2.28 / 2.95 |
| M4 run with audio, clipboard and resize, end-to-end p50 (ms) | n/a | 8.31, decode errors 0, resize events 2 |

The after loopback run has one frame more converted than presented, and the M4 run has one packet lost. Neither changed a decode error.

**The two-vCPU proxy on the CPU path (`WLR_RENDERER=pixman`), dropped frames over 300 s:**

| Run | Block after a failed picture | Dropped frames |
|---|---|---|
| Before the latency change (`baseline-tierb.json`) | 300 s | 5 |
| After, first run (`pixman-tierb.json`) | 60 s | 33 |
| After, second run (`rerun-pixman-tierb.json`) | 60 s | 29 |
| After, experiment (`experiment-block300-pixman-tierb.json`) | 300 s | 14 |

One run each except the two of the current code. The block length is the largest cause the runs show, not the only one: 14 is still above 5. Each run was made alone on the machine. The block length is open (known-limits O-39).

**Input.** `artifacts/latency/input.json`: 60000 events, p50 46 µs, p99 115 µs, 0 failures, 0 replays. This is the example injector with XTEST, not the client capture path (known-limits O-38).

**The GPU path in numbers.** The colour conversion on the CPU costs 6.81 ms at 1080p; the GPU path has no CPU conversion on the frames it handles. The VPP output matches the CPU conversion to a worst channel difference of 2 at 64x64. The 1280x720 comparison skips for lack of a fixture (known-limits O-40).

