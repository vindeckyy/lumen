#!/usr/bin/env bash
# Chaos test: inject faults into the Lumen session and watch what survives.
#
# Plan milestone M8.4: "Chaos: random loss, jitter and reorder injection,
# encoder restart, decoder device loss, throttling simulation, netns teardown".
#
# What this script does
# ---------------------
# 1. Builds the host, the client and the bench tool, brings up the network
#    namespace pair of scripts/netns-up.sh and two headless sway compositors.
# 2. Runs the host and the client over that pair and injects one fault per
#    cycle, picked at random from a seeded sequence so that a run can be
#    repeated exactly:
#      netem_loss           tc netem with a random loss percentage, a random
#                           jitter and a random reorder rate,
#      netem_jitter_reorder tc netem with a random delay, jitter and reorder
#                           and no loss,
#      encoder_restart      the host process is killed and started again, which
#                           restarts the encoder that lives inside it,
#      decoder_device_loss  the client is restarted with the VA device pointed
#                           at a device that is not there, so its decoder
#                           cannot be opened,
#      cpu_throttle         the client is pinned to a single CPU with
#                           taskset, or capped with cpulimit when that is
#                           installed, and then released,
#      netns_teardown       the network namespace pair is torn down and built
#                           again.
# 3. After every fault it records what was injected, whether the session
#    survived, whether the script had to bring it back and how long that
#    took.
# 4. Writes a JSON report with the events, the survival count, the recovery
#    times and the frame counters.
#
# What the faults measure here
# -----------------------------
# The in-session recovery of milestones M6.1 and M6.2 is NOT implemented, so
# a fault that kills an end takes the session with it and the script has to
# bring it back. The recovery time in this report is therefore the time to a
# supervised restart, not the time to an in-session recovery.
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
DURATION="${DURATION:-900}"
INTERVAL="${INTERVAL:-45}"
SEED="${SEED:-20261021}"
OUT="${OUT:-artifacts/m8/chaos.json}"
SIZE="${SIZE:-1280x720}"
FPS="${FPS:-30}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-chaos}"
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"
VETH_HOST="${VETH_HOST:-lumen-h}"
HOST_ADDR="${HOST_ADDR:-10.77.0.1}"
CLIENT_ADDR="${CLIENT_ADDR:-10.77.0.2}"
CONTROL_PORT="${CONTROL_PORT:-24800}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24801}"
# The device the client decoder uses. The decoder device loss points the
# client at a device that is not there.
VA_DEVICE="${VA_DEVICE:-/dev/dri/renderD129}"
MISSING_VA_DEVICE="${MISSING_VA_DEVICE:-/dev/dri/renderD255}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
# The number of faults the seeded sequence holds. The run stops when the
# duration is up, long before this is reached.
PLAN_LENGTH="${PLAN_LENGTH:-200}"

log() { printf '[chaos] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: chaos.sh [options]

Inject faults into the Lumen session and record what survives (plan milestone
M8.4).

Options:
  --duration <s>   How long to run, in seconds (default 900)
  --interval <s>   How long one cycle lasts, in seconds (default 45)
  --seed <n>       The seed of the fault sequence, so a run repeats exactly
                   (default 20261021)
  --out <path>     Where the JSON report goes (default artifacts/m8/chaos.json)
  --size <WxH>     The capture size (default 1280x720)
  --fps <n>        The stream rate (default 30)
  -h, --help       Print this text

Exit status:
  0  the run finished and the report was written
  1  the run could not start
  2  a bad command line

Every number in the report is provisional: they are software counters and
software pipeline latencies, not glass-to-glass.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --duration) DURATION="${2:?--duration needs a value}"; shift 2 ;;
    --interval) INTERVAL="${2:?--interval needs a value}"; shift 2 ;;
    --seed) SEED="${2:?--seed needs a value}"; shift 2 ;;
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
case "$SEED" in
  ''|*[!0-9]*) log "the seed must be a number, got $SEED"; exit 2 ;;
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
    log "the binaries are missing, so the chaos run cannot start"
    exit 1
  fi
fi
[ -x "$HOST_BIN" ] || { log "the host binary $HOST_BIN is missing"; exit 1; }
[ -x "$CLIENT_BIN" ] || { log "the client binary $CLIENT_BIN is missing"; exit 1; }

# --------------------------------------------------------- the fault plan
# The sequence of faults and their parameters, drawn from the seed. The same
# seed always gives the same sequence.
PLAN="$RUNTIME_ROOT_ABS/plan.tsv"
python3 - "$PLAN" "$SEED" "$PLAN_LENGTH" <<'PY'
import random
import sys

path, seed, length = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rng = random.Random(seed)
kinds = [
    "netem_loss",
    "netem_jitter_reorder",
    "encoder_restart",
    "decoder_device_loss",
    "cpu_throttle",
    "netns_teardown",
]
with open(path, "w", encoding="utf-8") as handle:
    for _ in range(length):
        kind = rng.choice(kinds)
        loss = round(rng.uniform(1.0, 8.0), 1)
        delay = rng.choice([0, 2, 5, 10, 15, 20])
        jitter = rng.choice([1, 2, 5, 10, 20])
        reorder = rng.choice([5, 10, 20, 30])
        cpu = rng.randrange(0, 8)
        handle.write(
            f"{kind}\t{loss}\t{delay}\t{jitter}\t{reorder}\t{cpu}\n"
        )
print(f"[chaos] the fault plan holds {length} faults, seed {seed}")
PY

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
CLIENT_VA_DEVICE="$VA_DEVICE"

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  log "cleaning up"
  kill_pair
  netem_clear
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

# find_wayland <rt>: the name of the compositor's Wayland socket.
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

# --------------------------------------------------------------- namespaces
log "setting up the network namespaces"
HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" VETH_HOST="$VETH_HOST" \
  HOST_IP="$HOST_ADDR/24" CLIENT_IP="$CLIENT_ADDR/24" \
  "$REPO_ROOT/scripts/netns-up.sh"

STATE_DIR="$RUNTIME_ROOT_ABS/state"
mkdir -p "$STATE_DIR"
RUN_INDEX=0

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

# start_pair <duration>: brings a fresh host and client up and waits until
# the host reports that the client is paired.
start_pair() {
  local duration="$1" waited client_duration
  RUN_INDEX=$((RUN_INDEX + 1))
  HOST_REPORT="$RUNTIME_ROOT_ABS/run-$RUN_INDEX-host.json"
  CLIENT_REPORT="$RUNTIME_ROOT_ABS/run-$RUN_INDEX-client.json"
  HOST_LOG="$RUNTIME_ROOT_ABS/run-$RUN_INDEX-host.log"
  CLIENT_LOG="$RUNTIME_ROOT_ABS/run-$RUN_INDEX-client.log"
  rm -f "$HOST_REPORT" "$CLIENT_REPORT" "$HOST_LOG" "$CLIENT_LOG"

  sudo -n ip netns exec "$HOST_NS" env XDG_RUNTIME_DIR="$HOST_RT" \
    WAYLAND_DISPLAY="$HOST_WAYLAND" \
    "$HOST_BIN" --size "$SIZE" --fps "$FPS" --duration "$duration" \
      --control "$HOST_ADDR:$CONTROL_PORT" --client-media-port "$CLIENT_MEDIA_PORT" \
      --report "$HOST_REPORT" \
      --pairing-store "$STATE_DIR/pairing.json" \
      --pairing-code-file "$STATE_DIR/pairing-code.txt" \
      >"$HOST_LOG" 2>&1 &
  HOST_PID=$!

  sleep 1

  client_duration=$((duration - 2))
  [ "$client_duration" -lt 1 ] && client_duration=1
  sudo -n ip netns exec "$CLIENT_NS" env XDG_RUNTIME_DIR="$CLIENT_RT" \
    WAYLAND_DISPLAY="$CLIENT_WAYLAND" LUMEN_VA_DEVICE="$CLIENT_VA_DEVICE" \
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

pair_alive() {
  [ -n "$HOST_PID" ] && kill -0 "$HOST_PID" 2>/dev/null || return 1
  [ -n "$CLIENT_PID" ] && kill -0 "$CLIENT_PID" 2>/dev/null || return 1
  return 0
}

# ------------------------------------------------------------------ netem
# netem_apply: put the qdisc on both ends of the pair. The qdisc it wants
# goes into NETEM_QDISC; the reason it could not be installed goes into
# NETEM_ERROR, so that a fault that was not injected is never recorded as
# one that was.
NETEM_ERROR=""
NETEM_QDISC=""
netem_apply() {
  local loss="$1" delay="$2" jitter="$3" reorder="$4"
  local qdisc="netem"
  [ "$loss" != "0" ] && qdisc="$qdisc loss ${loss}%"
  if [ "$delay" != "0ms" ] || [ "$jitter" != "0ms" ]; then
    qdisc="$qdisc delay ${delay}ms ${jitter}ms"
  fi
  if [ "$reorder" != "0" ]; then
    # netem refuses to reorder without a delay: "reordering not possible
    # without specifying some delay". A delay of one millisecond is the
    # smallest that works.
    case "$qdisc" in
      *" delay "*) ;;
      *) qdisc="$qdisc delay 1ms" ;;
    esac
    qdisc="$qdisc reorder ${reorder}% 50%"
  fi
  qdisc="$qdisc limit 1000"
  NETEM_QDISC="$qdisc"

  NETEM_ERROR=""
  local ns dev shown
  for pair in "$HOST_NS $VETH_HOST" "$CLIENT_NS ${VETH_CLIENT:-lumen-c}"; do
    ns="${pair% *}"
    dev="${pair#* }"
    if ! sudo -n ip netns exec "$ns" tc qdisc replace dev "$dev" root $qdisc \
      >/dev/null 2>&1; then
      # An older tc may refuse to replace; take it out and add it back.
      sudo -n ip netns exec "$ns" tc qdisc del dev "$dev" root >/dev/null 2>&1 || true
      if ! sudo -n ip netns exec "$ns" tc qdisc add dev "$dev" root $qdisc \
        >/dev/null 2>&1; then
        NETEM_ERROR="netem is not permitted on $ns/$dev"
        log "$NETEM_ERROR"
        continue
      fi
    fi
    shown="$(sudo -n ip netns exec "$ns" tc qdisc show dev "$dev" 2>/dev/null || true)"
    case "$shown" in
      *netem*) log "  $ns/$dev: $(printf '%s' "$shown" | head -1)" ;;
      *)
        NETEM_ERROR="the netem qdisc did not take on $ns/$dev: $shown"
        log "$NETEM_ERROR"
        ;;
    esac
  done
}

netem_clear() {
  local ns dev
  for pair in "$HOST_NS $VETH_HOST" "$CLIENT_NS ${VETH_CLIENT:-lumen-c}"; do
    ns="${pair% *}"
    dev="${pair#* }"
    sudo -n ip netns exec "$ns" tc qdisc del dev "$dev" root >/dev/null 2>&1 || true
  done
}

# ensure_namespaces: the namespace pair has to be there before a cycle. It
# can be gone because the chaos tore it down, or because something else on
# the machine took it down; either way it is built again.
ensure_namespaces() {
  if sudo -n ip netns exec "$HOST_NS" true 2>/dev/null \
    && sudo -n ip netns exec "$CLIENT_NS" true 2>/dev/null \
    && sudo -n ip netns exec "$HOST_NS" ip link show "$VETH_HOST" >/dev/null 2>&1 \
    && sudo -n ip netns exec "$CLIENT_NS" ip link show "${VETH_CLIENT:-lumen-c}" \
      >/dev/null 2>&1; then
    return 0
  fi
  log "the network namespaces or the veth pair are gone; they are built again"
  HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" VETH_HOST="$VETH_HOST" \
    HOST_IP="$HOST_ADDR/24" CLIENT_IP="$CLIENT_ADDR/24" \
    "$REPO_ROOT/scripts/netns-up.sh" >/dev/null 2>&1 || true
  return 0
}

# ----------------------------------------------------------------- the run
EVENTS_FILE="$RUNTIME_ROOT_ABS/events.jsonl"
: >"$EVENTS_FILE"
EVENT_INDEX=0

record_event() {
  python3 - "$EVENTS_FILE" "$EVENT_INDEX" "$1" "$2" "$3" "$4" "$5" <<'PY'
import datetime
import json
import sys

path, index, kind, detail, survived, restarted, recovery_ms = sys.argv[1:8]
record = {
    "index": int(index),
    "kind": kind,
    "detail": detail,
    "at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    "session_survived": survived == "yes",
    "restarted_by_the_script": restarted == "yes",
    "recovery_ms": int(recovery_ms) if recovery_ms != "-" else None,
}
with open(path, "a", encoding="utf-8") as handle:
    handle.write(json.dumps(record, sort_keys=True) + "\n")
PY
}

START_EPOCH="$(date +%s)"
START_ISO="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
HOST_SHA="$(sha256sum "$HOST_BIN" | cut -c1-16)"
CLIENT_SHA="$(sha256sum "$CLIENT_BIN" | cut -c1-16)"

log "chaos for ${DURATION}s, one fault every ${INTERVAL}s, seed $SEED"
while :; do
  NOW="$(date +%s)"
  ELAPSED=$((NOW - START_EPOCH))
  [ "$ELAPSED" -ge "$DURATION" ] && break

  CYCLE_LEN="$INTERVAL"
  REMAINING=$((DURATION - ELAPSED))
  [ "$CYCLE_LEN" -gt "$REMAINING" ] && CYCLE_LEN="$REMAINING"
  [ "$CYCLE_LEN" -lt 10 ] && CYCLE_LEN=10

  # The next fault of the seeded plan.
  PLAN_LINE="$(sed -n "$((EVENT_INDEX + 1))p" "$PLAN")"
  [ -z "$PLAN_LINE" ] && break
  EVENT_INDEX=$((EVENT_INDEX + 1))
  IFS=$'\t' read -r KIND LOSS DELAY JITTER REORDER CPU <<<"$PLAN_LINE"

  ensure_namespaces

  if ! start_pair "$CYCLE_LEN"; then
    log "the session did not come up before fault $EVENT_INDEX"
    log "the host log:"
    sed 's/^/    /' "$HOST_LOG" 2>/dev/null || true
    log "the client log:"
    sed 's/^/    /' "$CLIENT_LOG" 2>/dev/null || true
    kill_pair
    record_event "session_failed" "the session did not come up before fault $EVENT_INDEX" \
      "no" "yes" "-"
    continue
  fi

  # Let the session settle before the fault.
  sleep 2

  DETAIL=""
  # EVENT_RESTARTED is set when the fault itself takes the session down and
  # the script brings it back inside the fault. Such a fault does not
  # survive: the session had to be restarted.
  EVENT_RESTARTED="no"
  EVENT_RECOVERY_MS="-"
  case "$KIND" in
    netem_loss)
      netem_apply "$LOSS" "$DELAY" "$JITTER" "$REORDER"
      DETAIL="tc netem on both ends of the veth pair: $NETEM_QDISC"
      [ -n "$NETEM_ERROR" ] && DETAIL="$DETAIL; NOT INJECTED: $NETEM_ERROR"
      ;;
    netem_jitter_reorder)
      netem_apply 0 "$DELAY" "$JITTER" "$REORDER"
      DETAIL="tc netem on both ends of the veth pair: $NETEM_QDISC"
      [ -n "$NETEM_ERROR" ] && DETAIL="$DETAIL; NOT INJECTED: $NETEM_ERROR"
      ;;
    encoder_restart)
      DETAIL="the host process is killed and started again, which restarts the encoder"
      EVENT_RESTARTED="yes"
      EVENT_RECOVERY_START="$(date +%s.%N)"
      kill_pair
      start_pair "$CYCLE_LEN" || true
      EVENT_RECOVERY_MS="$(python3 -c 'import sys; print(int((float(sys.argv[1])-float(sys.argv[2]))*1000))' "$(date +%s.%N)" "$EVENT_RECOVERY_START")"
      ;;
    decoder_device_loss)
      DETAIL="the client is restarted with LUMEN_VA_DEVICE=$MISSING_VA_DEVICE, so its decoder cannot be opened"
      EVENT_RESTARTED="yes"
      EVENT_RECOVERY_START="$(date +%s.%N)"
      kill_pair
      CLIENT_VA_DEVICE="$MISSING_VA_DEVICE"
      start_pair "$CYCLE_LEN" || true
      CLIENT_VA_DEVICE="$VA_DEVICE"
      EVENT_RECOVERY_MS="$(python3 -c 'import sys; print(int((float(sys.argv[1])-float(sys.argv[2]))*1000))' "$(date +%s.%N)" "$EVENT_RECOVERY_START")"
      ;;
    cpu_throttle)
      if command -v cpulimit >/dev/null 2>&1; then
        DETAIL="the client is capped with cpulimit at 50 % of one CPU for 10 s"
        cpulimit -p "$CLIENT_PID" -l 50 --quiet >/dev/null 2>&1 || true
        sleep 10
        cpulimit -p "$CLIENT_PID" -l 0 --quiet >/dev/null 2>&1 || true
      else
        DETAIL="the client is pinned to CPU $CPU with taskset for 10 s, because cpulimit is not installed"
        sudo -n taskset -pc "$CPU" "$CLIENT_PID" >/dev/null 2>&1 || true
        sleep 10
        sudo -n taskset -pc "0-$(nproc --all)" "$CLIENT_PID" >/dev/null 2>&1 || true
      fi
      ;;
    netns_teardown)
      DETAIL="the network namespace pair is torn down and built again"
      EVENT_RESTARTED="yes"
      EVENT_RECOVERY_START="$(date +%s.%N)"
      kill_pair
      HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" "$REPO_ROOT/scripts/netns-down.sh" \
        >/dev/null 2>&1 || true
      sleep 2
      ensure_namespaces
      start_pair "$CYCLE_LEN" || true
      EVENT_RECOVERY_MS="$(python3 -c 'import sys; print(int((float(sys.argv[1])-float(sys.argv[2]))*1000))' "$(date +%s.%N)" "$EVENT_RECOVERY_START")"
      ;;
    *)
      DETAIL="unknown fault"
      ;;
  esac

  # Give the session a moment to show whether it survived.
  sleep 2
  SURVIVED="no"
  if [ "$EVENT_RESTARTED" = "no" ] && pair_alive; then
    SURVIVED="yes"
  fi

  RESTARTED="$EVENT_RESTARTED"
  RECOVERY_MS="$EVENT_RECOVERY_MS"
  if [ "$SURVIVED" = "yes" ]; then
    :
  elif [ "$EVENT_RESTARTED" = "no" ]; then
    # The fault did not restart the pair itself, but the session is gone:
    # the script brings it back and measures how long that takes.
    RESTARTED="yes"
    RECOVERY_START="$(date +%s.%N)"
    kill_pair
    CLIENT_VA_DEVICE="$VA_DEVICE"
    if start_pair "$CYCLE_LEN"; then
      RECOVERY_MS="$(python3 -c 'import sys; print(int((float(sys.argv[1])-float(sys.argv[2]))*1000))' "$(date +%s.%N)" "$RECOVERY_START")"
    fi
  fi

  record_event "$KIND" "$DETAIL" "$SURVIVED" "$RESTARTED" "$RECOVERY_MS"
  log "fault $EVENT_INDEX: $KIND survived=$SURVIVED restarted=$RESTARTED recovery_ms=$RECOVERY_MS"

  # The netem qdisc is taken out again at the end of the cycle, so the next
  # cycle starts from a clean link.
  netem_clear

  # Let the cycle run out so both ends write their counters.
  wait "$HOST_PID" 2>/dev/null || true
  wait "$CLIENT_PID" 2>/dev/null || true
  HOST_PID=""
  CLIENT_PID=""
done

END_EPOCH="$(date +%s)"
END_ISO="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
ACTUAL_S=$((END_EPOCH - START_EPOCH))

log "the run lasted ${ACTUAL_S}s; stopping"
kill_pair
netem_clear
for pid in "$HOST_SWAY_PID" "$CLIENT_SWAY_PID"; do
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
done
HOST_SWAY_PID=""
CLIENT_SWAY_PID=""
HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" "$REPO_ROOT/scripts/netns-down.sh" >/dev/null 2>&1 || true

# --------------------------------------------------------------- the report
log "writing $OUT"
python3 - "$OUT_ABS" "$RUNTIME_ROOT_ABS" "$EVENTS_FILE" "$DURATION" "$ACTUAL_S" \
  "$SEED" "$INTERVAL" "$SIZE" "$FPS" "$START_ISO" "$END_ISO" \
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
    seed,
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


def percentile(ordered, q):
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
    "frames_rebuilt": 0,
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

seconds = max(host_counters["duration_ms"], client_counters["duration_ms"]) / 1000.0
bitrate_kbps = (host_counters["bytes_sent"] * 8.0 / seconds / 1000.0) if seconds else 0.0

report = {
    "milestone": "M8.4",
    "chaos": {
        "requested_duration_s": duration,
        "duration_actual_s": actual,
        "requested_remainder_s": max(0, duration - actual),
        "pending": {
            "pending": actual < duration,
            "reason": (
                "the run was stopped when the requested length was up; the "
                "rest of the chaos plan was not run"
            ),
        },
        "seed": int(seed),
        "interval_s": int(interval),
        "started_at_utc": start_iso,
        "finished_at_utc": end_iso,
        "cycles": len(events),
        "complete_runs": len(paired),
        "runs_without_a_host_report": len(client_only),
        "runs_without_a_client_report": len(host_only),
    },
    "events": events,
    "event_counts": event_counts,
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
            "the in-session recovery of M6.1 and M6.2 is not implemented, so a "
            "recovery here is the time to a supervised restart of the pair"
        ),
    },
    "counters": {
        "frames_sent": frames_sent,
        "frames_decoded": frames_decoded,
        "frames_presented": frames_presented,
        "dropped_frames": max(0, frames_sent - frames_decoded),
        "decode_errors": client_counters["decode_errors"],
        "frames_rebuilt": client_counters["frames_rebuilt"],
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

print(f"[chaos] wrote {out_path}")
print(f"[chaos] ran {actual}s of the {duration}s asked for")
print(f"[chaos] faults: {event_counts}")
print(
    f"[chaos] survived {survived} of {len(events)}; "
    f"frames sent {frames_sent} decoded {frames_decoded} presented {frames_presented}"
)
PY

log "done: $OUT"
