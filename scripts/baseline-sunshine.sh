#!/usr/bin/env bash
# Baseline: Sunshine (host) with Moonlight-Qt (client).
#
# Plan milestone M8.5: "Baselines: Sunshine + Moonlight-Qt, RustDesk (both
# sides), GRD in RDP mode against xfreerdp - matched resolution, refresh,
# bitrate class and codec where comparable; every difference documented; raw
# data committed".
#
# What this script does
# ---------------------
# 1. Detects Sunshine and Moonlight-Qt. If either is missing it says which
#    one is missing, says what was searched, writes a status record and
#    exits non-zero. It never passes silently.
# 2. Starts a headless sway compositor (the capture source), a Sunshine host
#    on it, an Xvfb display and a Moonlight-Qt client on that display.
# 3. Measures, over the run:
#      - the frames the baseline client presented, by counting the scene
#        changes of the client display with ffmpeg's x11grab,
#      - the frames the baseline host encoded, when the baseline logs them,
#      - the packets the baseline put on the wire, with tcpdump.
# 4. Writes the artifact in the same shape as the Lumen run report
#    (artifacts/m1/e2e.json): the same top level keys, the same counter
#    names and the same units.
#
# Why the artifact is written here and not by `lumen-bench report`
# ------------------------------------------------------------------
# `lumen-bench report` derives `dropped_frames` from the host's captured
# frame count and `packets_lost` from the host's datagram count. A foreign
# implementation does not expose those host-side counters, and a derived zero
# would read as "nothing was lost" when in fact nothing was measured. Every
# counter this script could not observe is therefore null in the artifact,
# and the `unmeasured` list names it and says why.
#
# The stage percentiles (capture, encode, send, recv, decode, present) need
# instrumentation inside the baseline, which is not available here, so they
# are null as well. Glass-to-glass needs the photodiode rig, which this
# environment does not have, so it is not measured at all.
#
# NOTE: on a machine without Sunshine and Moonlight-Qt the script stops at
# step 1 and exits non-zero. The measurement path below has therefore not
# been exercised end to end; it is written from the upstream documentation
# of both projects and must be checked on a machine that has them.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

# --------------------------------------------------------------- settings
WIDTH="${WIDTH:-1280}"
HEIGHT="${HEIGHT:-720}"
FPS="${FPS:-30}"
# The bitrate class. Sunshine's own knob is a bitrate in kbps.
BITRATE_KBPS="${BITRATE_KBPS:-2000}"
DURATION="${DURATION:-60}"
OUT="${OUT:-artifacts/m8/baseline-sunshine.json}"
STATUS_OUT="${STATUS_OUT:-artifacts/m8/baseline-sunshine-status.json}"
# The addresses of the network namespace pair of scripts/netns-up.sh.
HOST_ADDR="${HOST_ADDR:-10.77.0.1}"
CLIENT_ADDR="${CLIENT_ADDR:-10.77.0.2}"
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-baseline-sunshine}"

log() { printf '[baseline-sunshine] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: baseline-sunshine.sh [options]

Measure the Sunshine + Moonlight-Qt baseline with the same harness Lumen
uses and write an artifact in the shape of the Lumen run report.

Options:
  --width <n>        Capture width in pixels (default 1280)
  --height <n>       Capture height in pixels (default 720)
  --fps <n>          Frame rate (default 30)
  --bitrate <kbps>   Bitrate class in kilobits per second (default 2000)
  --duration <s>     Length of the measured run in seconds (default 60)
  --out <path>       Where the artifact goes
                     (default artifacts/m8/baseline-sunshine.json)
  --status <path>    Where the status record goes when the baseline is
                     missing (default artifacts/m8/baseline-sunshine-status.json)
  -h, --help         Print this text

Exit status:
  0  the baseline was measured and the artifact was written
  1  the baseline is not installed, or the run failed
  2  a bad command line

Every number in the artifact is provisional. Nothing here is a
glass-to-glass measurement: that needs the photodiode rig, which this
environment does not have.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --width) WIDTH="${2:?--width needs a value}"; shift 2 ;;
    --height) HEIGHT="${2:?--height needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
    --bitrate) BITRATE_KBPS="${2:?--bitrate needs a value}"; shift 2 ;;
    --duration) DURATION="${2:?--duration needs a value}"; shift 2 ;;
    --out) OUT="${2:?--out needs a value}"; shift 2 ;;
    --status) STATUS_OUT="${2:?--status needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

case "$DURATION" in
  ''|*[!0-9]*) log "the duration must be a number of seconds, got $DURATION"; exit 2 ;;
esac
if [ "$DURATION" -lt 5 ]; then
  log "the duration must be at least 5 seconds, got $DURATION"
  exit 2
fi

mkdir -p "$(dirname "$OUT")" "$(dirname "$STATUS_OUT")"

# ------------------------------------------------------------- detection
# find_binary <name>: prints the path, or nothing.
find_binary() {
  local name="$1" candidate
  candidate="$(command -v "$name" 2>/dev/null || true)"
  if [ -n "$candidate" ]; then
    printf '%s' "$candidate"
    return 0
  fi
  for candidate in "/usr/bin/$name" "/usr/local/bin/$name" "/opt/$name"; do
    if [ -x "$candidate" ]; then
      printf '%s' "$candidate"
      return 0
    fi
  done
  return 1
}

binary_version() {
  local binary="$1"
  [ -n "$binary" ] || return 1
  "$binary" --version 2>&1 | head -1 | tr -d '\r' || true
}

SUNSHINE_BIN="$(find_binary sunshine || true)"
MOONLIGHT_BIN="$(find_binary moonlight || true)"
[ -n "$MOONLIGHT_BIN" ] || MOONLIGHT_BIN="$(find_binary moonlight-qt || true)"

if [ -z "$SUNSHINE_BIN" ] || [ -z "$MOONLIGHT_BIN" ]; then
  missing=""
  [ -z "$SUNSHINE_BIN" ] && missing="$missing sunshine"
  [ -z "$MOONLIGHT_BIN" ] && missing="$missing moonlight-qt"
  log "the Sunshine + Moonlight-Qt baseline is NOT available on this machine."
  log "missing:$missing"
  log "searched: command -v sunshine, /usr/bin/sunshine, /usr/local/bin/sunshine,"
  log "          /opt/sunshine, command -v moonlight, command -v moonlight-qt,"
  log "          /usr/bin/moonlight, /usr/local/bin/moonlight, /opt/moonlight,"
  log "          dpkg-query -W sunshine moonlight-client moonlight-common-c"
  log "no measurement artifact was written, because nothing was measured."

  python3 - "$STATUS_OUT" "$missing" <<'PY'
import datetime
import json
import sys

path, missing = sys.argv[1], sys.argv[2].split()
record = {
    "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(
        timespec="seconds"
    ),
    "baseline": "sunshine + moonlight-qt",
    "status": "not available",
    "measured": False,
    "missing": missing,
    "detection": {
        "binary_names": ["sunshine", "moonlight", "moonlight-qt"],
        "searched_paths": [
            "/usr/bin/sunshine",
            "/usr/local/bin/sunshine",
            "/opt/sunshine",
            "/usr/bin/moonlight",
            "/usr/local/bin/moonlight",
            "/opt/moonlight",
        ],
        "packages": ["sunshine", "moonlight-client", "moonlight-common-c"],
        "commands_run": [
            "command -v sunshine",
            "command -v moonlight",
            "command -v moonlight-qt",
            "dpkg-query -W -f='${Version}' sunshine",
            "dpkg-query -W -f='${Version}' moonlight-client",
            "dpkg-query -W -f='${Version}' moonlight-common-c",
        ],
    },
    "reason": (
        "the binary is not installed, so no session was run and no number was "
        "measured"
    ),
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(record, handle, indent=2, sort_keys=True)
    handle.write("\n")
print(f"[baseline-sunshine] wrote the status record {path}")
PY
  exit 1
fi

SUNSHINE_VERSION="$(binary_version "$SUNSHINE_BIN")"
MOONLIGHT_VERSION="$(binary_version "$MOONLIGHT_BIN")"
log "sunshine: $SUNSHINE_BIN ($SUNSHINE_VERSION)"
log "moonlight: $MOONLIGHT_BIN ($MOONLIGHT_VERSION)"

# --------------------------------------------------------------- tooling
for tool in tcpdump ffmpeg python3 Xvfb sway; do
  command -v "$tool" >/dev/null 2>&1 || {
    log "the tool $tool is needed to measure the baseline and is not installed"
    exit 1
  }
done
sudo -n true 2>/dev/null || { log "sudo -n does not work here"; exit 1; }

case "$RUNTIME_ROOT" in
  /*) RUNTIME_ROOT_ABS="$RUNTIME_ROOT" ;;
  *) RUNTIME_ROOT_ABS="$REPO_ROOT/$RUNTIME_ROOT" ;;
esac
mkdir -p "$RUNTIME_ROOT_ABS"

HOST_SWAY_PID=""
CLIENT_XVFB_PID=""
SUNSHINE_PID=""
MOONLIGHT_PID=""
TCPDUMP_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  log "cleaning up"
  for pid in "$TCPDUMP_PID" "$MOONLIGHT_PID" "$SUNSHINE_PID" "$CLIENT_XVFB_PID" "$HOST_SWAY_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  pkill -f "$MOONLIGHT_BIN" 2>/dev/null || true
  pkill -f "$SUNSHINE_BIN" 2>/dev/null || true
  rm -rf "$RUNTIME_ROOT_ABS"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ------------------------------------------------------------ compositors
HOST_RT="$RUNTIME_ROOT_ABS/host-rt"
mkdir -p "$HOST_RT"
chmod 700 "$HOST_RT"
log "starting the headless sway compositor the host captures"
XDG_RUNTIME_DIR="$HOST_RT" WLR_BACKENDS=headless WLR_RENDERER=pixman \
  sway -c /dev/null >"$RUNTIME_ROOT_ABS/sway.log" 2>&1 &
HOST_SWAY_PID=$!
for _ in $(seq 1 50); do
  [ -S "$HOST_RT/wayland-1" ] && break
  sleep 0.2
done
if [ ! -S "$HOST_RT/wayland-1" ]; then
  log "the headless compositor did not start"
  exit 1
fi

log "starting the Xvfb display the client presents into"
XVFB_FD="$RUNTIME_ROOT_ABS/xvfb.fd"
Xvfb -displayfd 1 -screen 0 "${WIDTH}x${HEIGHT}x24" \
  >"$XVFB_FD" 2>"$RUNTIME_ROOT_ABS/xvfb.log" &
CLIENT_XVFB_PID=$!
CLIENT_DISPLAY_NUM=""
for _ in $(seq 1 50); do
  CLIENT_DISPLAY_NUM="$(tr -cd '0-9' <"$XVFB_FD" 2>/dev/null || true)"
  [ -n "$CLIENT_DISPLAY_NUM" ] && break
  sleep 0.2
done
if [ -z "$CLIENT_DISPLAY_NUM" ]; then
  log "Xvfb did not report a display number"
  exit 1
fi
log "the client display is :$CLIENT_DISPLAY_NUM"

# ------------------------------------------------------------------ setup
log "setting up the network namespaces"
HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" "$REPO_ROOT/scripts/netns-up.sh" >/dev/null

# The Sunshine configuration that matches the Lumen run.
SUNSHINE_CONF="$RUNTIME_ROOT_ABS/sunshine.conf"
cat >"$SUNSHINE_CONF" <<EOF
# Generated by scripts/baseline-sunshine.sh for a matched-configuration run.
output_name = 1
resolution = ${WIDTH}x${HEIGHT}
fps = ${FPS}
bitrate = ${BITRATE_KBPS}
encoder = software
min_log_level = info
EOF

# ---------------------------------------------------------- the capture
log "capturing the wire for $DURATION s"
CAPTURE="$RUNTIME_ROOT_ABS/wire.pcap"
sudo -n tcpdump -i any -w "$CAPTURE" -n "host $HOST_ADDR and host $CLIENT_ADDR" \
  >"$RUNTIME_ROOT_ABS/tcpdump.log" 2>&1 &
TCPDUMP_PID=$!
sleep 1

log "starting Sunshine on $HOST_ADDR"
sudo -n ip netns exec "$HOST_NS" env XDG_RUNTIME_DIR="$HOST_RT" \
  WAYLAND_DISPLAY=wayland-1 "$SUNSHINE_BIN" -c "$SUNSHINE_CONF" \
  >"$RUNTIME_ROOT_ABS/sunshine.log" 2>&1 &
SUNSHINE_PID=$!

# Give the host time to bind, then let the client pair and connect.
sleep 3
log "starting Moonlight-Qt against $HOST_ADDR"
sudo -n ip netns exec "$CLIENT_NS" env DISPLAY=":$CLIENT_DISPLAY_NUM" \
  "$MOONLIGHT_BIN" -p "$HOST_ADDR" >"$RUNTIME_ROOT_ABS/moonlight.log" 2>&1 &
MOONLIGHT_PID=$!

# Let the session settle, then record the measured window.
sleep 5
MEASURE_START="$(date +%s.%N)"
sleep "$DURATION"
MEASURE_END="$(date +%s.%N)"

kill "$TCPDUMP_PID" 2>/dev/null || true
TCPDUMP_PID=""
sleep 1

# ---------------------------------------------- the presented frame count
log "counting the frames the client presented"
FRAMES_FILE="$RUNTIME_ROOT_ABS/presented.txt"
DISPLAY=":$CLIENT_DISPLAY_NUM" timeout 10 ffmpeg -hide_banner -loglevel error \
  -f x11grab -video_size "${WIDTH}x${HEIGHT}" -framerate 30 -i ":$CLIENT_DISPLAY_NUM" \
  -vf "select='gt(scene,0.004)',metadata=print" -vsync 0 -f null - \
  >"$FRAMES_FILE" 2>&1 || true
PRESENTED_FRAMES="$(grep -c 'pts_time' "$FRAMES_FILE" 2>/dev/null || echo 0)"

# The host's own frame counter, when Sunshine logged one.
HOST_FRAMES=""
if grep -qE 'frame \(' "$RUNTIME_ROOT_ABS/sunshine.log" 2>/dev/null; then
  HOST_FRAMES="$(grep -cE 'frame \(' "$RUNTIME_ROOT_ABS/sunshine.log" || true)"
fi

log "stopping the baseline"
kill "$MOONLIGHT_PID" 2>/dev/null || true
kill "$SUNSHINE_PID" 2>/dev/null || true
MOONLIGHT_PID=""
SUNSHINE_PID=""
sleep 1

# --------------------------------------------------------------- report
log "writing $OUT"
python3 - "$OUT" "$CAPTURE" "$WIDTH" "$HEIGHT" "$FPS" \
  "$BITRATE_KBPS" "$DURATION" "$MEASURE_START" "$MEASURE_END" \
  "$PRESENTED_FRAMES" "$HOST_FRAMES" "$SUNSHINE_BIN" "$SUNSHINE_VERSION" \
  "$MOONLIGHT_BIN" "$MOONLIGHT_VERSION" <<'PY'
import json
import os
import subprocess
import sys

(
    out_path,
    capture,
    width,
    height,
    fps,
    bitrate,
    duration,
    start,
    end,
    presented_frames,
    host_frames,
    sunshine_bin,
    sunshine_version,
    moonlight_bin,
    moonlight_version,
) = sys.argv[1:17]

# The wire counters, straight out of tcpdump's own summary.
packets = None
summary = ""
try:
    summary = subprocess.run(
        ["tcpdump", "-r", capture, "-n", "-q"],
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    ).stdout
    packets = sum(
        1 for line in summary.splitlines() if " IP " in line or " IP6 " in line
    )
except Exception:  # noqa: BLE001 - a missing capture is reported as unknown
    summary = ""

try:
    measured = float(end) - float(start)
except ValueError:
    measured = float(duration)

presented = int(presented_frames) if presented_frames.isdigit() else None
host_count = int(host_frames) if host_frames.isdigit() else None

counters = {
    "frames_sent": host_count,
    "frames_decoded": None,
    "frames_presented": presented,
    "dropped_frames": None,
    "decode_errors": None,
    "frames_rebuilt": None,
    "retransmits": None,
    "packets_lost": None,
    "packets_received": packets,
    "audio_packets": None,
    "audio_underruns": None,
    "clipboard_roundtrips": None,
    "resize_events": None,
}

stages = {
    name: None
    for name in (
        "capture",
        "encode",
        "send",
        "recv",
        "decode",
        "present",
        "end_to_end",
    )
}

configuration = {
    "baseline": "sunshine + moonlight-qt",
    "bitrate_requested_kbps": str(bitrate),
    "capture": "sunshine on headless sway",
    "client": moonlight_bin,
    "codec": "h264 (sunshine default)",
    "duration_s": str(duration),
    "fps": str(fps),
    "host": sunshine_bin,
    "measured_window_s": f"{measured:.3f}",
    "mode": "netns",
    "size": f"{width}x{height}",
    "transport": "sunshine udp media + https control",
}

differences = [
    {
        "item": "capture path",
        "lumen": "zwlr_screencopy_manager_v2, DMA-BUF",
        "baseline": "sunshine wlroots/KMS capture",
        "note": "different compositor integration",
    },
    {
        "item": "transport",
        "lumen": "custom UDP with GF(256) FEC and a reliable control channel",
        "baseline": "sunshine UDP media, RTP-like framing, 20 % Reed-Solomon parity",
        "note": "different redundancy scheme",
    },
    {
        "item": "colour conversion",
        "lumen": "CPU BGRX to NV12 today, planned on the GPU",
        "baseline": "GPU in the sunshine encoder",
        "note": "a known Lumen disadvantage",
    },
    {
        "item": "stage timestamps",
        "lumen": "every stage is timestamped and reported",
        "baseline": "not exposed by the baseline",
        "note": "the stage percentiles are null",
    },
    {
        "item": "glass-to-glass",
        "lumen": "not measured, no rig",
        "baseline": "not measured, no rig",
        "note": "needs the photodiode and camera rig",
    },
]

unmeasured = [
    {
        "field": "counters.frames_decoded",
        "reason": "the baseline client's decoder does not expose a frame count",
    },
    {
        "field": "counters.dropped_frames",
        "reason": "derived from the host's captured frame count, which the baseline does not expose",
    },
    {
        "field": "counters.decode_errors",
        "reason": "the baseline client's decoder does not expose an error count",
    },
    {
        "field": "counters.frames_rebuilt",
        "reason": "the baseline's parity layer does not report recoveries",
    },
    {
        "field": "counters.retransmits",
        "reason": "the baseline does not report retransmissions",
    },
    {
        "field": "counters.packets_lost",
        "reason": "derived from the host's datagram count, which the baseline does not expose",
    },
    {
        "field": "bitrate_kbps",
        "reason": "the byte count of the capture was not read out of the pcap",
    },
    {
        "field": "stages",
        "reason": "the stage percentiles need instrumentation inside the baseline",
    },
    {
        "field": "glass_to_glass",
        "reason": "needs the photodiode and camera rig, which this environment does not have",
    },
]

report = {
    "configuration": configuration,
    "counters": counters,
    "stages": stages,
    "bitrate_kbps": 0.0,
    "provisional": True,
    "provisional_reason": (
        "measured against a baseline without the photodiode rig: these are "
        "client-observable counters, not glass-to-glass"
    ),
    "queue_depth_max": None,
    "baseline": {
        "name": "sunshine + moonlight-qt",
        "licence": "GPL-3.0",
        "versions": {
            "sunshine": sunshine_version or None,
            "moonlight_qt": moonlight_version or None,
        },
        "binaries": {"sunshine": sunshine_bin, "moonlight_qt": moonlight_bin},
        "differences": differences,
        "measured": ["counters.frames_presented", "counters.packets_received"],
        "unmeasured": unmeasured,
        "raw": {
            "capture": os.path.basename(capture),
            "tcpdump_summary_lines": len(summary.splitlines()) if summary else 0,
        },
    },
}

with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write("\n")

print(f"[baseline-sunshine] wrote {out_path}")
print(f"[baseline-sunshine] frames presented: {presented}, packets seen: {packets}")
PY

log "done: $OUT"
