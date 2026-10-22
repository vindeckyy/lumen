#!/usr/bin/env bash
# Lumen environment bootstrap. Idempotent: safe to re-run.
set -euo pipefail

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_DIR="$REPO_ROOT/artifacts/m0"
mkdir -p "$LOG_DIR"

log() { printf '[env-setup] %s\n' "$*"; }

# ---------------------------------------------------------------- rust toolchain
if ! command -v rustup >/dev/null 2>&1; then
  log "installing rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/lumen-rustup-init.sh
  sh /tmp/lumen-rustup-init.sh -y --default-toolchain 1.99.1 --profile minimal \
    --component rustfmt --component clippy --no-modify-path
fi

log "ensuring toolchain 1.99.1"
rustup toolchain install 1.99.1 --profile minimal --component rustfmt --component clippy 2>/dev/null || true

# Nightly is only needed for cargo-fuzz targets; failure here is not fatal.
log "ensuring nightly toolchain (fuzz)"
rustup toolchain install nightly --profile minimal 2>/dev/null || log "nightly install failed; cargo-fuzz targets will be skipped"

# --------------------------------------------------------------- cargo tooling
if ! cargo deny --version >/dev/null 2>&1; then
  log "installing cargo-deny"
  cargo install cargo-deny --locked || log "cargo-deny install failed"
fi
if ! cargo audit --version >/dev/null 2>&1; then
  log "installing cargo-audit"
  cargo install cargo-audit --locked || log "cargo-audit install failed"
fi
if ! cargo fuzz --version >/dev/null 2>&1; then
  log "installing cargo-fuzz"
  cargo install cargo-fuzz --locked || log "cargo-fuzz install failed"
fi

# ------------------------------------------------------------- system packages
PKGS=(
  build-essential pkg-config clang libclang-dev
  libva-dev libdrm-dev libgbm-dev libegl-dev libvulkan-dev
  libwayland-dev libxkbcommon-dev libxcb1-dev libxcb-util-dev
  libpipewire-0.3-dev libopus-dev libudev-dev libinput-dev libasound2-dev
  xvfb x11-xserver-utils xserver-xorg-video-dummy sway
  xdg-desktop-portal xdg-desktop-portal-wlr
  mesa-va-drivers vainfo vulkan-tools nvidia-vaapi-driver
  iproute2 net-tools ffmpeg v4l-utils freerdp3-x11
)

if command -v apt-get >/dev/null 2>&1; then
  log "installing system packages"
  SUDO=""
  if [ "$(id -u)" -ne 0 ]; then SUDO="sudo -n"; fi
  $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq || log "apt update failed"
  $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${PKGS[@]}" \
    || log "apt install of some packages failed; see output"
fi

# ------------------------------------------------------------------- dri access
# The user must be able to open the render nodes for VA-API / DRM work.
if command -v usermod >/dev/null 2>&1 && [ "$(id -u)" -ne 0 ]; then
  sudo -n usermod -aG video,render,input "$USER" 2>/dev/null || true
  log "added $USER to video,render,input groups (takes effect on next login)"
fi

log "done"
log "rustc: $(rustc --version 2>/dev/null || echo missing)"
