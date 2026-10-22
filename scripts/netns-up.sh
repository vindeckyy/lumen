#!/usr/bin/env bash
# Create the lumen-host / lumen-client network namespace pair over a veth link.
# Requires root (sudo -n). Idempotent.
set -euo pipefail

HOST_NS=${HOST_NS:-lumen-host}
CLIENT_NS=${CLIENT_NS:-lumen-client}
VETH_HOST=${VETH_HOST:-lumen-h}
VETH_CLIENT=${VETH_CLIENT:-lumen-c}
HOST_IP=${HOST_IP:-10.77.0.1/24}
CLIENT_IP=${CLIENT_IP:-10.77.0.2/24}

SUDO=""
if [ "$(id -u)" -ne 0 ]; then SUDO="sudo -n"; fi

$SUDO ip netns del "$HOST_NS" 2>/dev/null || true
$SUDO ip netns del "$CLIENT_NS" 2>/dev/null || true
$SUDO ip link del "$VETH_HOST" 2>/dev/null || true

$SUDO ip netns add "$HOST_NS"
$SUDO ip netns add "$CLIENT_NS"
$SUDO ip link add "$VETH_HOST" type veth peer name "$VETH_CLIENT"
$SUDO ip link set "$VETH_HOST" netns "$HOST_NS"
$SUDO ip link set "$VETH_CLIENT" netns "$CLIENT_NS"

$SUDO ip -n "$HOST_NS" addr add "$HOST_IP" dev "$VETH_HOST"
$SUDO ip -n "$HOST_NS" link set "$VETH_HOST" up
$SUDO ip -n "$HOST_NS" link set lo up
$SUDO ip -n "$CLIENT_NS" addr add "$CLIENT_IP" dev "$VETH_CLIENT"
$SUDO ip -n "$CLIENT_NS" link set "$VETH_CLIENT" up
$SUDO ip -n "$CLIENT_NS" link set lo up

# Optional link shaping. Off by default so latency runs measure the bare veth.
# Enable with NETEM="delay 1ms limit 1000" (applied to both ends). Note that
# `ip -n NS tc ...` is invalid; tc must run inside the namespace via `ip netns exec`.
if [ -n "${NETEM:-}" ]; then
  $SUDO ip netns exec "$HOST_NS" tc qdisc add dev "$VETH_HOST" root netem $NETEM
  $SUDO ip netns exec "$CLIENT_NS" tc qdisc add dev "$VETH_CLIENT" root netem $NETEM
fi

echo "[netns-up] $HOST_NS ($HOST_IP) <-> $CLIENT_NS ($CLIENT_IP)"
echo "[netns-up] run commands with: sudo ip netns exec $HOST_NS <cmd>"
