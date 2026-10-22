#!/usr/bin/env bash
# Measure the input host-injection latency over the network namespace pair.
#
# Plan milestone M3.7: "timestamp at client event receipt and at host
# injection call, map host clock through the control channel, report p50/p99
# excluding the client's own polling interval".
#
# The measurement is the leg between the client stamping an event and the
# host injecting it, over the veth pair of scripts/netns-up.sh:
#
#   1. the host role of crates/lumen-input-inject/examples/input_latency.rs
#      binds the input socket inside the lumen-host namespace and injects
#      every event it receives into an Xvfb display with XTEST,
#   2. the client role of the same example generates the events inside the
#      lumen-client namespace at a fixed rate,
#   3. the counters of the host are merged into the report.
#
# The two namespaces share one kernel clock, so no clock mapping is needed
# here. The number is a software latency: it covers no display, no compositor
# and no real client. It is marked provisional in the report.
#
# Usage:
#   scripts/run-input-latency.sh --duration 60 --out artifacts/m3/input.json
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

# --------------------------------------------------------------- settings
DURATION="${DURATION:-60}"
SIZE="${SIZE:-1920x1080}"
# The rate of the generated events, in hertz. It is also the polling period
# of the client, in microseconds, which the report records.
FPS="${FPS:-1000}"
OUT="${OUT:-artifacts/m3/input.json}"
# The CPUs the two roles are pinned to. Empty means no pinning.
CPUS="${CPUS:-}"
# The runtime state: the Xvfb display, the namespaces and the logs.
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-input-latency}"
# The namespaces and the veth pair of scripts/netns-up.sh.
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"
HOST_ADDR="${HOST_ADDR:-10.77.0.1}"
CLIENT_ADDR="${CLIENT_ADDR:-10.77.0.2}"
# The port of the input socket inside the host namespace.
INPUT_PORT="${INPUT_PORT:-24803}"
# Where the build puts the binaries.
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"

log() { printf '[run-input-latency] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: scripts/run-input-latency.sh [options]

Measure the input host-injection latency over the network namespace pair.
Plan milestone M3.7. The report holds the p50 and the p99 of the latency
between the client stamping an event and the host injecting it.

Options:
  --duration <s>   How long the run takes, in seconds (default 60)
  --out <path>     Where the report goes (default artifacts/m3/input.json)
  --cpus <list>    Pin the two roles to these CPUs, for example 0-3
  --size <WxH>     The size of the Xvfb screen and of the client surface
                   (default 1920x1080)
  --fps <n>        The rate of the generated events, in hertz (default 1000)
  --help           Print this text

Environment:
  DURATION, OUT, CPUS, SIZE, FPS, RUNTIME_ROOT, HOST_NS, CLIENT_NS,
  HOST_ADDR, CLIENT_ADDR, INPUT_PORT

The number is a software latency. It covers no display, no compositor and no
real client, so the report marks it provisional.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --duration) DURATION="${2:?--duration needs a value}"; shift 2 ;;
    --out)     OUT="${2:?--out needs a value}"; shift 2 ;;
    --cpus)    CPUS="${2:?--cpus needs a value}"; shift 2 ;;
    --size)    SIZE="${2:?--size needs a value}"; shift 2 ;;
    --fps)     FPS="${2:?--fps needs a value}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

case "$DURATION" in
  ''|*[!0-9]*) log "DURATION must be a number of seconds, got $DURATION"; exit 2 ;;
esac
if [ "$DURATION" -lt 1 ]; then
  log "DURATION must be at least one second, got $DURATION"
  exit 2
fi
case "$FPS" in
  ''|*[!0-9]*) log "FPS must be a number, got $FPS"; exit 2 ;;
esac
if [ "$FPS" -lt 1 ]; then
  log "FPS must be at least one event per second, got $FPS"
  exit 2
fi
case "$SIZE" in
  *x*) WIDTH="${SIZE%x*}"; HEIGHT="${SIZE#*x}" ;;
  *) log "SIZE must be WxH, got $SIZE"; exit 2 ;;
esac
case "$WIDTH" in ''|*[!0-9]*) log "the width of SIZE must be a number"; exit 2 ;; esac
case "$HEIGHT" in ''|*[!0-9]*) log "the height of SIZE must be a number"; exit 2 ;; esac

command -v python3 >/dev/null 2>&1 || { log "python3 is needed to merge the counters"; exit 1; }
command -v sudo >/dev/null 2>&1 || { log "sudo is needed for the network namespaces"; exit 1; }
command -v Xvfb >/dev/null 2>&1 || { log "Xvfb is needed as the injection target"; exit 1; }
sudo -n true 2>/dev/null || { log "sudo -n does not work here"; exit 1; }
if [ -n "$CPUS" ]; then
  command -v taskset >/dev/null 2>&1 || { log "taskset is needed for --cpus"; exit 1; }
fi

case "$OUT" in
  /*) OUT_ABS="$OUT" ;;
  *) OUT_ABS="$REPO_ROOT/$OUT" ;;
esac
case "$RUNTIME_ROOT" in
  /*) RUNTIME_ROOT_ABS="$RUNTIME_ROOT" ;;
  *) RUNTIME_ROOT_ABS="$REPO_ROOT/$RUNTIME_ROOT" ;;
esac

OUT_DIR="$(dirname "$OUT_ABS")"
mkdir -p "$OUT_DIR" "$RUNTIME_ROOT_ABS"

# The number of events the client sends and the host expects.
EVENTS=$((DURATION * FPS))
HOST_JSON="$RUNTIME_ROOT_ABS/host.json"
HOST_LOG="$RUNTIME_ROOT_ABS/host.log"
CLIENT_LOG="$RUNTIME_ROOT_ABS/client.log"
XVFB_PID=""
HOST_PID=""
XVFB_DISPLAY=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  log "cleaning up"
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null || true
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
  HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" "$REPO_ROOT/scripts/netns-down.sh" \
    >/dev/null 2>&1 || true
  rm -rf "$RUNTIME_ROOT_ABS"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ------------------------------------------------------------------ build
log "building the input latency example"
cargo build --release -p lumen-input-inject --example input_latency >/dev/null
EXAMPLE="$TARGET_DIR/release/examples/input_latency"
[ -x "$EXAMPLE" ] || { log "the example was not built at $EXAMPLE"; exit 1; }

# --------------------------------------------------------------- the Xvfb
# The host injects with XTEST, so it needs a display. Xvfb picks its own
# display number with -displayfd, so the number cannot collide with a display
# that is already in use.
log "starting the Xvfb display"
XVFB_FD_FILE="$RUNTIME_ROOT_ABS/xvfb.display"
Xvfb -displayfd 3 -screen 0 "${WIDTH}x${HEIGHT}x24" 3>"$XVFB_FD_FILE" \
  >"$RUNTIME_ROOT_ABS/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 50); do
  XVFB_DISPLAY="$(cat "$XVFB_FD_FILE" 2>/dev/null || true)"
  [ -n "$XVFB_DISPLAY" ] && break
  sleep 0.1
done
[ -n "$XVFB_DISPLAY" ] || { log "Xvfb did not report a display number"; exit 1; }
log "the display is :$XVFB_DISPLAY"

# ------------------------------------------------------------- namespaces
log "setting up the network namespaces"
HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" \
  HOST_IP="$HOST_ADDR/24" CLIENT_IP="$CLIENT_ADDR/24" \
  "$REPO_ROOT/scripts/netns-up.sh"

# -------------------------------------------------------------- the roles
# The roles are pinned when --cpus is given, so the measurement does not move
# between the cores.
PIN=()
if [ -n "$CPUS" ]; then
  PIN=(taskset -c "$CPUS")
  log "pinning the roles to the CPUs $CPUS"
fi

log "starting the host injector in the $HOST_NS namespace"
sudo -n ip netns exec "$HOST_NS" env DISPLAY=":$XVFB_DISPLAY" \
  "${PIN[@]}" "$EXAMPLE" host \
    --bind "$HOST_ADDR:$INPUT_PORT" \
    --expect "$EVENTS" \
    --label "netns-input-latency" \
    --width "$WIDTH" --height "$HEIGHT" \
    --idle-ms 10000 \
    --max-s "$((DURATION + 60))" \
    --out "$HOST_JSON" \
    >"$HOST_LOG" 2>&1 &
HOST_PID=$!

# The host prints a listening line before it waits for the client.
for _ in $(seq 1 100); do
  grep -q "host listening" "$HOST_LOG" 2>/dev/null && break
  sleep 0.1
done
if ! grep -q "host listening" "$HOST_LOG" 2>/dev/null; then
  log "the host did not start"
  sed 's/^/    /' "$HOST_LOG" 2>/dev/null || true
  exit 1
fi

log "running the client generator for ${DURATION}s at ${FPS} events per second"
LOAD_BEFORE="$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo unknown)"
CLIENT_STATUS=0
sudo -n ip netns exec "$CLIENT_NS" "${PIN[@]}" "$EXAMPLE" client \
  --to "$HOST_ADDR:$INPUT_PORT" \
  --bind "$CLIENT_ADDR:0" \
  --count "$EVENTS" \
  --rate-hz "$FPS" \
  --width "$WIDTH" --height "$HEIGHT" \
  >"$CLIENT_LOG" 2>&1 || CLIENT_STATUS=$?
log "the client generator finished with the status $CLIENT_STATUS"

# The host stops when it has seen every event.
wait "$HOST_PID" || log "the host injector exited with a failure"
HOST_PID=""

if [ ! -f "$HOST_JSON" ]; then
  log "the run produced no report"
  sed 's/^/    /' "$HOST_LOG" 2>/dev/null || true
  exit 1
fi

# ------------------------------------------------------------------ merge
log "merging the counters into $OUT"
LOAD_AFTER="$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo unknown)"
python3 - "$HOST_JSON" "$OUT_ABS" "$DURATION" "$FPS" "$WIDTH" "$HEIGHT" \
  "netns" "$CPUS" "$LOAD_BEFORE" "$LOAD_AFTER" <<'PY'
import json
import sys

(
    host_path,
    out_path,
    duration,
    fps,
    width,
    height,
    mode,
    cpus,
    load_before,
    load_after,
) = sys.argv[1:11]

with open(host_path, encoding="utf-8") as handle:
    report = json.load(handle)

# The polling period of the client is the interval between two generated
# events. The example stamps an event when it seals it, so the interval is
# already outside the measurement; it is recorded so the report says so.
polling_us = round(1_000_000 / float(fps))

report["mode"] = mode
report["duration_s"] = int(duration)
report["rate_hz"] = int(fps)
report["client_polling_interval_us"] = polling_us
report["clock_offset_us"] = 0
report["clock_note"] = (
    "both namespaces share one kernel clock, so no clock mapping was needed"
)
report["cpus"] = cpus or "unpinned"
report["load_before"] = load_before
report["load_after"] = load_after
report["measures"] = "client stamp to host injection call"
report["provisional"] = True
report["provisional_reason"] = (
    "software latency: no display, no compositor and no real client are in the "
    "measurement"
)

with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle)
    handle.write("\n")

latency = report.get("latency_us") or {}
print(
    "[run-input-latency] p50 {p50} us, p99 {p99} us over {count} events".format(
        p50=latency.get("p50", "?"),
        p99=latency.get("p99", "?"),
        count=latency.get("count", "?"),
    )
)
PY

# The host runs as root inside the namespace, so the files it wrote belong to
# root. Hand them back to the user.
if [ "$(id -u)" -ne 0 ]; then
  sudo -n chown "$(id -u):$(id -g)" "$OUT_ABS" 2>/dev/null || true
fi

log "done: $OUT"
