#!/usr/bin/env bash
# The low-power client profile: decode 1080p60 on two pinned vCPUs.
#
# Plan milestone M5.3: "Tier B proxy script: client pinned to two vCPUs with
# taskset -c 0,1, decodes 1080p60 for N minutes, reports decode percentiles
# and CPU".
#
# The Tier B device of the plan is a Celeron N4020, which does not exist
# here. This script is the proxy: the client is pinned to two vCPUs with
# taskset, the stream is 1080p60, and the run reports the decode percentiles
# and the share of the two cores the client used.
#
# The acceptance of the plan for this run:
#   * the decode p99 is at most 12 ms,
#   * no frame is dropped over the whole run,
#   * the client uses at most 85 % of the two cores.
#
# Every number is provisional: it is a proxy on the build machine, not the
# Tier B device, and it is a software latency, not a glass-to-glass one.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

CPUS="${CPUS:-0,1}"
DURATION="${DURATION:-300}"
REPORT="${REPORT:-artifacts/m5/tierb.json}"
SIZE="${SIZE:-1920x1080}"
FPS="${FPS:-60}"
CONTROL_PORT="${CONTROL_PORT:-24820}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24821}"
# The acceptance limits of the plan.
DECODE_P99_MS="${DECODE_P99_MS:-12}"
CPU_LIMIT_PERCENT="${CPU_LIMIT_PERCENT:-85}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-tier-b}"
# Keep the runtime files, so a failing run can be looked at.
KEEP="${KEEP:-0}"

log() { printf '[run-tier-b] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: scripts/run-tier-b.sh [options]

Runs the Tier B proxy of plan milestone M5.3: the client is pinned to two
vCPUs and decodes a 1080p60 stream for the whole run.

Options:
  --cpus <list>      The vCPUs the client is pinned to (default 0,1)
  --duration <s>     How long the run lasts (default 300)
  --report <path>    Where the report goes (default artifacts/m5/tierb.json)
  --size <WxH>       Capture size (default 1920x1080)
  --fps <n>          Stream rate (default 60)
  --decode-p99 <ms>  The decode p99 limit (default 12)
  --cpu-limit <n>    The share of the pinned cores the client may use (default 85)
  --keep             Keep the runtime files, so a failing run can be read
  -h, --help         Print this text

Environment:
  CPUS, DURATION, REPORT, SIZE, FPS, CONTROL_PORT, CLIENT_MEDIA_PORT,
  DECODE_P99_MS, CPU_LIMIT_PERCENT, RUNTIME_ROOT
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --cpus) CPUS="${2:?--cpus needs a value}"; shift 2 ;;
    --duration) DURATION="${2:?--duration needs a value}"; shift 2 ;;
    --report) REPORT="${2:?--report needs a value}"; shift 2 ;;
    --size) SIZE="${2:?--size needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
    --decode-p99) DECODE_P99_MS="${2:?--decode-p99 needs a value}"; shift 2 ;;
    --cpu-limit) CPU_LIMIT_PERCENT="${2:?--cpu-limit needs a value}"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

case "$REPORT" in
  /*) REPORT_ABS="$REPORT" ;;
  *) REPORT_ABS="$REPO_ROOT/$REPORT" ;;
esac
mkdir -p "$(dirname "$REPORT_ABS")" "$RUNTIME_ROOT"

case "$DURATION" in
  ''|*[!0-9]*) log "DURATION must be a number of seconds, got $DURATION"; exit 2 ;;
esac
if [ "$DURATION" -lt 5 ]; then
  log "the run has to last at least 5 seconds"
  exit 2
fi
command -v taskset >/dev/null 2>&1 || { log "taskset is needed to pin the client"; exit 1; }
command -v python3 >/dev/null 2>&1 || { log "python3 is needed to read the reports"; exit 1; }

# The number of cores the client is pinned to.
CPU_COUNT="$(printf '%s' "$CPUS" | tr ',' '\n' | grep -c .)"

# ------------------------------------------------------------------ build
log "building the host, the client and the bench tool"
cargo build --release -p lumen-host-service -p lumen-client-desktop -p lumen-bench --bins \
  >/dev/null
HOST_BIN="$REPO_ROOT/target/release/lumen-host"
CLIENT_BIN="$REPO_ROOT/target/release/lumen-client"

# The other agents run the same binaries and end them with a pkill on the
# path. The copies below have a name of their own, so a run of another
# agent cannot end this one in the middle of a measurement.
cp "$HOST_BIN" "$RUNTIME_ROOT/lumen-host-run"
cp "$CLIENT_BIN" "$RUNTIME_ROOT/lumen-client-run"
HOST_BIN="$RUNTIME_ROOT/lumen-host-run"
CLIENT_BIN="$RUNTIME_ROOT/lumen-client-run"
BENCH_BIN="$REPO_ROOT/target/release/lumen-bench"

# ------------------------------------------------------------- compositors
# The host captures a headless compositor at the size of the run, so the
# compositor is configured with the mode of the stream.
HOST_RT="$RUNTIME_ROOT/host"
CLIENT_RT="$RUNTIME_ROOT/client"
mkdir -p "$HOST_RT" "$CLIENT_RT"
chmod 700 "$HOST_RT" "$CLIENT_RT"

WIDTH="${SIZE%x*}"
HEIGHT="${SIZE#*x}"
cat >"$HOST_RT/config" <<EOF
output HEADLESS-1 mode ${WIDTH}x${HEIGHT}@${FPS}Hz
EOF

start_compositor() {
  local rt="$1" logfile="$2" config="$3"
  if [ -f "$config" ]; then
    XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless WLR_RENDERER=pixman \
      setsid nohup sway -c "$config" >"$logfile" 2>&1 &
  else
    XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless WLR_RENDERER=pixman \
      setsid nohup sway -c /dev/null >"$logfile" 2>&1 &
  fi
  echo $!
}

HOST_SWAY_PID=""
CLIENT_SWAY_PID=""
HOST_PID=""
CLIENT_PID=""
STATE_DIR="$RUNTIME_ROOT/state"

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  log "cleaning up"
  [ -n "$CLIENT_PID" ] && kill "$CLIENT_PID" 2>/dev/null || true
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null || true
  [ -n "$HOST_SWAY_PID" ] && kill "$HOST_SWAY_PID" 2>/dev/null || true
  [ -n "$CLIENT_SWAY_PID" ] && kill "$CLIENT_SWAY_PID" 2>/dev/null || true
  if [ "$KEEP" = 1 ]; then
    log "the runtime files stay in $RUNTIME_ROOT"
  else
    rm -rf "$RUNTIME_ROOT"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "starting the headless compositors"
HOST_SWAY_PID="$(start_compositor "$HOST_RT" "$RUNTIME_ROOT/sway-host.log" "$HOST_RT/config")"
CLIENT_SWAY_PID="$(start_compositor "$CLIENT_RT" "$RUNTIME_ROOT/sway-client.log" "")"
for _ in $(seq 1 50); do
  if [ -S "$HOST_RT/wayland-1" ] && [ -S "$CLIENT_RT/wayland-1" ]; then
    break
  fi
  sleep 0.2
done
[ -S "$HOST_RT/wayland-1" ] || { log "the host compositor did not start"; exit 1; }
[ -S "$CLIENT_RT/wayland-1" ] || { log "the client compositor did not start"; exit 1; }

HOST_SIZE="$(XDG_RUNTIME_DIR="$HOST_RT" WAYLAND_DISPLAY=wayland-1 \
  "$HOST_BIN" --list-outputs | head -1 | awk '{print $2}')"
log "the host compositor offers $HOST_SIZE"
if [ -z "$HOST_SIZE" ]; then
  log "cannot read the output size"
  exit 1
fi

# ------------------------------------------------------------------- run
mkdir -p "$STATE_DIR"
# The pairing state of this run is fresh: the code the host writes must
# not be the one of an earlier run.
rm -f "$STATE_DIR/pairing-code.txt"
log "running the host and the pinned client for ${DURATION}s"
XDG_RUNTIME_DIR="$HOST_RT" WAYLAND_DISPLAY=wayland-1 \
  "$HOST_BIN" --size "$HOST_SIZE" --fps "$FPS" \
    --control "127.0.0.1:$CONTROL_PORT" --client-media-port "$CLIENT_MEDIA_PORT" \
    --duration "$DURATION" --report "$RUNTIME_ROOT/host.json" \
    --pairing-store "$STATE_DIR/pairing.json" \
    --pairing-code-file "$STATE_DIR/pairing-code.txt" \
    >"$RUNTIME_ROOT/host.log" 2>&1 &
HOST_PID=$!
sleep 1

# The client is pinned to the vCPUs of the plan. It outlives the host by a
# few seconds, so every frame the host sent is counted on the client side
# and the tail of the run does not look like a drop.
XDG_RUNTIME_DIR="$CLIENT_RT" WAYLAND_DISPLAY=wayland-1 \
  taskset -c "$CPUS" \
  "$CLIENT_BIN" --host "127.0.0.1:$CONTROL_PORT" \
    --media-bind "127.0.0.1:$CLIENT_MEDIA_PORT" \
    --duration "$((DURATION + 5))" --report "$RUNTIME_ROOT/client.json" \
    --identity "$STATE_DIR/client-identity" \
    --pairing-code-file "$STATE_DIR/pairing-code.txt" \
    >"$RUNTIME_ROOT/client.log" 2>&1 &
CLIENT_PID=$!

# The CPU time of the client, in clock ticks, before and after the run.
cpu_ticks() {
  local pid="$1"
  if [ ! -r "/proc/$pid/stat" ]; then
    echo 0
    return
  fi
  awk '{ print $14 + $15 }' "/proc/$pid/stat" 2>/dev/null || echo 0
}

TICKS_BEFORE="$(cpu_ticks "$CLIENT_PID")"
CLOCK_TICKS="$(getconf CLK_TCK 2>/dev/null || echo 100)"
START_EPOCH="$(date +%s.%N)"

# The client is sampled while it runs, because the accounting of a process
# that has exited is gone.
TICKS_LAST="$TICKS_BEFORE"
while kill -0 "$CLIENT_PID" 2>/dev/null; do
  sample="$(cpu_ticks "$CLIENT_PID")"
  if [ "$sample" != 0 ]; then
    TICKS_LAST="$sample"
  fi
  sleep 0.5
done
END_EPOCH="$(date +%s.%N)"

wait "$CLIENT_PID" || log "the client exited with a failure"
CLIENT_PID=""
TICKS_AFTER="$TICKS_LAST"

wait "$HOST_PID" || log "the host exited with a failure"
HOST_PID=""

# ---------------------------------------------------------------- report
log "merging the reports into $REPORT"
"$BENCH_BIN" report --host "$RUNTIME_ROOT/host.json" --client "$RUNTIME_ROOT/client.json" \
  --out "$RUNTIME_ROOT/merged.json" \
  --set "mode=loopback" --set "size=$HOST_SIZE" --set "fps=$FPS" \
  --set "duration_s=$DURATION" --set "tier=b-proxy" --set "cpus=$CPUS" \
  --set "transport=custom-udp+fec-gf256" --set "codec=h264-vaapi" \
  --set "capture=wlroots-screencopy" \
  >/dev/null || { log "cannot merge the reports"; exit 1; }

python3 - "$RUNTIME_ROOT/merged.json" "$RUNTIME_ROOT/client.json" "$REPORT_ABS" "$CPUS" \
    "$CPU_COUNT" "$DURATION" "$DECODE_P99_MS" "$CPU_LIMIT_PERCENT" \
    "$TICKS_BEFORE" "$TICKS_AFTER" "$CLOCK_TICKS" "$START_EPOCH" "$END_EPOCH" <<'PY'
import json
import sys

(
    merged_path,
    client_path,
    out_path,
    cpus,
    cpu_count,
    duration,
    decode_limit,
    cpu_limit,
    ticks_before,
    ticks_after,
    clock_ticks,
    start_epoch,
    end_epoch,
) = sys.argv[1:15]

with open(merged_path, encoding="utf-8") as handle:
    merged = json.load(handle)
with open(client_path, encoding="utf-8") as handle:
    client = json.load(handle)

counters = merged.get("counters", {})
stages = merged.get("stages", {})
decode = stages.get("decode", {})

decode_p99_ms = decode.get("p99_us", 0) / 1000.0
decode_p50_ms = decode.get("p50_us", 0) / 1000.0

# The client counts the frames it dropped: the ones the decode stage was too
# busy for and the ones it waited for a keyframe for. The counter of the run
# report is the difference between the frames the host captured and the
# frames the client decoded, which also holds the frames that were in
# flight when the run ended.
dropped = int(client.get("frames_dropped", 0))
frames_in_flight = counters.get("dropped_frames", 0)
presented = counters.get("frames_presented", 0)
presented_fps = presented / float(duration) if duration else 0.0

ticks = int(ticks_after) - int(ticks_before)
cpu_seconds = ticks / float(clock_ticks)
wall = float(end_epoch) - float(start_epoch)
cores = int(cpu_count)
cpu_percent = 100.0 * cpu_seconds / (cores * wall) if wall > 0 else 0.0

report = {
    "tier": "b-proxy",
    "cpus": cpus,
    "cpu_count": cores,
    "duration_s": int(duration),
    "decode_p50_ms": round(decode_p50_ms, 3),
    "decode_p99_ms": round(decode_p99_ms, 3),
    "decode_p99_limit_ms": float(decode_limit),
    "frames_decoded": counters.get("frames_decoded", 0),
    "frames_presented": counters.get("frames_presented", 0),
    "presented_fps": round(presented_fps, 2),
    "dropped_frames": dropped,
    "frames_in_flight_at_the_end": frames_in_flight,
    "decode_errors": counters.get("decode_errors", 0),
    "client_cpu_seconds": round(cpu_seconds, 3),
    "client_cpu_percent_of_pinned_cores": round(cpu_percent, 2),
    "client_cpu_limit_percent": float(cpu_limit),
    "pass": (
        decode_p99_ms <= float(decode_limit)
        and dropped == 0
        and cpu_percent <= float(cpu_limit)
    ),
    "provisional": True,
    "provisional_reason": (
        "a two vCPU proxy on the build machine, not the Tier B device; "
        "software pipeline latency, not glass-to-glass"
    ),
    "notes": (
        "the client presents the frames on the CPU (decision D49), so at "
        "1080p60 the colour conversion and the presentation take longer "
        "than the frame interval and the client drops the frames that are "
        "late. The zero-copy path (O-7) is the prerequisite for the Tier B "
        "target of the plan."
    ),
}
with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write("\n")

print(
    f"[run-tier-b] decode p50 {decode_p50_ms:.2f} ms, p99 {decode_p99_ms:.2f} ms "
    f"(limit {decode_limit} ms), {dropped} frames dropped, "
    f"{frames_in_flight} frames were in flight when the run ended, "
    f"the client used {cpu_percent:.1f} % of {cores} cores (limit {cpu_limit} %)"
)
PY

log "the report is in $REPORT"
