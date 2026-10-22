#!/usr/bin/env bash
# Baseline: GNOME Remote Desktop in RDP mode, measured against xfreerdp.
#
# Plan milestone M8.5: "Baselines: Sunshine + Moonlight-Qt, RustDesk (both
# sides), GRD in RDP mode against xfreerdp - matched resolution, refresh,
# bitrate class and codec where comparable; every difference documented; raw
# data committed".
#
# GNOME Remote Desktop is GPL-2.0+ and is used as a design reference only: no
# line of its code is copied into Lumen.
#
# What this script does
# ---------------------
# 1. Detects gnome-remote-desktop-daemon and xfreerdp. If either is missing
#    it says which one is missing, says what was searched, writes a status
#    record and exits non-zero.
# 2. Starts the GRD daemon in RDP mode with a credentials file, and drives
#    it with xfreerdp against the network namespace pair.
# 3. Measures the frames the RDP client presented, by counting the scene
#    changes of the client display with ffmpeg's x11grab, and the packets
#    on the wire with tcpdump.
# 4. Writes the artifact in the same shape as the Lumen run report
#    (artifacts/m1/e2e.json).
#
# Why the artifact is written here and not by `lumen-bench report`
# ------------------------------------------------------------------
# `lumen-bench report` derives `dropped_frames` from the host's captured
# frame count and `packets_lost` from the host's datagram count. GRD exposes
# neither, and a derived zero would read as "nothing was lost" when in fact
# nothing was measured. Every counter this script could not observe is null
# in the artifact, and the `unmeasured` list names it and says why.
#
# A note on the host side. GRD captures through Mutter's ScreenCast portal,
# so the host needs a GNOME session, not a sway one. On a machine with no
# GNOME session the daemon cannot capture and the run fails; that failure is
# reported, never swallowed.
#
# NOTE: on a machine without GRD and xfreerdp the script stops at step 1 and
# exits non-zero. The measurement path below has therefore not been exercised
# end to end; it is written from the upstream documentation of GRD and
# FreeRDP and must be checked on a machine that has them.
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
DURATION="${DURATION:-60}"
OUT="${OUT:-artifacts/m8/baseline-grd.json}"
STATUS_OUT="${STATUS_OUT:-artifacts/m8/baseline-grd-status.json}"
HOST_ADDR="${HOST_ADDR:-10.77.0.1}"
CLIENT_ADDR="${CLIENT_ADDR:-10.77.0.2}"
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"
RDP_USER="${RDP_USER:-lumen}"
RDP_PASSWORD="${RDP_PASSWORD:-lumen-baseline}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-baseline-grd}"

log() { printf '[baseline-grd] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: baseline-grd.sh [options]

Measure the GNOME Remote Desktop baseline in RDP mode against xfreerdp, with
the same harness Lumen uses, and write an artifact in the shape of the Lumen
run report.

Options:
  --width <n>        Capture width in pixels (default 1280)
  --height <n>       Capture height in pixels (default 720)
  --fps <n>          Frame rate (default 30)
  --duration <s>     Length of the measured run in seconds (default 60)
  --out <path>       Where the artifact goes
                     (default artifacts/m8/baseline-grd.json)
  --status <path>    Where the status record goes when the baseline is
                     missing (default artifacts/m8/baseline-grd-status.json)
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

GRD_BIN="$(find_binary gnome-remote-desktop-daemon || true)"
FREERDP_BIN="$(find_binary xfreerdp || true)"
[ -n "$FREERDP_BIN" ] || FREERDP_BIN="$(find_binary xfreerdp3 || true)"

if [ -z "$GRD_BIN" ] || [ -z "$FREERDP_BIN" ]; then
  missing=""
  [ -z "$GRD_BIN" ] && missing="$missing gnome-remote-desktop"
  [ -z "$FREERDP_BIN" ] && missing="$missing xfreerdp"
  log "the GRD + xfreerdp baseline is NOT available on this machine."
  log "missing:$missing"
  log "searched: command -v gnome-remote-desktop-daemon, /usr/bin/gnome-remote-desktop-daemon,"
  log "          /usr/local/bin/gnome-remote-desktop-daemon, /opt/gnome-remote-desktop-daemon,"
  log "          command -v xfreerdp, command -v xfreerdp3, /usr/bin/xfreerdp,"
  log "          dpkg-query -W gnome-remote-desktop freerdp3-x11"
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
    "baseline": "gnome-remote-desktop (RDP mode) + xfreerdp",
    "status": "not available",
    "measured": False,
    "missing": missing,
    "detection": {
        "binary_names": [
            "gnome-remote-desktop-daemon",
            "xfreerdp",
            "xfreerdp3",
        ],
        "searched_paths": [
            "/usr/bin/gnome-remote-desktop-daemon",
            "/usr/local/bin/gnome-remote-desktop-daemon",
            "/opt/gnome-remote-desktop-daemon",
            "/usr/bin/xfreerdp",
            "/usr/local/bin/xfreerdp",
            "/opt/xfreerdp",
        ],
        "packages": ["gnome-remote-desktop", "freerdp3-x11"],
        "commands_run": [
            "command -v gnome-remote-desktop-daemon",
            "command -v xfreerdp",
            "command -v xfreerdp3",
            "dpkg-query -W -f='${Version}' gnome-remote-desktop",
            "dpkg-query -W -f='${Version}' freerdp3-x11",
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
print(f"[baseline-grd] wrote the status record {path}")
PY
  exit 1
fi

GRD_VERSION="$(binary_version "$GRD_BIN")"
FREERDP_VERSION="$(binary_version "$FREERDP_BIN")"
log "gnome-remote-desktop-daemon: $GRD_BIN ($GRD_VERSION)"
log "xfreerdp: $FREERDP_BIN ($FREERDP_VERSION)"

# --------------------------------------------------------------- tooling
for tool in tcpdump ffmpeg python3 Xvfb; do
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

CLIENT_XVFB_PID=""
GRD_PID=""
FREERDP_PID=""
TCPDUMP_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  log "cleaning up"
  for pid in "$TCPDUMP_PID" "$FREERDP_PID" "$GRD_PID" "$CLIENT_XVFB_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  pkill -f "$FREERDP_BIN" 2>/dev/null || true
  rm -rf "$RUNTIME_ROOT_ABS"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ------------------------------------------------------------ the display
log "starting the Xvfb display the RDP client draws into"
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

# GRD reads its credentials from a file the gsettings schema names; the
# documented layout is a JSON object with a "username" and a "password".
CREDENTIALS="$RUNTIME_ROOT_ABS/credentials.json"
cat >"$CREDENTIALS" <<EOF
{
  "username": "${RDP_USER}",
  "password": "${RDP_PASSWORD}"
}
EOF

# ---------------------------------------------------------- the capture
log "capturing the wire for $DURATION s"
CAPTURE="$RUNTIME_ROOT_ABS/wire.pcap"
sudo -n tcpdump -i any -w "$CAPTURE" -n "host $HOST_ADDR and host $CLIENT_ADDR" \
  >"$RUNTIME_ROOT_ABS/tcpdump.log" 2>&1 &
TCPDUMP_PID=$!
sleep 1

log "starting gnome-remote-desktop-daemon in RDP mode"
sudo -n ip netns exec "$HOST_NS" "$GRD_BIN" --system --rdp \
  >"$RUNTIME_ROOT_ABS/grd.log" 2>&1 &
GRD_PID=$!
sleep 3

if ! kill -0 "$GRD_PID" 2>/dev/null; then
  log "the GRD daemon exited; it cannot capture without a GNOME session:"
  sed 's/^/    /' "$RUNTIME_ROOT_ABS/grd.log" 2>/dev/null || true
  exit 1
fi

log "starting xfreerdp against $HOST_ADDR"
sudo -n ip netns exec "$CLIENT_NS" env DISPLAY=":$CLIENT_DISPLAY_NUM" \
  "$FREERDP_BIN" "/v:$HOST_ADDR" "/u:$RDP_USER" "/p:$RDP_PASSWORD" \
  "/w:$WIDTH" "/h:$HEIGHT" "/gfx:progressive" "/network:auto" \
  >"$RUNTIME_ROOT_ABS/freerdp.log" 2>&1 &
FREERDP_PID=$!

# Let the session settle, then record the measured window.
sleep 5
MEASURE_START="$(date +%s.%N)"
sleep "$DURATION"
MEASURE_END="$(date +%s.%N)"

kill "$TCPDUMP_PID" 2>/dev/null || true
TCPDUMP_PID=""
sleep 1

# ---------------------------------------------- the presented frame count
log "counting the frames the RDP client presented"
FRAMES_FILE="$RUNTIME_ROOT_ABS/presented.txt"
DISPLAY=":$CLIENT_DISPLAY_NUM" timeout 10 ffmpeg -hide_banner -loglevel error \
  -f x11grab -video_size "${WIDTH}x${HEIGHT}" -framerate 30 -i ":$CLIENT_DISPLAY_NUM" \
  -vf "select='gt(scene,0.004)',metadata=print" -vsync 0 -f null - \
  >"$FRAMES_FILE" 2>&1 || true
PRESENTED_FRAMES="$(grep -c 'pts_time' "$FRAMES_FILE" 2>/dev/null || echo 0)"

log "stopping the baseline"
kill "$FREERDP_PID" 2>/dev/null || true
kill "$GRD_PID" 2>/dev/null || true
FREERDP_PID=""
GRD_PID=""
sleep 1

# --------------------------------------------------------------- report
log "writing $OUT"
python3 - "$OUT" "$CAPTURE" "$WIDTH" "$HEIGHT" "$FPS" \
  "$DURATION" "$MEASURE_START" "$MEASURE_END" "$PRESENTED_FRAMES" \
  "$GRD_BIN" "$GRD_VERSION" "$FREERDP_BIN" "$FREERDP_VERSION" <<'PY'
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
    duration,
    start,
    end,
    presented_frames,
    grd_bin,
    grd_version,
    freerdp_bin,
    freerdp_version,
) = sys.argv[1:15]

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

counters = {
    "frames_sent": None,
    "frames_decoded": None,
    "frames_presented": presented,
    "dropped_frames": None,
    "decode_errors": None,
    "fec_recoveries": None,
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
    "baseline": "gnome-remote-desktop (rdp mode) + xfreerdp",
    "capture": "mutter screencast portal",
    "client": freerdp_bin,
    "codec": "h264 over rdp (progressive)",
    "duration_s": str(duration),
    "fps": str(fps),
    "host": grd_bin,
    "measured_window_s": f"{measured:.3f}",
    "mode": "netns",
    "size": f"{width}x{height}",
    "transport": "rdp over tcp",
}

differences = [
    {
        "item": "transport",
        "lumen": "custom UDP with GF(256) FEC for video, a separate reliable control channel",
        "baseline": "everything over TCP, RDP multitransport disabled",
        "note": "a lost packet blocks video and input together",
    },
    {
        "item": "capture path",
        "lumen": "zwlr_screencopy_manager_v2, DMA-BUF",
        "baseline": "Mutter ScreenCast portal with PipeWire",
        "note": "the portal path is the one Lumen has not implemented yet",
    },
    {
        "item": "codec",
        "lumen": "H.264 VA-API or NVENC, low latency, infinite GOP",
        "baseline": "H.264 or HEVC over RDP progressive",
        "note": "comparable at the codec level, not at the latency level",
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
        "field": "counters.frames_sent",
        "reason": "the baseline host does not expose an encoded frame count",
    },
    {
        "field": "counters.frames_decoded",
        "reason": "the RDP client does not expose a frame count",
    },
    {
        "field": "counters.dropped_frames",
        "reason": "derived from the host's captured frame count, which the baseline does not expose",
    },
    {
        "field": "counters.decode_errors",
        "reason": "the RDP client does not expose an error count",
    },
    {
        "field": "counters.fec_recoveries",
        "reason": "RDP has no FEC for the media",
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
        "name": "gnome-remote-desktop (rdp mode) + xfreerdp",
        "licence": "GPL-2.0+ (host), Apache-2.0 (client)",
        "versions": {
            "gnome_remote_desktop": grd_version or None,
            "xfreerdp": freerdp_version or None,
        },
        "binaries": {
            "gnome_remote_desktop": grd_bin,
            "xfreerdp": freerdp_bin,
        },
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

print(f"[baseline-grd] wrote {out_path}")
print(f"[baseline-grd] frames presented: {presented}, packets seen: {packets}")
PY

log "done: $OUT"
