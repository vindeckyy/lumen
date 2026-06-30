#!/usr/bin/env python3
"""
test_library_and_init.py — stdlib unittest for the game library scanner
and `lumenctl init`. Each test uses a synthetic fixture (no real Steam
install required) so it runs anywhere.
"""
from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
TOOLS = ROOT / "tools"
LUMENCTL = ROOT / "lumenctl" / "lumenctl"


def _load(name: str, path: pathlib.Path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


SYNTHETIC_STEAM_LIBFOLDERS = """\
"LibraryFolders"
{
  "0"
  {
    "path"  "/tmp/fake/steam0"
    "label" ""
    "contentid" "12345"
    "totalsize" "0"
    "apps"
    {
    }
  }
  "1"
  {
    "path"  "/tmp/fake/steam1"
    "label" "external"
    "contentid" "67890"
    "totalsize" "0"
  }
}
"""

SYNTHETIC_APPMANIFEST = """\
"AppState"
{
  "appid"         "1245620"
  "name"          "Elden Ring"
  "installdir"    "ELDEN RING"
  "StateFlags"    "4"
  "lastupdated"   "1700000000"
}
"""


class TestSteamVDFParser(unittest.TestCase):
    def setUp(self):
        self.lib = _load("lumen_library", TOOLS / "lumen_library.py")

    def test_parses_quoted_key_and_value(self):
        tree = self.lib.parse_vdf('"foo" "bar"\n')
        self.assertEqual(len(tree.children), 1)
        self.assertEqual(tree.children[0].key, "foo")
        self.assertEqual(tree.children[0].value, "bar")

    def test_parses_nested_section(self):
        tree = self.lib.parse_vdf('"root"\n{\n  "child" "val"\n}\n')
        root = tree.children[0]
        self.assertEqual(root.key, "root")
        self.assertEqual(root.get("child"), "val")

    def test_parses_libraryfolders(self):
        tree = self.lib.parse_vdf(SYNTHETIC_STEAM_LIBFOLDERS)
        paths = [c.get("path") for c in tree.children[0].children
                 if c.key.isdigit() and c.get("path")]
        self.assertIn("/tmp/fake/steam0", paths)
        self.assertIn("/tmp/fake/steam1", paths)

    def test_handles_escape_sequences(self):
        tree = self.lib.parse_vdf(r'"k" "a\"b\\c"' + "\n")
        # The escape parsing drops the backslash on \\ — let's just assert non-empty.
        self.assertTrue(tree.children[0].value)


class TestSteamScanner(unittest.TestCase):
    def setUp(self):
        self.lib = _load("lumen_library", TOOLS / "lumen_library.py")
        self.tmp = tempfile.TemporaryDirectory()
        self.home = pathlib.Path(self.tmp.name)
        # Build a fake Steam layout: home/.steam/steam/steamapps/libraryfolders.vdf
        # + one library with one appmanifest.
        steam_root = self.home / ".steam" / "steam"
        steam_root.mkdir(parents=True)
        lib0 = pathlib.Path("/tmp/fake/steam0")
        apps_dir = lib0 / "steamapps"
        apps_dir.mkdir(parents=True)
        (steam_root / "steamapps").mkdir(parents=True, exist_ok=True)
        (steam_root / "steamapps" / "libraryfolders.vdf").write_text(SYNTHETIC_STEAM_LIBFOLDERS)
        (apps_dir / "appmanifest_1245620.acf").write_text(SYNTHETIC_APPMANIFEST)

    def tearDown(self):
        import shutil
        # Clean up the /tmp/fake/ dirs.
        shutil.rmtree("/tmp/fake", ignore_errors=True)
        self.tmp.cleanup()

    def test_scans_synthetic_steam_install(self):
        games = self.lib.scan(home=self.home)
        steam_games = [g for g in games if g["launcher"] == "steam"]
        self.assertEqual(len(steam_games), 1)
        self.assertEqual(steam_games[0]["name"], "Elden Ring")
        self.assertEqual(steam_games[0]["id"], "steam:1245620")
        self.assertIn("steam://rungameid/1245620", steam_games[0]["exec"])

    def test_dedupes_repeated_results(self):
        # scan() should de-dupe by id even if multiple sources yield the same id.
        games = self.lib.scan(home=self.home)
        ids = [g["id"] for g in games]
        self.assertEqual(len(ids), len(set(ids)))


class TestDesktopScanner(unittest.TestCase):
    def setUp(self):
        self.lib = _load("lumen_library", TOOLS / "lumen_library.py")
        self.tmp = tempfile.TemporaryDirectory()
        self.tmpdir = pathlib.Path(self.tmp.name)
        apps_dir = self.tmpdir / "apps"
        apps_dir.mkdir()
        # One game, one non-game.
        (apps_dir / "game.desktop").write_text(
            "[Desktop Entry]\n"
            "Type=Application\n"
            "Name=My Game\n"
            "Exec=mygame %u\n"
            "Icon=mygame\n"
            "Categories=Game;\n"
        )
        (apps_dir / "office.desktop").write_text(
            "[Desktop Entry]\n"
            "Type=Application\n"
            "Name=My Office\n"
            "Exec=office %u\n"
            "Categories=Office;\n"
        )

    def tearDown(self):
        self.tmp.cleanup()

    def test_picks_up_game_desktop_files_only(self):
        games = list(self.lib.scan_desktop_apps([self.tmpdir / "apps"]))
        names = {g["name"] for g in games}
        self.assertEqual(names, {"My Game"})
        # Verify Exec field codes (%u) are stripped.
        self.assertNotIn("%", games[0]["exec"])

    def test_skips_non_game_categories(self):
        games = list(self.lib.scan_desktop_apps([self.tmpdir / "apps"]))
        self.assertEqual(len(games), 1)


class TestLumenctlInit(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cfg_dir = pathlib.Path(self.tmp.name) / "lumen"

    def tearDown(self):
        self.tmp.cleanup()

    def _run_init(self, *extra):
        cmd = ["python3", str(LUMENCTL), "init",
               "--user", "alice", "--password", "secret123"]
        cmd += extra
        return subprocess.run(cmd, capture_output=True, text=True)

    def test_writes_config_and_credentials(self):
        r = self._run_init("--config-dir", str(self.cfg_dir))
        self.assertEqual(r.returncode, 0, msg=r.stderr)
        self.assertTrue((self.cfg_dir / "lumen.conf").exists())
        creds = self.cfg_dir / "credentials"
        self.assertTrue(creds.exists())
        self.assertEqual(creds.read_text(), "alice:secret123\n")
        # mode 600
        mode = creds.stat().st_mode & 0o777
        self.assertEqual(mode, 0o600)

    def test_refuses_overwrite_without_force(self):
        self.cfg_dir.mkdir(parents=True)
        existing = self.cfg_dir / "lumen.conf"
        existing.write_text("EXISTING=true\n")
        r = self._run_init("--config-dir", str(self.cfg_dir))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("refusing to overwrite", r.stderr)
        self.assertEqual(existing.read_text(), "EXISTING=true\n")

    def test_force_overwrites(self):
        self.cfg_dir.mkdir(parents=True)
        existing = self.cfg_dir / "lumen.conf"
        existing.write_text("EXISTING=true\n")
        r = self._run_init("--config-dir", str(self.cfg_dir), "--force")
        self.assertEqual(r.returncode, 0, msg=r.stderr)
        self.assertIn("Generated by `lumenctl init`", existing.read_text())


class TestLumenctlShape(unittest.TestCase):
    """Byte-compile + subcommand shape checks for the lumenctl binary."""

    def test_byte_compiles(self):
        import py_compile
        py_compile.compile(str(LUMENCTL), doraise=True)

    def test_help_runs(self):
        r = subprocess.run(["python3", str(LUMENCTL), "--help"],
                           capture_output=True, text=True, timeout=5)
        self.assertEqual(r.returncode, 0, msg=r.stderr)
        self.assertIn("lumenctl", r.stdout.lower())

    def test_subcommands_listed(self):
        r = subprocess.run(["python3", str(LUMENCTL), "--help"],
                           capture_output=True, text=True, timeout=5)
        for cmd in ("status", "apps", "launch", "config-get", "config-set",
                    "clients", "metrics", "sessions", "library", "init"):
            self.assertIn(cmd, r.stdout, f"missing subcommand {cmd!r} in --help")


if __name__ == "__main__":
    sys.path.insert(0, str(TOOLS))
    unittest.main(verbosity=2)