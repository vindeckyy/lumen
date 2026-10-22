#!/usr/bin/env bash
# Capture the camera frames of the photodiode rig.
#
# Plan milestone M8.6: "rig procedure, capture loop (v4l2-ctl to file),
# barcode decode, glass-to-glass computation, display-floor measurement".
#
# This is the capture loop. It writes one file per camera frame, in the shape
# `lumen_bench::camera::load_raw_frames` expects:
#
#   - the frames are BGRX, four bytes per pixel, tightly packed,
#   - each file holds exactly width * height * 4 bytes,
#   - the name of the file is the camera timestamp of the frame in
#     microseconds, with the .raw extension, for example
#     1728630012345.raw.
#
# The frames come out of the camera in the camera's own pixel format.
# v4l2-ctl writes them into one file and ffmpeg splits that file into the
# per-frame BGRX files. The timestamps are computed from the frame rate the
# camera was set to, because a V4L2 node has no per-frame timestamp of its
# own: the exposure timestamp of a frame is the start of its interval.
#
# The photodiode trigger timestamps are found afterwards, by
# harness/photodiode/detect-trigger.py, which looks at the bright spot the
# diode makes in the corner of the camera's field of view.
#
# Exit status:
#   0  the frames were captured
#   1  the capture failed, or a tool is missing
#   2  a bad command line
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

DEVICE="${DEVICE:-/dev/video0}"
WIDTH="${WIDTH:-1280}"
HEIGHT="${HEIGHT:-720}"
FRAMES="${FRAMES:-300}"
FPS="${FPS:-1000}"
NATIVE_FORMAT="${NATIVE_FORMAT:-MJPG}"
OUT_DIR="${OUT_DIR:-/tmp/lumen-rig-frames}"
# The exposure time of the camera in microseconds, used to turn the frame
# index into a timestamp.
EXPOSURE_US="${EXPOSURE_US:-1000}"

log() { printf '[capture] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: harness/photodiode/capture.sh [options]

Capture the camera frames of the photodiode rig and write one BGRX .raw file
per frame, named with the camera timestamp in microseconds.

Options:
  --device <path>    The v4l2 device (default /dev/video0)
  --width <n>        The capture width in pixels (default 1280)
  --height <n>       The capture height in pixels (default 720)
  --frames <n>       How many frames to capture (default 300)
  --fps <n>          The frame rate the camera is set to (default 1000)
  --format <fmt>     The camera's own pixel format (default MJPG)
  --exposure <us>    The exposure time in microseconds (default 1000)
  --out-dir <path>   Where the frames go (default /tmp/lumen-rig-frames)
  -h, --help         Print this text

The frames are written as <camera_timestamp_us>.raw, BGRX, tightly packed.
Run harness/photodiode/detect-trigger.py on the same directory to find the
photodiode triggers.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --device) DEVICE="${2:?--device needs a value}"; shift 2 ;;
    --width) WIDTH="${2:?--width needs a value}"; shift 2 ;;
    --height) HEIGHT="${2:?--height needs a value}"; shift 2 ;;
    --frames) FRAMES="${2:?--frames needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
    --format) NATIVE_FORMAT="${2:?--format needs a value}"; shift 2 ;;
    --exposure) EXPOSURE_US="${2:?--exposure needs a value}"; shift 2 ;;
    --out-dir) OUT_DIR="${2:?--out-dir needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

for tool in v4l2-ctl ffmpeg python3; do
  command -v "$tool" >/dev/null 2>&1 || {
    log "the tool $tool is needed to capture the rig and is not installed"
    exit 1
  }
done
[ -e "$DEVICE" ] || { log "the camera $DEVICE is not there"; exit 1; }

case "$OUT_DIR" in
  /*) ;;
  *) OUT_DIR="$REPO_ROOT/$OUT_DIR" ;;
esac
mkdir -p "$OUT_DIR"

log "the camera $DEVICE:"
v4l2-ctl --device "$DEVICE" --list-formats-ext 2>&1 | sed 's/^/    /' || true

RAW="$OUT_DIR/stream.${NATIVE_FORMAT,,}"
BGRX="$OUT_DIR/stream.bgrx"

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  # The intermediate stream files are not part of the result: the frames are
  # the result. They are only kept when the capture failed, so that the
  # failure can be looked at.
  if [ "$status" = "0" ]; then
    rm -f "$RAW" "$BGRX"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "capturing $FRAMES frames at ${WIDTH}x${HEIGHT} into $RAW"
v4l2-ctl --device "$DEVICE" \
  --set-fmt-video="width=$WIDTH,height=$HEIGHT,pixelformat=$NATIVE_FORMAT" \
  --set-fmt-stream-mmap=4 \
  --stream-mmap=4 --stream-count="$FRAMES" --stream-to="$RAW" \
  2>&1 | sed 's/^/    /'

[ -s "$RAW" ] || { log "the camera wrote no data"; exit 1; }

FRAME_BYTES=$((WIDTH * HEIGHT * 4))
log "converting the stream into BGRX frames of $FRAME_BYTES bytes"
# The native format is decoded first, then everything is written as BGRX.
case "${NATIVE_FORMAT^^}" in
  MJPG|MJPEG) FFMPEG_INPUT=(-f mjpeg) ;;
  YUYV) FFMPEG_INPUT=(-f rawvideo -pix_fmt yuyv422) ;;
  UYVY) FFMPEG_INPUT=(-f rawvideo -pix_fmt uyvy422) ;;
  NV12) FFMPEG_INPUT=(-f rawvideo -pix_fmt nv12) ;;
  GRAY) FFMPEG_INPUT=(-f rawvideo -pix_fmt gray) ;;
  BGRX|BGR24|RGB24) FFMPEG_INPUT=(-f rawvideo -pix_fmt bgr24) ;;
  *) log "the pixel format $NATIVE_FORMAT is not known to this script"; exit 1 ;;
esac

ffmpeg -hide_banner -loglevel error "${FFMPEG_INPUT[@]}" -i "$RAW" \
  -pix_fmt bgrx -f rawvideo "$BGRX" || { log "ffmpeg could not convert the stream"; exit 1; }

TOTAL_BYTES="$(stat -c %s "$BGRX")"
COUNT=$((TOTAL_BYTES / FRAME_BYTES))
log "the stream holds $COUNT whole frames of ${WIDTH}x${HEIGHT}"
if [ "$COUNT" -lt 1 ]; then
  log "the stream holds no whole frame"
  exit 1
fi

log "writing the frames into $OUT_DIR"
# The frame interval is the exposure time plus the readout time. At 1000 fps
# with a 1000 us exposure the interval is 1000 us.
python3 - "$BGRX" "$OUT_DIR" "$FRAME_BYTES" "$COUNT" "$EXPOSURE_US" "$FPS" <<'PY'
import os
import sys

source, out_dir, frame_bytes, count, exposure_us, fps = sys.argv[1:7]
frame_bytes = int(frame_bytes)
count = int(count)
exposure_us = int(exposure_us)
fps = int(fps)

# The interval between two frames. The exposure is part of it, the readout
# is the rest of the period.
interval_us = max(1, round(1_000_000 / fps))
if exposure_us > interval_us:
    exposure_us = interval_us

with open(source, "rb") as handle:
    data = handle.read()

written = 0
for index in range(count):
    chunk = data[index * frame_bytes : (index + 1) * frame_bytes]
    if len(chunk) != frame_bytes:
        break
    # The camera stamps a frame at the start of its exposure, so the
    # timestamp of frame n is n intervals after the start of the run.
    timestamp_us = index * interval_us
    path = os.path.join(out_dir, f"{timestamp_us}.raw")
    with open(path, "wb") as handle:
        handle.write(chunk)
    written += 1

print(
    f"[capture] wrote {written} frames to {out_dir}, "
    f"one every {interval_us} us ({1000.0 / interval_us:.1f} fps)"
)
PY

log "the capture is in $OUT_DIR"
log "next: harness/photodiode/detect-trigger.py --frames-dir $OUT_DIR"
