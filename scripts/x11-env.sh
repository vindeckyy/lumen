#!/usr/bin/env bash
# The X11 test environment: an Xvfb display and an Xorg dummy display.
#
# Plan milestone M7.5: "X11 test environment: Xvfb plus Xorg dummy driver for
# host and client displays".
#
# Two ways of getting an X server that needs no screen:
#
#   Xvfb          a framebuffer in user space. The server picks its own
#                 display number, so nothing collides with another server on
#                 the machine. The resolution is exact, the refresh rate is
#                 whatever the server reports.
#   Xorg + dummy  the real X server with the dummy video driver. It needs
#                 root and its own configuration, and the mode comes from a
#                 Modeline. It is the only one of the two that behaves like a
#                 real X server with a real driver.
#
# The script starts the servers, writes a JSON file with the display numbers
# and the process ids, and leaves them running. `stop` reads that file and
# ends them again.
#
#   scripts/x11-env.sh start --json artifacts/m7/x11-env.json
#   scripts/x11-env.sh stop  --json artifacts/m7/x11-env.json
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

RUNTIME_ROOT="${RUNTIME_ROOT:-/tmp/lumen-x11-env}"
JSON="${JSON:-artifacts/m7/x11-env.json}"
XVFB_SCREEN="${XVFB_SCREEN:-1920x1080x24}"
DUMMY_SIZE="${DUMMY_SIZE:-1920x1080}"
DUMMY_REFRESH="${DUMMY_REFRESH:-60}"
START_XORG="${START_XORG:-0}"
COMMAND="start"

log() { printf '[x11-env] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: scripts/x11-env.sh <start|stop> [options]

Starts and stops the X11 test environment of plan milestone M7.5.

Options:
  --json <path>        Where the environment is described (default artifacts/m7/x11-env.json)
  --screen <WxHxD>     The Xvfb screen geometry (default 1920x1080x24)
  --dummy-size <WxH>   The size of the Xorg dummy output (default 1920x1080)
  --dummy-refresh <n>  The refresh rate of the Xorg dummy output (default 60)
  --with-xorg          Also start an Xorg server with the dummy video driver
  --runtime <path>     Where the runtime files go (default /tmp/lumen-x11-env)
  -h, --help           Print this text

The "start" command leaves the servers running and writes the JSON file.
The "stop" command ends the servers the JSON file names.

The Xvfb server is started with -displayfd, so the server picks a free
display number itself and no number is ever guessed.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    start|stop) COMMAND="$1"; shift ;;
    --json) JSON="${2:?--json needs a value}"; shift 2 ;;
    --screen) XVFB_SCREEN="${2:?--screen needs a value}"; shift 2 ;;
    --dummy-size) DUMMY_SIZE="${2:?--dummy-size needs a value}"; shift 2 ;;
    --dummy-refresh) DUMMY_REFRESH="${2:?--dummy-refresh needs a value}"; shift 2 ;;
    --with-xorg) START_XORG=1; shift ;;
    --runtime) RUNTIME_ROOT="${2:?--runtime needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

case "$JSON" in
  /*) JSON_ABS="$JSON" ;;
  *) JSON_ABS="$REPO_ROOT/$JSON" ;;
esac
mkdir -p "$(dirname "$JSON_ABS")" "$RUNTIME_ROOT"

# --------------------------------------------------------------- the servers
# The display number is read from the file descriptor the server writes it
# to, so the server picks a free number and nothing collides.
start_xvfb() {
  local name="$1" screen="$2"
  local fd_file="$RUNTIME_ROOT/$name.display"
  rm -f "$fd_file"
  # The server writes the display number to the file descriptor 3 and keeps
  # it open, so the number is read from the file.
  ( Xvfb -displayfd 3 -screen 0 "$screen" -nolisten tcp 3>"$fd_file" \
      >"$RUNTIME_ROOT/$name.log" 2>&1 & echo $! >"$RUNTIME_ROOT/$name.pid" )
  local waited=0
  while [ ! -s "$fd_file" ]; do
    sleep 0.1
    waited=$((waited + 1))
    if [ "$waited" -gt 100 ]; then
      log "the $name server did not report a display number"
      sed 's/^/    /' "$RUNTIME_ROOT/$name.log" 2>/dev/null || true
      return 1
    fi
  done
  local number
  number="$(tr -d '[:space:]' <"$fd_file")"
  if [ -z "$number" ]; then
    log "the $name server reported an empty display number"
    return 1
  fi
  printf ':%s' "$number"
}

start_xorg() {
  local name="$1"
  local config="$RUNTIME_ROOT/$name.conf"
  local number
  number="$(pick_free_display)"
  # The dummy driver of xorg-server 21.1 has a fixed list of modes and
  # offers neither the "VideoOutput" nor a custom size, so the server is
  # started with the plain configuration and the size it comes up with is
  # measured afterwards.
  cat >"$config" <<EOF
Section "ServerLayout"
  Identifier "LumenLayout"
EndSection

Section "Device"
  Identifier "LumenDummy"
  Driver "dummy"
EndSection

Section "Monitor"
  Identifier "LumenMonitor"
  HorizSync 30.0-80.0
  VertRefresh 30.0-60.0
  Modeline "${DUMMY_SIZE}_60.00"  148.50  1920 2008 2052 2184  1080 1083 1088 1127 -hsync +vsync
EndSection

Section "Screen"
  Identifier "LumenScreen"
  Device "LumenDummy"
  Monitor "LumenMonitor"
  DefaultDepth 24
  SubSection "Display"
    Depth 24
  EndSubSection
EndSection

Section "ServerFlags"
  Option "AutoAddGPU" "off"
  Option "DontZap" "true"
  Option "AllowEmptyInitialConfiguration" "true"
EndSection
EOF
  # The real X server refuses to run for an ordinary user, so it runs as
  # root. This is the one option of the three that needs the privilege.
  local run=()
  if [ "$(id -u)" -ne 0 ]; then
    if ! sudo -n true 2>/dev/null; then
      log "the Xorg server needs root and sudo does not work here"
      return 1
    fi
    run=(sudo -n)
  fi
  ( "${run[@]}" Xorg ":$number" -config "$config" -nolisten tcp \
      >"$RUNTIME_ROOT/$name.log" 2>&1 & echo $! >"$RUNTIME_ROOT/$name.pid" )
  local waited=0
  while [ "$waited" -lt 100 ]; do
    if [ -e "/tmp/.X11-unix/X$number" ]; then
      printf ':%s' "$number"
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  log "the Xorg server did not open its socket"
  sed 's/^/    /' "$RUNTIME_ROOT/$name.log" 2>/dev/null || true
  return 1
}

# A free display number for the Xorg server, which cannot report its own.
pick_free_display() {
  local number=90
  while [ "$number" -lt 200 ]; do
    if [ ! -e "/tmp/.X11-unix/X$number" ] && [ ! -e "/tmp/.X"$number"-lock" ]; then
      printf '%s' "$number"
      return 0
    fi
    number=$((number + 1))
  done
  log "no free display number for the Xorg server"
  return 1
}

write_json() {
  local xvfb_display="$1" xvfb_pid="$2" xorg_display="$3" xorg_pid="$4"
  local xvfb_geometry xorg_geometry
  xvfb_geometry="$(display_geometry "$xvfb_display")"
  xorg_geometry="$(display_geometry "$xorg_display")"
  python3 - "$JSON_ABS" "$xvfb_display" "$xvfb_pid" "$xvfb_geometry" \
      "$xorg_display" "$xorg_pid" "$xorg_geometry" <<'PY'
import json
import sys

path, xvfb_display, xvfb_pid, xvfb_geometry, xorg_display, xorg_pid, xorg_geometry = sys.argv[1:8]
report = {
    "xvfb": {
        "display": xvfb_display,
        "pid": int(xvfb_pid),
        "geometry": xvfb_geometry,
    },
    "xorg_dummy": {
        "display": xorg_display,
        "pid": int(xorg_pid) if xorg_pid else None,
        "geometry": xorg_geometry,
    },
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
}

# The size of a display, as "WxH". Empty when the display cannot be reached.
display_geometry() {
  local display="$1"
  [ -n "$display" ] || return 0
  if command -v xrandr >/dev/null 2>&1; then
    DISPLAY="$display" xrandr 2>/dev/null |
      sed -n 's/.*current \([0-9]*\) x \([0-9]*\).*/\1x\2/p' |
      head -1
  fi
}

# -------------------------------------------------------------------- start
if [ "$COMMAND" = start ]; then
  command -v Xvfb >/dev/null 2>&1 || { log "Xvfb is not installed"; exit 1; }
  log "starting the Xvfb display with the screen $XVFB_SCREEN"
  XVFB_DISPLAY="$(start_xvfb xvfb "$XVFB_SCREEN")"
  XVFB_PID="$(cat "$RUNTIME_ROOT/xvfb.pid")"
  log "the Xvfb display is $XVFB_DISPLAY"

  XORG_DISPLAY=""
  XORG_PID=""
  if [ "$START_XORG" = 1 ]; then
    if [ ! -e /usr/lib/xorg/modules/drivers/dummy_drv.so ]; then
      log "the Xorg dummy driver is not installed; skipping the Xorg server"
    else
      log "starting the Xorg server with the dummy video driver"
      XORG_DISPLAY="$(start_xorg xorg)"
      XORG_PID="$(cat "$RUNTIME_ROOT/xorg.pid")"
      log "the Xorg display is $XORG_DISPLAY"
    fi
  fi

  write_json "$XVFB_DISPLAY" "$XVFB_PID" "$XORG_DISPLAY" "$XORG_PID"
  log "the environment is described in $JSON"
  exit 0
fi

# --------------------------------------------------------------------- stop
if [ ! -f "$JSON_ABS" ]; then
  log "there is no environment file at $JSON"
  exit 1
fi
read -r XVFB_PID XORG_PID < <(python3 - "$JSON_ABS" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    report = json.load(handle)
print(report["xvfb"]["pid"], report["xorg_dummy"]["pid"] or 0)
PY
)
if [ -n "$XVFB_PID" ] && [ "$XVFB_PID" != 0 ]; then
  log "ending the Xvfb server $XVFB_PID"
  kill "$XVFB_PID" 2>/dev/null || true
fi
if [ -n "$XORG_PID" ] && [ "$XORG_PID" != 0 ]; then
  log "ending the Xorg server $XORG_PID"
  kill "$XORG_PID" 2>/dev/null || true
fi
rm -f "$JSON_ABS"
log "the environment is gone"
