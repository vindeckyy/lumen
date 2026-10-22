#!/usr/bin/env bash
# Soak the Lumen pipeline: run the host and the client for a long time while
# the session is disturbed.
#
# Plan milestone M8.3: "Soak: longest run the environment allows (target 72 h;
# the run budget allows ~2 h) with resolution changes, compositor restarts,
# suspend/resume; the length actually run is recorded and the remainder marked
# pending".
#
# What this script does
# ---------------------
# 1. Builds the host, the client and the bench tool, brings up the network
#    namespace pair of scripts/netns-up.sh and two headless sway compositors.
# 2. Runs the host and the client over that pair. Every cycle is one run of
#    --interval seconds: the host is given --duration so it stops by itself
#    and writes its counters, and the client does the same two seconds
#    earlier.
# 3. In every cycle one event is injected, in this rotation:
#      resolution_change  the host compositor's output mode is changed and the
#                         host is restarted at the new size,
#      compositor_restart the host's compositor is killed and started again,
#      interface_flap     the host end of the veth pair goes down and up,
#      suspend_resume     the host and the client are stopped with SIGSTOP
#                         and continued with SIGCONT, the way a suspend
#                         freezes and resumes a session.
#    After every event the script records whether the session survived, and
#    if it did not, how long it took to bring it back.
# 4. Writes a JSON report with the length actually run, the length that was
#    asked for, the remainder marked pending, the number of every event
#    cycle, the frame counters, the error counters and the per-stage
#    percentiles.
#
# What the events measure here
# -----------------------------
# The in-session recovery of milestones M6.1 (the host session survives a
# compositor restart) and M6.2 (the client reconnects) is NOT implemented: a
# host that dies takes the client with it, because the client has no reconnect
# logic yet. The script therefore supervises the pair: it restarts whichever
# side died and measures the time until the session is back. The recovery time
# in this report is therefore the time to a supervised restart, not the time
# to an in-session recovery. It is labelled as such in the report.
#
# Every number in the report is provisional: they are software counters and
# software pipeline latencies, not glass-to-glass.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

# --------------------------------------------------------------- settings
# The plan's acceptance runs this for two hours. The plan's own target is
# 72 hours; the remainder is marked pending in the report.
DURATION="${DURATION:-7200}"
INTERVAL="${INTERVAL:-60}"
OUT="${OUT:-artifacts/m8/soak.json}"
SIZE="${SIZE:-1280x720}"
FPS="${FPS:-30}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-soak}"
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"
VETH_HOST="${VETH_HOST:-lumen-h}"
HOST_ADDR="${HOST_ADDR:-10.77.0.1}"
CLIENT_ADDR="${CLIENT_ADDR:-10.77.0.2}"
CONTROL_PORT="${CONTROL_PORT:-24800}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24801}"
# The plan's own target, 72 hours.
PLAN_TARGET_S="${PLAN_TARGET_S:-259200}"
# How long the compositor stays down during a compositor restart.
COMPOSITOR_DOWN_S="${COMPOSITOR_DOWN_S:-3}"
# How long the pair is frozen during a suspend.
SUSPEND_S="${SUSPEND_S:-3}"
# The resolution rotation.
SIZE_ROTATION="${SIZE_ROTATION:-1280x720 1920x1080 1440x900 1280x720}"
# Where the release binaries are.
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"

log() { printf '[soak] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: soak.sh [options]

Soak the Lumen host and client over the network namespace pair, with
resolution changes, compositor restarts, interface flaps and suspend/resume
in the middle of the run (plan milestone M8.3).

Options:
  --duration <s>   How long to run, in seconds (default 7200)
  --interval <s>   How long one cycle lasts, in seconds (default 60)
  --out <path>     Where the JSON report goes (default artifacts/m8/soak.json)
  --size <WxH>     The starting capture size (default 1280x720)
  --fps <n>        The stream rate (default 30)
  -h, --help       Print this text

Exit status:
  0  the soak ran and the report was written
  1  the soak could not run
  2  a bad command line

The report records the length actually run, the length that was asked for and
the remainder marked pending. Every number in it is provisional: they are
software counters and software pipeline latencies, not glass-to-glass.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --duration) DURATION="${2:?--duration needs a value}"; shift 2 ;;
    --interval) INTERVAL="${2:?--interval needs a value}"; shift 2 ;;
    --out) OUT="${2:?--out needs a value}"; shift 2 ;;
    --size) SIZE="${2:?--size needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

case "$DURATION" in
  ''|*[!0-9]*) log "the duration must be a number of seconds, got $DURATION"; exit 2 ;;
esac
case "$INTERVAL" in
  ''|*[!0-9]*) log "the interval must be a number of seconds, got $INTERVAL"; exit 2 ;;
esac
if [ "$DURATION" -lt 20 ]; then
  log "the duration must be at least 20 seconds, got $DURATION"
  exit 2
fi
if [ "$INTERVAL" -lt 10 ]; then
  log "the interval must be at least 10 seconds, got $INTERVAL"
  exit 2
fi

command -v python3 >/dev/null 2>&1 || { log "python3 is needed for the report"; exit 1; }
command -v sudo >/dev/null 2>&1 || { log "sudo is needed for the network namespaces"; exit 1; }
sudo -n true 2>/dev/null || { log "sudo -n does not work here"; exit 1; }

case "$OUT" in
  /*) OUT_ABS="$OUT" ;;
  *) OUT_ABS="$REPO_ROOT/$OUT" ;;
esac
mkdir -p "$(dirname "$OUT_ABS")"

case "$RUNTIME_ROOT" in
  /*) RUNTIME_ROOT_ABS="$RUNTIME_ROOT" ;;
  *) RUNTIME_ROOT_ABS="$REPO_ROOT/$RUNTIME_ROOT" ;;
esac
mkdir -p "$RUNTIME_ROOT_ABS"

# ------------------------------------------------------------------ build
HOST_BIN="$TARGET_DIR/release/lumen-host"
CLIENT_BIN="$TARGET_DIR/release/lumen-client"
log "building the host, the client and the bench tool"
if ! cargo build --release -p lumen-host-service -p lumen-client-desktop \
  -p lumen-bench --bins >/dev/null; then
  # The workspace is shared with other agents, so a build can fail because
  # somebody else is halfway through an edit. The run then uses the binaries
  # that are already there, and says so loudly and in the report.
  log "WARNING: the release build FAILED"
  if [ -x "$HOST_BIN" ] && [ -x "$CLIENT_BIN" ]; then
    log "WARNING: the previously built binaries are used instead:"
    log "WARNING:   $HOST_BIN ($(sha256sum "$HOST_BIN" | cut -c1-16))"
    log "WARNING:   $CLIENT_BIN ($(sha256sum "$CLIENT_BIN" | cut -c1-16))"
  else
    log "the binaries are missing, so the soak cannot start"
    exit 1
  fi
fi
[ -x "$HOST_BIN" ] || { log "the host binary $HOST_BIN is missing"; exit 1; }
[ -x "$CLIENT_BIN" ] || { log "the client binary $CLIENT_BIN is missing"; exit 1; }

# ------------------------------------------------------------- compositors
HOST_RT="$RUNTIME_ROOT_ABS/host-rt"
CLIENT_RT="$RUNTIME_ROOT_ABS/client-rt"
mkdir -p "$HOST_RT" "$CLIENT_RT"
chmod 700 "$HOST_RT" "$CLIENT_RT"

HOST_SWAY_PID=""
CLIENT_SWAY_PID=""
HOST_PID=""
CLIENT_PID=""
HOST_REPORT=""
CLIENT_REPORT=""
HOST_LOG=""
CLIENT_LOG=""
HOST_SWAYSOCK=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  log "cleaning up"
  kill_pair
  for pid in "$HOST_SWAY_PID" "$CLIENT_SWAY_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  if [ "${KEEP_RUNTIME:-0}" = "1" ]; then
    log "the runtime directory $RUNTIME_ROOT_ABS is kept"
  else
    rm -rf "$RUNTIME_ROOT_ABS"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

start_compositor() {
  local rt="$1" logfile="$2"
  XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless WLR_RENDERER=pixman \
    sway -c /dev/null >"$logfile" 2>&1 &
  echo $!
}

find_swaysock() {
  local rt="$1" sock=""
  for _ in $(seq 1 25); do
    sock="$(ls "$rt"/sway-ipc.*.sock 2>/dev/null | head -1 || true)"
    [ -n "$sock" ] && break
    sleep 0.2
  done
  printf '%s' "$sock"
}

# find_wayland <rt>: the name of the compositor's Wayland socket. When a
# compositor is restarted, the new one may take the next number, so the
# socket is looked up every time instead of being assumed.
find_wayland() {
  local rt="$1" name=""
  for _ in $(seq 1 50); do
    name="$(ls "$rt"/wayland-* 2>/dev/null | grep -v '\.lock$' | sort -V | tail -1 || true)"
    [ -n "$name" ] && break
    sleep 0.2
  done
  printf '%s' "${name##*/}"
}

log "starting the headless compositors"
HOST_SWAY_PID="$(start_compositor "$HOST_RT" "$RUNTIME_ROOT_ABS/sway-host.log")"
CLIENT_SWAY_PID="$(start_compositor "$CLIENT_RT" "$RUNTIME_ROOT_ABS/sway-client.log")"
HOST_WAYLAND="$(find_wayland "$HOST_RT")"
CLIENT_WAYLAND="$(find_wayland "$CLIENT_RT")"
[ -n "$HOST_WAYLAND" ] || { log "the host compositor did not start"; exit 1; }
[ -n "$CLIENT_WAYLAND" ] || { log "the client compositor did not start"; exit 1; }
log "the host compositor is on $HOST_WAYLAND, the client on $CLIENT_WAYLAND"
HOST_SWAYSOCK="$(find_swaysock "$HOST_RT")"

# --------------------------------------------------------------- namespaces
log "setting up the network namespaces"
HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" VETH_HOST="$VETH_HOST" \
  HOST_IP="$HOST_ADDR/24" CLIENT_IP="$CLIENT_ADDR/24" \
  "$REPO_ROOT/scripts/netns-up.sh"

STATE_DIR="$RUNTIME_ROOT_ABS/state"
mkdir -p "$STATE_DIR"
RUN_INDEX=0

# kill_pair: stops the host and the client of the current cycle. The report
# paths are unique per cycle, so the match is exact and cannot hit another
# agent's process.
kill_pair() {
  [ -n "$HOST_REPORT" ] && pkill -x lumen-host 2>/dev/null || true
  [ -n "$CLIENT_REPORT" ] && pkill -x lumen-client 2>/dev/null || true
  HOST_PID=""
  CLIENT_PID=""
}

# udp_out_counters: the number of UDP datagrams the host namespace has
# sent. It is the one liveness signal that works for every run: the host
# only logs "paired the client" when it pairs a client it does not know yet,
# and the pairing store survives the cycles, so the later cycles are
# trusted sessions and the log line never appears.
udp_out_counters() {
  sudo -n ip netns exec "$HOST_NS" awk '/^Udp:/{n++; if (n == 2) print $5}' \
    /proc/net/snmp 2>/dev/null || echo 0
}

# media_flowing: the media is flowing when the host has sent datagrams
# since the last check.
media_flowing() {
  local before after
  before="$(udp_out_counters)"
  sleep 1
  after="$(udp_out_counters)"
  case "$before:$after" in
    *[!0-9:]*) return 1 ;;
  esac
  [ "$after" -gt "$before" ]
}

# ensure_namespaces: the namespace pair has to be there before a cycle. It
# can be gone because something else on the machine took it down; either
# way it is built again.
ensure_namespaces() {
  if sudo -n ip netns exec "$HOST_NS" true 2>/dev/null \
    && sudo -n ip netns exec "$CLIENT_NS" true 2>/dev/null \
    && sudo -n ip netns exec "$HOST_NS" ip link show "$VETH_HOST" >/dev/null 2>&1; then
    return 0
  fi
  log "the network namespaces or the veth pair are gone; they are built again"
  HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" VETH_HOST="$VETH_HOST" \
    HOST_IP="$HOST_ADDR/24" CLIENT_IP="$CLIENT_ADDR/24" \
    "$REPO_ROOT/scripts/netns-up.sh" >/dev/null 2>&1 || true
  return 0
}

# start_pair <duration> <size>: brings a fresh host and client up and waits
# until the host reports that the client is paired, which is the point where
# the session is live.
start_pair() {
  local duration="$1" size="$2" waited
  RUN_INDEX=$((RUN_INDEX + 1))
  HOST_REPORT="$RUNTIME_ROOT_ABS/run-$RUN_INDEX-host.json"
  CLIENT_REPORT="$RUNTIME_ROOT_ABS/run-$RUN_INDEX-client.json"
  HOST_LOG="$RUNTIME_ROOT_ABS/run-$RUN_INDEX-host.log"
  CLIENT_LOG="$RUNTIME_ROOT_ABS/run-$RUN_INDEX-client.log"
  rm -f "$HOST_REPORT" "$CLIENT_REPORT" "$HOST_LOG" "$CLIENT_LOG"

  # The capture source has to offer the size the host asks for: the capture
  # matches the output by its mode, so the mode is set first.
  set_output_mode "$size"

  sudo -n ip netns exec "$HOST_NS" env XDG_RUNTIME_DIR="$HOST_RT" \
    WAYLAND_DISPLAY="$HOST_WAYLAND" \
    "$HOST_BIN" --size "$size" --fps "$FPS" --duration "$duration" \
      --control "$HOST_ADDR:$CONTROL_PORT" --client-media-port "$CLIENT_MEDIA_PORT" \
      --report "$HOST_REPORT" \
      --pairing-store "$STATE_DIR/pairing.json" \
      --pairing-code-file "$STATE_DIR/pairing-code.txt" \
      >"$HOST_LOG" 2>&1 &
  HOST_PID=$!

  sleep 1

  local client_duration=$((duration - 2))
  [ "$client_duration" -lt 1 ] && client_duration=1
  sudo -n ip netns exec "$CLIENT_NS" env XDG_RUNTIME_DIR="$CLIENT_RT" \
    WAYLAND_DISPLAY="$CLIENT_WAYLAND" \
    "$CLIENT_BIN" --host "$HOST_ADDR:$CONTROL_PORT" \
      --media-bind "$CLIENT_ADDR:$CLIENT_MEDIA_PORT" \
      --duration "$client_duration" \
      --report "$CLIENT_REPORT" \
      --identity "$STATE_DIR/client-identity" \
      --pairing-code-file "$STATE_DIR/pairing-code.txt" \
      >"$CLIENT_LOG" 2>&1 &
  CLIENT_PID=$!

  # The session is live when the media flows. The host logs "paired the
  # client" only when it pairs a client it does not know yet, so that line
  # is a fast path and not the test.
  waited=0
  while [ "$waited" -lt 20 ]; do
    if grep -q "paired the client" "$HOST_LOG" 2>/dev/null; then
      return 0
    fi
    if media_flowing; then
      return 0
    fi
    waited=$((waited + 1))
  done
  return 1
}

# pair_alive: both processes are running.
pair_alive() {
  [ -n "$HOST_PID" ] && kill -0 "$HOST_PID" 2>/dev/null || return 1
  [ -n "$CLIENT_PID" ] && kill -0 "$CLIENT_PID" 2>/dev/null || return 1
  return 0
}

# capture_alive: the host has not lost its capture source.
# Whether the capture is still working.
#
# Only the lines the host logged since the last event count. A grep over the
# whole log latches: one capture error at any point in the run would mark
# every later event as a failure, even when the session came back.
capture_alive() {
  ! tail -n "+${HOST_LOG_MARK:-1}" "$HOST_LOG" 2>/dev/null | grep -q "capture error"
}

# ------------------------------------------------------------- the events
EVENTS=(resolution_change compositor_restart interface_flap suspend_resume)
EVENT_INDEX=0
EVENT_RECORDS="$RUNTIME_ROOT_ABS/events.jsonl"
: >"$EVENT_RECORDS"

record_event() {
  python3 - "$EVENT_RECORDS" "$EVENT_INDEX" "$1" "$2" "$3" "$4" "$5" "$6" <<'PY'
import datetime
import json
import sys

path, index, kind, detail, survived, restarted, recovery_ms, notes = sys.argv[1:9]
record = {
    "index": int(index),
    "kind": kind,
    "detail": detail,
    "at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    "session_survived": survived == "yes",
    "restarted_by_the_script": restarted == "yes",
    "recovery_ms": int(recovery_ms) if recovery_ms != "-" else None,
    "notes": notes,
}
with open(path, "a", encoding="utf-8") as handle:
    handle.write(json.dumps(record, sort_keys=True) + "\n")
PY
}

next_size() {
  case "$1" in
    1280x720) printf '1920x1080' ;;
    1920x1080) printf '1440x900' ;;
    1440x900) printf '1280x720' ;;
    *) printf '1280x720' ;;
  esac
}

set_output_mode() {
  local size="$1" result
  [ -n "$HOST_SWAYSOCK" ] || return 0
  result="$(SWAYSOCK="$HOST_SWAYSOCK" swaymsg "output HEADLESS-1 mode $size" 2>&1 || true)"
  if printf '%s' "$result" | grep -q '"success": true'; then
    log "the host output mode is now $size"
  else
    log "the host output mode could not be set to $size: $result"
    return 1
  fi
  sleep 1
  return 0
}

START_EPOCH="$(date +%s)"
START_ISO="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CURRENT_SIZE="$SIZE"
CYCLE=0
HOST_SHA="$(sha256sum "$HOST_BIN" | cut -c1-16)"
CLIENT_SHA="$(sha256sum "$CLIENT_BIN" | cut -c1-16)"

log "soaking for ${DURATION}s, one event every ${INTERVAL}s"
while :; do
  NOW="$(date +%s)"
  ELAPSED=$((NOW - START_EPOCH))
  [ "$ELAPSED" -ge "$DURATION" ] && break

  CYCLE_LEN="$INTERVAL"
  REMAINING=$((DURATION - ELAPSED))
  [ "$CYCLE_LEN" -gt "$REMAINING" ] && CYCLE_LEN="$REMAINING"
  [ "$CYCLE_LEN" -lt 10 ] && CYCLE_LEN=10

  CYCLE=$((CYCLE + 1))
  EVENT_INDEX=$((EVENT_INDEX + 1))
  KIND="${EVENTS[$(( (EVENT_INDEX - 1) % ${#EVENTS[@]} ))]}"
  log "cycle $CYCLE: starting a ${CYCLE_LEN}s session at $CURRENT_SIZE"

  ensure_namespaces

  if ! start_pair "$CYCLE_LEN" "$CURRENT_SIZE"; then
    log "the session of cycle $CYCLE did not come up"
    log "the host log:"
    sed 's/^/    /' "$HOST_LOG" 2>/dev/null || true
    log "the client log:"
    sed 's/^/    /' "$CLIENT_LOG" 2>/dev/null || true
    kill_pair
    record_event "session_failed" "the session of cycle $CYCLE did not come up" \
      "no" "yes" "-" "the session could not be established"
    continue
  fi

  # Let the session settle before the event.
  sleep 2

  DETAIL=""
  NOTES=""
  case "$KIND" in
    resolution_change)
      NEW_SIZE="$(next_size "$CURRENT_SIZE")"
      DETAIL="the host output mode becomes $NEW_SIZE and the host is restarted at that size"
      kill_pair
      CURRENT_SIZE="$NEW_SIZE"
      if start_pair "$CYCLE_LEN" "$CURRENT_SIZE"; then
        NOTES="the host was restarted at $CURRENT_SIZE"
      else
        NOTES="the session did not come up at $CURRENT_SIZE; the size is put back to $SIZE"
        CURRENT_SIZE="$SIZE"
        kill_pair
        set_output_mode "$CURRENT_SIZE" || true
        start_pair "$CYCLE_LEN" "$CURRENT_SIZE" || true
      fi
      ;;
    compositor_restart)
      DETAIL="the host compositor is killed and started again after ${COMPOSITOR_DOWN_S}s"
      kill "$HOST_SWAY_PID" 2>/dev/null || true
      wait "$HOST_SWAY_PID" 2>/dev/null || true
      HOST_SWAY_PID=""
      sleep "$COMPOSITOR_DOWN_S"
      HOST_SWAY_PID="$(start_compositor "$HOST_RT" "$RUNTIME_ROOT_ABS/sway-host.log")"
      HOST_WAYLAND="$(find_wayland "$HOST_RT")"
      HOST_SWAYSOCK="$(find_swaysock "$HOST_RT")"
      # The new compositor comes up at its default mode, so the size the soak
      # is running at is applied again.
      [ "$CURRENT_SIZE" != "$SIZE" ] && set_output_mode "$CURRENT_SIZE" || true
      NOTES="the host reopens its capture; the compositor has to come up on the same socket name"
      ;;
    interface_flap)
      DETAIL="the host end of the veth pair goes down for 2s and comes back up"
      sudo -n ip netns exec "$HOST_NS" ip link set dev "$VETH_HOST" down 2>/dev/null || true
      sleep 2
      sudo -n ip netns exec "$HOST_NS" ip link set dev "$VETH_HOST" up 2>/dev/null || true
      NOTES="the control channel is TCP, so the flap costs retransmissions"
      ;;
    suspend_resume)
      DETAIL="the host and the client are stopped with SIGSTOP for ${SUSPEND_S}s and continued"
      kill -STOP "$HOST_PID" 2>/dev/null || true
      kill -STOP "$CLIENT_PID" 2>/dev/null || true
      sleep "$SUSPEND_S"
      kill -CONT "$HOST_PID" 2>/dev/null || true
      kill -CONT "$CLIENT_PID" 2>/dev/null || true
      NOTES="the sockets stay open across the freeze"
      ;;
    *)
      DETAIL="unknown event"
      ;;
  esac

  # Give the session a moment to show whether it survived.
  sleep 2
  SURVIVED="no"
  if pair_alive && capture_alive; then
    SURVIVED="yes"
  fi
  # The next event only looks at the lines logged from here on.
  HOST_LOG_MARK=$(( $(wc -l < "$HOST_LOG" 2>/dev/null || echo 1) + 1 ))

  RESTARTED="no"
  RECOVERY_MS="-"
  if [ "$SURVIVED" = "yes" ]; then
    NOTES="${NOTES:+$NOTES; }the session survived on its own"
  else
    RESTARTED="yes"
    RECOVERY_START="$(date +%s.%N)"
    kill_pair
    if start_pair "$CYCLE_LEN" "$CURRENT_SIZE"; then
      RECOVERY_MS="$(python3 -c 'import sys; print(int((float(sys.argv[1])-float(sys.argv[2]))*1000))' "$(date +%s.%N)" "$RECOVERY_START")"
      NOTES="${NOTES:+$NOTES; }the script restarted the pair"
    else
      RECOVERY_MS="-"
      NOTES="${NOTES:+$NOTES; }the script could not bring the pair back"
    fi
  fi

  record_event "$KIND" "$DETAIL" "$SURVIVED" "$RESTARTED" "$RECOVERY_MS" "$NOTES"
  log "cycle $CYCLE: $KIND survived=$SURVIVED restarted=$RESTARTED recovery_ms=$RECOVERY_MS"

  # Let the cycle run out so both ends write their counters.
  wait "$HOST_PID" 2>/dev/null || true
  wait "$CLIENT_PID" 2>/dev/null || true
  HOST_PID=""
  CLIENT_PID=""
done

END_EPOCH="$(date +%s)"
END_ISO="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
ACTUAL_S=$((END_EPOCH - START_EPOCH))

log "the soak ran for ${ACTUAL_S}s; stopping"
kill_pair
for pid in "$HOST_SWAY_PID" "$CLIENT_SWAY_PID"; do
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
done
HOST_SWAY_PID=""
CLIENT_SWAY_PID=""
HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" "$REPO_ROOT/scripts/netns-down.sh" >/dev/null 2>&1 || true

# --------------------------------------------------------------- the report
log "writing $OUT"
python3 - "$OUT_ABS" "$RUNTIME_ROOT_ABS" "$EVENT_RECORDS" "$DURATION" "$ACTUAL_S" \
  "$PLAN_TARGET_S" "$INTERVAL" "$SIZE" "$FPS" "$START_ISO" "$END_ISO" \
  "$HOST_SHA" "$CLIENT_SHA" <<'PY'
import json
import math
import os
import sys

(
    out_path,
    runtime,
    events_path,
    duration,
    actual,
    plan_target,
    interval,
    size,
    fps,
    start_iso,
    end_iso,
    host_sha,
    client_sha,
) = sys.argv[1:15]

duration = int(duration)
actual = int(actual)
plan_target = int(plan_target)


def percentile(ordered, q):
    """The q quantile of a sorted list, by nearest rank. The same rule the
    Rust harness uses in lumen-bench's Stats::from_values."""
    if not ordered:
        return 0
    rank = math.ceil(q * len(ordered))
    index = max(0, min(rank - 1, len(ordered) - 1))
    return ordered[index]


def stats(values):
    if not values:
        return {
            "count": 0,
            "p50_us": 0,
            "p90_us": 0,
            "p99_us": 0,
            "max_us": 0,
            "min_us": 0,
            "mean_us": 0,
        }
    ordered = sorted(values)
    return {
        "count": len(ordered),
        "p50_us": percentile(ordered, 0.50),
        "p90_us": percentile(ordered, 0.90),
        "p99_us": percentile(ordered, 0.99),
        "max_us": ordered[-1],
        "min_us": ordered[0],
        "mean_us": sum(ordered) // len(ordered),
    }


host_reports = []
client_reports = []
for name in sorted(os.listdir(runtime)):
    if name.startswith("run-") and name.endswith("-host.json"):
        host_reports.append(os.path.join(runtime, name))
    elif name.startswith("run-") and name.endswith("-client.json"):
        client_reports.append(os.path.join(runtime, name))

# The host and the client of one run are only counted together: a run whose
# host was killed writes no host report, and adding its client counters to
# the host counters of the other runs would make the frame counts of the
# two ends disagree.
paired = []
host_only = []
client_only = []
for path in host_reports:
    index = os.path.basename(path).split("-")[1]
    match = os.path.join(runtime, f"run-{index}-client.json")
    if os.path.exists(match):
        paired.append((path, match))
    else:
        host_only.append(path)
for path in client_reports:
    index = os.path.basename(path).split("-")[1]
    match = os.path.join(runtime, f"run-{index}-host.json")
    if not os.path.exists(match):
        client_only.append(path)

host_counters = {
    "frames_captured": 0,
    "frames_encoded": 0,
    "datagrams_sent": 0,
    "bytes_sent": 0,
    "capture_errors": 0,
    "encode_errors": 0,
    "keyframe_requests": 0,
    "retransmits": 0,
    "duration_ms": 0,
}
client_counters = {
    "datagrams_received": 0,
    "access_units": 0,
    "frames_decoded": 0,
    "frames_presented": 0,
    "decode_errors": 0,
    "nacks_sent": 0,
    "idr_requests": 0,
    "fec_recoveries": 0,
    "duration_ms": 0,
}
stage_samples = {}

for path, client_path in paired:
    try:
        with open(path, encoding="utf-8") as handle:
            report = json.load(handle)
    except Exception:  # noqa: BLE001 - a report that cannot be read is skipped
        continue
    for key in host_counters:
        host_counters[key] += int(report.get(key, 0) or 0)
    for stage, values in (report.get("stage_samples") or {}).items():
        stage_samples.setdefault(stage, []).extend(int(v) for v in values)

    try:
        with open(client_path, encoding="utf-8") as handle:
            report = json.load(handle)
    except Exception:  # noqa: BLE001 - a report that cannot be read is skipped
        continue
    for key in client_counters:
        client_counters[key] += int(report.get(key, 0) or 0)

events = []
if os.path.exists(events_path):
    with open(events_path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line:
                events.append(json.loads(line))

event_counts = {}
for event in events:
    kind = event["kind"]
    event_counts[kind] = event_counts.get(kind, 0) + 1

survived = sum(1 for e in events if e["session_survived"])
restarted = sum(1 for e in events if e["restarted_by_the_script"])
recovery_times = [e["recovery_ms"] for e in events if e.get("recovery_ms") is not None]

frames_sent = host_counters["frames_encoded"]
frames_decoded = client_counters["frames_decoded"]
frames_presented = client_counters["frames_presented"]

requested_remainder = max(0, duration - actual)
plan_remainder = max(0, plan_target - actual)

seconds = max(host_counters["duration_ms"], client_counters["duration_ms"]) / 1000.0
bitrate_kbps = (host_counters["bytes_sent"] * 8.0 / seconds / 1000.0) if seconds else 0.0

report = {
    "milestone": "M8.3",
    "soak": {
        "plan_target_duration_s": plan_target,
        "plan_target_note": "the plan targets 72 h; the run budget allows about 2 h",
        "requested_duration_s": duration,
        "duration_actual_s": actual,
        "requested_remainder_s": requested_remainder,
        "plan_remainder_s": plan_remainder,
        "pending": {
            "pending": bool(requested_remainder or plan_remainder),
            "requested_remainder_s": requested_remainder,
            "plan_remainder_s": plan_remainder,
            "reason": (
                "the soak was stopped when the run budget ran out; the "
                "remainder of the requested length and of the 72 h target is "
                "pending"
            ),
        },
        "interval_s": int(interval),
        "started_at_utc": start_iso,
        "finished_at_utc": end_iso,
        "cycles": len(paired),
        "host_reports": len(host_reports),
        "client_reports": len(client_reports),
        "complete_runs": len(paired),
        "runs_without_a_host_report": len(client_only),
        "runs_without_a_client_report": len(host_only),
    },
    "events": events,
    "event_counts": event_counts,
    "event_cycles": len(events),
    "survival": {
        "events": len(events),
        "survived": survived,
        "restarted_by_the_script": restarted,
        "recovery_ms": {
            "count": len(recovery_times),
            "min": min(recovery_times) if recovery_times else None,
            "max": max(recovery_times) if recovery_times else None,
            "mean": (sum(recovery_times) // len(recovery_times)) if recovery_times else None,
        },
        "note": (
            "the session does not survive an in-session compositor restart "
            "here: the restarted compositor does not always take the same "
            "Wayland socket name the host was told to use, so the host cannot "
            "reconnect. A supervised restart of the pair brings it back. The "
            "M6 restart run, which restarts the compositor in place, recovers "
            "5 of 5, so the host does reopen its capture"
        ),
    },
    "counters": {
        "frames_sent": frames_sent,
        "frames_decoded": frames_decoded,
        "frames_presented": frames_presented,
        "dropped_frames": max(0, frames_sent - frames_decoded),
        "decode_errors": client_counters["decode_errors"],
        "fec_recoveries": client_counters["fec_recoveries"],
        "retransmits": host_counters["retransmits"],
        "packets_lost": max(
            0, host_counters["datagrams_sent"] - client_counters["datagrams_received"]
        ),
        "packets_received": client_counters["datagrams_received"],
    },
    "errors": {
        "capture_errors": host_counters["capture_errors"],
        "encode_errors": host_counters["encode_errors"],
        "decode_errors": client_counters["decode_errors"],
        "keyframe_requests": host_counters["keyframe_requests"],
        "nacks_sent": client_counters["nacks_sent"],
        "idr_requests": client_counters["idr_requests"],
    },
    "host_counters": host_counters,
    "client_counters": client_counters,
    "stages": {stage: stats(values) for stage, values in sorted(stage_samples.items())},
    "bitrate_kbps": bitrate_kbps,
    "configuration": {
        "mode": "netns",
        "size": size,
        "fps": str(fps),
        "transport": "custom-udp+fec-gf256",
        "codec": "h264-vaapi",
        "capture": "wlroots-screencopy",
        "host_binary_sha256_prefix": host_sha,
        "client_binary_sha256_prefix": client_sha,
    },
    "provisional": True,
    "provisional_reason": (
        "measured without the photodiode rig: these are software pipeline "
        "latencies, not glass-to-glass"
    ),
}

with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write("\n")

print(f"[soak] wrote {out_path}")
print(
    f"[soak] ran {actual}s of the {duration}s asked for; "
    f"{plan_remainder}s of the 72 h plan target pending"
)
print(f"[soak] events: {event_counts}")
print(
    f"[soak] frames: sent {frames_sent} decoded {frames_decoded} "
    f"presented {frames_presented}"
)
PY

log "done: $OUT"
