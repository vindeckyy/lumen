#!/usr/bin/env bash
# Probe which baseline implementations are installed and runnable here.
#
# Plan milestone M0.6: "Baseline availability probe: detect runnable Sunshine,
# Moonlight-Qt, RustDesk (+server), GRD + xfreerdp; record versions and
# runnable commands -> artifacts/m0/baselines.json".
#
# The probe never guesses. For every candidate it records:
#   - whether the binary was found, and the exact command that found it,
#   - the version, taken from the binary or from the package database, or
#     null with the reason it could not be read,
#   - the command that would run the baseline,
#   - everything that was searched, so a reader can reproduce the result.
#
# Exit status:
#   0  the probe ran and wrote the file. The exit status says nothing about
#      the baselines: this script's job is to report the truth about them,
#      and the truth may well be "none of them is installed".
#   1  the probe could not run (no python3, no write permission).
#   2  a bad command line.
#   3  a baseline named with --require is not available.
#
# Use --require to turn the probe into a gate:
#   scripts/baseline-check.sh --require sunshine
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OUT_DIR="${OUT_DIR:-$REPO_ROOT/artifacts/m0}"
OUT="${OUT:-$OUT_DIR/baselines.json}"

log() { printf '[baseline-check] %s\n' "$*"; }

usage() {
  cat <<'EOF'
Usage: baseline-check.sh [options]

Probe the machine for the baseline implementations Lumen is compared against
and write the result to artifacts/m0/baselines.json.

Options:
  --out <path>      Where the JSON goes (default artifacts/m0/baselines.json)
  --require <name>  Exit non-zero when the named baseline is not available.
                     May be given more than once.
  -h, --help        Print this text

The candidates are:
  sunshine       the Sunshine host server, paired with Moonlight-Qt
  moonlight_qt   the Moonlight-Qt client
  rustdesk       the RustDesk client, which is also the host
  rustdesk_server the RustDesk rendezvous/relay server (hbbs)
  grd            the GNOME Remote Desktop host, in RDP mode
  xfreerdp       the RDP client that drives the GRD baseline

Nothing is fabricated. A baseline that is not installed is recorded as
available=false with version=null and the reason.
EOF
}

REQUIRE=()

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="${2:?--out needs a path}"; shift 2 ;;
    --require) REQUIRE+=("${2:?--require needs a name}"); shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) log "unknown option $1"; usage; exit 2 ;;
  esac
done

command -v python3 >/dev/null 2>&1 || {
  log "python3 is needed to write $OUT"
  exit 1
}

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR"

# The probe writes one file and nothing else, so there is nothing to clean
# up, but the trap keeps the script honest if that ever changes.
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "probing the baselines into $OUT"

python3 - "$OUT" "$REPO_ROOT" "${REQUIRE[@]+"${REQUIRE[@]}"}" <<'PY'
import datetime
import glob
import json
import os
import platform
import re
import shutil
import subprocess
import sys

out_path, repo_root = sys.argv[1], sys.argv[2]
required = sys.argv[3:]


def run(cmd):
    """Run a command and return (rc, combined output) without raising."""
    try:
        p = subprocess.run(
            cmd, capture_output=True, text=True, timeout=30, check=False
        )
    except Exception as exc:  # noqa: BLE001 - any failure means "not available"
        return 127, f"{type(exc).__name__}: {exc}"
    return p.returncode, (p.stdout or "") + (p.stderr or "")


def which(name):
    """The absolute path of a binary, or None. The search is PATH plus the
    standard install prefixes, so a binary that was installed by hand into
    /opt is still found."""
    found = shutil.which(name)
    if found:
        return found
    for prefix in ("/usr/bin", "/usr/local/bin", "/opt", "/snap/bin"):
        candidate = os.path.join(prefix, name)
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    return None


def dpkg(name):
    """The version of an installed Debian package, or None."""
    rc, out = run(["dpkg-query", "-W", "-f=${Version}", name])
    if rc == 0 and out.strip():
        return out.strip()
    return None


def deb_files(name):
    """A .deb dropped into baselines/ counts as an installable baseline."""
    hits = sorted(glob.glob(os.path.join(repo_root, "baselines", f"*{name}*.deb")))
    return hits or None


def first_line(text):
    for line in text.splitlines():
        line = line.strip()
        if line:
            return line
    return None


# Each candidate: the binary names to look for, the packages to ask the
# package database about, the version command to run and the command that
# would run the baseline.
CANDIDATES = {
    "sunshine": {
        "role": "host",
        "licence": "GPL-3.0",
        "binaries": ["sunshine"],
        "packages": ["sunshine"],
        "version_args": ["--version"],
        "run_command": "sunshine -c /usr/local/etc/sunshine/sunshine.conf",
        "notes": (
            "GPL-3.0 host server; pairs with Moonlight-Qt. Needs a capture "
            "backend (KMS/wlr/X11/portal) and an encoder."
        ),
    },
    "moonlight_qt": {
        "role": "client",
        "licence": "GPL-3.0",
        "binaries": ["moonlight", "moonlight-qt"],
        "packages": ["moonlight-client", "moonlight-common-c"],
        "version_args": ["--version"],
        "run_command": "moonlight -p <host-address>",
        "notes": (
            "GPL-3.0 client. The upstream package needs moonlight-common-c, "
            "which carries the host certificate."
        ),
    },
    "rustdesk": {
        "role": "host+client",
        "licence": "AGPL-3.0",
        "binaries": ["rustdesk"],
        "packages": ["rustdesk"],
        "version_args": ["--version"],
        "run_command": "rustdesk --password <password> <host-address>",
        "notes": (
            "AGPL-3.0, both sides. Direct IP access works on a LAN without the "
            "server; the server is only needed for NAT traversal."
        ),
    },
    "rustdesk_server": {
        "role": "server",
        "licence": "AGPL-3.0",
        "binaries": ["hbbs", "rustdesk-server"],
        "packages": ["rustdesk-server"],
        "version_args": ["--version"],
        "run_command": "hbbs -k _",
        "notes": (
            "AGPL-3.0 rendezvous and relay server. Optional for a LAN-only "
            "baseline: direct access does not need it."
        ),
    },
    "grd": {
        "role": "host",
        "licence": "GPL-2.0+",
        "binaries": ["gnome-remote-desktop-daemon"],
        "packages": ["gnome-remote-desktop"],
        "version_args": ["--version"],
        "run_command": "gnome-remote-desktop-daemon --system --rdp",
        "notes": (
            "GPL-2.0+ host, RDP mode, measured against xfreerdp. Needs Mutter "
            "for the ScreenCast portal and VA-API or NVENC for the encoder."
        ),
    },
    "xfreerdp": {
        "role": "rdp-client",
        "licence": "Apache-2.0",
        "binaries": ["xfreerdp", "xfreerdp3"],
        "packages": ["freerdp3-x11", "xfreerdp"],
        "version_args": ["--version"],
        "run_command": "xfreerdp /v:<host-address> /u:<user> /p:<password>",
        "notes": "RDP client used to drive the GRD baseline.",
    },
}

baselines = {}
missing = []
present = []

for name, spec in CANDIDATES.items():
    entry = {
        "role": spec["role"],
        "licence": spec["licence"],
        "notes": spec["notes"],
    }

    # 1. The binary.
    binary = None
    binary_name = None
    searched = []
    for candidate in spec["binaries"]:
        searched.append(f"command -v {candidate}")
        found = which(candidate)
        if found:
            binary = found
            binary_name = candidate
            break

    # 2. The package database.
    packages = {}
    for package in spec["packages"]:
        searched.append(f"dpkg-query -W -f='${{Version}}' {package}")
        packages[package] = dpkg(package)

    # 3. A .deb in baselines/.
    searched.append(f"ls baselines/*{name}*.deb")
    deb = deb_files(name)

    entry["detection"] = {
        "binary_names": spec["binaries"],
        "packages": spec["packages"],
        "package_versions": packages,
        "deb": deb,
        "commands_run": searched,
    }

    # 4. The version, from the binary if it runs, else from the package
    #    database. Never invented.
    version = None
    version_source = None
    version_command = None
    if binary:
        cmd = [binary] + spec["version_args"]
        rc, text = run(cmd)
        version_command = " ".join(cmd)
        if rc == 0:
            candidate = first_line(text)
            if candidate:
                version = candidate
                version_source = version_command
        elif packages:
            for package, package_version in packages.items():
                if package_version:
                    version = package_version
                    version_source = f"dpkg-query -W -f='${{Version}}' {package}"
                    break
    else:
        for package, package_version in packages.items():
            if package_version:
                version = package_version
                version_source = f"dpkg-query -W -f='${{Version}}' {package}"
                break

    available = bool(binary or deb)
    entry["available"] = available
    entry["version"] = version
    entry["version_source"] = version_source
    entry["version_command"] = version_command
    # The semantic version, when the tool printed one inside its version
    # string. `version` keeps the tool's own words either way.
    if version:
        match = re.search(r"\d+\.\d+(?:\.\d+)?", version)
        entry["version_number"] = match.group(0) if match else None
    else:
        entry["version_number"] = None
    entry["binary"] = binary
    entry["binary_name"] = binary_name
    entry["run_command"] = spec["run_command"] if available else None

    if available:
        present.append(name)
        entry["status"] = (
            "available"
            if binary
            else "available as a .deb only; it still has to be installed"
        )
    else:
        missing.append(name)
        entry["status"] = "not available"
        entry["reason"] = (
            "no binary in PATH or in /usr/bin, /usr/local/bin, /opt or "
            "/snap/bin, no .deb in baselines/, and the package database has "
            "no entry for " + ", ".join(spec["packages"])
        )

    baselines[name] = entry

report = {
    "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(
        timespec="seconds"
    ),
    "host": {
        "os": platform.platform(),
        "kernel": platform.release(),
        "arch": platform.machine(),
    },
    "probe": {
        "script": "scripts/baseline-check.sh",
        "milestone": "M0.6",
        "note": (
            "Availability probe only. Nothing was run against a baseline and "
            "no latency was measured."
        ),
    },
    "baselines": baselines,
    "summary": {
        "candidates": len(CANDIDATES),
        "available": len(present),
        "unavailable": len(missing),
        "available_names": sorted(present),
        "unavailable_names": sorted(missing),
        "measured": False,
    },
}

with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write("\n")

for name in sorted(baselines):
    entry = baselines[name]
    version = entry["version"] if entry["version"] is not None else "no version"
    print(
        f"{name:16} available={str(entry['available']).lower():5} "
        f"version={version} run={entry['run_command'] or '-'}"
    )

print(
    f"[baseline-check] {len(present)} of {len(CANDIDATES)} candidates available: "
    + ", ".join(sorted(present))
)
if missing:
    print(
        "[baseline-check] NOT available: "
        + ", ".join(sorted(missing))
        + " - see the 'reason' field of each entry"
    )

failed = [name for name in required if not baselines.get(name, {}).get("available")]
if failed:
    print(f"[baseline-check] required but not available: {', '.join(failed)}")
    sys.exit(3)
PY

log "wrote $OUT"

for name in "${REQUIRE[@]+"${REQUIRE[@]}"}"; do
  if ! python3 - "$OUT" "$name" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    report = json.load(handle)
entry = report["baselines"].get(sys.argv[2], {})
sys.exit(0 if entry.get("available") else 1)
PY
  then
    log "the required baseline $name is not available; see $OUT"
    exit 3
  fi
done

log "done"
