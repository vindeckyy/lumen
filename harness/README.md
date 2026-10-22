# The photodiode rig: the full procedure

Plan milestone **M8.6**: "Photodiode and camera harness: rig procedure,
capture loop (v4l2-ctl to file), barcode decode, glass-to-glass computation,
display-floor measurement; validated end to end with the simulated trigger."

This file is the runbook. It says what hardware is needed, how to wire it,
how to aim the camera, how to capture, how to turn the recording into a
number, and how to measure the display's own floor. It is written so that
somebody who has never seen the rig can execute it on a machine that has
one.

**Status in this repository: the rig does not exist in this environment.**
No photodiode, no high-speed camera rig, no second machine and no Tier B
device. The harness *software* is built and is validated end to end against
the simulated trigger (`harness/photodiode/selftest.sh`), but **no
glass-to-glass number has been measured**. Every glass-to-glass target in
`docs/plan.md` and `docs/benchmarks.md` is marked **PENDING**, and it stays
that way until somebody runs this procedure on a machine with the rig.

The wiring, the part list and the trigger detection are in
[`photodiode/README.md`](photodiode/README.md). This file is the procedure
that uses them.

---

## 0. The two numbers this rig produces

| Number | What it is | How it is produced |
|---|---|---|
| **Glass-to-glass** | The time between a pixel changing in the host's *presented output* and the same pixel changing on the client's panel. | The camera sees the host's panel and the client's panel in the same frame; the diode marks the host's presentation. |
| **Display floor** | The latency of a panel by itself: the time between the moment a pixel is written to the panel and the moment its backlight changes. | The same rig, with the trigger driven straight from the graphics output and no Lumen in the loop. |

The software pipeline latency that `lumen-bench` reports is a **third**
number, and it is not one of these: it stops when the client hands the
frame to the compositor. The difference between the glass-to-glass and the
software pipeline latency is everything the software does not see: the
compositor's scan-out, the panel, and the camera path.

Never quote a software pipeline latency as a glass-to-glass latency. The
report writer stamps every software number `provisional: true` for exactly
this reason.

---

## 1. The hardware

| Item | Requirement | Notes |
|---|---|---|
| Photodiode **or** LED | A photodiode with a transimpedance amplifier, or a bright white LED | The LED is simpler and, for a time reference, just as good. The photodiode is faster and does not load the host. |
| Trigger source | A GPIO line on the host, or an audio output | A GPIO is exact. An audio output works when the host has no spare GPIO: a 3.3 V pulse through a 1 kΩ series resistor into the LED. |
| Camera | **At least 1000 fps**, global shutter preferred, USB 3.0 or GigE | One camera frame at 1000 fps is 1000 us, so an 8 ms latency is eight frames. Below 500 fps the resolution of the measurement is not good enough for a 12 ms p99 target. |
| Host panel | 1920x1080 at 60 Hz | The test pattern is 1920x1080. |
| Client panel | 1920x1080 at 60 Hz | The same, so the two panels show the same content. |
| Optics | The camera must see **both panels and the diode at once** | See section 3. |
| Mounting | A rigid tripod or a bracket | The camera must not move between the calibration and the measurement. |
| Second machine | The client | The rig measures the distance between two machines. |

Optional but recommended: a second camera or a second run, so the display
floor can be measured on each panel with the camera in the same place.

---

## 2. Wiring the diode to the host

### 2.1 With an LED (the simple case)

```
   host GPIO (3.3 V) ---[1 kΩ]---[LED]--- GND
```

- The LED's anode goes to the resistor, the resistor to the GPIO.
- The LED's cathode goes to the host's ground.
- A 1 kΩ resistor gives about 1.7 mA through a white LED at 3.3 V. That is
  dim but the camera's gain is enough; if the camera cannot see it, use
  330 Ω (about 6 mA) and move the LED further from the host's screen so
  the camera does not see the reflection.
- Put a piece of black tape over the LED's body so only the lens is
  visible. The camera must see the LED, not its housing.

### 2.2 With a photodiode

```
   host GPIO ---[1 kΩ]---[LED in series]---[photodiode]---[10 kΩ]--- GND
                     (or: the host's audio out into a comparator)
```

A photodiode needs a transimpedance amplifier to turn its current into a
voltage; without one it is nearly useless. A practical minimum:

```
   +3.3 V ---[10 kΩ]--- A --- photodiode --- K --- GND
                       |
                       +----> GPIO input (through a 1 kΩ series resistor)
```

with the diode reverse biased. Any of these works:

- a dedicated transimpedance amplifier IC;
- an op-amp inverting stage with a 100 kΩ feedback resistor and a 10 nF
  compensation capacitor;
- a photodiode module with an amplifier on it, which is what most people
  actually use.

### 2.3 Driving the trigger from the host

The trigger has to be fired **at the moment the host's panel shows the new
frame**, not at the moment the host captures it. The capture timestamp is
several stages earlier than the presentation:

```
   capture -> colour convert -> encode -> packetise -> send -> reassemble
           -> decode -> colour convert -> present -> compositor -> scan-out
```

The diode marks the **scan-out**, which is the last of those steps.

The way the host drives the diode is therefore part of the host's present
path, and it must be documented with the measurement. Two honest options:

1. **The diode is driven by the compositor.** A small helper on the host
   watches the compositor's commit and pulses the GPIO on every frame
   commit. This measures the frame as the panel receives it, which is what
   the glass-to-glass is defined against. It adds the GPIO write's latency
   (a few microseconds) to the measurement, which has to be stated.
2. **The diode is driven by the panel's own VSync.** Connect the diode to
   the monitor's vertical sync through a comparator. This removes the
   software from the trigger entirely, at the price of tying the trigger to
   the panel's refresh rather than to Lumen's frames.

Either way, **the trigger delay has to be measured and stated**, because it
is part of the number. The simulated trigger in `harness/fixtures` models
exactly this: a `trigger_delay_us` between the host's timestamp and the
diode firing. It is not part of the glass-to-glass; it aligns the two
clocks.

---

## 3. Aiming the camera

The camera has to see three things at once:

1. the top left corner of the **host** panel, where the barcode is,
2. the top left corner of the **client** panel, where the barcode is,
3. the **diode**.

Layout, viewed from the camera:

```
    +---------------------------------------------------------+
    |                                                         |
    |   +------------------+            +------------------+  |
    |   |  HOST panel      |            |  CLIENT panel    |  |
    |   |  1920x1080       |            |  1920x1080       |  |
    |   |  [barcode]       |            |  [barcode]       |  |
    |   |                  |            |                  |  |
    |   +------------------+            +------------------+  |
    |                                                         |
    |   [LED]                                                |
    +---------------------------------------------------------+
```

Rules that matter:

- **The barcode strip must be readable in the camera image.** The barcode
  is 544 pixels wide and 64 pixels tall at the pattern's native size
  (68 blocks of 8 pixels). The decoder only needs to classify each block
  as dark or light, so it works at a much lower resolution, but the blocks
  must not be blurred together. At 1000 px of camera width over a 1920 px
  panel, one barcode block is about 2.8 camera pixels: that is enough.
- **Both panels must be in the same image**, so the camera's frame
  timestamp is one clock for both.
- **The diode must not overlap a panel** and must not be washed out by
  ambient light. Black card behind it.
- **Do not change the camera's position between the floor measurement and
  the measurement.** Moving the camera changes the mapping from camera
  pixels to panel pixels.
- Mark the camera's position on the table so a later run can reproduce it.

A quick check that the aim is right, before any of Lumen is involved:

```console
$ v4l2-ctl -d /dev/video0 --stream-mmap=4 --stream-count=120 --stream-to=/tmp/aim.mjpg
$ ffmpeg -hide_banner -i /tmp/aim.mjpg -vf "select='eq(n\,60)'" -frames:v 1 /tmp/aim.png
```

Look at `/tmp/aim.png`: both barcodes must be sharp and the diode's
rectangle must be visible.

---

## 4. Capturing with v4l2-ctl

Set the camera up once and write the settings down:

```console
$ v4l2-ctl -d /dev/video0 --list-formats-ext
$ v4l2-ctl -d /dev/video0 \
    --set-fmt-video=width=1280,height=720,pixelformat=MJPG \
    --set-parm=1000 \
    --set-ctrl=exposure_absolute=200
$ v4l2-ctl -d /dev/video0 --get-fmt-video --get-parm --get-ctrl
```

Then capture. The script does the whole loop:

```console
$ harness/photodiode/capture.sh \
    --device /dev/video0 \
    --width 1280 --height 720 \
    --frames 1200 --fps 1000 --format MJPG \
    --out-dir /tmp/rig-frames
```

What it does, and what it produces:

1. `v4l2-ctl --stream-mmap=4 --stream-count=N --stream-to=stream.mjpg`
   writes the camera's own frames into one file.
2. `ffmpeg` decodes that stream and writes it as **BGRX**, four bytes per
   pixel, tightly packed.
3. The stream is split into one file per camera frame, named with the
   **camera timestamp in microseconds**: `0.raw`, `1000.raw`, `2000.raw`,
   ... The name is the timestamp because a V4L2 node has no per-frame
   timestamp of its own: the exposure timestamp of a frame is the start of
   its interval, and the interval is `1e6 / fps` microseconds.

This is the shape `lumen_bench::camera::load_raw_frames` expects: a
directory of `.raw` files, each exactly `width * height * 4` bytes, whose
stem is the camera timestamp in microseconds.

The camera's exposure must be short enough that the diode's flash is one
frame and not a smear: with a 200 us exposure and a 1000 us interval the
flash is lit for at most one frame.

---

## 5. Finding the trigger

The diode's flash is a bright rectangle in a known corner of the image.

```console
$ harness/photodiode/detect-trigger.py \
    --frames-dir /tmp/rig-frames \
    --roi 0 0 64 64 \
    --threshold 200 \
    --out /tmp/rig-triggers.json
```

- `--roi X Y W H` is the rectangle the diode is in, in the camera's own
  pixel coordinates.
- `--threshold` is the mean brightness above which the diode counts as
  lit. The script prints the mean brightness of every frame, so pick the
  threshold half way between the dark baseline and the lit level.

The output holds the camera timestamps of the frames in which the diode
was lit, in `triggers_us`. One trigger per host frame is expected. If the
host runs at 60 fps and the camera at 1000 fps, there are about sixteen
camera frames per host frame and one of them carries the flash.

**Check the count.** If the number of triggers is much smaller than the
number of host frames, the flash is being missed: either the exposure is
too long, or the camera's frame rate is too close to the host's.

---

## 6. Matching the barcode to the host timestamp

The test pattern carries two things in the top left corner of every frame:

- a **machine readable barcode**: a start block, one block per bit with a
  known bit width, and a stop block. The payload is a 32-bit frame counter
  and a 32-bit host timestamp in microseconds;
- a **human readable** strip of digits under it, so a person can read the
  frame number off a photograph of the screen.

The decoder is `lumen_bench::pattern::decode_barcode`. It samples the
strip, classifies each block by its mean brightness against a threshold of
127, and returns `(frame_index, timestamp_us)`.

The pairing is:

```
   camera frame  ──►  barcode  ──►  (frame_index n, host timestamp T(n))
                                        │
   camera frame  ──►  the diode is lit ──►  the trigger, camera timestamp S(n)
```

`S(n)` is the camera time at which the host's panel showed frame `n`, and
the camera frame whose barcode reads `n` is the camera time at which the
client's panel showed frame `n`. The **frame counter** is what pairs the
two; the host timestamp is what lets a reader confirm the pairing and
aligns the host's clock with the camera's.

The decoder is deliberately tolerant: it only needs one camera pixel per
barcode block, so the camera can be far away and the panel can be small in
the image.

---

## 7. Computing the glass-to-glass

For every camera frame that carries a readable barcode:

```
   glass_to_glass = camera_timestamp_of_the_barcode_frame
                  - camera_timestamp_of_the_matching_trigger
```

That is `lumen_bench::trigger::glass_to_glass` and
`lumen_bench::camera::analyse`. The analysis reports the p50, the p99 and
the maximum over all the observations.

**NOTE:** `lumen-bench` has no `analyse` subcommand at the time of
writing. The functions exist in the library
(`lumen_bench::camera::analyse`, `lumen_bench::camera::load_raw_frames`),
but the binary only has `report` and `rig-selftest`. Until the subcommand
exists, the analysis is done by a small program that links the crate, or
by hand from the JSON. This is recorded in `AGENT-D-NOTES.md`.

The number is reported **raw**, and the trigger delay is reported next to
it:

- `glass_to_glass` = the gap between the diode firing and the barcode
  appearing on the client's panel. This is the number the plan's targets
  are about.
- `trigger_delay` = the gap between the host's own timestamp and the
  diode firing. It is **not** part of the glass-to-glass; it aligns the
  host's clock with the camera's. It has to be measured (by driving the
  diode from a program that timestamps the write) and reported.

The simulated trigger in `harness/fixtures/simulated-trigger.json` models
this exactly: `trigger_ts_us` is the host's timestamp plus the trigger
delay, `camera_ts_us` is the trigger plus the client latency, and the
glass-to-glass is the client latency alone.

---

## 8. Measuring the display's own floor

The floor is the latency of a panel by itself. It is the floor of every
glass-to-glass number, and it has to be subtracted from a measurement
before the measurement can be compared with the plan's targets, because the
targets are about the remote desktop system and not about the panel.

The procedure:

1. Put the camera where it will be for the measurement, aimed at the panel
   whose floor is being measured. Do not move it afterwards.
2. Drive the diode **from the graphics output**, not from Lumen: a tiny
   program that renders a black frame, then a white frame, and pulses the
   GPIO at the moment it commits the white frame. The program must print
   the timestamp of the commit, so that the trigger delay can be computed.
3. Capture, detect the triggers, and compute the gap the same way. That
   gap is the floor: it contains the panel's latency, the commit-to-photon
   time of the panel, the GPIO write, and the camera path.
4. Subtract the GPIO write's latency, which is a few microseconds and is
   measured separately by looping the GPIO write and reading the GPIO back.
5. Repeat on **every** panel that takes part in a comparison, and repeat
   whenever a panel's settings change: overdrive on and off, a different
   refresh rate, a different mode. A panel with overdrive has a different
   floor from the same panel without it.

The floor is what makes the numbers comparable. Without it, a "9 ms
glass-to-glass" against a fast panel and a "9 ms glass-to-glass" against a
slow panel are two different measurements.

---

## 9. The check that does not need the rig

The rig's arithmetic is verified against a simulated trigger, so that a
failure of the harness is not confused with a failure of the rig.

```console
$ cargo run -p lumen-bench --bin lumen-bench -- rig-selftest \
    --fixture harness/fixtures/simulated-trigger.json
```

**NOTE:** `lumen-bench rig-selftest` does not accept a `--fixture` flag.
It takes `--out`, `--frames`, `--trigger-delay` and `--client-latency`, and
it always *writes* a fixture. The helper does the equivalent:

```console
$ harness/photodiode/selftest.sh
```

It runs the existing self-test with the parameters the committed fixture
was made with (30 frames, a trigger delay of 1000 us and a client latency
of 5000 us), compares the result with
`harness/fixtures/simulated-trigger.json` field by field, checks that the
glass-to-glass of every observation equals the injected latency within
1 ms, and fails when anything differs.

This is recorded in `AGENT-D-NOTES.md` as wiring needed: the bench crate
should grow a `--fixture` flag that reads a fixture back and verifies it,
so that the plan's own verification step 9 works as written.

---

## 10. What to record for every measurement

A glass-to-glass result is only worth what is recorded beside it. At the
minimum:

| Field | Why |
|---|---|
| The camera's format, frame rate and exposure | The resolution of the measurement |
| The camera's position, marked | So the run can be reproduced |
| The trigger delay, measured | It is not part of the glass-to-glass, but it has to be known |
| The display floor of both panels | Without it the number cannot be compared |
| The host's and the client's software pipeline latency from the **same run** | The difference between the two is what the software does not see |
| The codec, the resolution, the frame rate and the bitrate | The comparison with the baselines needs them |
| The link: wired, and what the measured RTT was | A loopback number is not a LAN number |
| The raw recording, committed | Every number can be recomputed from it |

---

## 11. The files

| Path | What it is |
|---|---|
| `photodiode/README.md` | The wiring, the part list, the trigger detection, the per-step procedure |
| `photodiode/capture.sh` | The capture loop: v4l2-ctl into a file, then one BGRX `.raw` file per camera frame |
| `photodiode/detect-trigger.py` | Finds the photodiode triggers in a recording |
| `photodiode/selftest.sh` | Runs the harness self-test against the committed fixture and compares |
| `fixtures/simulated-trigger.json` | The committed simulated-trigger fixture: 30 observations, a glass-to-glass of 5000 us |
| `../docs/benchmarks.md` | Where the measured numbers go, with their provisional markers |
| `../docs/final-report.md` | The final report, with the pending items |
