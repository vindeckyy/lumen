#!/usr/bin/env bash
# The X11 end-to-end run: an X11 host and an X11 client.
#
# Plan milestone M7: "DISPLAY=:77 scripts/run-e2e.sh --x11 --duration 60
# --report artifacts/m7/e2e-x11.json". The main script cannot be changed
# here, so this script does the X11 variant and writes the same report.
#
# The matrix of the plan has four rows. This script runs the X11/X11 row by
# default and the other rows with --matrix:
#
#   wayland/wayland   the compositor is captured and the client presents
#                     into a compositor
#   wayland/x11       the compositor is captured and the client presents
#                     into an X server
#   x11/wayland       an X server is captured and the client presents into
#                     a compositor
#   x11/x11           an X server is captured and the client presents into
#                     an X server
#
# The X11 capture is gated by XDamage, so a screen that never changes
# produces no frames. The host display therefore gets a painter
# (lumen-x11-source), which changes the root window at the frame rate.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

DURATION="${DURATION:-60}"
OUT="${OUT:-artifacts/m7/e2e-x11.json}"
MATRIX="${MATRIX:-x11/x11}"
FPS="${FPS:-30}"
SIZE="${SIZE:-1280x720}"
CONTROL_PORT="${CONTROL_PORT:-24860}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24861}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-e2e-x11}"

log() { printf '[run-e2e-x11] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: scripts/run-e2e-x11.sh [options]

Runs the X11 end-to-end acceptance of plan milestone M7.

Options:
  --matrix <a/b>    The row of the backend matrix to run:
                    wayland/wayland, wayland/x11, x11/wayland or x11/x11
                    (default x11/x11)
  --duration <s>    How long to run, in seconds (default 60)
  --report <path>   Where the merged report goes (default artifacts/m7/e2e-x11.json)
  --size <WxH>      The size of the X display (default 1280x720)
  --fps <n>         Stream rate (default 30)
  -h, --help        Print this text

Environment:
  DURATION, OUT, MATRIX, FPS, SIZE, CONTROL_PORT, CLIENT_MEDIA_PORT,
  RUNTIME_ROOT
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --matrix) MATRIX="${2:?--matrix needs a value}"; shift 2 ;;
    --duration) DURATION="${2:?--duration needs a value}"; shift 2 ;;
    --report) OUT="${2:?--report needs a value}"; shift 2 ;;
    --size) SIZE="${2:?--size needs a value}"; shift 2 ;;
    --fps) FPS="${2:?--fps needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

case "$MATRIX" in
  wayland/wayland|wayland/x11|x11/wayland|x11/x11) ;;
  *) log "the matrix must be wayland/wayland, wayland/x11, x11/wayland or x11/x11"; exit 2 ;;
esac
CAPTURE="${MATRIX%%/*}"
PRESENT="${MATRIX##*/}"

case "$OUT" in
  /*) OUT_ABS="$OUT" ;;
  *) OUT_ABS="$REPO_ROOT/$OUT" ;;
esac
OUT_DIR="$(dirname "$OUT_ABS")"
mkdir -p "$OUT_DIR" "$RUNTIME_ROOT"

case "$DURATION" in
  ''|*[!0-9]*) log "DURATION must be a number of seconds, got $DURATION"; exit 2 ;;
esac
command -v python3 >/dev/null 2>&1 || { log "python3 is needed to read the reports"; exit 1; }

# ------------------------------------------------------------------ build
log "building the host, the client and the bench tool"
cargo build --release -p lumen-host-service -p lumen-client-desktop -p lumen-bench --bins \
  >/dev/null
HOST_BIN="$REPO_ROOT/target/release/lumen-host"
CLIENT_BIN="$REPO_ROOT/target/release/lumen-client"
SOURCE_BIN="$REPO_ROOT/target/release/lumen-x11-source"
BENCH_BIN="$REPO_ROOT/target/release/lumen-bench"

# The other agents run the same binaries and end them with a pkill on the
# path. The copies below have a name of their own, so a run of another
# agent cannot end this one in the middle of a measurement.
cp "$HOST_BIN" "$RUNTIME_ROOT/lumen-host-run"
cp "$CLIENT_BIN" "$RUNTIME_ROOT/lumen-client-run"
cp "$SOURCE_BIN" "$RUNTIME_ROOT/lumen-x11-source-run"
HOST_BIN="$RUNTIME_ROOT/lumen-host-run"
CLIENT_BIN="$RUNTIME_ROOT/lumen-client-run"
SOURCE_BIN="$RUNTIME_ROOT/lumen-x11-source-run"

# ------------------------------------------------------------------- the X11
# The Xvfb servers are started with -displayfd, so the server picks a free
# display number itself and nothing collides.
XVFB_PIDS=()
XORG_PID=""
PAINTER_PID=""
HOST_PID=""
CLIENT_PID=""
SWAY_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  log "cleaning up"
  [ -n "$PAINTER_PID" ] && kill "$PAINTER_PID" 2>/dev/null || true
  [ -n "$CLIENT_PID" ] && kill "$CLIENT_PID" 2>/dev/null || true
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null || true
  [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null || true
  for pid in "${XVFB_PIDS[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  [ -n "$XORG_PID" ] && kill "$XORG_PID" 2>/dev/null || true
  rm -rf "$RUNTIME_ROOT"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

start_xvfb() {
  local name="$1" screen="$2"
  local fd_file="$RUNTIME_ROOT/$name.display"
  rm -f "$fd_file"
  ( Xvfb -displayfd 3 -screen 0 "$screen" -nolisten tcp 3>"$fd_file" \
      >"$RUNTIME_ROOT/$name.log" 2>&1 & echo $! >"$RUNTIME_ROOT/$name.pid" )
  local waited=0
  while [ ! -s "$fd_file" ]; do
    sleep 0.1
    waited=$((waited + 1))
    if [ "$waited" -gt 100 ]; then
      log "the $name server did not report a display number"
      return 1
    fi
  done
  local number
  number="$(tr -d '[:space:]' <"$fd_file")"
  [ -n "$number" ] || return 1
  printf ':%s' "$number"
}

HOST_DISPLAY=""
CLIENT_DISPLAY=""
CLIENT_RT="$RUNTIME_ROOT/client"

if [ "$CAPTURE" = x11 ] || [ "$PRESENT" = x11 ]; then
  command -v Xvfb >/dev/null 2>&1 || { log "Xvfb is not installed"; exit 1; }
  WIDTH="${SIZE%x*}"
  HEIGHT="${SIZE#*x}"
  if [ "$CAPTURE" = x11 ]; then
    log "starting the Xvfb display the host captures"
    HOST_DISPLAY="$(start_xvfb host "${WIDTH}x${HEIGHT}x24")"
    XVFB_PIDS+=("$(cat "$RUNTIME_ROOT/host.pid")")
    log "the host display is $HOST_DISPLAY"
  fi
  if [ "$PRESENT" = x11 ]; then
    log "starting the Xvfb display the client presents into"
    CLIENT_DISPLAY="$(start_xvfb client "${WIDTH}x${HEIGHT}x24")"
    if [ -z "$CLIENT_DISPLAY" ]; then
      CLIENT_DISPLAY="$HOST_DISPLAY"
    else
      XVFB_PIDS+=("$(cat "$RUNTIME_ROOT/client.pid")")
    fi
    log "the client display is $CLIENT_DISPLAY"
  fi
fi

if [ "$CAPTURE" = wayland ] || [ "$PRESENT" = wayland ]; then
  mkdir -p "$CLIENT_RT"
  chmod 700 "$CLIENT_RT"
  log "starting the headless compositor"
  XDG_RUNTIME_DIR="$CLIENT_RT" WLR_BACKENDS=headless WLR_RENDERER=pixman \
    setsid nohup sway -c /dev/null >"$RUNTIME_ROOT/sway.log" 2>&1 &
  SWAY_PID=$!
  for _ in $(seq 1 50); do
    [ -S "$CLIENT_RT/wayland-1" ] && break
    sleep 0.2
  done
  [ -S "$CLIENT_RT/wayland-1" ] || { log "the compositor did not start"; exit 1; }
fi

# The host display needs a changing picture, or the damage gate of the X11
# capture never opens.
if [ -n "$HOST_DISPLAY" ]; then
  log "painting the host display so the capture has something to capture"
  DISPLAY="$HOST_DISPLAY" "$SOURCE_BIN" --display "$HOST_DISPLAY" --fps "$FPS" \
    >"$RUNTIME_ROOT/source.log" 2>&1 &
  PAINTER_PID=$!
  sleep 1
fi

# ------------------------------------------------------------------- run
STATE_DIR="$RUNTIME_ROOT/state"
mkdir -p "$STATE_DIR"
rm -f "$STATE_DIR/pairing-code.txt"

HOST_ENV=(env)
CLIENT_ENV=(env)
if [ -n "$HOST_DISPLAY" ]; then
  HOST_ENV=(env "DISPLAY=$HOST_DISPLAY")
fi
if [ -n "$CLIENT_DISPLAY" ]; then
  CLIENT_ENV=(env "DISPLAY=$CLIENT_DISPLAY")
else
  CLIENT_ENV=(env "XDG_RUNTIME_DIR=$CLIENT_RT" "WAYLAND_DISPLAY=wayland-1")
fi

CAPTURE_FLAG=(--capture wayland)
if [ "$CAPTURE" = x11 ]; then
  CAPTURE_FLAG=(--capture x11 "--display" "$HOST_DISPLAY")
fi
PRESENT_FLAG=(--present wayland)
if [ "$PRESENT" = x11 ]; then
  PRESENT_FLAG=(--present x11)
fi

log "matrix $MATRIX, $SIZE at $FPS fps for ${DURATION}s"
"${HOST_ENV[@]}" "$HOST_BIN" --size "$SIZE" --fps "$FPS" "${CAPTURE_FLAG[@]}" \
  --control "127.0.0.1:$CONTROL_PORT" --client-media-port "$CLIENT_MEDIA_PORT" \
  --duration "$DURATION" --report "$OUT_DIR/host.json" \
  --pairing-store "$STATE_DIR/pairing.json" \
  --pairing-code-file "$STATE_DIR/pairing-code.txt" \
  >"$OUT_DIR/host.log" 2>&1 &
HOST_PID=$!
sleep 1

"${CLIENT_ENV[@]}" "$CLIENT_BIN" --host "127.0.0.1:$CONTROL_PORT" \
  --media-bind "127.0.0.1:$CLIENT_MEDIA_PORT" "${PRESENT_FLAG[@]}" \
  --duration "$((DURATION - 2))" --report "$OUT_DIR/client.json" \
  --identity "$STATE_DIR/client-identity" \
  --pairing-code-file "$STATE_DIR/pairing-code.txt" \
  >"$OUT_DIR/client.log" 2>&1 &
CLIENT_PID=$!

wait "$CLIENT_PID" || log "the client exited with a failure"
CLIENT_PID=""
wait "$HOST_PID" || log "the host exited with a failure"
HOST_PID=""

log "host log:"
sed 's/^/    /' "$OUT_DIR/host.log" | tail -20
log "client log:"
sed 's/^/    /' "$OUT_DIR/client.log" | tail -20

if [ ! -f "$OUT_DIR/host.json" ] || [ ! -f "$OUT_DIR/client.json" ]; then
  log "the run produced no reports"
  exit 1
fi

# ---------------------------------------------------------------- report
log "merging the reports into $OUT"
"$BENCH_BIN" report --host "$OUT_DIR/host.json" --client "$OUT_DIR/client.json" \
  --out "$OUT_ABS" \
  --set "mode=loopback" --set "matrix=$MATRIX" --set "size=$SIZE" --set "fps=$FPS" \
  --set "duration_s=$DURATION" --set "transport=custom-udp+fec-gf256" \
  --set "codec=h264-vaapi" --set "capture=$CAPTURE" --set "present=$PRESENT" \
  >/dev/null || { log "cannot merge the reports"; exit 1; }

log "done: $OUT"
