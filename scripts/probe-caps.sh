#!/usr/bin/env bash
# Probe the local graphics/codec capabilities into artifacts/m0/capabilities.json.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$REPO_ROOT/artifacts/m0"
mkdir -p "$OUT"

python3 - "$OUT/capabilities.json" <<'PY'
import json, os, subprocess, sys, glob

def run(cmd):
    try:
        p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=90)
        return (p.stdout + p.stderr).strip()
    except Exception as e:  # noqa: BLE001
        return f"error: {e}"

def pkg(name):
    out = run(f"pkg-config --modversion {name}")
    return None if out.startswith("error") else out

caps = {
    "os": run("uname -srmo"),
    "cpu": run("grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-").strip(),
    "cores": os.cpu_count(),
    "dri_nodes": sorted(os.path.basename(p) for p in glob.glob("/dev/dri/*")),
    "nvidia_nodes": sorted(os.path.basename(p) for p in glob.glob("/dev/nvidia*")),
    "pkg_config": {n: pkg(n) for n in [
        "libva", "libva-drm", "libdrm", "gbm", "egl", "libpipewire-0.3", "wayland-client",
        "xkbcommon", "xcb", "opus", "libvulkan", "vulkan",
    ]},
    "va": {},
    "vulkan": {},
}

# VA-API per DRM device: the radeonsi driver needs an explicit --device, the
# NVIDIA NVDEC driver does not.
for dev in sorted(glob.glob("/dev/dri/renderD*")) + sorted(glob.glob("/dev/dri/card*")):
    key = os.path.basename(dev)
    for drv in ("", "radeonsi", "nvidia"):
        ld = f"LIBVA_DRIVER_NAME={drv} " if drv else ""
        # vainfo needs --display drm --device to bind to a specific node;
        # VA_DISPLAY alone does not select the device reliably.
        out = run(f"{ld}timeout 30 vainfo --display drm --device {dev} 2>&1")
        if "VAEntrypoint" in out:
            def prof(line):
                return line.split(":", 1)[0].strip()
            entry = {
                "driver": drv or "auto",
                "device": dev,
                "profiles": sorted({prof(l) for l in out.splitlines()
                                    if "VAEntrypoint" in l and ":" in l}),
                "encoders": sorted({prof(l) for l in out.splitlines()
                                    if ("VAEntrypointEncSlice" in l or "VAEntrypointEncPicture" in l)
                                    and ":" in l}),
            }
            caps["va"][key] = entry
            break
    else:
        caps["va"][key] = {"error": "no working VA driver"}

vk = run("timeout 90 vulkaninfo 2>/dev/null")
caps["vulkan"] = {
    "devices": sorted({l.strip() for l in vk.splitlines() if "deviceName" in l}),
    "video_encode_ext": "VK_KHR_video_encode_queue" in vk,
    "video_decode_ext": "VK_KHR_video_decode_queue" in vk,
    "version": next((l.split(":", 1)[1].strip() for l in vk.splitlines()
                     if "Vulkan Instance Version" in l and ":" in l), None),
}
caps["nvidia_smi"] = run("nvidia-smi --query-gpu=name,driver_version --format=csv,noheader")
caps["portal_backends"] = sorted(os.path.basename(p) for p in
                                 glob.glob("/usr/libexec/xdg-desktop-portal*"))
caps["pipewire"] = run("pipewire --version")

with open(sys.argv[1], "w") as f:
    json.dump(caps, f, indent=2, sort_keys=True)
print(json.dumps(caps, indent=2, sort_keys=True)[:2000])
PY
echo "[probe-caps] wrote $OUT/capabilities.json"
