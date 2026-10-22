#!/usr/bin/env bash
# Sweep the loss rate of the host to client link and publish the curve.
#
# Plan milestone M2.8: "Publish degradation curves at 0.5 %, 2 %, 5 % loss and
# 10 ms jitter -> CSV plus curve in docs/benchmarks.md".
#
# For every level this script
#   1. puts a netem qdisc on both ends of the veth pair, inside the network
#      namespaces of scripts/netns-up.sh,
#   2. runs the host and the client the way scripts/run-e2e.sh does, against
#      two headless sway compositors, so the capture has a real source,
#   3. merges the two counter files with `lumen-bench report` into
#      artifacts/m2/loss-<rate>.json and appends one row to the curve CSV.
#
# The levels are loss fractions, as the plan names them: 0.005 is 0.5 % of the
# packets. netem takes percentages, so the fraction is multiplied by
# LOSS_SCALE (100 by default) before it reaches the qdisc.
#
# The qdisc is replaced inside the namespaces between the runs, with
# `ip netns exec NS tc qdisc replace`, so the namespaces are built once and
# the sweep does not tear them down and build them again for every level.
# `ip -n NS tc ...` is invalid; tc has to run inside the namespace.
#
# The client stops two seconds before the host, the way scripts/run-e2e.sh
# does it, so the client can drain the last frames. The datagrams the host
# sends after the client is gone count as lost, which adds the same small
# tail to every level of the curve.
#
# Every number in the curve is provisional: they are software counters, not
# glass-to-glass.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

# --------------------------------------------------------------- settings
# The plan names the levels as loss fractions: 0.005, 0.02 and 0.05, that is
# 0.5 %, 2 % and 5 % of the packets.
LEVELS="${LEVELS:-0.005,0.02,0.05}"
JITTER="${JITTER:-10ms}"
DURATION="${DURATION:-12}"
SIZE="${SIZE:-1280x720}"
FPS="${FPS:-30}"
OUT_DIR="${OUT_DIR:-artifacts/m2}"
OUT="${OUT:-}"
# netem takes percentages, the levels are fractions.
LOSS_SCALE="${LOSS_SCALE:-100}"
# The runtime state: the compositors, the pairing state and the logs of the
# run. It is removed again when the sweep ends.
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-sweep-loss}"
# The base ports. Every level gets its own pair, so a level never inherits a
# socket of the level before it.
CONTROL_PORT="${CONTROL_PORT:-24800}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24801}"
# The namespaces and the veth pair of scripts/netns-up.sh.
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"
VETH_HOST="${VETH_HOST:-lumen-h}"
VETH_CLIENT="${VETH_CLIENT:-lumen-c}"
HOST_ADDR="${HOST_ADDR:-10.77.0.1}"
CLIENT_ADDR="${CLIENT_ADDR:-10.77.0.2}"

CSV_HEADER="loss_rate,jitter_ms,frames_sent,frames_decoded,frames_presented,dropped_frames,decode_errors,frames_rebuilt,retransmits,packets_lost,packets_received,bitrate_kbps"

log() { printf '[sweep-loss] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: sweep-loss.sh [options]

Sweep the loss rate of the lumen host to client link and write the
degradation curve (plan milestone M2.8). Every level runs the host and the
client over the veth pair of scripts/netns-up.sh, with netem on both ends.

Options:
  --levels <a,b,c>  Loss levels to sweep, as loss fractions
                    (default 0.005,0.02,0.05, that is 0.5 %, 2 % and 5 %)
  --jitter <d>      Jitter netem applies; the same value is used as the
                    mean delay (default 10ms)
  --duration <s>    Length of one run in seconds (default 12)
  --size <WxH>      Capture size (default 1280x720)
  --fps <n>         Stream rate (default 30)
  --out <path>      Where the curve CSV goes (default artifacts/m2/curve.csv)
  --out-dir <path>  Where the per level reports go (default artifacts/m2)
  -h, --help        Print this text

Environment:
  LEVELS, JITTER, DURATION, SIZE, FPS, OUT, OUT_DIR, LOSS_SCALE, RUNTIME_ROOT,
  CONTROL_PORT, CLIENT_MEDIA_PORT, HOST_NS, CLIENT_NS, HOST_ADDR, CLIENT_ADDR

The names of the levels follow the plan: 0.005 is a loss fraction, so it
means 0.5 % of the packets. netem takes percentages, so the fraction is
scaled by LOSS_SCALE (100 by default) before it reaches the qdisc.

Every number in the curve is provisional: they are software counters, not
glass-to-glass.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --levels) LEVELS="${2:?--levels needs a value}"; shift 2 ;;
    --jitter) JITTER="${2:?--jitter needs a value}"; shift 2 ;;
    --duration) DURATION="${2:?--duration needs a value}"; shift 2 ;;
    --size) SIZE="${2:?--size needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
    --out) OUT="${2:?--out needs a value}"; shift 2 ;;
    --out-dir) OUT_DIR="${2:?--out-dir needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

# The curve lives in the output directory unless it is named explicitly.
OUT="${OUT:-$OUT_DIR/curve.csv}"

# The files are touched from inside the namespaces too, so they are absolute.
case "$OUT_DIR" in
  /*) OUT_DIR_ABS="$OUT_DIR" ;;
  *) OUT_DIR_ABS="$REPO_ROOT/$OUT_DIR" ;;
esac
case "$OUT" in
  /*) OUT_ABS="$OUT" ;;
  *) OUT_ABS="$REPO_ROOT/$OUT" ;;
esac
case "$RUNTIME_ROOT" in
  /*) RUNTIME_ROOT_ABS="$RUNTIME_ROOT" ;;
  *) RUNTIME_ROOT_ABS="$REPO_ROOT/$RUNTIME_ROOT" ;;
esac

case "$LEVELS" in
  '') log "LEVELS is empty"; exit 2 ;;
esac
case "$DURATION" in
  ''|*[!0-9]*) log "DURATION must be a number of seconds, got $DURATION"; exit 2 ;;
esac
if [ "$DURATION" -lt 4 ]; then
  log "DURATION must be at least 4 seconds, got $DURATION"
  exit 2
fi
command -v python3 >/dev/null 2>&1 || { log "python3 is needed to read the reports"; exit 1; }
command -v sudo >/dev/null 2>&1 || { log "sudo is needed for the network namespaces"; exit 1; }
sudo -n true 2>/dev/null || { log "sudo -n does not work here"; exit 1; }

# The jitter in milliseconds, for the CSV column.
JITTER_MS="${JITTER%ms}"
case "$JITTER_MS" in
  ''|*[!0-9.]*) log "cannot read the jitter $JITTER in milliseconds"; exit 2 ;;
esac

IFS=',' read -r -a LEVEL_LIST <<<"$LEVELS"
if [ "${#LEVEL_LIST[@]}" -eq 0 ]; then
  log "no levels to sweep"
  exit 2
fi

# Check the levels before anything is started.
for level in "${LEVEL_LIST[@]}"; do
  case "${level//[[:space:]]/}" in
    ''|*[!0-9.]*) log "the level $level is not a number"; exit 2 ;;
  esac
done

mkdir -p "$OUT_DIR_ABS" "$RUNTIME_ROOT_ABS"

# ------------------------------------------------------------------ build
log "building the host, the client and the bench tool"
cargo build --release -p lumen-host-service -p lumen-client-desktop -p lumen-bench --bins \
  >/dev/null
HOST_BIN="$REPO_ROOT/target/release/lumen-host"
CLIENT_BIN="$REPO_ROOT/target/release/lumen-client"
BENCH_BIN="$REPO_ROOT/target/release/lumen-bench"

# ------------------------------------------------------------- compositors
# Two headless compositors: one the host captures, one the client presents
# into. Each gets its own XDG_RUNTIME_DIR so they do not collide. They live
# outside the namespaces: the Wayland socket is a filesystem path, so a
# compositor started outside a namespace is still reachable from inside one.
HOST_RT="$RUNTIME_ROOT_ABS/host"
CLIENT_RT="$RUNTIME_ROOT_ABS/client"
mkdir -p "$HOST_RT" "$CLIENT_RT"
chmod 700 "$HOST_RT" "$CLIENT_RT"

start_compositor() {
  local rt="$1" logfile="$2"
  XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless WLR_RENDERER="${WLR_RENDERER:-pixman}" \
    setsid nohup sway -c /dev/null >"$logfile" 2>&1 &
  echo $!
}

HOST_SWAY_PID=""
CLIENT_SWAY_PID=""
HOST_PID=""
CLIENT_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  log "cleaning up"
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null || true
  [ -n "$CLIENT_PID" ] && kill "$CLIENT_PID" 2>/dev/null || true
  pkill -x lumen-host 2>/dev/null || true
  pkill -x lumen-client 2>/dev/null || true
  [ -n "$HOST_SWAY_PID" ] && kill "$HOST_SWAY_PID" 2>/dev/null || true
  [ -n "$CLIENT_SWAY_PID" ] && kill "$CLIENT_SWAY_PID" 2>/dev/null || true
  rm -rf "$HOST_RT" "$CLIENT_RT"
  HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" "$REPO_ROOT/scripts/netns-down.sh" \
    >/dev/null 2>&1 || true
  rm -rf "$RUNTIME_ROOT_ABS"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "starting the headless compositors"
HOST_SWAY_PID="$(start_compositor "$HOST_RT" "$RUNTIME_ROOT_ABS/sway-host.log")"
CLIENT_SWAY_PID="$(start_compositor "$CLIENT_RT" "$RUNTIME_ROOT_ABS/sway-client.log")"

for _ in $(seq 1 50); do
  if [ -S "$HOST_RT/wayland-1" ] && [ -S "$CLIENT_RT/wayland-1" ]; then
    break
  fi
  sleep 0.2
done
[ -S "$HOST_RT/wayland-1" ] || { log "the host compositor did not start"; exit 1; }
[ -S "$CLIENT_RT/wayland-1" ] || { log "the client compositor did not start"; exit 1; }

# --------------------------------------------------------------- namespaces
log "setting up the network namespaces"
HOST_NS="$HOST_NS" CLIENT_NS="$CLIENT_NS" VETH_HOST="$VETH_HOST" VETH_CLIENT="$VETH_CLIENT" \
  HOST_IP="$HOST_ADDR/24" CLIENT_IP="$CLIENT_ADDR/24" \
  "$REPO_ROOT/scripts/netns-up.sh"
if ! sudo -n ip netns exec "$HOST_NS" true 2>/dev/null; then
  log "the namespace $HOST_NS is not usable"
  exit 1
fi

# ------------------------------------------------------------------ netem
# Put the qdisc on one end of the veth pair. `replace` adds the qdisc when
# there is none and swaps it when there is one, so the same call works for
# the first level and for every level after it.
netem_apply() {
  local ns="$1" dev="$2" loss_pct="$3"
  local shown
  if ! sudo -n ip netns exec "$ns" tc qdisc replace dev "$dev" root netem \
    loss "${loss_pct}%" delay "$JITTER" "$JITTER" limit 1000 2>/dev/null; then
    # An older tc may refuse to replace; take the qdisc out and add it back.
    sudo -n ip netns exec "$ns" tc qdisc del dev "$dev" root 2>/dev/null || true
    if ! sudo -n ip netns exec "$ns" tc qdisc add dev "$dev" root netem \
      loss "${loss_pct}%" delay "$JITTER" "$JITTER" limit 1000; then
      log "netem is not permitted on $ns/$dev: cannot add the qdisc"
      return 1
    fi
  fi
  shown="$(sudo -n ip netns exec "$ns" tc qdisc show dev "$dev" 2>/dev/null || true)"
  case "$shown" in
    *netem*) ;;
    *) log "the netem qdisc did not take on $ns/$dev: $shown"; return 1 ;;
  esac
  log "  $ns/$dev: $(printf '%s' "$shown" | head -1)"
}

# --------------------------------------------------------------- one level
# Run the host and the client the way scripts/run-e2e.sh does, with the
# netem qdisc of this level on both ends of the link.
run_level() {
  local level="$1" index="$2" loss_pct="$3"
  local tag="loss-$level"
  local state_dir="$RUNTIME_ROOT_ABS/state/$tag"
  local host_report="$OUT_DIR_ABS/$tag-host.json"
  local client_report="$OUT_DIR_ABS/$tag-client.json"
  local merged="$OUT_DIR_ABS/$tag.json"
  local control_port=$((CONTROL_PORT + 2 * index))
  local media_port=$((CLIENT_MEDIA_PORT + 2 * index))

  mkdir -p "$state_dir"
  rm -f "$host_report" "$client_report" "$merged" "$state_dir/pairing-code.txt"

  log "level $level ($loss_pct % of the packets), jitter $JITTER"
  netem_apply "$HOST_NS" "$VETH_HOST" "$loss_pct" || return 1
  netem_apply "$CLIENT_NS" "$VETH_CLIENT" "$loss_pct" || return 1

  log "running the host and the client for ${DURATION}s"
  sudo -n ip netns exec "$HOST_NS" env XDG_RUNTIME_DIR="$HOST_RT" \
    WAYLAND_DISPLAY=wayland-1 \
    "$HOST_BIN" --size "$SIZE" --fps "$FPS" \
      --control "$HOST_ADDR:$control_port" --client-media-port "$media_port" \
      --duration "$DURATION" --report "$host_report" \
      --pairing-store "$state_dir/pairing.json" \
      --pairing-code-file "$state_dir/pairing-code.txt" \
      >"$OUT_DIR_ABS/$tag-host.log" 2>&1 &
  HOST_PID=$!

  # Give the host time to bind the control listener before the client dials.
  sleep 1

  sudo -n ip netns exec "$CLIENT_NS" env XDG_RUNTIME_DIR="$CLIENT_RT" \
    WAYLAND_DISPLAY=wayland-1 \
    "$CLIENT_BIN" --host "$HOST_ADDR:$control_port" \
      --media-bind "$CLIENT_ADDR:$media_port" \
      --duration "$((DURATION - 2))" --report "$client_report" \
      --identity "$state_dir/client-identity" \
      --pairing-code-file "$state_dir/pairing-code.txt" \
      >"$OUT_DIR_ABS/$tag-client.log" 2>&1 &
  CLIENT_PID=$!

  wait "$CLIENT_PID" || log "the client exited with a failure"
  wait "$HOST_PID" || log "the host exited with a failure"
  HOST_PID=""
  CLIENT_PID=""

  if [ ! -f "$host_report" ] || [ ! -f "$client_report" ]; then
    log "the run at $level produced no reports"
    sed 's/^/    /' "$OUT_DIR_ABS/$tag-host.log" 2>/dev/null || true
    sed 's/^/    /' "$OUT_DIR_ABS/$tag-client.log" 2>/dev/null || true
    return 1
  fi

  # The host and the client run as root inside the namespaces, so the files
  # they write would stay root owned; hand them back to the user.
  if [ "$(id -u)" -ne 0 ]; then
    sudo -n chown "$(id -u):$(id -g)" \
      "$host_report" "$client_report" \
      "$OUT_DIR_ABS/$tag-host.log" "$OUT_DIR_ABS/$tag-client.log" 2>/dev/null || true
  fi

  log "merging the reports into $merged"
  "$BENCH_BIN" report --host "$host_report" --client "$client_report" --out "$merged" \
    --set "mode=netns" --set "size=$SIZE" --set "fps=$FPS" \
    --set "duration_s=$DURATION" --set "transport=custom-udp+fec-gf256" \
    --set "codec=h264-vaapi" --set "capture=wlroots-screencopy" \
    --set "loss_rate=$level" --set "loss_percent=$loss_pct" \
    --set "jitter=$JITTER" \
    --set "netem=loss ${loss_pct}% delay $JITTER $JITTER limit 1000" \
    >/dev/null || { log "cannot merge the reports of $level"; return 1; }
}

# ------------------------------------------------------------- csv helpers
# One row of the curve, read out of the merged report.
csv_row() {
  python3 - "$1" "$2" "$3" <<'PY'
import json
import sys

report_path, loss_rate, jitter_ms = sys.argv[1], sys.argv[2], sys.argv[3]
with open(report_path, encoding="utf-8") as handle:
    report = json.load(handle)
counters = report.get("counters", {})


def count(name):
    return counters.get(name, 0)


print(
    ",".join(
        [
            loss_rate,
            jitter_ms,
            str(count("frames_sent")),
            str(count("frames_decoded")),
            str(count("frames_presented")),
            str(count("dropped_frames")),
            str(count("decode_errors")),
            str(count("frames_rebuilt")),
            str(count("retransmits")),
            str(count("packets_lost")),
            str(count("packets_received")),
            f'{report.get("bitrate_kbps", 0.0):.3f}',
        ]
    )
)
PY
}

# ------------------------------------------------------------------ sweep
log "sweeping the loss levels $LEVELS, jitter $JITTER, $SIZE at $FPS fps"
printf '%s\n' "$CSV_HEADER" >"$OUT_ABS"

index=0
for level in "${LEVEL_LIST[@]}"; do
  # Trim the spaces around a level.
  level="${level#"${level%%[![:space:]]*}"}"
  level="${level%"${level##*[![:space:]]}"}"
  case "$level" in
    ''|*[!0-9.]*) log "the level $level is not a number"; exit 2 ;;
  esac
  # The levels are fractions, netem takes percentages.
  loss_pct="$(awk -v level="$level" -v scale="$LOSS_SCALE" 'BEGIN { printf "%g", level * scale }')"

  if ! run_level "$level" "$index" "$loss_pct"; then
    log "the level $level failed"
    exit 1
  fi

  row="$(csv_row "$OUT_DIR_ABS/loss-$level.json" "$level" "$JITTER_MS")"
  printf '%s\n' "$row" >>"$OUT_ABS"
  log "  $row"
  index=$((index + 1))
done

# ---------------------------------------------------------------- summary
log "the loss curve:"
printf '    %-9s %-8s %6s %8s %10s %8s %6s %11s\n' \
  loss jitter sent decoded presented dropped lost bitrate
# $1 loss, $2 jitter, $3 sent, $4 decoded, $5 presented, $6 dropped,
# $10 lost, $12 bitrate.
awk -F, 'NR > 1 {printf "    %-9s %-8s %6s %8s %10s %8s %6s %11s\n", \
  $1, $2 "ms", $3, $4, $5, $6, $10, $12}' "$OUT_ABS"
log "the curve is in $OUT"
