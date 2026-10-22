#!/usr/bin/env bash
# The session survives a network flap.
#
# Plan milestone M6.6: "Network flap resilience: session survives interface
# down/up, client reconnects <= 2 s".
#
# The script runs the host and the client inside the network namespaces of
# scripts/netns-up.sh, takes the veth pair down and up again, and measures
# how long the client needs to be back.
#
# The client runs with --reconnect, so it comes back on its own with the
# backoff of the plan: 200 ms to 2 s.
#
# The artifact holds one row per flap and the summary: every flap recovered
# and the reconnect took at most 2000 ms.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

FLAPS="${FLAPS:-3}"
OUT="${OUT:-artifacts/m6/wifi-flap.json}"
LIMIT_MS="${LIMIT_MS:-2000}"
DURATION="${DURATION:-30}"
SIZE="${SIZE:-1280x720}"
FPS="${FPS:-30}"
CONTROL_PORT="${CONTROL_PORT:-24850}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24851}"
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"
VETH_HOST="${VETH_HOST:-lumen-h}"
VETH_CLIENT="${VETH_CLIENT:-lumen-c}"
HOST_ADDR="${HOST_ADDR:-10.77.0.1}"
CLIENT_ADDR="${CLIENT_ADDR:-10.77.0.2}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-wifi-flap}"
# Keep the runtime files, so a failing run can be looked at.
KEEP="${KEEP:-0}"

log() { printf '[run-wifi-flap] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: scripts/run-wifi-flap.sh [options]

Runs the network flap acceptance of plan milestone M6.6: the link between
the host and the client goes down and up again, and the client has to be
back within the limit.

Options:
  --flaps <n>        How many flaps to run (default 3)
  --out <path>       Where the report goes (default artifacts/m6/wifi-flap.json)
  --limit-ms <n>     The reconnect limit in milliseconds (default 2000)
  --duration <s>     How long one phase of a flap lasts (default 30)
  --size <WxH>       Capture size (default 1280x720)
  --fps <n>          Stream rate (default 30)
  --keep             Keep the runtime files, so a failing run can be read
  -h, --help         Print this text

Environment:
  FLAPS, OUT, LIMIT_MS, DURATION, SIZE, FPS, CONTROL_PORT, CLIENT_MEDIA_PORT,
  HOST_NS, CLIENT_NS, VETH_HOST, VETH_CLIENT, HOST_ADDR, CLIENT_ADDR,
  RUNTIME_ROOT
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --flaps) FLAPS="${2:?--flaps needs a value}"; shift 2 ;;
    --out) OUT="${2:?--out needs a value}"; shift 2 ;;
    --limit-ms) LIMIT_MS="${2:?--limit-ms needs a value}"; shift 2 ;;
    --duration) DURATION="${2:?--duration needs a value}"; shift 2 ;;
    --size) SIZE="${2:?--size needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
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

case "$FLAPS" in
  ''|*[!0-9]*) log "FLAPS must be a number, got $FLAPS"; exit 2 ;;
esac
if [ "$FLAPS" -lt 1 ]; then
  log "there must be at least one flap"
  exit 2
fi
command -v python3 >/dev/null 2>&1 || { log "python3 is needed to read the events"; exit 1; }
command -v sudo >/dev/null 2>&1 || { log "sudo is needed for the network namespaces"; exit 1; }
sudo -n true 2>/dev/null || { log "sudo -n does not work here"; exit 1; }

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
  if [ "$KEEP" = 1 ]; then
    log "the runtime files stay in $RUNTIME_ROOT"
  else
    rm -rf "$RUNTIME_ROOT"
  fi
  HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" "$REPO_ROOT/scripts/netns-down.sh" \
    >/dev/null 2>&1 || true
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

# ------------------------------------------------------------- namespaces
log "setting up the network namespaces"
HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" VETH_HOST="$VETH_HOST" VETH_CLIENT="$VETH_CLIENT" \
  HOST_IP="$HOST_ADDR/24" CLIENT_IP="$CLIENT_ADDR/24" \
  "$REPO_ROOT/scripts/netns-up.sh"
if ! sudo -n ip netns exec "$HOST_NS" true 2>/dev/null; then
  log "the namespace $HOST_NS is not usable"
  exit 1
fi

# ------------------------------------------------------------------- run
mkdir -p "$STATE_DIR"
rm -f "$EVENTS_FILE" "$STATE_DIR/pairing-code.txt"

log "starting the host in the namespace $HOST_NS"
sudo -n ip netns exec "$HOST_NS" env XDG_RUNTIME_DIR="$CLIENT_RT" \
  WAYLAND_DISPLAY=wayland-1 \
  "$HOST_BIN" --size "$SIZE" --fps "$FPS" \
    --control "$HOST_ADDR:$CONTROL_PORT" --client-media-port "$CLIENT_MEDIA_PORT" \
    --duration "$((FLAPS * 40 + 120))" --persist-timeout 30 \
    --report "$RUNTIME_ROOT/host.json" \
    --pairing-store "$STATE_DIR/pairing.json" \
    --pairing-code-file "$STATE_DIR/pairing-code.txt" \
    >"$RUNTIME_ROOT/host.log" 2>&1 &
HOST_PID=$!
sleep 1

log "starting the client in the namespace $CLIENT_NS"
sudo -n ip netns exec "$CLIENT_NS" env XDG_RUNTIME_DIR="$CLIENT_RT" \
  WAYLAND_DISPLAY=wayland-1 \
  "$CLIENT_BIN" --host "$HOST_ADDR:$CONTROL_PORT" \
    --media-bind "$CLIENT_ADDR:$CLIENT_MEDIA_PORT" \
    --duration "$((FLAPS * 40 + 110))" --report "$RUNTIME_ROOT/client.json" \
    --identity "$STATE_DIR/client-identity" \
    --pairing-code-file "$STATE_DIR/pairing-code.txt" \
    --reconnect --events "$EVENTS_FILE" \
    >"$RUNTIME_ROOT/client.log" 2>&1 &
CLIENT_PID=$!

# The number of sessions the client has established so far.
sessions() {
  python3 - "$EVENTS_FILE" <<'PY'
import json
import sys

count = 0
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
            if event.get("event") == "established":
                count = max(count, int(event.get("sessions", 0)))
except FileNotFoundError:
    pass
print(count)
PY
}

# ----------------------------------------------------------------- flaps
RESULTS="$RUNTIME_ROOT/results.csv"
printf 'flap,recovered,reconnect_ms,sessions_before,sessions_after\n' >"$RESULTS"

flap=1
while [ "$flap" -le "$FLAPS" ]; do
  before="$(sessions)"
  log "flap $flap: taking the link down"
  sudo -n ip netns exec "$HOST_NS" ip link set "$VETH_HOST" down
  sleep 2

  log "flap $flap: bringing the link up again"
  UP_EPOCH_MS="$(date +%s%3N)"
  sudo -n ip netns exec "$HOST_NS" ip link set "$VETH_HOST" up

  # The client comes back on its own; the watch is on the event log.
  recovered=0
  reconnect_ms=""
  waited=0
  while [ "$waited" -lt 300 ]; do
    after="$(sessions)"
    if [ "$after" -gt "$before" ]; then
      recovered=1
      now_ms="$(date +%s%3N)"
      reconnect_ms=$((now_ms - UP_EPOCH_MS))
      break
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  after="$(sessions)"
  printf '%s,%s,%s,%s,%s\n' "$flap" "$recovered" "$reconnect_ms" "$before" "$after" >>"$RESULTS"
  if [ "$recovered" = 1 ]; then
    log "flap $flap: the client was back after ${reconnect_ms} ms"
  else
    log "flap $flap: the client did not come back"
  fi
  # Let the stream run again before the next flap.
  sleep 3
  flap=$((flap + 1))
done

# ----------------------------------------------------------------- report
log "merging the flaps into $OUT"
python3 - "$RESULTS" "$OUT_ABS" "$FLAPS" "$LIMIT_MS" <<'PY'
import csv
import json
import sys

results_path, out_path, flaps, limit_ms = sys.argv[1:5]
limit = int(limit_ms)
rows = []
with open(results_path, encoding="utf-8") as handle:
    for row in csv.DictReader(handle):
        rows.append(row)

samples = []
recovered = 0
worst = 0
for row in rows:
    ok = row["recovered"] == "1" and row["reconnect_ms"] != ""
    elapsed = int(row["reconnect_ms"]) if row["reconnect_ms"] else None
    if ok:
        recovered += 1
        worst = max(worst, elapsed)
    samples.append({
        "flap": int(row["flap"]),
        "recovered": bool(ok),
        "reconnect_ms": elapsed,
        "sessions_before": int(row["sessions_before"]),
        "sessions_after": int(row["sessions_after"]),
    })

report = {
    "flaps": int(flaps),
    "recovered": recovered,
    "failed": int(flaps) - recovered,
    "max_reconnect_ms": worst if recovered else None,
    "limit_ms": limit,
    "pass": recovered == int(flaps) and worst <= limit,
    "samples": samples,
}
with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write("\n")
print(
    f"[run-wifi-flap] {recovered}/{flaps} flaps recovered, "
    f"the slowest reconnect took {worst} ms, the limit is {limit} ms"
)
PY

log "the report is in $OUT"
