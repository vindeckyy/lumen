#!/usr/bin/env bash
# Unattended reconnect: the client comes back after the session ends.
#
# Plan milestone M6.2: "Unattended reconnect: client retries with 200 ms to
# 2 s backoff, full reconnect <= 2 s measured from link-up".
#
# The script runs the host and the client the way scripts/run-e2e.sh does,
# but it kills the client and starts it again, once per cycle. The host
# keeps the session between the cycles, so the client finds the session
# where it left it.
#
# The measurement is the time from the start of the client process to the
# moment the session is established. That is the whole reconnect, from the
# process start, and it is more than the plan asks for: the plan measures
# from the moment the link is up, and the process start is earlier.
#
# The artifact holds one row per cycle and the summary the plan asks for:
# 20 of 20 cycles reconnected and a maximum reconnect of at most 2000 ms.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

CYCLES="${CYCLES:-20}"
OUT="${OUT:-artifacts/m6/reconnect.json}"
LIMIT_MS="${LIMIT_MS:-2000}"
SIZE="${SIZE:-1280x720}"
FPS="${FPS:-30}"
CONTROL_PORT="${CONTROL_PORT:-24830}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24831}"
# How long the host keeps the session after the client left.
PERSIST_TIMEOUT="${PERSIST_TIMEOUT:-60}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-reconnect}"

log() { printf '[run-reconnect] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: scripts/run-reconnect.sh [options]

Runs the reconnect acceptance of plan milestone M6.2: the client is killed
and started again, once per cycle, and every cycle has to reconnect within
the limit.

Options:
  --cycles <n>      How many reconnect cycles to run (default 20)
  --out <path>      Where the report goes (default artifacts/m6/reconnect.json)
  --limit-ms <n>    The reconnect limit in milliseconds (default 2000)
  --size <WxH>      Capture size (default 1280x720)
  --fps <n>         Stream rate (default 30)
  --timeout <s>     How long the host keeps the session (default 60)
  -h, --help        Print this text

Environment:
  CYCLES, OUT, LIMIT_MS, SIZE, FPS, CONTROL_PORT, CLIENT_MEDIA_PORT,
  PERSIST_TIMEOUT, RUNTIME_ROOT
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --cycles) CYCLES="${2:?--cycles needs a value}"; shift 2 ;;
    --out) OUT="${2:?--out needs a value}"; shift 2 ;;
    --limit-ms) LIMIT_MS="${2:?--limit-ms needs a value}"; shift 2 ;;
    --size) SIZE="${2:?--size needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
    --timeout) PERSIST_TIMEOUT="${2:?--timeout needs a value}"; shift 2 ;;
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

# ------------------------------------------------------------- compositor
# The client presents into a headless compositor, so one is started.
CLIENT_RT="$RUNTIME_ROOT/client"
mkdir -p "$CLIENT_RT"
chmod 700 "$CLIENT_RT"

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
  [ -n "$CLIENT_SWAY_PID" ] && kill "$CLIENT_SWAY_PID" 2>/dev/null || true
  rm -rf "$RUNTIME_ROOT"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "starting the headless compositor for the client"
XDG_RUNTIME_DIR="$CLIENT_RT" WLR_BACKENDS=headless WLR_RENDERER=pixman \
  setsid nohup sway -c /dev/null >"$RUNTIME_ROOT/sway-client.log" 2>&1 &
CLIENT_SWAY_PID=$!
for _ in $(seq 1 50); do
  [ -S "$CLIENT_RT/wayland-1" ] && break
  sleep 0.2
done
[ -S "$CLIENT_RT/wayland-1" ] || { log "the client compositor did not start"; exit 1; }

# ------------------------------------------------------------------- host
mkdir -p "$STATE_DIR"
rm -f "$EVENTS_FILE" "$STATE_DIR/pairing-code.txt"

log "starting the host with a session timeout of ${PERSIST_TIMEOUT}s"
"$HOST_BIN" --size "$SIZE" --fps "$FPS" \
  --control "127.0.0.1:$CONTROL_PORT" --client-media-port "$CLIENT_MEDIA_PORT" \
  --duration "$((CYCLES * 4 + 120))" \
  --persist-timeout "$PERSIST_TIMEOUT" \
  --report "$RUNTIME_ROOT/host.json" \
  --pairing-store "$STATE_DIR/pairing.json" \
  --pairing-code-file "$STATE_DIR/pairing-code.txt" \
  >"$RUNTIME_ROOT/host.log" 2>&1 &
HOST_PID=$!

# Give the host time to bind the control listener before the client dials.
sleep 1

# ----------------------------------------------------------------- cycles
log "running $CYCLES reconnect cycles"
: >"$EVENTS_FILE"
RESULTS="$RUNTIME_ROOT/results.csv"
printf 'cycle,reconnect_ms,established,detail\n' >"$RESULTS"

cycle=1
while [ "$cycle" -le "$CYCLES" ]; do
  before=$(wc -l <"$EVENTS_FILE")
  XDG_RUNTIME_DIR="$CLIENT_RT" WAYLAND_DISPLAY=wayland-1 \
    "$CLIENT_BIN" --host "127.0.0.1:$CONTROL_PORT" \
      --media-bind "127.0.0.1:$CLIENT_MEDIA_PORT" \
      --duration 3 --report "$RUNTIME_ROOT/client.json" \
      --identity "$STATE_DIR/client-identity" \
      --pairing-code-file "$STATE_DIR/pairing-code.txt" \
      --events "$EVENTS_FILE" \
      >"$RUNTIME_ROOT/client.log" 2>&1 &
  CLIENT_PID=$!

  # Wait for the session to be established.
  established=0
  elapsed=""
  for _ in $(seq 1 100); do
    line="$(sed -n "$((before + 1)),\$p" "$EVENTS_FILE" 2>/dev/null |
      grep '"event":"established"' | tail -1 || true)"
    if [ -n "$line" ]; then
      elapsed="$(printf '%s' "$line" |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["elapsed_ms"])')"
      established=1
      break
    fi
    sleep 0.1
  done

  if [ "$established" = 1 ]; then
    printf '%s,%s,1,\n' "$cycle" "$elapsed" >>"$RESULTS"
    log "cycle $cycle: reconnected in ${elapsed} ms"
  else
    printf '%s,,0,no established event\n' "$cycle" >>"$RESULTS"
    log "cycle $cycle: the client did not establish a session"
  fi

  # The client is killed, so the host sees the session go away.
  kill -9 "$CLIENT_PID" 2>/dev/null || true
  wait "$CLIENT_PID" 2>/dev/null || true
  CLIENT_PID=""
  # Give the host a moment to notice and to pause the session.
  sleep 0.3
  cycle=$((cycle + 1))
done

# ----------------------------------------------------------------- report
log "merging the cycles into $OUT"
python3 - "$RESULTS" "$OUT_ABS" "$LIMIT_MS" "$CYCLES" <<'PY'
import csv
import json
import sys

results_path, out_path, limit_ms, cycles = sys.argv[1:5]
limit = int(limit_ms)
rows = []
with open(results_path, encoding="utf-8") as handle:
    for row in csv.DictReader(handle):
        rows.append(row)

samples = []
reconnected = 0
worst = 0
for row in rows:
    raw = row["reconnect_ms"]
    ok = row["established"] == "1" and raw != ""
    elapsed = int(raw) if raw else None
    if ok:
        reconnected += 1
        worst = max(worst, elapsed)
    samples.append({
        "cycle": int(row["cycle"]),
        "reconnect_ms": elapsed,
        "established": bool(ok),
    })

report = {
    "cycles": int(cycles),
    "reconnected": reconnected,
    "failed": int(cycles) - reconnected,
    "max_reconnect_ms": worst if reconnected else None,
    "limit_ms": limit,
    "pass": reconnected == int(cycles) and worst <= limit,
    "samples": samples,
}
with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write("\n")
print(
    f"[run-reconnect] {reconnected}/{cycles} cycles reconnected, "
    f"the slowest took {worst} ms, the limit is {limit} ms"
)
PY

log "the report is in $OUT"
