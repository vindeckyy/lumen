# The photodiode and camera rig

This directory holds the wiring of the rig that measures **glass-to-glass**
latency, and the scripts that drive it.

The rig answers the one question the software cannot answer: how long does a
pixel take to travel from the host's screen to the client's screen? The
software pipeline latency that `lumen-bench` reports stops when the client
hands the frame to the compositor. Everything after that — the compositor's
own scan-out, the panel's response, the camera's exposure — is outside the
software and can only be measured optically.

## What the rig is

| Part | What it is | Why |
|---|---|---|
| Photodiode | A photodiode with a transimpedance amplifier, or a bright LED | It marks the exact moment the host presented a frame. The host drives it in step with the frame it just captured. |
| Trigger | A GPIO or an audio output on the host, wired to the diode | The trigger and the diode are the same event: when the host drives the trigger, the diode lights. |
| Camera | A high-speed camera, at least 1000 fps | It has to resolve the millisecond between the host's panel and the client's panel. At 1000 fps one frame is 1000 us, so a 8 ms latency is 8 frames. |
| Panels | A 1920x1080 60 Hz panel on the host and the same on the client | The test pattern has to fit on both. |
| Optics | The camera sees both panels and the diode at once | The camera frame in which the diode is lit and the camera frame in which the barcode shows the same counter are the two ends of the measurement. |

## The wiring

```
        HOST MACHINE                          CLIENT MACHINE
   +----------------------+            +----------------------+
   |  host panel (1080p60)|            |  client panel(1080p60)|
   |   [ Lumen test       |            |   [ Lumen test      |
   |     pattern +        |            |     pattern +       |
   |     barcode ]        |            |     barcode ]       |
   +----------+-----------+            +-----------+----------+
              |  (camera sees both panels)         |
              |                                    |
              +----------------+-------------------+
                               |
                        [ high-speed camera ]
                               |  USB3 / GigE
                               v
                         [ host machine ]

   HOST MACHINE, trigger wiring
   +------------------------------------------------------+
   |  GPIO / audio out  ---[ 1k resistor ]-->  anode (+)|  LED or
   |                                        cathode (-)|  photodiode
   |                                             |     |
   |                                    [ 100R ]  |     |
   |                                             +--[ photodiode ]--+
   |                                                   |
   |                                              [ transimpedance ]
   |                                                   |
   |                                          [ 3.3 V pull-up ]
   |                                                   |
   |                                            GPIO / audio in
   +------------------------------------------------------+
```

The exact electrical detail depends on the parts:

- **With a photodiode**: the diode is reverse biased. A transimpedance
  amplifier (a transimpedance amplifier IC such as a TIA, or an
  op-amp inverting stage with a 100 kΩ feedback resistor) turns the
  photocurrent into a voltage. A 3.3 V pull-up on the amplifier output
  makes the line idle high and the flash pull it low. Feed the amplifier
  output to a GPIO input on the machine that runs the camera, or to a
  logic level recorder.
- **With an LED**: the GPIO drives the LED through a current limiting
  resistor (330 Ω for a 3.3 V LED is about 6 mA, which a camera lens
  picks up easily). The LED is simpler and, for a rig that only needs a
  time reference, just as good.

The diode has to be **in the camera's field of view**, next to the panels,
and it must not be lit by anything else. Put it on a black piece of
cardboard so the ambient light does not lift the baseline.

### Where the diode goes

The diode has to be visible in the same camera frame as the two panels.
The simplest layout is:

```
    +-----------------------------------------------------+
    |                                                     |
    |   +----------------+          +----------------+   |
    |   |  host panel    |          | client panel   |   |
    |   |  1920x1080     |          |  1920x1080     |   |
    |   +----------------+          +----------------+   |
    |                                                     |
    |   [LED]  <- here, bottom left of the host panel     |
    +-----------------------------------------------------+
```

Place the diode **at the bottom left of the host panel**, inside the
panel's own rectangle. That way the camera does not have to resolve a
second small object far away, and the diode's position in the image is
fixed.

## The timing, and why the camera is the clock

The camera is the only clock in the measurement. Everything else — the
host's timestamp, the client's timestamp — is on a different machine with a
different clock, and the difference between those clocks is exactly the
quantity under test. The rig avoids the problem:

1. The host captures frame *n* and writes the frame counter *n* and the
   host's own timestamp *T(n)* into the barcode of the test pattern.
2. The host drives the diode at the same moment, or at a fixed, known
   delay after it stamped *T(n)*.
3. The camera records the panels and the diode. The camera frame in which
   the diode is lit is the camera frame in which the host's panel showed
   frame *n*.
4. The camera frame in which the client's panel shows the barcode *n* is
   the camera frame in which the client presented frame *n*.
5. The glass-to-glass is the difference between those two camera frame
   timestamps.

The host's own timestamp is still needed, but only to **pair the frames**:
the barcode carries both the frame counter and the host timestamp, and the
frame counter says which camera frame belongs to which host frame. The
host timestamp lets a reader confirm the pairing after the fact.

## The procedure, step by step

### 1. The hardware

- [ ] A photodiode with a transimpedance amplifier, or a bright LED and a
      330 Ω resistor.
- [ ] A way to drive it from the host: a GPIO line, or a sound card output
      that the host can pulse. On a machine without accessible GPIO, an
      audio output works: the host writes a short click to a null sink and
      a resistor divider feeds the microphone input of the camera, or a
      separate USB sound card with an input.
- [ ] A camera that does at least 1000 fps at the resolution that covers
      both panels. A USB 3.0 high-speed camera with a global shutter is
      the usual choice; a rolling shutter camera has to be accounted for
      in the analysis, because the top and the bottom of the image are
      read out at different times.
- [ ] Two 1920x1080 60 Hz panels, one on the host and one on the client.
- [ ] A tripod or a rigid mount for the camera, so it does not move
      between the calibration and the measurement.
- [ ] Black card or matte tape for the background, so the diode's
      threshold is unambiguous.

### 2. The camera

```console
# what is there
$ v4l2-ctl --list-devices
$ v4l2-ctl -d /dev/video0 --list-formats-ext
```

Set the camera to the highest frame rate it offers at a resolution that
covers both panels. A 1000 fps camera usually wants a small region of
interest; 640x480 at 1000 fps is enough to see a barcode strip.

```console
# 1000 fps, the shortest exposure that still shows the panels
$ v4l2-ctl -d /dev/video0 \
    --set-fmt-video=width=640,height=480,pixelformat=MJPG \
    --set-parm=1000 \
    --set-ctrl=exposure_absolute=500
```

Check the result with `v4l2-ctl -d /dev/video0 --get-fmt-video --get-parm`.

### 3. The diode

Wire the diode as in the diagram above. Check it with the camera before
anything else:

```console
$ v4l2-ctl -d /dev/video0 --stream-mmap=4 --stream-count=120 --stream-to=/tmp/probe.mjpg
```

Watch `/tmp/probe.mjpg` in any player, or convert it and look at the
rectangle the diode is in:

```console
$ ffmpeg -i /tmp/probe.mjpg -vf "crop=64:64:0:0,scale=320:320" /tmp/probe.png
```

The rectangle must go completely black when the diode is off and completely
white when it is on. If it does not, the diode is not aimed at the camera
or the threshold is too low.

### 4. The display floor

The **display floor** is the latency of the panel itself: the time between
the moment a pixel is written to the panel's framebuffer and the moment the
panel's backlight changes. It is the floor of every glass-to-glass number,
because a pixel cannot appear on the client panel sooner than the client
panel's own latency allows.

Measure it with the same rig, on the host panel alone:

1. Point the camera at the host panel only.
2. Drive the diode **directly from the host's graphics output**, with no
   Lumen in the loop: a tiny program that writes a black frame and then a
   white frame to the panel, and pulses the diode at the same moment it
   commits the white frame.
3. The glass-to-glass the rig reports for that program is the display
   floor: it contains the panel's latency and nothing else.

The floor has to be measured on **every** panel that is going to be used
in a comparison, and it has to be re-measured whenever a panel's settings
change (overdrive, a different refresh rate, a different mode). A panel
with overdrive on has a different floor from the same panel with it off.

The floor is subtracted from a glass-to-glass measurement to get the
latency that belongs to the remote desktop system rather than to the
display.

### 5. The measurement

On the host:

```console
$ lumen-host --size 1920x1080 --fps 60 --output HEADLESS-1 \
    --control 10.77.0.1:24800 --client-media-port 24801
```

On the client:

```console
$ lumen-client --host 10.77.0.1:24800 --media-bind 10.77.0.2:24801
```

The client panel must show the **Lumen test pattern**: the barcode in the
top left corner with the frame counter and the host timestamp, and the
human readable digits under it. The test pattern is generated by
`lumen_bench::pattern::TestPattern`.

While the session runs, capture the camera:

```console
$ harness/photodiode/capture.sh --device /dev/video0 --width 640 --height 480 \
    --frames 600 --fps 1000 --format MJPG --out-dir /tmp/rig-frames
```

The capture writes one file per camera frame, BGRX, named with the camera
timestamp in microseconds, in the shape
`lumen_bench::camera::load_raw_frames` expects.

### 6. The triggers

The diode flashes in a few of the camera frames. Find them:

```console
$ harness/photodiode/detect-trigger.py --frames-dir /tmp/rig-frames \
    --roi 0 0 64 64 --threshold 200 --out /tmp/rig-triggers.json
```

The `--roi` is the rectangle the diode is in, in the camera's own pixel
coordinates. `--threshold` is the mean brightness above which the diode
counts as lit; pick it from the levels the script prints, half way between
the dark baseline and the lit level.

### 7. The glass-to-glass

```console
$ cargo run -p lumen-bench --bin lumen-bench -- analyse \
    --frames /tmp/rig-frames --triggers /tmp/rig-triggers.json \
    --out artifacts/m8/glass-to-glass.json
```

`lumen_bench::camera::analyse` reads every frame, decodes the barcode out
of it, pairs it with the nearest trigger and computes the glass-to-glass of
every observation. The result carries the p50, the p99 and the maximum, in
microseconds.

**NOTE:** `lumen-bench` has no `analyse` subcommand at the time of
writing. The analysis functions exist in the library
(`lumen_bench::camera::analyse` and `lumen_bench::camera::load_raw_frames`),
but the binary only has `report` and `rig-selftest`. Until the subcommand
exists, the analysis is done by a small program that links the crate, or
by the Python in `harness/photodiode/`. This is recorded in
`AGENT-D-NOTES.md`.

### 8. The software side of the same run

The glass-to-glass and the software pipeline latency belong to the **same**
run. Run the host and the client with `--report` and merge the two counter
files:

```console
$ lumen-bench report --host host.json --client client.json --out e2e.json
```

The report's `end_to_end` percentile is the software pipeline latency, from
the capture timestamp to the presentation. The difference between that and
the glass-to-glass is what the panel and the camera path cost.

### 9. The check without the rig

The rig's arithmetic is checked without any hardware, against a simulated
trigger:

```console
$ cargo run -p lumen-bench --bin lumen-bench -- rig-selftest \
    --fixture harness/fixtures/simulated-trigger.json
```

**NOTE:** `lumen-bench rig-selftest` does not accept a `--fixture` flag.
It takes `--out`, `--frames`, `--trigger-delay` and `--client-latency`, and
it always writes a fixture. `harness/photodiode/selftest.sh` runs the
existing self-test with the parameters the committed fixture was made with
and compares the result with the committed fixture, field by field. This is
recorded in `AGENT-D-NOTES.md`.

## What is in this directory

| File | What it is |
|---|---|
| `README.md` | This file: the wiring and the procedure. |
| `capture.sh` | The capture loop: v4l2-ctl into a file, then one BGRX `.raw` file per camera frame, named with the camera timestamp in microseconds. |
| `detect-trigger.py` | Finds the photodiode triggers in a recording and writes their camera timestamps. |
| `selftest.sh` | Runs the harness self-test and compares it with the committed fixture. |
| `../fixtures/simulated-trigger.json` | The committed simulated-trigger fixture, 30 observations with a glass-to-glass of 5000 us. |

## The checks that matter

1. The barcode round trip: the decoder must read the frame counter and the
   host timestamp back out of the camera image. `lumen-bench rig-selftest`
   does this against synthetic camera frames and is run by
   `harness/photodiode/selftest.sh`.
2. The glass-to-glass arithmetic: every observation must give the same
   glass-to-glass, equal to the injected latency within 1 ms. The self-test
   checks this.
3. The trigger pairing: the diode must fire once per host frame. If the
   host presents at 60 fps and the camera runs at 1000 fps, there are
   about sixteen camera frames per host frame, and one of them carries the
   flash. If the host's frame rate and the camera's frame rate are close,
   the flash may be missed; in that case the camera has to run faster than
   the host.
