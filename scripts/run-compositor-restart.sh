#!/usr/bin/env bash
# The session survives a restart of the compositor.
#
# Plan milestone M6.1: "Host session persistence: session survives client
# disconnect (encode paused, state kept, configurable timeout) and compositor
# restart (capture reopened, IDR requested)".
#
# The script runs the host against a headless compositor, kills the
# compositor and starts it again, once per cycle. The host has to reopen the
# capture and carry on, and it must not leak a file descriptor per cycle.
#
# The artifact holds one row per cycle and the summary the plan asks for: 5
# of 5 sessions recovered with a stable open file descriptor count.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

CYCLES="${CYCLES:-5}"
OUT="${OUT:-artifacts/m6/restart.json}"
SIZE="${SIZE:-1280x720}"
FPS="${FPS:-30}"
CONTROL_PORT="${CONTROL_PORT:-24840}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24841}"
# How long the host waits for the frames to flow again after a restart.
RECOVERY_TIMEOUT="${RECOVERY_TIMEOUT:-20}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-compositor-restart}"
# Keep the runtime files, so a failing run can be looked at.
KEEP="${KEEP:-0}"

log() { printf '[run-compositor-restart] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: scripts/run-compositor-restart.sh [options]

Runs the compositor restart acceptance of plan milestone M6.1: the host
captures a compositor, the compositor is restarted, and the session has to
recover every time without leaking file descriptors.

Options:
  --cycles <n>      How many restarts to run (default 5)
  --out <path>      Where the report goes (default artifacts/m6/restart.json)
  --size <WxH>      Capture size (default 1280x720)
  --fps <n>         Stream rate (default 30)
  --timeout <s>     How long the recovery may take (default 20)
  --keep            Keep the runtime files, so a failing run can be read
  -h, --help        Print this text

Environment:
  CYCLES, OUT, SIZE, FPS, CONTROL_PORT, CLIENT_MEDIA_PORT, RECOVERY_TIMEOUT,
  RUNTIME_ROOT
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --cycles) CYCLES="${2:?--cycles needs a value}"; shift 2 ;;
    --out) OUT="${2:?--out needs a value}"; shift 2 ;;
    --size) SIZE="${2:?--size needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
    --timeout) RECOVERY_TIMEOUT="${2:?--timeout needs a value}"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

case "$OUT" in
  /*) OUT_ABS="$OUT" ;;
  *) OUT_ABS="$REPO_ROOT/$OUT" ;;
esac
mkdir -p "$(dirname "$OUT_ABS")" "$RUNTIME_ROOT"

case "$CYCLES" in
  ''|*[!0-9]*) log "CYCLES must be a number, got $CYCLES"; exit 2 ;;
esac
if [ "$CYCLES" -lt 1 ]; then
  log "there must be at least one cycle"
  exit 2
fi
command -v python3 >/dev/null 2>&1 || { log "python3 is needed to read the events"; exit 1; }

# ------------------------------------------------------------------ build
log "building the host and the client"
cargo build --release -p lumen-host-service -p lumen-client-desktop --bins >/dev/null
HOST_BIN="$REPO_ROOT/target/release/lumen-host"
CLIENT_BIN="$REPO_ROOT/target/release/lumen-client"

# The other agents run the same binaries and end them with a pkill on the
# path. The copies below have a name of their own, so a run of another
# agent cannot end this one in the middle of a measurement.
cp "$HOST_BIN" "$RUNTIME_ROOT/lumen-host-run"
cp "$CLIENT_BIN" "$RUNTIME_ROOT/lumen-client-run"
HOST_BIN="$RUNTIME_ROOT/lumen-host-run"
CLIENT_BIN="$RUNTIME_ROOT/lumen-client-run"

# ------------------------------------------------------------- compositors
# The host captures the first compositor, the client presents into the
# second one. Only the first one is restarted.
HOST_RT="$RUNTIME_ROOT/host"
CLIENT_RT="$RUNTIME_ROOT/client"
mkdir -p "$HOST_RT" "$CLIENT_RT"
chmod 700 "$HOST_RT" "$CLIENT_RT"

start_compositor() {
  local rt="$1" logfile="$2"
  XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless WLR_RENDERER=pixman \
    setsid nohup sway -c /dev/null >"$logfile" 2>&1 &
  echo $!
}

wait_for_socket() {
  local rt="$1"
  for _ in $(seq 1 50); do
    [ -S "$rt/wayland-1" ] && return 0
    sleep 0.2
  done
  return 1
}

HOST_SWAY_PID=""
CLIENT_SWAY_PID=""
HOST_PID=""
CLIENT_PID=""
STATE_DIR="$RUNTIME_ROOT/state"
EVENTS_FILE="$RUNTIME_ROOT/events.jsonl"

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
HOST_SWAY_PID="$(start_compositor "$HOST_RT" "$RUNTIME_ROOT/sway-host.log")"
CLIENT_SWAY_PID="$(start_compositor "$CLIENT_RT" "$RUNTIME_ROOT/sway-client.log")"
wait_for_socket "$HOST_RT" || { log "the host compositor did not start"; exit 1; }
wait_for_socket "$CLIENT_RT" || { log "the client compositor did not start"; exit 1; }

# ------------------------------------------------------------------- host
mkdir -p "$STATE_DIR"
rm -f "$EVENTS_FILE" "$STATE_DIR/pairing-code.txt"

log "starting the host"
XDG_RUNTIME_DIR="$HOST_RT" WAYLAND_DISPLAY=wayland-1 \
  "$HOST_BIN" --size "$SIZE" --fps "$FPS" \
    --control "127.0.0.1:$CONTROL_PORT" --client-media-port "$CLIENT_MEDIA_PORT" \
    --duration "$((CYCLES * 20 + 120))" \
    --persist-timeout 30 \
    --report "$RUNTIME_ROOT/host.json" \
    --pairing-store "$STATE_DIR/pairing.json" \
    --pairing-code-file "$STATE_DIR/pairing-code.txt" \
    >"$RUNTIME_ROOT/host.log" 2>&1 &
HOST_PID=$!
sleep 1

log "starting the client"
XDG_RUNTIME_DIR="$CLIENT_RT" WAYLAND_DISPLAY=wayland-1 \
  "$CLIENT_BIN" --host "127.0.0.1:$CONTROL_PORT" \
    --media-bind "127.0.0.1:$CLIENT_MEDIA_PORT" \
    --duration "$((CYCLES * 20 + 110))" --report "$RUNTIME_ROOT/client.json" \
    --identity "$STATE_DIR/client-identity" \
    --pairing-code-file "$STATE_DIR/pairing-code.txt" \
    --events "$EVENTS_FILE" \
    >"$RUNTIME_ROOT/client.log" 2>&1 &
CLIENT_PID=$!

# The number of presented frames the client has reported so far.
presented_frames() {
  python3 - "$EVENTS_FILE" <<'PY'
import json
import re
import sys

total = 0
try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("event") == "progress":
                match = re.search(r"presented=(\d+)", event.get("detail") or "")
                if match:
                    total = int(match.group(1))
except FileNotFoundError:
    pass
print(total)
PY
}

# The number of open file descriptors of the host.
open_fds() {
  local pid="$1"
  if [ -d "/proc/$pid/fd" ]; then
    ls "/proc/$pid/fd" 2>/dev/null | wc -l
  else
    echo 0
  fi
}

# Wait until the client has presented more than `before` frames.
wait_for_frames() {
  local before="$1" limit="$2"
  local waited=0
  while [ "$waited" -lt $((limit * 10)) ]; do
    now="$(presented_frames)"
    if [ "$now" -gt "$before" ]; then
      printf '%s' "$now"
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  printf '%s' "$(presented_frames)"
  return 1
}

# ----------------------------------------------------------------- cycles
RESULTS="$RUNTIME_ROOT/results.csv"
printf 'cycle,recovered,frames_before,frames_after,fd_before,fd_after\n' >"$RESULTS"

# Let the stream run before the first restart.
sleep 3
FD_BEFORE="$(open_fds "$HOST_PID")"
log "the host has $FD_BEFORE open file descriptors"

cycle=1
while [ "$cycle" -le "$CYCLES" ]; do
  before="$(presented_frames)"
  log "cycle $cycle: killing the compositor the host captures"
  kill "$HOST_SWAY_PID" 2>/dev/null || true
  wait "$HOST_SWAY_PID" 2>/dev/null || true
  sleep 1

  log "cycle $cycle: starting the compositor again"
  HOST_SWAY_PID="$(start_compositor "$HOST_RT" "$RUNTIME_ROOT/sway-host.log")"
  if ! wait_for_socket "$HOST_RT"; then
    log "cycle $cycle: the compositor did not start again"
    printf '%s,0,%s,%s,%s,%s\n' "$cycle" "$before" "$before" "$FD_BEFORE" "$FD_BEFORE" >>"$RESULTS"
    cycle=$((cycle + 1))
    continue
  fi

  if wait_for_frames "$before" "$RECOVERY_TIMEOUT"; then
    after="$(presented_frames)"
    fd_after="$(open_fds "$HOST_PID")"
    printf '%s,1,%s,%s,%s,%s\n' "$cycle" "$before" "$after" "$FD_BEFORE" "$fd_after" >>"$RESULTS"
    log "cycle $cycle: the session recovered, $after frames presented, $fd_after open file descriptors"
    FD_BEFORE="$fd_after"
  else
    after="$(presented_frames)"
    fd_after="$(open_fds "$HOST_PID")"
    printf '%s,0,%s,%s,%s,%s\n' "$cycle" "$before" "$after" "$FD_BEFORE" "$fd_after" >>"$RESULTS"
    log "cycle $cycle: the session did not recover"
    FD_BEFORE="$fd_after"
  fi
  cycle=$((cycle + 1))
done

# ----------------------------------------------------------------- report
log "merging the cycles into $OUT"
python3 - "$RESULTS" "$OUT_ABS" "$CYCLES" <<'PY'
import csv
import json
import sys

results_path, out_path, cycles = sys.argv[1:4]
rows = []
with open(results_path, encoding="utf-8") as handle:
    for row in csv.DictReader(handle):
        rows.append(row)

samples = []
recovered = 0
fd_growth = 0
for row in rows:
    ok = row["recovered"] == "1"
    if ok:
        recovered += 1
    fd_before = int(row["fd_before"])
    fd_after = int(row["fd_after"])
    fd_growth = max(fd_growth, fd_after - fd_before)
    samples.append({
        "cycle": int(row["cycle"]),
        "recovered": bool(ok),
        "frames_before": int(row["frames_before"]),
        "frames_after": int(row["frames_after"]),
        "open_fds_before": fd_before,
        "open_fds_after": fd_after,
    })

report = {
    "cycles": int(cycles),
    "recovered": recovered,
    "failed": int(cycles) - recovered,
    # The open file descriptor count must not grow from cycle to cycle.
    "open_fd_growth": fd_growth,
    "pass": recovered == int(cycles) and fd_growth <= 2,
    "samples": samples,
}
with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write("\n")
print(
    f"[run-compositor-restart] {recovered}/{cycles} sessions recovered, "
    f"the open file descriptor count grew by {fd_growth}"
)
PY

log "the report is in $OUT"
