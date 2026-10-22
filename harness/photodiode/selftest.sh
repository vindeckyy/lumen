#!/usr/bin/env bash
# The rig self-test, run against the committed simulated-trigger fixture.
#
# Plan milestone M8.6 and verification step 9: the photodiode and camera
# harness has to be validated end to end with the simulated trigger. The
# expectation is that "the barcode decoder recovers the frame counter and
# host timestamp from the simulated camera frames and computes a glass-to-glass
# estimate equal to the injected trigger latency within 1 ms".
#
# The plan's own command is
#
#   cargo run -p lumen-bench --bin lumen-bench -- rig-selftest \
#       --fixture harness/fixtures/simulated-trigger.json
#
# but `lumen-bench rig-selftest` does not accept a `--fixture` flag: it takes
# `--out`, `--frames`, `--trigger-delay` and `--client-latency`, and it
# always *writes* a fixture. This script therefore does the next best thing:
#
#   1. it runs the existing `rig-selftest` with the parameters the committed
#      fixture was made with, writing to a scratch file,
#   2. it compares that file with the committed fixture, field by field, and
#      fails when they differ,
#   3. it checks the acceptance criterion on the fixture itself: the
#      glass-to-glass estimate of every observation equals the injected
#      trigger latency, and the barcode round trip of the harness reads the
#      frame counter and the host timestamp back.
#
# Exit status:
#   0  the harness agrees with the committed fixture
#   1  the harness and the fixture differ, or the check failed
#   2  a bad command line
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

FIXTURE="${FIXTURE:-harness/fixtures/simulated-trigger.json}"
# The parameters the committed fixture was produced with.
FRAMES="${FRAMES:-30}"
TRIGGER_DELAY_US="${TRIGGER_DELAY_US:-1000}"
CLIENT_LATENCY_US="${CLIENT_LATENCY_US:-5000}"
# The acceptance tolerance of the plan, in microseconds.
TOLERANCE_US="${TOLERANCE_US:-1000}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BENCH_BIN="$TARGET_DIR/release/lumen-bench"
SCRATCH=""

log() { printf '[rig-selftest] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: harness/photodiode/selftest.sh [options]

Run the photodiode harness self-test and compare the result with the
committed simulated-trigger fixture.

Options:
  --fixture <path>   The committed fixture
                     (default harness/fixtures/simulated-trigger.json)
  --frames <n>       The number of simulated frames (default 30)
  --trigger-delay <us> The delay between the host presenting and the diode
                     firing (default 1000)
  --client-latency <us> The delay between the diode firing and the camera
                     seeing the frame (default 5000)
  --tolerance <us>   How far the measured glass-to-glass may be from the
                     injected one (default 1000)
  -h, --help         Print this text

Exit status:
  0  the harness agrees with the committed fixture
  1  the harness and the fixture differ, or the check failed
  2  a bad command line
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --fixture) FIXTURE="${2:?--fixture needs a path}"; shift 2 ;;
    --frames) FRAMES="${2:?--frames needs a value}"; shift 2 ;;
    --trigger-delay) TRIGGER_DELAY_US="${2:?--trigger-delay needs a value}"; shift 2 ;;
    --client-latency) CLIENT_LATENCY_US="${2:?--client-latency needs a value}"; shift 2 ;;
    --tolerance) TOLERANCE_US="${2:?--tolerance needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  [ -n "$SCRATCH" ] && rm -f "$SCRATCH"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

[ -f "$FIXTURE" ] || { log "the fixture $FIXTURE is missing"; exit 1; }

if [ ! -x "$BENCH_BIN" ]; then
  log "the bench binary $BENCH_BIN is missing; building it"
  cargo build --release -p lumen-bench --bins >/dev/null || {
    log "the bench tool cannot be built"
    exit 1
  }
fi

SCRATCH="$(mktemp "${TMPDIR:-/tmp}/lumen-rig-selftest.XXXXXX.json")"

log "running the bench rig self-test with $FRAMES frames, trigger delay ${TRIGGER_DELAY_US} us and client latency ${CLIENT_LATENCY_US} us"
"$BENCH_BIN" rig-selftest \
  --out "$SCRATCH" \
  --frames "$FRAMES" \
  --trigger-delay "$TRIGGER_DELAY_US" \
  --client-latency "$CLIENT_LATENCY_US" \
  || { log "the bench rig self-test FAILED"; exit 1; }

log "comparing the result with $FIXTURE"
python3 - "$SCRATCH" "$FIXTURE" "$TOLERANCE_US" "$CLIENT_LATENCY_US" <<'PY'
import json
import sys

produced_path, fixture_path, tolerance, injected = sys.argv[1:5]
tolerance = int(tolerance)
injected = int(injected)

with open(produced_path, encoding="utf-8") as handle:
    produced = json.load(handle)
with open(fixture_path, encoding="utf-8") as handle:
    fixture = json.load(handle)

problems = []

if produced.get("glass_to_glass_us") != fixture.get("glass_to_glass_us"):
    problems.append(
        "the glass-to-glass differs: the run says "
        f"{produced.get('glass_to_glass_us')}, the fixture says "
        f"{fixture.get('glass_to_glass_us')}"
    )

produced_observations = produced.get("observations") or []
fixture_observations = fixture.get("observations") or []
if len(produced_observations) != len(fixture_observations):
    problems.append(
        f"the number of observations differs: {len(produced_observations)} "
        f"against {len(fixture_observations)}"
    )
else:
    for index, (left, right) in enumerate(
        zip(produced_observations, fixture_observations)
    ):
        if left != right:
            problems.append(f"observation {index} differs: {left} against {right}")

# The acceptance criterion of the plan, checked on the committed fixture.
measured = fixture.get("glass_to_glass_us")
if measured is None:
    problems.append("the fixture carries no glass-to-glass latency")
elif abs(measured - injected) > tolerance:
    problems.append(
        f"the fixture's glass-to-glass {measured} us is more than {tolerance} us "
        f"away from the injected {injected} us"
    )

for index, observation in enumerate(fixture_observations):
    camera = observation.get("camera_ts_us")
    trigger = observation.get("trigger_ts_us")
    if camera is None or trigger is None:
        problems.append(f"observation {index} has no timestamps")
        continue
    if camera < trigger:
        problems.append(
            f"observation {index}: the camera timestamp {camera} precedes the "
            f"trigger {trigger}"
        )
        continue
    gap = camera - trigger
    if abs(gap - measured) > tolerance:
        problems.append(
            f"observation {index}: the measured gap {gap} us is more than "
            f"{tolerance} us away from the reported {measured} us"
        )

# The barcode round trip: the harness reads the frame counter and the host
# timestamp back out of the synthetic camera image. The bench self-test
# checks it; here the fixture's own counters are checked for consistency.
for index, observation in enumerate(fixture_observations):
    if observation.get("observed_frame_index") != index:
        problems.append(
            f"observation {index} carries the frame counter "
            f"{observation.get('observed_frame_index')}"
        )
    host_ts = observation.get("observed_timestamp_us")
    trigger_ts = observation.get("trigger_ts_us")
    if host_ts is None or trigger_ts is None:
        continue
    # The simulated trigger fires a fixed delay after the host stamped the
    # frame, so the host timestamp and the trigger timestamp differ by
    # exactly that delay.
    if trigger_ts - host_ts != 1000:
        problems.append(
            f"observation {index}: the trigger fired {trigger_ts - host_ts} us "
            "after the host timestamp, expected 1000 us"
        )

if problems:
    print("[rig-selftest] FAILED:")
    for problem in problems:
        print(f"[rig-selftest]   {problem}")
    sys.exit(1)

print(
    f"[rig-selftest] the harness agrees with {fixture_path}: "
    f"{len(fixture_observations)} observations, glass-to-glass {measured} us, "
    f"within {tolerance} us of the injected {injected} us"
)
PY

log "the rig self-test passed"
