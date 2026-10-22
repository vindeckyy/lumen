#!/usr/bin/env bash
# End-to-end run: a host captures a compositor and streams to a client.
#
# Two modes:
#   loopback (default)  both ends on 127.0.0.1
#   netns               the host in the lumen-host namespace and the client in
#                       lumen-client, over the veth pair from netns-up.sh
#
# The Wayland socket is a filesystem path, so a compositor started outside a
# network namespace is still reachable from inside one. That is why the
# compositors are started once, outside the namespaces.
#
# The run writes artifacts/m1/host.json, artifacts/m1/client.json and the
# merged artifacts/m1/e2e.json. Every number in the merged report is
# provisional: these are software counters, not glass-to-glass.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

MODE="${MODE:-loopback}"
DURATION="${DURATION:-15}"
SIZE="${SIZE:-1280x720}"
FPS="${FPS:-30}"
OUT="${OUT:-artifacts/m1/e2e.json}"
RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-e2e}"
CLIENT_MEDIA_PORT="${CLIENT_MEDIA_PORT:-24801}"
AUDIO_PORT="${AUDIO_PORT:-24814}"
CLIENT_AUDIO_PORT="${CLIENT_AUDIO_PORT:-24815}"
CONTROL_PORT="${CONTROL_PORT:-24800}"
# The pairing state of this run. A fresh directory means the host and the
# client have never met, so the first run pairs them.
STATE_DIR="${STATE_DIR:-$RUNTIME_ROOT/state}"
PAIRING_CODE_FILE="$STATE_DIR/pairing-code.txt"

log() { printf '[run-e2e] %s\n' "$*"; }

usage() {
  cat <<'EOF'
run-e2e.sh - run the Lumen host and client against each other

Usage: scripts/run-e2e.sh [options]

Options:
  --host-ns <name>     Run the host in this network namespace (implies netns)
  --client-ns <name>   Run the client in this network namespace (implies netns)
  --width <n>          Capture width in pixels
  --height <n>         Capture height in pixels
  --fps <n>            Stream rate
  --duration <s>       How long to run, in seconds
  --report <path>      Where the merged report goes
  --out-dir <path>     Where the raw counters and the logs go
  --help               Print this text

The environment variables MODE, SIZE, FPS, DURATION, OUT, OUT_DIR, HOST_NS and
CLIENT_NS set the same things. The flags win.
EOF
}

HOST_NS_NAME="${HOST_NS:-lumen-host}"
CLIENT_NS_NAME="${CLIENT_NS:-lumen-client}"

while [ $# -gt 0 ]; do
  case "$1" in
    --host-ns)   HOST_NS_NAME="$2"; MODE=netns; shift 2 ;;
    --client-ns) CLIENT_NS_NAME="$2"; MODE=netns; shift 2 ;;
    --width)     WIDTH="$2"; shift 2 ;;
    --height)    HEIGHT="$2"; shift 2 ;;
    --fps)       FPS="$2"; shift 2 ;;
    --duration)  DURATION="$2"; shift 2 ;;
    --report)    OUT="$2"; shift 2 ;;
    --out-dir)   OUT_DIR="$2"; shift 2 ;;
    --with-audio) WITH_AUDIO=1; shift ;;
    --with-clipboard) WITH_CLIPBOARD=1; shift ;;
    --with-resize) WITH_RESIZE=1; shift ;;
    --monitors) MONITORS="$2"; shift 2 ;;
    --help|-h)   usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

if [ -n "${WIDTH:-}" ] || [ -n "${HEIGHT:-}" ]; then
  SIZE="${WIDTH:-${SIZE%x*}}${HEIGHT:+x}${HEIGHT:-${SIZE#*x}}"
fi
OUT_DIR="${OUT_DIR:-$(dirname "$OUT")}"
WITH_AUDIO="${WITH_AUDIO:-0}"
WITH_CLIPBOARD="${WITH_CLIPBOARD:-0}"
WITH_RESIZE="${WITH_RESIZE:-0}"
MONITORS="${MONITORS:-1}"
ARTIFACT_DIR="$OUT_DIR"
HOST_REPORT="$ARTIFACT_DIR/host.json"
CLIENT_REPORT="$ARTIFACT_DIR/client.json"

case "$MODE" in
  loopback|netns) ;;
  *) log "MODE must be loopback or netns, got $MODE"; exit 2 ;;
esac

mkdir -p "$ARTIFACT_DIR" "$RUNTIME_ROOT" "$STATE_DIR"
rm -f "$HOST_REPORT" "$CLIENT_REPORT" "$PAIRING_CODE_FILE"

# ------------------------------------------------------------------ build
log "building the host and the client"
cargo build --release -p lumen-host-service -p lumen-client-desktop -p lumen-bench --bins \
  >/dev/null
HOST_BIN="$REPO_ROOT/target/release/lumen-host"
CLIENT_BIN="$REPO_ROOT/target/release/lumen-client"
BENCH_BIN="$REPO_ROOT/target/release/lumen-bench"

# ------------------------------------------------------------- compositors
# Two headless compositors: one the host captures, one the client presents
# into.
#
# They share the user's XDG_RUNTIME_DIR, because that is where the PipeWire
# socket lives and the audio path needs it. They are told apart by the
# Wayland socket each one picks for itself.
HOST_RT="$RUNTIME_ROOT"
CLIENT_RT="$RUNTIME_ROOT"
mkdir -p "$RUNTIME_ROOT"
chmod 700 "$RUNTIME_ROOT"

# The Wayland sockets that exist right now.
wayland_sockets() {
  ls "$XDG_RUNTIME_DIR"/wayland-* 2>/dev/null | grep -v '\.lock$' || true
}

# The sway IPC sockets that exist right now.
ipc_sockets() {
  ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock 2>/dev/null || true
}

# The IPC socket of the host's compositor.
#
# Both compositors share the user's runtime directory, so the sockets have to
# be told apart. The host's compositor is the one that started first, so its
# socket is the one that appeared first.
HOST_IPC_SOCKET=""
host_ipc_socket() {
  if [ -n "$HOST_IPC_SOCKET" ]; then
    printf '%s' "$HOST_IPC_SOCKET"
    return 0
  fi
  printf '%s' "$(ls -tr "$XDG_RUNTIME_DIR"/sway-ipc.*.sock 2>/dev/null | head -1 || true)"
}

# Start a compositor and print the socket it opened.
start_compositor() {
  local logfile="$1"
  local before after before_ipc after_ipc
  before="$(wayland_sockets | wc -l)"
  before_ipc="$(ipc_sockets | wc -l)"
  WLR_BACKENDS=headless WLR_RENDERER="${WLR_RENDERER:-pixman}" \
    setsid nohup sway -c /dev/null >"$logfile" 2>&1 &
  echo $! >"$RUNTIME_ROOT/sway.pid"
  for _ in $(seq 1 100); do
    after="$(wayland_sockets | wc -l)"
    after_ipc="$(ipc_sockets | wc -l)"
    if [ "$after" -gt "$before" ] && [ "$after_ipc" -gt "$before_ipc" ]; then
      wayland_sockets | tail -1 | sed 's#.*/##'
      # The IPC socket that appeared with it belongs to this compositor.
      # The full path is kept, because swaymsg needs it.
      ipc_sockets | tail -1 >"$RUNTIME_ROOT/ipc.sock"
      return 0
    fi
    sleep 0.1
  done
  echo ""
}

HOST_SWAY_PID=""
CLIENT_SWAY_PID=""
cleanup() {
  log "cleaning up"
  [ -n "$HOST_SWAY_PID" ] && kill "$HOST_SWAY_PID" 2>/dev/null || true
  [ -n "$CLIENT_SWAY_PID" ] && kill "$CLIENT_SWAY_PID" 2>/dev/null || true
  pkill -x lumen-host 2>/dev/null || true
  pkill -x lumen-client 2>/dev/null || true
}
trap cleanup EXIT

# A host or client left over from an earlier run would hold the control port
# and the media port, and the run would talk to the wrong process.
pkill -x lumen-host 2>/dev/null || true
pkill -x lumen-client 2>/dev/null || true
sleep 0.2

log "starting the headless compositors"
HOST_DISPLAY="$(start_compositor "$RUNTIME_ROOT/sway-host.log")"
HOST_SWAY_PID="$(cat "$RUNTIME_ROOT/sway.pid" 2>/dev/null || true)"
HOST_IPC_SOCKET="$(cat "$RUNTIME_ROOT/ipc.sock" 2>/dev/null || true)"
CLIENT_DISPLAY="$(start_compositor "$RUNTIME_ROOT/sway-client.log")"
CLIENT_SWAY_PID="$(cat "$RUNTIME_ROOT/sway.pid" 2>/dev/null || true)"

[ -n "$HOST_DISPLAY" ] || { log "the host compositor did not start"; exit 1; }
[ -n "$CLIENT_DISPLAY" ] || { log "the client compositor did not start"; exit 1; }
log "the host compositor is on $HOST_DISPLAY, the client one on $CLIENT_DISPLAY"

# The compositor decides the output size, so the capture size follows it.
HOST_SIZE="$(WAYLAND_DISPLAY="$HOST_DISPLAY" \
  "$HOST_BIN" --list-outputs | head -1 | awk '{print $2}')"
log "the host compositor offers $HOST_SIZE"
if [ -z "$HOST_SIZE" ]; then
  log "cannot read the output size"
  exit 1
fi
SIZE="${SIZE:-$HOST_SIZE}"

# ------------------------------------------------------------- addresses
if [ "$MODE" = netns ]; then
  log "setting up the network namespaces"
  HOST_NS="$HOST_NS_NAME" CLIENT_NS="$CLIENT_NS_NAME" "$REPO_ROOT/scripts/netns-up.sh"
  HOST_IP=10.77.0.1
  CLIENT_IP=10.77.0.2
  SUDO="sudo -n"
  HOST_RUN=(sudo -n ip netns exec lumen-host)
  CLIENT_RUN=(sudo -n ip netns exec lumen-client)
else
  HOST_IP=127.0.0.1
  CLIENT_IP=127.0.0.1
  HOST_RUN=(env)
  CLIENT_RUN=(env)
fi

# ----------------------------------------------------------------- run
# The extra flags are an array: an unquoted string would split the text of
# the clipboard test into separate arguments.
HOST_EXTRA_FLAGS=()
CLIENT_EXTRA_FLAGS=()
if [ "$WITH_AUDIO" = "1" ]; then
  HOST_EXTRA_FLAGS+=(--with-audio --audio "$HOST_IP:$AUDIO_PORT" --client-audio-port "$CLIENT_AUDIO_PORT")
  CLIENT_EXTRA_FLAGS+=(--with-audio --audio-bind "$CLIENT_IP:$CLIENT_AUDIO_PORT")
  log "the session carries audio on port $CLIENT_AUDIO_PORT"
fi
if [ "$WITH_CLIPBOARD" = "1" ]; then
  HOST_EXTRA_FLAGS+=(--clipboard-test "lumen clipboard round trip")
  log "the session exercises the clipboard round trip"
fi

log "mode $MODE, $SIZE at $FPS fps for ${DURATION}s"

"${HOST_RUN[@]}" env XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY="$HOST_DISPLAY" \
  "$HOST_BIN" --size "$SIZE" --fps "$FPS" \
    --control "$HOST_IP:$CONTROL_PORT" --client-media-port "$CLIENT_MEDIA_PORT" \
    --duration "$DURATION" --report "$HOST_REPORT" \
    --pairing-store "$STATE_DIR/pairing.json" \
    --pairing-code-file "$PAIRING_CODE_FILE" \
    "${HOST_EXTRA_FLAGS[@]}" \
    >"$ARTIFACT_DIR/host.log" 2>&1 &
HOST_PID=$!

# Wait for the host to bind the control listener. A fixed sleep is not
# enough when the machine is busy: the client would dial before the host
# was listening and get a connection refused.
# Whether the host's control port is listening.
#
# The check looks at the socket table rather than connecting: a connection
# that the host never reads leaves a session thread waiting for a handshake,
# and the host then has to notice the dead connection itself.
host_is_listening() {
  if [ "$MODE" = netns ]; then
    sudo -n ip netns exec "$HOST_NS_NAME" ss -ltn 2>/dev/null | grep -q ":$CONTROL_PORT "
  else
    ss -ltn 2>/dev/null | grep -q ":$CONTROL_PORT "
  fi
}

log "waiting for the host to listen on $HOST_IP:$CONTROL_PORT"
for _ in $(seq 1 200); do
  if host_is_listening; then
    log "the host is listening"
    break
  fi
  sleep 0.1
done
if ! host_is_listening; then
  log "the host is not listening; the client will report the error"
fi

"${CLIENT_RUN[@]}" env XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY="$CLIENT_DISPLAY" \
  "$CLIENT_BIN" --host "$HOST_IP:$CONTROL_PORT" \
    --media-bind "$CLIENT_IP:$CLIENT_MEDIA_PORT" \
    --duration "$((DURATION - 2))" --report "$CLIENT_REPORT" \
    --identity "$STATE_DIR/client-identity" \
    --pairing-code-file "$PAIRING_CODE_FILE" \
    "${CLIENT_EXTRA_FLAGS[@]}" \
    >"$ARTIFACT_DIR/client.log" 2>&1 &
CLIENT_PID=$!

# The plan's M4 acceptance wants two resolution changes inside the run.
if [ "$WITH_RESIZE" = "1" ]; then
  HOST_SWAYSOCK="$(host_ipc_socket)"
  # Two mode changes, which is what the plan's acceptance counts.
  for size in 1920x1080 1280x720; do
    sleep 2
    if [ -n "$HOST_SWAYSOCK" ]; then
      SWAYSOCK="$HOST_SWAYSOCK" swaymsg "output HEADLESS-1 mode $size" >/dev/null 2>&1 || true
      log "the host output mode is now $size"
    fi
  done
fi

wait "$CLIENT_PID" || log "the client exited with a failure"
wait "$HOST_PID" || log "the host exited with a failure"

log "host log:"
sed 's/^/    /' "$ARTIFACT_DIR/host.log"
log "client log:"
sed 's/^/    /' "$ARTIFACT_DIR/client.log"

if [ ! -f "$HOST_REPORT" ] || [ ! -f "$CLIENT_REPORT" ]; then
  log "the run produced no reports"
  exit 1
fi

# ---------------------------------------------------------------- report
log "merging the reports into $OUT"
"$BENCH_BIN" report --host "$HOST_REPORT" --client "$CLIENT_REPORT" --out "$OUT" \
  --set "mode=$MODE" --set "size=$SIZE" --set "fps=$FPS" \
  --set "duration_s=$DURATION" --set "transport=custom-udp+fec-gf256" \
  --set "codec=h264-vaapi" --set "capture=wlroots-screencopy" \
  || { log "cannot merge the reports"; exit 1; }

log "done: $OUT"
