#!/usr/bin/env bash
# Tear down the lumen network namespace pair.
set -uo pipefail
HOST_NS=${HOST_NS:-lumen-host}
CLIENT_NS=${CLIENT_NS:-lumen-client}
SUDO=""
if [ "$(id -u)" -ne 0 ]; then SUDO="sudo -n"; fi
$SUDO ip netns del "$HOST_NS" 2>/dev/null || true
$SUDO ip netns del "$CLIENT_NS" 2>/dev/null || true
$SUDO ip link del lumen-h 2>/dev/null || true
echo "[netns-down] removed $HOST_NS, $CLIENT_NS"
