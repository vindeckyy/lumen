#!/usr/bin/env bash
# Drive `tc netem` on the network namespace pair.
#
# Plan milestone M2.5: "Loss/jitter/reorder injection in-process for tests,
# plus `tc netem` driver for system-level runs".
#
# The in-process injector lives in `crates/lumen-transport/src/inject.rs` and
# needs no privileges. This script is the other half: it puts a real netem
# qdisc on a real interface, so the kernel drops, delays and reorders the
# packets of a real run.
#
# What it does
# ------------
# 1. Puts a netem qdisc on the given interface, in the given namespace.
#    The default is both ends of the veth pair of scripts/netns-up.sh, which
#    is what a symmetric path looks like.
# 2. Checks that the qdisc really took, with `tc qdisc show`. If it did not,
#    the script says so and exits non-zero: a run that thinks it has 5 % loss
#    and has none is worse than no run at all.
# 3. Optionally runs a command while the qdisc is in place.
# 4. Takes the qdisc out again, including when the command failed.
#
# `ip -n NS tc ...` is invalid. tc has to run inside the namespace, so the
# script uses `ip netns exec NS tc ...`.
#
# Exit status:
#   0  the qdisc was installed (and, when a command was given, the command
#      succeeded)
#   1  the qdisc could not be installed, or the command failed
#   2  a bad command line
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# --------------------------------------------------------------- settings
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"
VETH_HOST="${VETH_HOST:-lumen-h}"
VETH_CLIENT="${VETH_CLIENT:-lumen-c}"
# The netem parameters. A loss of 0 and a delay of 0 mean "not set", which
# is how netem itself reads them.
LOSS="${LOSS:-0}"
DELAY="${DELAY:-0ms}"
JITTER="${JITTER:-0ms}"
CORRELATION="${CORRELATION:-0%}"
REORDER="${REORDER:-0%}"
REORDER_GAP="${REORDER_GAP:-50}"
LIMIT="${LIMIT:-1000}"
# Which ends to touch: both, host or client.
ENDS="${ENDS:-both}"

log() { printf '[netem-run] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: netem-run.sh [options] [-- command ...]

Put a tc netem qdisc on the network namespace pair, optionally run a command
while it is in place, and take the qdisc out again.

Options:
  --loss <pct>      The percentage of packets to drop (default 0)
  --delay <d>       The mean delay netem adds, for example 10ms (default 0ms)
  --jitter <d>      The jitter of that delay (default 0ms)
  --correlation <p> How much of the loss is correlated, in percent
                    (default 0%)
  --reorder <pct>   The percentage of packets to reorder (default 0%)
  --reorder-gap <n> The gap between the reordered packets, in packets
                    (default 50)
  --limit <n>       The netem queue limit (default 1000)
  --ends <which>    both, host or client (default both)
  --show            Print the qdisc that is in place and exit
  --clear           Take the qdisc out and exit
  -h, --help        Print this text

Everything after a bare -- is run while the qdisc is in place.

Examples:
  netem-run.sh --loss 5% --delay 10ms --jitter 2ms
  netem-run.sh --loss 2% -- scripts/sweep-loss.sh --levels 0.02 --out /tmp/c.csv
  netem-run.sh --show
  netem-run.sh --clear
EOF
}

COMMAND=()
SEEN_SEPARATOR=0

while [ $# -gt 0 ]; do
  if [ "$SEEN_SEPARATOR" = "1" ]; then
    COMMAND+=("$1")
    shift
    continue
  fi
  case "$1" in
    --) SEEN_SEPARATOR=1; shift ;;
    --loss) LOSS="${2:?--loss needs a value}"; shift 2 ;;
    --delay) DELAY="${2:?--delay needs a value}"; shift 2 ;;
    --jitter) JITTER="${2:?--jitter needs a value}"; shift 2 ;;
    --correlation) CORRELATION="${2:?--correlation needs a value}"; shift 2 ;;
    --reorder) REORDER="${2:?--reorder needs a value}"; shift 2 ;;
    --reorder-gap) REORDER_GAP="${2:?--reorder-gap needs a value}"; shift 2 ;;
    --limit) LIMIT="${2:?--limit needs a value}"; shift 2 ;;
    --ends) ENDS="${2:?--ends needs a value}"; shift 2 ;;
    --show) SHOW_ONLY=1; shift ;;
    --clear) CLEAR_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

SHOW_ONLY="${SHOW_ONLY:-0}"
CLEAR_ONLY="${CLEAR_ONLY:-0}"

command -v sudo >/dev/null 2>&1 || { log "sudo is needed for the network namespaces"; exit 1; }
sudo -n true 2>/dev/null || { log "sudo -n does not work here"; exit 1; }

case "$ENDS" in
  both|host|client) ;;
  *) log "the ends must be both, host or client, got $ENDS"; exit 2 ;;
esac

# The pairs of namespace and interface that are touched.
PAIRS=()
case "$ENDS" in
  both)
    PAIRS+=("$HOST_NS $VETH_HOST")
    PAIRS+=("$CLIENT_NS $VETH_CLIENT")
    ;;
  host) PAIRS+=("$HOST_NS $VETH_HOST") ;;
  client) PAIRS+=("$CLIENT_NS $VETH_CLIENT") ;;
esac

netem_args() {
  local args="netem"
  [ "$LOSS" != "0" ] && args="$args loss $LOSS"
  if [ "$DELAY" != "0ms" ] || [ "$JITTER" != "0ms" ]; then
    args="$args delay $DELAY $JITTER"
  fi
  [ "$CORRELATION" != "0%" ] && args="$args loss $LOSS $CORRELATION"
  if [ "$REORDER" != "0%" ]; then
    # netem refuses to reorder without a delay: "reordering not possible
    # without specifying some delay".
    case "$args" in
      *" delay "*) ;;
      *) args="$args delay 1ms" ;;
    esac
    args="$args reorder $REORDER $REORDER_GAP"
  fi
  args="$args limit $LIMIT"
  printf '%s' "$args"
}

apply_one() {
  local ns="$1" dev="$2" args
  args="$(netem_args)"
  # `replace` adds the qdisc when there is none and swaps it when there is
  # one, so the same call works every time.
  if ! sudo -n ip netns exec "$ns" tc qdisc replace dev "$dev" root $args \
    >/dev/null 2>&1; then
    # An older tc may refuse to replace; take it out and add it back.
    sudo -n ip netns exec "$ns" tc qdisc del dev "$dev" root >/dev/null 2>&1 || true
    if ! sudo -n ip netns exec "$ns" tc qdisc add dev "$dev" root $args; then
      log "netem is not permitted on $ns/$dev: cannot add the qdisc"
      return 1
    fi
  fi
  local shown
  shown="$(sudo -n ip netns exec "$ns" tc qdisc show dev "$dev" 2>/dev/null || true)"
  case "$shown" in
    *netem*) ;;
    *) log "the netem qdisc did not take on $ns/$dev: $shown"; return 1 ;;
  esac
  log "  $ns/$dev: $(printf '%s' "$shown" | head -1)"
  return 0
}

clear_one() {
  local ns="$1" dev="$2"
  sudo -n ip netns exec "$ns" tc qdisc del dev "$dev" root >/dev/null 2>&1 || true
  log "  $ns/$dev: the qdisc is gone"
}

if [ "$SHOW_ONLY" = "1" ]; then
  for pair in "${PAIRS[@]}"; do
    ns="${pair% *}"
    dev="${pair#* }"
    shown="$(sudo -n ip netns exec "$ns" tc qdisc show dev "$dev" 2>/dev/null || true)"
    log "$ns/$dev: ${shown:-no qdisc}"
  done
  exit 0
fi

if [ "$CLEAR_ONLY" = "1" ]; then
  for pair in "${PAIRS[@]}"; do
    clear_one "${pair% *}" "${pair#* }"
  done
  exit 0
fi

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [ "$INSTALLED" = "1" ]; then
    log "taking the qdisc out again"
    for pair in "${PAIRS[@]}"; do
      clear_one "${pair% *}" "${pair#* }"
    done
  fi
  exit "$status"
}
INSTALLED=0
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "installing the qdisc: $(netem_args)"
for pair in "${PAIRS[@]}"; do
  apply_one "${pair% *}" "${pair#* }" || exit 1
done
INSTALLED=1

if [ "${#COMMAND[@]}" -eq 0 ]; then
  log "the qdisc is in place; it stays there until this script is stopped"
  log "run 'scripts/netem-run.sh --clear' to take it out"
  INSTALLED=0
  exit 0
fi

log "running the command while the qdisc is in place"
set +e
"${COMMAND[@]}"
STATUS=$?
set -e
log "the command exited with $STATUS"
exit "$STATUS"
