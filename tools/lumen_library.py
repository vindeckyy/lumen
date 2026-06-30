#!/usr/bin/env python3
"""
lumen_library.py — scan installed games from Steam, Heroic, Flatpak,
and the system for one-shot import into Lumen.

Stdlib only. Returns a JSON list of:

    {
      "name":       "Elden Ring",
      "launcher":   "steam",
      "exec":       "steam steam://rungameid/1245620",
      "icon":       "/home/x/.steam/steam/steam/games/.../icon.png",
      "args":       [],
      "id":         "steam:1245620",
      "added":      false
    }

`added` is computed against an existing lumen.conf's apps section so
the caller can show "X new games found" without re-querying.

Usage:
    lumen_library.py --scan                     # JSON to stdout
    lumen_library.py --scan --pretty            # indented JSON
    lumen_library.py --diff LUMEN_CONF          # only show games not already in lumen.conf
"""
from __future__ import annotations

import argparse
import configparser
import glob
import json
import os
import re
import sys
from pathlib import Path
from typing import Iterable


# ---------------------------------------------------------------------------
# Steam VDF parser — minimal subset (handles libraryfolders.vdf + appmanifest_*.acf)
# ---------------------------------------------------------------------------

class _VDFNode:
    __slots__ = ("key", "value", "children")

    def __init__(self, key: str = "", value: str = ""):
        self.key = key
        self.value = value
        self.children: list["_VDFNode"] = []

    def walk(self):
        yield self
        for c in self.children:
            yield from c.walk()

    def find(self, key: str) -> "_VDFNode | None":
        for c in self.children:
            if c.key == key:
                return c
        return None

    def get(self, key: str, default: str = "") -> str:
        n = self.find(key)
        return n.value if n else default


def parse_vdf(text: str) -> _VDFNode:
    """Parse a VDF blob into a tree. Quote-aware, escape-aware.

    VDF is a small enough grammar that a hand-rolled parser is shorter
    than pulling vdf-py. Quotes can wrap values; escapes are \\\\".
    """
    root = _VDFNode()
    stack = [root]
    i, n = 0, len(text)

    def read_quoted() -> str:
        nonlocal i
        i += 1  # opening quote
        out = []
        while i < n:
            c = text[i]
            if c == "\\" and i + 1 < n:
                nxt = text[i + 1]
                out.append({"n": "\n", "t": "\t", "\\": "\\", '"': '"'}.get(nxt, nxt))
                i += 2
                continue
            if c == '"':
                i += 1
                return "".join(out)
            out.append(c)
            i += 1
        return "".join(out)

    while i < n:
        c = text[i]
        if c.isspace():
            i += 1
            continue
        if c == "{":
            i += 1
            continue
        if c == "}":
            i += 1
            if len(stack) > 1:
                stack.pop()
            continue
        if c == '"':
            key = read_quoted()
            # skip ws
            while i < n and text[i].isspace():
                i += 1
            if i < n and text[i] == "{":
                node = _VDFNode(key=key)
                stack[-1].children.append(node)
                stack.append(node)
                i += 1
                continue
            if i < n and text[i] == '"':
                val = read_quoted()
                stack[-1].children.append(_VDFNode(key=key, value=val))
                continue
        i += 1
    return root


# ---------------------------------------------------------------------------
# Source-specific scanners. Each returns Iterable[dict].
# ---------------------------------------------------------------------------

def _extract_steam_libraries(tree: _VDFNode) -> list[Path]:
    """Pull every `path` value out of a parsed libraryfolders.vdf tree.

    Three historical shapes to handle:
      - Modern (2020+): `LibraryFolders` -> `"0"|"1"|...` -> `path` = "..."
      - 2010s:          `LibraryFolders` -> `"0"|"1"|...` with `path` as the value
      - Very old:       `LibraryFolders` -> direct children whose value is a path.
    """
    paths: list[Path] = []
    for top in tree.children:
        if top.key == "LibraryFolders":
            for entry in top.children:
                p = entry.find("path")
                if p and p.value:
                    paths.append(Path(p.value))
                elif entry.value and entry.value.startswith("/"):
                    paths.append(Path(entry.value))
        elif top.key.isdigit():
            # Some variants skip the LibraryFolders wrapper.
            p = top.find("path")
            if p and p.value:
                paths.append(Path(p.value))
    return paths


def scan_steam(home: Path) -> Iterable[dict]:
    """Walk Steam's libraryfolders.vdf + each library's appmanifest_*.acf."""
    candidates = [
        home / ".steam" / "steam" / "steamapps" / "libraryfolders.vdf",
        home / ".var" / "app" / "com.valvesoftware.Steam" / "data" / "Steam" / "steam" / "steamapps" / "libraryfolders.vdf",
    ]
    libfolders_vdf = next((p for p in candidates if p.exists()), None)
    if libfolders_vdf is None:
        return

    tree = parse_vdf(libfolders_vdf.read_text(errors="ignore"))
    libraries = _extract_steam_libraries(tree)

    for lib in libraries:
        for manifest in glob.glob(str(lib / "steamapps" / "appmanifest_*.acf")):
            try:
                parsed = parse_vdf(Path(manifest).read_text(errors="ignore"))
            except Exception:
                continue
            # Manifest structure: AppState -> { appid, name, installdir, ... }
            state = parsed.find("AppState")
            if state is None:
                continue
            appid = state.get("appid")
            name = state.get("name")
            installdir = state.get("installdir")
            if not (appid and name and installdir):
                continue
            icon_path = lib / "steamapps" / "common" / installdir / "icon.png"
            yield {
                "name": name,
                "launcher": "steam",
                "exec": f"steam steam://rungameid/{appid}",
                "icon": str(icon_path) if icon_path.exists() else "",
                "args": [],
                "id": f"steam:{appid}",
            }


def scan_heroic(home: Path) -> Iterable[dict]:
    """Heroic writes one JSON per sideloaded app under ~/.config/heroic/sideload_apps/."""
    cfg = home / ".config" / "heroic"
    if not cfg.exists():
        return
    for path in glob.glob(str(cfg / "**" / "*.json"), recursive=True):
        try:
            data = json.loads(Path(path).read_text(errors="ignore"))
        except Exception:
            continue
        if not isinstance(data, dict):
            continue
        if data.get("appName") and data.get("executable"):
            yield {
                "name": data["appName"],
                "launcher": "heroic",
                "exec": data["executable"],
                "icon": data.get("iconUrl", ""),
                "args": data.get("launcherArgs", "").split() if isinstance(data.get("launcherArgs"), str) else data.get("launcherArgs", []),
                "id": f"heroic:{path}",
            }


def scan_desktop_apps(search_dirs: list[Path]) -> Iterable[dict]:
    """Pick up .desktop files whose Categories include Game or GameEmulator."""
    seen: set[str] = set()
    for d in search_dirs:
        if not d.exists():
            continue
        for path in glob.glob(str(d / "*.desktop")):
            try:
                cp = configparser.ConfigParser(interpolation=None)
                cp.read(path)
                if not cp.has_section("Desktop Entry"):
                    continue
                cats = cp.get("Desktop Entry", "Categories", fallback="")
                if "Game" not in cats:
                    continue
                name = cp.get("Desktop Entry", "Name", fallback="")
                exec_ = cp.get("Desktop Entry", "Exec", fallback="")
                icon = cp.get("Desktop Entry", "Icon", fallback="")
                if not (name and exec_):
                    continue
                # Strip Exec field codes (%u, %U, %f, etc.).
                clean_exec = re.sub(r"\s+%[a-zA-Z]", "", exec_).strip()
                key = f"desktop:{name}"
                if key in seen:
                    continue
                seen.add(key)
                yield {
                    "name": name,
                    "launcher": "desktop",
                    "exec": clean_exec,
                    "icon": icon,
                    "args": [],
                    "id": key,
                }
            except Exception:
                continue


def scan_flatpak() -> Iterable[dict]:
    """Use `flatpak list --app --columns=name,application` if flatpak is on PATH."""
    import shutil
    import subprocess
    if not shutil.which("flatpak"):
        return
    try:
        out = subprocess.run(
            ["flatpak", "list", "--app", "--columns=name,application"],
            check=True, capture_output=True, text=True, timeout=10,
        ).stdout
    except Exception:
        return
    for line in out.splitlines():
        parts = line.strip().split("\t")
        if len(parts) != 2:
            continue
        name, app_id = parts[0].strip(), parts[1].strip()
        if not (name and app_id):
            continue
        yield {
            "name": name,
            "launcher": "flatpak",
            "exec": f"flatpak run {app_id}",
            "icon": "",
            "args": [],
            "id": f"flatpak:{app_id}",
        }


# ---------------------------------------------------------------------------
# Aggregation + diff against existing config.
# ---------------------------------------------------------------------------

def scan(home: Path = Path.home()) -> list[dict]:
    home = Path(home)
    games: list[dict] = []
    games.extend(scan_steam(home))
    games.extend(scan_heroic(home))
    games.extend(scan_flatpak())
    games.extend(scan_desktop_apps([
        home / ".local" / "share" / "applications",
        Path("/usr/share/applications"),
        Path("/var/lib/flatpak/exports/share/applications"),
    ]))
    # De-dup by id.
    seen = set()
    out = []
    for g in games:
        if g["id"] in seen:
            continue
        seen.add(g["id"])
        out.append(g)
    return out


def load_existing_app_names(config_path: Path) -> set[str]:
    if not config_path.exists():
        return set()
    cp = configparser.ConfigParser(interpolation=None)
    cp.read(config_path)
    names: set[str] = set()
    for section in cp.sections():
        # Apps section names start with "apps."
        if section.startswith("apps"):
            for key in ("name", "cmd"):
                if cp.has_option(section, key):
                    names.add(cp.get(section, key))
    return names


def mark_added(games: list[dict], existing: set[str]) -> None:
    for g in games:
        g["added"] = g["name"] in existing


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--scan", action="store_true", help="Scan and print JSON to stdout.")
    p.add_argument("--pretty", action="store_true", help="Indent the JSON output.")
    p.add_argument("--diff", type=Path, default=None,
                   help="Only show games not already present in this lumen.conf (sets `added: false`).")
    p.add_argument("--home", type=Path, default=Path.home(),
                   help="Override $HOME for scanning (mostly for tests).")
    args = p.parse_args()

    if not args.scan:
        p.error("--scan is required (this tool is read-only)")

    games = scan(home=args.home)
    if args.diff:
        existing = load_existing_app_names(args.diff)
        mark_added(games, existing)
        games = [g for g in games if not g.get("added")]
    print(json.dumps(games, indent=2 if args.pretty else None, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())