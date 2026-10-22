#!/usr/bin/env bash
# Measure what a shaped link carries, over a matrix of link profiles.
#
# The loss sweep (scripts/sweep-loss.sh) answers "what happens when packets
# are lost". This script answers the other half: "what happens when the link
# is slow, late, or both". Every profile shapes the link with netem (rate,
# delay, jitter, loss) and runs the host and the client the way
# scripts/run-e2e.sh does, against two headless sway compositors.
#
# The result is one CSV row per profile: how many frames were presented, how
# many were dropped, the decode p99, the bitrate the host sent, and the
# picture the client's quality ladder settled on. The last column is the one
# the ladder was built for: a link that cannot carry the picture makes the
# client ask for a smaller one.
#
# The idea is punktfunk's scripts/abr-rig (MIT OR Apache-2.0, Copyright (c)
# 2026 unom - Enrico Bühler), which shapes a link and measures the same
# thing.
#
# Every number is a software counter, not glass-to-glass.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

# --------------------------------------------------------------- settings
# The profiles, as "name:netem arguments". A profile without a rate limit
# runs at the speed of the link.
PROFILES="${PROFILES:-clean:delay 0ms 0ms limit 1000
fast:rate 20mbit delay 5ms 5ms limit 1000
slow:rate 2mbit delay 20ms 10ms limit 1000
lossy:rate 8mbit delay 10ms 5ms limit 1000 loss 2%}"
DURATION="${DURATION:-12}"
SIZE="${SIZE:-1280x720}"
FPS="${FPS:-30}"
OUT_DIR="${OUT_DIR:-artifacts/link-matrix}"
CSV_HEADER="profile,netem,frames_sent,frames_decoded,frames_presented,dropped_frames,decode_errors,frames_rebuilt,decode_p99_ms,end_to_end_p99_ms,bitrate_kbps,final_picture,quality_steps"

RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-link-matrix}"
CONTROL_PORT="${CONTROL_PORT:-24800}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24801}"
HOST_NS="${HOST_NS:-lumen-host}"
CLIENT_NS="${CLIENT_NS:-lumen-client}"

log() { printf '[link-matrix] %s\n' "$*"; }

usage() {
  cat <<'TEXT'
Usage: scripts/link-matrix.sh [options]

  --profiles <list>   The profiles, "name:netem args" separated by ';'
  --duration <s>      How long every profile runs (default 12)
  --size <WxH>        Capture size (default 1280x720)
  --fps <n>           Frame rate (default 30)
  --out-dir <path>    Where the reports and the CSV go (default artifacts/link-matrix)
  --help              Print this text
TEXT
}

while [ $# -gt 0 ]; do
  case "$1" in
    --profiles) PROFILES="$2"; shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --size) SIZE="$2"; shift 2 ;;
    --fps) FPS="$2"; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

OUT_ABS="$REPO_ROOT/$OUT_DIR"
mkdir -p "$OUT_ABS"
CSV="$OUT_ABS/matrix.csv"

# ------------------------------------------------------------- build
cargo build --release -p lumen-host-service -p lumen-client-desktop -p lumen-bench --bins \
  2>&1 | tail -2
HOST_BIN="$REPO_ROOT/target/release/lumen-host"
CLIENT_BIN="$REPO_ROOT/target/release/lumen-client"
BENCH_BIN="$REPO_ROOT/target/release/lumen-bench"

# --------------------------------------------------------- namespaces
if ! sudo -n ip netns exec "$HOST_NS" true 2>/dev/null; then
  log "the namespaces are not up; run scripts/netns-up.sh first"
  exit 1
fi

# The netem qdisc of one profile, on both ends of the veth pair.
shape() {
  local args="$1"
  for ns_dev in "$HOST_NS:lumen-h" "$CLIENT_NS:lumen-c"; do
    local ns="${ns_dev%%:*}" dev="${ns_dev##*:}"
    sudo -n ip netns exec "$ns" tc qdisc replace dev "$dev" root netem $args 2>/dev/null || {
      sudo -n ip netns exec "$ns" tc qdisc del dev "$dev" root 2>/dev/null || true
      sudo -n ip netns exec "$ns" tc qdisc add dev "$dev" root netem $args
    }
  done
}

# One profile: shape the link, run the pair, merge the reports.
run_profile() {
  local name="$1" netem_args="$2" index="$3"
  local tag="$index-$name"
  local host_report="$OUT_ABS/$tag-host.json"
  local client_report="$OUT_ABS/$tag-client.json"
  local merged="$OUT_ABS/$tag.json"

  log "profile $name: netem $netem_args"
  shape "$netem_args"

  if ! "$REPO_ROOT/scripts/run-e2e.sh" \
      --host-ns "$HOST_NS" --client-ns "$CLIENT_NS" \
      --width "${SIZE%x*}" --height "${SIZE#*x}" --fps "$FPS" --duration "$DURATION" \
      --out-dir "$OUT_ABS/$tag" \
      --report "$merged" >"$OUT_ABS/$tag.log" 2>&1; then
    log "the profile $name failed; see $OUT_ABS/$tag.log"
    return 1
  fi

  python3 - "$merged" "$name" "$netem_args" >>"$CSV" <<'PY'
import json
import sys

report_path, name, netem = sys.argv[1], sys.argv[2], sys.argv[3]
with open(report_path, encoding="utf-8") as handle:
    report = json.load(handle)
counters = report.get("counters", {})
stages = report.get("stages", {})


def count(name):
    return counters.get(name, 0)


def stage(name, key):
    values = stages.get(name, {})
    return values.get(key, 0)


def p99_ms(name):
    return stage(name, "p99_us") / 1000.0


print(
    ",".join(
        [
            name,
            netem.replace(" ", "_"),
            str(count("frames_sent")),
            str(count("frames_decoded")),
            str(count("frames_presented")),
            str(count("dropped_frames")),
            str(count("decode_errors")),
            str(count("frames_rebuilt")),
            f'{p99_ms("decode"):.3f}',
            f'{p99_ms("end_to_end"):.3f}',
            f'{report.get("bitrate_kbps", 0.0):.3f}',
            str(counters.get("final_picture", "")),
            str(counters.get("quality_steps", 0)),
        ]
    )
)
PY
}

# ------------------------------------------------------------------ run
log "the profiles:"
printf '%s\n' "$PROFILES" | while read -r line; do log "  $line"; done

printf '%s\n' "$CSV_HEADER" >"$CSV"
# The profiles are read into a list first: a pipeline would run the loop in a
# subshell, where the counter does not survive.
profile_list=()
while IFS= read -r profile; do
  [ -z "$profile" ] && continue
  profile_list+=("$profile")
done <<<"$PROFILES"

index=0
for profile in "${profile_list[@]}"; do
  name="${profile%%:*}"
  netem_args="${profile#*:}"
  if ! run_profile "$name" "$netem_args" "$index"; then
    exit 1
  fi
  index=$((index + 1))
done

log "the matrix is in $CSV"
cat "$CSV"
