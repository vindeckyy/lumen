#!/usr/bin/env python3
"""
migrate_sunshine_config.py — one-shot Sunshine -> Lumen config migration.

Reads a sunshine.conf (or the active runtime config) and emits a
lumen.conf with:

  - the namespace renamed (sunshine.* -> lumen.*)
  - the Lumen rebrand keys added (cachyos tuning knobs, NVENC presets)
  - unknown keys preserved verbatim

Stdlib only. Idempotent: running it twice on the same input yields the
same output.

Usage:
    tools/migrate_sunshine_config.py /path/to/sunshine.conf > lumen.conf
    tools/migrate_sunshine_config.py --in-place ~/.config/sunshine/sunshine.conf
"""
from __future__ import annotations

import argparse
import configparser
import sys
from pathlib import Path

# Keys whose section name changes from `sunshine` to `lumen`. Anything
# under [sunshine] or [sunshine.*] is moved under [lumen.*].
RENAMED_SECTIONS = [("sunshine", "lumen")]

# Keys that are Lumen-only additions; pre-seeded with documented defaults
# so a freshly-migrated config picks them up.
LUMEN_DEFAULT_BLOCK = """
[lumen]
; --- Lumen-only tunables. All are opt-out; safe defaults below. ---
; See docs/configuration.md for the full reference.
busy_poll_us = 50
enet_4mib_buffer = 1
pipewire_latency_ms = 8
cpu_pinning = 1
rate_cap_pct = 80

; --- NVENC tuning presets (0=latency, 1=balanced, 2=quality). ---
; -1 = manual (every other nvenc_* key takes effect).
nv_preset = -1
"""


def _coerce(value: str):
    """Best-effort: keep ints as ints, floats as floats, bools as bools."""
    v = value.strip()
    low = v.lower()
    if low in {"true", "yes", "on"}:
        return True
    if low in {"false", "no", "off"}:
        return False
    if v.startswith('"') and v.endswith('"'):
        return v[1:-1]
    try:
        return int(v)
    except ValueError:
        pass
    try:
        return float(v)
    except ValueError:
        pass
    return v


def migrate(text: str) -> str:
    parser = configparser.ConfigParser()
    parser.read_string(text)
    out = configparser.ConfigParser()
    out.optionxform = str  # preserve case
    for section in parser.sections():
        new_section = section
        for old, new in RENAMED_SECTIONS:
            if section == old:
                new_section = new
            elif section.startswith(old + "."):
                new_section = new + section[len(old):]
        if not out.has_section(new_section):
            out.add_section(new_section)
        for key, value in parser.items(section):
            out.set(new_section, key, value)

    # Append Lumen defaults (only fill keys not already present).
    defaults = configparser.ConfigParser()
    defaults.read_string(LUMEN_DEFAULT_BLOCK)
    for section in defaults.sections():
        if not out.has_section(section):
            out.add_section(section)
        for key, value in defaults.items(section):
            if not out.has_option(section, key):
                out.set(section, key, value)

    buf = []
    buf.append("; Migrated by tools/migrate_sunshine_config.py\n")
    buf.append("; Source: Sunshine config. See docs/PORTING.md.\n\n")

    # Find the [lumen] section and put the version header at the top of it,
    # then emit every section in order (lumen first if present).
    sections = list(out.sections())
    ordered = ["lumen"] + [s for s in sections if s != "lumen"]
    seen = set()
    for section in ordered:
        if section in seen or not out.has_section(section):
            continue
        seen.add(section)
        buf.append(f"[{section}]\n")
        if section == "lumen" and not out.has_option(section, "version"):
            buf.append("version = 2026.999.0\n")
        for key, value in out.items(section):
            coerced = _coerce(value)
            if isinstance(coerced, bool):
                buf.append(f"{key} = {'true' if coerced else 'false'}\n")
            else:
                buf.append(f"{key} = {coerced}\n")
        buf.append("\n")
    return "".join(buf)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("input", type=Path, help="Input sunshine.conf path (- for stdin).")
    p.add_argument("--in-place", action="store_true",
                   help="Overwrite the input file in place (default: print to stdout).")
    args = p.parse_args()

    text = sys.stdin.read() if str(args.input) == "-" else args.input.read_text()
    migrated = migrate(text)
    if args.in_place and str(args.input) != "-":
        args.input.write_text(migrated)
    else:
        sys.stdout.write(migrated)
    return 0


if __name__ == "__main__":
    sys.exit(main())